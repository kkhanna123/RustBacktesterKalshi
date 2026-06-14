# howToRun.md — running the backtester, step by step (idiot-proof)

Follow these in order. Each block is copy-paste. Commands assume you start in the repo root
`RustBacktesterTickLevelKalshiData/`. If a step says "(optional)", you can skip it.

---

## 0. One-time prerequisites

You need **Rust** and **Python 3.8+**. Check:

```bash
cargo --version      # need Rust (any recent stable). Install: https://rustup.rs
python3 --version    # need 3.8+
```

Python libraries used by the collector / tools / dashboard. **Use a virtual environment** (already created
for you as `.venv/` if you ran the setup; otherwise create it):

```bash
python3 -m venv .venv                       # create it (once)
source .venv/bin/activate                   # ENGAGE it — your prompt shows (.venv). Do this every new shell.
pip install -r requirements.txt             # install all deps into the venv (once)
```

To leave the venv later: `deactivate`. On **Windows** activate with `.venv\Scripts\activate`.

> Don't want to activate? You can always call the venv's interpreter directly, e.g.
> `.venv/bin/python dashboard/build_dashboard.py …` — no activation needed.

(optional) **ClickHouse** — only needed to store/query tick data. It's a single native binary, **no Docker**:

```bash
curl -fsSL https://clickhouse.com/ | sh      # downloads ./clickhouse
mkdir -p bin && mv clickhouse bin/clickhouse # the repo expects it at bin/clickhouse
```

---

## 1. Build the backtester

```bash
cd backtester
cargo build --release          # first build takes a couple minutes
cd ..
```

The binary is now at `backtester/target/release/kalshi-backtest`. There's also a convenience wrapper at
the repo root: `./kalshi-backtest` (Mac/Linux) or `kalshi-backtest.bat` (Windows) which builds + runs.

Sanity check:

```bash
./kalshi-backtest list-strategies     # prints the 10 strategies + their params
```

> Want ClickHouse support compiled in? Build with the feature: `cargo build --release --features clickhouse`
> (or set `KALSHI_BT_FEATURES=clickhouse` before the `./kalshi-backtest` wrapper).

---

## 2. Run your first backtest (real NatGas TICK data is bundled)

Backtests run **only on tick data** (orderbook deltas + trades). A real capture ships at
`data/tick/natgas_tick_demo.ndjson.gz`.

```bash
./kalshi-backtest backtest \
    --source ndjson \
    --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' \
    --strategy imbalance \
    --out-dir data/exports/myrun \
    --tearsheet figures/myrun.html
```

What you'll see:
- A **human summary table** on screen (stderr): PnL, Sharpe, fills, cost breakdown, etc.
- The machine-readable `report.json` printed between `===REPORT_JSON_START===` / `===REPORT_JSON_END===`.
- `figures/myrun.html` (open it in any browser) and `data/exports/myrun/` (report.json + CSVs).

**Model latency (the headline feature):** an order sent at tick `T` with latency `L` only becomes effective at
`T+L` and fills at that timestep. Re-run with `--latency-ns` and watch the fills/PnL change:

```bash
./kalshi-backtest backtest --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy imbalance --latency-ns 1000000000   # 1 second
```

Tune a strategy param, or layer on slippage / drop fees:
```bash
./kalshi-backtest backtest --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --strategy market_maker --strategy-param half_spread_cents=3 --slippage-ticks 1
```

Or use the Makefile shortcuts: `make demo` and `make demo-latency`.

### 2a. Execution-realism, optimize & walk-forward (new features)

All use the same bundled tick capture. See `backtesterDescription.md` (the deep reference) for full semantics.

```bash
# Risk-guarded run: cap gross exposure and HALT if equity drops below $900
# (on a breach the engine cancels + flattens, bypassing latency; report shows halted=true).
./kalshi-backtest backtest --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy market_maker --max-gross 100 --equity-floor 900

# Stochastic latency: draw each order's latency from a SEEDED normal distribution
# (mean 500ms, std 300ms) instead of a fixed constant — same seed reproduces exactly.
./kalshi-backtest backtest --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy imbalance \
    --latency-dist normal --latency-ns 500000000 --latency-std-ns 300000000 --latency-seed 42

# OPTIMIZE: parse the data ONCE, sweep a parameter grid across all cores, rank by a metric.
# Writes optimize_results.csv + the best combo's report.json into --out-dir.
./kalshi-backtest optimize --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy market_maker \
    --param 'half_spread_cents=1,2,3' --param 'quote_size=5,10,20' \
    --metric sharpe --out-dir data/exports/opt

# WALK-FORWARD: 4 rolling train/test folds; optimize on each train segment, then measure
# OUT-OF-SAMPLE on the next. Writes walk_forward.csv + walk_forward_oos.json.
./kalshi-backtest walk-forward --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy market_maker \
    --param 'half_spread_cents=1,2,3' --windows 4 --out-dir data/exports/wf
```

Other new knobs (see `backtesterDescription.md`): `--queue-model optimistic` (maker-queue model),
`--settlements <file>` (settle held positions to $1/$0 at expiry; build the file with
`python adapters/fetch_settlements.py --ndjson data/tick/natgas_tick_demo.ndjson.gz --out settlements.csv`).
Order time-in-force (GTC/IOC) and post-only are **strategy-facing** (`Ctx::place_limit_ex`) — no CLI flag.

---

## 3. Explore what's in a data source

```bash
./kalshi-backtest describe-data    --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz
./kalshi-backtest list-instruments --source ndjson --ndjson data/tick/natgas_tick_demo.ndjson.gz
```

---

## 4. Reproducible runs with a config file

```bash
./kalshi-backtest init-config run.toml     # writes a fully-commented template
# edit run.toml (source, strategy, [strategy_params], [execution] toggles), then:
./kalshi-backtest backtest --config run.toml
# CLI flags still override the file: ... --config run.toml --strategy momentum
```

See `backtesterDescription.md` for every field.

---

## 5. (optional) Capture live tick data into ClickHouse

### 5a. Start ClickHouse + load the schema
```bash
make ch-server        # starts the server (http :8123, tcp :9000) in the background
make ch-init          # loads clickhouse/schema/01_tables.sql
curl -s localhost:8123/ping     # should print "Ok."
```

### 5b. Run a collector
**With Kalshi API keys** (true per-tick deltas). Put your key file at `KalshiAPIKeysDONOTPUSH/.api_keys`
(format: a line `API Key ID: <uuid>` then the RSA `-----BEGIN PRIVATE KEY-----` PEM block — this folder is
gitignored). Then:
```bash
cd adapters && ./run_collector_supervised.sh      # auto-restarts, keeps 2 days of data
```

**No keys?** Use the credential-free REST poller (snapshot-granular, public endpoint):
```bash
python adapters/rest_orderbook_collector.py --series KXNATGASD --out data/raw --clickhouse http://localhost:8123
```

Either way, data lands in **both** `data/raw/<date>/*.ndjson.gz` (always) and ClickHouse (when up). Check it:
```bash
curl -s "http://localhost:8123/?query=SELECT count() FROM kalshi.orderbook_deltas"
```

### 5c. Backtest on the real collected ticks
```bash
cargo build --release --features clickhouse        # once, to enable the clickhouse source
./kalshi-backtest backtest --source clickhouse --clickhouse http://localhost:8123 \
    --instrument 'KXNATGASD-%' --strategy imbalance --out-dir data/exports/realtick
```

---

## 6. View results in the interactive dashboard

Generate a self-contained, offline HTML dashboard from any run's `--out-dir`:
```bash
.venv/bin/python dashboard/build_dashboard.py --export-dir data/exports/myrun --out dashboard/myrun.html
```
Then just **open `dashboard/myrun.html`** in any browser (no server, no internet) — equity curve, drawdown,
fills, per-instrument breakdown, and the full metrics panel, all interactive. A ready-made example is at
`dashboard/sample/dashboard.html`. Shortcut: `make dashboard`.

---

## 7. Inspect the tick data format (notebook)

```bash
.venv/bin/jupyter notebook notebooks/clickhouse_data_inspection.ipynb
```
It connects to ClickHouse over HTTP, lists the tables, shows the `orderbook_deltas`/`trades` schema, and prints
sample rows so you can see exactly what the collector wrote (and reconstructs a top-of-book to confirm it's
backtest-ready).

> Anything not part of the core tick-backtester + dashboard (greeks analytics, param-sweep/risk tools, the
> capstone, paper trading) lives under `Archive/` to keep the repo focused.

---

## 8. Run the test suite

```bash
make test         # cargo test (Rust, 198 tests) + python -m unittest (collector)
```

---

## Troubleshooting

| Symptom | Fix |
|---|---|
| `binary not found` | Run `cargo build --release` in `backtester/` (step 1). |
| `--source clickhouse` errors about a feature | Rebuild with `cargo build --release --features clickhouse`. |
| ClickHouse `Multi-statements are not allowed` | Load schema via `make ch-init` (uses the native client), not the HTTP endpoint. |
| Collector exits with "creds" message | Put keys in `KalshiAPIKeysDONOTPUSH/.api_keys`, or use the REST poller (no keys). |
| `0 fills` on a backtest | Expected if the data has no trades crossing your quotes (e.g. overnight, or a market-maker on quiet data). Try a taker strategy (`imbalance`/`momentum`) or richer data. |
| Strategy loses money on candle data | Candle fidelity is illustrative — evaluate on true tick deltas (`--source clickhouse`/`ndjson`). |
| Windows: ClickHouse binary won't run | The CH binary is *nix-only; point `--clickhouse` at a remote/WSL ClickHouse, or use `--source ndjson`. The backtester itself is fully cross-platform. |

For deep detail on every flag and the internals, read `backtesterDescription.md`. To write your own strategy,
read `strategyFormat.md`.
