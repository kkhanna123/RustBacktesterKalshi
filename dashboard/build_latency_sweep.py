#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""build_latency_sweep.py -- zero-dependency LATENCY-SWEEP comparison page generator.

PURPOSE
-------
Answers one question: **"does my edge survive latency?"**

A *latency sweep* is the SAME strategy backtested several times, once per simulated
order-to-exchange latency (e.g. 0ns, 100ms, 500ms, 1s). Each run is a standard
``--out-dir`` export from ``kalshi-backtest``. This tool ingests several such export
dirs and overlays them into a SINGLE self-contained ``latency_sweep.html`` so you can
*see* the edge erode (or hold) as latency grows.

LATENCY FILL MODEL (one line, shown in the page header)
-------------------------------------------------------
Each order is held for ``--latency-ns`` nanoseconds before it can match; it then fills
against the order book *as it exists after that delay*, so quotes you would have hit at
zero latency may have moved, been taken, or pulled -- which is exactly the edge decay
this page visualizes.

WHAT THE PAGE SHOWS
-------------------
1. **Overlaid equity curves** -- one interactive Canvas line per latency (distinct
   colors, legend, hover showing latency+time+equity, drag-to-zoom, reset). The
   centerpiece. x-axis = wall-clock time (ts_ns -> UTC), y = account equity.
2. **Halt markers** -- for any run whose ``summary.halted`` is true, a red vertical
   line + crossed marker is drawn on that run's curve at the located halt point
   (see "HALT MARKER LOCATION" below), labelled with ``halt_reason``.
3. **Comparison table** -- one row per latency: pnl_total, pnl_pct, sharpe,
   max_drawdown_pct, win_rate, num_fills, total_fees, risk_rejections,
   halted/halt_reason. Sortable; PnL colour-coded.
4. **Decay mini-chart** -- pnl_total and Sharpe vs latency (grouped bars) so you see
   the edge eroding as latency grows.

HALT MARKER LOCATION (graceful degradation, in priority order)
--------------------------------------------------------------
For a run with ``summary.halted == true`` we locate the halt timestamp as:
  1. an explicit ``halt_ts`` / ``halt_unix_ns`` / ``halt_ts_ns`` field if present in
     ``report.json`` (top-level or under ``summary``) or ``meta.json``; else
  2. the first equity timestamp at/after the running peak where the curve goes flat
     (i.e. equity stops changing through to the end of the series -- the "flatline
     after a halt" signature); else
  3. the run's minimum-equity timestamp.
The marker is always labelled from ``halt_reason``. If ``halted`` is absent entirely
(older runs) no marker is drawn -- the page degrades silently.

DESIGN
------
    SweepRun              -- loads one export dir (label, latency_ns, summary, equity)
    LatencySweepRenderer  -- turns a list[SweepRun] into the final HTML string
    generate_synthetic_sweep() -- writes a realistic fake sweep for dev/verification

The output HTML embeds all data + CSS + JS inline (vanilla-JS Canvas charts, the same
dark-theme idiom as ``build_dashboard.py``) so it opens by double-click, fully offline,
no server / no internet / no CDN / no pip installs.

USAGE
-----
    # Explicit runs (repeatable):
    python build_latency_sweep.py \
        --runs 'label=0ns,dir=../data/exports/sweep_0' \
        --runs 'label=1s,dir=../data/exports/sweep_1s' \
        --out latency_sweep.html

    # Auto-discover a sweep dir of lat_<ns> subdirs:
    python build_latency_sweep.py --sweep-dir ../data/exports/sweep_sample \
        --out latency_sweep.html

    # Regenerate the synthetic sample (and build a page from it):
    python build_latency_sweep.py --make-sample

Python 3.8+ compatible. Only the standard library is required.
"""
from __future__ import annotations

import argparse
import csv
import json
import math
import os
import sys
from datetime import datetime, timezone

# ---------------------------------------------------------------------------
# Export-dir contract (a subset of build_dashboard.py's; we only need these).
# ---------------------------------------------------------------------------
REPORT_JSON = "report.json"
META_JSON = "meta.json"
EQUITY_CSV = "equity.csv"

NS_PER_MS = 1_000_000
NS_PER_S = 1_000_000_000

# Distinct, colour-blind-friendly-ish palette for the overlaid curves.
PALETTE = [
    "#58a6ff",  # blue
    "#3fb950",  # green
    "#d29922",  # amber
    "#bc8cff",  # purple
    "#f85149",  # red
    "#39c5cf",  # teal
    "#ff7b72",  # salmon
    "#7ee787",  # light-green
]


# ===========================================================================
# Small numeric coercion helpers (same semantics as build_dashboard.py).
# ===========================================================================
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


def _esc(s):
    return (str(s).replace("&", "&amp;").replace("<", "&lt;")
            .replace(">", "&gt;").replace('"', "&quot;"))


def humanize_latency(ns):
    """Render a latency in ns as a compact human label: 0ns, 100ms, 500ms, 1s, 1.5s."""
    ns = int(ns)
    if ns <= 0:
        return "0ns"
    if ns % NS_PER_S == 0:
        return "%ds" % (ns // NS_PER_S)
    if ns >= NS_PER_S:
        return ("%.3f" % (ns / NS_PER_S)).rstrip("0").rstrip(".") + "s"
    if ns % NS_PER_MS == 0:
        return "%dms" % (ns // NS_PER_MS)
    if ns >= NS_PER_MS:
        return ("%.3f" % (ns / NS_PER_MS)).rstrip("0").rstrip(".") + "ms"
    if ns >= 1000:
        return "%dus" % (ns // 1000)
    return "%dns" % ns


# ===========================================================================
# SweepRun -- one backtest export dir at a single latency.
# ===========================================================================
class SweepRun:
    """Loads ONE latency run's export dir.

    Attributes
    ----------
    label : str
        Human label for the run (e.g. ``"100ms"``). Defaults to the humanized latency.
    latency_ns : int
        Simulated order latency in nanoseconds for this run.
    summary : dict
        ``report.json`` -> ``summary`` (pnl_total, sharpe, halted, halt_reason, ...).
    meta : dict
        ``meta.json`` contents.
    eq_ts / eq_total / eq_dd : list
        Parallel equity-curve arrays (ts_ns, total, drawdown).
    halt_ns : int | None
        Located halt timestamp (ns) if the run halted, else None. See module docstring
        "HALT MARKER LOCATION" for how this is determined.
    warnings : list[str]
        Non-fatal load issues (missing files, etc.).
    """

    def __init__(self, label, latency_ns, export_dir):
        self.label = label
        self.latency_ns = int(latency_ns)
        self.export_dir = export_dir
        self.report = {}
        self.summary = {}
        self.meta = {}
        self.eq_ts = []
        self.eq_total = []
        self.eq_dd = []
        self.halt_ns = None
        self.warnings = []

    # ---- loading ---------------------------------------------------------
    @classmethod
    def load(cls, label, latency_ns, export_dir):
        self = cls(label, latency_ns, export_dir)
        self._load_report()
        self._load_meta()
        self._load_equity()
        # latency_ns may be overridden by meta if the caller didn't supply one.
        if self.latency_ns == 0:
            self.latency_ns = _i(self.meta.get("latency_ns", self.summary.get("latency_ns", 0)))
        if not self.label:
            self.label = humanize_latency(self.latency_ns)
        self.halt_ns = self._locate_halt()
        return self

    def _path(self, name):
        return os.path.join(self.export_dir, name)

    def _load_report(self):
        p = self._path(REPORT_JSON)
        if not os.path.isfile(p):
            self.warnings.append("missing %s" % REPORT_JSON)
            return
        with open(p, "r") as f:
            self.report = json.load(f)
        self.summary = self.report.get("summary", {}) or {}
        self._report_equity = self.report.get("equity_curve", []) or []

    def _load_meta(self):
        p = self._path(META_JSON)
        if not os.path.isfile(p):
            self.warnings.append("missing %s" % META_JSON)
            return
        with open(p, "r") as f:
            self.meta = json.load(f)

    def _load_equity(self):
        p = self._path(EQUITY_CSV)
        if not os.path.isfile(p):
            # Fall back to report.json equity_curve if present.
            pts = getattr(self, "_report_equity", None) or []
            if not pts:
                self.warnings.append("missing %s" % EQUITY_CSV)
                return
            peak = float("-inf")
            for pt in pts:
                tot = _f(pt.get("total"))
                peak = max(peak, tot)
                self.eq_ts.append(_i(pt.get("ts_ns")))
                self.eq_total.append(tot)
                self.eq_dd.append(round(tot - peak, 6))
            return
        with open(p, "r", newline="") as f:
            for r in csv.DictReader(f):
                self.eq_ts.append(_i(r.get("ts_ns")))
                self.eq_total.append(_f(r.get("total")))
                self.eq_dd.append(_f(r.get("drawdown")))

    # ---- halt location ---------------------------------------------------
    def is_halted(self):
        """True iff this run reports halted. Absent field -> treated as not halted."""
        return bool(self.summary.get("halted", False))

    def halt_reason(self):
        return str(self.summary.get("halt_reason", "") or "")

    def _explicit_halt_ns(self):
        """Pull an explicit halt timestamp from report/summary/meta if present."""
        for src in (self.summary, self.report, self.meta):
            for key in ("halt_ts_ns", "halt_unix_ns", "halt_ts", "halt_ns"):
                if key in src and src[key] not in (None, ""):
                    return _i(src[key])
        return None

    def _locate_halt(self):
        """Locate the halt timestamp (ns) for a halted run; see module docstring.

        Returns None if the run is not halted (or has no equity to anchor a marker).
        """
        if not self.is_halted():
            return None
        if not self.eq_ts:
            return None
        # (1) explicit field wins.
        explicit = self._explicit_halt_ns()
        if explicit:
            return explicit
        # (2) flatline-after-peak: the first index at/after the running-peak index
        #     from which equity never changes again (within a tiny epsilon).
        n = len(self.eq_total)
        peak_idx = 0
        peak_val = self.eq_total[0]
        for i in range(1, n):
            if self.eq_total[i] > peak_val:
                peak_val = self.eq_total[i]
                peak_idx = i
        eps = max(1e-9, abs(peak_val) * 1e-9)
        # Find the start of the final flat run (constant tail).
        flat_start = n - 1
        last = self.eq_total[-1]
        i = n - 1
        while i > 0 and abs(self.eq_total[i - 1] - last) <= eps:
            flat_start = i - 1
            i -= 1
        if flat_start > peak_idx and flat_start < n - 1:
            # Genuine flat tail after the peak -> that's the halt point.
            return self.eq_ts[flat_start]
        # (3) fall back to the minimum-equity timestamp.
        min_idx = min(range(n), key=lambda k: self.eq_total[k])
        return self.eq_ts[min_idx]

    # ---- payload ---------------------------------------------------------
    def to_payload(self, color):
        """JSON-serializable dict for the front-end."""
        s = self.summary
        return {
            "label": self.label,
            "latency_ns": self.latency_ns,
            "color": color,
            "ts": self.eq_ts,
            "total": self.eq_total,
            "drawdown": self.eq_dd,
            "halt_ns": self.halt_ns,
            "halted": self.is_halted(),
            "halt_reason": self.halt_reason(),
            "summary": {
                "pnl_total": _f(s.get("pnl_total")),
                "pnl_pct": _f(s.get("pnl_pct")),
                "sharpe": _f(s.get("sharpe")),
                "max_drawdown_pct": _f(s.get("max_drawdown_pct")),
                "win_rate": _f(s.get("win_rate")),
                "num_fills": _i(s.get("num_fills")),
                "total_fees": _f(s.get("total_fees")),
                "risk_rejections": _i(s.get("risk_rejections")),
            },
            "warnings": self.warnings,
        }


# ===========================================================================
# Run discovery / CLI parsing
# ===========================================================================
def parse_run_spec(spec):
    """Parse a ``--runs 'label=0ns,dir=../data/exports/sweep_0'`` token.

    Returns ``(label, dir, latency_ns_or_None)``. ``latency=`` is optional; if omitted
    we try to infer it from the label or from meta.json at load time.
    """
    label = None
    directory = None
    latency_ns = None
    for part in spec.split(","):
        part = part.strip()
        if not part:
            continue
        if "=" not in part:
            # Bare token: treat as the dir.
            directory = part
            continue
        k, v = part.split("=", 1)
        k = k.strip().lower()
        v = v.strip()
        if k == "label":
            label = v
        elif k in ("dir", "path", "out-dir", "out_dir"):
            directory = v
        elif k in ("latency", "latency_ns", "lat", "lat_ns"):
            latency_ns = _parse_latency(v)
    if directory is None:
        raise ValueError("run spec missing dir=: %r" % spec)
    if latency_ns is None and label:
        latency_ns = _parse_latency(label, soft=True)
    return label, directory, latency_ns


def _parse_latency(text, soft=False):
    """Parse a latency string like '0ns','100ms','500ms','1s','1500000000' -> ns."""
    t = str(text).strip().lower()
    if not t:
        return None
    try:
        if t.endswith("ns"):
            return int(float(t[:-2]))
        if t.endswith("us"):
            return int(float(t[:-2]) * 1000)
        if t.endswith("ms"):
            return int(float(t[:-2]) * NS_PER_MS)
        if t.endswith("s"):
            return int(float(t[:-1]) * NS_PER_S)
        return int(float(t))  # bare number = ns
    except ValueError:
        if soft:
            return None
        raise ValueError("cannot parse latency: %r" % text)


def discover_sweep_dir(sweep_dir):
    """Auto-discover ``lat_<ns>`` subdirs under ``sweep_dir``.

    Returns a list of ``(label, dir, latency_ns)`` sorted by latency ascending.
    """
    runs = []
    for name in sorted(os.listdir(sweep_dir)):
        full = os.path.join(sweep_dir, name)
        if not os.path.isdir(full):
            continue
        lat = None
        low = name.lower()
        if low.startswith("lat_"):
            lat = _parse_latency(low[4:], soft=True)
        elif low.startswith("latency_"):
            lat = _parse_latency(low[8:], soft=True)
        if lat is None:
            # Not a recognizable latency dir; skip but only if it lacks a report.
            if not os.path.isfile(os.path.join(full, REPORT_JSON)):
                continue
            lat = 0
        runs.append((humanize_latency(lat), full, lat))
    runs.sort(key=lambda r: r[2])
    return runs


# ===========================================================================
# Renderer
# ===========================================================================
class LatencySweepRenderer:
    """Renders a list of :class:`SweepRun` into a self-contained HTML document."""

    def __init__(self, runs):
        # Order runs by latency so colors / legend / decay chart read left-to-right.
        self.runs = sorted(runs, key=lambda r: r.latency_ns)

    def render(self):
        runs_payload = []
        all_warnings = []
        for i, run in enumerate(self.runs):
            color = PALETTE[i % len(PALETTE)]
            runs_payload.append(run.to_payload(color))
            for w in run.warnings:
                all_warnings.append("%s: %s" % (run.label, w))
        payload = {
            "runs": runs_payload,
            "warnings": all_warnings,
            "currency": self._currency(),
        }
        data_json = json.dumps(payload, separators=(",", ":"), default=str)
        # Guard the inline <script> tag against a literal </script> in data.
        data_json = data_json.replace("</", "<\\/")
        generated = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")
        html = _TEMPLATE
        html = html.replace("__GENERATED__", generated)
        html = html.replace("__NRUNS__", str(len(self.runs)))
        html = html.replace("__CSS__", _CSS)
        html = html.replace("__DATA_JSON__", data_json)
        html = html.replace("__JS__", _JS)
        return html

    def _currency(self):
        for run in self.runs:
            c = run.summary.get("currency") or run.meta.get("currency")
            if c:
                return c
        return "USD"

    def write(self, out_path):
        html = self.render()
        os.makedirs(os.path.dirname(os.path.abspath(out_path)) or ".", exist_ok=True)
        with open(out_path, "w", encoding="utf-8") as f:
            f.write(html)
        return out_path


# ===========================================================================
# Front-end assets (CSS / HTML template / JS) -- all inlined -> offline.
# Reuses build_dashboard.py's dark theme + the vanilla-JS Canvas LineChart idiom.
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
  border-bottom:1px solid var(--border);padding-bottom:14px;margin-bottom:14px}
header.top h1{font-size:22px;margin:0;font-weight:650}
.muted{color:var(--muted)}
.model-note{background:#58a6ff14;border:1px solid #2f4a6b;border-radius:8px;
  padding:9px 13px;margin-bottom:18px;font-size:12.8px;color:var(--txt)}
.model-note b{color:var(--accent)}

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

/* legend */
.legend{display:flex;flex-wrap:wrap;gap:14px;margin:2px 0 10px}
.legend .item{display:flex;align-items:center;gap:6px;cursor:pointer;user-select:none;
  font-size:12.5px;padding:2px 6px;border-radius:6px}
.legend .item:hover{background:#ffffff08}
.legend .item.off{opacity:.38}
.legend .swatch{width:14px;height:3px;border-radius:2px}
.legend .halt{color:var(--red);font-weight:600;margin-left:2px}

/* tables */
table{width:100%;border-collapse:collapse;font-size:12.8px}
th,td{text-align:right;padding:7px 10px;border-bottom:1px solid var(--border);white-space:nowrap}
th:first-child,td:first-child{text-align:left}
th{color:var(--muted);font-weight:600;cursor:pointer;user-select:none;position:sticky;top:0;background:var(--panel)}
th.sort-asc::after{content:" \25B2";color:var(--accent)}
th.sort-desc::after{content:" \25BC";color:var(--accent)}
tbody tr:hover{background:#ffffff08}
.table-scroll{max-height:520px;overflow:auto;border:1px solid var(--border);border-radius:8px}
.pos{color:var(--green)} .neg{color:var(--red)} .neu{color:var(--txt)}
.swatch-cell{display:inline-block;width:11px;height:11px;border-radius:3px;margin-right:7px;vertical-align:-1px}
.badge{display:inline-block;padding:1px 8px;border-radius:10px;font-size:11px;font-weight:600}
.badge.halted{background:#f8514922;color:var(--red)}
.badge.ok{background:#3fb95022;color:var(--green)}

.warnbar{background:#d2992218;border:1px solid var(--amber);color:var(--amber);
  border-radius:8px;padding:8px 12px;margin-bottom:14px;font-size:12.5px}
.footer{color:var(--muted);font-size:11.5px;text-align:center;margin:24px 0 8px}
"""

_TEMPLATE = r"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Latency Sweep &middot; Backtest</title>
<style>__CSS__</style>
</head>
<body>
<div class="wrap">
  <header class="top">
    <div>
      <h1>Latency Sweep &mdash; does the edge survive latency?</h1>
      <div class="muted" style="font-size:12.5px">Same strategy, __NRUNS__ latencies overlaid</div>
    </div>
    <div class="muted" style="font-size:12px">generated __GENERATED__</div>
  </header>

  <div class="model-note">
    <b>Latency fill model:</b> each order is held <b>latency_ns</b> before it can match,
    then fills against the book <i>as it exists after that delay</i> &mdash; quotes you'd
    have hit at 0&nbsp;latency may have moved, been taken, or pulled. This page overlays the
    same strategy at several latencies so you can watch the edge decay.
  </div>

  <div id="warnings"></div>

  <!-- 1. OVERLAID EQUITY CURVES (centerpiece) -->
  <div class="panel">
    <h2>Overlaid equity curves</h2>
    <div class="hint">One line per latency &middot; hover for latency / time / equity &middot;
      drag horizontally to zoom &middot; Reset &middot; click a legend item to toggle.
      Red &#10005; / vertical line = risk halt.</div>
    <div class="legend" id="legend"></div>
    <div class="toolbar"><button class="btn" id="eq-reset">Reset zoom</button></div>
    <div class="chart-wrap"><canvas id="eq-canvas" height="340"></canvas>
      <div class="chart-tip" id="eq-tip"></div></div>
  </div>

  <!-- 3. COMPARISON TABLE -->
  <div class="panel">
    <h2>Comparison table</h2>
    <div class="hint">One row per latency. Click a header to sort. PnL colour-coded.</div>
    <div class="table-scroll"><table id="cmp-table"></table></div>
  </div>

  <!-- 4. DECAY MINI-CHART -->
  <div class="row">
    <div class="panel">
      <h2>Edge decay vs latency</h2>
      <div class="hint">PnL total (left, bars) and Sharpe (right, line) against latency.
        Watch the edge erode as latency grows.</div>
      <div class="chart-wrap"><canvas id="decay-canvas" height="260"></canvas>
        <div class="chart-tip" id="decay-tip"></div></div>
    </div>
  </div>

  <div class="footer">Self-contained offline latency-sweep page &middot; no server, no internet required.</div>
</div>

<script id="payload" type="application/json">__DATA_JSON__</script>
<script>__JS__</script>
</body>
</html>
"""

_JS = r"""
"use strict";
const DATA = JSON.parse(document.getElementById('payload').textContent);
const RUNS = DATA.runs || [];
const CUR = DATA.currency || 'USD';

// ---------- formatting helpers (same idiom as build_dashboard.py) ----------
function nsToDate(ns){ return new Date(Number(ns)/1e6); }
function fmtTime(ns){
  const d = nsToDate(ns);
  return d.toISOString().replace('T',' ').replace('.000Z',' UTC').replace('Z',' UTC');
}
function fmtDateShort(ns){
  const d = nsToDate(ns); const p=n=>String(n).padStart(2,'0');
  return (d.getUTCMonth()+1)+'/'+d.getUTCDate()+' '+p(d.getUTCHours())+':'+p(d.getUTCMinutes());
}
function num(v,dp){ if(v===null||v===undefined||v==='') return '-';
  const n=Number(v); if(!isFinite(n)) return '-';
  return n.toLocaleString(undefined,{minimumFractionDigits:dp,maximumFractionDigits:dp}); }
function money(v,dp){ if(v===null||v===undefined||v==='') return '-';
  const n=Number(v); if(!isFinite(n)) return '-';
  return (n<0?'-':'')+'$'+Math.abs(n).toLocaleString(undefined,
    {minimumFractionDigits:dp===undefined?2:dp,maximumFractionDigits:dp===undefined?2:dp}); }
function pct(v,dp){ if(v===null||v===undefined||v==='') return '-';
  const n=Number(v); if(!isFinite(n)) return '-';
  return (n*100).toFixed(dp===undefined?2:dp)+'%'; }
function signClass(n){ return n>0?'pos':(n<0?'neg':'neu'); }
function esc(s){ return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;'); }

// ---------- warnings ----------
(function(){
  const w = DATA.warnings || [];
  if(w.length){ document.getElementById('warnings').innerHTML =
    '<div class="warnbar">Partial sweep: '+w.map(esc).join(', ')+'</div>'; }
})();

// =====================================================================
// MultiLineChart -- overlays N equity curves on one Canvas.
//   - DPR-aware crisp rendering
//   - hover crosshair + tooltip (nearest point across all visible series)
//   - drag horizontally to zoom; reset restores full range
//   - per-series visibility toggle (driven by the legend)
//   - per-series "halt" markers (red vertical line + crossed X)
// (Same structural idiom as build_dashboard.py's LineChart, extended for N series.)
// =====================================================================
class MultiLineChart{
  constructor(canvasId, tipId){
    this.canvas=document.getElementById(canvasId);
    this.tip=document.getElementById(tipId);
    this.series=[];        // [{xs,ys,color,label,visible,halt_ns,halt_reason}]
    this.xfmt=fmtDateShort; this.yfmt=v=>money(v,0);
    this.xrange=null; this.fullx=null;
    this.pad={l:66,r:14,t:12,b:26};
    this._hoverX=null; this._drag=null;
    this._bind();
  }
  setData(series){
    this.series=series||[];
    let mn=Infinity,mx=-Infinity;
    for(const s of this.series){ for(const x of s.xs){ if(x<mn)mn=x; if(x>mx)mx=x; } }
    this.fullx=(mn<=mx)?[mn,mx]:[0,1];
    this.xrange=this.fullx.slice();
  }
  reset(){ this.xrange=this.fullx.slice(); this.draw(); }
  _bind(){
    const c=this.canvas;
    c.addEventListener('mousemove',e=>this._move(e));
    c.addEventListener('mouseleave',()=>{ this.tip.style.display='none'; this._hoverX=null; this.draw(); });
    c.addEventListener('mousedown',e=>{ this._drag={x0:this._px(e)}; });
    window.addEventListener('mouseup',e=>{
      if(this._drag){
        const x1=this._px(e), x0=this._drag.x0; this._drag=null;
        if(Math.abs(x1-x0)>6){
          const a=this._invX(Math.min(x0,x1)), b=this._invX(Math.max(x0,x1));
          this.xrange=[a,b];
        }
        this.draw();
      }
    });
  }
  _px(e){ const r=this.canvas.getBoundingClientRect(); return e.clientX-r.left; }
  _py(e){ const r=this.canvas.getBoundingClientRect(); return e.clientY-r.top; }
  _dims(){ return {w:this.canvas.clientWidth,h:this.canvas.clientHeight}; }
  _plot(){ const {w,h}=this._dims(); return {x:this.pad.l,y:this.pad.t,
    w:w-this.pad.l-this.pad.r,h:h-this.pad.t-this.pad.b}; }
  _X(v){ const p=this._plot(),[a,b]=this.xrange; return p.x+(b===a?0:(v-a)/(b-a))*p.w; }
  _invX(px){ const p=this._plot(),[a,b]=this.xrange; return a+(px-p.x)/p.w*(b-a); }
  _vis(){ return this.series.filter(s=>s.visible!==false); }
  _yrange(){
    let mn=Infinity,mx=-Infinity; const [a,b]=this.xrange;
    for(const s of this._vis()){ for(let i=0;i<s.xs.length;i++){ const x=s.xs[i];
      if(x<a||x>b)continue; const y=s.ys[i]; if(y<mn)mn=y; if(y>mx)mx=y; } }
    if(!isFinite(mn)||!isFinite(mx)){ mn=0;mx=1; }
    if(mn===mx){ mn-=1; mx+=1; }
    const pad=(mx-mn)*0.08; return [mn-pad,mx+pad];
  }
  _move(e){
    const px=this._px(e); this._hoverX=this._invX(px); this.draw();
    let best=null,bestS=null;
    for(const s of this._vis()){
      if(!s.xs.length)continue;
      let lo=0,hi=s.xs.length-1;
      while(lo<=hi){ const m=(lo+hi)>>1; if(s.xs[m]<this._hoverX)lo=m+1; else hi=m-1; }
      const cand=[lo-1,lo].filter(i=>i>=0&&i<s.xs.length);
      for(const i of cand){ const d=Math.abs(s.xs[i]-this._hoverX);
        if(best===null||d<best.d){ best={i,d}; bestS=s; } }
    }
    if(best&&bestS){
      const i=best.i, x=bestS.xs[i];
      this.tip.innerHTML='<b>'+esc(bestS.label)+'</b> &middot; '+this.xfmt(x)+
        '<br>equity: '+money(bestS.ys[i]);
      this.tip.style.left=this._X(x)+'px';
      this.tip.style.top=(this.pad.t+8)+'px';
      this.tip.style.display='block';
    } else { this.tip.style.display='none'; }
  }
  draw(){
    const c=this.canvas,dpr=window.devicePixelRatio||1;
    const W=c.clientWidth,H=c.clientHeight; if(W===0)return;
    c.width=W*dpr;c.height=H*dpr;
    const ctx=c.getContext('2d'); ctx.setTransform(dpr,0,0,dpr,0,0); ctx.clearRect(0,0,W,H);
    const p=this._plot(); const [ymn,ymx]=this._yrange();
    const Y=v=> p.y+(ymx===ymn?0:(1-(v-ymn)/(ymx-ymn)))*p.h;
    // grid + y labels
    ctx.strokeStyle='#222b38'; ctx.fillStyle='#8b949e'; ctx.font='11px sans-serif';
    ctx.lineWidth=1; ctx.textBaseline='middle';
    for(let i=0;i<=5;i++){ const v=ymn+(ymx-ymn)*i/5; const yy=Y(v);
      ctx.beginPath(); ctx.moveTo(p.x,yy); ctx.lineTo(p.x+p.w,yy); ctx.stroke();
      ctx.textAlign='right'; ctx.fillText(this.yfmt(v),p.x-6,yy); }
    // x labels
    ctx.textAlign='center'; ctx.textBaseline='top';
    const [xa,xb]=this.xrange;
    for(let i=0;i<=6;i++){ const xv=xa+(xb-xa)*i/6; ctx.fillText(this.xfmt(xv),this._X(xv),p.y+p.h+5); }
    // series
    for(const s of this._vis()){
      ctx.lineWidth=s.width||1.7; ctx.strokeStyle=s.color;
      ctx.beginPath(); let started=false;
      for(let i=0;i<s.xs.length;i++){ const x=s.xs[i]; if(x<xa||x>xb)continue;
        const X=this._X(x),Yv=Y(s.ys[i]);
        if(!started){ ctx.moveTo(X,Yv); started=true; } else ctx.lineTo(X,Yv); }
      ctx.stroke();
    }
    // halt markers (drawn on top, only for visible halted series)
    for(const s of this._vis()){
      if(s.halt_ns==null) continue;
      const hx=s.halt_ns; if(hx<xa||hx>xb)continue;
      // y at the halt point = the series value at/just-after halt_ns
      let hy=null;
      for(let i=0;i<s.xs.length;i++){ if(s.xs[i]>=hx){ hy=s.ys[i]; break; } }
      if(hy==null && s.ys.length) hy=s.ys[s.ys.length-1];
      const X=this._X(hx), Yv=Y(hy);
      // vertical line
      ctx.strokeStyle='#f8514999'; ctx.lineWidth=1.3; ctx.setLineDash([4,3]);
      ctx.beginPath(); ctx.moveTo(X,p.y); ctx.lineTo(X,p.y+p.h); ctx.stroke(); ctx.setLineDash([]);
      // crossed X marker
      const r=5; ctx.strokeStyle='#f85149'; ctx.lineWidth=2;
      ctx.beginPath(); ctx.moveTo(X-r,Yv-r); ctx.lineTo(X+r,Yv+r);
      ctx.moveTo(X+r,Yv-r); ctx.lineTo(X-r,Yv+r); ctx.stroke();
      // reason label near the top of the line
      ctx.fillStyle='#f85149'; ctx.font='10px sans-serif';
      ctx.textAlign='left'; ctx.textBaseline='top';
      const lbl='HALT'+(s.halt_reason?(': '+s.halt_reason):'');
      const tx=Math.min(X+4, p.x+p.w-90);
      ctx.fillText(lbl, tx, p.y+2);
    }
    // hover crosshair
    if(this._hoverX!=null && this._hoverX>=xa && this._hoverX<=xb){
      const X=this._X(this._hoverX);
      ctx.strokeStyle='#58a6ff66'; ctx.lineWidth=1;
      ctx.beginPath(); ctx.moveTo(X,p.y); ctx.lineTo(X,p.y+p.h); ctx.stroke();
    }
    ctx.strokeStyle='#2a3242'; ctx.lineWidth=1; ctx.strokeRect(p.x,p.y,p.w,p.h);
  }
}

// =====================================================================
// Build the overlaid equity chart + legend
// =====================================================================
const eqChart=new MultiLineChart('eq-canvas','eq-tip');
const eqSeries=RUNS.map(r=>({
  xs:r.ts, ys:r.total, color:r.color, label:r.label, visible:true,
  halt_ns: r.halted? r.halt_ns : null, halt_reason: r.halt_reason||'',
}));
eqChart.setData(eqSeries);

(function buildLegend(){
  const host=document.getElementById('legend');
  host.innerHTML = RUNS.map((r,i)=>
    '<div class="item" data-i="'+i+'">'+
      '<span class="swatch" style="background:'+r.color+'"></span>'+
      esc(r.label)+
      (r.halted?'<span class="halt">&#10005; halt'+(r.halt_reason?(' ('+esc(r.halt_reason)+')'):'')+'</span>':'')+
    '</div>').join('');
  host.querySelectorAll('.item').forEach(el=>{
    el.addEventListener('click',()=>{
      const i=+el.dataset.i; const s=eqSeries[i];
      s.visible = (s.visible===false);
      el.classList.toggle('off', s.visible===false);
      eqChart.draw();
    });
  });
})();
document.getElementById('eq-reset').addEventListener('click',()=>eqChart.reset());

// =====================================================================
// 3. Comparison table (sortable, PnL colour-coded)
// =====================================================================
(function(){
  const cols=[
    {k:'label', t:'Latency', fmt:(v,row)=>'<span class="swatch-cell" style="background:'+row._color+'"></span>'+esc(v), html:true},
    {k:'pnl_total', t:'PnL total', fmt:v=>'<span class="'+signClass(v)+'">'+money(v)+'</span>', html:true},
    {k:'pnl_pct', t:'PnL %', fmt:v=>'<span class="'+signClass(v)+'">'+pct(v)+'</span>', html:true},
    {k:'sharpe', t:'Sharpe', fmt:v=>'<span class="'+signClass(v)+'">'+num(v,2)+'</span>', html:true},
    {k:'max_drawdown_pct', t:'Max DD %', fmt:v=>'<span class="neg">'+pct(v)+'</span>', html:true},
    {k:'win_rate', t:'Win rate', fmt:v=>pct(v)},
    {k:'num_fills', t:'Fills', fmt:v=>num(v,0)},
    {k:'total_fees', t:'Fees', fmt:v=>money(v)},
    {k:'risk_rejections', t:'Risk rej.', fmt:v=>num(v,0)},
    {k:'halted', t:'Halt', fmt:(v,row)=> v
        ? '<span class="badge halted">HALTED'+(row._reason?(' &middot; '+esc(row._reason)):'')+'</span>'
        : '<span class="badge ok">ok</span>', html:true},
  ];
  let rows=RUNS.map(r=>{
    const o=Object.assign({}, r.summary);
    o.label=r.label; o.latency_ns=r.latency_ns; o._color=r.color;
    o.halted=r.halted; o._reason=r.halt_reason;
    return o;
  });
  let sortKey='latency_ns', sortDir=1;
  const tbl=document.getElementById('cmp-table');
  function render(){
    const r=rows.slice().sort((a,b)=>{ const av=a[sortKey],bv=b[sortKey];
      if(av<bv)return -1*sortDir; if(av>bv)return 1*sortDir; return 0; });
    const head='<thead><tr>'+cols.map(c=>{
      const sk=(c.k==='label')?'latency_ns':c.k;
      return '<th data-k="'+sk+'" class="'+(sk===sortKey?(sortDir>0?'sort-asc':'sort-desc'):'')+'">'+c.t+'</th>';
    }).join('')+'</tr></thead>';
    const body='<tbody>'+r.map(x=>'<tr>'+cols.map(c=>{
      const v=x[c.k]; return '<td>'+(c.fmt?c.fmt(v,x):esc(v))+'</td>'; }).join('')+'</tr>').join('')+'</tbody>';
    tbl.innerHTML=head+body;
    tbl.querySelectorAll('th').forEach(th=>th.addEventListener('click',()=>{
      const k=th.dataset.k; if(k===sortKey)sortDir*=-1; else {sortKey=k;sortDir=(k==='latency_ns'?1:-1);} render(); }));
  }
  render();
})();

// =====================================================================
// 4. Decay mini-chart: PnL bars (left axis) + Sharpe line (right axis) vs latency
// =====================================================================
class DecayChart{
  constructor(canvasId,tipId){
    this.canvas=document.getElementById(canvasId); this.tip=document.getElementById(tipId);
    this.items=[]; // [{label, pnl, sharpe}]
    this.pad={l:66,r:58,t:14,b:40};
    this._bind();
  }
  setItems(items){ this.items=items||[]; }
  _bind(){ const c=this.canvas;
    c.addEventListener('mousemove',e=>this._move(e));
    c.addEventListener('mouseleave',()=>{ this.tip.style.display='none'; }); }
  _plot(){ const w=this.canvas.clientWidth,h=this.canvas.clientHeight;
    return {x:this.pad.l,y:this.pad.t,w:w-this.pad.l-this.pad.r,h:h-this.pad.t-this.pad.b}; }
  _move(e){
    const r=this.canvas.getBoundingClientRect(); const px=e.clientX-r.left;
    const p=this._plot(); if(!this.items.length)return;
    const bw=p.w/this.items.length; let idx=Math.floor((px-p.x)/bw);
    if(idx<0||idx>=this.items.length){ this.tip.style.display='none'; return; }
    const it=this.items[idx];
    this.tip.innerHTML='<b>'+esc(it.label)+'</b><br>PnL: '+money(it.pnl)+'<br>Sharpe: '+num(it.sharpe,2);
    this.tip.style.left=(p.x+bw*(idx+0.5))+'px';
    this.tip.style.top=(this.pad.t+8)+'px'; this.tip.style.display='block';
  }
  draw(){
    const c=this.canvas,dpr=window.devicePixelRatio||1;
    const W=c.clientWidth,H=c.clientHeight; if(W===0)return;
    c.width=W*dpr;c.height=H*dpr; const ctx=c.getContext('2d');
    ctx.setTransform(dpr,0,0,dpr,0,0); ctx.clearRect(0,0,W,H);
    const p=this._plot(); const items=this.items; if(!items.length)return;
    // PnL axis (left)
    let pmn=0,pmx=0; for(const it of items){ pmn=Math.min(pmn,it.pnl); pmx=Math.max(pmx,it.pnl); }
    if(pmn===0&&pmx===0)pmx=1; const ppad=(pmx-pmn)*0.12||1; pmx+=ppad; if(pmn<0)pmn-=ppad;
    const PY=v=> p.y+(1-(v-pmn)/(pmx-pmn))*p.h;
    // Sharpe axis (right)
    let smn=Infinity,smx=-Infinity; for(const it of items){ smn=Math.min(smn,it.sharpe); smx=Math.max(smx,it.sharpe); }
    if(!isFinite(smn)){ smn=0;smx=1; } if(smn===smx){ smn-=1;smx+=1; }
    const spad=(smx-smn)*0.12; smn-=spad; smx+=spad;
    const SY=v=> p.y+(1-(v-smn)/(smx-smn))*p.h;
    // grid + left labels (PnL)
    ctx.strokeStyle='#222b38'; ctx.fillStyle='#8b949e'; ctx.font='11px sans-serif'; ctx.textBaseline='middle';
    for(let i=0;i<=5;i++){ const v=pmn+(pmx-pmn)*i/5; const yy=PY(v);
      ctx.beginPath();ctx.moveTo(p.x,yy);ctx.lineTo(p.x+p.w,yy);ctx.stroke();
      ctx.textAlign='right'; ctx.fillStyle='#8b949e'; ctx.fillText(money(v,0),p.x-6,yy); }
    // right labels (Sharpe)
    for(let i=0;i<=5;i++){ const v=smn+(smx-smn)*i/5; const yy=SY(v);
      ctx.textAlign='left'; ctx.fillStyle='#bc8cff'; ctx.fillText(num(v,1),p.x+p.w+6,yy); }
    // zero line for PnL
    const zeroY=PY(0); ctx.strokeStyle='#3a4456'; ctx.beginPath();ctx.moveTo(p.x,zeroY);ctx.lineTo(p.x+p.w,zeroY);ctx.stroke();
    // PnL bars
    const bw=p.w/items.length;
    for(let i=0;i<items.length;i++){ const it=items[i];
      const x=p.x+bw*i+bw*0.22, w=bw*0.56;
      const yv=PY(it.pnl); const top=Math.min(yv,zeroY), hh=Math.abs(yv-zeroY);
      ctx.fillStyle = it.pnl>=0?'#3fb95099':'#f8514999';
      ctx.fillRect(x,top,w,Math.max(1,hh));
      ctx.fillStyle='#8b949e'; ctx.font='10px sans-serif'; ctx.textAlign='center'; ctx.textBaseline='top';
      ctx.fillText(it.label, p.x+bw*i+bw*0.5, p.y+p.h+6);
    }
    // Sharpe line
    ctx.strokeStyle='#bc8cff'; ctx.lineWidth=2; ctx.beginPath();
    for(let i=0;i<items.length;i++){ const X=p.x+bw*i+bw*0.5, Yv=SY(items[i].sharpe);
      if(i===0)ctx.moveTo(X,Yv); else ctx.lineTo(X,Yv); }
    ctx.stroke();
    for(let i=0;i<items.length;i++){ const X=p.x+bw*i+bw*0.5, Yv=SY(items[i].sharpe);
      ctx.beginPath(); ctx.arc(X,Yv,3,0,7); ctx.fillStyle='#bc8cff'; ctx.fill(); }
    // axis titles
    ctx.fillStyle='#3fb950'; ctx.font='10px sans-serif'; ctx.textAlign='left'; ctx.textBaseline='top';
    ctx.fillText('PnL ('+CUR+')', p.x, 0);
    ctx.fillStyle='#bc8cff'; ctx.textAlign='right'; ctx.fillText('Sharpe', p.x+p.w, 0);
    ctx.strokeStyle='#2a3242'; ctx.lineWidth=1; ctx.strokeRect(p.x,p.y,p.w,p.h);
  }
}
const decay=new DecayChart('decay-canvas','decay-tip');
decay.setItems(RUNS.map(r=>({label:r.label, pnl:r.summary.pnl_total, sharpe:r.summary.sharpe})));

// ---------- redraw orchestration ----------
function redrawAll(){ eqChart.draw(); decay.draw(); }
window.addEventListener('resize',redrawAll);
redrawAll();
"""


# ===========================================================================
# Synthetic sweep generator (for dev / verification without the Rust binary)
# ===========================================================================
def generate_synthetic_sweep(root):
    """Write a realistic fake latency sweep under ``root``.

    Creates ``lat_0``, ``lat_100000000`` (100ms), ``lat_500000000`` (500ms) and
    ``lat_1000000000`` (1s) subdirs, each with ``report.json`` + ``equity.csv`` +
    ``meta.json``. PnL / Sharpe degrade as latency grows; the 1s run halts on an
    equity floor and flatlines after the halt point. Returns the list of latencies.

    NOTE: this is SYNTHETIC. Once the Rust crate is rebuilt, regenerate the real
    sweep by running ``kalshi-backtest backtest ... --latency-ns <L> --out-dir <dir>``
    for each latency and pointing this tool at those dirs instead.
    """
    os.makedirs(root, exist_ok=True)
    start_ns = 1_700_000_000 * NS_PER_S          # fixed start (UTC) for reproducibility
    n_points = 240                                # equity samples per run
    step_ns = 30 * NS_PER_S                       # 30s between samples
    start_balance = 10_000.0

    # latency -> (edge_per_step, vol, halted)
    latencies = [
        (0,             1.00, 1.0, False),
        (100 * NS_PER_MS, 0.62, 1.15, False),
        (500 * NS_PER_MS, 0.28, 1.45, False),
        (1 * NS_PER_S,  -0.35, 1.9,  True),
    ]

    def synth_curve(edge_mult, vol_mult, halted):
        """Deterministic pseudo-random equity path (no numpy)."""
        import random
        rng = random.Random(0xC0FFEE + int(edge_mult * 1000) + int(vol_mult * 100))
        eq = start_balance
        base_drift = 3.2 * edge_mult       # $ per step at zero latency
        base_vol = 14.0 * vol_mult
        ts_list, total_list, dd_list = [], [], []
        peak = eq
        halt_idx = int(n_points * 0.55) if halted else None
        for i in range(n_points):
            if halt_idx is not None and i >= halt_idx:
                # flatline after halt: equity frozen at the halt level.
                pass
            else:
                shock = rng.gauss(0, 1) * base_vol
                drift = base_drift + 0.0008 * i * edge_mult
                eq += drift + shock
            ts = start_ns + i * step_ns
            peak = max(peak, eq)
            ts_list.append(ts)
            total_list.append(round(eq, 2))
            dd_list.append(round(eq - peak, 2))
        return ts_list, total_list, dd_list, halt_idx

    def derive_summary(total_list, halted, halt_idx, ts_list):
        rets = []
        for i in range(1, len(total_list)):
            prev = total_list[i - 1]
            if prev != 0:
                rets.append((total_list[i] - prev) / abs(prev))
        mean = sum(rets) / len(rets) if rets else 0.0
        var = sum((r - mean) ** 2 for r in rets) / len(rets) if rets else 0.0
        std = math.sqrt(var)
        sharpe = (mean / std * math.sqrt(252)) if std > 0 else 0.0
        peak = total_list[0]
        max_dd = 0.0
        for v in total_list:
            peak = max(peak, v)
            if peak > 0:
                max_dd = min(max_dd, (v - peak) / peak)
        pnl_total = round(total_list[-1] - start_balance, 2)
        wins = sum(1 for r in rets if r > 0)
        win_rate = wins / len(rets) if rets else 0.0
        num_fills = 1200 - 90 * 0  # scaled below
        return {
            "sharpe": round(sharpe, 4),
            "pnl_total": pnl_total,
            "pnl_pct": round(pnl_total / start_balance, 6),
            "max_drawdown_pct": round(max_dd, 6),
            "win_rate": round(win_rate, 4),
        }

    written = []
    for idx, (lat_ns, edge, vol, halted) in enumerate(latencies):
        d = os.path.join(root, "lat_%d" % lat_ns)
        os.makedirs(d, exist_ok=True)
        ts_list, total_list, dd_list, halt_idx = synth_curve(edge, vol, halted)
        summ = derive_summary(total_list, halted, halt_idx, ts_list)
        # fills / fees / rejections scale with latency (fewer good fills, more rejects).
        num_fills = max(120, int(1180 - idx * 230))
        total_fees = round(num_fills * 0.013, 2)
        risk_rejections = idx * 7 + (38 if halted else 0)
        halt_unix_ns = ts_list[halt_idx] if (halted and halt_idx is not None) else None

        summary = {
            "currency": "USD",
            "starting_balance": start_balance,
            "ending_balance": total_list[-1],
            "pnl_total": summ["pnl_total"],
            "pnl_pct": summ["pnl_pct"],
            "sharpe": summ["sharpe"],
            "max_drawdown_pct": summ["max_drawdown_pct"],
            "win_rate": summ["win_rate"],
            "num_fills": num_fills,
            "total_fees": total_fees,
            "latency_ns": lat_ns,
            "halted": halted,
            "halt_reason": "equity_floor" if halted else "",
            "risk_rejections": risk_rejections,
        }
        if halt_unix_ns is not None:
            summary["halt_unix_ns"] = halt_unix_ns

        report = {
            "plugin_name": "imbalance",
            "summary": summary,
        }
        with open(os.path.join(d, REPORT_JSON), "w") as f:
            json.dump(report, f, indent=2)

        with open(os.path.join(d, EQUITY_CSV), "w", newline="") as f:
            w = csv.writer(f)
            w.writerow(["ts_ns", "total", "currency", "drawdown"])
            for t, tot, dd in zip(ts_list, total_list, dd_list):
                w.writerow([t, tot, "USD", dd])

        meta = {
            "strategy": "imbalance",
            "instrument_filter": "KXNATGASD-%",
            "currency": "USD",
            "starting_balance": start_balance,
            "latency_ns": lat_ns,
            "source": "ndjson",
            "synthetic": True,
            "generated_unix_ns": start_ns,
        }
        with open(os.path.join(d, META_JSON), "w") as f:
            json.dump(meta, f, indent=2)

        written.append(lat_ns)
        print("  wrote %s (latency=%s, pnl=%s, halted=%s)"
              % (d, humanize_latency(lat_ns), summ["pnl_total"], halted))
    return written


# ===========================================================================
# Run assembly
# ===========================================================================
def build_runs(args):
    """Resolve CLI args into a list of loaded :class:`SweepRun`."""
    specs = []  # (label, dir, latency_ns)
    if args.runs:
        for spec in args.runs:
            label, directory, latency_ns = parse_run_spec(spec)
            specs.append((label, directory, latency_ns if latency_ns is not None else 0))
    if args.sweep_dir:
        specs.extend(discover_sweep_dir(args.sweep_dir))

    runs = []
    for label, directory, latency_ns in specs:
        if not os.path.isdir(directory):
            print("warning: run dir not found, skipping: %s" % directory, file=sys.stderr)
            continue
        runs.append(SweepRun.load(label, latency_ns, directory))
    return runs


# ===========================================================================
# CLI
# ===========================================================================
def main(argv=None):
    ap = argparse.ArgumentParser(
        description="Generate a self-contained interactive LATENCY-SWEEP comparison page.")
    ap.add_argument("--runs", action="append", default=[], metavar="SPEC",
                    help="a run spec 'label=0ns,dir=../data/exports/sweep_0' (repeatable)")
    ap.add_argument("--sweep-dir", default=None,
                    help="auto-discover lat_<ns> subdirs under this dir")
    ap.add_argument("--out", default="latency_sweep.html", help="output HTML path")
    ap.add_argument("--make-sample", action="store_true",
                    help="generate the synthetic sweep under ../data/exports/sweep_sample "
                         "and build dashboard/sample/latency_sweep.html from it")
    args = ap.parse_args(argv)

    here = os.path.dirname(os.path.abspath(__file__))

    if args.make_sample:
        sample_src = os.path.normpath(os.path.join(here, "..", "data", "exports", "sweep_sample"))
        print("Generating synthetic sweep under %s ..." % sample_src)
        generate_synthetic_sweep(sample_src)
        if not args.sweep_dir:
            args.sweep_dir = sample_src
        if args.out == "latency_sweep.html":
            args.out = os.path.join(here, "sample", "latency_sweep.html")

    if not args.runs and not args.sweep_dir:
        ap.error("provide --runs and/or --sweep-dir (or use --make-sample)")

    runs = build_runs(args)
    if not runs:
        print("error: no runs loaded", file=sys.stderr)
        return 2

    out = LatencySweepRenderer(runs).write(args.out)
    size = os.path.getsize(out)
    print("Wrote %s (%.1f KB) from %d run(s)" % (out, size / 1024.0, len(runs)))
    for r in runs:
        print("  %-8s latency=%-7s pnl=%-10s sharpe=%-7s halted=%s%s"
              % (r.label, humanize_latency(r.latency_ns),
                 r.summary.get("pnl_total"), r.summary.get("sharpe"),
                 r.is_halted(),
                 ("  halt@%s" % r.halt_ns) if r.halt_ns else ""))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
