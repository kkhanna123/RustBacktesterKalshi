"""Tests for the venue-agnostic feeds.base.run_feed routing loop.

A FakeFeed yields a known set of canonical rows from a finite source; we assert they
all land in a FakeSink, in order, and that the sink is flushed and closed once.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

from feeds.base import Feed, StopFeed, run_feed  # noqa: E402


class FakeSink:
    def __init__(self):
        self.rows = []
        self.flushed = 0
        self.closed = 0

    def write_row(self, row):
        self.rows.append(row)

    def flush(self):
        self.flushed += 1

    def close(self):
        self.closed += 1


class FakeFeed(Feed):
    """Emits two raw 'messages'; normalize fans the first into 2 rows, second into 1."""

    name = "fake"
    venue = "FAKE"

    def __init__(self):
        super().__init__()
        self._queue = ["A", "B"]
        self.connected = 0
        self.subscribed_to = None

    def discover_markets(self):
        return ["m1", "m2"]

    def connect(self):
        self.connected += 1

    def subscribe(self, markets):
        self.subscribed_to = markets

    def recv(self):
        if not self._queue:
            raise StopFeed
        return self._queue.pop(0)

    def normalize(self, raw):
        if raw == "A":
            yield {"kind": "delta", "instrument": "m1", "tag": "a1"}
            yield {"kind": "delta", "instrument": "m1", "tag": "a2"}
        elif raw == "B":
            yield {"kind": "trade", "instrument": "m2", "tag": "b1"}


class TestRunFeedRouting(unittest.TestCase):
    def test_rows_routed_to_sink_in_order(self):
        feed = FakeFeed()
        sink = FakeSink()
        rc = run_feed(feed, sink, once=True, install_signal_handlers=False)
        self.assertEqual(rc, 0)
        self.assertEqual([r["tag"] for r in sink.rows], ["a1", "a2", "b1"])
        self.assertEqual(sink.flushed, 1)
        self.assertEqual(sink.closed, 1)

    def test_discover_markets_used_when_none_passed(self):
        feed = FakeFeed()
        sink = FakeSink()
        run_feed(feed, sink, once=True, install_signal_handlers=False)
        self.assertEqual(feed.subscribed_to, ["m1", "m2"])
        self.assertEqual(feed.connected, 1)

    def test_explicit_markets_override_discovery(self):
        feed = FakeFeed()
        sink = FakeSink()
        run_feed(feed, sink, markets=["x"], once=True, install_signal_handlers=False)
        self.assertEqual(feed.subscribed_to, ["x"])


if __name__ == "__main__":
    unittest.main()
