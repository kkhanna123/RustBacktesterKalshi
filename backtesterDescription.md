# `kalshi-backtest` — How the Backtester Works & Complete Config Reference

This document explains, precisely, **how the Rust backtester works internally** and documents **every
configuration switch** (CLI flags, config-file fields, strategy parameters, and execution-model toggles).
It is grounded in the source under `backtester/src/`. For the why-behind-the-design see
[`DECISIONS.md`](DECISIONS.md); for the broader stack see [`README.md`](README.md).

---

## 1. Mental model

`kalshi-backtest` is a **single-threaded, deterministic, event-driven** simulator for **Kalshi binary
(cash-or-nothing) markets**. A binary contract settles **\$1** if its event resolves YES and **\$0**
otherwise, and trades in whole cents in **[1¢, 99¢]**.

Core principles:

- **Integer cents internally.** Prices are `Cents(i32)` in `1..=99` (`price.to_dollars() = cents/100`).
  No floating-point price drift.
- **YES-native single book.** Every market is one two-sided book quoted in YES terms. A resting **NO** bid
  at price `q` is stored as a YES **ask** (`SELL`) at `1 − q`. So `Side::Bid` = buying YES,
  `Side::Ask` = selling YES.
- **Event-driven.** The simulation consumes one time-ordered stream of `MarketEvent`s (orderbook deltas +
  trades) and advances state event by event. There is no wall-clock; everything is driven by event
  timestamps (nanoseconds).
- **One engine core for batch and live.** Batch backtesting and live paper-trading drive the *same*
  per-event function (`Engine::step`), so their fill/PnL logic is identical (guaranteed by the
  `incremental_core_matches_batch_run` test).

---

## 2. The data model (`src/types.rs`)

| Type | Meaning |
|---|---|
| `Cents(i32)` | Price level in integer cents, 1..=99. `to_dollars()`, `from_dollars()`, `complement()` (= `100 − c`). |
| `Side { Bid, Ask }` | YES-native book side. `Bid` = buy YES, `Ask` = sell YES. |
| `Action { Add, Update, Delete }` | Orderbook delta action. |
| `BookDelta { ts_ns, instrument, action, side, price, size, sequence, is_snapshot }` | One row of the `orderbook_deltas` contract. `is_snapshot=true` ⇒ the book is reset before applying. `size` = resting contracts at that level after the update. |
| `TradeEvent { ts_ns, instrument, aggressor_yes, price, size, trade_id }` | One executed trade. |
| `MarketEvent::{Delta, Trade}` | The unified, time-ordered stream the engine consumes. |
| `Order { kind: Limit\|Market, ... }`, `OrderView` | Strategy orders; `OrderView` is the read-only view strategies see. |
| `Fill { ts_ns, order_id, instrument, side, price, qty, liquidity: Maker\|Taker, fee }` | A produced fill. |
| `Report`, `Summary`, `EquityPoint` | The output schema (see §10). |

---

## 3. End-to-end pipeline

```
 data source ──► Vec<MarketEvent> (time-ordered)
     │                │
     │                ▼
     │           ┌──────────── Engine (implements Ctx) ───────────┐
     │           │  per event (Engine::step):                     │
     │           │   0. apply due cancels (cancel latency)        │
     │           │   1. Delta -> OrderBook/BookSet.apply()        │
     │           │   2. match resting orders vs Trade (maker)     │
     │           │   3. accrue liquidity rewards                  │
     │           │   4. strat.on_event(ev, &mut Ctx)              │
     │           │   5. drain queued actions:                     │
     │           │        place_limit -> resting order            │
     │           │        place_market -> walk book (taker)       │
     │           │        cancel -> schedule (cancel latency)     │
     │           │   (periodic equity snapshot)                   │
     │           └────────────────────────────────────────────────┘
     │                │
     ▼                ▼
 Portfolio (cash, positions, fills, round-trips, instrument stats)
     │
     ▼
 Metrics ──► Report{plugin_name, summary, equity_curve} + tearsheet + CSV/JSON exports
```

---

## 4. The event loop in detail (`src/engine.rs`)

`Engine::run_collecting(events, strat, cfg)` feeds each event through `Engine::step`, takes an equity
snapshot every `equity_snapshot_secs`, then `finalize`s (flatten if configured, credit rewards, build the
report). `Engine::step(event, strat)` does, in order:

0. **Apply due cancels.** Cancels requested earlier become effective once `effective_ts` (placement +
   `cancel_latency_ns`) ≤ the current event time.
1. **Update the book** if the event is a `Delta` (`BookSet::apply`).
2. **Match resting orders against trades.** If the event is a `Trade`, every resting strategy order whose
   **latency activation time** has been reached is tested for a fill against it (see §6). Maker fills are
   applied to the portfolio (`apply_fill_ex`, carrying any maker adverse-selection slippage cost).
3. **Accrue liquidity rewards** for this event window based on whether the strategy's resting quotes
   currently qualify (see §9).
4. **Run the strategy hook** `strat.on_event(ev, &mut ctx)`. The `Engine` itself implements `Ctx`, so the
   strategy reads book/position/cash and queues actions.
5. **Drain queued actions:** `place_limit` becomes a resting order (with its `queue_ahead` measured from the
   current book and an activation timestamp = now + `order_latency_ns` + jitter); `place_market` walks the
   opposing book immediately as a taker; `cancel` is scheduled with cancel latency.

`Engine::step` is the **shared incremental core**; live paper-trading calls the same `step` (the caller just
decides snapshot cadence and never sees a "batch end").

---

## 5. Order-book reconstruction (`src/orderbook.rs`)

`OrderBook` holds two `BTreeMap<Cents, f64>` (bids high→low, asks low→ask). `apply(delta)`:

- if `delta.is_snapshot` ⇒ **clear both sides** first (a full-book snapshot resets the book);
- `Add`/`Update` ⇒ set the level's size (size ≤ 0 removes it); `Delete` ⇒ remove the level;
- tracks `last_seq` and exposes `seq_gap(prev)`.

Accessors: `best_bid()`, `best_ask()`, `mid()` (dollars), `spread()`, `imbalance()`
(`(bid_sz − ask_sz)/(bid_sz + ask_sz)`), `microprice()` (size-weighted mid). `BookSet` is the per-instrument
map (`HashMap<String, OrderBook>`), so multiple instruments — and, with venue-tagged ids like
`"POLYMARKET:0x…"`, multiple venues — coexist automatically.

---

## 6. Execution / fill model (`src/execution.rs`)

**Resting limit orders (maker).** Each resting order records a `queue_ahead` = the contracts already resting
at its price level when it was placed (a price-time-priority approximation). A fill happens when a
**`TradeEvent` crosses** the order's price:

- a **BUY** limit at `P` fills when a trade prints at price **≤ P**;
- a **SELL** limit at `P` fills when a trade prints at price **≥ P**.

Incoming trade size first **burns down `queue_ahead`**, then fills `min(remaining, leftover)` as a **Maker**
fill (maker fee). Consequence: with no trades, makers never fill (correct — you can't earn the spread if
nobody trades).

**Market orders (taker).** Walk the opposing book levels immediately, consuming size level by level, paying
**taker** fees, plus any configured slippage. (The walk consumes ephemeral liquidity; the engine owns the
canonical book state.)

**Latency gating.** A resting order is only eligible to fill once the current event time ≥ its activation
timestamp (placement + `order_latency_ns` + deterministic jitter). See §8.

---

## 7. Fee model (`src/fees.rs`)

Default (`fee_bps_formula = true`) uses **Kalshi's published general trading fee**:

```
fee_per_fill = ceil( 0.07 · C · p · (1 − p) · 100 ) / 100     # round UP to the cent
```

where `C` = contracts and `p` = fill price in dollars. It **peaks at p = 0.5** (0.07·100·0.25 = \$1.75 per
100 contracts) and is cheap deep ITM/OTM. **Makers are free** by default. If `fee_bps_formula = false`, a
flat `taker_fee_rate` (default 1% of notional) is charged on takers and `maker_fee` per contract on makers.
`--no-fees` records fees on fills but does **not** charge them to cash/PnL.

---

## 8. Latency model (`src/latency.rs`) — the headline feature

This is the model that matters most: **an order sent at tick time `T` with latency `L` only becomes effective
at `T + L`, and fills at that timestep** — never earlier. Set it with `--latency-ns` (or
`[execution.latency]`); the components are `order_latency_ns + market_data_latency_ns + jitter_ns` (= `L`),
plus a separate `cancel_latency_ns` for cancels.

Every order gets `activation_ts = T + L`:

- **Resting limit (maker):** can only be matched by a `TradeEvent` whose `ts ≥ activation_ts`. A trade that
  crosses your price *before* `T+L` does **not** fill you.
- **Market / taker:** does **NOT** execute instantly. It is parked as a *pending in-flight* order and executes
  at the **first engine step whose event `ts ≥ activation_ts`**, walking the book **as of that later tick** (the
  latency-delayed book), with fills stamped at that event's ts. So a market order sent at `T` fills at
  `order_send + L` against the book that exists *then* — which may have moved away from where you saw it.
- `cancel_latency_ns` — a cancel takes effect only after this delay (so you can't instantly pull a quote).
- `jitter_ns` — **deterministic** per-order activation jitter (splitmix64 hash of the order's sequence number;
  no RNG → runs reproduce exactly).

**Zero latency (default):** `activation_ts == T`, so market orders execute during the same event they're
placed (immediate) and limits are matchable immediately — identical to a no-latency simulation. Turning
latency on is what makes the backtest realistic: orders go stale, market orders pay for the book moving
during the delay, and a strategy that looked profitable at zero latency can flip to losing.

### 8a. Stochastic latency distributions (`--latency-dist` / `[execution.latency].dist`)

By default order latency is `fixed` — the legacy `order_latency_ns + deterministic hash-jitter` model above,
which uses **no RNG** and is byte-for-byte the original behaviour. To stress-test whether an edge survives
realistic latency *variance*, set a non-`fixed` distribution: the per-order latency term is then **drawn from
a seeded [SplitMix64] PRNG** (`--latency-seed` / `[execution.latency].seed`, default `0`) instead of being
constant. The drawn value **replaces** the `order_latency_ns + jitter` term; `market_data_latency_ns` is still
added on top, and `cancel_latency_ns` is unaffected (always flat, no distribution). Setting any non-`fixed`
dist also **enables** the latency model.

| `--latency-dist` | Samples from | Params |
|---|---|---|
| `fixed` *(default)* | `order_latency_ns + hash_jitter` (no RNG) | `--latency-ns`, `--jitter-ns` |
| `uniform` | Uniform in `[min, max]` | `--latency-min-ns`, `--latency-max-ns` |
| `normal` | `Normal(mean, std)`, clamped ≥ 0 | `--latency-ns` (mean), `--latency-std-ns` |
| `exponential` | Exponential with the given mean (heavy-ish tail) | `--latency-mean-ns` (falls back to `--latency-ns`) |
| `empirical` | Replays measured samples (with replacement) | `--latency-empirical <file>` (newline/CSV of latency-ns; falls back to `fixed` if missing/empty) |

In config files this is the optional `[execution.latency.dist]` block tagged by `kind` (mirror the
`init-config` template; omit it for `fixed`):

```toml
[execution.latency.dist]
kind = "normal"        # uniform | normal | exponential | empirical | fixed
mean_ns = 500000000    # uniform=min_ns/max_ns; normal=mean_ns/std_ns; exponential=mean_ns; empirical=path
std_ns  = 300000000
```

**Determinism note:** with a non-`fixed` dist the run is no longer "byte-identical from inputs alone" — it is
reproducible **given inputs + flags + seed**. Same seed ⇒ identical run; a different seed ⇒ a different but
equally reproducible run. The `fixed` default keeps the historical no-RNG guarantee (the seed is ignored).

---

## 9. Liquidity-rewards model (`src/rewards.rs`)

Models Kalshi liquidity-incentive programs. Off by default; enable with `--rewards` (or
`[execution.rewards].enabled`). While the strategy rests **≥ `min_resting_size`** contracts within
**`max_spread_cents`** of the mid (on **both sides** if `both_sides_required`), it accrues a pro-rata share
of `reward_per_period` per `period_secs` window (qualifying-time / period). Accrued rewards are credited to
the ending balance **only when `include_rewards` is true**. Single-participant share model (no pool-splitting
across competitors); for multi-instrument runs it credits the strongest-qualifying instrument per event.

---

## 10. PnL decomposition, portfolio & metrics

`pnl_total` is an **explicit decomposition** (each term gated by its toggle):

```
pnl_total = gross_pnl_ex_costs − (fees if include_fees) − (slippage if enabled) + (rewards if include_rewards)
```

`Portfolio` (`src/portfolio.rs`) tracks cash, per-instrument `Position {net_qty, avg_cost, realized_pnl}`,
the fills log, **round-trips** (a position returning to flat / flipping sign records
`{instrument, entry_ts, exit_ts, qty, entry_price, exit_price, pnl}`), and per-instrument stats. Equity =
cash + Σ positions marked at mid.

`Metrics` (`src/metrics.rs`) computes (all surfaced in `summary`, all per-snapshot and documented as
unannualized): Sharpe, Sortino, volatility, downside volatility, max drawdown (abs + %), Calmar, win rate,
profit factor, gross profit/loss, avg win/loss, payoff ratio, expectancy, largest win/loss, max consecutive
wins/losses, avg trade PnL, exposure %, avg holding seconds, turnover, fees % of gross, total volume.

### `report.json` schema (infra-orchestrator compatible)

Printed to **stdout** between `===REPORT_JSON_START===` / `===REPORT_JSON_END===` sentinels (the
human-readable summary table goes to **stderr**, so stdout stays machine-parseable). Optionally the tearsheet
HTML is emitted base64 between `===TEARSHEET_HTML_B64_START===`/`END` with `--emit-tearsheet-b64`.

```jsonc
{
  "plugin_name": "<strategy>",
  "summary": {
    // first 9 fields are the exact infra-orchestrator contract:
    "currency", "starting_balance", "ending_balance", "pnl_total", "pnl_pct",
    "total_orders", "total_positions", "avg_buy_price", "avg_sell_price",
    // additive analytics:
    "num_trades", "num_fills", "win_rate", "sharpe", "sortino",
    "max_drawdown", "max_drawdown_pct", "turnover", "total_fees",
    "profit_factor", "gross_profit", "gross_loss", "avg_win", "avg_loss",
    "payoff_ratio", "expectancy", "num_round_trips", "avg_trade_pnl",
    "largest_win", "largest_loss", "max_consecutive_wins", "max_consecutive_losses",
    "calmar_ratio", "volatility", "downside_volatility", "exposure_pct",
    "avg_holding_secs", "fees_pct_of_gross", "total_volume_contracts",
    // execution-cost decomposition (additive):
    "total_slippage_cost", "liquidity_rewards", "gross_pnl_ex_costs",
    // binary settlement-at-expiry (additive):
    "settled_pnl", "num_settled",
    // risk-layer status & order-type counters (additive):
    "halted", "halt_reason", "risk_rejections", "post_only_rejects"
  },
  "equity_curve": [ { "ts_ns": 0, "total": 1000.0, "currency": "USD" }, ... ],
  "instrument_breakdown": [ ... ]   // additive
}
```

The trailing additive fields are produced by the new execution-realism layers (all **zero / `false` / `""`
at defaults**, so legacy runs are unchanged). Exact field set + order match `Summary` in `src/types.rs`:

| Field | Meaning |
|---|---|
| `total_slippage_cost` | Total adverse-slippage dollars charged on fills (0 when slippage off). |
| `liquidity_rewards` | Total accrued liquidity-incentive dollars (reported even when not credited). |
| `gross_pnl_ex_costs` | Pure price-movement PnL before fees/slippage and excluding rewards. |
| `settled_pnl` | PnL booked from **settling** open positions at expiry vs their known $1/$0 outcome (see §12). 0 with no settlement map. |
| `num_settled` | Number of positions settled at expiry (vs flattened at mid). 0 with no settlement map. |
| `halted` | `true` if the risk layer HALTED the run (equity-floor or max-drawdown breach — see §13). |
| `halt_reason` | Human-readable halt cause (`""` when not halted). |
| `risk_rejections` | Count of orders the risk layer dropped or clamped to zero qty. |
| `post_only_rejects` | Count of `post_only` limit orders rejected because they were marketable at activation (see §11). |

### `--out-dir` structured exports (for the dashboard)

When `--out-dir DIR` is set, the run also writes: `report.json`, `equity.csv`
(`ts_ns,total,currency,drawdown`), `fills.csv` (`ts_ns,instrument,order_id,side,price,qty,liquidity,fee`),
`trades.csv` (`ts_ns,instrument,aggressor_side,price,size`), `round_trips.csv`
(`instrument,entry_ts_ns,exit_ts_ns,qty,entry_price,exit_price,pnl`), `instrument_stats.csv`
(`instrument,pnl,num_fills,num_round_trips,net_position,volume`), and `meta.json`.

---

## 11. Order types & time-in-force (`Ctx::place_limit_ex`)

**Strategy-facing — there is no CLI flag.** Strategies place limit orders through the `Ctx` trait
(`src/strategy.rs`). The simple `place_limit(inst, side, price, qty)` is exactly
`place_limit_ex(inst, side, price, qty, Tif::Gtc, post_only=false)`, so existing strategies are unchanged. The
richer form exposes a time-in-force and a post-only flag:

```rust
ctx.place_limit_ex(inst, side, price, qty, tif, post_only);   // tif: Tif::Gtc | Tif::Ioc
```

**Marketability is judged at activation** (send + latency, §8), against the book *as of that later tick*: a
**BUY** at price `P` is marketable iff `P ≥ best ask`; a **SELL** at `P` iff `P ≤ best bid`. Every take is
bounded by `P` and never fills past it (it reuses the same latency-deferred taker machinery as
`place_market`). Behaviour:

| `tif` | `post_only` | If marketable at activation | If not marketable |
|---|---|---|---|
| `Gtc` *(default)* | `false` | Crossing portion **takes** (taker, bounded by `P`); any **remainder RESTS** as a maker. | Whole order rests. |
| `Ioc` | `false` | Crossing portion **takes**; unfilled **remainder is cancelled** (never rests). | Nothing happens. |
| `Gtc`/`Ioc` | `true` | **REJECTED in full** (post-only guarantees maker-only); counted in `summary.post_only_rejects`. | Rests normally. |

`post_only=true` overrides the take behaviour of both GTC and IOC — a post-only order is never allowed to
cross. `Tif` defaults to `Gtc`, reproducing the historical `place_limit` semantics exactly. Fills carry a
`Liquidity` of `Maker`, `Taker`, or `Settle` (see §12).

---

## 12. Binary settlement at expiry (`--settlements` / `[execution.settlement]`)

Kalshi markets are cash-or-nothing: at resolution a YES contract pays **\$1** and a NO contract pays **\$0**.
By default, any position still open at end-of-run is **flattened at the last mid** (the historical behaviour).
Point `--settlements <path>` (or `[execution.settlement].path`) at a settlement file and, at the end of the
run, every open position whose instrument has a **KNOWN outcome** is instead **SETTLED** to its binary
payoff:

- a net position of `q` YES contracts settles to cash `q · payout`, `payout = $1.00` (YES) or `$0.00` (NO);
- realized PnL booked is `q · (payout − avg_cost)`, recorded as a `Liquidity::Settle` fill with **zero fee**
  (Kalshi charges no settlement fee) and no slippage. Settlement **takes precedence over flatten**.
- Instruments **absent** from the file have an `Unknown` outcome and still **flatten at mid**.

Summary gains `settled_pnl` (Σ settlement PnL) and `num_settled` (count of settled positions); both are 0 when
no map is provided.

**File formats** (auto-detected, lenient result tokens — `yes/no`, `y/n`, `true/false`, `1/0`, case-insensitive):

```text
# CSV (instrument_id,result; optional header auto-skipped)
instrument_id,result
KXNATGASD-26JUN05-T3.5,yes
KXNATGASD-26JUN05-T4.0,no
```
```jsonc
{"KXNATGASD-26JUN05-T3.5": "yes", "KXNATGASD-26JUN05-T4.0": "no"}   // or an array of {instrument_id,result}
```

Build a real settlement file from Kalshi outcomes with the helper `adapters/fetch_settlements.py` (it can read
the instruments from `--instruments`, an `--ndjson` capture, or `--from-clickhouse`, and pulls each market's
resolved outcome from the Kalshi REST API).

---

## 13. Hard risk limits (`[execution.risk]` / risk flags)

Engine-enforced risk controls. **Every limit is OPTIONAL and disabled by default**, so omitting them changes
nothing (the engine takes the original check-free path). There are two kinds of control:

**Order/position CLAMPS** (a position-**reducing**/flattening order is **NEVER blocked**):

| Flag | `[execution.risk]` key | Effect |
|---|---|---|
| `--max-order-qty <f64>` | `max_order_qty` | Cap a single order's contract qty (larger orders clamped down). |
| `--max-position <f64>` | `max_position_per_instrument` | Cap `\|net qty\|` for any one instrument; an opening order is reduced so the resulting `\|net\|` exactly hits the cap. |
| `--max-gross <f64>` | `max_gross_position` | Cap `Σ\|net qty\|` (gross exposure) across all instruments; opening orders reduced to keep gross ≤ cap. |

A clamp that drops an order or reduces it to zero qty increments `summary.risk_rejections`.

**Equity HALTS** (a hard stop on breach):

| Flag | `[execution.risk]` key | Effect |
|---|---|---|
| `--equity-floor <f64>` | `equity_floor` | HALT if equity ≤ this at any step. |
| `--max-drawdown-pct <f64>` | `max_drawdown_pct` | HALT if drawdown from the running equity peak ≥ this percent (e.g. `50` = 50%). |

On a halt the engine **cancels all resting orders and immediately flattens every position with market orders
that BYPASS latency**, then **ignores all further strategy orders** for the rest of the run. Summary then
reports `halted = true` and a `halt_reason` string. All risk checks are no-ops when their limits are unset.

---

## 14. Maker-queue model (`--queue-model` / `[execution.queue]`)

We can't observe our true FIFO position from L2 data, so two models bracket the extremes (§6: each resting
order carries a `queue_ahead`). Default `pessimistic` reproduces the original behaviour exactly.

| `--queue-model` | `[execution.queue].model` | Behaviour |
|---|---|---|
| `pessimistic` *(default)* | `pessimistic` | Cancellations ahead of you **don't help** — `queue_ahead` only burns down on actual **trades** (conservative; you fill last). |
| `optimistic` | `optimistic` | When the resting size at **your** price level shrinks via a **cancel**, your `queue_ahead` drops by that amount (assume the cancelled size was ahead of you), so you fill sooner. |

The bundled **`queue_probe`** strategy (§16) is built to exercise this: it *joins the touch* (rests behind
existing depth at the best bid/ask, so its orders carry nonzero `queue_ahead`) and leaves a quote in place
while it stays at the touch — so the `pessimistic` vs `optimistic` choice visibly changes its fills.

---

## 15. Data sources (`--source`) — tick-level only

Backtests run **only on tick data** (orderbook deltas + trades). There is no candle / bar mode.

| `--source` | Reads | Notes |
|---|---|---|
| `ndjson` | `--ndjson <file.ndjson(.gz)>` | The collector's raw capture (lines `{"kind":"delta"\|"trade", ...}`). True tick fidelity. The simplest source — needs no ClickHouse. |
| `clickhouse` | `--clickhouse <url>` | The `kalshi.orderbook_deltas`/`trades` warehouse. Requires building `--features clickhouse`. `--ch-config` remaps a slightly different schema without recompiling. |
| `adapter` | `--adapter <key> --venue <TAG> --adapter-path <path>` | Resolve a venue adapter via the registry. Combine with repeatable `--extra-source` (or a `[[sources]]` config list) for **multi-venue** runs merged time-ordered. Adapter keys: `kalshi_ndjson`, `generic_ndjson`, `generic_csv`, `polymarket`, `hyperliquid` (all tick-level). |

All sources also honor `--instrument <glob>` (exact, or trailing `%`/`*` prefix) and `--start`/`--end`
(inclusive/exclusive `YYYY-MM-DD`, UTC).

### 15a. Integrating a NEW data stream — the 3 paths

Pick the cheapest path that fits your data, then **always `validate --preview` first** (§15c).

1. **Already canonical NDJSON** — if your producer can emit one JSON object per line in the canonical
   contract (`{"kind":"delta"|"trade","ts_ns":…,"instrument":…,"action":…,"side":…,"price":…,"size":…,
   "sequence":…,"is_snapshot":…}`, prices in dollars 0–1), just point `--source ndjson --ndjson file`
   (or `--source adapter --adapter kalshi_ndjson`) at it. Zero config.

2. **Any format → `adapters/convert/to_canonical.py`** — for **Parquet, complex CSV, snapshot/L2 feeds, ISO
   timestamps, NO-side quotes, or snapshot-diffing**, use the Python converter. It maps any
   CSV/Parquet/DataFrame into canonical NDJSON via a small JSON/TOML *ingest profile* (`--infer`
   guesses one; `--preview` shows the first events). This is the recommended path for anything
   non-trivial — the backtester adds **no** Parquet/arrow deps. See `adapters/convert/README.md`.

3. **In-Rust generic adapter (+ profile)** — for a **row-per-event CSV or NDJSON** that just has
   different column/field names or units, skip Python: use `--source adapter --adapter generic_csv`
   (or `generic_ndjson`) and describe the field mapping. Run `kalshi-backtest list-adapters` to see
   every adapter and the exact `mapping` keys it understands (with defaults). Supply the mapping
   inline (`validate --map price=px --map ts_unit=ms`) or, better, in a reusable **adapter profile**
   file via `--adapter-profile` (§15b).

#### Generic-adapter mapping keys (opt-in; defaults unchanged)

Both `generic_ndjson` and `generic_csv` take a `mapping` of *canonical → source name* plus these knobs:

| Key | Meaning |
|---|---|
| `ts_ns,instrument,kind,price,size,side,action,sequence,is_snapshot,aggressor_side,trade_id` | source field/column name for each canonical field (default = the canonical name). |
| `ts_unit` *(CSV)* | `s\|ms\|us\|ns` — scales a non-ns timestamp up to ns (default `ns`). |
| `price_scale` | `dollars\|cents\|bps\|prob` — how the raw price maps to internal cents (default `dollars`; `cents`→42, `bps`→4200, `prob`/`dollars`→0.42 all give 42¢). Generalizes the old `price_is_cents`, which still works as an alias for `price_scale=cents`. |
| `side_from_sign` | name of a **signed** size/qty column; the side is derived from its SIGN (positive→Bid, negative→Ask, `|value|`→size) when there is no explicit `side` column. |
| dotted paths *(NDJSON only)* | a mapping value may be a dotted path into nested JSON, e.g. `price = "data.px"` reads `{"data":{"px":…}}`. |

### 15b. Adapter profile files (`--adapter-profile`)

Keep a reusable mapping out of the CLI in a JSON or TOML file and pass `--adapter-profile <path>`
(usable on `backtest`, `optimize`, `walk-forward`, and `validate`, applied to the primary
`--source adapter` source). It may also pin `adapter`, `venue`, and `instrument`. **CLI flags and
`--map` override file values.**

```json
{
  "adapter": "generic_csv",
  "venue":   "POLYMARKET",
  "instrument": "0x%",
  "mapping": {
    "ts_ns": "ts_ms", "ts_unit": "ms",
    "instrument": "symbol",
    "price": "price_cents", "price_scale": "cents",
    "size": "size", "side": "side"
  }
}
```

The key names mirror `adapters/convert/to_canonical.py` profiles where it makes sense, so the two are easy to keep
in sync. Two conveniences make the common case interchangeable: a top-level `ts_unit` / `price_scale`
and any `*_col` key (e.g. `price_col = "px"` ⇒ `mapping.price = "px"`) are folded into `mapping` on
load. **Differences vs the Python profile:** snapshot-mode / NO-side complement / snapshot-diffing and
ISO timestamp parsing are converter-only (use path 2 for those); this profile targets the
row-per-event generic adapters.

### 15c. `validate` — see it working before a full run

```
kalshi-backtest validate --source <ndjson|clickhouse|adapter> [same source/instrument/date flags as backtest] \
    [--adapter --venue --adapter-path --map k=v --adapter-profile p.json] [--preview N]
```

`validate` loads the source with the **real loaders** and prints (a) a SUMMARY (events, deltas/trades,
snapshot rows, distinct instruments + first ids, time span, per-instrument table — reusing
`describe-data`'s logic), (b) the first `--preview N` (default 10) parsed `MarketEvent`s in
human-readable form (ts→UTC, instrument, delta/trade fields) so you can eyeball your mapping, and
(c) non-fatal **data-quality WARNINGS** — prices outside (0,1], zero/negative sizes, out-of-order
events, instruments with no snapshot row, all-same timestamps, empty result — each specific and
actionable (e.g. *"12 delta rows have a price outside (0,1] — likely a price_scale mistake"*). It
exits 0 even with warnings; only a source that can't load at all is an error (with an actionable
message). Drop a file, `validate --preview`, fix the mapping, repeat — then run `backtest`.

---

## 16. Strategies (`--strategy`) and their parameters

Tune any param with repeatable `--strategy-param key=value` (alias `--param`), or a `[strategy_params]`
config table. **Omitted keys use the strategy default**, so an empty set reproduces default behavior exactly.

| Strategy | What it does | Params (= default) |
|---|---|---|
| `noop` | Observes everything, trades nothing (sanity baseline). | (none) |
| `momentum` | Long when mid rises N ticks over a window; flatten on reversal. | `up_ticks=2, size=10, window=10` |
| `mean_reversion` | Buy when mid z-score is very negative; flatten on revert. | `window=20, entry_z=1, exit_z=0.25, size=10` |
| `market_maker` | Quote resting bid+ask around mid with half-spread + inventory skew. | `half_spread_cents=2, quote_size=10, skew=0.05, max_inventory=50` |
| `queue_probe` | JOIN the touch (rest at best bid AND best ask, behind existing depth) and keep each quote in place while it stays at the touch — built to exercise the maker-queue model (§14). | `quote_size=10, max_inventory=100` |
| `imbalance` | Trade persistent top-of-book imbalance / microprice lean (EMA). | `imbalance_threshold=0.35, micro_edge=0, ema_period=10, size=10, max_inventory=40` |
| `breakout` | Long on N-window range breakout; exit on revert or hard stop. | `window=20, stop_dollars=0.05, size=10, max_inventory=30` |
| `cross_venue_arb` | Watch one underlying on two venues; buy cheaper / sell richer; flatten on convergence. | `entry_edge=0.03, exit_edge=0.01, size=10` (venues/legs set via `[[sources]]`) |
| `avellaneda_stoikov` | Inventory-aware optimal MM (reservation price + optimal spread). | `gamma=0.1, kappa=1.5, sigma_window=30, horizon_secs=3600, quote_size=10, max_inventory=50` |
| `template` | Minimal copy-me z-score example (starting point for new ideas). | `window=20, entry_z=1.5, exit_z=0.25, size=10` |

**Avellaneda–Stoikov math:** reservation price `r = s − q·γσ²(T−t)` (skews fair value against signed
inventory `q`); optimal half-spread `δ = ½γσ²(T−t) + (1/γ)·ln(1+γ/κ)` (a risk term + a liquidity term). `s` =
mid, `σ` = rolling stdev of mid (`sigma_window`), `(T−t)` decays linearly from 1.0 over `horizon_secs`
(floored at 0.05, since the stream carries no settlement time); quotes `r ± δ` are converted to cents,
clamped `[1,99]`, capped by `max_inventory`.

To add a strategy: copy `src/strategies/template.rs`, implement `Strategy` + `from_params`, register one line
in `src/strategies/mod.rs`. The `toolkit.rs` helpers (`RollingWindow`, `Ema`, `Signal`, `PositionSizer`,
`BaseStrategy`) make it ~20 lines.

---

## 17. Complete CLI reference

### Top-level
```
kalshi-backtest [OPTIONS] <COMMAND>
  Commands: backtest | optimize | walk-forward | list-strategies | list-adapters | validate | list-instruments | describe-data | init-config
  -v, --verbose   (repeatable) more logs/progress on stderr
  -q, --quiet     near-silent stderr (wins over -v); stdout report unaffected
  -V, --version
```

### `backtest` flags

| Flag | Meaning |
|---|---|
| `--config <file>` | Load a full run spec (TOML/JSON). CLI flags below override file fields. |
| `--source <ndjson\|clickhouse\|adapter>` | Tick data source (required unless from `--config`). |
| `--ndjson` / `--clickhouse` | Source path/URL matching `--source`. |
| `--ch-config <file>` | ClickHouse schema-map (rename db/tables/columns w/o recompiling). |
| `--adapter <key>` / `--venue <TAG>` / `--adapter-path <path>` | For `--source adapter`. See `list-adapters`. |
| `--adapter-profile <p.json\|toml>` | Reusable field mapping/profile for the primary `--source adapter` source (may also pin adapter/venue/instrument). CLI flags override file values. §15b. |
| `--extra-source 'adapter=…,venue=…,path=…[,instrument=…]'` | Repeatable; merge an extra venue source. |
| `--instrument <glob>` | Exact or trailing `%`/`*` prefix filter. |
| `--start <YYYY-MM-DD>` / `--end <YYYY-MM-DD>` | Inclusive start / exclusive end (UTC). |
| `--strategy <name>` | One of the 10 strategies (required unless from `--config`). |
| `--strategy-param key=value` | Repeatable (alias `--param`); overrides `[strategy_params]`. |
| `--starting-balance <f64>` | Opening cash (default 1000). |
| `--tearsheet <path>` | Write the standalone HTML tearsheet. |
| `--emit-tearsheet-b64` | Also print tearsheet base64 between sentinels. |
| `--out-dir <dir>` | Write report.json + the 6 CSV/JSON exports (§10). |
| `--exec-config <file.json>` | Load an ExecutionConfig preset (flags below override it). |
| **Fees** `--no-fees` | Don't charge fees to PnL. |
| **Rewards** `--rewards` | Enable liquidity-rewards + credit them to PnL. |
| `--reward-per-period <f64>` `--reward-period-secs <i64>` `--min-resting-size <f64>` `--max-spread-cents <i32>` | Reward model params. |
| **Latency** `--latency-ns <i64>` `--cancel-latency-ns <i64>` `--md-latency-ns <i64>` `--jitter-ns <i64>` | Any > 0 enables the latency model. |
| **Latency dist** (§8a) `--latency-dist <fixed\|uniform\|normal\|exponential\|empirical>` | Model latency as a seeded distribution (default `fixed`). Non-fixed enables latency. |
| `--latency-min-ns` `--latency-max-ns` `--latency-std-ns` `--latency-mean-ns` `--latency-empirical <file>` `--latency-seed <u64>` | Distribution params + PRNG seed (see §8a). |
| **Slippage** `--slippage-ticks <i32>` `--slippage-bps <f64>` | Extra adverse taker cost (cents / fraction of notional); > 0 enables slippage. |
| **Queue** (§14) `--queue-model <pessimistic\|optimistic>` | Maker-queue model (default `pessimistic`). |
| **Settlement** (§12) `--settlements <path>` | CSV/JSON instrument→outcome; settle held positions to $1/$0 at expiry (else flatten at mid). |
| **Risk** (§13) `--max-order-qty <f64>` `--max-position <f64>` `--max-gross <f64>` | Order/position CLAMPS (reducing/flattening never blocked). |
| `--equity-floor <f64>` `--max-drawdown-pct <f64>` | HALT the run on breach (cancel + flatten, ignore later orders). |

### `optimize` flags

Parallel parameter sweep over **one** parsed dataset. Parses the gzip+JSON **exactly once**, runs the full
cartesian product of the `--param` grids across worker threads, ranks every combo by `--metric`, and (with
`--out-dir`) writes `optimize_results.csv` + the best combo's `report.json`. Deterministic (config-order
output). Reuses **all** the `backtest` source / fees / rewards / latency / slippage / queue / settlement /
risk flags above.

| Flag | Meaning |
|---|---|
| `--source` `--ndjson`/`--clickhouse`/`--ch-config` `--adapter`/`--venue`/`--adapter-path` `--extra-source` | Data source (required), same as `backtest`. |
| `--instrument <glob>` `--start`/`--end` | Filters, same as `backtest`. |
| `--strategy <name>` | Strategy to sweep (**required**). |
| `--param 'name=v1,v2,v3'` | One sweep **axis** (repeatable, one per parameter). Each value parses as f64; full cartesian product, capped at **5000** combos. **Required.** |
| `--metric <m>` | Metric to MAXIMIZE: `pnl_total` (default), `sharpe`, `sortino`, `calmar_ratio`, `win_rate`, `profit_factor`, `ending_balance`, `expectancy`. |
| `--starting-balance <f64>` | Opening cash for every run (default `1000`). |
| `--threads <n>` | Worker threads (default = machine parallelism). |
| `--out-dir <dir>` | Write `optimize_results.csv` + best `report.json`. Omit to print only. |
| *(all exec/latency/slippage/queue/settlement/risk flags)* | Applied identically to every combo. |

### `walk-forward` flags

Rolling out-of-sample validation. Splits the dataset into **K+1** contiguous, equal-size, time-ordered
segments; for each fold `i` it **optimizes** the `--param` grid on segment `i` (train) and measures the chosen
config's **OUT-OF-SAMPLE** performance on segment `i+1` (test). Reports per-fold + an aggregate "does it
generalize" OOS number; with `--out-dir` writes `walk_forward.csv` + `walk_forward_oos.json`. Same flags as
`optimize`, **plus**:

| Flag | Meaning |
|---|---|
| `--windows <K>` | Number of folds K (**required**). Dataset split into K+1 segments; fold `i` trains on `i`, tests OOS on `i+1`. |

### Utility subcommands
- `list-strategies` — every strategy + description + `param=default`.
- `list-adapters` — every data adapter + default venue + description + the `mapping` keys it understands (with defaults). The discoverability companion to `list-strategies` (§15a).
- `validate --source … [source/instrument/date flags] [--adapter-profile p.json] [--preview N]` — load a source, print a summary + the first N parsed events + data-quality warnings, **before** a full run (§15c). Exits 0 unless the source can't load at all.
- `list-instruments --source … [paths]` — distinct instruments with counts + time span.
- `describe-data --source … [paths]` — totals: events, snapshots, deltas, trades, instruments, time span.
- `init-config <file> [--json]` — write the fully-commented config template (the authoritative field list).

---

## 18. Config file (`--config`) schema

A run spec in TOML (default) or JSON; **every field optional, shown with its default**. CLI flags override
matching fields. Top-level keys: `source`, `ndjson`/`clickhouse`/`ch_config`, `instrument`,
`start`, `end`, `strategy`, `[strategy_params]`, `starting_balance`, `tearsheet`, `out_dir`, optional
`[[sources]]` list (multi-venue), and the `[execution]` block:

```toml
[execution]
include_fees = true       # charge fees to PnL
include_rewards = false   # credit accrued rewards to PnL

[execution.latency]       # NO-OP at defaults
enabled = false
order_latency_ns = 0
cancel_latency_ns = 0
market_data_latency_ns = 0
jitter_ns = 0             # deterministic per-order jitter (used only by the `fixed` dist)
seed = 0                  # PRNG seed for stochastic dists (same seed => identical run); unused by `fixed`

[execution.latency.dist]  # §8a — omit for `fixed` (the default = today's behaviour)
kind = "fixed"            # fixed | uniform | normal | exponential | empirical
# uniform:     min_ns, max_ns
# normal:      mean_ns, std_ns        (clamped >= 0)
# exponential: mean_ns
# empirical:   path = "../data/latency_ns.txt"   (sampled with replacement)

[execution.slippage]
enabled = false
taker_ticks = 0
taker_bps = 0.0                    # fraction of notional, 0.0005 = 5 bps
maker_adverse_selection_bps = 0.0

[execution.rewards]
enabled = false
period_secs = 3600
reward_per_period = 0.0
min_resting_size = 10.0
max_spread_cents = 4
both_sides_required = true

[execution.queue]         # §14 — maker-queue model (CLI --queue-model overrides)
model = "pessimistic"     # pessimistic (default) | optimistic

[execution.risk]          # §13 — ALL keys OPTIONAL & DISABLED by default (omit => no-op)
# max_order_qty = 100               # cap a single order's qty
# max_position_per_instrument = 500 # cap |net| per instrument (reducing orders exempt)
# max_gross_position = 1000         # cap Σ|net| across instruments
# equity_floor = 0.0                # HALT if equity <= this
# max_drawdown_pct = 50.0           # HALT if drawdown from peak reaches this %

[execution.settlement]    # §12 — omit `path` => flatten-at-mid as before
# path = "../data/settlements.csv"  # CSV/JSON instrument->outcome; settle to $1/$0 at expiry
```

**With everything at default, `[execution]` is a no-op:** no latency (`fixed` dist), no slippage, fees ON,
rewards OFF, `pessimistic` queue, no risk limits, flatten-at-mid (no settlement) — so a plain backtest
reproduces the simplest model exactly. Generate the full annotated template any time with
`kalshi-backtest init-config run.toml`.

---

## 19. Default values (quick reference)

| Setting | Default |
|---|---|
| `starting_balance` / `currency` | `1000.0` / `USD` |
| `fee_bps_formula` (Kalshi formula) / `maker_fee` / `taker_fee_rate` | `true` / `0.0` / `0.01` |
| `equity_snapshot_secs` | `1` |
| `flatten_at_end` | `true` |
| `include_fees` / `include_rewards` | `true` / `false` |
| latency (all) | `0`, disabled |
| `latency.dist.kind` / `latency.seed` | `fixed` (deterministic hash-jitter, no RNG) / `0` |
| slippage (all) | `0`, disabled |
| rewards `period_secs` / `min_resting_size` / `max_spread_cents` / `both_sides_required` | `3600` / `10` / `4` / `true` |
| `queue.model` | `pessimistic` |
| risk limits (`max_order_qty`/`max_position_per_instrument`/`max_gross_position`/`equity_floor`/`max_drawdown_pct`) | all `None` (disabled) |
| `settlement.path` | unset (flatten-at-mid) |
| `optimize`/`walk-forward` `--metric` / `--starting-balance` / cartesian-product cap | `pnl_total` / `1000` / `5000` combos |

---

## 20. Determinism & reproducibility

The simulation is fully deterministic: integer-cent prices, no RNG by default (jitter is a hash of the order
sequence), no wall-clock, and a fixed event order. With the default `fixed` latency distribution, same inputs
+ same flags ⇒ byte-identical `report.json`. This is why the latency jitter is hashed rather than random, and
why `meta.json` stamps `generated_unix_ns: 0` (stamped externally) — so cached/resumed runs match.

**One caveat (§8a):** a non-`fixed` `--latency-dist` draws per-order latency from a seeded SplitMix64 PRNG, so
the guarantee becomes *reproducible given inputs + flags + `--latency-seed`* (same seed ⇒ identical run). The
`optimize` and `walk-forward` sweeps remain deterministic — they report in config order regardless of thread
scheduling.

---

## 21. Worked examples

```bash
# Simplest run — a tick NDJSON capture + a strategy:
kalshi-backtest backtest --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy imbalance --out-dir out/

# THE point of this tool — model latency: the SAME run at 1 second of order latency (fills move):
kalshi-backtest backtest --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy imbalance --latency-ns 1000000000 --out-dir out_lat/

# Tune strategy params + layer on slippage:
kalshi-backtest backtest --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --strategy market_maker --strategy-param half_spread_cents=3 --slippage-ticks 1 --out-dir out/

# True-tick backtest from the ClickHouse warehouse (build with the feature first):
cargo build --release --features clickhouse
kalshi-backtest backtest --source clickhouse --clickhouse http://localhost:8123 \
    --instrument 'KXNATGASD-%' --strategy imbalance --out-dir out/

# Reproducible run from a config file (CLI overrides the file):
kalshi-backtest init-config run.toml
kalshi-backtest backtest --config run.toml --strategy avellaneda_stoikov --param gamma=0.5

# Risk-guarded run (§13): cap gross exposure + HALT if equity drops below $900:
kalshi-backtest backtest --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy market_maker \
    --max-gross 100 --equity-floor 900 --max-drawdown-pct 50 --out-dir out_risk/
#   On a breach: halted=true, halt_reason set, positions flattened (bypassing latency).

# Stochastic latency (§8a): draw each order's latency from a seeded normal dist (mean 500ms, std 300ms):
kalshi-backtest backtest --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy imbalance \
    --latency-dist normal --latency-ns 500000000 --latency-std-ns 300000000 --latency-seed 42

# Maker-queue model (§14): the queue_probe strategy under the optimistic queue:
kalshi-backtest backtest --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy queue_probe --queue-model optimistic --out-dir out_q/

# Binary settlement at expiry (§12): settle held positions to $1/$0 instead of flatten-at-mid:
python adapters/fetch_settlements.py --ndjson data/tick/natgas_tick_demo.ndjson.gz --out settlements.csv
kalshi-backtest backtest --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy momentum --settlements settlements.csv --out-dir out_settle/
#   => summary.settled_pnl / num_settled populated for resolved markets.

# post_only / IOC (§11) are STRATEGY-FACING (no CLI flag) — inside a strategy:
#   ctx.place_limit_ex(inst, Side::Bid, Cents(42), 10.0, Tif::Gtc,  true);  // post-only maker (rejected if it would cross)
#   ctx.place_limit_ex(inst, Side::Bid, Cents(42), 10.0, Tif::Ioc, false);  // take the crossing part, cancel the rest

# OPTIMIZE (§17): sweep market_maker params in parallel, rank by Sharpe (parse happens ONCE):
kalshi-backtest optimize --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy market_maker \
    --param 'half_spread_cents=1,2,3' --param 'quote_size=5,10,20' \
    --metric sharpe --out-dir out_opt/        # => optimize_results.csv + best report.json

# WALK-FORWARD (§17): 4 rolling train/test folds, measure OUT-OF-SAMPLE generalization:
kalshi-backtest walk-forward --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy market_maker \
    --param 'half_spread_cents=1,2,3' --windows 4 \
    --metric pnl_total --out-dir out_wf/       # => walk_forward.csv + walk_forward_oos.json
```

> **Realism reminder:** the headline knob is `--latency-ns`. A strategy that looks profitable at zero latency
> can flip to losing once orders only become effective at `order_send + latency` and market orders execute
> against the book *as of* that later tick. Always sanity-check your edge under realistic latency.
