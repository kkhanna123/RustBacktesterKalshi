"""Unit tests for the conversion helpers and pipeline in to_canonical.py.

Run with the repo venv:
    .venv/bin/python -m unittest discover -s tools/tests -v
"""

import gzip
import io
import json
import os
import sys
import tempfile
import unittest

# Make the parent tools/ dir importable.
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from to_canonical import (  # noqa: E402
    CanonicalWriter,
    DeltaEvent,
    IngestProfile,
    SnapshotDiffer,
    TradeEvent,
    ValidationError,
    convert,
    flip_side,
    infer_profile,
    iter_events,
    no_to_yes,
    to_aggressor,
    to_dollars,
    to_ns,
    to_side,
)


class TestToNs(unittest.TestCase):
    def test_units(self):
        self.assertEqual(to_ns(1, "s"), 1_000_000_000)
        self.assertEqual(to_ns(1, "ms"), 1_000_000)
        self.assertEqual(to_ns(1, "us"), 1_000)
        self.assertEqual(to_ns(5, "ns"), 5)

    def test_numeric_string(self):
        self.assertEqual(to_ns("1700000000000", "ms"), 1_700_000_000_000_000_000)

    def test_iso(self):
        self.assertEqual(to_ns("2023-11-14T22:13:20Z", "iso"), 1_700_000_000_000_000_000)

    def test_iso_custom_fmt(self):
        got = to_ns("2023-11-14 22:13:20", "iso", fmt="%Y-%m-%d %H:%M:%S")
        self.assertEqual(got, 1_700_000_000_000_000_000)

    def test_bad_unit(self):
        with self.assertRaises(ValueError):
            to_ns(1, "minutes")


class TestToDollars(unittest.TestCase):
    def test_scales(self):
        self.assertAlmostEqual(to_dollars(42, "cents"), 0.42)
        self.assertAlmostEqual(to_dollars(4200, "bps"), 0.42)
        self.assertAlmostEqual(to_dollars(0.42, "dollars"), 0.42)
        self.assertAlmostEqual(to_dollars(0.42, "prob"), 0.42)

    def test_custom_factor(self):
        self.assertAlmostEqual(to_dollars(7, factor=0.1), 0.7)

    def test_string(self):
        self.assertAlmostEqual(to_dollars("55", "cents"), 0.55)


class TestSides(unittest.TestCase):
    def test_to_side(self):
        for tok in ["yes", "bid", "buy", "B", "+", "1", "TRUE"]:
            self.assertEqual(to_side(tok), "BUY", tok)
        for tok in ["no", "ask", "sell", "S", "A", "-", "0"]:
            self.assertEqual(to_side(tok), "SELL", tok)

    def test_bad_side(self):
        with self.assertRaises(ValueError):
            to_side("sideways")

    def test_aggressor(self):
        self.assertEqual(to_aggressor("buy"), "yes")
        self.assertEqual(to_aggressor("no"), "no")

    def test_no_to_yes(self):
        self.assertAlmostEqual(no_to_yes(0.3), 0.7)
        self.assertEqual(flip_side("BUY"), "SELL")
        self.assertEqual(flip_side("SELL"), "BUY")


class TestWriter(unittest.TestCase):
    def test_writes_and_validates(self):
        buf = io.StringIO()
        w = CanonicalWriter(fileobj=buf)
        w.write_delta(DeltaEvent(1, "X", "ADD", "BUY", 0.42, 100, sequence=1, is_snapshot=1))
        w.write_trade(TradeEvent(2, "X", "no", 0.55, 3, "t1"))
        lines = [json.loads(l) for l in buf.getvalue().splitlines()]
        self.assertEqual(lines[0]["kind"], "delta")
        self.assertEqual(lines[0]["side"], "BUY")
        self.assertEqual(lines[0]["is_snapshot"], 1)
        self.assertEqual(lines[0]["market_alias"], "")
        self.assertEqual(lines[1]["kind"], "trade")
        self.assertEqual(lines[1]["aggressor_side"], "no")
        self.assertEqual(w.stats["deltas"], 1)
        self.assertEqual(w.stats["trades"], 1)
        self.assertEqual(w.stats["snapshots"], 1)

    def test_price_out_of_range_strict(self):
        w = CanonicalWriter(fileobj=io.StringIO())
        with self.assertRaises(ValidationError):
            w.write_delta(DeltaEvent(1, "X", "ADD", "BUY", 42, 100))  # 42 dollars -> invalid

    def test_price_out_of_range_lenient(self):
        w = CanonicalWriter(fileobj=io.StringIO(), strict_price=False, warn=False)
        w.write_delta(DeltaEvent(1, "X", "ADD", "BUY", 42, 100))
        self.assertEqual(w.stats["deltas"], 1)
        self.assertEqual(w.stats["warnings"], 1)

    def test_bad_action(self):
        w = CanonicalWriter(fileobj=io.StringIO())
        with self.assertRaises(ValidationError):
            w.write_delta(DeltaEvent(1, "X", "FOO", "BUY", 0.4, 1))

    def test_non_monotonic_counted(self):
        w = CanonicalWriter(fileobj=io.StringIO(), warn=False)
        w.write_delta(DeltaEvent(10, "X", "ADD", "BUY", 0.4, 1))
        w.write_delta(DeltaEvent(5, "X", "ADD", "BUY", 0.4, 1))
        self.assertEqual(w.stats["non_monotonic"], 1)

    def test_gzip_roundtrip(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "o.ndjson.gz")
            with CanonicalWriter(p) as w:
                w.write_delta(DeltaEvent(1, "X", "ADD", "BUY", 0.42, 100))
            with gzip.open(p, "rt") as fh:
                obj = json.loads(fh.readline())
            self.assertEqual(obj["price"], 0.42)


class TestSnapshotDiffer(unittest.TestCase):
    def test_first_snapshot_all_adds(self):
        d = SnapshotDiffer()
        out = d.diff_snapshot("X", 1, [("bid", 0.4, 100), ("ask", 0.5, 200)])
        self.assertEqual(len(out), 2)
        self.assertTrue(all(e.is_snapshot == 1 and e.action == "ADD" for e in out))

    def test_update_add_delete(self):
        d = SnapshotDiffer()
        d.diff_snapshot("X", 1, [("bid", 0.4, 100), ("ask", 0.5, 200)])
        out = d.diff_snapshot("X", 2, [("bid", 0.4, 150), ("bid", 0.39, 50)])
        actions = {(e.action, e.side, e.price): e.size for e in out}
        self.assertEqual(actions[("UPDATE", "BUY", 0.4)], 150)  # size changed
        self.assertEqual(actions[("ADD", "BUY", 0.39)], 50)     # new level
        self.assertEqual(actions[("DELETE", "SELL", 0.5)], 0.0)  # vanished ask
        self.assertTrue(all(e.is_snapshot == 0 for e in out))


class TestProfileAndConvert(unittest.TestCase):
    EX = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "examples")
    PROF = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "profiles")

    def test_profile_from_dict_rejects_unknown(self):
        with self.assertRaises(ValueError):
            IngestProfile.from_dict({"bogus_key": 1})

    def test_no_side_complement(self):
        # A NO bid at 0.30 should become a YES ask (SELL) at 0.70.
        prof = IngestProfile(kind="delta", instrument_const="X", ts_col="t", ts_unit="ns",
                             price_col="p", price_scale="prob", size_const=1.0,
                             side_const="bid", no_side=True)
        import pandas as pd
        df = pd.DataFrame({"t": [1], "p": [0.30]})
        evs = list(iter_events(df, prof))
        self.assertEqual(evs[0].side, "SELL")
        self.assertAlmostEqual(evs[0].price, 0.70)

    def test_convert_l2_delta(self):
        prof = IngestProfile.from_file(os.path.join(self.PROF, "l2_delta_csv.json"))
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "o.ndjson.gz")
            stats = convert(os.path.join(self.EX, "l2_delta.csv"), prof, out, warn=False)
            self.assertGreater(stats["deltas"], 100)
            self.assertEqual(stats["trades"], 0)
            self.assertEqual(stats["snapshots"], 6)

    def test_convert_trades(self):
        prof = IngestProfile.from_file(os.path.join(self.PROF, "trades_csv.json"))
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "o.ndjson")
            stats = convert(os.path.join(self.EX, "trades.csv"), prof, out, warn=False)
            self.assertEqual(stats["trades"], 50)
            self.assertEqual(stats["deltas"], 0)

    def test_convert_snapshot(self):
        prof = IngestProfile.from_file(os.path.join(self.PROF, "snapshot_feed.json"))
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "o.ndjson")
            stats = convert(os.path.join(self.EX, "snapshot_feed.csv"), prof, out, warn=False)
            self.assertGreater(stats["deltas"], 0)
            self.assertGreater(stats["snapshots"], 0)  # first snapshot's levels

    def test_infer_delta(self):
        prof = infer_profile(["ts_ms", "symbol", "side", "price_cents", "size", "seq"])
        self.assertEqual(prof.kind, "delta")
        self.assertEqual(prof.ts_col, "ts_ms")
        self.assertEqual(prof.ts_unit, "ms")
        self.assertEqual(prof.price_scale, "cents")
        self.assertEqual(prof.side_col, "side")

    def test_infer_trade(self):
        prof = infer_profile(["time", "ticker", "taker_side", "price", "qty", "trade_id"])
        self.assertEqual(prof.kind, "trade")
        self.assertEqual(prof.aggressor_col, "taker_side")


if __name__ == "__main__":
    unittest.main()
