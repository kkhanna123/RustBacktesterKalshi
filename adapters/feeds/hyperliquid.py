"""hyperliquid.py — STUB ``HyperliquidFeed`` for the Hyperliquid L2 book.

STATUS: stub. The live WS wiring is NOT implemented (see the TODO in ``connect``);
the ``normalize`` mapping plus a WORKING local file-replay fallback ARE implemented and
tested, so the feed is usable TODAY without live access:

    python -m feeds.run_feed --venue hyperliquid --replay sample.ndjson --out /tmp/x

PRICE CAVEAT (important)
========================
Hyperliquid is a perps/spot DEX, NOT a binary market. Its prices are real asset prices
(e.g. a BTC perp at 60000.5), so the canonical ``price`` field here is NOT in [0, 1] and
the YES/NO complement trick does NOT apply. We therefore PASS THROUGH the venue's
bid/ask levels directly: a bid level -> book side ``BUY``, an ask level -> ``SELL``,
both at their native price. The canonical row schema is unchanged; only the *meaning*
of ``price`` differs (absolute price, not a probability). Downstream consumers that
assume [0, 1] must special-case ``venue == "HYPERLIQUID"``.

ASSUMED RAW MESSAGE SHAPE (Hyperliquid ``l2Book`` style, until the real feed is wired)
-------------------------------------------------------------------------------------
A book update::

    {"channel": "l2Book", "ts_ms": 1700000000000, "coin": "BTC",
     "levels": [[{"px": "60000.5", "sz": "1.5", "n": 3}, ...],   # bids (index 0)
                [{"px": "60001.0", "sz": "2.0", "n": 4}, ...]],  # asks (index 1)
     "seq": 42, "is_snapshot": 1}

A trade::

    {"channel": "trades", "ts_ms": 1700000000000, "coin": "BTC",
     "side": "B"|"A", "px": "60000.5", "sz": "0.3", "tid": "t9"}
"""

from __future__ import annotations

import logging
from typing import Iterable, List, Optional

from .base import Feed
from ._replay import FileReplaySource

log = logging.getLogger("collector.feeds.hyperliquid")

PROD_WS = "wss://api.hyperliquid.xyz/ws"  # TODO: confirm endpoint + subscription shape


def _to_float(x):
    if x is None:
        return 0.0
    if isinstance(x, str):
        return float(x.strip())
    return float(x)


class HyperliquidFeed(Feed):
    """Hyperliquid L2 book + trades feed (stub live WS + working file replay).

    Args:
        markets: coin symbols to subscribe to (e.g. ``["BTC", "ETH"]``).
        replay_path: if set, read messages from this NDJSON/CSV file instead of a WS.
        replay_fmt: ``"ndjson"`` (default) or ``"csv"`` for the replay file.
    """

    name = "hyperliquid"
    venue = "HYPERLIQUID"

    def __init__(
        self,
        markets: Optional[List[str]] = None,
        replay_path: Optional[str] = None,
        replay_fmt: str = "ndjson",
    ):
        super().__init__()
        self._markets = markets or []
        self._replay = FileReplaySource(replay_path, replay_fmt) if replay_path else None

    def discover_markets(self) -> List[str]:
        return list(self._markets)

    def connect(self) -> None:
        if self._replay is not None:
            self._replay.connect()
            return
        # TODO: real WS endpoint + auth + message shape.
        # Open a websocket to PROD_WS, then in subscribe() send {"method":"subscribe",
        # "subscription":{"type":"l2Book","coin": <coin>}} per market, and in recv()
        # read frames. Until then, live mode is unavailable; use --replay.
        raise NotImplementedError(
            "HyperliquidFeed live WS not implemented yet; run with --replay <file>. "
            "TODO: wire PROD_WS subscribe/recv."
        )

    def subscribe(self, markets: List[str]) -> None:
        if self._replay is not None:
            return
        # TODO: send one l2Book + trades subscription per coin in `markets`.
        raise NotImplementedError

    def recv(self):
        if self._replay is not None:
            return self._replay.recv()
        raise NotImplementedError

    def close(self) -> None:
        if self._replay is not None:
            self._replay.close()

    # --- normalization (implemented + tested) ---------------------------------
    def normalize(self, raw) -> Iterable[dict]:
        """Map one Hyperliquid message to canonical rows (native price passthrough)."""
        if not isinstance(raw, dict):
            return
        channel = raw.get("channel") or raw.get("type")
        ts_ms = raw.get("ts_ms") or raw.get("timestamp")
        ts_ns = int(ts_ms) * 1_000_000 if ts_ms is not None else 0
        coin = raw.get("coin", "")

        if channel == "trades" or channel == "trade":
            # Hyperliquid trade side: "B" (buy/bid aggressor) / "A" (ask).
            side = (raw.get("side") or "").upper()
            aggressor = "buy" if side == "B" else "sell" if side == "A" else side.lower()
            yield {
                "kind": "trade",
                "ts_ns": ts_ns,
                "instrument": coin,
                "aggressor_side": aggressor,
                "price": _to_float(raw.get("px") or raw.get("price")),
                "size": _to_float(raw.get("sz") or raw.get("size")),
                "trade_id": str(raw.get("tid") or raw.get("trade_id", "")),
                "venue": self.venue,
            }
            return

        if channel == "l2Book" or channel == "book":
            seq = int(raw.get("seq", 0) or 0)
            is_snapshot = int(raw.get("is_snapshot", 0) or 0)
            levels = raw.get("levels") or [[], []]
            # levels[0] = bids -> BUY, levels[1] = asks -> SELL. Native price passthrough.
            for book_side, lst in (("BUY", levels[0]), ("SELL", levels[1] if len(levels) > 1 else [])):
                for lvl in lst:
                    size = _to_float(lvl.get("sz"))
                    action = "DELETE" if size <= 0 else ("ADD" if is_snapshot else "UPDATE")
                    yield {
                        "kind": "delta",
                        "ts_ns": ts_ns,
                        "instrument": coin,
                        "action": action,
                        "side": book_side,
                        "price": _to_float(lvl.get("px")),
                        "size": size,
                        "sequence": seq,
                        "is_snapshot": is_snapshot,
                        "venue": self.venue,
                        "market_alias": "",
                    }
            return
        # unknown/control message -> nothing
