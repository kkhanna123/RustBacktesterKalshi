"""replay_to_clickhouse.py — load on-disk NDJSON(.gz) into ClickHouse idempotently.

Reads the shared-contract NDJSON rows our collector wrote and batch-INSERTs them into
ClickHouse via ClickHouseSink (same row->column mapping the live collector uses).

    python replay_to_clickhouse.py --in ../data/raw --clickhouse http://localhost:8123

Idempotency: ClickHouse MergeTree tables are not natively dedup'd, so replaying the same
files twice will duplicate rows. To stay idempotent, replay only files you have not loaded
before, or TRUNCATE the target tables first. (The collector writes immutable, date-rotated
files, so the natural unit of replay is a whole file.)
"""

import argparse
import gzip
import json
import logging
import os
import sys

from sinks import ClickHouseSink

log = logging.getLogger("collector.replay")


def iter_ndjson_files(in_path):
    """Yield NDJSON(.gz) file paths under in_path (file or directory)."""
    if os.path.isfile(in_path):
        yield in_path
        return
    for root, _dirs, files in os.walk(in_path):
        for name in sorted(files):
            if name.endswith(".ndjson.gz") or name.endswith(".ndjson"):
                yield os.path.join(root, name)


def _open(path):
    if path.endswith(".gz"):
        return gzip.open(path, "rt", encoding="utf-8")
    return open(path, "r", encoding="utf-8")


def replay(in_path, clickhouse_url, batch_size=2000):
    sink = ClickHouseSink(clickhouse_url, batch_size=batch_size, flush_interval_s=1e9)
    total = 0
    for fpath in iter_ndjson_files(in_path):
        n = 0
        with _open(fpath) as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                sink.write_row(row)
                n += 1
        sink.flush()
        total += n
        log.info("replayed %d rows from %s", n, fpath)
    sink.close()
    log.info("done: %d rows total", total)
    return total


def main(argv=None):
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    ap = argparse.ArgumentParser(description="Replay NDJSON(.gz) into ClickHouse.")
    ap.add_argument("--in", dest="in_path", default="../data/raw")
    ap.add_argument("--clickhouse", required=True)
    args = ap.parse_args(argv)
    replay(args.in_path, args.clickhouse)
    return 0


if __name__ == "__main__":
    sys.exit(main())
