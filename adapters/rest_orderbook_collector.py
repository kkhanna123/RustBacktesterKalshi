#!/usr/bin/env python3
"""
Credential-free live Kalshi orderbook collector (REST polling).

The WebSocket `orderbook_delta` channel requires API-key auth. When you don't have keys, Kalshi's
PUBLIC REST endpoint `GET /trade-api/v2/markets/{ticker}/orderbook` still returns the full live L2
book (`orderbook_fp.yes_dollars` / `no_dollars`) unauthenticated. This collector polls that endpoint
for every open market in a series (default KXNATGASD natgas), reconstructs the YES-native book, DIFFS
consecutive polls into ADD/UPDATE/DELETE delta rows (with periodic full `is_snapshot=1` snapshots),
and writes them to the SAME NDJSON + ClickHouse schema as the WebSocket collector — so the backtester
can't tell the difference (fidelity is snapshot-granular at the poll interval, not every native tick).

Retention is capped (default 2 days): NDJSON files older than --max-age-days are pruned each cycle.

Usage:
    python rest_orderbook_collector.py --series KXNATGASD --out ../data/raw \
        --clickhouse http://localhost:8123 --interval 5 --max-age-days 2

This is the "collector running the whole time" path when no Kalshi credentials are available.
"""
import argparse
import datetime as dt
import logging
import os
import signal
import sys
import time

import requests

# Reuse the shared price mapping + sinks so output is byte-identical in schema to the WS collector.
from book import price_to_cents, complement_cents  # YES-native helpers (operate in cents)
from sinks import NdjsonSink, ClickHouseSink, FanoutSink
from natgas_markets import list_open_markets

LOG = logging.getLogger("rest_collector")
DEFAULT_HOST = "api.elections.kalshi.com"


class OrderbookPoller:
    """Polls the public REST orderbook for a set of markets and emits delta rows by diffing."""

    def __init__(self, host: str, snapshot_every: int = 60):
        self.host = host
        self.base = f"https://{host}/trade-api/v2"
        self.session = requests.Session()
        self.session.headers.update({"Accept": "application/json",
                                     "User-Agent": "kalshi-rest-orderbook-collector/1.0"})
        # per-market last book: {instrument: {"BUY": {cents: size}, "SELL": {cents: size}}}
        self._last: dict = {}
        # per-market poll counter, to force a full snapshot every `snapshot_every` polls
        self._count: dict = {}
        self.snapshot_every = snapshot_every

    def fetch_book(self, ticker: str):
        """Return (yes_levels, no_levels) as lists of (price_dollars, size), or None on error."""
        url = f"{self.base}/markets/{ticker}/orderbook"
        try:
            r = self.session.get(url, params={"depth": 100}, timeout=15)
        except requests.RequestException as e:
            LOG.debug("net error %s for %s", e, ticker)
            return None
        if r.status_code != 200:
            LOG.debug("%s -> http %s", ticker, r.status_code)
            return None
        ob = (r.json() or {}).get("orderbook_fp") or {}
        yes = [(float(p), float(s)) for p, s in (ob.get("yes_dollars") or [])]
        no = [(float(p), float(s)) for p, s in (ob.get("no_dollars") or [])]
        return yes, no

    @staticmethod
    def _book_from_levels(yes, no) -> dict:
        """Build a YES-native book {BUY:{cents:size}, SELL:{cents:size}} from yes/no levels."""
        book = {"BUY": {}, "SELL": {}}
        for price, size in yes:
            if size > 0:
                book["BUY"][price_to_cents(price)] = size
        for price, size in no:  # a NO bid at q == a YES ask at 1-q
            if size > 0:
                book["SELL"][complement_cents(price_to_cents(price))] = size
        return book

    def poll_market(self, ticker: str, ts_ns: int):
        """Poll one market; return a list of delta row dicts (diffed vs the previous poll)."""
        fetched = self.fetch_book(ticker)
        if fetched is None:
            return []
        new_book = self._book_from_levels(*fetched)
        n = self._count.get(ticker, 0)
        self._count[ticker] = n + 1
        force_snapshot = (ticker not in self._last) or (n % self.snapshot_every == 0)

        rows = []
        if force_snapshot:
            first = True
            for side in ("BUY", "SELL"):
                for cents, size in sorted(new_book[side].items()):
                    rows.append(self._row(ticker, ts_ns, "ADD", side, cents, size,
                                          is_snapshot=1 if first else 0))
                    first = False
            if first:  # empty book -> still mark a reset
                rows.append(self._row(ticker, ts_ns, "ADD", "BUY", 0, 0.0, is_snapshot=1))
        else:
            old = self._last[ticker]
            for side in ("BUY", "SELL"):
                old_side, new_side = old[side], new_book[side]
                for cents, size in new_side.items():
                    prev = old_side.get(cents)
                    if prev is None:
                        rows.append(self._row(ticker, ts_ns, "ADD", side, cents, size))
                    elif abs(prev - size) > 1e-9:
                        rows.append(self._row(ticker, ts_ns, "UPDATE", side, cents, size))
                for cents in old_side:
                    if cents not in new_side:
                        rows.append(self._row(ticker, ts_ns, "DELETE", side, cents, 0.0))
        self._last[ticker] = new_book
        return rows

    @staticmethod
    def _row(ticker, ts_ns, action, side, cents, size, is_snapshot=0):
        return {
            "kind": "delta", "ts_ns": ts_ns, "instrument": ticker, "action": action,
            "side": side, "price": round(cents / 100.0, 2), "size": float(size),
            "sequence": 0, "is_snapshot": is_snapshot, "venue": "KALSHI", "market_alias": "",
        }


def prune_old(out_dir: str, max_age_days: float):
    """Delete dated subdirectories / files older than max_age_days (the retention cap)."""
    if not os.path.isdir(out_dir):
        return
    cutoff = time.time() - max_age_days * 86400
    removed = 0
    for root, _dirs, files in os.walk(out_dir):
        for fn in files:
            fp = os.path.join(root, fn)
            try:
                if os.path.getmtime(fp) < cutoff:
                    os.remove(fp)
                    removed += 1
            except OSError:
                pass
    if removed:
        LOG.info("pruned %d file(s) older than %.1f days", removed, max_age_days)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--series", default="KXNATGASD")
    ap.add_argument("--out", default="../data/raw")
    ap.add_argument("--clickhouse", default=None, help="ClickHouse HTTP url, e.g. http://localhost:8123")
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--interval", type=float, default=5.0, help="seconds between poll cycles")
    ap.add_argument("--req-sleep", type=float, default=0.04, help="seconds between per-market requests")
    ap.add_argument("--snapshot-every", type=int, default=60, help="full snapshot every N polls per market")
    ap.add_argument("--max-age-days", type=float, default=2.0, help="retention cap for NDJSON files")
    ap.add_argument("--refresh-markets-secs", type=float, default=600, help="re-list open markets cadence")
    ap.add_argument("--max-markets", type=int, default=0, help="cap markets polled (0 = all)")
    ap.add_argument("--max-runtime-secs", type=float, default=0, help="auto-stop after N seconds (0 = forever)")
    ap.add_argument("--log-level", default="INFO")
    args = ap.parse_args(argv)

    logging.basicConfig(level=getattr(logging, args.log_level.upper(), logging.INFO),
                        format="%(asctime)s %(levelname)s %(message)s")

    sinks = [NdjsonSink(args.out)]
    if args.clickhouse:
        sinks.append(ClickHouseSink(args.clickhouse))
    sink = FanoutSink(sinks)
    poller = OrderbookPoller(args.host, snapshot_every=args.snapshot_every)

    stop = {"flag": False}
    signal.signal(signal.SIGINT, lambda *_: stop.update(flag=True))
    signal.signal(signal.SIGTERM, lambda *_: stop.update(flag=True))

    markets, last_refresh = [], 0.0
    start = time.time()
    total = 0
    cycles = 0
    while not stop["flag"]:
        now = time.time()
        if args.max_runtime_secs and now - start > args.max_runtime_secs:
            LOG.info("max runtime reached, stopping")
            break
        if now - last_refresh > args.refresh_markets_secs or not markets:
            try:
                markets = list_open_markets(series=args.series, host=args.host)
                if args.max_markets:
                    markets = markets[: args.max_markets]
                last_refresh = now
                LOG.info("tracking %d open %s markets", len(markets), args.series)
            except Exception as e:  # noqa: BLE001
                LOG.warning("market refresh failed: %s", e)
            prune_old(args.out, args.max_age_days)

        cycle_rows = 0
        for tk in markets:
            if stop["flag"]:
                break
            ts_ns = time.time_ns()
            for row in poller.poll_market(tk, ts_ns):
                sink.write_row(row)
                cycle_rows += 1
            time.sleep(args.req_sleep)
        sink.flush()
        total += cycle_rows
        cycles += 1
        if cycles % 5 == 1:
            LOG.info("cycle %d: +%d rows (%d total) across %d markets",
                     cycles, cycle_rows, total, len(markets))
        # sleep the remainder of the interval
        elapsed = time.time() - now
        if elapsed < args.interval and not stop["flag"]:
            time.sleep(args.interval - elapsed)

    sink.close()
    LOG.info("collector stopped: %d rows written across %d cycles", total, cycles)
    return 0


if __name__ == "__main__":
    sys.exit(main())
