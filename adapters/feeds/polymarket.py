"""polymarket.py — STUB ``PolymarketFeed`` for the Polymarket CLOB.

STATUS: stub. The live WS wiring is NOT implemented (see the TODO in ``connect``);
what IS implemented and tested is the ``normalize`` mapping plus a WORKING local
file-replay fallback, so the feed is usable TODAY without live access:

    python -m feeds.run_feed --venue polymarket --replay sample.ndjson --out /tmp/x

CANONICAL MAPPING (documented assumption)
=========================================
Polymarket's CLOB is a central limit order book over ERC-1155 outcome *tokens*. A
binary market has two complementary token ids (the YES token and the NO token), and
prices live in [0, 1] (a probability / dollar payoff of $1).

We normalize to a single YES-native book, exactly like Kalshi:

  * a level on the YES token at price ``p`` (size ``q``)  -> book side per ``BUY/SELL``
    at price ``p``;
  * a level on the NO token at price ``p`` (size ``q``)   -> the equivalent YES level
    at price ``round(1 - p, 2)`` with the same size (a buyer of NO at p is a seller of
    YES at 1 - p). This is the same complement trick ``book.py`` uses for Kalshi NO.

ASSUMED RAW MESSAGE SHAPE (until the real endpoint is wired)
------------------------------------------------------------
A book event::

    {"event_type": "book", "ts_ms": 1700000000000, "market": "0xMARKET",
     "outcome": "YES"|"NO", "side": "BUY"|"SELL", "action": "ADD"|"UPDATE"|"DELETE",
     "price": "0.62", "size": "120", "seq": 7, "is_snapshot": 0}

A trade event::

    {"event_type": "trade", "ts_ms": 1700000000000, "market": "0xMARKET",
     "outcome": "YES"|"NO", "taker_side": "BUY"|"SELL",
     "price": "0.62", "size": "50", "trade_id": "t1"}
"""

from __future__ import annotations

import logging
from typing import Iterable, List, Optional

from .base import Feed, StopFeed
from ._replay import FileReplaySource

log = logging.getLogger("collector.feeds.polymarket")

PROD_WS = "wss://ws-subscriptions-clob.polymarket.com/ws/market"  # TODO: confirm


def _to_float(x):
    if x is None:
        return 0.0
    if isinstance(x, str):
        return float(x.strip())
    return float(x)


def _yes_price(outcome: str, price: float) -> float:
    """Map a (outcome, price) onto the YES-native price. NO -> 1 - price."""
    if (outcome or "YES").upper() == "NO":
        return round(1.0 - price, 2)
    return round(price, 2)


class PolymarketFeed(Feed):
    """Polymarket CLOB feed (stub live WS + working file replay).

    Args:
        markets: token/market ids to subscribe to.
        replay_path: if set, read messages from this NDJSON/CSV file instead of a WS.
        replay_fmt: ``"ndjson"`` (default) or ``"csv"`` for the replay file.
    """

    name = "polymarket"
    venue = "POLYMARKET"

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
        # Open a websocket to PROD_WS, then in subscribe() send the Polymarket
        # market-channel subscription for the given token ids, and in recv() read
        # frames. Until then, live mode is unavailable; use --replay.
        raise NotImplementedError(
            "PolymarketFeed live WS not implemented yet; run with --replay <file>. "
            "TODO: wire PROD_WS subscribe/recv."
        )

    def subscribe(self, markets: List[str]) -> None:
        if self._replay is not None:
            return  # replay file is self-contained
        # TODO: send the Polymarket subscription frame for `markets`.
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
        """Map one Polymarket message to canonical rows (YES-native)."""
        if not isinstance(raw, dict):
            return
        etype = raw.get("event_type") or raw.get("type")
        ts_ms = raw.get("ts_ms") or raw.get("timestamp")
        ts_ns = int(ts_ms) * 1_000_000 if ts_ms is not None else 0
        outcome = raw.get("outcome", "YES")
        is_no = (outcome or "YES").upper() == "NO"
        price = _yes_price(outcome, _to_float(raw.get("price")))

        if etype == "trade":
            # Canonical aggressor_side is "yes"/"no" (NOT buy/sell): "yes" iff the taker ended up
            # BUYING YES-equivalent — buying the YES token, or SELLING the NO token (buying NO == selling YES).
            taker_buy = (raw.get("taker_side") or "BUY").upper() == "BUY"
            bought_yes = taker_buy != is_no  # the NO token flips the direction
            yield {
                "kind": "trade",
                "ts_ns": ts_ns,
                "instrument": raw.get("market", ""),
                "aggressor_side": "yes" if bought_yes else "no",
                "price": price,
                "size": _to_float(raw.get("size")),
                "trade_id": str(raw.get("trade_id", "")),
                "venue": self.venue,
            }
            return

        if etype in ("book", "delta", "level"):
            # A NO-token level maps to a YES level at 1-price with the side FLIPPED: a bid (BUY) for
            # NO at p is an ask (SELL) for YES at 1-p (and vice-versa). YES-token levels pass through.
            side = (raw.get("side") or "BUY").upper()
            if is_no:
                side = "SELL" if side == "BUY" else "BUY"
            yield {
                "kind": "delta",
                "ts_ns": ts_ns,
                "instrument": raw.get("market", ""),
                "action": (raw.get("action") or "UPDATE").upper(),
                "side": side,
                "price": price,
                "size": _to_float(raw.get("size")),
                "sequence": int(raw.get("seq", 0) or 0),
                "is_snapshot": int(raw.get("is_snapshot", 0) or 0),
                "venue": self.venue,
                "market_alias": raw.get("market_alias", ""),
            }
            return
        # unknown/control message -> nothing
