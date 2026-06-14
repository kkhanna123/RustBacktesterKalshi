# `feeds/` — a multi-venue live-tick collector framework

This package generalizes the (originally Kalshi-only) collector so that **adding a new
live tick source is one small class**. Every venue normalizes its raw messages into the
**same canonical NDJSON rows** the Kalshi collector already emits (see `../book.py`), and
is driven by one venue-agnostic loop that fans rows out to the **existing sinks**
(`../sinks.py`: `NdjsonSink`, `ClickHouseSink`, `FanoutSink`).

Nothing here modifies the existing collector files — `KalshiFeed` *reuses*
`book.BookReconstructor`, `kalshi_auth`, and `natgas_markets` by import.

## Layout

| File | Purpose |
|------|---------|
| `base.py` | The `Feed` contract + the generic `run_feed(feed, sink, ...)` driver (connect / subscribe / recv→normalize→sink, reconnect-with-backoff, graceful SIGINT). |
| `kalshi.py` | `KalshiFeed` — reference impl over the real, working Kalshi WS. Reuses the existing modules; produces rows identical to `kalshi_ws_collector.py`. |
| `polymarket.py` | `PolymarketFeed` — **stub** live WS + tested `normalize` (CLOB binary tokens → YES-native, NO→YES complement) + working file replay. |
| `hyperliquid.py` | `HyperliquidFeed` — **stub** live WS + tested `normalize` (L2 book → native price passthrough, **prices not in [0,1]**) + working file replay. |
| `run_feed.py` | CLI: resolve a venue, wire sinks, run. |
| `_replay.py` | `FileReplaySource` — replay a local NDJSON/CSV file through the same `normalize` path (no creds needed). |
| `tests/` | `unittest` tests for the `run_feed` routing and each stub's `normalize`. |

## The canonical row contract

`normalize(raw)` yields plain dicts in one of two shapes — exactly what the sinks accept.

**Delta** (`kind == "delta"`):
`ts_ns`, `instrument`, `action` (`ADD`/`UPDATE`/`DELETE`), `side` (`BUY`/`SELL`),
`price` (float), `size` (float), `sequence` (int), `is_snapshot` (0/1), `venue`,
`market_alias`.

**Trade** (`kind == "trade"`):
`ts_ns`, `instrument`, `aggressor_side`, `price`, `size`, `trade_id`, `venue`.

For binary venues (Kalshi, Polymarket) `price` is a probability in `[0, 1]` and the book
is **YES-native** (a NO level at `p` becomes a YES level at `1 - p`). For Hyperliquid,
`price` is the **absolute asset price** (not `[0, 1]`); bids→`BUY`, asks→`SELL` pass
through unchanged.

## Add a venue in 3 steps

1. **Subclass `Feed`** in a new `feeds/<venue>.py`, set `name` and `venue`.
2. **Implement the 5 methods**: `discover_markets()` (instruments to subscribe to),
   `connect()` (open the WS/source), `subscribe(markets)`, `recv()` (return one raw
   message; return `None` / raise `StopFeed` at end), and `normalize(raw)` (yield
   canonical rows — pure, no sinks/network). `close()` is optional.
3. **Register it** in `run_feed.REGISTRY` with a one-line factory.

That's it — `run_feed` handles the connection lifecycle, reconnect/backoff, graceful
shutdown, and sink routing. To also support offline testing, compose `_replay.FileReplaySource`
(see the Polymarket/Hyperliquid stubs).

## Running

```bash
cd adapters

# Live Kalshi (needs creds in the env):
export KALSHI_API_KEY_ID=...
export KALSHI_PRIVATE_KEY=/path/to/kalshi_key.pem
../.venv/bin/python -m feeds.run_feed --venue kalshi --series KXNATGASD \
    --out ../data/raw --clickhouse http://localhost:8123

# Any stub venue WITHOUT live creds, via --replay:
../.venv/bin/python -m feeds.run_feed --venue polymarket --replay sample.ndjson --out /tmp/feedtest
```

`--replay <file>` reads a local NDJSON/CSV file through the exact same `normalize` path
as the live source, so you can build and test a venue with **no live access**. A finite
replay runs one session and stops, flushing the sinks.

## Tests

```bash
cd adapters && ../.venv/bin/python -m unittest discover -s feeds/tests
```
