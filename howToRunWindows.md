# howToRunWindows.md — running this folder on Windows

Step-by-step for running the **tick-level backtester + interactive dashboard** if this exact folder is on a
Windows PC. Commands are for **PowerShell** (the default terminal); notes for `cmd.exe` are called out.
Start every block from the repo root `RustBacktesterTickLevelKalshiData\`.

> The Rust backtester is fully cross-platform. The only *nix-only piece is the ClickHouse **native binary** —
> on Windows you either run ClickHouse under WSL2/Docker, or you skip it and **backtest directly on the
> NDJSON tick captures** (`--source ndjson`), which needs nothing extra.

---

## 0. One-time prerequisites

### Rust
Install with **rustup**: download and run `https://win.rustup.rs` (`rustup-init.exe`). Choose the default
(MSVC) toolchain. If it asks, install the **"Visual Studio C++ Build Tools"** (the installer links them) —
Rust needs a linker. Then open a NEW terminal and check:
```powershell
cargo --version
```

### Python
Install **Python 3.10+** from https://www.python.org/downloads/ (tick **"Add python.exe to PATH"** in the
installer). Check:
```powershell
py --version        # the Windows launcher
```

### Create and engage the virtual environment
```powershell
py -m venv .venv
.\.venv\Scripts\Activate.ps1          # PowerShell — your prompt shows (.venv)
# cmd.exe instead:  .\.venv\Scripts\activate.bat
pip install -r requirements.txt
```
If PowerShell blocks the activate script, allow it once:
`Set-ExecutionPolicy -Scope CurrentUser -ExecutionPolicy RemoteSigned`, then re-run Activate.
Leave the venv later with `deactivate`. (No activation needed if you call `.\.venv\Scripts\python.exe` directly.)

---

## 1. Build the backtester

```powershell
cd backtester
cargo build --release            # produces target\release\kalshi-backtest.exe
cd ..
```
There's a convenience launcher at the repo root: `kalshi-backtest.bat` (builds + runs, forwarding args).
Sanity check:
```powershell
.\kalshi-backtest.bat list-strategies
```
To compile in the ClickHouse source: `cargo build --release --features clickhouse` (or set the env var
`$env:KALSHI_BT_FEATURES = "clickhouse"` before the `.bat`).

---

## 2. Run a backtest on TICK data (no ClickHouse needed)

The repo ships a real tick capture at `data\tick\natgas_tick_demo.ndjson.gz`. Backtests run **only** on tick
data (orderbook deltas) — there is no candle mode.

```powershell
.\kalshi-backtest.bat backtest `
    --source ndjson `
    --ndjson data\tick\natgas_tick_demo.ndjson.gz `
    --instrument "KXNATGASD-%" `
    --strategy imbalance `
    --out-dir data\exports\myrun `
    --tearsheet figures\myrun.html
```
(In `cmd.exe`, put it on one line and drop the backtick line-continuations.)

You'll get a human summary on screen, `report.json` between sentinels on stdout, `figures\myrun.html`
(double-click to open), and `data\exports\myrun\` (report.json + CSVs for the dashboard).

### Model latency (the headline feature)
An order sent at tick `T` with latency `L` only becomes effective at `T+L` and fills at that timestep. Add
`--latency-ns` (nanoseconds) and watch fills/PnL change:
```powershell
.\kalshi-backtest.bat backtest --source ndjson --ndjson data\tick\natgas_tick_demo.ndjson.gz `
    --instrument "KXNATGASD-%" --strategy imbalance --latency-ns 1000000000      # 1 second
```
Other realism toggles: `--slippage-ticks`, `--slippage-bps`, `--no-fees`, `--rewards …` (see
`backtesterDescription.md`).

### 2a. Execution-realism, optimize & walk-forward (new features)

All use the bundled tick capture. Full semantics live in `backtesterDescription.md`.

```powershell
# Risk-guarded run: cap gross exposure + HALT if equity drops below $900 (engine cancels + flattens on breach).
.\kalshi-backtest.bat backtest --source ndjson --ndjson data\tick\natgas_tick_demo.ndjson.gz `
    --instrument "KXNATGASD-%" --strategy market_maker --max-gross 100 --equity-floor 900

# Stochastic latency: draw each order's latency from a SEEDED normal dist (mean 500ms, std 300ms).
.\kalshi-backtest.bat backtest --source ndjson --ndjson data\tick\natgas_tick_demo.ndjson.gz `
    --instrument "KXNATGASD-%" --strategy imbalance `
    --latency-dist normal --latency-ns 500000000 --latency-std-ns 300000000 --latency-seed 42

# OPTIMIZE: parse ONCE, sweep a param grid across cores, rank by a metric -> optimize_results.csv + best report.json.
.\kalshi-backtest.bat optimize --source ndjson --ndjson data\tick\natgas_tick_demo.ndjson.gz `
    --instrument "KXNATGASD-%" --strategy market_maker `
    --param "half_spread_cents=1,2,3" --param "quote_size=5,10,20" --metric sharpe --out-dir data\exports\opt

# WALK-FORWARD: 4 rolling train/test folds, measure OUT-OF-SAMPLE -> walk_forward.csv + walk_forward_oos.json.
.\kalshi-backtest.bat walk-forward --source ndjson --ndjson data\tick\natgas_tick_demo.ndjson.gz `
    --instrument "KXNATGASD-%" --strategy market_maker `
    --param "half_spread_cents=1,2,3" --windows 4 --out-dir data\exports\wf
```
(In `cmd.exe`, drop the backtick line-continuations and put each command on one line.)

Other new knobs: `--queue-model optimistic` (maker-queue model) and `--settlements <file>` (settle held
positions to $1/$0 at expiry — build the file with
`.\.venv\Scripts\python.exe collector\fetch_settlements.py --ndjson data\tick\natgas_tick_demo.ndjson.gz --out settlements.csv`).
Order time-in-force (GTC/IOC) and post-only are **strategy-facing** (`Ctx::place_limit_ex`), not CLI flags.

---

## 3. View the interactive dashboard

```powershell
.\.venv\Scripts\python.exe dashboard\build_dashboard.py --export-dir data\exports\myrun --out dashboard\myrun.html
```
Then double-click `dashboard\myrun.html` (or `start dashboard\myrun.html`). It's fully self-contained and
offline — equity curve, drawdown, fills, per-instrument stats, and the full metrics panel. A pre-built
example is at `dashboard\sample\dashboard.html`.

### 3a. Interactive control panel (configure + run + compare from a UI)

```powershell
.\.venv\Scripts\python.exe ui\server.py        # prints a http://127.0.0.1:<port>/ URL — open it
```
A local web app to toggle every config, launch backtests with a live progress bar + ETA, and browse/compare
cached runs. It finds `kalshi-backtest.exe` automatically and runs the chart generators with your venv. Runs
cache under `ui\runs\`. (Stop it with Ctrl-C.)

---

## 4. (optional) Capture more tick data with the collector

The collector is Python and runs natively on Windows; it writes `.ndjson.gz` captures you can backtest on.

```powershell
# credential-free public REST poller (no API keys needed):
.\.venv\Scripts\python.exe collector\rest_orderbook_collector.py --series KXNATGASD --out data\raw
# with Kalshi API keys (true per-tick deltas): put them in KalshiAPIKeysDONOTPUSH\.api_keys, then:
.\.venv\Scripts\python.exe collector\kalshi_ws_collector.py --series KXNATGASD --out data\raw
```
Captures land in `data\raw\<date>\*.ndjson.gz`. Point a backtest at one with `--source ndjson --ndjson <file>`.

> The supervised wrapper `run_collector_supervised.sh` is a bash script — on Windows run the Python collector
> directly (above), or run the bash script under **Git Bash** / **WSL**.

---

## 5. (optional) ClickHouse on Windows

The bundled ClickHouse binary is *nix-only. Two options:

- **WSL2 / Docker Desktop:** run ClickHouse there and point the backtester at it:
  ```powershell
  .\kalshi-backtest.bat backtest --source clickhouse --clickhouse http://localhost:8123 `
      --instrument "KXNATGASD-%" --strategy imbalance            # needs the clickhouse feature build
  ```
  Inspect the data with the notebook (step 6) — it talks to ClickHouse over HTTP, which works from Windows.
- **Skip it:** everything works on `--source ndjson` using the `.ndjson.gz` captures. You only need ClickHouse
  if you want a queryable warehouse.

---

## 6. Inspect the tick data format (notebook)

```powershell
.\.venv\Scripts\jupyter.exe notebook notebooks\clickhouse_data_inspection.ipynb
```
It connects to ClickHouse over HTTP (set `CLICKHOUSE_URL` if not `localhost:8123`), lists the tables, shows the
`orderbook_deltas`/`trades` schema, and prints sample rows so you can see exactly what the collector wrote.

---

## 7. Tests

```powershell
cd backtester ; cargo test ; cd ..
.\.venv\Scripts\python.exe -m unittest discover -s collector\tests
```

---

## Windows gotchas

| Issue | Fix |
|---|---|
| `cargo` "linker `link.exe` not found" | Install the Visual Studio **C++ Build Tools** (rustup links to them). |
| PowerShell won't run `Activate.ps1` | `Set-ExecutionPolicy -Scope CurrentUser RemoteSigned`, then re-run. |
| `make` not found | There's no `make` on Windows — use the raw `cargo` / `python` commands shown here (or install via WSL/Git Bash). |
| ClickHouse binary won't start | It's Linux/macOS only — use WSL/Docker, or just use `--source ndjson`. |
| `.sh` scripts (e.g. `run_collector_supervised.sh`) | Run under Git Bash/WSL, or call the Python collector directly. |
| Paths | Rust accepts both `\` and `/`; PowerShell uses `\`. The `.bat` launcher and `.exe` are Windows-native. |

For the full flag/internals reference see `backtesterDescription.md`; to write a strategy see `strategyFormat.md`.
