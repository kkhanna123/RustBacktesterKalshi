"""Tests for the stub feeds' normalize() mappings to canonical rows.

Polymarket: NO outcome maps to the YES complement price (1 - p).
Hyperliquid: native price passthrough (NOT [0,1]); bids->BUY, asks->SELL.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

from feeds.hyperliquid import HyperliquidFeed  # noqa: E402
from feeds.polymarket import PolymarketFeed  # noqa: E402

# the two canonical row shapes, for schema assertions
DELTA_KEYS = {"kind", "ts_ns", "instrument", "action", "side", "price", "size",
              "sequence", "is_snapshot", "venue", "market_alias"}
TRADE_KEYS = {"kind", "ts_ns", "instrument", "aggressor_side", "price", "size",
              "trade_id", "venue"}


class TestPolymarketNormalize(unittest.TestCase):
    def test_yes_book_passthrough(self):
        feed = PolymarketFeed()
        raw = {"event_type": "book", "ts_ms": 1700000000000, "market": "0xM",
               "outcome": "YES", "side": "BUY", "action": "ADD",
               "price": "0.62", "size": "120", "seq": 7, "is_snapshot": 1}
        rows = list(feed.normalize(raw))
        self.assertEqual(len(rows), 1)
        r = rows[0]
        self.assertEqual(set(r.keys()), DELTA_KEYS)
        self.assertEqual(r["kind"], "delta")
        self.assertEqual(r["venue"], "POLYMARKET")
        self.assertEqual(r["side"], "BUY")
        self.assertEqual(r["price"], 0.62)
        self.assertEqual(r["size"], 120.0)
        self.assertEqual(r["sequence"], 7)
        self.assertEqual(r["is_snapshot"], 1)
        self.assertEqual(r["ts_ns"], 1700000000000 * 1_000_000)

    def test_no_outcome_maps_to_yes_complement(self):
        feed = PolymarketFeed()
        raw = {"event_type": "book", "ts_ms": 1700000000000, "market": "0xM",
               "outcome": "NO", "side": "BUY", "action": "ADD",
               "price": "0.62", "size": "10"}
        r = list(feed.normalize(raw))[0]
        # buyer of NO at 0.62 == seller of YES at 1 - 0.62 = 0.38, so side flips BUY -> SELL.
        self.assertEqual(r["price"], 0.38)
        self.assertEqual(r["side"], "SELL")

    def test_trade_row(self):
        feed = PolymarketFeed()
        raw = {"event_type": "trade", "ts_ms": 1700000000000, "market": "0xM",
               "outcome": "NO", "taker_side": "BUY", "price": "0.40",
               "size": "5", "trade_id": "t1"}
        r = list(feed.normalize(raw))[0]
        self.assertEqual(set(r.keys()), TRADE_KEYS)
        self.assertEqual(r["kind"], "trade")
        # taker BUYS the NO token == sold YES, so the YES-native aggressor took the NO side.
        self.assertEqual(r["aggressor_side"], "no")
        self.assertEqual(r["price"], 0.60)  # NO 0.40 -> YES 0.60
        self.assertEqual(r["trade_id"], "t1")


class TestHyperliquidNormalize(unittest.TestCase):
    def test_l2book_passthrough_native_price(self):
        feed = HyperliquidFeed()
        raw = {"channel": "l2Book", "ts_ms": 1700000000000, "coin": "BTC",
               "levels": [[{"px": "60000.5", "sz": "1.5", "n": 3}],
                          [{"px": "60001.0", "sz": "2.0", "n": 4}]],
               "seq": 42, "is_snapshot": 1}
        rows = list(feed.normalize(raw))
        self.assertEqual(len(rows), 2)
        bid, ask = rows
        self.assertEqual(set(bid.keys()), DELTA_KEYS)
        self.assertEqual(bid["side"], "BUY")
        self.assertEqual(bid["price"], 60000.5)  # NOT in [0,1] — native passthrough
        self.assertEqual(bid["size"], 1.5)
        self.assertEqual(bid["action"], "ADD")  # snapshot
        self.assertEqual(ask["side"], "SELL")
        self.assertEqual(ask["price"], 60001.0)
        self.assertEqual(bid["venue"], "HYPERLIQUID")

    def test_zero_size_level_is_delete(self):
        feed = HyperliquidFeed()
        raw = {"channel": "l2Book", "ts_ms": 1, "coin": "ETH",
               "levels": [[{"px": "3000", "sz": "0"}], []], "is_snapshot": 0}
        r = list(feed.normalize(raw))[0]
        self.assertEqual(r["action"], "DELETE")

    def test_trade_side_mapping(self):
        feed = HyperliquidFeed()
        raw = {"channel": "trades", "ts_ms": 1700000000000, "coin": "BTC",
               "side": "B", "px": "60000.5", "sz": "0.3", "tid": "t9"}
        r = list(feed.normalize(raw))[0]
        self.assertEqual(set(r.keys()), TRADE_KEYS)
        self.assertEqual(r["aggressor_side"], "buy")
        self.assertEqual(r["price"], 60000.5)
        self.assertEqual(r["trade_id"], "t9")


if __name__ == "__main__":
    unittest.main()
