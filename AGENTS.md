# AGENTS.md — how to safely extend this codebase

Read this first. It tells an AI agent (or any contributor) **exactly** how to add to this project without
breaking it. It is short on purpose; the deep reference is [`backtesterDescription.md`](backtesterDescription.md)
(engine internals + every config switch) and [`strategyFormat.md`](strategyFormat.md) (the strategy contract).

This repo does **one thing**: backtest strategies on **tick-level** Kalshi (and any-venue) orderbook data, with a
**realistic latency fill model**, and view results in an **interactive dashboard**. Keep changes in service of that.

---

## The 6 invariants — never violate these

1. **Tests stay green, zero warnings.** Before claiming done, run BOTH:
   `cd backtester && cargo test && cargo test --features clickhouse && cargo build --release`.
   No new compiler warnings. (Pre-existing clippy *style* lints are tolerated; new `cargo build`/`test` warnings are not.)
2. **New features default to a byte-for-byte NO-OP.** Any new toggle/model must, at its default, produce an
   IDENTICAL `report.json` to before. Prove it with a test (see `no_settlements_reproduce_report_byte_for_byte`,
   `risk_defaults_reproduce_report_byte_for_byte`, `queue_default_reproduces_report_byte_for_byte` for the pattern).
3. **The `report.json` contract is sacred.** `summary`'s **first 9 fields stay first and unchanged**
   (`currency, starting_balance, ending_balance, pnl_total, pnl_pct, total_orders, total_positions,
   avg_buy_price, avg_sell_price`). New fields are **additive**, appended after the existing ones in `types.rs`.
4. **Determinism.** Same inputs + same flags + same `--seed` ⇒ identical run. No `Date::now`/`rand`. Randomness
   (e.g. stochastic latency) uses the seeded `SplitMix64` PRNG. New `HashMap`-iteration over positions in any
   summed quantity is suspect (it causes last-digit float noise — prefer ordered iteration if you add such code).
5. **Don't break the running collector.** A live `kalshi_ws_collector.py` may be running off
   `adapters/{book,sinks,kalshi_auth,natgas_markets}.py`. Extend the collector ADDITIVELY (new files under
   `adapters/feeds/`), don't edit those in place without cause.
6. **Never commit secrets.** Kalshi keys live in the gitignored `KalshiAPIKeysDONOTPUSH/`. Check `git status`
   before every commit.

---

## Repo map — two distinct, modular domains: `adapters/` (data in) and `backtester/` (backtest)

The two code domains do not cross over: `adapters/` (Python) never imports `backtester/`, and `backtester/`
(Rust) never imports `adapters/`. They communicate only through the **canonical data contract** (below) — files
on disk / ClickHouse, never code. The dashboards/UI drive the binary by subprocess (also not a code import).

```
backtester/        THE BACKTESTER (Rust). bin `kalshi-backtest`.
  src/engine.rs         per-event loop (Engine::step), order draining, latency, risk halt, settlement, finalize
  src/execution.rs      fill model: maker queue + bounded taker walk + marketable limits
  src/orderbook.rs      L2 book reconstruction (snapshot reset, add/update/delete); BookSet (per instrument)
  src/portfolio.rs      cash, positions, avg_cost, realized PnL, round-trips, equity marking, instrument stats
  src/types.rs          Cents, Side, Action, MarketEvent, Order, Fill, Tif, Liquidity, Summary, Report  ← the contract
  src/config.rs         BacktestConfig + ExecutionConfig + sub-configs (latency/slippage/rewards/risk/queue/settlement)
  src/fit_logistic.rs   logistic strike-curve fit → implied fair value (mu) + implied vol (scale)
  src/event_curves.rs   per-event strike ladder; re-fits the logistic on every delta (lazy + invalidated)
  src/fees.rs slippage.rs latency.rs rewards.rs settlement.rs metrics.rs report.rs exports.rs
  src/strategy.rs       the `Strategy` + `Ctx` traits (what strategies see, incl. implied_fair_value/implied_vol)
  src/strategies/       one .rs per strategy + toolkit.rs + mod.rs (registry) + README.md
  src/adapters/         IN-RUST data-source adapters: DataAdapter trait + generic_{ndjson,csv} + venue stubs
  src/data/             source loaders: ndjson.rs, clickhouse.rs (feature-gated), mod.rs, summary.rs
  src/optimize.rs       parse-once parallel grid + walk-forward core
  src/main.rs           clap CLI: backtest / optimize / walk-forward / list-adapters / validate / describe-data / init-config
adapters/          THE DATA ADAPTERS (Python) — everything that produces canonical data:
  book.py sinks.py kalshi_auth.py natgas_markets.py   shared collector internals
  kalshi_ws_collector.py rest_orderbook_collector.py  live Kalshi collectors → NDJSON + ClickHouse
  feeds/                multi-venue live-feed framework (add a venue = one Feed class) + Kalshi/Polymarket/HL
  convert/              to_canonical.py: ANY CSV/Parquet/feed → canonical NDJSON (+ profiles, examples, tests)
  fetch_settlements.py parquet_to_ndjson.py           settlement + parquet helpers
clickhouse/             schema + native-binary runner (no Docker) — the warehouse adapters write to
dashboard/              build_dashboard.py (interactive offline HTML) + build_latency_sweep.py
ui/                     server.py — interactive control panel (configure/run/cache/compare), drives the binary
notebooks/              clickhouse_data_inspection.ipynb
data/                   tick captures (data/tick/natgas_tick_demo.ndjson.gz) + make_synthetic_demo.py
docs/, *.md             backtesterDescription, strategyFormat, howToRun(+Windows), DECISIONS, this file
```

> Note the symmetry: `adapters/` (top level, Python) gets external data **into** the canonical format;
> `backtester/src/adapters/` (Rust) reads canonical data **into** the engine. Different layers, same idea.

## The data contract (everything normalizes to this)

- **In-memory:** `MarketEvent::{Delta(BookDelta), Trade(TradeEvent)}`, time-ordered. Prices are integer
  `Cents(1..=99)`. The book is **YES-native**: `Side::Bid` = buy YES, `Side::Ask` = sell YES; a NO bid at `q`
  is a YES ask at `1−q`. `is_snapshot=true` resets the book.
- **On disk (NDJSON, `--source ndjson`):**
  - `{"kind":"delta","ts_ns":i64,"instrument":str,"action":"ADD|UPDATE|DELETE","side":"BUY|SELL","price":f64$,"size":f64,"sequence":i64,"is_snapshot":0|1,"venue":str,"market_alias":""}`
  - `{"kind":"trade","ts_ns":i64,"instrument":str,"aggressor_side":"yes|no","price":f64$,"size":f64,"trade_id":str,"venue":str}`
- **ClickHouse:** tables `kalshi.orderbook_deltas` / `kalshi.trades` (see `clickhouse/schema/01_tables.sql`).
- `instrument` may be bare or venue-tagged `"VENUE:symbol"` (multi-venue). The exact field names are defined by
  `src/data/ndjson.rs` + `src/adapters/generic_ndjson.rs` — match them.

---

## Recipe: add a STRATEGY (no engine changes ever)

3 steps — full detail in [`backtester/src/strategies/README.md`](backtester/src/strategies/README.md):
1. Copy `src/strategies/template.rs` → `my_idea.rs`; set `name()`; write `on_event`; add a `Default` + a
   `from_params(&StrategyParams)` so it's tunable (empty params == defaults). Use `toolkit.rs`.
2. `pub mod my_idea;` + a `build` match arm in `src/strategies/mod.rs`.
3. Add the name to `ALL` and a `StrategyInfo` to `INFO` (same order — a test enforces it).
Strategies act only through `Ctx` (`place_limit`/`place_limit_ex`/`place_market`/`cancel`); they never touch the engine.

## Recipe: add a DATA ADAPTER / new feed

**First, discover + always preview:** `kalshi-backtest list-adapters` lists every adapter, its default
venue, and the `mapping` keys it understands (with defaults). Before any full run on a new feed, run
`kalshi-backtest validate --source … --preview` — it loads with the real loaders and prints a summary +
the first N parsed events (ts→UTC, instrument, delta/trade fields) + actionable data-quality warnings,
so you SEE whether the backtester understood your data before committing to a run. Exits 0 even with
warnings; only a hard load failure is an error.

Pick the **easiest path that works**:
- **Already canonical NDJSON?** Just `--source ndjson --ndjson file`. Nothing to write.
- **Any CSV/Parquet/other (complex)?** Convert with `adapters/convert/to_canonical.py` (an "ingest profile" maps your
  columns → canonical fields; `--infer` guesses, `--preview` shows events) → NDJSON → backtest. **Zero Rust.**
  The default recommendation for Parquet / snapshot-L2 / ISO-ts / NO-side / snapshot-diff feeds.
- **Row-per-event CSV/NDJSON with different names/units?** Skip Python: `--source adapter --adapter
  generic_csv|generic_ndjson` with a field `mapping`. Mapping knobs: `price_scale=dollars|cents|bps|prob`
  (alias `price_is_cents`), `ts_unit` (CSV), `side_from_sign=<signed col>`, and dotted JSON paths
  (`price=data.px`, NDJSON). Keep the mapping in a reusable file and pass `--adapter-profile <p.json|toml>`
  (CLI/`--map` override file values; key names mirror `to_canonical.py` profiles). See `list-adapters`.
- **Custom in-Rust adapter (only if you need it):** copy `src/adapters/template.rs`, implement
  `DataAdapter::{name, default_venue, load(spec) -> Vec<MarketEvent>}` (normalize into venue-tagged
  `MarketEvent`s + `finalize_events`); override `description()`/`mapping_keys()` so it shows in
  `list-adapters`; then register one line in `AdapterRegistry::with_builtins`. Run via
  `--source adapter --adapter <name> --venue <V> --adapter-path <p>`; merge venues with `--extra-source`.
- **Live capture of a new venue?** Subclass `Feed` in `adapters/feeds/` (implement `discover_markets`,
  `subscribe`, `normalize`); see `adapters/feeds/README.md`.

## Recipe: add an EXECUTION-REALISM feature (latency/risk/fee-style toggle)

Follow the established pattern (e.g. risk controls, settlement, queue model):
1. Add a sub-config struct in `src/config.rs` under `ExecutionConfig` with `#[serde(default)]` and a default
   that is a NO-OP.
2. Wire it into `src/engine.rs` at the right point in `step`/`apply_pending`/`finalize`.
3. Add a CLI flag in `src/main.rs` that overrides the config; show it in the human summary.
4. Add additive `Summary` field(s) in `types.rs` + populate in `report.rs`; document `[execution.*]` in
   `src/runspec.rs`'s `init-config` templates.
5. Tests: behavior tests + the **byte-for-byte no-op default** test.
6. Document it in `backtesterDescription.md`.

---

## Build / test / run

```bash
# Rust
cd backtester && cargo build --release            # +--features clickhouse to enable the CH source
cargo test && cargo test --features clickhouse         # MUST be green, zero warnings

# Python (always use the repo venv)
.venv/bin/python -m unittest discover -s adapters/tests
.venv/bin/python -m unittest discover -s adapters/convert/tests

# Run a tick backtest
./backtester/target/release/kalshi-backtest backtest --source ndjson \
    --ndjson data/tick/natgas_tick_demo.ndjson.gz --instrument 'KXNATGASD-%' --strategy imbalance --out-dir data/exports/run

# Convenience
make demo        make demo-latency        make test        make dashboard
```

## Debugging & transparency (use these, and improve them)

- `-v`/`-vv` = more progress/detail on stderr; `-q` = near-silent. `report.json` always prints between
  `===REPORT_JSON_START===`/`END` on **stdout** (machine-readable); the human summary goes to **stderr**.
- `kalshi-backtest describe-data --source ... ` summarizes a source (events, instruments, span, snapshots/trades).
- Inspect raw data: `notebooks/clickhouse_data_inspection.ipynb` (tables, schema, sample rows, reconstructed book).
- Errors should be **actionable** (use `anyhow::Context`); a missing file / bad mapping / unreachable ClickHouse
  must say what to fix. When you add ingestion, add a `--preview N` / validation path so users see parsed events
  before a full run. Transparency over magic.

## Workflow rules

- **Verify before claiming done.** Paste real `cargo test` / demo output; never assert green without running it.
- **One coherent change per commit**, descriptive message ending with the Co-Authored-By trailer. Branch off the
  default branch if needed. Don't commit generated artifacts (`target/`, `.venv/`, `data/raw/`, the CH binary).
- **Keep it clean and readable.** Match the surrounding style; document new public items; OOP where it helps.
- When unsure about a contract, read `types.rs` and the relevant module's tests — they are the source of truth.
