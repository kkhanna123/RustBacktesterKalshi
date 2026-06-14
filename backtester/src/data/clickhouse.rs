//! ClickHouse HTTP loader (feature-gated behind `clickhouse`).
//!
//! Queries the configured deltas/trades tables over HTTP `:8123` using `ureq` (rustls TLS, no
//! OpenSSL — see Cargo.toml), requesting `FORMAT JSONEachRow`, and parses rows into `MarketEvent`s.
//!
//! The schema (database, table, and every column name) is fully configurable via
//! [`ClickHouseSchema`]; pass `--ch-config <path>` to load a custom map. Value parsing is tolerant:
//! action/side accept enum strings, ints, or lowercase; timestamps accept epoch ns or ISO strings.

use crate::data::clickhouse_schema::{
    aggressor_is_yes, parse_action, parse_side, parse_ts_ns, ClickHouseSchema,
};
use crate::data::sort_events;
use crate::types::{BookDelta, Cents, MarketEvent, TradeEvent};
use anyhow::{anyhow, Context, Result};

/// Load deltas + trades from ClickHouse for an instrument glob within `[start, end)`, using the
/// configured schema map.
pub fn load(
    base_url: &str,
    instrument_like: &str,
    start: Option<&str>,
    end: Option<&str>,
    schema: &ClickHouseSchema,
) -> Result<Vec<MarketEvent>> {
    let start = start.unwrap_or("1970-01-01");
    let end = end.unwrap_or("2100-01-01");

    let mut events = Vec::new();
    events.extend(load_deltas(base_url, instrument_like, start, end, schema)?);
    events.extend(load_trades(base_url, instrument_like, start, end, schema)?);
    Ok(sort_events(events))
}

fn run_query(base_url: &str, sql: &str) -> Result<String> {
    use std::io::Read;
    let resp = ureq::post(base_url)
        .send_string(sql)
        .map_err(|e| anyhow!("clickhouse request failed: {e}"))?;
    // Stream the body via `into_reader()` instead of `into_string()`: the latter caps responses at
    // ~10 MB, which a realistic tick query (hundreds of thousands of rows) blows past. The reader has
    // no such cap, so large result sets load fine.
    let mut body = String::new();
    resp.into_reader()
        .read_to_string(&mut body)
        .context("read clickhouse response")?;
    Ok(body)
}

/// One row of the cheap `list-instruments` / `describe-data` GROUP BY over the deltas table.
#[derive(Debug, Clone)]
pub struct ChInstrumentRow {
    pub instrument: String,
    pub rows: u64,
    pub first_ns: i64,
    pub last_ns: i64,
}

/// Cheaply enumerate distinct instruments in the deltas table with row counts and time span,
/// without pulling the underlying deltas. Uses a `GROUP BY` aggregate so it is fast even on big
/// tables. `instrument_like` is a SQL `LIKE` glob (default `%` = all).
pub fn list_instruments(
    base_url: &str,
    instrument_like: &str,
    start: Option<&str>,
    end: Option<&str>,
    schema: &ClickHouseSchema,
) -> Result<Vec<ChInstrumentRow>> {
    use crate::data::clickhouse_schema::esc;
    let start = start.unwrap_or("1970-01-01");
    let end = end.unwrap_or("2100-01-01");
    let c = &schema.deltas;
    let sql = format!(
        "SELECT {inst} AS instrument_id, count() AS rows, \
         toUnixTimestamp64Nano(min({ts})) AS first_ns, \
         toUnixTimestamp64Nano(max({ts})) AS last_ns \
         FROM {table} \
         WHERE {inst} LIKE '{like}' AND {ts} >= '{start}' AND {ts} < '{end}' \
         GROUP BY {inst} ORDER BY {inst} FORMAT JSONEachRow",
        inst = c.instrument_id,
        ts = c.timestamp,
        table = schema.deltas_fqtn(),
        like = esc(instrument_like),
        start = esc(start),
        end = esc(end),
    );
    let body = run_query(base_url, &sql)
        .with_context(|| format!("querying instruments from {}", schema.deltas_fqtn()))?;
    let mut out = Vec::new();
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line)?;
        out.push(ChInstrumentRow {
            instrument: v
                .get("instrument_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            rows: num_i64(&v, "rows").max(0) as u64,
            first_ns: v.get("first_ns").and_then(parse_ts_ns).unwrap_or(0),
            last_ns: v.get("last_ns").and_then(parse_ts_ns).unwrap_or(0),
        });
    }
    Ok(out)
}

fn load_deltas(
    base_url: &str,
    like: &str,
    start: &str,
    end: &str,
    schema: &ClickHouseSchema,
) -> Result<Vec<MarketEvent>> {
    let sql = schema.deltas_sql(like, start, end);
    let body = run_query(base_url, &sql)?;
    let mut out = Vec::new();
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line)?;
        let action = v
            .get("action")
            .and_then(parse_action)
            .ok_or_else(|| anyhow!("bad/unknown action in row: {line}"))?;
        let side = v
            .get("side")
            .and_then(parse_side)
            .ok_or_else(|| anyhow!("bad/unknown side in row: {line}"))?;
        out.push(MarketEvent::Delta(BookDelta {
            ts_ns: v.get("ts_ns").and_then(parse_ts_ns).unwrap_or(0),
            instrument: v
                .get("instrument_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            action,
            side,
            price: Cents::from_dollars(num_f64(&v, "price")),
            size: num_f64(&v, "size"),
            sequence: num_i64(&v, "sequence"),
            is_snapshot: num_i64(&v, "is_snapshot") != 0,
        }));
    }
    Ok(out)
}

fn load_trades(
    base_url: &str,
    like: &str,
    start: &str,
    end: &str,
    schema: &ClickHouseSchema,
) -> Result<Vec<MarketEvent>> {
    let sql = schema.trades_sql(like, start, end);
    let body = run_query(base_url, &sql)?;
    let mut out = Vec::new();
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line)?;
        let aggressor_yes = v.get("aggressor_side").map(aggressor_is_yes).unwrap_or(true);
        out.push(MarketEvent::Trade(TradeEvent {
            ts_ns: v.get("ts_ns").and_then(parse_ts_ns).unwrap_or(0),
            instrument: v
                .get("instrument_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            aggressor_yes,
            price: Cents::from_dollars(num_f64(&v, "price")),
            size: num_f64(&v, "size"),
            trade_id: v
                .get("trade_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        }));
    }
    Ok(out)
}

/// JSONEachRow may render numbers as strings; accept either.
fn num_i64(v: &serde_json::Value, k: &str) -> i64 {
    match v.get(k) {
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn num_f64(v: &serde_json::Value, k: &str) -> f64 {
    match v.get(k) {
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}
