# Kalshi Tick-Level Backtester & Data Stack

A self-contained, end-to-end stack to **capture Kalshi orderbook deltas** and **backtest trading
strategies in Rust** — built for NatGas (`KXNATGASD`) binary-option markets, generalizable to any Kalshi
series. Mirrors the data contract of the production `oracletrading/infra-orchestrator` system (see the
`InfraOrchestrator` Claude skill) so data and results are interchangeable with it.

```
 Kalshi WS / REST  ──►  collector  ──►  data/raw/*.ndjson.gz   (durable, always)
 (orderbook deltas)     (Python)    └─►  ClickHouse kalshi.*    (queryable warehouse)
                                              │   schema mirrors infra-orchestrator
                                              ▼
                         Rust backtester (kalshi-backtest) — TICK ONLY
              reads ClickHouse / NDJSON tick deltas ─► L2 book reconstruction ─► event-
              driven engine ─► Strategy trait (GTC/IOC + post-only orders) ─► LATENCY-
              modeled fills (effective at order_send + latency; optional stochastic dist) +
              maker-queue model + fees + slippage + rewards + binary settlement + hard risk
              limits ─► portfolio/metrics ─► report.json + HTML tearsheet + CSV/JSON exports
                                              │
                              optimize / walk-forward: parallel param sweep + OOS validation
                                              │
                                              ▼
                       Interactive backtest dashboard (offline HTML):
                       equity / drawdown / fills / per-instrument / full metrics
```

## Why this design (read this first)

**Kalshi's historical REST API has no orderbook history** — only 1-minute candlesticks. Full L2 depth and
per-tick deltas exist **only on the live WebSocket** and are not publicly archived. So *"every delta for a
past month"* is physically impossible from public Kalshi. This repo therefore **captures deltas going
forward** with its own collector, and the backtester accepts three data fidelities honestly:

| Fidelity | Source | Granularity | Past month? |
|---|---|---|---|
| **L2 deltas** | this repo's WS collector → ClickHouse/NDJSON | every tick | going forward only |
| Full-book snapshots | REST orderbook poller (no creds) / Dome backfill | seconds | yes (poller = forward; Dome = history w/ keys) |
| 1-min candlesticks | Kalshi historical REST | 1 minute | ~2 months back, public |

Full detail: [`docs/ORDERBOOK_DELTAS_HOWTO.md`](docs/ORDERBOOK_DELTAS_HOWTO.md). Design rationale and
controversial calls: [`DECISIONS.md`](DECISIONS.md). Master plan: [`docs/PLAN.md`](docs/PLAN.md).

**Key docs:** [`howToRun.md`](howToRun.md) (step-by-step, idiot-proof) ·
[`howToRunWindows.md`](howToRunWindows.md) (Windows) ·
[`backtesterDescription.md`](backtesterDescription.md) (how the engine works + every config switch + the latency
model) · [`strategyFormat.md`](strategyFormat.md) (how to write a strategy). Extending the codebase (for agents/contributors): [`AGENTS.md`](AGENTS.md). Inspect the tick data with
[`notebooks/clickhouse_data_inspection.ipynb`](notebooks/clickhouse_data_inspection.ipynb).

## Quickstart

```bash
# 0. Build the backtester
make build                      # or: cd backtester && cargo build --release

# 1. (optional) Start the local ClickHouse warehouse — native binary, no Docker
make ch-server && make ch-init  # http :8123, tcp :9000; loads clickhouse/schema/01_tables.sql

# 2. Capture live NatGas orderbook data
#    a) authenticated WS (true per-tick deltas) — put creds in KalshiAPIKeysDONOTPUSH/.api_keys:
cd adapters && ./run_collector_supervised.sh          # supervised, auto-restart, 2-day retention
#    b) OR credential-free REST orderbook poller:
python adapters/rest_orderbook_collector.py --series KXNATGASD --out data/raw --clickhouse http://localhost:8123

# 3. Backtest on TICK data (the bundled real capture, or your own ClickHouse/NDJSON ticks)
make demo                       # imbalance on real NatGas tick deltas -> report.json + tearsheet + exports
make demo-latency               # same run WITH 1s order latency — see the fills/PnL change

# 4. Visualize
make dashboard                  # builds dashboard/dashboard.html — open in any browser (offline)
```

> **The headline feature is latency.** An order sent at tick `T` with `--latency-ns L` only becomes effective
> at `T+L` and fills at that timestep; market orders execute against the book *as of* `T+L`, not instantly.
> A strategy that looks profitable at zero latency can flip to losing — that's the point of testing at the
> tick level.

## Components

| Dir | What |
|---|---|
| `backtester/` | **The core.** Tick-only event-driven engine, L2 book reconstruction, **realistic latency fill model** (order effective at `order_send + latency`, optionally a seeded **stochastic latency distribution**), `Strategy` trait + 10 strategies, **order types / time-in-force** (GTC/IOC + post-only), a **maker-queue model** (pessimistic/optimistic), **binary settlement at expiry** ($1/$0 payoff), engine-enforced **hard risk limits** (position/gross caps + equity-floor/drawdown halts), Kalshi fees, slippage & liquidity-reward models (all toggleable), multi-venue adapters, the parallel **`optimize`** + **`walk-forward`** subcommands, portfolio + rich metrics, `report.json`, HTML tearsheet, CSV/JSON exports. 198 `cargo test`. |
| `dashboard/` | The **interactive backtest dashboard** — self-contained offline HTML (equity/drawdown/fills/per-instrument + full metrics). |
| `adapters/` | Kalshi tick-data collectors: authenticated **WS** `orderbook_delta` (true per-tick), credential-free **REST** poller, RSA-PSS auth, YES-native book reconstruction → NDJSON + ClickHouse. Supervised runner with retention cap. |
| `clickhouse/` | Schema for `orderbook_deltas`/`trades` + native-binary runner (no Docker). |
| `notebooks/` | `clickhouse_data_inspection.ipynb` — load ClickHouse and check the tick-data format. |
| `docs/` | PLAN, ORDERBOOK_DELTAS_HOWTO. |
| `Archive/` | Everything not core to the tick-backtester + dashboard (greeks analytics, param-sweep/risk tools, capstone, paper trading) — kept for reference, out of the way. |

## Data contract (ClickHouse / NDJSON)

`orderbook_deltas`: `timestamp(ns), instrument_id, venue, action(ADD/UPDATE/DELETE), side(BUY/SELL),
price($), size, market_alias, sequence, is_snapshot`. `trades`: `ts_event, instrument_id, venue,
aggressor_side, price, size, market_alias, trade_id`. The book is **YES-native**: a NO bid at price `q`
is stored as a YES ask (`SELL`) at `1 − q`. `is_snapshot=1` marks a full-book reset.

## Strategies

`noop, momentum, mean_reversion, market_maker, queue_probe, avellaneda_stoikov, imbalance, breakout,
cross_venue_arb, template`. Pick with `--strategy`; tune any params with `--strategy-param key=value` (or a
`[strategy_params]` config table). `avellaneda_stoikov` is the inventory-aware optimal MM (reservation price +
optimal spread); `queue_probe` joins the touch to exercise the maker-queue model. Strategies place orders with
a time-in-force (GTC/IOC) and an optional post-only flag via `Ctx::place_limit_ex`. The `template` +
`strategies/toolkit.rs` (rolling stats, EMA, z-score, position sizer) make a new idea ~20 lines — see "How to
add a strategy" in `backtester/src/strategies/mod.rs`.

**Sweep & validate.** Beyond a single `backtest`, two parallel subcommands reuse the same parse-once engine:
`optimize` runs a strategy's parameter grid across cores and ranks every combo by a metric (`optimize_results.csv`
+ best `report.json`); `walk-forward` splits the data into K+1 time-ordered folds and measures **out-of-sample**
performance per fold (`walk_forward.csv` + `walk_forward_oos.json`).

## Integrate any data stream

Everything normalizes to one canonical contract (orderbook deltas + trades, venue-tagged `"VENUE:symbol"`,
YES-native integer-cent book). Pick the easiest path — **discover with `kalshi-backtest list-adapters`, and
always `kalshi-backtest validate --source … --preview` first** (it prints a summary + the first N parsed events +
actionable warnings, so you see whether your data is understood *before* a full run):

1. **Already canonical NDJSON** → `--source ndjson --ndjson file`. Nothing to write.
2. **Any CSV / Parquet / messy feed** → `adapters/convert/to_canonical.py` (an ingest *profile* maps your columns;
   `--infer` guesses one, `--preview` shows events) → NDJSON → backtest. **Zero Rust.**
3. **Row-per-event CSV/NDJSON with different names/units** → `--source adapter --adapter generic_csv|generic_ndjson`
   with a `mapping` (`price_scale`, `ts_unit`, `side_from_sign`, dotted JSON paths) — keep it in a file and pass
   `--adapter-profile <p.json|toml>`.
4. **Custom in-Rust adapter** (only if needed) → copy `src/adapters/template.rs`, register one line.

**Multi-venue:** merge sources in one run with repeatable `--extra-source` (or a `[[sources]]` config list) and
trade across them with `cross_venue_arb`. **Live capture of a new venue:** subclass `Feed` in `adapters/feeds/`
(one class; `--replay` to test offline). Extending the codebase: see [`AGENTS.md`](AGENTS.md). Deep reference:
[`backtesterDescription.md`](backtesterDescription.md) §15.

## Windows

The Rust backtester is fully cross-platform: pure-Rust TLS (rustls, no OpenSSL), `PathBuf` everywhere,
CRLF-safe parsing, no shelling out. Build with `cargo build --release` and run `kalshi-backtest.exe`. The
dashboard HTML opens by double-click. ClickHouse's native binary is *nix; on Windows point
`--source clickhouse --clickhouse http://<host>:8123` at a remote/WSL ClickHouse, or use NDJSON/CSV sources.

## Tests & CI

`make test` runs `cargo test` (**198 Rust tests**, default + `--features clickhouse`) and `python -m unittest`
(collector). GitHub Actions (`.github/workflows/ci.yml`) runs all of it on push, including a **Windows build job**
validating cross-platform compilation. The latency fill model, the ClickHouse round-trip, and a full tick
backtest are verified end-to-end.
