# How to get every orderbook delta for every tick — Kalshi NatGas, a whole month

This answers the original question two ways: (A) inside the **infra-orchestrator research-jupyter** environment
(if you have cluster access), and (B) the **self-contained local path** in this repo (no cluster needed). It also
states plainly what is and isn't physically possible.

---

## The one thing you must internalize first

**Kalshi does not serve historical orderbook deltas over REST.** The historical REST API returns only 1-minute
candlesticks (best YES bid/ask OHLC + volume + OI) — that's the ceiling of `~/kalshi_commodities/collect_kalshi.py`.
Per-event L2 deltas exist **only on the live WebSocket** (`orderbook_snapshot` then `orderbook_delta`). So "every
delta for every tick over a month" is obtainable only if **something was subscribed to that WS during that month**.

Three fidelities, pick based on what you actually have access to:

| What you get | Source | Granularity | Covers a *past* month? |
|---|---|---|---|
| **Every tick delta** | a live WS collector running that month → QuestDB/ClickHouse | per-event | only if it was running then |
| Periodic full-book snapshots | Dome API backfill (needs `DOME_API_KEY_*`) | seconds–minutes | yes, if Dome has it |
| 1-min candlesticks | Kalshi historical REST (public) | 1 minute | yes (~2 months back) |

---

## A. Inside infra-orchestrator research-jupyter (cluster access required)

### A0. Make sure a collector is/was running for natgas
Deltas only land in QuestDB if the live collector streamed them. Start one (Discord bot):
```
/collect target:KXNATGASD source:kalshi mode:live scope:series
```
This deploys `infra-trader-run-collector --plugin kalshi --sink questdb`, which subscribes to the natgas markets'
`orderbook_delta` + `trade` WS channels and ILP-writes into the `book_deltas_v1` / `orderbook_deltas` and
`trades_v1` / `trades` tables. It only captures **from now on**. For a *past* month, instead backfill snapshots:
```
/backfill target:<KXNATGASD-...-Tx.xxx market ticker> source:kalshi scope:market start:2026-05-01 end:2026-06-01
```
(Kalshi backfill is per-**market**; loop it over every strike market in the month. Note: snapshot-granular, not
every native delta.)

### A1. Open research-jupyter
Browser → `https://jupyter.oracletrading.org` (GitHub OAuth, org `oracletrading`). You land on
`/lab/tree/oracle-research-examples`. Every notebook pod already has `QUESTDB_*` env wired to
`questdb.data.svc.cluster.local` (pgwire :8812). Locally instead: `make port8888` (Jupyter) and `make port9000`
(QuestDB) from the infra repo.

### A2. Pull every delta for a month into a DataFrame
The shipped helper does exactly this — full L2 deltas with `sequence` and `is_snapshot`:
```python
import questdb_backtest as qb   # /opt/oracle/examples on the notebook path

# discover which natgas markets exist in the store
qb.list_orderbook_instruments(venue="KALSHI", limit=200)  # filter rows starting "KXNATGASD"

# every delta for ONE market over the month (UTC, end-exclusive):
deltas = qb.load_orderbook_deltas(
    instrument_id="KXNATGASD-26MAY0117-T2.650",
    venue="KALSHI",
    start="2026-05-01T00:00:00Z",
    end="2026-06-01T00:00:00Z",
)
# columns: ts_event, instrument_id, venue, action, side, price, size, market_alias, sequence, is_snapshot

# every market for the whole series — loop:
import pandas as pd
ids = qb.list_orderbook_instruments(venue="KALSHI", limit=10000)
natgas_ids = [i for i in ids["instrument_id"] if i.startswith("KXNATGASD")]
month = pd.concat(
    qb.load_orderbook_deltas(instrument_id=i, venue="KALSHI",
                             start="2026-05-01T00:00:00Z", end="2026-06-01T00:00:00Z")
    for i in natgas_ids
)
month.to_parquet("natgas_2026-05_deltas.parquet")   # hand this to the Rust backtester
```
Raw SQL if you prefer (pgwire :8812 or REST `/exec` :9000):
```sql
SELECT timestamp AS ts_event, instrument_id, venue, action, side, price, size,
       market_alias, sequence, is_snapshot
FROM orderbook_deltas
WHERE venue='KALSHI' AND instrument_id LIKE 'KXNATGASD-%'
  AND timestamp >= '2026-05-01T00:00:00.000000Z'
  AND timestamp <  '2026-06-01T00:00:00.000000Z'
ORDER BY instrument_id, timestamp, sequence;
```

### A3. Reconstruct the book / features (optional, in-notebook)
```python
tob   = qb.build_top_of_book(deltas)                       # best_bid/ask/mid/spread/sizes/imbalance per event
feats = qb.build_snapshot_features(tob, trades=qb.load_trades(... ), freq="1s")
```

---

## B. The self-contained local path (this repo — no cluster, no Dome keys)

Use this when you're on a laptop with no cluster access (our situation). It reproduces the **exact same schema**,
so the Rust backtester and the infra tooling both read it.

### B1. Capture every delta going forward with our collector
```bash
export KALSHI_API_KEY_ID=...                      # your Kalshi API key id
export KALSHI_PRIVATE_KEY=/path/to/kalshi_key.pem # RSA private key (PEM)

# discover live natgas markets, then stream their orderbook_delta + trade channels:
python adapters/kalshi_ws_collector.py --series KXNATGASD --out data/raw --clickhouse http://localhost:8123
```
This writes **both**: durable `data/raw/*.ndjson.gz` (always) and ClickHouse `kalshi.orderbook_deltas`/`trades`
(when the server is up). Run it continuously (e.g. `nohup`, a `launchd` job, or a tmux session) for the month you
want to capture. Gaps in Kalshi `seq` trigger an automatic resubscribe + fresh snapshot.

### B2. Start ClickHouse (native, no Docker) and load the schema
```bash
./clickhouse/run_clickhouse.sh server &     # http :8123, tcp :9000
./clickhouse/run_clickhouse.sh init         # loads clickhouse/schema/01_tables.sql
```

### B3. (If you only have history) fall back to candlesticks
We already have ~2 months of 1-min natgas candlesticks at `~/kalshi_commodities/data/NatGas/*.parquet`. The Rust
backtester's `candles` loader turns them into synthetic top-of-book events so strategies can be tested on **real
past natgas data today**, while you accumulate true tick deltas going forward with B1.

### B4. Backtest
```bash
# on captured deltas:
cargo run --release -- backtest --source clickhouse --instrument 'KXNATGASD-%' \
    --start 2026-05-01 --end 2026-06-01 --strategy mean_reversion

# or on a parquet dump (from A2 or B1):
cargo run --release -- backtest --source parquet --deltas natgas_2026-05_deltas.parquet --strategy market_maker

# or on the real candlesticks we already have:
cargo run --release -- backtest --source candles \
    --candles ~/kalshi_commodities/data/NatGas --strategy momentum
```
Output: `report.json` (infra-orchestrator compatible, between `===REPORT_JSON_START===` sentinels) + an HTML
tearsheet in `figures/`.

---

## TL;DR
- **Want a *past* month at true tick granularity?** Not possible from public Kalshi. Use Dome backfill (snapshot
  granularity, needs keys) via `/backfill`, or the 1-min candlesticks we already have.
- **Want every tick delta?** Run a live WS collector for that month — infra-orchestrator's `/collect`, or this
  repo's `adapters/kalshi_ws_collector.py` → ClickHouse. Then query/backtest with identical schema either way.
