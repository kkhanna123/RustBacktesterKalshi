"""natgas_markets.py — discover live Kalshi KXNATGASD market tickers via public REST.

Uses the public, unauthenticated endpoint
    GET /trade-api/v2/markets?series_ticker=KXNATGASD&status=open
following the ``cursor`` pagination field. Returns the list of market tickers.
"""

import requests

DEFAULT_HOST = "api.elections.kalshi.com"
DEMO_HOST = "demo-api.elections.kalshi.com"


def list_open_markets(series="KXNATGASD", host=DEFAULT_HOST, status="open",
                      session=None, limit=1000, timeout=30):
    """Return a list of open market tickers for ``series`` on ``host``.

    Follows the REST cursor pagination until exhausted.
    """
    sess = session or requests.Session()
    base = f"https://{host}/trade-api/v2/markets"
    tickers = []
    cursor = None
    while True:
        params = {"series_ticker": series, "status": status, "limit": limit}
        if cursor:
            params["cursor"] = cursor
        resp = sess.get(base, params=params, timeout=timeout)
        resp.raise_for_status()
        data = resp.json()
        for m in data.get("markets", []) or []:
            t = m.get("ticker")
            if t:
                tickers.append(t)
        cursor = data.get("cursor") or None
        if not cursor:
            break
    return tickers


def host_for(demo=False):
    return DEMO_HOST if demo else DEFAULT_HOST


if __name__ == "__main__":
    import argparse
    import json

    ap = argparse.ArgumentParser(description="List open Kalshi markets for a series.")
    ap.add_argument("--series", default="KXNATGASD")
    ap.add_argument("--demo", action="store_true")
    args = ap.parse_args()
    print(json.dumps(list_open_markets(series=args.series, host=host_for(args.demo)), indent=2))
