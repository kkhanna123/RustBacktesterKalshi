# Backtest Control Panel (`ui/`)

A self-contained, zero-install **local web app** to configure, launch, watch, and
compare runs of the Rust tick-level Kalshi backtester. It *drives* the already-built
binary and *reuses* the existing chart generators by read-only subprocess — it does
**not** modify any code outside this `ui/` directory.

## Run it

```bash
python ui/server.py
```

It picks a free port, prints a `http://127.0.0.1:<port>/` URL, and serves the
single-page app. Stop with `Ctrl-C`.

- Uses the repo venv automatically if present (`.venv/bin/python`) for the chart
  generators; otherwise the interpreter that launched the server.
- Requires the release binary at
  `backtester/target/release/kalshi-backtest`. If missing, build it:
  `cd backtester && cargo build --release`. The UI shows an actionable error
  if it can't find the binary.

## Views (left nav)

- **New Run** — a grouped form (Run / Data / Strategy Params / Execution realism /
  Latency / Risk / Escape hatch). The strategy dropdown dynamically shows that
  strategy's tunable params with their defaults. Click **Run backtest** to start;
  a live **progress bar** with **ETA**, **elapsed**, and **events** count polls
  every ~500 ms. On completion the run's dashboard opens automatically. You can
  **Cancel** a running backtest.
- **Runs** — a card per cached run (label, strategy, source, PnL / Sharpe / fills,
  time, duration). Click a card to view its interactive per-run dashboard + config;
  **Clone into form** loads a saved config back into the New Run form; **Delete**
  removes a cached run.
- **Compare** — pick two runs and get an overlaid equity chart plus a side-by-side
  metrics table that highlights the better/worse value per metric.

## Where things are cached

- Each run lives in `ui/runs/<run_id>/` where `<run_id>` is a sortable timestamp +
  short slug. It contains: `config.json`, the binary's exports (`report.json`,
  `equity.csv`, `fills.csv`, `trades.csv`, `round_trips.csv`,
  `instrument_stats.csv`, `meta.json`), the captured `run.log`, the generated
  per-run `dashboard.html`, and a UI `meta.json` summary.
- Compare overlays are cached as `ui/runs/compare_<a>__<b>.html`.
- The self-improving ETA calibration is `ui/calibration.json`.

## How progress + ETA work

- The total event count comes from the binary's `loaded N events` stderr line
  (scraped from `run.log` as soon as it appears). For the ndjson source the UI
  pre-fetches the count via `describe-data` so the ETA can appear immediately.
- `estimate_secs = events / rate`, where `rate` (events/sec) is persisted in
  `ui/calibration.json`, seeded at ~270,000 and updated after every successful run
  via an exponential moving average of that run's `events / duration`. The estimate
  therefore self-improves with use.
- `pct = min(99, elapsed / estimate * 100)` while running; `100` on done.

## Endpoints

`GET /` · `GET /api/meta` · `POST /api/run` · `GET /api/progress?run_id=` ·
`GET /api/runs` · `GET /api/run?id=` · `GET /api/compare?a=&b=` ·
`POST /api/delete?id=` · `POST /api/cancel?run_id=` ·
`GET /runs/<id>/dashboard.html` (+ export assets and compare overlays).

## Notes

- Runs are executed **one at a time** (a concurrent submit is rejected with a clear
  message) so the ETA stays honest.
- Generated dashboards and compare overlays are **fully offline** (no CDN, no
  external HTTP references).
- Stack: Python 3.8 **standard library only** (`http.server`, `threading`,
  `subprocess`, `json`, `urllib`) + vanilla JS/HTML/CSS. Nothing to install.
- This app **alters no other code** in the repository. It only reads the binary,
  the data files, and the dashboard scripts, and writes exclusively under `ui/`.
