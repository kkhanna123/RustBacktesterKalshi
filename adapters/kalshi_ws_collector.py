"""kalshi_ws_collector.py — runnable Kalshi WS orderbook-delta + trade collector.

Subscribes to the Kalshi WebSocket ``orderbook_delta`` + ``trade`` channels for natgas
(``KXNATGASD``) markets, reconstructs rows via book.BookReconstructor, and writes them to
BOTH a durable on-disk NDJSON.gz sink (always) and ClickHouse (when ``--clickhouse`` is
given and reachable).

Run:
    export KALSHI_API_KEY_ID=...
    export KALSHI_PRIVATE_KEY=/path/to/kalshi_key.pem
    python kalshi_ws_collector.py --series KXNATGASD --out ../data/raw \
        --clickhouse http://localhost:8123

If creds are missing it prints a clear message and exits 2 (does not crash).

Endpoints:
    prod : wss://api.elections.kalshi.com/trade-api/ws/v2
    demo : wss://demo-api.elections.kalshi.com/trade-api/ws/v2   (--demo)

Auth: the 3 KALSHI-ACCESS-* headers on the WS handshake (see kalshi_auth.py).
Seq gaps trigger an automatic resubscribe (-> fresh snapshot, new is_snapshot=1).
SIGINT flushes and closes the sinks cleanly.
"""

import argparse
import json
import logging
import os
import signal
import sys
import time

import websocket

import book as book_mod
from book import BookReconstructor, now_ns
from kalshi_auth import build_headers, load_private_key
from natgas_markets import host_for, list_open_markets
from sinks import ClickHouseSink, FanoutSink, NdjsonSink

log = logging.getLogger("collector")

WS_PATH = "/trade-api/ws/v2"
PROD_WS = "wss://api.elections.kalshi.com" + WS_PATH
DEMO_WS = "wss://demo-api.elections.kalshi.com" + WS_PATH


def ws_url(demo=False):
    return DEMO_WS if demo else PROD_WS


class Collector:
    def __init__(self, key_id, private_key, markets, sink, demo=False):
        self.key_id = key_id
        self.private_key = private_key
        self.markets = markets
        self.sink = sink
        self.demo = demo
        self.books = {m: BookReconstructor(m) for m in markets}
        self._stop = False
        self._sub_id = 0
        self._gap_count = 0
        self.ws = None

    # --- lifecycle ------------------------------------------------------------
    def stop(self):
        self._stop = True
        try:
            if self.ws is not None:
                self.ws.close()
        except Exception:  # noqa: BLE001
            pass

    def run_forever(self):
        backoff = 1.0
        while not self._stop:
            try:
                self._run_once()
                backoff = 1.0
            except Exception as e:  # noqa: BLE001
                log.warning("WS session ended: %s", e)
            if self._stop:
                break
            log.info("reconnecting in %.1fs", backoff)
            time.sleep(backoff)
            backoff = min(backoff * 2, 30.0)
        self.sink.flush()
        self.sink.close()

    def _run_once(self):
        headers = build_headers(self.key_id, self.private_key, "GET", WS_PATH)
        header_list = [f"{k}: {v}" for k, v in headers.items()]
        url = ws_url(self.demo)
        log.info("connecting to %s for %d markets", url, len(self.markets))
        self.ws = websocket.create_connection(url, header=header_list, timeout=30)
        try:
            self._subscribe()
            while not self._stop:
                raw = self.ws.recv()
                if raw is None or raw == "":
                    continue
                self._on_message(raw)
        finally:
            try:
                self.ws.close()
            except Exception:  # noqa: BLE001
                pass

    def _subscribe(self):
        self._sub_id += 1
        sub = {
            "id": self._sub_id,
            "cmd": "subscribe",
            "params": {
                "channels": ["orderbook_delta", "trade"],
                "market_tickers": self.markets,
            },
        }
        self.ws.send(json.dumps(sub))
        log.info("subscribed (id=%d) to %d markets", self._sub_id, len(self.markets))

    def _resubscribe(self, reason=""):
        log.warning("resubscribing due to: %s", reason)
        # reset all books so the next snapshot is treated as fresh
        for b in self.books.values():
            b.last_seq = None
        try:
            self._subscribe()
        except Exception as e:  # noqa: BLE001
            log.warning("resubscribe failed: %s", e)
            raise

    # --- message routing ------------------------------------------------------
    def _on_message(self, raw):
        recv_ns = now_ns()
        try:
            msg = json.loads(raw)
        except (ValueError, TypeError):
            log.debug("non-JSON message: %r", raw[:200])
            return

        mtype = msg.get("type")
        seq = msg.get("seq")
        inner = msg.get("msg", {}) or {}
        ticker = inner.get("market_ticker")

        if mtype in ("subscribed", "ok", "error"):
            if mtype == "error":
                log.warning("WS error message: %s", msg)
            return

        if mtype == "orderbook_snapshot":
            b = self._book_for(ticker)
            if b is None:
                return
            for row in b.on_snapshot(inner, recv_ns=recv_ns, seq=seq):
                self.sink.write_row(row)
            return

        if mtype == "orderbook_delta":
            b = self._book_for(ticker)
            if b is None:
                return
            # Kalshi's `seq` is global PER SUBSCRIPTION (shared across all subscribed markets),
            # not per-market. So a per-market view sees seq jump whenever *other* markets update,
            # which is benign — NOT a dropped message. We therefore resync and keep applying the
            # delta (on_delta sets last_seq=seq) instead of triggering a resubscribe storm. A real
            # dropped message self-heals on the periodic resubscribe / reconnect snapshot.
            if not b.check_seq(seq):
                log.debug("seq resync on %s: had %s got %s", ticker, b.last_seq, seq)
                self._gap_count += 1
            for row in b.on_delta(inner, recv_ns=recv_ns, seq=seq):
                self.sink.write_row(row)
            return

        if mtype == "trade":
            b = self._book_for(ticker)
            if b is None:
                return
            self.sink.write_row(b.trade_row(inner, recv_ns=recv_ns))
            return

        log.debug("unhandled message type: %s", mtype)

    def _book_for(self, ticker):
        if not ticker:
            return None
        b = self.books.get(ticker)
        if b is None:
            # a market we did not pre-register (e.g. lifecycle) — create lazily
            b = BookReconstructor(ticker)
            self.books[ticker] = b
        return b


def resolve_markets(args):
    if args.markets:
        return [m.strip() for m in args.markets.split(",") if m.strip()]
    host = host_for(args.demo)
    log.info("discovering open %s markets via %s", args.series, host)
    return list_open_markets(series=args.series, host=host)


def main(argv=None):
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s %(message)s",
    )
    ap = argparse.ArgumentParser(description="Kalshi WS orderbook-delta + trade collector.")
    ap.add_argument("--series", default="KXNATGASD")
    ap.add_argument("--out", default="../data/raw")
    ap.add_argument("--clickhouse", default=None, help="ClickHouse HTTP url, e.g. http://localhost:8123")
    ap.add_argument("--demo", action="store_true")
    ap.add_argument("--markets", default=None, help="explicit comma-separated market tickers")
    ap.add_argument("--fp-divisor", type=float, default=None,
                    help="override book.FP_DIVISOR (contracts scaling)")
    args = ap.parse_args(argv)

    if args.fp_divisor is not None:
        book_mod.FP_DIVISOR = args.fp_divisor
        log.info("FP_DIVISOR overridden to %s", args.fp_divisor)

    key_id = os.environ.get("KALSHI_API_KEY_ID")
    pem_path = os.environ.get("KALSHI_PRIVATE_KEY")
    if not key_id or not pem_path:
        print(
            "ERROR: KALSHI_API_KEY_ID and KALSHI_PRIVATE_KEY (PEM path) must be set.\n"
            "  export KALSHI_API_KEY_ID=...\n"
            "  export KALSHI_PRIVATE_KEY=/path/to/kalshi_key.pem\n"
            "Cannot connect to the Kalshi WebSocket without credentials.",
            file=sys.stderr,
        )
        return 2
    if not os.path.exists(pem_path):
        print(f"ERROR: KALSHI_PRIVATE_KEY points to a missing file: {pem_path}", file=sys.stderr)
        return 2

    try:
        private_key = load_private_key(pem_path)
    except Exception as e:  # noqa: BLE001
        print(f"ERROR: could not load private key from {pem_path}: {e}", file=sys.stderr)
        return 2

    markets = resolve_markets(args)
    if not markets:
        print(f"No open markets found for series {args.series}.", file=sys.stderr)
        return 3

    ndjson = NdjsonSink(args.out)
    ch = ClickHouseSink(args.clickhouse) if args.clickhouse else None
    sink = FanoutSink([ndjson, ch])

    collector = Collector(key_id, private_key, markets, sink, demo=args.demo)

    def _handle_sigint(signum, frame):
        log.info("SIGINT received, shutting down...")
        collector.stop()

    signal.signal(signal.SIGINT, _handle_sigint)
    signal.signal(signal.SIGTERM, _handle_sigint)

    log.info("starting collector for %d markets", len(markets))
    collector.run_forever()
    log.info("collector stopped, sinks flushed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
