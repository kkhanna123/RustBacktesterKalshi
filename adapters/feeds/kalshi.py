"""kalshi.py — ``KalshiFeed``: the reference :class:`feeds.base.Feed` over Kalshi.

This is a THIN wrapper that proves the abstraction fits the real, working venue. It
REUSES the existing, battle-tested collector modules unchanged:

  * ``book.BookReconstructor`` for orderbook/trade reconstruction (so rows are
    byte-for-byte identical to ``kalshi_ws_collector.py``),
  * ``kalshi_auth`` for the RSA-PSS WS handshake headers,
  * ``natgas_markets`` for public REST market discovery.

It contributes ONLY the venue glue: opening the WS, subscribing, receiving raw
messages, and routing each message type to the right ``BookReconstructor`` method
inside ``normalize``. Credentials come from the environment, exactly as the existing
collector expects::

    export KALSHI_API_KEY_ID=...
    export KALSHI_PRIVATE_KEY=/path/to/kalshi_key.pem
"""

from __future__ import annotations

import json
import logging
import os
from typing import Iterable, List, Optional

from book import BookReconstructor, now_ns  # reuse: identical reconstruction
from kalshi_auth import build_headers, load_private_key  # reuse: WS auth
from natgas_markets import host_for, list_open_markets  # reuse: discovery

from .base import Feed, StopFeed

log = logging.getLogger("collector.feeds.kalshi")

WS_PATH = "/trade-api/ws/v2"
PROD_WS = "wss://api.elections.kalshi.com" + WS_PATH
DEMO_WS = "wss://demo-api.elections.kalshi.com" + WS_PATH


class KalshiFeed(Feed):
    """Live Kalshi orderbook-delta + trade feed, emitting canonical rows.

    Args:
        series: Kalshi series ticker used for discovery (default ``KXNATGASD``).
        markets: explicit market tickers; if given, ``discover_markets`` returns them.
        demo: use the Kalshi demo cluster.
        key_id / private_key_path: override the env-var credentials.
    """

    name = "kalshi"
    venue = "KALSHI"

    def __init__(
        self,
        series: str = "KXNATGASD",
        markets: Optional[List[str]] = None,
        demo: bool = False,
        key_id: Optional[str] = None,
        private_key_path: Optional[str] = None,
    ):
        super().__init__()
        self.series = series
        self._explicit_markets = markets
        self.demo = demo
        self.key_id = key_id or os.environ.get("KALSHI_API_KEY_ID")
        self.private_key_path = private_key_path or os.environ.get("KALSHI_PRIVATE_KEY")
        self._private_key = None
        self.ws = None
        self._sub_id = 0
        self.books = {}  # instrument -> BookReconstructor

    # --- discovery ------------------------------------------------------------
    def discover_markets(self) -> List[str]:
        if self._explicit_markets:
            return list(self._explicit_markets)
        host = host_for(self.demo)
        log.info("discovering open %s markets via %s", self.series, host)
        return list_open_markets(series=self.series, host=host)

    # --- source lifecycle -----------------------------------------------------
    def _ensure_creds(self):
        if not self.key_id or not self.private_key_path:
            raise RuntimeError(
                "KALSHI_API_KEY_ID and KALSHI_PRIVATE_KEY (PEM path) must be set "
                "to connect to the Kalshi WebSocket."
            )
        if not os.path.exists(self.private_key_path):
            raise RuntimeError(
                f"KALSHI_PRIVATE_KEY points to a missing file: {self.private_key_path}"
            )
        if self._private_key is None:
            self._private_key = load_private_key(self.private_key_path)

    def connect(self) -> None:
        import websocket  # local import so file-replay feeds don't need it

        self._ensure_creds()
        headers = build_headers(self.key_id, self._private_key, "GET", WS_PATH)
        header_list = [f"{k}: {v}" for k, v in headers.items()]
        url = DEMO_WS if self.demo else PROD_WS
        log.info("connecting to %s", url)
        self.ws = websocket.create_connection(url, header=header_list, timeout=30)

    def subscribe(self, markets: List[str]) -> None:
        # reset book state so the next snapshot is treated as fresh
        self.books = {m: BookReconstructor(m) for m in markets}
        self._sub_id += 1
        sub = {
            "id": self._sub_id,
            "cmd": "subscribe",
            "params": {
                "channels": ["orderbook_delta", "trade"],
                "market_tickers": markets,
            },
        }
        self.ws.send(json.dumps(sub))
        log.info("subscribed (id=%d) to %d markets", self._sub_id, len(markets))

    def recv(self):
        raw = self.ws.recv()
        if raw is None or raw == "":
            return self.recv()  # skip keepalive blanks
        return raw

    def close(self) -> None:
        if self.ws is not None:
            try:
                self.ws.close()
            except Exception:  # noqa: BLE001
                pass
            self.ws = None

    # --- normalization --------------------------------------------------------
    def _book_for(self, ticker: str) -> Optional[BookReconstructor]:
        if not ticker:
            return None
        b = self.books.get(ticker)
        if b is None:  # market not pre-registered (e.g. lifecycle) — create lazily
            b = BookReconstructor(ticker)
            self.books[ticker] = b
        return b

    def normalize(self, raw) -> Iterable[dict]:
        """Map one raw Kalshi WS message (str/bytes JSON or dict) to canonical rows.

        Delegates entirely to ``BookReconstructor`` so the rows are identical to the
        existing ``kalshi_ws_collector``. Mirrors that collector's message routing.
        """
        recv_ns = now_ns()
        if isinstance(raw, (bytes, bytearray)):
            raw = raw.decode("utf-8")
        if isinstance(raw, str):
            try:
                msg = json.loads(raw)
            except (ValueError, TypeError):
                return
        else:
            msg = raw

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
            yield from b.on_snapshot(inner, recv_ns=recv_ns, seq=seq)
            return

        if mtype == "orderbook_delta":
            b = self._book_for(ticker)
            if b is None:
                return
            # Kalshi `seq` is per-subscription, not per-market; a per-market gap is
            # benign. Resync (on_delta sets last_seq) instead of resubscribe storms.
            if not b.check_seq(seq):
                log.debug("seq resync on %s: had %s got %s", ticker, b.last_seq, seq)
            yield from b.on_delta(inner, recv_ns=recv_ns, seq=seq)
            return

        if mtype == "trade":
            b = self._book_for(ticker)
            if b is None:
                return
            yield b.trade_row(inner, recv_ns=recv_ns)
            return

        log.debug("unhandled message type: %s", mtype)
