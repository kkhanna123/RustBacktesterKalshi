# Kalshi WebSocket orderbook-delta collector

Subscribes to Kalshi's WebSocket `orderbook_delta` + `trade` channels for natural-gas
(`KXNATGASD`) markets, reconstructs rows matching the infra-orchestrator data contract, and
writes them to **both**:

1. **Durable on-disk NDJSON.gz** (always) — `data/raw/<YYYY-MM-DD>/kalshi_<ts>.ndjson.gz`
2. **ClickHouse** `kalshi.orderbook_deltas` / `kalshi.trades` (when `--clickhouse <url>` is
   given and the server is reachable; if it's down we log and keep the NDJSON).

The NDJSON rows are the **shared contract** the Rust backtester reads, and the ClickHouse
columns mirror `clickhouse/schema/01_tables.sql` (itself a mirror of the infra-orchestrator
QuestDB schema).

## Files

| file | purpose |
|---|---|
| `book.py` | **PURE** snapshot/delta/trade → row reconstruction. No network. Fully unit-tested. |
| `kalshi_auth.py` | RSA-PSS request signing (the 3 `KALSHI-ACCESS-*` headers). |
| `natgas_markets.py` | Discover open `KXNATGASD` market tickers via public REST. |
| `sinks.py` | `NdjsonSink` (gzip, date-rotated), `ClickHouseSink` (HTTP, best-effort), `FanoutSink`. |
| `kalshi_ws_collector.py` | Runnable entrypoint: auth → subscribe → route → sinks, with reconnect/backoff + seq-gap resubscribe + SIGINT flush. |
| `replay_to_clickhouse.py` | Load on-disk NDJSON(.gz) into ClickHouse. |
| `parquet_to_ndjson.py` | Convert infra-schema parquet (QuestDB/Dome exports) into the shared NDJSON contract. |
| `tests/` | `unittest` suite for `book.py` and `kalshi_auth.py`. |

## Setup

```bash
pip install -r requirements.txt      # requests, websocket-client==1.8.0, cryptography, pandas, pyarrow

export KALSHI_API_KEY_ID=...                       # your Kalshi API key id
export KALSHI_PRIVATE_KEY=/path/to/kalshi_key.pem  # RSA private key (PEM path)
```

## Run

```bash
# discover open natgas markets, stream to disk + ClickHouse:
python kalshi_ws_collector.py --series KXNATGASD --out ../data/raw \
    --clickhouse http://localhost:8123

# explicit markets, demo endpoint, no ClickHouse:
python kalshi_ws_collector.py --markets KXNATGASD-26APR0817-T2.650,KXNATGASD-26APR0817-T2.700 --demo

# replay captured NDJSON into ClickHouse:
python replay_to_clickhouse.py --in ../data/raw --clickhouse http://localhost:8123

# convert exported parquet into the shared NDJSON contract:
python parquet_to_ndjson.py --deltas deltas.parquet --trades trades.parquet --out out.ndjson.gz
```

If `KALSHI_API_KEY_ID` / `KALSHI_PRIVATE_KEY` are unset the collector prints a clear message
and exits **2** (it does not crash). Run it continuously (tmux / nohup / launchd) for the
month you want to capture — Kalshi serves L2 deltas only on the live WS, going forward.

## Tests

```bash
python3 -m unittest discover -s tests -v
```

Covers: snapshot reset + yes/no→BUY/SELL(complement) mapping, delta ADD/UPDATE/DELETE,
delete on size→0, seq-gap detection, trade field-name variants, the `FP_DIVISOR` knob, and
the RSA-PSS signature round-trip (ephemeral key, verified with the public key).

## Schema mapping (NDJSON → ClickHouse)

Delta row → `kalshi.orderbook_deltas`:
`ts_ns`→`timestamp` (ISO ns), `instrument`→`instrument_id`, `venue`, `action`, `side`,
`price`, `size`, `market_alias`, `sequence`, `is_snapshot`.

Trade row → `kalshi.trades`:
`ts_ns`→`ts_event`, `instrument`→`instrument_id`, `venue`, `aggressor_side`, `price`,
`size`, `market_alias`, `trade_id`.

## YES-native book mapping (critical)

The book is stored as a single YES-priced two-sided book:

- **YES** level at price `p`, `q` contracts → book side **BUY** at price `p`.
- **NO** resting bid at price `q` → equivalent YES **ask** at `1 - q` → side **SELL**,
  price `round(1 - q, 2)`, same contracts. (A buyer of NO at q is a seller of YES at 1-q.)

Prices are stored internally as integer cents (1..99) for exact keying and emitted as float
dollars rounded to 2 decimals; the downstream Rust converts to integer cents.

## FP / field-encoding caveats (things we had to guess — easy to flip)

All parsing is centralized in `book.py` so a single edit re-tunes it:

- **`FP_DIVISOR = 1.0`** — per the spec the snapshot/delta numbers are plain decimal strings
  (`"300.00"` = 300 contracts), so no scaling. If Kalshi actually uses integer fixed-point
  (e.g. `÷100`), set `FP_DIVISOR = 100.0` (or pass `--fp-divisor 100`). A unit test exercises
  the scaling so you can confirm the flip.
- **Prices** arrive as decimal dollar strings (`"0.0800"` = $0.08); parsed with
  `parse_price_dollars` (accepts string **or** number, rounds to 2 dp).
- **Trade field names vary** — we accept `yes_price`/`price`, `count`/`size`,
  `taker_side`/`aggressor_side`, `ts`(seconds)/`ts_ms`(millis), `trade_id`/`id`.
- **Timestamps** — deltas use `ts_ms*1e6` when present, else the receive-time ns; trades
  heuristically treat values `< 1e12` as unix seconds and larger as millis. `aggressor_side`
  is lower-cased to match the ClickHouse `Enum8('yes','no')`.
```
