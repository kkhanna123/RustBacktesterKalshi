#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""ui/server.py -- interactive control-panel for the Kalshi tick-level backtester.

A self-contained, zero-install local web app that lets you CONFIGURE + LAUNCH
backtests, WATCH live progress (with a self-improving ETA), and BROWSE / COMPARE
cached runs -- all by *driving* the already-built Rust binary and *reusing* the
existing chart generators (``dashboard/build_dashboard.py`` and
``dashboard/build_latency_sweep.py``) by read-only subprocess.

Design constraints (hard):
    * Python 3.8 standard library ONLY (http.server, threading, subprocess,
      json, urllib).  No pip installs, no third-party packages.
    * Vanilla JS / HTML / CSS, fully offline (no CDN).
    * Touches NOTHING outside this ``ui/`` directory.  The Rust binary and the
      dashboard scripts are invoked read-only; their source is never modified.

Architecture:
    Paths            -- resolves repo-relative paths (binary, data, dashboards).
    Calibration      -- persisted events/sec rate (EMA) -> honest ETAs.
    RunManager       -- single-run queue; spawns the binary in a background
                        thread, tails ``run.log`` for progress, builds the
                        per-run dashboard, writes ``meta.json``.
    Handler          -- the http.server request handler with the JSON API +
                        static serving of cached dashboards.
    APP_HTML         -- the single-page app (dark theme), served at ``/``.

Run it:
    python ui/server.py            # picks a free port, prints the URL

Cached runs live in ``ui/runs/<run_id>/``.  Calibration in ``ui/calibration.json``.
"""
from __future__ import annotations

import json
import os
import re
import shutil
import socket
import subprocess
import sys
import threading
import time
import urllib.parse
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# ===========================================================================
# Paths -- everything resolved relative to the repo root (the parent of ui/).
# ===========================================================================
UI_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(UI_DIR)
RUNS_DIR = os.path.join(UI_DIR, "runs")
CALIB_PATH = os.path.join(UI_DIR, "calibration.json")

# The release binary built by `cd backtester && cargo build --release`.
# On Windows the artifact is `kalshi-backtest.exe`; CreateProcess won't auto-append the extension
# when handed a full path, so add it ourselves based on the OS.
_EXE = ".exe" if os.name == "nt" else ""
BINARY = os.path.join(REPO_ROOT, "backtester", "target", "release", "kalshi-backtest" + _EXE)
# The clickhouse-feature build (optional) lives at a sibling target path; we
# only *detect* it (default source stays ndjson).
BINARY_CH = os.path.join(REPO_ROOT, "backtester", "target", "release", "kalshi-backtest" + _EXE)

# Prefer the repo venv's interpreter (it has pandas for the chart generators). The venv layout
# differs by OS (Scripts\python.exe on Windows, bin/python elsewhere); fall back to the running one.
if os.name == "nt":
    PYTHON = os.path.join(REPO_ROOT, ".venv", "Scripts", "python.exe")
else:
    PYTHON = os.path.join(REPO_ROOT, ".venv", "bin", "python")
if not os.path.isfile(PYTHON):
    PYTHON = sys.executable

BUILD_DASHBOARD = os.path.join(REPO_ROOT, "dashboard", "build_dashboard.py")
BUILD_SWEEP = os.path.join(REPO_ROOT, "dashboard", "build_latency_sweep.py")

DATA_DIR = os.path.join(REPO_ROOT, "data")

os.makedirs(RUNS_DIR, exist_ok=True)

# Default calibration rate (events/sec) used before any real run calibrates it.
SEED_RATE = 270000.0


# ===========================================================================
# Calibration -- a persisted events/sec rate, updated by EMA after each run so
# the ETA self-improves.  estimate_secs = events / rate.
# ===========================================================================
class Calibration:
    """Persisted events/sec processing rate with exponential-moving-average update."""

    ALPHA = 0.3  # weight of the newest observation in the EMA

    def __init__(self, path):
        self.path = path
        self.rate = SEED_RATE
        self.samples = 0
        self._load()

    def _load(self):
        try:
            with open(self.path) as f:
                d = json.load(f)
            self.rate = float(d.get("rate", SEED_RATE)) or SEED_RATE
            self.samples = int(d.get("samples", 0))
        except Exception:
            self.rate = SEED_RATE
            self.samples = 0

    def save(self):
        try:
            with open(self.path, "w") as f:
                json.dump({"rate": self.rate, "samples": self.samples,
                           "updated": _now_iso()}, f, indent=2)
        except Exception:
            pass

    def update(self, events, duration_secs):
        """Fold one finished run's measured rate into the EMA."""
        if events <= 0 or duration_secs <= 0:
            return
        obs = events / duration_secs
        if self.samples == 0:
            self.rate = obs
        else:
            self.rate = self.ALPHA * obs + (1 - self.ALPHA) * self.rate
        self.samples += 1
        self.save()

    def estimate_secs(self, events):
        if events <= 0 or self.rate <= 0:
            return None
        return events / self.rate


# ===========================================================================
# Small helpers
# ===========================================================================
def _now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _slug(s):
    s = re.sub(r"[^a-zA-Z0-9]+", "-", (s or "").strip().lower()).strip("-")
    return s[:24] or "run"


def _free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def _list_data_files():
    """Scan for ndjson tick captures the form can offer as a dropdown.

    Looks in ``data/tick/*.ndjson*`` and ``data/raw/**/*.ndjson*``.  Returns
    repo-relative paths (what the binary expects from cwd=REPO_ROOT).
    """
    files = []
    tick = os.path.join(DATA_DIR, "tick")
    if os.path.isdir(tick):
        for n in sorted(os.listdir(tick)):
            if ".ndjson" in n:
                files.append(os.path.relpath(os.path.join(tick, n), REPO_ROOT))
    raw = os.path.join(DATA_DIR, "raw")
    if os.path.isdir(raw):
        for root, _dirs, names in os.walk(raw):
            for n in sorted(names):
                if ".ndjson" in n:
                    files.append(os.path.relpath(os.path.join(root, n), REPO_ROOT))
    return files


# Strategy descriptions parsed once at import; params parsed live in /api/meta.
STRATEGY_LINE = re.compile(r"^\s{2}(\w+)\s{2,}(.+)$")
PARAMS_LINE = re.compile(r"^\s+params:\s*(.+)$")


def parse_strategies():
    """Run ``kalshi-backtest list-strategies`` and parse each strategy's
    description + tunable params (key=default).

    Returns list of dicts: {name, desc, params:[{key, default}]}.
    """
    out = ""
    try:
        out = subprocess.run([BINARY, "list-strategies"], capture_output=True,
                             text=True, timeout=30, cwd=REPO_ROOT).stdout
    except Exception:
        return []
    strategies = []
    cur = None
    for line in out.splitlines():
        m = STRATEGY_LINE.match(line)
        if m and "params:" not in line:
            cur = {"name": m.group(1), "desc": m.group(2).strip(), "params": []}
            strategies.append(cur)
            continue
        p = PARAMS_LINE.match(line)
        if p and cur is not None:
            body = p.group(1).strip()
            if body and body != "(none)":
                for tok in body.split(","):
                    tok = tok.strip()
                    if "=" in tok:
                        k, v = tok.split("=", 1)
                        cur["params"].append({"key": k.strip(), "default": v.strip()})
    return strategies


ADAPTER_LINE = re.compile(r"^\s{2}(\w+)\s+\[([A-Z]+)\]\s+(.+)$")


def parse_adapters():
    """Run ``kalshi-backtest list-adapters`` (fallback to the cached help) and
    return [{key, venue, desc}]."""
    out = ""
    for cmd in (["list-adapters"], None):
        if cmd is None:
            break
        try:
            r = subprocess.run([BINARY] + cmd, capture_output=True, text=True,
                               timeout=30, cwd=REPO_ROOT)
            if r.returncode == 0 and r.stdout.strip():
                out = r.stdout
                break
        except Exception:
            pass
    adapters = []
    for line in out.splitlines():
        m = ADAPTER_LINE.match(line)
        if m:
            adapters.append({"key": m.group(1), "venue": m.group(2),
                             "desc": m.group(3).strip()})
    if not adapters:
        # Hard fallback to the known adapter set if the binary lacks the subcmd.
        adapters = [
            {"key": "kalshi_ndjson", "venue": "KALSHI", "desc": "Kalshi tick NDJSON capture"},
            {"key": "generic_ndjson", "venue": "GENERIC", "desc": "Any venue tick NDJSON"},
            {"key": "generic_csv", "venue": "GENERIC", "desc": "Any venue tick CSV"},
            {"key": "polymarket", "venue": "POLYMARKET", "desc": "Polymarket CLOB canonical NDJSON"},
            {"key": "hyperliquid", "venue": "HYPERLIQUID", "desc": "Hyperliquid perps L2 NDJSON"},
        ]
    return adapters


# The execution-realism / risk toggle surface, with defaults.  Each entry maps a
# config key -> the binary flag.  `kind`: bool|str|num.  When a value equals its
# default (or is empty), the flag is OMITTED so runs stay clean.
TOGGLES = [
    # --- fees / rewards ---
    {"key": "no_fees", "flag": "--no-fees", "kind": "bool", "default": False,
     "group": "Execution realism", "label": "Exclude fees from PnL"},
    {"key": "rewards", "flag": "--rewards", "kind": "bool", "default": False,
     "group": "Execution realism", "label": "Enable Kalshi liquidity rewards"},
    {"key": "reward_per_period", "flag": "--reward-per-period", "kind": "num", "default": "",
     "group": "Execution realism", "label": "Reward pool / period ($)"},
    {"key": "reward_period_secs", "flag": "--reward-period-secs", "kind": "num", "default": "",
     "group": "Execution realism", "label": "Reward period (secs)"},
    {"key": "min_resting_size", "flag": "--min-resting-size", "kind": "num", "default": "",
     "group": "Execution realism", "label": "Min resting size (contracts)"},
    {"key": "max_spread_cents", "flag": "--max-spread-cents", "kind": "num", "default": "",
     "group": "Execution realism", "label": "Reward max spread (cents)"},
    # --- latency ---
    {"key": "latency_ns", "flag": "--latency-ns", "kind": "num", "default": "",
     "group": "Latency", "label": "Order latency (ns) / mean"},
    {"key": "latency_dist", "flag": "--latency-dist", "kind": "str", "default": "",
     "group": "Latency", "label": "Latency distribution",
     "choices": ["", "fixed", "uniform", "normal", "exponential"]},
    {"key": "latency_min_ns", "flag": "--latency-min-ns", "kind": "num", "default": "",
     "group": "Latency", "label": "Latency min (ns) [uniform]"},
    {"key": "latency_max_ns", "flag": "--latency-max-ns", "kind": "num", "default": "",
     "group": "Latency", "label": "Latency max (ns) [uniform]"},
    {"key": "latency_std_ns", "flag": "--latency-std-ns", "kind": "num", "default": "",
     "group": "Latency", "label": "Latency std (ns) [normal]"},
    {"key": "latency_mean_ns", "flag": "--latency-mean-ns", "kind": "num", "default": "",
     "group": "Latency", "label": "Latency mean (ns) [exponential]"},
    {"key": "latency_seed", "flag": "--latency-seed", "kind": "num", "default": "",
     "group": "Latency", "label": "Latency PRNG seed"},
    # --- slippage / queue / settlement ---
    {"key": "slippage_ticks", "flag": "--slippage-ticks", "kind": "num", "default": "",
     "group": "Execution realism", "label": "Slippage (ticks/cents)"},
    {"key": "slippage_bps", "flag": "--slippage-bps", "kind": "num", "default": "",
     "group": "Execution realism", "label": "Slippage (bps fraction)"},
    {"key": "queue_model", "flag": "--queue-model", "kind": "str", "default": "",
     "group": "Execution realism", "label": "Queue model",
     "choices": ["", "pessimistic", "optimistic"]},
    {"key": "settlements", "flag": "--settlements", "kind": "str", "default": "",
     "group": "Execution realism", "label": "Settlements file (path)"},
    # --- risk ---
    {"key": "max_order_qty", "flag": "--max-order-qty", "kind": "num", "default": "",
     "group": "Risk", "label": "Max order qty"},
    {"key": "max_position", "flag": "--max-position", "kind": "num", "default": "",
     "group": "Risk", "label": "Max |position| per instrument"},
    {"key": "max_gross", "flag": "--max-gross", "kind": "num", "default": "",
     "group": "Risk", "label": "Max gross exposure"},
    {"key": "equity_floor", "flag": "--equity-floor", "kind": "num", "default": "",
     "group": "Risk", "label": "Equity floor (halt)"},
    {"key": "max_drawdown_pct", "flag": "--max-drawdown-pct", "kind": "num", "default": "",
     "group": "Risk", "label": "Max drawdown % (halt)"},
]


# ===========================================================================
# RunManager -- owns the single-run queue, the background worker, progress.
# ===========================================================================
class RunManager:
    """Drives ONE backtest at a time.

    A POST /api/run validates + persists ``config.json`` then launches the
    binary in a background thread.  Only one run is active at once (concurrent
    requests are rejected with a clear message) so the ETA stays honest.

    Progress is derived by tailing ``run.log``:
      * total events = the ``loaded N events`` line the binary prints at start
        (or, before that line appears, a pre-fetched ``describe-data`` count).
      * estimate     = total_events / calibration.rate.
      * pct          = min(99, elapsed / estimate * 100) while running; 100 done.
    """

    def __init__(self, calib):
        self.calib = calib
        self.lock = threading.Lock()
        self.active = None      # the run_id currently running, or None
        self.proc = None        # the live subprocess.Popen, or None
        # In-memory progress state for the active run.
        self.state = {}         # run_id -> dict(status, events, start_ts, ...)

    # ---- launch ----------------------------------------------------------
    def submit(self, config):
        """Validate + start a run.  Returns (run_id, error_or_None)."""
        with self.lock:
            if self.active is not None:
                return None, ("A backtest is already running (%s). "
                              "Wait for it to finish or cancel it." % self.active)
            err = self._validate(config)
            if err:
                return None, err
            run_id = self._make_run_id(config)
            run_dir = os.path.join(RUNS_DIR, run_id)
            os.makedirs(run_dir, exist_ok=True)
            with open(os.path.join(run_dir, "config.json"), "w") as f:
                json.dump(config, f, indent=2)
            self.active = run_id
            self.state[run_id] = {
                "status": "queued", "events": 0, "start_ts": time.time(),
                "message": "starting...", "pct": 0,
            }
        t = threading.Thread(target=self._worker, args=(run_id, run_dir, config),
                             daemon=True)
        t.start()
        return run_id, None

    def _validate(self, c):
        if not os.path.isfile(BINARY):
            return ("Binary not found at %s -- build it with "
                    "`cd backtester && cargo build --release`." % BINARY)
        if not c.get("strategy"):
            return "No strategy selected."
        src = c.get("source")
        if src == "ndjson":
            p = c.get("ndjson")
            if not p:
                return "source=ndjson requires an ndjson path."
            if not os.path.isfile(os.path.join(REPO_ROOT, p)) and not os.path.isfile(p):
                return "ndjson file not found: %s" % p
        elif src == "clickhouse":
            if not c.get("clickhouse"):
                return "source=clickhouse requires a base URL."
        elif src == "adapter":
            if not c.get("adapter") or not c.get("adapter_path"):
                return "source=adapter requires adapter + adapter_path."
        else:
            return "Unknown source: %r" % src
        return None

    def _make_run_id(self, c):
        ts = datetime.now().strftime("%Y%m%d-%H%M%S")
        label = c.get("label") or c.get("strategy") or "run"
        return "%s_%s" % (ts, _slug(label))

    # ---- flag mapping ----------------------------------------------------
    def build_cmd(self, config, run_dir):
        """Map a CONFIG object to the binary's `backtest` argv.

        Flags are OMITTED whenever the value is empty / left at default so the
        spawned command stays minimal and runs stay clean.
        """
        cmd = [BINARY, "backtest", "--out-dir", run_dir]
        src = config.get("source")
        cmd += ["--source", src]
        if src == "ndjson":
            cmd += ["--ndjson", config["ndjson"]]
        elif src == "clickhouse":
            cmd += ["--clickhouse", config["clickhouse"]]
        elif src == "adapter":
            cmd += ["--adapter", config["adapter"], "--adapter-path", config["adapter_path"]]
            if config.get("venue"):
                cmd += ["--venue", config["venue"]]
            if config.get("adapter_profile"):
                cmd += ["--adapter-profile", config["adapter_profile"]]

        if config.get("instrument"):
            cmd += ["--instrument", config["instrument"]]
        if config.get("start"):
            cmd += ["--start", config["start"]]
        if config.get("end"):
            cmd += ["--end", config["end"]]

        cmd += ["--strategy", config["strategy"]]
        for k, v in (config.get("strategy_params") or {}).items():
            if str(v).strip() != "":
                cmd += ["--strategy-param", "%s=%s" % (k, v)]

        if str(config.get("starting_balance", "")).strip() != "":
            cmd += ["--starting-balance", str(config["starting_balance"])]

        # Toggles (bool flags + valued flags), omitting empties/defaults.
        tg = config.get("toggles") or {}
        for spec in TOGGLES:
            key, flag, kind = spec["key"], spec["flag"], spec["kind"]
            if key not in tg:
                continue
            val = tg[key]
            if kind == "bool":
                if bool(val) is True and bool(spec["default"]) is False:
                    cmd += [flag]
            else:
                if val is None:
                    continue
                sval = str(val).strip()
                if sval == "" or sval == str(spec["default"]):
                    continue
                cmd += [flag, sval]

        # Free-form escape hatch.
        extra = (config.get("extra_flags") or "").strip()
        if extra:
            cmd += extra.split()
        return cmd

    # ---- background worker ----------------------------------------------
    def _worker(self, run_id, run_dir, config):
        log_path = os.path.join(run_dir, "run.log")
        st = self.state[run_id]
        st["status"] = "running"
        st["message"] = "loading events..."

        # Pre-fetch a total event count for the ndjson source so the ETA can
        # show before the binary prints its own "loaded N events" line.
        if config.get("source") == "ndjson":
            n = self._prefetch_count(config)
            if n:
                st["events"] = n

        cmd = self.build_cmd(config, run_dir)
        st["cmd"] = " ".join(cmd)
        rc = 1
        try:
            with open(log_path, "w") as logf:
                logf.write("$ %s\n" % " ".join(cmd))
                logf.flush()
                proc = subprocess.Popen(
                    cmd, cwd=REPO_ROOT, stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT, text=True, bufsize=1)
                with self.lock:
                    self.proc = proc
                # Stream output -> log, scraping the "loaded N events" line.
                for line in proc.stdout:
                    logf.write(line)
                    logf.flush()
                    m = re.search(r"loaded\s+([\d,]+)\s+events", line)
                    if m:
                        st["events"] = int(m.group(1).replace(",", ""))
                        st["message"] = "running strategy..."
                rc = proc.wait()
        except Exception as e:  # pragma: no cover
            st["status"] = "error"
            st["message"] = "launch failed: %s" % e
            self._finish(run_id)
            return

        duration = time.time() - st["start_ts"]
        with self.lock:
            self.proc = None

        if rc != 0:
            st["status"] = "error"
            st["message"] = self._log_tail(log_path)
            self._write_meta(run_id, run_dir, config, status="error",
                             duration=duration)
            self._finish(run_id)
            return

        # Success: update calibration, build the per-run dashboard, write meta.
        if st.get("events"):
            self.calib.update(st["events"], duration)
        st["message"] = "building dashboard..."
        self._build_dashboard(run_dir, log_path)
        self._write_meta(run_id, run_dir, config, status="done", duration=duration)
        st["status"] = "done"
        st["pct"] = 100
        st["message"] = "done"
        self._finish(run_id)

    def _finish(self, run_id):
        with self.lock:
            if self.active == run_id:
                self.active = None

    def _prefetch_count(self, config):
        try:
            r = subprocess.run(
                [BINARY, "describe-data", "--source", "ndjson",
                 "--ndjson", config["ndjson"]],
                cwd=REPO_ROOT, capture_output=True, text=True, timeout=120)
            m = re.search(r"total events\s*:\s*([\d,]+)", r.stdout)
            if m:
                return int(m.group(1).replace(",", ""))
        except Exception:
            pass
        return None

    def _build_dashboard(self, run_dir, log_path):
        out = os.path.join(run_dir, "dashboard.html")
        try:
            with open(log_path, "a") as logf:
                logf.write("\n$ build_dashboard.py\n")
                subprocess.run(
                    [PYTHON, BUILD_DASHBOARD, "--export-dir", run_dir, "--out", out],
                    cwd=REPO_ROOT, stdout=logf, stderr=subprocess.STDOUT, timeout=300)
        except Exception:
            pass

    def _write_meta(self, run_id, run_dir, config, status, duration):
        """Write the run's summary meta.json used by the Runs / Compare views."""
        report = {}
        try:
            with open(os.path.join(run_dir, "report.json")) as f:
                report = json.load(f)
        except Exception:
            pass
        summary = report.get("summary", {}) or {}
        st = self.state.get(run_id, {})
        keys = ["pnl_total", "pnl_pct", "sharpe", "sortino", "max_drawdown_pct",
                "num_fills", "num_trades", "win_rate", "ending_balance",
                "total_fees", "starting_balance"]
        metrics = {k: summary.get(k) for k in keys if k in summary}
        meta = {
            "id": run_id,
            "label": config.get("label") or config.get("strategy"),
            "timestamp": _now_iso(),
            "source": config.get("source"),
            "instrument": config.get("instrument") or "(all)",
            "strategy": config.get("strategy"),
            "metrics": metrics,
            "total_events": st.get("events", 0),
            "duration_secs": round(duration, 2),
            "status": status,
        }
        with open(os.path.join(run_dir, "meta.json"), "w") as f:
            json.dump(meta, f, indent=2)

    def _log_tail(self, log_path, n=40):
        try:
            with open(log_path) as f:
                lines = f.readlines()
            return "".join(lines[-n:]).strip() or "run failed (no output)"
        except Exception:
            return "run failed (no log)"

    # ---- progress --------------------------------------------------------
    def progress(self, run_id):
        st = self.state.get(run_id)
        if st is None:
            # Not in memory -> read its final meta (cached run / server restart).
            meta = _read_meta(run_id)
            if meta:
                return {"status": meta.get("status", "done"), "pct": 100,
                        "elapsed_secs": meta.get("duration_secs", 0),
                        "eta_secs": 0, "events": meta.get("total_events", 0),
                        "message": meta.get("status", "done")}
            return {"status": "error", "pct": 0, "elapsed_secs": 0,
                    "eta_secs": 0, "events": 0, "message": "unknown run"}
        elapsed = time.time() - st["start_ts"]
        events = st.get("events", 0)
        est = self.calib.estimate_secs(events) if events else None
        pct = st.get("pct", 0)
        eta = 0
        if st["status"] == "running":
            if est and est > 0:
                pct = min(99, int(elapsed / est * 100))
                eta = max(0, int(est - elapsed))
            else:
                pct = min(95, int(elapsed * 2))  # crude until we know events
        elif st["status"] == "done":
            pct = 100
        return {"status": st["status"], "pct": pct,
                "elapsed_secs": round(elapsed, 1), "eta_secs": eta,
                "events": events, "message": st.get("message", "")}

    def cancel(self, run_id):
        with self.lock:
            if self.active == run_id and self.proc is not None:
                try:
                    self.proc.kill()
                except Exception:
                    pass
                if run_id in self.state:
                    self.state[run_id]["status"] = "error"
                    self.state[run_id]["message"] = "cancelled by user"
                return True
        return False


# ===========================================================================
# Cached-run helpers (read meta/config/report off disk).
# ===========================================================================
def _read_meta(run_id):
    p = os.path.join(RUNS_DIR, run_id, "meta.json")
    try:
        with open(p) as f:
            return json.load(f)
    except Exception:
        return None


def list_runs():
    runs = []
    for name in os.listdir(RUNS_DIR):
        meta = _read_meta(name)
        if meta:
            runs.append(meta)
    runs.sort(key=lambda m: m.get("id", ""), reverse=True)
    return runs


def compare_runs(a, b):
    """Ensure a latency-sweep overlay HTML exists for runs a,b; return url + the
    two summaries for a side-by-side diff."""
    dir_a = os.path.join(RUNS_DIR, a)
    dir_b = os.path.join(RUNS_DIR, b)
    if not (os.path.isdir(dir_a) and os.path.isdir(dir_b)):
        return None, "one or both runs not found"
    out_name = "compare_%s__%s.html" % (a, b)
    out_path = os.path.join(RUNS_DIR, out_name)
    if not os.path.isfile(out_path):
        try:
            subprocess.run(
                [PYTHON, BUILD_SWEEP,
                 "--runs", "label=%s,dir=%s" % (a, dir_a),
                 "--runs", "label=%s,dir=%s" % (b, dir_b),
                 "--out", out_path],
                cwd=REPO_ROOT, capture_output=True, text=True, timeout=300)
        except Exception as e:
            return None, "compare build failed: %s" % e
    if not os.path.isfile(out_path):
        return None, "compare overlay was not generated"
    ma, mb = _read_meta(a) or {}, _read_meta(b) or {}
    return {"url": "/runs/" + out_name, "a": ma, "b": mb}, None


def delete_run(run_id):
    d = os.path.join(RUNS_DIR, run_id)
    if not os.path.isdir(d) or os.path.dirname(os.path.abspath(d)) != RUNS_DIR:
        return False
    shutil.rmtree(d, ignore_errors=True)
    # Drop any compare overlays that referenced this run.
    for n in os.listdir(RUNS_DIR):
        if n.startswith("compare_") and run_id in n:
            try:
                os.remove(os.path.join(RUNS_DIR, n))
            except Exception:
                pass
    return True


# ===========================================================================
# HTTP handler
# ===========================================================================
CALIB = Calibration(CALIB_PATH)
MANAGER = RunManager(CALIB)


class Handler(BaseHTTPRequestHandler):
    """JSON API + static serving of cached per-run dashboards & compare overlays."""

    server_version = "BacktestUI/1.0"

    def log_message(self, fmt, *args):  # quieter console
        sys.stderr.write("[ui] %s\n" % (fmt % args))

    # ---- response helpers ----
    def _json(self, obj, code=200):
        body = json.dumps(obj).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _html(self, text, code=200):
        body = text.encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _query(self):
        return urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)

    def _body(self):
        length = int(self.headers.get("Content-Length", 0) or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            return json.loads(raw.decode("utf-8") or "{}")
        except Exception:
            return {}

    # ---- routing ----
    def do_GET(self):
        path = urllib.parse.urlparse(self.path).path
        if path == "/":
            return self._html(APP_HTML)
        if path == "/api/meta":
            return self._api_meta()
        if path == "/api/runs":
            return self._json({"runs": list_runs()})
        if path == "/api/run":
            return self._api_run_detail()
        if path == "/api/progress":
            return self._api_progress()
        if path == "/api/compare":
            return self._api_compare()
        if path.startswith("/runs/"):
            return self._serve_run_asset(path)
        return self._json({"error": "not found"}, 404)

    def do_POST(self):
        path = urllib.parse.urlparse(self.path).path
        if path == "/api/run":
            return self._api_run_start()
        if path == "/api/delete":
            run_id = (self._query().get("id", [""])[0])
            ok = delete_run(run_id)
            return self._json({"ok": ok})
        if path == "/api/cancel":
            run_id = (self._query().get("run_id", [""])[0])
            return self._json({"ok": MANAGER.cancel(run_id)})
        return self._json({"error": "not found"}, 404)

    # ---- endpoints ----
    def _api_meta(self):
        """Everything the form needs: strategies+params, adapters, data files,
        the toggle surface w/ defaults, clickhouse availability, calibration."""
        meta = {
            "strategies": parse_strategies(),
            "adapters": parse_adapters(),
            "data_files": _list_data_files(),
            "toggles": TOGGLES,
            "clickhouse_available": os.path.isfile(BINARY_CH),
            "binary_ok": os.path.isfile(BINARY),
            "calibration": {"rate": CALIB.rate, "samples": CALIB.samples},
            "active_run": MANAGER.active,
            "sources": ["ndjson", "clickhouse", "adapter"],
        }
        return self._json(meta)

    def _api_run_start(self):
        config = self._body()
        run_id, err = MANAGER.submit(config)
        if err:
            return self._json({"error": err}, 409)
        return self._json({"run_id": run_id})

    def _api_progress(self):
        run_id = self._query().get("run_id", [""])[0]
        if not run_id:
            return self._json({"error": "run_id required"}, 400)
        return self._json(MANAGER.progress(run_id))

    def _api_run_detail(self):
        run_id = self._query().get("id", [""])[0]
        d = os.path.join(RUNS_DIR, run_id)
        meta = _read_meta(run_id)
        if not meta:
            return self._json({"error": "run not found"}, 404)
        config, report = {}, {}
        try:
            with open(os.path.join(d, "config.json")) as f:
                config = json.load(f)
        except Exception:
            pass
        try:
            with open(os.path.join(d, "report.json")) as f:
                report = json.load(f)
        except Exception:
            pass
        return self._json({"meta": meta, "config": config,
                           "summary": report.get("summary", {}),
                           "has_dashboard": os.path.isfile(os.path.join(d, "dashboard.html"))})

    def _api_compare(self):
        q = self._query()
        a, b = q.get("a", [""])[0], q.get("b", [""])[0]
        if not a or not b or a == b:
            return self._json({"error": "pick two distinct runs"}, 400)
        res, err = compare_runs(a, b)
        if err:
            return self._json({"error": err}, 400)
        return self._json(res)

    def _serve_run_asset(self, path):
        """Serve a cached file under ui/runs/ (dashboard.html, export csvs,
        compare overlays).  Path-traversal-safe."""
        rel = urllib.parse.unquote(path[len("/runs/"):])
        full = os.path.normpath(os.path.join(RUNS_DIR, rel))
        if not full.startswith(RUNS_DIR + os.sep) and full != RUNS_DIR:
            return self._json({"error": "forbidden"}, 403)
        if not os.path.isfile(full):
            return self._json({"error": "not found"}, 404)
        ctype = "text/html; charset=utf-8" if full.endswith(".html") else "text/plain"
        if full.endswith(".csv"):
            ctype = "text/csv"
        if full.endswith(".json"):
            ctype = "application/json"
        with open(full, "rb") as f:
            body = f.read()
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


# ===========================================================================
# Single-page app (dark theme matching dashboard/build_dashboard.py).
# ===========================================================================
APP_HTML = r"""<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Backtest Control Panel</title>
<style>
:root{
  --bg:#0d1117; --panel:#161b22; --panel2:#1c2330; --border:#2a3242;
  --txt:#e6edf3; --muted:#8b949e; --accent:#58a6ff; --green:#3fb950;
  --red:#f85149; --amber:#d29922; --grid:#222b38;
}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--txt);
  font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;
  font-size:14px;display:flex;min-height:100vh}
a{color:var(--accent);text-decoration:none}
h1,h2,h3{margin:0 0 .4em}
nav{width:200px;background:var(--panel);border-right:1px solid var(--border);
  padding:18px 0;flex-shrink:0;position:sticky;top:0;height:100vh}
nav .brand{font-weight:700;font-size:16px;padding:0 18px 16px;color:var(--accent)}
nav button{display:block;width:100%;text-align:left;background:none;border:none;
  color:var(--muted);padding:11px 18px;font-size:14px;cursor:pointer;border-left:3px solid transparent}
nav button:hover{color:var(--txt);background:var(--panel2)}
nav button.active{color:var(--txt);border-left-color:var(--accent);background:var(--panel2)}
nav .calib{position:absolute;bottom:14px;left:18px;right:18px;font-size:11px;color:var(--muted)}
main{flex:1;padding:24px 30px;max-width:1200px;overflow:auto}
.card{background:var(--panel);border:1px solid var(--border);border-radius:8px;
  padding:18px;margin-bottom:16px}
.grp{margin-bottom:18px}
.grp>h3{font-size:13px;text-transform:uppercase;letter-spacing:.5px;color:var(--accent);
  border-bottom:1px solid var(--border);padding-bottom:6px;margin-bottom:12px}
.row{display:flex;flex-wrap:wrap;gap:14px}
.fld{display:flex;flex-direction:column;gap:4px;min-width:160px;flex:1}
.fld label{font-size:12px;color:var(--muted)}
input,select,textarea{background:var(--bg);border:1px solid var(--border);color:var(--txt);
  border-radius:6px;padding:7px 9px;font-size:13px;font-family:inherit}
input:focus,select:focus,textarea:focus{outline:none;border-color:var(--accent)}
textarea{width:100%;resize:vertical;min-height:50px}
.chk{flex-direction:row;align-items:center;gap:8px;min-width:auto}
.chk input{width:auto}
button.go{background:var(--accent);color:#04101f;border:none;border-radius:6px;
  padding:10px 22px;font-size:14px;font-weight:600;cursor:pointer}
button.go:hover{filter:brightness(1.1)}
button.go:disabled{opacity:.5;cursor:not-allowed}
button.ghost{background:var(--panel2);color:var(--txt);border:1px solid var(--border);
  border-radius:6px;padding:8px 14px;cursor:pointer;font-size:13px}
button.ghost:hover{border-color:var(--accent)}
button.danger{color:var(--red);border-color:var(--red)}
.prog{margin-top:18px;display:none}
.prog.show{display:block}
.bar{height:22px;background:var(--bg);border:1px solid var(--border);border-radius:6px;
  overflow:hidden;position:relative}
.bar>span{display:block;height:100%;background:linear-gradient(90deg,#1f6feb,#58a6ff);
  width:0%;transition:width .4s}
.bar>em{position:absolute;inset:0;display:flex;align-items:center;justify-content:center;
  font-style:normal;font-size:12px;color:var(--txt)}
.pstat{display:flex;gap:24px;margin-top:10px;font-size:13px;color:var(--muted)}
.pstat b{color:var(--txt)}
.runs{display:grid;grid-template-columns:repeat(auto-fill,minmax(280px,1fr));gap:14px}
.runcard{background:var(--panel);border:1px solid var(--border);border-radius:8px;padding:14px;cursor:pointer}
.runcard:hover{border-color:var(--accent)}
.runcard h3{font-size:15px}
.runcard .sub{color:var(--muted);font-size:12px;margin-bottom:8px}
.metrics{display:flex;flex-wrap:wrap;gap:6px 16px;font-size:12px}
.metrics span{color:var(--muted)}
.metrics b{color:var(--txt)}
.pos{color:var(--green)} .neg{color:var(--red)}
.runcard .actions{margin-top:10px;display:flex;gap:8px}
table{width:100%;border-collapse:collapse;font-size:13px}
th,td{text-align:left;padding:7px 10px;border-bottom:1px solid var(--border)}
th{color:var(--muted);font-weight:600}
.better{color:var(--green);font-weight:600} .worse{color:var(--red)}
.err{color:var(--red);background:#2d1416;border:1px solid var(--red);border-radius:6px;
  padding:10px;margin-top:12px;white-space:pre-wrap;font-family:ui-monospace,monospace;font-size:12px;display:none}
.err.show{display:block}
iframe{width:100%;height:78vh;border:1px solid var(--border);border-radius:8px;background:#fff}
.pill{display:inline-block;padding:2px 8px;border-radius:10px;font-size:11px;background:var(--panel2);
  border:1px solid var(--border);color:var(--muted)}
.note{color:var(--muted);font-size:12px;margin-top:6px}
.hidden{display:none}
</style></head>
<body>
<nav>
  <div class="brand">Backtest<br>Control&nbsp;Panel</div>
  <button data-view="new" class="active">New Run</button>
  <button data-view="runs">Runs</button>
  <button data-view="compare">Compare</button>
  <div class="calib" id="calib"></div>
</nav>
<main>
  <!-- NEW RUN -->
  <section id="view-new">
    <h1>New Run</h1>
    <div id="binwarn" class="err"></div>
    <div class="card">
      <div class="grp"><h3>Run</h3>
        <div class="row">
          <div class="fld"><label>Label</label><input id="f_label" placeholder="my run"></div>
          <div class="fld"><label>Strategy</label><select id="f_strategy"></select></div>
          <div class="fld"><label>Starting balance</label><input id="f_balance" placeholder="1000"></div>
        </div>
        <div class="note" id="strat_desc"></div>
      </div>

      <div class="grp"><h3>Data</h3>
        <div class="row">
          <div class="fld"><label>Source</label><select id="f_source">
            <option value="ndjson">ndjson</option>
            <option value="clickhouse">clickhouse</option>
            <option value="adapter">adapter</option></select></div>
          <div class="fld" id="wrap_ndjson"><label>NDJSON file</label><select id="f_ndjson"></select></div>
          <div class="fld hidden" id="wrap_clickhouse"><label>ClickHouse URL</label><input id="f_clickhouse" placeholder="http://localhost:8123"></div>
          <div class="fld hidden" id="wrap_adapter"><label>Adapter</label><select id="f_adapter"></select></div>
          <div class="fld hidden" id="wrap_adapterpath"><label>Adapter path</label><input id="f_adapterpath"></div>
          <div class="fld hidden" id="wrap_venue"><label>Venue tag</label><input id="f_venue"></div>
        </div>
        <div class="row">
          <div class="fld"><label>Instrument glob</label><input id="f_instrument" placeholder="KXNATGASD-%"></div>
          <div class="fld"><label>Start (YYYY-MM-DD)</label><input id="f_start"></div>
          <div class="fld"><label>End (YYYY-MM-DD)</label><input id="f_end"></div>
        </div>
      </div>

      <div class="grp"><h3>Strategy Params</h3>
        <div class="row" id="strat_params"><span class="note">select a strategy</span></div>
      </div>

      <div class="grp"><h3>Execution realism</h3><div class="row" id="grp_exec"></div></div>
      <div class="grp"><h3>Latency</h3><div class="row" id="grp_lat"></div></div>
      <div class="grp"><h3>Risk</h3><div class="row" id="grp_risk"></div></div>

      <div class="grp"><h3>Escape hatch</h3>
        <div class="fld"><label>Extra flags (appended verbatim)</label>
          <textarea id="f_extra" placeholder="--verbose"></textarea></div>
      </div>

      <button class="go" id="runbtn">Run backtest</button>
      <div class="err" id="runerr"></div>

      <div class="prog" id="prog">
        <div class="bar"><span id="bar"></span><em id="barlabel">0%</em></div>
        <div class="pstat">
          <div>Status: <b id="p_status">-</b></div>
          <div>Elapsed: <b id="p_elapsed">0s</b></div>
          <div>ETA: <b id="p_eta">-</b></div>
          <div>Events: <b id="p_events">-</b></div>
          <div id="p_msg" style="color:var(--accent)"></div>
        </div>
        <div style="margin-top:10px"><button class="ghost danger" id="cancelbtn">Cancel</button></div>
      </div>
    </div>
  </section>

  <!-- RUNS -->
  <section id="view-runs" class="hidden">
    <h1>Cached Runs</h1>
    <div id="runs" class="runs"></div>
    <div id="run-detail" class="hidden">
      <button class="ghost" onclick="closeDetail()">&larr; back</button>
      <button class="ghost" id="clonebtn">Clone into form</button>
      <h2 id="rd_title"></h2>
      <iframe id="rd_frame"></iframe>
    </div>
  </section>

  <!-- COMPARE -->
  <section id="view-compare" class="hidden">
    <h1>Compare</h1>
    <div class="card">
      <div class="row">
        <div class="fld"><label>Run A</label><select id="cmp_a"></select></div>
        <div class="fld"><label>Run B</label><select id="cmp_b"></select></div>
        <div class="fld" style="justify-content:flex-end"><button class="go" id="cmpbtn">Compare</button></div>
      </div>
      <div class="err" id="cmperr"></div>
    </div>
    <div id="cmp_out" class="hidden">
      <h2>Metrics diff</h2>
      <div class="card"><table id="cmp_table"></table></div>
      <h2>Equity overlay</h2>
      <iframe id="cmp_frame"></iframe>
    </div>
  </section>
</main>
<script>
const $=s=>document.querySelector(s); const $$=s=>[...document.querySelectorAll(s)];
let META=null, POLL=null, CUR=null;

function fmt(n,d=2){if(n==null||n==='')return '-';const x=+n;return isNaN(x)?n:x.toLocaleString(undefined,{maximumFractionDigits:d});}
function cls(n){return (+n>=0)?'pos':'neg';}

// ---- nav ----
$$('nav button').forEach(b=>b.onclick=()=>{
  $$('nav button').forEach(x=>x.classList.remove('active'));b.classList.add('active');
  ['new','runs','compare'].forEach(v=>$('#view-'+v).classList.toggle('hidden',v!==b.dataset.view));
  if(b.dataset.view==='runs')loadRuns();
  if(b.dataset.view==='compare')loadCompareOptions();
});

// ---- meta / form build ----
async function init(){
  META=await (await fetch('/api/meta')).json();
  $('#calib').textContent='calibration: '+Math.round(META.calibration.rate).toLocaleString()+' ev/s ('+META.calibration.samples+' runs)';
  if(!META.binary_ok){const w=$('#binwarn');w.classList.add('show');
    w.textContent='Binary missing. Build it:\\n  cd backtester && cargo build --release';}
  // strategy dropdown
  const ss=$('#f_strategy'); META.strategies.forEach(s=>{const o=document.createElement('option');o.value=s.name;o.textContent=s.name;ss.appendChild(o);});
  ss.onchange=renderStratParams;
  // ndjson files
  const nf=$('#f_ndjson'); META.data_files.forEach(f=>{const o=document.createElement('option');o.value=f;o.textContent=f;nf.appendChild(o);});
  // adapters
  const af=$('#f_adapter'); META.adapters.forEach(a=>{const o=document.createElement('option');o.value=a.key;o.textContent=a.key+' ['+a.venue+']';af.appendChild(o);});
  if(!META.clickhouse_available){[...$('#f_source').options].forEach(o=>{if(o.value==='clickhouse')o.textContent='clickhouse (binary lacks feature)';});}
  // toggles by group
  renderToggles();
  $('#f_source').onchange=onSource; onSource();
  renderStratParams();
}

function onSource(){const s=$('#f_source').value;
  $('#wrap_ndjson').classList.toggle('hidden',s!=='ndjson');
  $('#wrap_clickhouse').classList.toggle('hidden',s!=='clickhouse');
  ['wrap_adapter','wrap_adapterpath','wrap_venue'].forEach(id=>$('#'+id).classList.toggle('hidden',s!=='adapter'));
}

function renderStratParams(){
  const name=$('#f_strategy').value; const s=META.strategies.find(x=>x.name===name);
  $('#strat_desc').textContent=s?s.desc:'';
  const box=$('#strat_params'); box.innerHTML='';
  if(!s||!s.params.length){box.innerHTML='<span class="note">no tunable params</span>';return;}
  s.params.forEach(p=>{const d=document.createElement('div');d.className='fld';
    d.innerHTML='<label>'+p.key+'</label><input data-pk="'+p.key+'" placeholder="'+p.default+'">';
    box.appendChild(d);});
}

function renderToggles(){
  const map={'Execution realism':'#grp_exec','Latency':'#grp_lat','Risk':'#grp_risk'};
  for(const k in map)$(map[k]).innerHTML='';
  META.toggles.forEach(t=>{const box=$(map[t.group]);if(!box)return;const d=document.createElement('div');
    if(t.kind==='bool'){d.className='fld chk';
      d.innerHTML='<input type="checkbox" data-tk="'+t.key+'" data-kind="bool"><label>'+t.label+'</label>';}
    else if(t.kind==='str'&&t.choices){let opts=t.choices.map(c=>'<option value="'+c+'">'+(c||'(default)')+'</option>').join('');
      d.className='fld';d.innerHTML='<label>'+t.label+'</label><select data-tk="'+t.key+'" data-kind="str">'+opts+'</select>';}
    else{d.className='fld';d.innerHTML='<label>'+t.label+'</label><input data-tk="'+t.key+'" data-kind="'+t.kind+'" placeholder="default">';}
    box.appendChild(d);});
}

function collectConfig(){
  const src=$('#f_source').value;
  const c={label:$('#f_label').value.trim(),source:src,strategy:$('#f_strategy').value,
    instrument:$('#f_instrument').value.trim(),start:$('#f_start').value.trim(),end:$('#f_end').value.trim(),
    starting_balance:$('#f_balance').value.trim(),extra_flags:$('#f_extra').value.trim(),
    strategy_params:{},toggles:{}};
  if(src==='ndjson')c.ndjson=$('#f_ndjson').value;
  if(src==='clickhouse')c.clickhouse=$('#f_clickhouse').value.trim();
  if(src==='adapter'){c.adapter=$('#f_adapter').value;c.adapter_path=$('#f_adapterpath').value.trim();c.venue=$('#f_venue').value.trim();}
  $$('#strat_params input').forEach(i=>{if(i.value.trim()!=='')c.strategy_params[i.dataset.pk]=i.value.trim();});
  $$('[data-tk]').forEach(i=>{const k=i.dataset.tk,kind=i.dataset.kind;
    if(kind==='bool'){if(i.checked)c.toggles[k]=true;}
    else if(i.value.trim()!=='')c.toggles[k]=i.value.trim();});
  return c;
}

// ---- run + progress ----
$('#runbtn').onclick=async()=>{
  $('#runerr').classList.remove('show');
  const r=await fetch('/api/run',{method:'POST',body:JSON.stringify(collectConfig())});
  const j=await r.json();
  if(j.error){$('#runerr').textContent=j.error;$('#runerr').classList.add('show');return;}
  CUR=j.run_id; $('#prog').classList.add('show'); $('#runbtn').disabled=true;
  poll();
};
$('#cancelbtn').onclick=async()=>{if(CUR)await fetch('/api/cancel?run_id='+CUR,{method:'POST'});};

function poll(){clearInterval(POLL);POLL=setInterval(async()=>{
  if(!CUR)return;const p=await(await fetch('/api/progress?run_id='+CUR)).json();
  $('#bar').style.width=p.pct+'%'; $('#barlabel').textContent=p.pct+'%';
  $('#p_status').textContent=p.status; $('#p_elapsed').textContent=p.elapsed_secs+'s';
  $('#p_eta').textContent=p.eta_secs?p.eta_secs+'s':(p.status==='done'?'0s':'-');
  $('#p_events').textContent=p.events?p.events.toLocaleString():'-';
  $('#p_msg').textContent=p.message||'';
  if(p.status==='done'){clearInterval(POLL);$('#runbtn').disabled=false;
    setTimeout(()=>openRun(CUR),400);}
  if(p.status==='error'){clearInterval(POLL);$('#runbtn').disabled=false;
    $('#runerr').textContent=p.message;$('#runerr').classList.add('show');}
},500);}

// ---- runs view ----
async function loadRuns(){
  const j=await(await fetch('/api/runs')).json(); const box=$('#runs'); box.innerHTML='';
  $('#run-detail').classList.add('hidden'); box.classList.remove('hidden');
  if(!j.runs.length){box.innerHTML='<span class="note">no cached runs yet</span>';return;}
  j.runs.forEach(m=>{const m2=m.metrics||{};const d=document.createElement('div');d.className='runcard';
    d.innerHTML='<h3>'+(m.label||m.strategy)+' <span class="pill">'+m.status+'</span></h3>'+
      '<div class="sub">'+m.strategy+' &middot; '+m.source+' &middot; '+(m.instrument||'')+'<br>'+m.timestamp+' &middot; '+fmt(m.duration_secs,1)+'s</div>'+
      '<div class="metrics">'+
        '<span>PnL <b class="'+cls(m2.pnl_total)+'">'+fmt(m2.pnl_total)+'</b></span>'+
        '<span>PnL% <b class="'+cls(m2.pnl_pct)+'">'+fmt(m2.pnl_pct)+'</b></span>'+
        '<span>Sharpe <b>'+fmt(m2.sharpe)+'</b></span>'+
        '<span>Fills <b>'+fmt(m2.num_fills,0)+'</b></span>'+
        '<span>Events <b>'+fmt(m.total_events,0)+'</b></span></div>'+
      '<div class="actions"><button class="ghost" data-open="'+m.id+'">View</button>'+
        '<button class="ghost danger" data-del="'+m.id+'">Delete</button></div>';
    box.appendChild(d);});
  $$('[data-open]').forEach(b=>b.onclick=e=>{e.stopPropagation();openRun(b.dataset.open);});
  $$('[data-del]').forEach(b=>b.onclick=async e=>{e.stopPropagation();
    if(confirm('Delete run '+b.dataset.del+'?')){await fetch('/api/delete?id='+b.dataset.del,{method:'POST'});loadRuns();}});
  $$('.runcard').forEach(c=>c.onclick=()=>{const id=c.querySelector('[data-open]').dataset.open;openRun(id);});
}

async function openRun(id){
  $$('nav button').forEach(x=>x.classList.remove('active'));
  $('nav button[data-view="runs"]').classList.add('active');
  ['new','runs','compare'].forEach(v=>$('#view-'+v).classList.toggle('hidden',v!=='runs'));
  $('#runs').classList.add('hidden'); const det=$('#run-detail');det.classList.remove('hidden');
  const j=await(await fetch('/api/run?id='+id)).json();
  $('#rd_title').textContent=(j.meta.label||id)+'  ['+j.meta.strategy+']';
  $('#rd_frame').src=j.has_dashboard?('/runs/'+id+'/dashboard.html'):'about:blank';
  $('#clonebtn').onclick=()=>cloneConfig(j.config);
}
function closeDetail(){$('#run-detail').classList.add('hidden');$('#runs').classList.remove('hidden');}

function cloneConfig(c){
  $('#f_label').value=(c.label||'')+' (clone)'; $('#f_strategy').value=c.strategy; renderStratParams();
  $('#f_source').value=c.source; onSource();
  if(c.ndjson)$('#f_ndjson').value=c.ndjson;
  if(c.clickhouse)$('#f_clickhouse').value=c.clickhouse;
  if(c.adapter)$('#f_adapter').value=c.adapter;
  if(c.adapter_path)$('#f_adapterpath').value=c.adapter_path;
  if(c.venue)$('#f_venue').value=c.venue;
  $('#f_instrument').value=c.instrument||''; $('#f_start').value=c.start||''; $('#f_end').value=c.end||'';
  $('#f_balance').value=c.starting_balance||''; $('#f_extra').value=c.extra_flags||'';
  for(const k in (c.strategy_params||{})){const el=document.querySelector('[data-pk="'+k+'"]');if(el)el.value=c.strategy_params[k];}
  $$('[data-tk]').forEach(i=>{const k=i.dataset.tk;if(k in (c.toggles||{})){if(i.dataset.kind==='bool')i.checked=true;else i.value=c.toggles[k];}else{if(i.dataset.kind==='bool')i.checked=false;}});
  $$('nav button').forEach(x=>x.classList.remove('active'));$('nav button[data-view="new"]').classList.add('active');
  ['new','runs','compare'].forEach(v=>$('#view-'+v).classList.toggle('hidden',v!=='new'));
}

// ---- compare ----
async function loadCompareOptions(){
  const j=await(await fetch('/api/runs')).json();const done=j.runs.filter(r=>r.status==='done');
  const opts=done.map(r=>'<option value="'+r.id+'">'+(r.label||r.id)+'</option>').join('');
  $('#cmp_a').innerHTML=opts; $('#cmp_b').innerHTML=opts; if(done.length>1)$('#cmp_b').selectedIndex=1;
}
$('#cmpbtn').onclick=async()=>{
  $('#cmperr').classList.remove('show');
  const a=$('#cmp_a').value,b=$('#cmp_b').value;
  const j=await(await fetch('/api/compare?a='+a+'&b='+b)).json();
  if(j.error){$('#cmperr').textContent=j.error;$('#cmperr').classList.add('show');return;}
  $('#cmp_out').classList.remove('hidden'); $('#cmp_frame').src=j.url;
  renderDiff(j.a,j.b);
};
function renderDiff(a,b){
  const am=a.metrics||{},bm=b.metrics||{};
  const rows=[['pnl_total','PnL'],['pnl_pct','PnL %'],['sharpe','Sharpe'],['sortino','Sortino'],
    ['max_drawdown_pct','Max DD %'],['num_fills','Fills'],['num_trades','Trades'],['win_rate','Win rate']];
  let html='<tr><th>Metric</th><th>'+(a.label||a.id)+'</th><th>'+(b.label||b.id)+'</th></tr>';
  rows.forEach(([k,lab])=>{const av=am[k],bv=bm[k];
    const higherBad=k==='max_drawdown_pct';
    let ca='',cb='';
    if(av!=null&&bv!=null&&+av!==+bv){const aBetter=higherBad?(+av<+bv):(+av>+bv);
      ca=aBetter?'better':'worse';cb=aBetter?'worse':'better';}
    html+='<tr><td>'+lab+'</td><td class="'+ca+'">'+fmt(av)+'</td><td class="'+cb+'">'+fmt(bv)+'</td></tr>';});
  $('#cmp_table').innerHTML=html;
}

init();
</script>
</body></html>
"""


# ===========================================================================
# Entry point
# ===========================================================================
def main():
    port = _free_port()
    httpd = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    url = "http://127.0.0.1:%d/" % port
    print("=" * 64)
    print(" Kalshi Backtest Control Panel")
    print(" Serving at: %s" % url)
    print(" Binary    : %s%s" % (BINARY, "" if os.path.isfile(BINARY) else "  [MISSING -- build it]"))
    print(" Runs cache: %s" % RUNS_DIR)
    print(" Calibration: %.0f events/sec (%d samples)" % (CALIB.rate, CALIB.samples))
    print("=" * 64)
    sys.stdout.flush()
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\nshutting down")
        httpd.shutdown()


if __name__ == "__main__":
    main()
