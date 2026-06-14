"""run_feed.py — CLI to run ANY venue feed into the shared sinks.

Resolves a venue to its :class:`feeds.base.Feed`, wires the durable NDJSON sink (and
optionally ClickHouse) via ``sinks.FanoutSink``, and drives it with
:func:`feeds.base.run_feed`.

Examples::

    # Live Kalshi (needs KALSHI_API_KEY_ID + KALSHI_PRIVATE_KEY in the env):
    python -m feeds.run_feed --venue kalshi --series KXNATGASD \
        --out ../data/raw --clickhouse http://localhost:8123

    # A stub venue WITHOUT live creds, replaying a recorded file:
    python -m feeds.run_feed --venue polymarket --replay sample.ndjson --out /tmp/feedtest

Run this module from the ``collector/`` directory (so ``book`` / ``sinks`` import).
"""

from __future__ import annotations

import argparse
import logging
import sys

from sinks import ClickHouseSink, FanoutSink, NdjsonSink  # reuse existing sinks

from .base import run_feed
from .hyperliquid import HyperliquidFeed
from .kalshi import KalshiFeed
from .polymarket import PolymarketFeed

log = logging.getLogger("collector.feeds.run_feed")


def _make_kalshi(args):
    markets = [m.strip() for m in args.markets.split(",") if m.strip()] if args.markets else None
    return KalshiFeed(series=args.series, markets=markets, demo=args.demo)


def _make_polymarket(args):
    markets = [m.strip() for m in args.markets.split(",") if m.strip()] if args.markets else None
    return PolymarketFeed(markets=markets, replay_path=args.replay)


def _make_hyperliquid(args):
    markets = [m.strip() for m in args.markets.split(",") if m.strip()] if args.markets else None
    return HyperliquidFeed(markets=markets, replay_path=args.replay)


#: venue name -> factory(args) -> Feed.  Register a new venue by adding one entry.
REGISTRY = {
    "kalshi": _make_kalshi,
    "polymarket": _make_polymarket,
    "hyperliquid": _make_hyperliquid,
}


def main(argv=None) -> int:
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s %(message)s",
    )
    ap = argparse.ArgumentParser(description="Run a multi-venue tick feed into the shared sinks.")
    ap.add_argument("--venue", required=True, choices=sorted(REGISTRY.keys()))
    ap.add_argument("--markets", default=None, help="explicit comma-separated instrument ids")
    ap.add_argument("--replay", default=None,
                    help="replay an NDJSON/CSV file instead of connecting live (stub venues)")
    ap.add_argument("--out", default="../data/raw", help="NDJSON output directory")
    ap.add_argument("--clickhouse", default=None, help="ClickHouse HTTP url, e.g. http://localhost:8123")
    ap.add_argument("--series", default="KXNATGASD", help="(kalshi) series ticker for discovery")
    ap.add_argument("--demo", action="store_true", help="(kalshi) use the demo cluster")
    args = ap.parse_args(argv)

    feed = REGISTRY[args.venue](args)

    ndjson = NdjsonSink(args.out)
    ch = ClickHouseSink(args.clickhouse) if args.clickhouse else None
    sink = FanoutSink([ndjson, ch])

    # A finite replay should run a single session then stop.
    once = bool(args.replay)
    markets = None
    if args.markets:
        markets = [m.strip() for m in args.markets.split(",") if m.strip()]

    log.info("running venue=%s replay=%s -> out=%s", args.venue, args.replay, args.out)
    return run_feed(feed, sink, markets=markets, once=once)


if __name__ == "__main__":
    sys.exit(main())
