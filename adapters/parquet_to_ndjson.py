"""parquet_to_ndjson.py — convert infra-schema parquet into the shared NDJSON contract.

Lets the Rust backtester read exported QuestDB / Dome data. Reads orderbook_deltas and/or
trades parquet whose columns follow the infra-orchestrator schema and emits NDJSON rows
identical to those book.BookReconstructor produces.

    python parquet_to_ndjson.py --deltas deltas.parquet --trades trades.parquet \
        --out out.ndjson.gz

Deltas parquet columns: timestamp, instrument_id, venue, action, side, price, size,
    market_alias, sequence, is_snapshot
Trades parquet columns:  ts_event (or timestamp), instrument_id, venue, aggressor_side,
    price, size, market_alias, trade_id
"""

import argparse
import gzip
import json
import logging
import sys

import pandas as pd

log = logging.getLogger("collector.pq2ndjson")


def _ts_to_ns(val):
    """Convert a pandas/py timestamp value to int nanoseconds since epoch."""
    ts = pd.Timestamp(val)
    if ts.tz is not None:
        ts = ts.tz_convert("UTC").tz_localize(None)
    return int(ts.value)  # pandas Timestamp.value is ns since epoch (UTC)


def delta_rows(df):
    for r in df.itertuples(index=False):
        d = r._asdict()
        yield {
            "kind": "delta",
            "ts_ns": _ts_to_ns(d.get("timestamp")),
            "instrument": d.get("instrument_id"),
            "action": d.get("action"),
            "side": d.get("side"),
            "price": float(d.get("price")),
            "size": float(d.get("size")),
            "sequence": int(d.get("sequence", 0) or 0),
            "is_snapshot": int(d.get("is_snapshot", 0) or 0),
            "venue": d.get("venue", "KALSHI"),
            "market_alias": d.get("market_alias", "") or "",
        }


def trade_rows(df):
    for r in df.itertuples(index=False):
        d = r._asdict()
        ts = d.get("ts_event", d.get("timestamp"))
        yield {
            "kind": "trade",
            "ts_ns": _ts_to_ns(ts),
            "instrument": d.get("instrument_id"),
            "aggressor_side": d.get("aggressor_side"),
            "price": float(d.get("price")),
            "size": float(d.get("size")),
            "trade_id": str(d.get("trade_id", "") or ""),
            "venue": d.get("venue", "KALSHI"),
        }


def _open_out(path):
    if path.endswith(".gz"):
        return gzip.open(path, "wt", encoding="utf-8")
    return open(path, "w", encoding="utf-8")


def convert(deltas_pq=None, trades_pq=None, out_path="out.ndjson.gz"):
    n = 0
    with _open_out(out_path) as fh:
        if deltas_pq:
            df = pd.read_parquet(deltas_pq)
            for row in delta_rows(df):
                fh.write(json.dumps(row, separators=(",", ":")) + "\n")
                n += 1
        if trades_pq:
            df = pd.read_parquet(trades_pq)
            for row in trade_rows(df):
                fh.write(json.dumps(row, separators=(",", ":")) + "\n")
                n += 1
    log.info("wrote %d rows to %s", n, out_path)
    return n


def main(argv=None):
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    ap = argparse.ArgumentParser(description="Convert infra-schema parquet to shared NDJSON.")
    ap.add_argument("--deltas", default=None)
    ap.add_argument("--trades", default=None)
    ap.add_argument("--out", required=True)
    args = ap.parse_args(argv)
    if not args.deltas and not args.trades:
        print("ERROR: provide at least one of --deltas / --trades", file=sys.stderr)
        return 2
    convert(args.deltas, args.trades, args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
