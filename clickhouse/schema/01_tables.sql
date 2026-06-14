-- ClickHouse schema mirroring oracletrading/infra-orchestrator QuestDB market-data tables.
-- Column names/semantics match services/backfill/writer.py and examples/questdb_backtest.py so
-- data captured here is drop-in compatible with their research tooling.

CREATE DATABASE IF NOT EXISTS kalshi;

-- Every orderbook delta for every tick. is_snapshot=1 marks the first row of a full-book snapshot
-- (the reconstruction must clear the book on that row).
CREATE TABLE IF NOT EXISTS kalshi.orderbook_deltas
(
    timestamp      DateTime64(9, 'UTC'),                       -- event time (ns)
    instrument_id  LowCardinality(String),                     -- raw Kalshi market ticker
    venue          LowCardinality(String) DEFAULT 'KALSHI',
    action         Enum8('ADD' = 1, 'UPDATE' = 2, 'DELETE' = 3),
    side           Enum8('BUY' = 1, 'SELL' = 2),               -- YES-native book: BUY=yes bid, SELL=yes ask
    price          Float64,                                    -- dollars in [0.01, 0.99]
    size           Float64,                                    -- resting contracts at level after the update
    market_alias   LowCardinality(String) DEFAULT '',
    sequence       Int64 DEFAULT 0,                            -- Kalshi seq, for gap detection
    is_snapshot    UInt8 DEFAULT 0
)
ENGINE = MergeTree
PARTITION BY (instrument_id, toYYYYMMDD(timestamp))
ORDER BY (instrument_id, timestamp, sequence)
SETTINGS index_granularity = 8192;

-- Executed trades.
CREATE TABLE IF NOT EXISTS kalshi.trades
(
    ts_event       DateTime64(9, 'UTC'),
    instrument_id  LowCardinality(String),
    venue          LowCardinality(String) DEFAULT 'KALSHI',
    aggressor_side Enum8('yes' = 1, 'no' = 2),
    price          Float64,
    size           Float64,
    market_alias   LowCardinality(String) DEFAULT '',
    trade_id       String DEFAULT ''
)
ENGINE = MergeTree
PARTITION BY (instrument_id, toYYYYMMDD(ts_event))
ORDER BY (instrument_id, ts_event);

-- Optional: 1-second derived book features, mirroring snapshot_export engine.py FEATURE_COLUMNS.
CREATE TABLE IF NOT EXISTS kalshi.book_features_1s
(
    ts_event         DateTime64(9, 'UTC'),
    instrument_id    LowCardinality(String),
    best_bid         Float64,
    best_ask         Float64,
    mid              Float64,
    spread           Float64,
    bid_size         Float64,
    ask_size         Float64,
    imbalance        Float64,
    mid_open         Float64,
    mid_high         Float64,
    mid_low          Float64,
    mid_close        Float64,
    trade_count      Int64,
    trade_volume     Float64,
    trade_notional   Float64,
    last_trade_price Float64
)
ENGINE = MergeTree
PARTITION BY (instrument_id, toYYYYMMDD(ts_event))
ORDER BY (instrument_id, ts_event);
