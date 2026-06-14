# Backtest Results Dashboard

A self-contained, **interactive** dashboard for a single tick-level backtest run produced by
`kalshi-backtest`. Cross-platform, offline (no server, no internet, no CDN), Python 3.8+.

## Use it
```bash
# 1. Run a backtest with --out-dir to produce the export files:
kalshi-backtest backtest --source ndjson --ndjson ../data/tick/natgas_tick_demo.ndjson.gz \
    --instrument 'KXNATGASD-%' --strategy imbalance --out-dir ../data/exports/myrun

# 2. Build the dashboard from that export dir:
python build_dashboard.py --export-dir ../data/exports/myrun --out dashboard.html

# 3. Open dashboard.html — double-click it (Windows: just open in any browser).
```
A ready-made example is at [`sample/dashboard.html`](sample/dashboard.html).

## What it shows
KPI cards (PnL, %, Sharpe, Sortino, max drawdown, win rate, profit factor, Calmar, orders, fills, round-trips,
exposure, fees/slippage/rewards), an **interactive equity curve** (hover, drag-to-zoom, reset) with a linked
**underwater drawdown** plot, a per-instrument **price line with BUY/SELL fill markers**, a **round-trip PnL
histogram**, a sortable/filterable **fills table**, a sortable **per-instrument breakdown** + PnL bar chart,
and a full stats panel. Charts are hand-rolled vanilla-JS Canvas (no external assets).

## Inputs (the `--out-dir` export contract)
`report.json`, `equity.csv`, `fills.csv`, `trades.csv`, `round_trips.csv`, `instrument_stats.csv`, `meta.json`.
The dashboard degrades gracefully if a file is missing.

---

# Latency-Sweep Comparison Page

A second, separate page (`build_latency_sweep.py`) answers one question: **"does my edge
survive latency?"** It overlays the equity curves of the **same strategy run at several
latencies**, draws **risk-halt markers**, and shows a comparison table + an edge-decay
mini-chart. Same dark theme, same hand-rolled vanilla-JS Canvas charts, fully offline.

### Latency fill model (shown in the page header)
Each order is held `--latency-ns` nanoseconds before it can match, then fills against the
order book *as it exists after that delay* — quotes you would have hit at zero latency may
have moved, been taken, or pulled. That is exactly the edge decay this page visualizes.

### 1. Produce one export per latency (the "sweep")
Run the **same** backtest several times, varying only `--latency-ns`, into per-latency
`--out-dir`s. Name them `lat_<ns>` so the page can auto-discover + label them:
```bash
for L in 0 100000000 500000000 1000000000; do        # 0ns, 100ms, 500ms, 1s
  kalshi-backtest backtest --source ndjson --ndjson ../data/tick/natgas_tick_demo.ndjson.gz \
      --instrument 'KXNATGASD-%' --strategy imbalance \
      --latency-ns "$L" --out-dir ../data/exports/mysweep/lat_$L
done
```

### 2. Build the page
```bash
# Auto-discover lat_<ns> subdirs (labels by latency, sorted ascending):
python build_latency_sweep.py --sweep-dir ../data/exports/mysweep --out latency_sweep.html

# …or list runs explicitly (repeatable; label/latency optional, inferred otherwise):
python build_latency_sweep.py \
    --runs 'label=0ns,dir=../data/exports/mysweep/lat_0' \
    --runs 'label=1s,dir=../data/exports/mysweep/lat_1000000000' \
    --out latency_sweep.html

# Open latency_sweep.html — double-click in any browser, fully offline.
```
A ready-made example is at [`sample/latency_sweep.html`](sample/latency_sweep.html).

### What it shows
1. **Overlaid equity curves** — one interactive line per latency (distinct colors, legend
   with click-to-toggle, hover showing latency+time+equity, drag-to-zoom, reset).
2. **Halt markers** — for any run with `summary.halted == true`, a red dashed vertical line
   + crossed `✕` at the halt point, labelled from `halt_reason`.
3. **Comparison table** — pnl_total, pnl_pct, sharpe, max_drawdown_pct, win_rate, num_fills,
   total_fees, risk_rejections, halted/halt_reason. Sortable; PnL colour-coded.
4. **Edge-decay mini-chart** — PnL bars (left axis) + Sharpe line (right axis) vs latency.

### How the halt point is located (graceful degradation, in order)
1. an explicit `halt_ts` / `halt_unix_ns` / `halt_ts_ns` / `halt_ns` field in
   `report.json` (top-level or `summary`) or `meta.json`; else
2. the start of the flat tail after the running peak (the "flatline after a halt" signature); else
3. the run's minimum-equity timestamp.
The marker is always labelled from `halt_reason`. If `halted` is absent (older runs), no
marker is drawn — the page degrades silently.

### Regenerating the synthetic sample
The committed sample is **synthetic** (built without the Rust binary for development):
```bash
python build_latency_sweep.py --make-sample
# writes ../data/exports/sweep_sample/lat_{0,100000000,500000000,1000000000}
# and builds dashboard/sample/latency_sweep.html from it
```
Once the backtester crate is rebuilt, regenerate the real sample from actual
`--latency-ns` runs (step 1 above) and rebuild the page.
