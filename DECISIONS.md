# DECISIONS.md — controversial / non-obvious calls

Decisions made autonomously (operator was away and asked me not to block on questions). Each lists the call,
why, and how to reverse it if you disagree. Ordered roughly by impact.

## 1. "Every orderbook delta for every tick over a *past* month" is not physically possible from public Kalshi
**Call:** I did not pretend to produce a historical month of native deltas. Kalshi's historical REST API serves
only 1-minute candlesticks; per-event L2 deltas exist *only* on the live WebSocket and are not publicly archived.
**What I did instead:** built a forward-running WS collector (captures every delta from now on) and documented the
two honest fallbacks for history — Dome API snapshots (needs keys, snapshot-granular) and the 1-min candlesticks
we already have. See `docs/ORDERBOOK_DELTAS_HOWTO.md`.
**Reverse it:** if you have Dome API keys or cluster access to the infra-orchestrator QuestDB (which already has
collected deltas), point the backtester at that instead — the schema is identical.

## 2. Built a self-contained local stack (own collector + ClickHouse) instead of using infra-orchestrator
**Call:** This laptop has no Kubernetes/cluster access (QuestDB is at `questdb.data.svc.cluster.local`) and no Dome
keys, so the infra path can't pull data here. Per operator instruction ("if you can't query QuestDB build your own
collector, use ClickHouse"), I built `adapters/` → ClickHouse, mirroring infra's exact `orderbook_deltas`/`trades`
schema so data is drop-in compatible both ways.
**Reverse it:** the backtester also reads NDJSON/CSV and (feature-gated) ClickHouse over HTTP, so you can swap to
QuestDB by exporting to parquet→NDJSON (`adapters/parquet_to_ndjson.py`) or adding a QuestDB loader.

## 3. YES-native single book: NO levels mapped to YES asks at price (1 − q)
**Call:** A resting NO bid at price q is economically a YES ask at 1 − q (a NO buyer = a YES seller). So the
collector stores the whole book in YES terms: `side=BUY` (yes bids) and `side=SELL` (= 1 − no_price). This gives one
clean two-sided YES-priced book instead of two separate yes/no books.
**Why controversial:** some workflows prefer keeping raw yes/no books separate. **Reverse it:** drop the
`complement()` mapping in `adapters/book.py` and store the raw side; the Rust book is side-agnostic.

## 4. Prices are integer cents in [1, 99]
**Call:** Kalshi binary contracts settle $1/$0 and trade in whole cents, so internally price = `Cents(i32)`. Exact,
fast, no float drift on price. Sizes stay `f64` (contracts).
**Reverse it:** widen `Cents` or switch to a decimal type if sub-cent venues are ever added.

## 5. Kalshi `*_fp` fields treated as plain decimals (FP_DIVISOR = 1.0), not integer fixed-point
**Call:** The WS docs show snapshot/delta quantities as decimal strings (`"300.00"` = 300 contracts), so no scaling.
**Uncertain:** the `_fp` ("fixed-point") naming *could* mean integer ÷100 on some fields. I centralized this in one
constant `FP_DIVISOR` in `adapters/book.py` with a unit test on the scaling path.
**Reverse it:** set `--fp-divisor 100` (or flip the constant) if live data comes back off by 100×.

## 6. Fee model = Kalshi general formula `ceil(0.07 · C · p · (1−p))`, maker free
**Call:** Implemented Kalshi's published general trading-fee formula (7% × contracts × p × (1−p), rounded up to the
cent), with maker fee 0. Configurable in `BacktestConfig`.
**Why controversial:** Kalshi has per-market fee schedules, maker rebates on some markets, and settlement fees that
this doesn't fully model. **Reverse it:** edit `fees.rs` / pass a flat rate; the model is swappable.

## 7. Execution/fill model is a documented approximation
**Call:** Resting limit orders fill when a market trade crosses their price, after consuming `queue_ahead`
(contracts resting ahead at that level when placed) — a simple price-time-priority proxy. Market orders walk the
opposing book as taker. No partial-queue decay, no latency by default.
**Why controversial:** real fills depend on exact queue position and cancels you can't observe. This is optimistic-
to-neutral, not adversarial. **Reverse it:** tune `execution.rs` (add latency via `order_latency_ns`, make the queue
model stricter).

## 8. The demo runs on candle-derived synthetic top-of-book (1-minute fidelity)
**Call:** To have a runnable demo on *real* NatGas data today, the `candles` loader synthesizes a 2-level
top-of-book per minute from the existing `KXNATGASD` candlesticks (`~/kalshi_commodities/data/NatGas`). Sizes are
nominal (volume or a default).
**Consequence:** strategy P&L on candle data is ILLUSTRATIVE, not realistic — e.g. mean-reversion loses money
crossing wide synthetic 1-minute spreads. That's a fidelity artifact, not a bug. Real evaluation needs true tick
deltas via the collector → ClickHouse/NDJSON path.

## 9. Sharpe / Sortino reported unannualized (per-snapshot)
**Call:** Annualization needs a fixed sampling period and assumptions that don't hold across heterogeneous Kalshi
markets, so I report per-snapshot risk-adjusted ratios deterministically and document it in `metrics.rs`.
**Reverse it:** multiply by `sqrt(periods_per_year)` if you standardize the snapshot cadence.

## 10. report.json keeps infra-orchestrator's 9 summary fields first, then adds many analytics fields
**Call:** To stay drop-in compatible with their `/graph` bot command (which reads specific keys), `summary` preserves
those 9 fields in order; all extra stats are appended. Their consumers access by key and ignore unknowns, so this is
safe. **Reverse it:** nothing to reverse; just don't rename/remove the first 9.

## 11. Rust avoids a heavy native parquet dependency
**Call:** Kept the crate lean (fast compile, easy Windows builds) by reading NDJSON/CSV natively and leaving parquet
ingestion to a tiny Python helper (`adapters/parquet_to_ndjson.py`) or ClickHouse. **Reverse it:** add a
feature-gated `polars`/`arrow` loader if you want native parquet in Rust.

## 12. ClickHouse runs as the self-contained native binary (no Docker)
**Call:** Docker's daemon was down and ClickHouse ships a single native binary, so `bin/clickhouse` + a tiny runner
script is the local warehouse — zero Docker. The binary is macOS/Linux. **Windows:** run ClickHouse remotely (or in
WSL/Docker) and point the backtester's `--clickhouse http://host:8123` at it; the backtester itself is fully
cross-platform. See `clickhouse/run_clickhouse.sh` and README.

## 13. The infra-orchestrator repo was cloned read-only and never modified
It was public-cloneable without auth (`git ls-remote` succeeded). I cloned it to `/tmp` only to read it, built the
`InfraOrchestrator` skill from the survey, and made no changes to it.

## 14. Live collection uses the authenticated WS (true deltas) with a REST poller fallback
**Call:** With API keys present, the primary collector is the authenticated **WebSocket** `orderbook_delta` feed
(true per-tick deltas). Without keys, a credential-free **REST orderbook poller** hits the public
`/markets/{ticker}/orderbook` endpoint and diffs snapshots into deltas (snapshot-granular). Both write the identical
schema. **Why it matters:** the WS is higher fidelity but Kalshi's `seq` is **global per subscription**, not
per-market — the initial code misread it as per-market and triggered a resubscribe storm. Fixed to resync-and-continue
(deltas are still applied in order; books stay correct). Keys are loaded from the gitignored `KalshiAPIKeysDONOTPUSH/`
and never committed. A supervisor script auto-restarts the collector and prunes data older than 2 days.

## 15. Execution-cost models are explicit, additive, and OFF-by-default where they'd change a vanilla backtest
**Call:** Latency, slippage, and liquidity rewards default to disabled (or zero), so a plain backtest matches the
simple model; fees default ON (Kalshi charges them). Each cost is reported on its own line and `pnl_total` is an
explicit decomposition. **Why controversial:** some shops bake costs in by default. I chose opt-in so researchers can
isolate each effect — toggle via `--latency-ns/--slippage-*/--rewards/--no-fees`. **Reverse it:** flip defaults in
`ExecutionConfig`. Modeling simplifications (single-participant reward share; market-data latency folded into order
activation) are documented in the execution agent's modules.

## 16. Greeks: F and σ for short-dated digitals are estimated, not observed
**Call:** Kalshi gives only the binary-price ladder, not the underlying forward F or vol σ. The model-free implied
distribution (Breeden–Litzenberger on the ladder) needs no F/σ and is the primary output. The parametric greeks
estimate F at the implied median and fit σ by least-squares on quoted strikes, **bounding F to the strike range ±10%**
because short-dated digitals only identify total vol σ√T (an unbounded fit diverges). σ is annualized and looks large
because these are intraday events (T ~ 0.001–0.004 yr) — the stable quantity is σ√T. **Reverse it:** feed an external
Henry Hub forward/vol (e.g. from CME/Pyth) to pin F/σ instead of estimating from the ladder.
