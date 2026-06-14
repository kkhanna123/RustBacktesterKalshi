# `adapters/convert/` — Convert ANY market data into the backtester's canonical NDJSON

This folder contains a **language-agnostic data converter**. Point it at any tabular feed
(CSV / Parquet / a pandas DataFrame), describe your columns once in a small **ingest profile**,
and it emits the exact newline-delimited-JSON (NDJSON) the Rust backtester reads via
`--source ndjson`. **Adding a new data source needs ZERO Rust.**

- Library + CLI: [`to_canonical.py`](./to_canonical.py)
- Example profiles: [`profiles/`](./profiles/)
- Example inputs: [`examples/`](./examples/)
- Unit tests: [`tests/`](./tests/)

The repo Python is at `../.venv/bin/python` (has pandas, numpy, pyarrow). Run everything with it.

---

## Your data → a backtest in 3 steps

Say you have `mydata.csv` with order-book updates. From the repo root:

**1. Make (or pick) a profile** mapping your columns to the canonical fields. Start from
[`profiles/l2_delta_csv.json`](./profiles/l2_delta_csv.json) and edit the column names. Or let the
tool guess one for you with `--infer`.

**2. Convert to canonical NDJSON** (gzip if the name ends in `.gz`):

```bash
.venv/bin/python adapters/convert/to_canonical.py \
    --input adapters/convert/examples/l2_delta.csv \
    --profile adapters/convert/profiles/l2_delta_csv.json \
    --out /tmp/mydata.ndjson.gz
# [ok] wrote /tmp/mydata.ndjson.gz: 806 deltas, 0 trades, 6 snapshot rows, 0 out-of-order, 0 warning(s).
```

**3. Run the backtester on it:**

```bash
backtester/target/release/kalshi-backtest backtest \
    --source ndjson --ndjson /tmp/mydata.ndjson.gz \
    --strategy imbalance -q
# ...prints a ===REPORT_JSON_START===...===REPORT_JSON_END=== block.
```

(Build the binary first if needed: `cd backtester && cargo build --release`.)

That's it. The same three steps work for trades and for full-book "snapshot" feeds — just use a
different profile (see below).

---

## Quick exploration: `--infer` and `--preview`

You don't have to write a profile by hand to get started:

- **`--infer`** guesses a profile from your column names (and sniffs the first row to detect ISO
  timestamps vs epoch units). It detects trade-vs-delta from the presence of an aggressor/trade_id
  column, the timestamp unit from the column name/value (`*_ns`→ns, `*_ms`→ms, a datetime string→iso),
  and the price scale from the name (`*_cents`→cents). Always confirm the guess with `--preview`.

- **`--preview N`** prints the first `N` canonical events to stdout and writes **no file** — a fast
  dry-run to eyeball the mapping.

```bash
# Guess a profile and preview 5 events without writing anything:
.venv/bin/python adapters/convert/to_canonical.py --input adapters/convert/examples/trades.csv --infer --preview 5
```

Other flags: `--venue TAG` (venue for `--infer`), `--no-strict-price` (warn instead of error on
prices outside `(0,1]`), `--quiet/-q` (silence warnings).

---

## The canonical contract (must match exactly)

Confirmed against `backtester/src/data/ndjson.rs` and
`backtester/src/adapters/generic_ndjson.rs`. One JSON object per line.

**delta line** (an order-book level change):

```json
{"kind":"delta","ts_ns":1700000000000000000,"instrument":"MKT-A","action":"ADD",
 "side":"BUY","price":0.42,"size":200.0,"sequence":1,"is_snapshot":1,
 "venue":"GENERIC","market_alias":""}
```

| field          | type   | notes |
|----------------|--------|-------|
| `kind`         | str    | `"delta"` |
| `ts_ns`        | i64    | nanoseconds since the Unix epoch (UTC) |
| `instrument`   | str    | bare symbol or `"VENUE:symbol"` |
| `action`       | str    | `"ADD"` \| `"UPDATE"` \| `"DELETE"` |
| `side`         | str    | `"BUY"` (= YES bid) \| `"SELL"` (= YES ask) |
| `price`        | f64    | **dollars / probability in `(0, 1]`** (0.42 = 42¢) |
| `size`         | f64    | contracts |
| `sequence`     | i64    | monotonic per stream (auto if absent) |
| `is_snapshot`  | 0\|1   | 1 if this row is part of a full-book snapshot |
| `venue`        | str    | venue TAG |
| `market_alias` | str    | always `""` |

**trade line** (a print):

```json
{"kind":"trade","ts_ns":1700000000000000000,"instrument":"MKT-A",
 "aggressor_side":"yes","price":0.55,"size":3.0,"trade_id":"t1","venue":"GENERIC"}
```

| field            | type | notes |
|------------------|------|-------|
| `kind`           | str  | `"trade"` |
| `ts_ns`          | i64  | nanoseconds since epoch |
| `instrument`     | str  | bare symbol or `"VENUE:symbol"` |
| `aggressor_side` | str  | `"yes"` \| `"no"` (who crossed the spread) |
| `price`          | f64  | dollars |
| `size`           | f64  | contracts |
| `trade_id`       | str  | id (may be `""`) |
| `venue`          | str  | venue TAG |

### YES-native book — IMPORTANT

The book is YES-native: **`BUY` = YES bid, `SELL` = YES ask.** If your feed quotes the **NO** side,
a *NO bid at price q* is a *YES ask at (1 − q)*. Set `"no_side": true` in the profile and the tool
will (a) complement the price `p → 1 − p` and (b) flip the side `bid ↔ ask` for you.

---

## Profile reference

A profile is a JSON (or TOML) object. Each canonical field is filled **either from a column**
(`*_col`) **or pinned to a constant** (`*_const`). Unknown keys are rejected with a helpful error.

### Routing / shared

| key                | default     | meaning |
|--------------------|-------------|---------|
| `kind`             | `null`      | `"delta"` or `"trade"` for the whole file. If `null`, set `kind_col`. |
| `kind_col`         | `null`      | column that selects delta/trade per row |
| `venue`            | `"GENERIC"` | venue TAG written to every event |
| `instrument_col` / `instrument_const` | — | which symbol each row belongs to |
| `ts_col`           | —           | timestamp column |
| `ts_unit`          | `"ns"`      | `s` \| `ms` \| `us` \| `ns` \| `iso` |
| `ts_format`        | `null`      | strptime format for non-ISO datetime strings (with `ts_unit:"iso"`) |
| `price_col`        | —           | price column |
| `price_scale`      | `"dollars"` | `dollars` \| `prob` \| `cents` \| `bps` |
| `price_factor`     | `null`      | custom multiplier to dollars; **overrides** `price_scale` |
| `size_col` / `size_const` | — | contracts |

### Delta-specific

| key                                  | default  | meaning |
|--------------------------------------|----------|---------|
| `side_col` / `side_const`            | —        | YES-native side token (`BUY/SELL/bid/ask/yes/no/B/S/+/-`) |
| `action_col` / `action_const`        | `"ADD"`  | `ADD` \| `UPDATE` \| `DELETE` |
| `sequence_col` / `sequence_const`    | auto     | i64 sequence (auto-increments if both absent) |
| `is_snapshot_col` / `is_snapshot_const` | `0`   | mark snapshot rows |
| `no_side`                            | `false`  | treat side tokens as NO-side; apply NO→YES complement |

### Trade-specific

| key                                | default | meaning |
|------------------------------------|---------|---------|
| `aggressor_col` / `aggressor_const` | —      | `yes`/`no` (or any side token) |
| `trade_id_col` / `trade_id_const`   | `""`   | trade id |

### Snapshot mode (full-book feeds → diffed deltas)

Some feeds publish the **entire book every tick** instead of incremental changes. Set
`"snapshot_mode": true` and the tool diffs each snapshot against the previous one per instrument,
emitting `is_snapshot=1 ADD`s for the first book and `ADD`/`UPDATE`/`DELETE` for subsequent changes.

| key                  | default | meaning |
|----------------------|---------|---------|
| `snapshot_mode`      | `false` | enable snapshot→delta differ |
| `snapshot_group_col` | `null`  | optional snapshot-id column; otherwise `(instrument, ts_ns)` groups a snapshot |
| `price_decimals`     | `4`     | round prices before diffing to avoid float-jitter churn |

Requires `instrument`, `ts_col`, `side_col`, `price_col`, `size_col`.

### Example profiles (in [`profiles/`](./profiles/))

- **`l2_delta_csv.json`** — incremental L2 deltas, prices in cents, ms timestamps.
- **`trades_csv.json`** — trade prints, ISO timestamps, dollar prices, `yes/no` taker side.
- **`snapshot_feed.json`** — full-book snapshots (probability prices) diffed into deltas.

---

## Library / helper API

Import from `to_canonical` (add `adapters/convert/` to `sys.path`, or run from `adapters/convert/`):

```python
from to_canonical import convert, IngestProfile, CanonicalWriter, iter_events
import pandas as pd

# One-shot: DataFrame or path -> NDJSON file.
stats = convert("mydata.csv", "tools/profiles/l2_delta_csv.json", "out.ndjson.gz")
print(stats)  # {'deltas': ..., 'trades': ..., 'snapshots': ..., 'non_monotonic': ..., 'warnings': ...}

# Stream events (e.g. in a notebook) without writing:
for ev in iter_events(pd.read_csv("mydata.csv"), IngestProfile.from_file("p.json")):
    ...  # ev is a DeltaEvent or TradeEvent; ev.to_dict() is the canonical line
```

**Composable conversion helpers** (each independently unit-tested):

| helper | purpose |
|--------|---------|
| `to_ns(value, unit, fmt=None)` | timestamp → i64 nanoseconds (`s\|ms\|us\|ns\|iso` + optional strptime `fmt`) |
| `to_dollars(value, scale, factor=None)` | price → dollars (`dollars\|prob\|cents\|bps`, or custom `factor`) |
| `to_side(token)` | side token → `"BUY"`/`"SELL"` (yes/no, bid/ask, buy/sell, B/S, +/-) |
| `to_aggressor(token)` | trade aggressor token → `"yes"`/`"no"` |
| `no_to_yes(price)` | NO→YES price complement (`1 - price`) |
| `flip_side(side)` | `BUY`↔`SELL` (use with `no_to_yes`) |
| `SnapshotDiffer` | diff successive full books into `is_snapshot` + delta rows |
| `CanonicalWriter` | validating gzip-NDJSON writer (price range, action/side, monotonicity) |
| `infer_profile(columns, venue, sample_row=None)` | guess a profile from column names |

`CanonicalWriter` rejects delta/trade prices outside `(0, 1]` (use `strict_price=False` to downgrade
to a warning), validates `action`/`side`/`aggressor_side`, coerces `is_snapshot` to 0/1, and counts
(with a single warning) non-monotonic timestamps — which usually means a wrong `ts_unit`.

---

## Running the tests

```bash
.venv/bin/python -m unittest discover -s adapters/convert/tests -v
```
