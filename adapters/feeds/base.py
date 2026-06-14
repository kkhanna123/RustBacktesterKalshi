"""base.py — the venue-agnostic ``Feed`` contract and the ``run_feed`` driver loop.

This module defines the *entire* surface a new live-tick venue must implement.
A venue is one class that subclasses :class:`Feed`; the generic :func:`run_feed`
loop handles connection lifecycle, reconnect-with-backoff, graceful SIGINT/SIGTERM,
and routing every normalized row to the sinks. Nothing in here is Kalshi-specific.

THE CANONICAL ROW CONTRACT
==========================
Every ``Feed.normalize(raw)`` call yields zero or more *canonical rows* — plain
dicts in the SHARED NDJSON schema already emitted by ``book.BookReconstructor`` and
consumed by the Rust backtester / ClickHouse tables. There are exactly two shapes:

Orderbook delta row (``kind == "delta"``)::

    {
      "kind":        "delta",
      "ts_ns":       int,        # event time, unix nanoseconds
      "instrument":  str,        # venue-native instrument / market id
      "action":      str,        # "ADD" | "UPDATE" | "DELETE"
      "side":        str,        # "BUY" | "SELL"  (YES-native book side)
      "price":       float,      # dollars, 2 decimals; for [0,1] venues a probability
      "size":        float,      # resting size at this level after the event
      "sequence":    int,        # venue sequence number (0 if none)
      "is_snapshot": int,        # 1 == first row of a fresh book snapshot, else 0
      "venue":       str,        # e.g. "KALSHI" | "POLYMARKET" | "HYPERLIQUID"
      "market_alias": str,       # optional human alias, "" if none
    }

Trade row (``kind == "trade"``)::

    {
      "kind":           "trade",
      "ts_ns":          int,
      "instrument":     str,
      "aggressor_side": str,     # taker side, venue-native string ("yes"/"buy"/...)
      "price":          float,
      "size":           float,
      "trade_id":       str,
      "venue":          str,
    }

These dicts are exactly what ``sinks.NdjsonSink`` / ``sinks.ClickHouseSink`` accept,
so a Feed never touches a sink directly — it only ever yields these dicts.

THE Feed CONTRACT (5 methods)
=============================
``discover_markets() -> list[str]``
    Return the instrument ids to subscribe to. May hit a REST endpoint or read
    config. Called once before ``connect``. If the user passed ``--markets`` the
    runner injects those instead and this is skipped.

``connect() -> None``
    Open the underlying source (a WebSocket, a file handle, ...). After this returns
    the feed must be ready for ``subscribe`` and ``recv``. Raise on failure — the
    runner will back off and call ``connect`` again.

``subscribe(markets: list[str]) -> None``
    Subscribe the open source to ``markets``. Safe to call again to re-subscribe.

``recv() -> object | None``
    Block for and return ONE raw venue message (already JSON-decoded, or a raw line
    — whatever ``normalize`` expects). Return ``None`` / raise ``StopFeed`` to signal
    the source ended (the runner then reconnects, unless stopping).

``normalize(raw) -> Iterable[dict]``
    Map ONE raw message to zero or more canonical rows (above). This is a PURE
    function of ``raw`` plus the feed's own book state — it must not touch sinks or
    the network. Unknown/control messages yield nothing.

``close() -> None`` (optional)
    Release the source. Default is a no-op.

A feed also exposes ``name`` and ``venue`` (the canonical ``venue`` string stamped
on rows). See ``feeds/kalshi.py`` for the reference implementation over the real,
working Kalshi WS, and ``feeds/polymarket.py`` / ``feeds/hyperliquid.py`` for stubs
that additionally support a local file-replay source (no live creds needed).
"""

from __future__ import annotations

import logging
import signal
import time
from typing import Iterable, List, Optional

log = logging.getLogger("collector.feeds")


class StopFeed(Exception):
    """Raised by ``recv()`` to signal the source is exhausted (e.g. replay EOF)."""


class Feed:
    """Abstract base class for a single-venue live tick source.

    Subclasses MUST set ``name`` and ``venue`` (or pass them to ``__init__``) and
    implement ``discover_markets`` / ``connect`` / ``subscribe`` / ``recv`` /
    ``normalize``. See the module docstring for the full contract.
    """

    #: Short human name of the feed, e.g. "kalshi".
    name: str = "feed"
    #: Canonical venue string stamped onto rows, e.g. "KALSHI".
    venue: str = "UNKNOWN"

    def __init__(self, name: Optional[str] = None, venue: Optional[str] = None):
        if name is not None:
            self.name = name
        if venue is not None:
            self.venue = venue

    # --- discovery ------------------------------------------------------------
    def discover_markets(self) -> List[str]:
        """Return the list of instrument ids to subscribe to. Default: empty."""
        return []

    # --- source lifecycle -----------------------------------------------------
    def connect(self) -> None:
        """Open the underlying source. Raise on failure."""
        raise NotImplementedError

    def subscribe(self, markets: List[str]) -> None:
        """Subscribe the open source to ``markets``."""
        raise NotImplementedError

    def recv(self):
        """Block for and return ONE raw message. Return None / raise StopFeed at end."""
        raise NotImplementedError

    def normalize(self, raw) -> Iterable[dict]:
        """Map one raw message to zero+ canonical rows. Pure; no I/O."""
        raise NotImplementedError

    def close(self) -> None:
        """Release the source. Default no-op."""
        return None


def run_feed(
    feed: Feed,
    sink,
    markets: Optional[List[str]] = None,
    max_backoff_s: float = 30.0,
    install_signal_handlers: bool = True,
    once: bool = False,
) -> int:
    """Drive ``feed`` end-to-end: connect, subscribe, recv→normalize→sink, forever.

    This is the venue-agnostic generalization of the Kalshi collector's
    ``run_forever`` loop. For each session it:

      1. ``feed.connect()`` then ``feed.subscribe(markets)``,
      2. loops ``raw = feed.recv()`` -> ``for row in feed.normalize(raw): sink.write_row(row)``,
      3. on any error logs it and reconnects after exponential backoff (capped at
         ``max_backoff_s``); a clean source end (``recv`` returns ``None`` or raises
         :class:`StopFeed`) ends the session.

    On stop (SIGINT/SIGTERM, or ``once=True`` after one session) it flushes and
    closes the sink and the feed.

    Args:
        feed: the venue feed to run.
        sink: any object with ``write_row(row)`` / ``flush()`` / ``close()``
            (typically a ``sinks.FanoutSink``).
        markets: instruments to subscribe to. If ``None``, uses
            ``feed.discover_markets()``.
        max_backoff_s: cap on the reconnect backoff.
        install_signal_handlers: install SIGINT/SIGTERM handlers for graceful stop.
            Set ``False`` when embedding (signals can only be set on the main thread).
        once: run a single source session then stop (used for finite replays/tests).

    Returns:
        ``0`` on a clean shutdown.
    """
    state = {"stop": False}

    def _handle_signal(signum, frame):  # noqa: ARG001
        log.info("signal %s received, shutting down feed %s...", signum, feed.name)
        state["stop"] = True
        try:
            feed.close()
        except Exception:  # noqa: BLE001
            pass

    if install_signal_handlers:
        try:
            signal.signal(signal.SIGINT, _handle_signal)
            signal.signal(signal.SIGTERM, _handle_signal)
        except (ValueError, OSError):
            # not on the main thread — caller must handle stopping itself
            log.debug("could not install signal handlers (not main thread)")

    if markets is None:
        markets = feed.discover_markets()
    log.info("feed %s (venue=%s): %d markets", feed.name, feed.venue, len(markets))

    backoff = 1.0
    n_rows = 0
    try:
        while not state["stop"]:
            try:
                feed.connect()
                feed.subscribe(markets)
                backoff = 1.0
                while not state["stop"]:
                    try:
                        raw = feed.recv()
                    except StopFeed:
                        log.info("feed %s: source ended", feed.name)
                        break
                    if raw is None:
                        log.info("feed %s: source ended (recv->None)", feed.name)
                        break
                    for row in feed.normalize(raw):
                        sink.write_row(row)
                        n_rows += 1
            except Exception as e:  # noqa: BLE001
                log.warning("feed %s session ended: %s", feed.name, e)
            finally:
                try:
                    feed.close()
                except Exception:  # noqa: BLE001
                    pass

            if once or state["stop"]:
                break
            log.info("feed %s reconnecting in %.1fs", feed.name, backoff)
            time.sleep(backoff)
            backoff = min(backoff * 2, max_backoff_s)
    finally:
        try:
            sink.flush()
        finally:
            sink.close()
        log.info("feed %s stopped, %d rows written, sink closed.", feed.name, n_rows)
    return 0
