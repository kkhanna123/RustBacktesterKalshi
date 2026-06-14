#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""build_dashboard.py -- zero-dependency interactive backtest dashboard generator.

Ingests a Rust-backtester *export directory* and emits a SINGLE self-contained
``dashboard.html`` with all data, CSS and JS embedded inline.  The file opens by
double-click in any browser on any OS -- no server, no internet, no pip installs.

Charts are hand-rolled in vanilla JS on HTML5 Canvas (interactive: hover tooltips,
drag-to-zoom, reset) so there is ZERO external dependency and it works fully offline
and cross-platform (Windows / macOS / Linux).

Design:
    DashboardData      -- loads + parses an export dir (stdlib csv/json; pandas not required)
    DashboardRenderer  -- turns a DashboardData into the final HTML string

Usage:
    python build_dashboard.py --export-dir ../data/exports/demo --out dashboard.html

Python 3.8+ compatible.  Only the standard library is required.
"""
from __future__ import annotations

import argparse
import csv
import json
import os
import sys
from datetime import datetime, timezone

# ---------------------------------------------------------------------------
# Expected files in an export dir (the fixed contract).
# ---------------------------------------------------------------------------
REPORT_JSON = "report.json"
META_JSON = "meta.json"
EQUITY_CSV = "equity.csv"
FILLS_CSV = "fills.csv"
TRADES_CSV = "trades.csv"
ROUND_TRIPS_CSV = "round_trips.csv"
INSTRUMENT_STATS_CSV = "instrument_stats.csv"

NS_PER_S = 1_000_000_000


# ===========================================================================
# Data loader
# ===========================================================================
class DashboardData:
    """Loads and lightly normalizes a backtest export directory.

    Every loader is defensive: a missing or empty file yields an empty/default
    structure rather than raising, so a partial export still renders.
    """

    def __init__(self, export_dir):
        self.export_dir = export_dir
        self.report = {}
        self.summary = {}
        self.meta = {}
        self.equity = []          # list[dict]: ts_ns,total,currency,drawdown
        self.fills = []           # list[dict]
        self.trades = []          # list[dict]
        self.round_trips = []     # list[dict]
        self.instrument_stats = []  # list[dict]
        self.warnings = []

    # ---- public API ------------------------------------------------------
    @classmethod
    def load(cls, export_dir):
        self = cls(export_dir)
        self._load_report()
        self._load_meta()
        self._load_equity()
        self._load_fills()
        self._load_trades()
        self._load_round_trips()
        self._load_instrument_stats()
        return self

    def _path(self, name):
        return os.path.join(self.export_dir, name)

    def _exists(self, name):
        return os.path.isfile(self._path(name))

    # ---- individual loaders ---------------------------------------------
    def _load_report(self):
        if not self._exists(REPORT_JSON):
            self.warnings.append("missing %s" % REPORT_JSON)
            return
        with open(self._path(REPORT_JSON), "r") as f:
            self.report = json.load(f)
        self.summary = self.report.get("summary", {}) or {}
        # equity_curve from report.json is a fallback if equity.csv is absent.
        self._report_equity = self.report.get("equity_curve", []) or []

    def _load_meta(self):
        if not self._exists(META_JSON):
            self.warnings.append("missing %s" % META_JSON)
            return
        with open(self._path(META_JSON), "r") as f:
            self.meta = json.load(f)

    def _read_csv(self, name):
        rows = []
        if not self._exists(name):
            self.warnings.append("missing %s" % name)
            return rows
        with open(self._path(name), "r", newline="") as f:
            for row in csv.DictReader(f):
                rows.append(row)
        return rows

    def _load_equity(self):
        rows = self._read_csv(EQUITY_CSV)
        if not rows and getattr(self, "_report_equity", None):
            # Build from report.json equity_curve (no drawdown column there).
            pts = self._report_equity
            peak = float("-inf")
            for p in pts:
                tot = _f(p.get("total"))
                peak = max(peak, tot)
                self.equity.append({
                    "ts_ns": _i(p.get("ts_ns")),
                    "total": tot,
                    "currency": p.get("currency", ""),
                    "drawdown": round(tot - peak, 6),
                })
            return
        for r in rows:
            self.equity.append({
                "ts_ns": _i(r.get("ts_ns")),
                "total": _f(r.get("total")),
                "currency": r.get("currency", ""),
                "drawdown": _f(r.get("drawdown")),
            })

    def _load_fills(self):
        for r in self._read_csv(FILLS_CSV):
            self.fills.append({
                "ts_ns": _i(r.get("ts_ns")),
                "instrument": r.get("instrument", ""),
                "order_id": r.get("order_id", ""),
                "side": r.get("side", ""),
                "price": _f(r.get("price")),
                "qty": _f(r.get("qty")),
                "liquidity": r.get("liquidity", ""),
                "fee": _f(r.get("fee")),
            })

    def _load_trades(self):
        for r in self._read_csv(TRADES_CSV):
            self.trades.append({
                "ts_ns": _i(r.get("ts_ns")),
                "instrument": r.get("instrument", ""),
                "aggressor_side": r.get("aggressor_side", ""),
                "price": _f(r.get("price")),
                "size": _f(r.get("size")),
            })

    def _load_round_trips(self):
        for r in self._read_csv(ROUND_TRIPS_CSV):
            self.round_trips.append({
                "instrument": r.get("instrument", ""),
                "entry_ts_ns": _i(r.get("entry_ts_ns")),
                "exit_ts_ns": _i(r.get("exit_ts_ns")),
                "qty": _f(r.get("qty")),
                "entry_price": _f(r.get("entry_price")),
                "exit_price": _f(r.get("exit_price")),
                "pnl": _f(r.get("pnl")),
            })

    def _load_instrument_stats(self):
        for r in self._read_csv(INSTRUMENT_STATS_CSV):
            self.instrument_stats.append({
                "instrument": r.get("instrument", ""),
                "pnl": _f(r.get("pnl")),
                "num_fills": _i(r.get("num_fills")),
                "num_round_trips": _i(r.get("num_round_trips")),
                "net_position": _f(r.get("net_position")),
                "volume": _f(r.get("volume")),
            })

    # ---- derived helpers -------------------------------------------------
    def instruments(self):
        names = set()
        for t in self.trades:
            names.add(t["instrument"])
        for s in self.instrument_stats:
            names.add(s["instrument"])
        for f in self.fills:
            names.add(f["instrument"])
        return sorted(n for n in names if n)

    def to_payload(self):
        """Everything the front-end needs, as a JSON-serializable dict.

        Series are emitted as compact parallel arrays (not arrays-of-dicts) to
        keep the embedded file small for large exports.
        """
        eq_ts = [e["ts_ns"] for e in self.equity]
        eq_total = [e["total"] for e in self.equity]
        eq_dd = [e["drawdown"] for e in self.equity]

        # trades grouped by instrument -> {ts:[], price:[]}
        trades_by_inst = {}
        for t in self.trades:
            d = trades_by_inst.setdefault(t["instrument"], {"ts": [], "price": []})
            d["ts"].append(t["ts_ns"])
            d["price"].append(t["price"])

        # fills grouped by instrument for overlay markers
        fills_by_inst = {}
        for fl in self.fills:
            d = fills_by_inst.setdefault(fl["instrument"], {"ts": [], "price": [], "side": []})
            d["ts"].append(fl["ts_ns"])
            d["price"].append(fl["price"])
            d["side"].append(fl["side"])

        return {
            "meta": self.meta,
            "summary": self.summary,
            "plugin_name": self.report.get("plugin_name", self.meta.get("strategy", "")),
            "equity": {"ts": eq_ts, "total": eq_total, "drawdown": eq_dd},
            "fills": self.fills,
            "round_trips": self.round_trips,
            "round_trip_pnls": [r["pnl"] for r in self.round_trips],
            "instrument_stats": self.instrument_stats,
            "instruments": self.instruments(),
            "trades_by_inst": trades_by_inst,
            "fills_by_inst": fills_by_inst,
            "warnings": self.warnings,
        }


def _f(v, default=0.0):
    try:
        if v is None or v == "":
            return default
        return float(v)
    except (TypeError, ValueError):
        return default


def _i(v, default=0):
    try:
        if v is None or v == "":
            return default
        return int(float(v))
    except (TypeError, ValueError):
        return default


# ===========================================================================
# Renderer
# ===========================================================================
class DashboardRenderer:
    """Renders a :class:`DashboardData` into a self-contained HTML document."""

    def __init__(self, data):
        self.data = data

    def render(self):
        payload = self.data.to_payload()
        data_json = json.dumps(payload, separators=(",", ":"), default=str)
        # Guard against the </script> sequence breaking the inline script tag.
        data_json = data_json.replace("</", "<\\/")
        title = payload.get("plugin_name") or "Backtest"
        generated = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")
        html = _TEMPLATE
        html = html.replace("__TITLE__", _esc(title))
        html = html.replace("__GENERATED__", generated)
        html = html.replace("__CSS__", _CSS)
        html = html.replace("__DATA_JSON__", data_json)
        html = html.replace("__JS__", _JS)
        return html

    def write(self, out_path):
        html = self.render()
        with open(out_path, "w", encoding="utf-8") as f:
            f.write(html)
        return out_path


def _esc(s):
    return (str(s).replace("&", "&amp;").replace("<", "&lt;")
            .replace(">", "&gt;").replace('"', "&quot;"))


# ===========================================================================
# Front-end assets (CSS / HTML template / JS).  All inlined -> offline.
# ===========================================================================
_CSS = r"""
:root{
  --bg:#0d1117; --panel:#161b22; --panel2:#1c2330; --border:#2a3242;
  --txt:#e6edf3; --muted:#8b949e; --accent:#58a6ff; --green:#3fb950;
  --red:#f85149; --amber:#d29922; --grid:#222b38;
}
*{box-sizing:border-box}
html,body{margin:0;padding:0;background:var(--bg);color:var(--txt);
  font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;
  font-size:14px;line-height:1.45}
a{color:var(--accent)}
.wrap{max-width:1280px;margin:0 auto;padding:20px}
header.top{display:flex;flex-wrap:wrap;align-items:baseline;gap:12px;justify-content:space-between;
  border-bottom:1px solid var(--border);padding-bottom:14px;margin-bottom:18px}
header.top h1{font-size:22px;margin:0;font-weight:650}
.muted{color:var(--muted)}
.meta-line{font-size:12.5px;color:var(--muted);display:flex;flex-wrap:wrap;gap:14px}
.meta-line b{color:var(--txt);font-weight:600}

/* KPI cards */
.kpis{display:grid;grid-template-columns:repeat(auto-fill,minmax(150px,1fr));gap:12px;margin-bottom:22px}
.kpi{background:var(--panel);border:1px solid var(--border);border-radius:10px;padding:12px 14px}
.kpi .label{font-size:11px;text-transform:uppercase;letter-spacing:.5px;color:var(--muted)}
.kpi .val{font-size:21px;font-weight:680;margin-top:4px}
.kpi .sub{font-size:11.5px;color:var(--muted);margin-top:2px}
.pos{color:var(--green)} .neg{color:var(--red)} .neu{color:var(--txt)}

/* tabs */
.tabs{display:flex;gap:4px;flex-wrap:wrap;border-bottom:1px solid var(--border);margin-bottom:18px}
.tab{padding:9px 16px;cursor:pointer;color:var(--muted);border-bottom:2px solid transparent;
  user-select:none;font-weight:550}
.tab:hover{color:var(--txt)}
.tab.active{color:var(--txt);border-bottom-color:var(--accent)}
.view{display:none}
.view.active{display:block}

.panel{background:var(--panel);border:1px solid var(--border);border-radius:10px;padding:16px;margin-bottom:18px}
.panel h2{margin:0 0 4px;font-size:15px;font-weight:620}
.panel .hint{font-size:12px;color:var(--muted);margin-bottom:10px}
.row{display:flex;gap:16px;flex-wrap:wrap}
.row>.panel{flex:1;min-width:320px}

.chart-wrap{position:relative;width:100%}
canvas{display:block;width:100%;background:var(--panel2);border-radius:6px;cursor:crosshair}
.chart-tip{position:absolute;pointer-events:none;background:#000c;border:1px solid var(--border);
  border-radius:6px;padding:6px 8px;font-size:12px;color:var(--txt);white-space:nowrap;
  transform:translate(-50%,-115%);display:none;z-index:5}
.toolbar{display:flex;gap:8px;align-items:center;margin-bottom:8px;flex-wrap:wrap}
button.btn{background:var(--panel2);color:var(--txt);border:1px solid var(--border);
  border-radius:6px;padding:5px 11px;cursor:pointer;font-size:12.5px}
button.btn:hover{border-color:var(--accent)}
select,input[type=text]{background:var(--panel2);color:var(--txt);border:1px solid var(--border);
  border-radius:6px;padding:5px 9px;font-size:12.5px}

/* tables */
table{width:100%;border-collapse:collapse;font-size:12.8px}
th,td{text-align:right;padding:7px 10px;border-bottom:1px solid var(--border);white-space:nowrap}
th:first-child,td:first-child{text-align:left}
th{color:var(--muted);font-weight:600;cursor:pointer;user-select:none;position:sticky;top:0;background:var(--panel)}
th.sort-asc::after{content:" \25B2";color:var(--accent)}
th.sort-desc::after{content:" \25BC";color:var(--accent)}
tbody tr:hover{background:#ffffff08}
.table-scroll{max-height:480px;overflow:auto;border:1px solid var(--border);border-radius:8px}
.badge{display:inline-block;padding:1px 7px;border-radius:10px;font-size:11px;font-weight:600}
.badge.buy{background:#3fb95022;color:var(--green)}
.badge.sell{background:#f8514922;color:var(--red)}
.badge.maker{background:#58a6ff22;color:var(--accent)}
.badge.taker{background:#d2992222;color:var(--amber)}

/* stats grid */
.stats-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(220px,1fr));gap:2px}
.stat{display:flex;justify-content:space-between;padding:7px 10px;background:var(--panel2);border-radius:6px}
.stat .k{color:var(--muted)}
.stat .v{font-weight:600}
.warnbar{background:#d2992218;border:1px solid var(--amber);color:var(--amber);
  border-radius:8px;padding:8px 12px;margin-bottom:14px;font-size:12.5px}
.footer{color:var(--muted);font-size:11.5px;text-align:center;margin:24px 0 8px}
"""

_TEMPLATE = r"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>__TITLE__ &middot; Backtest Dashboard</title>
<style>__CSS__</style>
</head>
<body>
<div class="wrap">
  <header class="top">
    <div>
      <h1 id="h-title">__TITLE__</h1>
      <div class="meta-line" id="meta-line"></div>
    </div>
    <div class="muted" style="font-size:12px">generated __GENERATED__</div>
  </header>

  <div id="warnings"></div>
  <div class="kpis" id="kpis"></div>

  <div class="tabs" id="tabs">
    <div class="tab active" data-view="overview">Overview</div>
    <div class="tab" data-view="trades">Trades &amp; Fills</div>
    <div class="tab" data-view="instruments">Instruments</div>
    <div class="tab" data-view="stats">Full Stats</div>
  </div>

  <!-- OVERVIEW -->
  <section class="view active" id="view-overview">
    <div class="panel">
      <h2>Equity Curve</h2>
      <div class="hint">Hover for value &middot; drag horizontally to zoom &middot; Reset to clear</div>
      <div class="toolbar"><button class="btn" id="eq-reset">Reset zoom</button></div>
      <div class="chart-wrap"><canvas id="eq-canvas" height="280"></canvas>
        <div class="chart-tip" id="eq-tip"></div></div>
    </div>
    <div class="panel">
      <h2>Drawdown (underwater)</h2>
      <div class="hint">Distance below the running peak. Shares the equity-curve zoom window.</div>
      <div class="chart-wrap"><canvas id="dd-canvas" height="180"></canvas>
        <div class="chart-tip" id="dd-tip"></div></div>
    </div>
  </section>

  <!-- TRADES & FILLS -->
  <section class="view" id="view-trades">
    <div class="panel">
      <h2>Price &amp; Fill Markers</h2>
      <div class="hint">Market prints (trades.csv) as a line; our fills overlaid (green=BUY, red=SELL). Hover for detail; drag to zoom.</div>
      <div class="toolbar">
        <label class="muted">Instrument</label>
        <select id="price-inst"></select>
        <button class="btn" id="price-reset">Reset zoom</button>
      </div>
      <div class="chart-wrap"><canvas id="price-canvas" height="300"></canvas>
        <div class="chart-tip" id="price-tip"></div></div>
    </div>
    <div class="row">
      <div class="panel">
        <h2>Round-trip PnL distribution</h2>
        <div class="hint">Histogram of per-round-trip PnL. Green bars are profitable buckets.</div>
        <div class="chart-wrap"><canvas id="hist-canvas" height="240"></canvas>
          <div class="chart-tip" id="hist-tip"></div></div>
      </div>
    </div>
    <div class="panel">
      <h2>Fills</h2>
      <div class="toolbar">
        <input type="text" id="fill-filter" placeholder="filter (instrument / side / liquidity)...">
        <span class="muted" id="fill-count"></span>
      </div>
      <div class="table-scroll"><table id="fills-table"></table></div>
    </div>
  </section>

  <!-- INSTRUMENTS -->
  <section class="view" id="view-instruments">
    <div class="panel">
      <h2>PnL by instrument</h2>
      <div class="chart-wrap"><canvas id="inst-canvas" height="260"></canvas>
        <div class="chart-tip" id="inst-tip"></div></div>
    </div>
    <div class="panel">
      <h2>Per-instrument breakdown</h2>
      <div class="hint">Click a header to sort.</div>
      <div class="table-scroll"><table id="inst-table"></table></div>
    </div>
  </section>

  <!-- FULL STATS -->
  <section class="view" id="view-stats">
    <div class="panel">
      <h2>Summary &mdash; all fields</h2>
      <div class="hint">Every key from report.json &rarr; summary, formatted.</div>
      <div class="stats-grid" id="stats-grid"></div>
    </div>
    <div class="panel">
      <h2>Run metadata</h2>
      <div class="stats-grid" id="meta-grid"></div>
    </div>
  </section>

  <div class="footer">Self-contained offline dashboard &middot; no server, no internet required.</div>
</div>

<script id="payload" type="application/json">__DATA_JSON__</script>
<script>__JS__</script>
</body>
</html>
"""

_JS = r"""
"use strict";
const DATA = JSON.parse(document.getElementById('payload').textContent);

// ---------- formatting helpers ----------
const NS = 1e9;
function nsToDate(ns){ return new Date(Number(ns)/1e6); }
function fmtTime(ns){
  const d = nsToDate(ns);
  return d.toISOString().replace('T',' ').replace('.000Z',' UTC').replace('Z',' UTC');
}
function fmtTimeShort(ns){
  const d = nsToDate(ns);
  const p=n=>String(n).padStart(2,'0');
  return p(d.getUTCHours())+':'+p(d.getUTCMinutes());
}
function fmtDateShort(ns){
  const d = nsToDate(ns);
  const p=n=>String(n).padStart(2,'0');
  return (d.getUTCMonth()+1)+'/'+d.getUTCDate()+' '+p(d.getUTCHours())+':'+p(d.getUTCMinutes());
}
function num(v,dp){ if(v===null||v===undefined||v==='') return '-';
  const n=Number(v); if(!isFinite(n)) return '-';
  return n.toLocaleString(undefined,{minimumFractionDigits:dp,maximumFractionDigits:dp}); }
function money(v,dp){ if(v===null||v===undefined||v==='') return '-';
  const n=Number(v); if(!isFinite(n)) return '-';
  const s=(n<0?'-':'')+'$'+Math.abs(n).toLocaleString(undefined,{minimumFractionDigits:dp===undefined?2:dp,maximumFractionDigits:dp===undefined?2:dp});
  return s; }
function pct(v,dp){ if(v===null||v===undefined||v==='') return '-';
  const n=Number(v); if(!isFinite(n)) return '-';
  return (n*100).toFixed(dp===undefined?2:dp)+'%'; }
function signClass(n){ return n>0?'pos':(n<0?'neg':'neu'); }

const S = DATA.summary || {};
const META = DATA.meta || {};
const CUR = S.currency || META.currency || 'USD';

// ---------- header / meta ----------
(function(){
  document.getElementById('h-title').textContent = DATA.plugin_name || META.strategy || 'Backtest';
  const ml = document.getElementById('meta-line'); const bits=[];
  if(META.strategy) bits.push('<b>'+esc(META.strategy)+'</b>');
  if(META.instrument_filter) bits.push('filter <b>'+esc(META.instrument_filter)+'</b>');
  if(META.start||META.end) bits.push((META.start||'?')+' &rarr; '+(META.end||'?'));
  if(META.starting_balance!==undefined) bits.push('start '+money(META.starting_balance));
  if(CUR) bits.push(esc(CUR));
  if(META.source) bits.push('src '+esc(META.source));
  ml.innerHTML = bits.join('<span class="muted">&middot;</span>');
  const w = DATA.warnings||[];
  if(w.length){ document.getElementById('warnings').innerHTML =
    '<div class="warnbar">Partial export: '+w.map(esc).join(', ')+'</div>'; }
})();
function esc(s){ return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;'); }

// ---------- KPI cards ----------
(function(){
  const cards = [
    {label:'PnL total', val:money(S.pnl_total), cls:signClass(S.pnl_total), sub:CUR},
    {label:'PnL %', val:pct(S.pnl_pct), cls:signClass(S.pnl_pct)},
    {label:'Sharpe', val:num(S.sharpe,2), cls:signClass(S.sharpe)},
    {label:'Sortino', val:num(S.sortino,2), cls:signClass(S.sortino)},
    {label:'Max DD %', val:pct(S.max_drawdown_pct), cls:'neg', sub:money(S.max_drawdown)},
    {label:'Win rate', val:pct(S.win_rate), cls:'neu'},
    {label:'Profit factor', val:(S.profit_factor==null?'-':num(S.profit_factor,2)), cls:'neu'},
    {label:'Calmar', val:num(S.calmar_ratio,2), cls:signClass(S.calmar_ratio)},
    {label:'Expectancy', val:money(S.expectancy), cls:signClass(S.expectancy)},
    {label:'Volatility', val:(S.volatility==null?'-':pct(S.volatility)), cls:'neu'},
    {label:'Exposure', val:(S.exposure_pct==null?'-':pct(S.exposure_pct)), cls:'neu'},
    {label:'Total orders', val:num(S.total_orders,0), cls:'neu'},
    {label:'Fills', val:num(S.num_fills!=null?S.num_fills:DATA.fills.length,0), cls:'neu'},
    {label:'Round trips', val:num(S.num_round_trips!=null?S.num_round_trips:DATA.round_trips.length,0), cls:'neu'},
    {label:'End balance', val:money(S.ending_balance), cls:signClass((S.ending_balance||0)-(S.starting_balance||0))},
  ];
  const host=document.getElementById('kpis');
  host.innerHTML = cards.map(c=>
    '<div class="kpi"><div class="label">'+esc(c.label)+'</div>'+
    '<div class="val '+(c.cls||'neu')+'">'+c.val+'</div>'+
    (c.sub?'<div class="sub">'+esc(c.sub)+'</div>':'')+'</div>').join('');
})();

// ---------- tabs ----------
(function(){
  const tabs=[...document.querySelectorAll('.tab')];
  tabs.forEach(t=>t.addEventListener('click',()=>{
    tabs.forEach(x=>x.classList.remove('active'));
    document.querySelectorAll('.view').forEach(v=>v.classList.remove('active'));
    t.classList.add('active');
    document.getElementById('view-'+t.dataset.view).classList.add('active');
    // canvases sized to 0 while hidden must be re-drawn on show.
    redrawAll();
  }));
})();

// =====================================================================
// Generic interactive line/area chart on Canvas.
//   - DPR-aware crisp rendering
//   - hover crosshair + tooltip
//   - drag horizontally to zoom into an x-range; reset restores full range
// =====================================================================
class LineChart{
  constructor(canvasId, tipId){
    this.canvas=document.getElementById(canvasId);
    this.tip=document.getElementById(tipId);
    this.series=[];          // [{xs,ys,color,label,area}]
    this.markers=[];         // [{x,y,color,label}]
    this.xfmt=fmtDateShort; this.yfmt=v=>num(v,2);
    this.tipFmt=null;        // (idx, nearestSeries)=>html
    this.xrange=null;        // [min,max] in data units, null=full
    this.fullx=null;
    this.pad={l:62,r:14,t:12,b:26};
    this.onZoom=null;        // callback(range)
    this._bind();
  }
  setData(series, markers){
    this.series=series||[]; this.markers=markers||[];
    let mn=Infinity,mx=-Infinity;
    for(const s of this.series){ for(const x of s.xs){ if(x<mn)mn=x; if(x>mx)mx=x; } }
    for(const m of this.markers){ if(m.x<mn)mn=m.x; if(m.x>mx)mx=m.x; }
    this.fullx=(mn<=mx)?[mn,mx]:[0,1];
    if(!this.xrange) this.xrange=this.fullx.slice();
  }
  setRange(r){ this.xrange = r? r.slice() : this.fullx.slice(); this.draw(); }
  reset(){ this.setRange(null); if(this.onZoom) this.onZoom(this.xrange); }
  _bind(){
    const c=this.canvas;
    this._drag=null;
    c.addEventListener('mousemove',e=>this._move(e));
    c.addEventListener('mouseleave',()=>{ this.tip.style.display='none'; this._hoverX=null; this.draw(); });
    c.addEventListener('mousedown',e=>{ this._drag={x0:this._px(e)}; });
    window.addEventListener('mouseup',e=>{
      if(this._drag){
        const x1=this._px(e), x0=this._drag.x0; this._drag=null;
        if(Math.abs(x1-x0)>6){
          const a=this._invX(Math.min(x0,x1)), b=this._invX(Math.max(x0,x1));
          this.xrange=[a,b]; if(this.onZoom) this.onZoom(this.xrange);
        }
        this.draw();
      }
    });
  }
  _px(e){ const r=this.canvas.getBoundingClientRect(); return (e.clientX-r.left); }
  _py(e){ const r=this.canvas.getBoundingClientRect(); return (e.clientY-r.top); }
  _dims(){ return {w:this.canvas.clientWidth, h:this.canvas.clientHeight}; }
  _plot(){ const {w,h}=this._dims(); return {x:this.pad.l,y:this.pad.t,
      w:w-this.pad.l-this.pad.r, h:h-this.pad.t-this.pad.b}; }
  _X(v){ const p=this._plot(),[a,b]=this.xrange; return p.x + (b===a?0:(v-a)/(b-a))*p.w; }
  _invX(px){ const p=this._plot(),[a,b]=this.xrange; return a+(px-p.x)/p.w*(b-a); }
  _yrange(){
    let mn=Infinity,mx=-Infinity; const [a,b]=this.xrange;
    for(const s of this.series){ for(let i=0;i<s.xs.length;i++){ const x=s.xs[i];
      if(x<a||x>b)continue; const y=s.ys[i]; if(y<mn)mn=y; if(y>mx)mx=y; } }
    for(const m of this.markers){ if(m.x<a||m.x>b)continue; if(m.y<mn)mn=m.y; if(m.y>mx)mx=m.y; }
    if(!isFinite(mn)||!isFinite(mx)){ mn=0;mx=1; }
    if(mn===mx){ mn-=1; mx+=1; }
    const pad=(mx-mn)*0.08; return [mn-pad, mx+pad];
  }
  _move(e){
    const px=this._px(e);
    this._hoverX=this._invX(px);
    this.draw();
    // find nearest point in first series for tooltip
    let best=null, bestSeries=null;
    for(const s of this.series){
      let lo=0,hi=s.xs.length-1, idx=0;
      // binary search nearest
      while(lo<=hi){ const m=(lo+hi)>>1; if(s.xs[m]<this._hoverX) lo=m+1; else hi=m-1; }
      const cand=[lo-1,lo].filter(i=>i>=0&&i<s.xs.length);
      for(const i of cand){ const d=Math.abs(s.xs[i]-this._hoverX);
        if(best===null||d<best.d){ best={i,d}; bestSeries=s; } }
    }
    if(best && bestSeries){
      const i=best.i; const x=bestSeries.xs[i];
      const py=this._py(e);
      let html;
      if(this.tipFmt) html=this.tipFmt(i,bestSeries);
      else html='<b>'+this.xfmt(x)+'</b><br>'+esc(bestSeries.label||'')+': '+this.yfmt(bestSeries.ys[i]);
      this.tip.innerHTML=html;
      this.tip.style.left=this._X(x)+'px';
      this.tip.style.top=(this.pad.t+8)+'px';
      this.tip.style.display='block';
    } else { this.tip.style.display='none'; }
  }
  draw(){
    const c=this.canvas, dpr=window.devicePixelRatio||1;
    const W=c.clientWidth, H=c.clientHeight;
    if(W===0){ return; }
    c.width=W*dpr; c.height=H*dpr;
    const ctx=c.getContext('2d'); ctx.setTransform(dpr,0,0,dpr,0,0);
    ctx.clearRect(0,0,W,H);
    const p=this._plot(); const [ymn,ymx]=this._yrange();
    const Y=v=> p.y + (ymx===ymn?0:(1-(v-ymn)/(ymx-ymn)))*p.h;
    // grid + y labels
    ctx.strokeStyle='#222b38'; ctx.fillStyle='#8b949e'; ctx.font='11px sans-serif';
    ctx.lineWidth=1; ctx.textBaseline='middle';
    const ticks=5;
    for(let i=0;i<=ticks;i++){
      const v=ymn+(ymx-ymn)*i/ticks; const yy=Y(v);
      ctx.beginPath(); ctx.moveTo(p.x,yy); ctx.lineTo(p.x+p.w,yy); ctx.stroke();
      ctx.textAlign='right'; ctx.fillText(this.yfmt(v), p.x-6, yy);
    }
    // x labels
    ctx.textAlign='center'; ctx.textBaseline='top';
    const [xa,xb]=this.xrange;
    for(let i=0;i<=6;i++){ const xv=xa+(xb-xa)*i/6; const xx=this._X(xv);
      ctx.fillText(this.xfmt(xv), xx, p.y+p.h+5); }
    // series
    for(const s of this.series){
      ctx.lineWidth=s.width||1.6; ctx.strokeStyle=s.color;
      if(s.area){
        ctx.beginPath(); let started=false;
        for(let i=0;i<s.xs.length;i++){ const x=s.xs[i]; if(x<xa||x>xb)continue;
          const X=this._X(x), Yv=Y(s.ys[i]);
          if(!started){ ctx.moveTo(X,Y( (s.baseline!==undefined?s.baseline:ymn) )); ctx.lineTo(X,Yv); started=true;}
          else ctx.lineTo(X,Yv); }
        // close to baseline
        const lastX=this._X(Math.min(xb, s.xs[s.xs.length-1]));
        const base=Y(s.baseline!==undefined?s.baseline:ymn);
        ctx.lineTo(lastX, base);
        ctx.closePath(); ctx.fillStyle=s.fill||(s.color+'22'); ctx.fill();
      }
      ctx.beginPath(); let started=false;
      for(let i=0;i<s.xs.length;i++){ const x=s.xs[i]; if(x<xa||x>xb)continue;
        const X=this._X(x), Yv=Y(s.ys[i]);
        if(!started){ ctx.moveTo(X,Yv); started=true; } else ctx.lineTo(X,Yv); }
      ctx.stroke();
    }
    // markers
    for(const m of this.markers){ if(m.x<xa||m.x>xb)continue;
      const X=this._X(m.x), Yv=Y(m.y);
      ctx.beginPath(); ctx.arc(X,Yv,3.2,0,7); ctx.fillStyle=m.color; ctx.fill();
      ctx.strokeStyle='#0d1117'; ctx.lineWidth=0.8; ctx.stroke(); }
    // hover crosshair
    if(this._hoverX!=null && this._hoverX>=xa && this._hoverX<=xb){
      const X=this._X(this._hoverX);
      ctx.strokeStyle='#58a6ff66'; ctx.lineWidth=1;
      ctx.beginPath(); ctx.moveTo(X,p.y); ctx.lineTo(X,p.y+p.h); ctx.stroke();
    }
    // drag selection rect
    if(this._drag){
      // nothing persistent; selection drawn live via mousemove redraw not tracked here
    }
    // frame
    ctx.strokeStyle='#2a3242'; ctx.lineWidth=1;
    ctx.strokeRect(p.x,p.y,p.w,p.h);
  }
}

// =====================================================================
// Bar chart (instrument PnL, histogram) on Canvas with hover.
// =====================================================================
class BarChart{
  constructor(canvasId, tipId){
    this.canvas=document.getElementById(canvasId);
    this.tip=document.getElementById(tipId);
    this.bars=[]; // [{label,value,color}]
    this.pad={l:62,r:14,t:12,b:54};
    this.yfmt=v=>num(v,2);
    this._bind();
  }
  setBars(bars){ this.bars=bars||[]; }
  _bind(){
    const c=this.canvas;
    c.addEventListener('mousemove',e=>this._move(e));
    c.addEventListener('mouseleave',()=>{ this.tip.style.display='none'; });
  }
  _plot(){ const w=this.canvas.clientWidth,h=this.canvas.clientHeight;
    return {x:this.pad.l,y:this.pad.t,w:w-this.pad.l-this.pad.r,h:h-this.pad.t-this.pad.b}; }
  _move(e){
    const r=this.canvas.getBoundingClientRect(); const px=e.clientX-r.left;
    const p=this._plot(); if(!this.bars.length)return;
    const bw=p.w/this.bars.length; let idx=Math.floor((px-p.x)/bw);
    if(idx<0||idx>=this.bars.length){ this.tip.style.display='none'; return; }
    const b=this.bars[idx];
    this.tip.innerHTML='<b>'+esc(b.label)+'</b><br>'+this.yfmt(b.value);
    this.tip.style.left=(p.x+bw*(idx+0.5))+'px';
    this.tip.style.top=(this.pad.t+8)+'px';
    this.tip.style.display='block';
  }
  draw(){
    const c=this.canvas,dpr=window.devicePixelRatio||1;
    const W=c.clientWidth,H=c.clientHeight; if(W===0)return;
    c.width=W*dpr;c.height=H*dpr; const ctx=c.getContext('2d');
    ctx.setTransform(dpr,0,0,dpr,0,0); ctx.clearRect(0,0,W,H);
    const p=this._plot();
    let mn=0,mx=0; for(const b of this.bars){ mn=Math.min(mn,b.value); mx=Math.max(mx,b.value); }
    if(mn===0&&mx===0){ mx=1; }
    const padv=(mx-mn)*0.1||1; mx+=padv; if(mn<0) mn-=padv;
    const Y=v=> p.y+(1-(v-mn)/(mx-mn))*p.h;
    // grid + y labels
    ctx.strokeStyle='#222b38'; ctx.fillStyle='#8b949e'; ctx.font='11px sans-serif';
    ctx.textBaseline='middle';
    for(let i=0;i<=5;i++){ const v=mn+(mx-mn)*i/5; const yy=Y(v);
      ctx.beginPath();ctx.moveTo(p.x,yy);ctx.lineTo(p.x+p.w,yy);ctx.stroke();
      ctx.textAlign='right'; ctx.fillText(this.yfmt(v),p.x-6,yy); }
    const zeroY=Y(0);
    ctx.strokeStyle='#3a4456'; ctx.beginPath();ctx.moveTo(p.x,zeroY);ctx.lineTo(p.x+p.w,zeroY);ctx.stroke();
    const bw=p.w/Math.max(1,this.bars.length);
    for(let i=0;i<this.bars.length;i++){ const b=this.bars[i];
      const x=p.x+bw*i+bw*0.12, w=bw*0.76;
      const yv=Y(b.value); const top=Math.min(yv,zeroY), hh=Math.abs(yv-zeroY);
      ctx.fillStyle=b.color||(b.value>=0?'#3fb950':'#f85149');
      ctx.fillRect(x,top,w,Math.max(1,hh));
      // x label (rotated if long)
      ctx.save(); ctx.fillStyle='#8b949e'; ctx.font='10px sans-serif';
      ctx.translate(p.x+bw*i+bw*0.5, p.y+p.h+6);
      const lbl=b.label.length>14? b.label.slice(0,13)+'…' : b.label;
      ctx.rotate(-Math.PI/5); ctx.textAlign='right'; ctx.textBaseline='middle';
      ctx.fillText(lbl,0,0); ctx.restore();
    }
    ctx.strokeStyle='#2a3242'; ctx.strokeRect(p.x,p.y,p.w,p.h);
  }
}

// =====================================================================
// Build charts
// =====================================================================
const EQ=DATA.equity||{ts:[],total:[],drawdown:[]};

const eqChart=new LineChart('eq-canvas','eq-tip');
const ddChart=new LineChart('dd-canvas','dd-tip');
eqChart.xfmt=fmtDateShort; eqChart.yfmt=v=>money(v,0);
eqChart.setData([{xs:EQ.ts,ys:EQ.total,color:'#58a6ff',label:'Equity',area:true,
  fill:'#58a6ff1f', baseline:Math.min.apply(null, EQ.total.length?EQ.total:[0])}],[]);
eqChart.tipFmt=(i,s)=> '<b>'+fmtTime(EQ.ts[i])+'</b><br>Equity: '+money(EQ.total[i])+
  '<br>DD: '+money(EQ.drawdown[i]);
ddChart.xfmt=fmtDateShort; ddChart.yfmt=v=>money(v,0);
ddChart.setData([{xs:EQ.ts,ys:EQ.drawdown,color:'#f85149',label:'Drawdown',area:true,
  fill:'#f851491f', baseline:0}],[]);
ddChart.tipFmt=(i,s)=> '<b>'+fmtTime(EQ.ts[i])+'</b><br>Drawdown: '+money(EQ.drawdown[i]);
// link zoom both ways
eqChart.onZoom=r=>{ ddChart.setRange(r); };
ddChart.onZoom=r=>{ eqChart.setRange(r); };
document.getElementById('eq-reset').addEventListener('click',()=>{ eqChart.reset(); ddChart.reset(); });

// Price + fills chart
const priceChart=new LineChart('price-canvas','price-tip');
priceChart.xfmt=fmtDateShort; priceChart.yfmt=v=>'$'+num(v,3);
const instSel=document.getElementById('price-inst');
(DATA.instruments||[]).forEach(n=>{ const o=document.createElement('option'); o.value=n;o.textContent=n; instSel.appendChild(o); });
function loadPrice(inst){
  const tr=(DATA.trades_by_inst||{})[inst]||{ts:[],price:[]};
  const fb=(DATA.fills_by_inst||{})[inst]||{ts:[],price:[],side:[]};
  const markers=[]; for(let i=0;i<fb.ts.length;i++){
    markers.push({x:fb.ts[i],y:fb.price[i],color:fb.side[i]==='BUY'?'#3fb950':'#f85149',
      label:fb.side[i]}); }
  priceChart.xrange=null;
  priceChart.setData([{xs:tr.ts,ys:tr.price,color:'#8b949e',label:inst,width:1.3}], markers);
  priceChart.tipFmt=(i,s)=> '<b>'+fmtTime(s.xs[i])+'</b><br>'+esc(inst)+': $'+num(s.ys[i],3);
  priceChart.draw();
}
instSel.addEventListener('change',()=>loadPrice(instSel.value));
document.getElementById('price-reset').addEventListener('click',()=>priceChart.reset());
if((DATA.instruments||[]).length){ instSel.value=DATA.instruments[0]; }

// Histogram of round-trip PnL
const histChart=new BarChart('hist-canvas','hist-tip');
histChart.yfmt=v=>num(v,0);
(function(){
  const pnls=DATA.round_trip_pnls||[];
  if(!pnls.length){ histChart.setBars([]); return; }
  let mn=Math.min.apply(null,pnls), mx=Math.max.apply(null,pnls);
  if(mn===mx){ mn-=1; mx+=1; }
  const nb=Math.min(24,Math.max(8,Math.round(Math.sqrt(pnls.length))));
  const w=(mx-mn)/nb; const counts=new Array(nb).fill(0);
  for(const v of pnls){ let b=Math.floor((v-mn)/w); if(b>=nb)b=nb-1; if(b<0)b=0; counts[b]++; }
  const bars=counts.map((cnt,i)=>{ const lo=mn+w*i, hi=lo+w; const mid=(lo+hi)/2;
    return {label:money(lo,0)+'..'+money(hi,0), value:cnt, color: mid>=0?'#3fb950':'#f85149'}; });
  histChart.setBars(bars);
})();

// Instrument PnL bar chart
const instChart=new BarChart('inst-canvas','inst-tip');
instChart.yfmt=v=>money(v,0);
(function(){
  const rows=DATA.instrument_stats||[];
  instChart.setBars(rows.map(r=>({label:r.instrument, value:r.pnl,
    color:r.pnl>=0?'#3fb950':'#f85149'})));
})();

// ---------- redraw orchestration (handles hidden->visible canvas sizing) ----------
function redrawAll(){
  eqChart.draw(); ddChart.draw(); priceChart.draw(); histChart.draw(); instChart.draw();
}
window.addEventListener('resize',redrawAll);

// ---------- Fills table (sortable + filterable) ----------
(function(){
  const cols=[
    {k:'ts_ns',  t:'Time',       fmt:v=>fmtTime(v), raw:true},
    {k:'instrument',t:'Instrument'},
    {k:'order_id', t:'Order'},
    {k:'side',   t:'Side',  fmt:v=>'<span class="badge '+(v==='BUY'?'buy':'sell')+'">'+esc(v)+'</span>', html:true},
    {k:'price',  t:'Price', fmt:v=>'$'+num(v,3)},
    {k:'qty',    t:'Qty',   fmt:v=>num(v,0)},
    {k:'liquidity',t:'Liq', fmt:v=>'<span class="badge '+(v==='Maker'?'maker':'taker')+'">'+esc(v)+'</span>', html:true},
    {k:'fee',    t:'Fee',   fmt:v=>'$'+num(v,4)},
  ];
  let rows=(DATA.fills||[]).slice();
  let sortKey='ts_ns', sortDir=1, filter='';
  const tbl=document.getElementById('fills-table');
  const cnt=document.getElementById('fill-count');
  function render(){
    let r=rows;
    if(filter){ const f=filter.toLowerCase();
      r=r.filter(x=>(x.instrument+' '+x.side+' '+x.liquidity+' '+x.order_id).toLowerCase().includes(f)); }
    r=r.slice().sort((a,b)=>{ const av=a[sortKey],bv=b[sortKey];
      if(av<bv)return -1*sortDir; if(av>bv)return 1*sortDir; return 0; });
    const head='<thead><tr>'+cols.map(c=>
      '<th data-k="'+c.k+'" class="'+(c.k===sortKey?(sortDir>0?'sort-asc':'sort-desc'):'')+'">'+c.t+'</th>').join('')+'</tr></thead>';
    const body='<tbody>'+r.slice(0,2000).map(x=>'<tr>'+cols.map(c=>{
      const v=x[c.k]; const cell=c.fmt?c.fmt(v):esc(v);
      return '<td>'+cell+'</td>'; }).join('')+'</tr>').join('')+'</tbody>';
    tbl.innerHTML=head+body;
    cnt.textContent=r.length+' fills'+(r.length>2000?' (showing 2000)':'');
    tbl.querySelectorAll('th').forEach(th=>th.addEventListener('click',()=>{
      const k=th.dataset.k; if(k===sortKey)sortDir*=-1; else {sortKey=k;sortDir=1;} render(); }));
  }
  document.getElementById('fill-filter').addEventListener('input',e=>{ filter=e.target.value; render(); });
  render();
})();

// ---------- Instrument table (sortable) ----------
(function(){
  const cols=[
    {k:'instrument',t:'Instrument'},
    {k:'pnl',t:'PnL',fmt:v=>'<span class="'+signClass(v)+'">'+money(v)+'</span>',html:true},
    {k:'num_fills',t:'Fills',fmt:v=>num(v,0)},
    {k:'num_round_trips',t:'Round trips',fmt:v=>num(v,0)},
    {k:'net_position',t:'Net pos',fmt:v=>num(v,0)},
    {k:'volume',t:'Volume',fmt:v=>num(v,0)},
  ];
  let rows=(DATA.instrument_stats||[]).slice();
  let sortKey='pnl', sortDir=-1;
  const tbl=document.getElementById('inst-table');
  function render(){
    const r=rows.slice().sort((a,b)=>{ const av=a[sortKey],bv=b[sortKey];
      if(av<bv)return -1*sortDir; if(av>bv)return 1*sortDir; return 0; });
    const head='<thead><tr>'+cols.map(c=>
      '<th data-k="'+c.k+'" class="'+(c.k===sortKey?(sortDir>0?'sort-asc':'sort-desc'):'')+'">'+c.t+'</th>').join('')+'</tr></thead>';
    const body='<tbody>'+r.map(x=>'<tr>'+cols.map(c=>{
      const v=x[c.k]; return '<td>'+(c.fmt?c.fmt(v):esc(v))+'</td>'; }).join('')+'</tr>').join('')+'</tbody>';
    tbl.innerHTML=head+body;
    tbl.querySelectorAll('th').forEach(th=>th.addEventListener('click',()=>{
      const k=th.dataset.k; if(k===sortKey)sortDir*=-1; else {sortKey=k;sortDir=(k==='instrument'?1:-1);} render(); }));
  }
  render();
})();

// ---------- Full stats panel ----------
(function(){
  const moneyKeys=new Set(['starting_balance','ending_balance','pnl_total','max_drawdown',
    'avg_buy_price','avg_sell_price','expectancy','total_fees','turnover']);
  const pctKeys=new Set(['pnl_pct','max_drawdown_pct','win_rate','volatility','exposure_pct']);
  function fmtVal(k,v){
    if(v===null||v===undefined) return '-';
    if(typeof v==='number'){
      if(pctKeys.has(k)) return pct(v);
      if(moneyKeys.has(k)) return money(v);
      return num(v, Number.isInteger(v)?0:3);
    }
    return esc(v);
  }
  const grid=document.getElementById('stats-grid'); const items=[];
  Object.keys(S).forEach(k=>{ items.push('<div class="stat"><span class="k">'+esc(k)+
    '</span><span class="v">'+fmtVal(k,S[k])+'</span></div>'); });
  grid.innerHTML=items.join('')||'<div class="muted">no summary</div>';

  const mg=document.getElementById('meta-grid'); const mitems=[];
  Object.keys(META).forEach(k=>{ let v=META[k];
    if(k==='generated_unix_ns'){ v=fmtTime(v); }
    mitems.push('<div class="stat"><span class="k">'+esc(k)+
      '</span><span class="v">'+esc(v)+'</span></div>'); });
  mg.innerHTML=mitems.join('')||'<div class="muted">no meta</div>';
})();

// initial paint
loadPriceIfReady();
function loadPriceIfReady(){ if((DATA.instruments||[]).length){ loadPrice(DATA.instruments[0]); } }
redrawAll();
"""


# ===========================================================================
# CLI
# ===========================================================================
def main(argv=None):
    ap = argparse.ArgumentParser(
        description="Generate a self-contained interactive backtest dashboard HTML.")
    ap.add_argument("--export-dir", required=True,
                    help="path to a backtest export directory (report.json, *.csv, meta.json)")
    ap.add_argument("--out", default="dashboard.html", help="output HTML path")
    args = ap.parse_args(argv)

    if not os.path.isdir(args.export_dir):
        print("error: export-dir not found: %s" % args.export_dir, file=sys.stderr)
        return 2

    data = DashboardData.load(args.export_dir)
    if data.warnings:
        print("warnings: " + ", ".join(data.warnings), file=sys.stderr)
    out = DashboardRenderer(data).write(args.out)
    size = os.path.getsize(out)
    print("Wrote %s (%.1f KB) from %s" % (out, size / 1024.0, args.export_dir))
    print("  equity points: %d | fills: %d | trades: %d | round-trips: %d | instruments: %d"
          % (len(data.equity), len(data.fills), len(data.trades),
             len(data.round_trips), len(data.instruments())))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
