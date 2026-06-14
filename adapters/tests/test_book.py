"""Unit tests for book.BookReconstructor — pure reconstruction logic."""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import book  # noqa: E402
from book import BookReconstructor  # noqa: E402

INSTR = "KXNATGASD-26APR0817-T2.650"


class TestSnapshot(unittest.TestCase):
    def test_yes_no_mapping_and_snapshot_flag(self):
        b = BookReconstructor(INSTR)
        msg = {
            "market_ticker": INSTR,
            "yes_dollars_fp": [["0.0800", "300.00"], ["0.4200", "10.00"]],
            "no_dollars_fp": [["0.3000", "5.00"]],  # NO @ 0.30 -> YES SELL @ 0.70
        }
        rows = b.on_snapshot(msg, recv_ns=1_000, seq=10)
        self.assertEqual(len(rows), 3)
        # first row is_snapshot=1, rest 0; all ADD
        self.assertEqual(rows[0]["is_snapshot"], 1)
        self.assertTrue(all(r["is_snapshot"] == 0 for r in rows[1:]))
        self.assertTrue(all(r["action"] == "ADD" for r in rows))

        # YES levels -> BUY at the same price
        buys = {r["price"]: r["size"] for r in rows if r["side"] == "BUY"}
        self.assertEqual(buys[0.08], 300.0)
        self.assertEqual(buys[0.42], 10.0)

        # NO @ 0.30 -> SELL @ 0.70 (1 - q), same contracts
        sells = {r["price"]: r["size"] for r in rows if r["side"] == "SELL"}
        self.assertIn(0.70, sells)
        self.assertEqual(sells[0.70], 5.0)

        # internal book uses integer cents keys
        self.assertEqual(b.book["BUY"][8], 300.0)
        self.assertEqual(b.book["SELL"][70], 5.0)

    def test_snapshot_resets_book(self):
        b = BookReconstructor(INSTR)
        b.on_snapshot({"yes_dollars_fp": [["0.5000", "100.00"]], "no_dollars_fp": []}, seq=1)
        self.assertIn(50, b.book["BUY"])
        # a new snapshot must clear the previous state
        b.on_snapshot({"yes_dollars_fp": [["0.6000", "20.00"]], "no_dollars_fp": []}, seq=2)
        self.assertNotIn(50, b.book["BUY"])
        self.assertIn(60, b.book["BUY"])

    def test_venue_and_contract_fields(self):
        b = BookReconstructor(INSTR)
        rows = b.on_snapshot({"yes_dollars_fp": [["0.5000", "100.00"]]}, seq=1)
        r = rows[0]
        self.assertEqual(r["venue"], "KALSHI")
        self.assertEqual(r["kind"], "delta")
        self.assertEqual(r["instrument"], INSTR)
        self.assertEqual(r["market_alias"], "")


class TestDelta(unittest.TestCase):
    def setUp(self):
        self.b = BookReconstructor(INSTR)
        self.b.on_snapshot(
            {"yes_dollars_fp": [["0.4200", "10.00"]], "no_dollars_fp": [["0.3000", "5.00"]]},
            seq=100,
        )

    def test_delta_add_new_level(self):
        rows = self.b.on_delta(
            {"price_dollars": "0.5000", "delta_fp": "7.00", "side": "yes", "ts_ms": 1700000000000},
            seq=101,
        )
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["action"], "ADD")
        self.assertEqual(rows[0]["side"], "BUY")
        self.assertEqual(rows[0]["price"], 0.50)
        self.assertEqual(rows[0]["size"], 7.0)
        self.assertEqual(rows[0]["ts_ns"], 1700000000000 * 1_000_000)

    def test_delta_update_existing(self):
        rows = self.b.on_delta(
            {"price_dollars": "0.4200", "delta_fp": "3.00", "side": "yes"}, seq=101
        )
        self.assertEqual(rows[0]["action"], "UPDATE")
        self.assertEqual(rows[0]["size"], 13.0)  # 10 + 3
        self.assertEqual(self.b.book["BUY"][42], 13.0)

    def test_delta_negative_then_delete(self):
        # reduce 10 -> 4
        rows = self.b.on_delta(
            {"price_dollars": "0.4200", "delta_fp": "-6.00", "side": "yes"}, seq=101
        )
        self.assertEqual(rows[0]["action"], "UPDATE")
        self.assertEqual(rows[0]["size"], 4.0)
        # reduce 4 -> 0 => DELETE and removal
        rows = self.b.on_delta(
            {"price_dollars": "0.4200", "delta_fp": "-4.00", "side": "yes"}, seq=102
        )
        self.assertEqual(rows[0]["action"], "DELETE")
        self.assertEqual(rows[0]["size"], 0.0)
        self.assertNotIn(42, self.b.book["BUY"])

    def test_delta_no_side_maps_to_sell_complement(self):
        # NO delta at price 0.30 -> SELL @ 0.70
        rows = self.b.on_delta(
            {"price_dollars": "0.3000", "delta_fp": "2.00", "side": "no"}, seq=101
        )
        self.assertEqual(rows[0]["side"], "SELL")
        self.assertEqual(rows[0]["price"], 0.70)
        self.assertEqual(rows[0]["size"], 7.0)  # 5 existing + 2

    def test_delta_delete_on_size_zero_absent_level(self):
        # delta that drives a never-seen level to <= 0 emits DELETE, no crash
        rows = self.b.on_delta(
            {"price_dollars": "0.9000", "delta_fp": "0.00", "side": "yes"}, seq=101
        )
        self.assertEqual(rows[0]["action"], "DELETE")
        self.assertNotIn(90, self.b.book["BUY"])


class TestSeq(unittest.TestCase):
    def test_seq_gap_detection(self):
        b = BookReconstructor(INSTR)
        b.on_snapshot({"yes_dollars_fp": [["0.5000", "1.00"]]}, seq=5)
        self.assertTrue(b.check_seq(6))   # contiguous
        b.on_delta({"price_dollars": "0.5000", "delta_fp": "1.00", "side": "yes"}, seq=6)
        self.assertTrue(b.check_seq(7))
        self.assertFalse(b.check_seq(9))  # gap (skipped 7,8)

    def test_first_seq_always_ok(self):
        b = BookReconstructor(INSTR)
        self.assertTrue(b.check_seq(12345))


class TestTrade(unittest.TestCase):
    def test_trade_field_variants_yes_price_count_taker(self):
        b = BookReconstructor(INSTR)
        row = b.trade_row(
            {"market_ticker": INSTR, "yes_price": "0.55", "count": "12",
             "taker_side": "yes", "ts": 1700000000},
        )
        self.assertEqual(row["kind"], "trade")
        self.assertEqual(row["price"], 0.55)
        self.assertEqual(row["size"], 12.0)
        self.assertEqual(row["aggressor_side"], "yes")
        self.assertEqual(row["venue"], "KALSHI")
        self.assertEqual(row["ts_ns"], 1700000000 * 1_000_000_000)  # seconds -> ns

    def test_trade_field_variants_price_size_aggressor(self):
        b = BookReconstructor(INSTR)
        row = b.trade_row(
            {"market_ticker": INSTR, "price": 0.31, "size": 4,
             "aggressor_side": "NO", "ts_ms": 1700000000000, "id": "abc"},
        )
        self.assertEqual(row["price"], 0.31)
        self.assertEqual(row["size"], 4.0)
        self.assertEqual(row["aggressor_side"], "no")
        self.assertEqual(row["trade_id"], "abc")
        self.assertEqual(row["ts_ns"], 1700000000000 * 1_000_000)  # ms -> ns

    def test_trade_recv_ns_fallback(self):
        b = BookReconstructor(INSTR)
        row = b.trade_row({"price": 0.5, "count": 1, "taker_side": "yes"}, recv_ns=999)
        self.assertEqual(row["ts_ns"], 999)


class TestFpDivisor(unittest.TestCase):
    def test_fp_divisor_scaling(self):
        # flip FP_DIVISOR to simulate integer fixed-point, confirm it scales
        old = book.FP_DIVISOR
        try:
            book.FP_DIVISOR = 100.0
            b = BookReconstructor(INSTR)
            rows = b.on_snapshot({"yes_dollars_fp": [["0.5000", "30000"]]}, seq=1)
            self.assertEqual(rows[0]["size"], 300.0)  # 30000 / 100
        finally:
            book.FP_DIVISOR = old


if __name__ == "__main__":
    unittest.main()
