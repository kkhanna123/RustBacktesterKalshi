# Summary — tick-level Kalshi backtester + interactive dashboard

The repo does **one thing well**: backtest trading strategies on **tick-level** Kalshi orderbook data with a
**realistic latency fill model**, and view the results in an **interactive dashboard**. Everything else is in
`Archive/`.

## The two things that matter
1. **Tick-only backtesting.** Backtests run straight on orderbook deltas + trades (`--source ndjson` or
   `--source clickhouse`). There is no candle/bar mode.
2. **Latency modeling.** An order sent at tick `T` with `--latency-ns L` only becomes effective at `T+L` and
   fills at that timestep — a resting limit can only be hit by trades at `ts ≥ T+L`, and a **market order
   executes against the book as of `T+L`, not instantly**. This is the detail that makes a backtest honest.

## Layout
- `backtester/` — the engine, 9 strategies, latency/slippage/fees/rewards toggles, multi-venue adapters,
  metrics, `report.json` + tearsheet + exports. 132 `cargo test`, Windows-compatible.
- `dashboard/` — `build_dashboard.py` turns a run's `--out-dir` into a self-contained, offline interactive
  HTML dashboard (equity, drawdown, fills, per-instrument, full metrics).
- `adapters/` — Kalshi WS (true per-tick) + REST collectors → NDJSON + ClickHouse.
- `clickhouse/` — schema + native runner (no Docker).
- `notebooks/clickhouse_data_inspection.ipynb` — load ClickHouse, check the tick-data format.
- `docs/`, plus `howToRun.md`, `howToRunWindows.md`, `backtesterDescription.md`, `strategyFormat.md`, `DECISIONS.md`.
- `Archive/` — greeks analytics, param-sweep/risk tools, capstone, paper trading, old notebook (reference only).

## Run it
```bash
make build
make demo            # backtest on the bundled real tick capture
make demo-latency    # same run with 1s latency — see the difference
make dashboard       # build + open the interactive dashboard
```
Full guide: `howToRun.md` (Windows: `howToRunWindows.md`). Every flag + the engine internals: `backtesterDescription.md`.

## Live state
A supervised WS collector + native ClickHouse are typically left running to accumulate tick data
(`pgrep -f kalshi_ws_collector`; `curl localhost:8123/ping`). Keys live in the gitignored `KalshiAPIKeysDONOTPUSH/`.
