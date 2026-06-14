# Tick-Level Kalshi NatGas Backtester — Master Plan

> Status: living document. Built autonomously 2026-06-12. Grounded in the `oracletrading/infra-orchestrator`
> code survey (see the `InfraOrchestrator` skill) and the live Kalshi WebSocket API spec.

## 0. Goal

Two deliverables, one pipeline:

1. **Obtain every orderbook delta for every tick** for Kalshi natural-gas (`KXNATGASD`) markets over a month.
2. **Backtest strategies on that data, end-to-end, fully in Rust.**

## 1. The hard reality (why this design)

From the infra-orchestrator survey and the Kalshi API:

- Kalshi's **historical REST API has no orderbook history** — only 1-minute candlesticks (best YES bid/ask OHLC
  + volume + OI). This is what `~/kalshi_commodities/collect_kalshi.py` already pulls. Confirmed by that file's
  own docstring and by Kalshi's docs.
- Full L2 depth and **per-event deltas exist only on the live WebSocket** (`orderbook_snapshot` then
  `orderbook_delta`). They are **not archived publicly** for past months.
- infra-orchestrator gets deltas two ways: (a) a **live collector** (`infra-trader-run-collector`) that streams
  the Kalshi WS into QuestDB **going forward**, and (b) **Dome API backfill**, which returns **periodic full-book
  snapshots, not native deltas**. Both require infra we cannot reach from this laptop (in-cluster QuestDB at
  `questdb.data.svc.cluster.local`, Dome API keys).

**Therefore** (the pivot, per operator instruction "if you can't query QuestDB build your own collector, use
ClickHouse"): we build a **self-contained local stack** that reproduces the infra-orchestrator data contract:

```
 Kalshi WS  ──►  collector  ──►  raw NDJSON/parquet on disk  (ALWAYS, durable capture)
 (orderbook_      (Python)   └─►  ClickHouse tables          (when the DB is up; queryable warehouse)
  snapshot/                       orderbook_deltas / trades   (schema MIRRORS infra-orchestrator)
  delta/trade)
                                          │
                                          ▼
                              Rust backtester (this repo's core)
                  reads ClickHouse OR parquet/NDJSON  ──►  reconstruct L2 book
                  ──► event-driven engine ──► Strategy trait ──► fills+fees+PnL
                  ──► metrics + equity curve ──► report.json (infra-orchestrator compatible)
                                                + HTML tearsheet
```

Every native delta for a month is captured **going forward** by our collector. Past months at native
tick granularity are physically unavailable from public Kalshi; the closest is Dome snapshot-granularity
(needs keys) or the 1-min candlesticks we already have. The backtester therefore accepts **three data
fidelities** and is honest about which it's using:

| Fidelity | Source | Granularity | Availability |
|---|---|---|---|
| **L2 deltas** (target) | our collector → ClickHouse/parquet | every tick | going forward only |
| Full-book snapshots | Dome backfill (needs keys) | periodic | past months, if keys |
| 1-min candlesticks | `collect_kalshi.py` (have it) | 1 minute | ~2 months back, public |

## 2. Data contract (mirror of infra-orchestrator)

### `orderbook_deltas` (ClickHouse + parquet)
Columns match infra-orchestrator `services/backfill/writer.py` exactly, so anything we collect is drop-in for
their tooling and vice-versa:

| col | type | meaning |
|---|---|---|
| `timestamp` | DateTime64(9, 'UTC') | event time, nanosecond |
| `instrument_id` | LowCardinality(String) | the raw Kalshi market ticker, e.g. `KXNATGASD-26APR0817-T2.650` |
| `venue` | LowCardinality(String) | `KALSHI` |
| `action` | Enum8 | `ADD` / `DELETE` / `UPDATE` |
| `side` | Enum8 | `BUY` (=yes bids) / `SELL` (=yes asks, from no side) |
| `price` | Float64 | price in dollars [0.01, 0.99] |
| `size` | Float64 | resting contracts at that level (post-update for UPDATE/ADD) |
| `market_alias` | LowCardinality(String) | human alias (event/strike) |
| `sequence` | Int64 | Kalshi `seq`; gap detection |
| `is_snapshot` | UInt8 | 1 = first row of a full-book snapshot (book reset) |

### `trades`
`ts_event` (DateTime64(9)), `instrument_id`, `venue`, `aggressor_side` (yes/no), `price`, `size`, `market_alias`,
`trade_id`.

### Kalshi → our schema mapping (the collector's job)
- `orderbook_snapshot.msg.yes_dollars_fp = [[price, contracts], …]` → one `is_snapshot=1 ADD BUY` row per level.
- `orderbook_snapshot.msg.no_dollars_fp` → the YES-ask side: a resting NO bid at price `p` is a YES ask at
  `1 - p`. Store as `side=SELL`, `price = 1 - p` (so the book is a single YES-priced two-sided book), and keep
  raw no-side too if useful. (Decision: store YES-native book; SELL price = 1 − no_price.)
- `orderbook_delta.msg`: `{price_dollars, delta_fp, side}` → `action=UPDATE`, `size += delta` at that level;
  new size 0 ⇒ emit `DELETE`. `side=yes`→BUY, `side=no`→SELL (price flipped as above).
- `delta_fp` / `*_fp` are fixed-point **÷100** (2 decimals) per Kalshi docs ("300.00" = 300 contracts).
- `seq` monotonic per market; a gap ⇒ resubscribe (`get_snapshot`) and write a fresh `is_snapshot=1`.

## 3. Components & file layout

```
RustBacktesterTickLevelKalshiData/
├── docs/
│   ├── PLAN.md                      ← this file
│   ├── ORDERBOOK_DELTAS_HOWTO.md    ← how to get every delta for a month (jupyter + our collector)
│   └── ARCHITECTURE.md              ← infra-orchestrator distilled (also in InfraOrchestrator skill)
├── clickhouse/
│   ├── schema/01_tables.sql         ← orderbook_deltas, trades, book_features_1s (mirror)
│   └── run_clickhouse.sh            ← start local server (native binary, no docker)
├── adapters/
│   ├── kalshi_ws_collector.py       ← WS → disk NDJSON/parquet + ClickHouse
│   ├── kalshi_auth.py               ← RSA-PSS request signing
│   ├── natgas_markets.py            ← discover live KXNATGASD market tickers via REST
│   ├── book.py                      ← snapshot/delta → row reconstruction (shared logic, unit-tested)
│   ├── replay_to_clickhouse.py      ← load on-disk NDJSON → ClickHouse (idempotent)
│   └── tests/test_book.py           ← deterministic tests against synthetic messages
├── backtester/                 ← THE CORE (Cargo crate `kalshi_backtester`)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                  ← clap CLI: backtest / replay / report
│       ├── lib.rs                   ← module wiring + re-exports
│       ├── types.rs                 ← Px, Qty, Side, Action, BookDelta, Trade, Order, Fill, ...
│       ├── data/                    ← loaders: ndjson.rs, parquet.rs, clickhouse.rs, candles.rs
│       ├── orderbook.rs             ← L2 book reconstruction from deltas (snapshot resets, seq gaps)
│       ├── engine.rs                ← event-driven loop merging deltas+trades by ts
│       ├── strategy.rs              ← Strategy trait + Context (place/cancel orders)
│       ├── strategies/              ← mean_reversion.rs, momentum.rs, market_maker.rs, noop.rs
│       ├── execution.rs             ← matching against book, queue model, latency
│       ├── fees.rs                  ← Kalshi fee model (maker/taker, settlement)
│       ├── portfolio.rs            ← positions, cash, realized/unrealized PnL, equity curve
│       ├── metrics.rs               ← sharpe, sortino, max drawdown, win rate, turnover
│       ├── report.rs                ← report.json (infra-compatible) + sentinels + HTML tearsheet
│       └── config.rs                ← BacktestConfig (toml/CLI)
├── data/{raw,parquet}/              ← captured + derived data (gitignored)
├── bin/clickhouse                   ← native ClickHouse binary (gitignored)
└── Makefile / README.md
```

## 4. Rust backtester design (the core)

**Principles:** event-driven, deterministic, single-threaded core (no time-of-day nondeterminism), zero-copy
where cheap, exact integer cents internally (price stored as `i32` cents 1..99; size as `f64` contracts).

- **types.rs** — the shared vocabulary. `Cents(i32)`, `Qty(f64)`, `Side{Bid,Ask}`, `Action{Add,Update,Delete}`,
  `BookDelta{ts_ns, instrument, action, side, price, size, seq, is_snapshot}`, `TradeEvent`, `Order{id, side,
  price, qty, kind: Limit|Market, tif}`, `Fill{ts, order_id, price, qty, liquidity: Maker|Taker, fee}`,
  `MarketEvent` enum (Delta | Trade | Clock).
- **orderbook.rs** — `OrderBook` keyed by instrument: two `BTreeMap<Cents, Qty>` (bids desc, asks asc). Applies
  deltas; `is_snapshot=1` clears first; tracks `last_seq`, flags gaps. O(log n) best-bid/ask, depth, imbalance,
  microprice.
- **engine.rs** — merges the per-instrument delta stream and trade stream into one time-ordered
  `Iterator<MarketEvent>` (k-way merge by ts_ns). For each event: update book → call strategy hook → match
  resting strategy orders against book/trades → record fills → mark portfolio. Emits equity snapshots on a clock.
- **strategy.rs** — `trait Strategy { fn on_event(&mut self, ev:&MarketEvent, ctx:&mut Ctx); }`. `Ctx` exposes
  the book (read-only), position, cash, and `place_limit/place_market/cancel`. Strategies are pure given events.
- **execution.rs** — resting limit orders fill when the book trades through them (price-time priority approximated
  by queue-ahead size at the level; a strategy order at level L fills only after `queue_ahead` contracts trade).
  Taker/market orders walk the book. Optional latency (`order_delay_ns`).
- **fees.rs** — Kalshi general fee: `fee = ceil(0.07 × C × P × (1−P))` per contract bucket (the published
  formula), maker rebates where applicable, plus settlement at $1/$0. Configurable; default to Kalshi's trading
  fee schedule.
- **portfolio.rs / metrics.rs** — cash + per-market net position; realized PnL on offsetting fills, unrealized at
  mid/settlement; equity curve at configurable cadence. Metrics: total/%, Sharpe, Sortino, max drawdown,
  win-rate, #orders, #positions, turnover, avg buy/sell price.
- **report.rs** — emits the **infra-orchestrator `report.json` schema**: `plugin_name`, `summary{currency,
  starting_balance, ending_balance, pnl_total, pnl_pct, total_orders, total_positions, avg_buy_price,
  avg_sell_price}`, `equity_curve[{ts_ns,total,currency}]`, printed between `===REPORT_JSON_START===`/`END`
  sentinels so it drops into their `/graph` bot command. **Additive** fields (sharpe, sortino, max_drawdown,
  win_rate, trades[]) included under `summary` (they ignore unknown keys). Plus a standalone HTML tearsheet
  (equity curve + drawdown + stats table) written to `figures/` and base64-embeddable via their
  `===TEARSHEET_HTML_B64_START===` sentinel.

**Data loaders** all yield the same `BookDelta`/`TradeEvent` iterators so the engine is source-agnostic:
- `ndjson.rs` — read our collector's raw `.ndjson(.gz)`.
- `parquet.rs` — read `orderbook_deltas`/`trades` parquet (polars/arrow).
- `clickhouse.rs` — HTTP `:8123` `SELECT … FORMAT JSONEachRow` (optional; feature-gated).
- `candles.rs` — adapter that turns the existing 1-min candlestick parquet into synthetic top-of-book deltas, so
  strategies can be smoke-tested on **real natgas data we already have** today.

## 5. Build order (subagent-driven)

1. ✅ Survey infra-orchestrator (3 agents) → InfraOrchestrator skill.
2. ✅ Plan + ClickHouse schema + Kalshi WS spec.
3. Collector (Python) + book unit tests.
4. Rust crate: foundation (Cargo.toml, types.rs, lib.rs, traits) written by orchestrator; then parallel subagents
   implement isolated modules (orderbook, engine, execution+fees, portfolio+metrics, report, loaders, strategies)
   against the fixed type contract; orchestrator integrates + `cargo build`/`test` loop.
5. Generate a synthetic + candle-derived dataset; run an end-to-end backtest; produce report.json + tearsheet.
6. README, Makefile, ORDERBOOK_DELTAS_HOWTO.md. Verify everything compiles and the demo backtest runs.

## 6. Definition of done

- `cargo build --release` clean; `cargo test` green.
- `make demo` runs a backtest on real natgas candle-derived data and emits `report.json` (valid against the
  infra schema) + an HTML tearsheet in `figures/`.
- Collector unit tests green; collector runs against Kalshi WS when `KALSHI_API_KEY_ID`/`KALSHI_PRIVATE_KEY` are
  present (documented; not runnable here without creds) and writes both disk + ClickHouse.
- ClickHouse schema loads into the local native binary; `replay_to_clickhouse.py` round-trips.
- Docs explain exactly how to pull a month of deltas (live collector going forward; Dome/candles for history).
