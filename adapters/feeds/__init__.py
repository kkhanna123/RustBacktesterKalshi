"""feeds — a small MULTI-VENUE live-tick collector framework.

Every venue is reduced to ONE class that implements the :class:`feeds.base.Feed`
contract. All feeds normalize their raw venue messages into the SAME canonical
NDJSON rows already produced by the Kalshi collector (see ``book.py``), and are
driven by the venue-agnostic :func:`feeds.base.run_feed` loop, which fans rows out
to the existing sinks (``sinks.NdjsonSink`` / ``sinks.ClickHouseSink`` /
``sinks.FanoutSink``).

Add a venue in 3 steps (see ``feeds/README.md``):
  1. Subclass :class:`feeds.base.Feed`.
  2. Implement ``discover_markets`` / ``connect`` / ``subscribe`` /
     ``recv`` / ``normalize``.
  3. Register it in :data:`feeds.run_feed.REGISTRY`.
"""

from .base import Feed, run_feed

__all__ = ["Feed", "run_feed"]
