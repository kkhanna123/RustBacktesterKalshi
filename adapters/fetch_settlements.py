#!/usr/bin/env python3
"""Fetch Kalshi market SETTLEMENT outcomes and write a settlements CSV for the backtester.

Kalshi markets are BINARY (cash-or-nothing): at resolution a YES contract pays $1 and a NO
contract pays $0. The Rust backtester can SETTLE positions held to expiry against each market's
outcome instead of flattening them at the last mid (see `--settlements` / `[execution.settlement]`).
This script builds the `instrument_id,result` CSV that feature consumes, straight from Kalshi's
PUBLIC REST API — no auth required — so settlement is reproducible from real data.

For every instrument id (a Kalshi *ticker* like ``KXNATGASD-26JUN1517-T3.135``) it calls

    GET https://api.elections.kalshi.com/trade-api/v2/markets/{ticker}

reads ``market.result`` (``"yes"`` / ``"no"``) and ``market.status``, and writes one CSV row
``<instrument_id>,<yes|no>``. Markets that are NOT yet finalized (no/empty ``result``, e.g. still
``active``) are SKIPPED with a note, so re-running later picks them up once they settle.

Instrument ids come from one of three mutually-exclusive sources:
  * ``--instruments a,b,c``                      an explicit comma-separated list
  * ``--ndjson <file[.gz]>``                     distinct ``instrument`` values in a tick capture
  * ``--from-clickhouse http://localhost:8123``  ``SELECT DISTINCT instrument_id`` from ClickHouse

Robustness: each request is retried (exponential backoff) on transient HTTP/network errors;
unknown / 404 tickers and not-yet-final markets are skipped (never abort the whole run).

Examples
--------
    # From an explicit list, write to settlements.csv:
    python fetch_settlements.py --instruments KXNATGASD-26JUN1517-T3.135,KXNATGASD-26JUN1517-T3.5 \
        --out ../data/settlements.csv

    # Discover instruments from a tick capture and fetch all their outcomes:
    python fetch_settlements.py --ndjson ../data/tick/natgas_tick_demo.ndjson.gz \
        --out ../data/settlements.csv

    # Discover instruments from the local ClickHouse warehouse:
    python fetch_settlements.py --from-clickhouse http://localhost:8123 \
        --instrument-like 'KXNATGASD-%' --out ../data/settlements.csv

Then run the backtest with settlement:
    kalshi-backtest backtest --source ndjson --ndjson ../data/tick/natgas_tick_demo.ndjson.gz \
        --strategy imbalance --settlements ../data/settlements.csv
"""

from __future__ import annotations

import argparse
import csv
import gzip
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Iterable, Optional

# Kalshi's public, unauthenticated market endpoint (elections/trade-api v2).
KALSHI_API_BASE = "https://api.elections.kalshi.com/trade-api/v2"


# ---------------------------------------------------------------------------
# Instrument-id collection (three sources)
# ---------------------------------------------------------------------------
def instruments_from_list(raw: str) -> list[str]:
    """Parse a comma-separated ``--instruments`` value into a de-duplicated, ordered list."""
    seen: dict[str, None] = {}
    for part in raw.split(","):
        t = part.strip()
        if t:
            seen.setdefault(t, None)
    return list(seen.keys())


def instruments_from_ndjson(path: str) -> list[str]:
    """Collect the distinct ``instrument`` (or ``instrument_id``) values from an NDJSON(.gz) capture.

    Each line is a JSON object (an orderbook delta or trade) carrying an instrument field; lines
    that don't parse or lack the field are skipped. Order of first appearance is preserved.
    """
    opener = gzip.open if path.endswith(".gz") else open
    seen: dict[str, None] = {}
    with opener(path, "rt", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            inst = obj.get("instrument") or obj.get("instrument_id")
            if isinstance(inst, str) and inst:
                seen.setdefault(inst, None)
    return list(seen.keys())


def instruments_from_clickhouse(
    url: str, instrument_like: str, database: str, table: str
) -> list[str]:
    """``SELECT DISTINCT instrument_id`` from the ClickHouse orderbook-deltas table over HTTP.

    ``instrument_like`` is a SQL ``LIKE`` pattern (default ``%`` = all). The query is sent to the
    ClickHouse HTTP interface; results come back one id per line (``FORMAT TabSeparated``).
    """
    safe_like = instrument_like.replace("'", "''")
    query = (
        f"SELECT DISTINCT instrument_id FROM {database}.{table} "
        f"WHERE instrument_id LIKE '{safe_like}' ORDER BY instrument_id FORMAT TabSeparated"
    )
    full = url.rstrip("/") + "/?" + urllib.parse.urlencode({"query": query})
    req = urllib.request.Request(full)
    with urllib.request.urlopen(req, timeout=30) as resp:  # noqa: S310 (trusted local URL)
        body = resp.read().decode("utf-8")
    return [line.strip() for line in body.splitlines() if line.strip()]


# ---------------------------------------------------------------------------
# Kalshi REST: fetch one market's result
# ---------------------------------------------------------------------------
def fetch_market(ticker: str, retries: int, backoff: float, timeout: float) -> Optional[dict]:
    """GET one market by ticker, returning the ``market`` object, or ``None`` if it can't be fetched.

    A 404 (unknown ticker) returns ``None`` immediately (no point retrying). Transient errors
    (5xx, timeouts, connection resets) are retried with exponential backoff up to ``retries`` times.
    """
    url = f"{KALSHI_API_BASE}/markets/{urllib.parse.quote(ticker, safe='')}"
    attempt = 0
    while True:
        try:
            req = urllib.request.Request(url, headers={"Accept": "application/json"})
            with urllib.request.urlopen(req, timeout=timeout) as resp:  # noqa: S310
                payload = json.loads(resp.read().decode("utf-8"))
            return payload.get("market")
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None  # unknown ticker — skip, don't retry
            if e.code < 500 and e.code != 429:
                # Other 4xx (bad request, auth) won't fix themselves; give up on this ticker.
                sys.stderr.write(f"  [warn] {ticker}: HTTP {e.code}, skipping\n")
                return None
            # 429 / 5xx -> retry
        except (urllib.error.URLError, TimeoutError, ConnectionError, json.JSONDecodeError):
            pass  # transient -> retry
        attempt += 1
        if attempt > retries:
            sys.stderr.write(f"  [warn] {ticker}: gave up after {retries} retries\n")
            return None
        time.sleep(backoff * (2 ** (attempt - 1)))


def normalize_result(market: dict) -> Optional[str]:
    """Map a Kalshi market object to ``"yes"``/``"no"``, or ``None`` if not yet finalized.

    Kalshi reports the binary outcome in ``market.result`` once the market settles. A finalized
    market has ``status`` in {``settled``, ``finalized``, ``closed``} AND a ``result`` of
    ``yes``/``no``. Anything else (``active``, empty result) is treated as not-yet-final => skip.
    """
    result = str(market.get("result") or "").strip().lower()
    if result in ("yes", "no"):
        return result
    return None


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
def collect_instruments(args: argparse.Namespace) -> list[str]:
    """Resolve the instrument-id list from whichever single source flag was given."""
    sources = [bool(args.instruments), bool(args.ndjson), bool(args.from_clickhouse)]
    if sum(sources) != 1:
        raise SystemExit(
            "error: pass EXACTLY ONE of --instruments, --ndjson, or --from-clickhouse"
        )
    if args.instruments:
        return instruments_from_list(args.instruments)
    if args.ndjson:
        return instruments_from_ndjson(args.ndjson)
    return instruments_from_clickhouse(
        args.from_clickhouse, args.instrument_like, args.ch_database, args.ch_table
    )


def write_settlements_csv(path: str, rows: Iterable[tuple[str, str]]) -> int:
    """Write ``instrument_id,result`` rows (with a header) to ``path``; returns the row count."""
    n = 0
    with open(path, "w", newline="", encoding="utf-8") as fh:
        writer = csv.writer(fh)
        writer.writerow(["instrument_id", "result"])
        for instrument_id, result in rows:
            writer.writerow([instrument_id, result])
            n += 1
    return n


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        description="Fetch Kalshi market settlement outcomes into a settlements CSV "
        "(instrument_id,result) for the tick-level backtester's --settlements flag.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    src = p.add_argument_group("instrument source (choose exactly one)")
    src.add_argument(
        "--instruments",
        help="comma-separated instrument ids / Kalshi tickers (e.g. 'TICKER_A,TICKER_B')",
    )
    src.add_argument(
        "--ndjson",
        help="NDJSON(.gz) tick capture; distinct `instrument` values are used",
    )
    src.add_argument(
        "--from-clickhouse",
        metavar="URL",
        help="ClickHouse HTTP base url (e.g. http://localhost:8123); SELECT DISTINCT instrument_id",
    )
    p.add_argument(
        "--instrument-like",
        default="%",
        help="SQL LIKE filter for --from-clickhouse (default: %% = all)",
    )
    p.add_argument(
        "--ch-database", default="kalshi", help="ClickHouse database (default: kalshi)"
    )
    p.add_argument(
        "--ch-table",
        default="orderbook_deltas",
        help="ClickHouse table with instrument_id (default: orderbook_deltas)",
    )
    p.add_argument(
        "--out",
        "-o",
        default="settlements.csv",
        help="output CSV path (default: settlements.csv)",
    )
    p.add_argument("--retries", type=int, default=4, help="max retries per market (default: 4)")
    p.add_argument(
        "--backoff",
        type=float,
        default=0.5,
        help="initial retry backoff seconds, doubled each retry (default: 0.5)",
    )
    p.add_argument(
        "--timeout", type=float, default=15.0, help="per-request timeout seconds (default: 15)"
    )
    p.add_argument(
        "--sleep",
        type=float,
        default=0.05,
        help="polite delay between requests in seconds (default: 0.05)",
    )
    return p


def main(argv: Optional[list[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    instruments = collect_instruments(args)
    if not instruments:
        sys.stderr.write("No instruments found from the chosen source.\n")
        return 1

    sys.stderr.write(f"Fetching settlement outcomes for {len(instruments)} instrument(s)...\n")
    settled: list[tuple[str, str]] = []
    skipped_unfinal = 0
    skipped_missing = 0
    for i, inst in enumerate(instruments, 1):
        market = fetch_market(inst, args.retries, args.backoff, args.timeout)
        if market is None:
            skipped_missing += 1
        else:
            result = normalize_result(market)
            if result is None:
                status = str(market.get("status") or "?")
                sys.stderr.write(f"  [skip] {inst}: not finalized (status={status})\n")
                skipped_unfinal += 1
            else:
                settled.append((inst, result))
        if args.sleep > 0 and i < len(instruments):
            time.sleep(args.sleep)

    n = write_settlements_csv(args.out, settled)
    sys.stderr.write(
        f"Wrote {n} settled market(s) to {args.out} "
        f"(skipped {skipped_unfinal} not-finalized, {skipped_missing} unknown/unreachable).\n"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
