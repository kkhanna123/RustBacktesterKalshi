"""sinks.py — durable on-disk NDJSON.gz sink + ClickHouse HTTP sink.

NdjsonSink   : always-on durable capture. Writes gzip NDJSON lines, one row dict per
               line, rotating the file by UTC date:
                   <out_dir>/<YYYY-MM-DD>/kalshi_<unix_ts>.ndjson.gz
ClickHouseSink: best-effort warehouse. Batches rows and POSTs
                   INSERT INTO kalshi.orderbook_deltas FORMAT JSONEachRow\n<rows>
                (and the trades equivalent) to {url}/?query=... via requests.
                Tolerates ClickHouse being down (logs and drops the batch; NDJSON
                remains the source of truth).

Both sinks are no-op-safe and expose ``write_row(row)`` / ``flush()`` / ``close()``.
A row is the SHARED NDJSON contract dict produced by book.BookReconstructor.
"""

import datetime as _dt
import gzip
import json
import logging
import os
import time

import requests

log = logging.getLogger("collector.sinks")


def _ts_ns_to_iso(ts_ns):
    """Nanosecond epoch -> ClickHouse DateTime64(9) ISO string (UTC, 9 frac digits)."""
    ts_ns = int(ts_ns)
    secs = ts_ns // 1_000_000_000
    frac = ts_ns % 1_000_000_000
    dt = _dt.datetime.utcfromtimestamp(secs)
    return dt.strftime("%Y-%m-%d %H:%M:%S") + f".{frac:09d}"


# --- mapping NDJSON rows -> ClickHouse column dicts ---------------------------
def delta_row_to_ch(row):
    return {
        "timestamp": _ts_ns_to_iso(row["ts_ns"]),
        "instrument_id": row["instrument"],
        "venue": row.get("venue", "KALSHI"),
        "action": row["action"],
        "side": row["side"],
        "price": row["price"],
        "size": row["size"],
        "market_alias": row.get("market_alias", ""),
        "sequence": row.get("sequence", 0),
        "is_snapshot": row.get("is_snapshot", 0),
    }


def trade_row_to_ch(row):
    return {
        "ts_event": _ts_ns_to_iso(row["ts_ns"]),
        "instrument_id": row["instrument"],
        "venue": row.get("venue", "KALSHI"),
        "aggressor_side": row["aggressor_side"],
        "price": row["price"],
        "size": row["size"],
        "market_alias": row.get("market_alias", ""),
        "trade_id": row.get("trade_id", ""),
    }


class NdjsonSink:
    """Durable gzip NDJSON sink, rotating by UTC date."""

    def __init__(self, out_dir, flush_every=200, flush_interval_s=5.0):
        self.out_dir = out_dir
        self.flush_every = flush_every
        self.flush_interval_s = flush_interval_s
        self._fh = None
        self._date = None
        self._path = None
        self._since_flush = 0
        self._last_flush = time.time()
        os.makedirs(out_dir, exist_ok=True)

    def _ensure_file(self):
        today = _dt.datetime.utcnow().strftime("%Y-%m-%d")
        if self._fh is not None and today == self._date:
            return
        # rotate
        if self._fh is not None:
            self._fh.close()
        day_dir = os.path.join(self.out_dir, today)
        os.makedirs(day_dir, exist_ok=True)
        ts = int(time.time())
        self._path = os.path.join(day_dir, f"kalshi_{ts}.ndjson.gz")
        self._fh = gzip.open(self._path, "at", encoding="utf-8")
        self._date = today
        log.info("NdjsonSink writing to %s", self._path)

    def write_row(self, row):
        self._ensure_file()
        self._fh.write(json.dumps(row, separators=(",", ":")) + "\n")
        self._since_flush += 1
        now = time.time()
        if self._since_flush >= self.flush_every or (now - self._last_flush) >= self.flush_interval_s:
            self.flush()

    def flush(self):
        if self._fh is not None:
            self._fh.flush()
        self._since_flush = 0
        self._last_flush = time.time()

    def close(self):
        if self._fh is not None:
            self._fh.flush()
            self._fh.close()
            self._fh = None


class ClickHouseSink:
    """Best-effort ClickHouse HTTP sink. Tolerates the server being unreachable."""

    def __init__(self, url, batch_size=500, flush_interval_s=2.0,
                 deltas_table="kalshi.orderbook_deltas", trades_table="kalshi.trades",
                 session=None, timeout=15):
        self.url = (url or "").rstrip("/")
        self.enabled = bool(self.url)
        self.batch_size = batch_size
        self.flush_interval_s = flush_interval_s
        self.deltas_table = deltas_table
        self.trades_table = trades_table
        self.session = session or requests.Session()
        self.timeout = timeout
        self._delta_buf = []
        self._trade_buf = []
        self._last_flush = time.time()
        self.healthy = self.enabled

    def write_row(self, row):
        if not self.enabled:
            return
        if row.get("kind") == "trade":
            self._trade_buf.append(trade_row_to_ch(row))
        else:
            self._delta_buf.append(delta_row_to_ch(row))
        if (len(self._delta_buf) + len(self._trade_buf)) >= self.batch_size or \
                (time.time() - self._last_flush) >= self.flush_interval_s:
            self.flush()

    def _post(self, table, rows):
        if not rows:
            return
        query = f"INSERT INTO {table} FORMAT JSONEachRow"
        body = "\n".join(json.dumps(r, separators=(",", ":")) for r in rows)
        try:
            resp = self.session.post(
                self.url + "/",
                params={"query": query},
                data=body.encode("utf-8"),
                timeout=self.timeout,
            )
            if resp.status_code != 200:
                log.warning("ClickHouse insert into %s failed (%s): %s",
                            table, resp.status_code, resp.text[:300])
                self.healthy = False
            else:
                self.healthy = True
        except Exception as e:  # noqa: BLE001 - tolerate CH down
            log.warning("ClickHouse unreachable (%s); keeping NDJSON only: %s", table, e)
            self.healthy = False

    def flush(self):
        if not self.enabled:
            return
        if self._delta_buf:
            self._post(self.deltas_table, self._delta_buf)
            self._delta_buf = []
        if self._trade_buf:
            self._post(self.trades_table, self._trade_buf)
            self._trade_buf = []
        self._last_flush = time.time()

    def close(self):
        try:
            self.flush()
        except Exception:  # noqa: BLE001
            pass


class FanoutSink:
    """Write each row to every child sink, isolating per-sink failures."""

    def __init__(self, sinks):
        self.sinks = [s for s in sinks if s is not None]

    def write_row(self, row):
        for s in self.sinks:
            try:
                s.write_row(row)
            except Exception as e:  # noqa: BLE001
                log.warning("sink %s.write_row failed: %s", type(s).__name__, e)

    def flush(self):
        for s in self.sinks:
            try:
                s.flush()
            except Exception:  # noqa: BLE001
                pass

    def close(self):
        for s in self.sinks:
            try:
                s.close()
            except Exception:  # noqa: BLE001
                pass
