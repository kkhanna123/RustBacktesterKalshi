//! Configurable ClickHouse schema mapping + SQL builder + tolerant value parsing.
//!
//! The point of this module: support "slightly different ClickHouse schemas" by editing CONFIG,
//! never recompiling. Every logical field the loader needs is mapped to a column name in
//! [`ClickHouseSchema`]; the [`SQL builder`](ClickHouseSchema::deltas_sql) emits a SELECT using the
//! configured names. A [`ClickHouseSchema::default`] matches the infra-orchestrator schema.
//!
//! This module is **always compiled** (outside the `clickhouse` cargo feature) so the schema and
//! SQL builder are unit-testable without a live database. Only the HTTP loader itself is gated.
//!
//! Load a custom map from JSON or TOML (both always supported) via [`ClickHouseSchema::from_path`].
//! See `clickhouse/schema_map.example.json` for every overridable field documented with its default.

use crate::types::{Action, Side};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Column-name mapping for the `orderbook_deltas` (or equivalent) table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DeltaColumns {
    pub timestamp: String,
    pub instrument_id: String,
    pub venue: String,
    pub action: String,
    pub side: String,
    pub price: String,
    pub size: String,
    pub market_alias: String,
    pub sequence: String,
    pub is_snapshot: String,
}

impl Default for DeltaColumns {
    fn default() -> Self {
        DeltaColumns {
            timestamp: "timestamp".into(),
            instrument_id: "instrument_id".into(),
            venue: "venue".into(),
            action: "action".into(),
            side: "side".into(),
            price: "price".into(),
            size: "size".into(),
            market_alias: "market_alias".into(),
            sequence: "sequence".into(),
            is_snapshot: "is_snapshot".into(),
        }
    }
}

/// Column-name mapping for the `trades` (or equivalent) table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TradeColumns {
    pub ts_event: String,
    pub instrument_id: String,
    pub venue: String,
    pub aggressor_side: String,
    pub price: String,
    pub size: String,
    pub market_alias: String,
    pub trade_id: String,
}

impl Default for TradeColumns {
    fn default() -> Self {
        TradeColumns {
            ts_event: "ts_event".into(),
            instrument_id: "instrument_id".into(),
            venue: "venue".into(),
            aggressor_side: "aggressor_side".into(),
            price: "price".into(),
            size: "size".into(),
            market_alias: "market_alias".into(),
            trade_id: "trade_id".into(),
        }
    }
}

/// A full ClickHouse schema map: database, table names, and per-table column maps.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClickHouseSchema {
    pub database: String,
    pub deltas_table: String,
    pub trades_table: String,
    pub deltas: DeltaColumns,
    pub trades: TradeColumns,
}

impl Default for ClickHouseSchema {
    fn default() -> Self {
        ClickHouseSchema {
            database: "kalshi".into(),
            deltas_table: "orderbook_deltas".into(),
            trades_table: "trades".into(),
            deltas: DeltaColumns::default(),
            trades: TradeColumns::default(),
        }
    }
}

impl ClickHouseSchema {
    /// Load a schema map from a file. `.toml` files are parsed as TOML; everything else (and
    /// `.json`) is parsed as JSON. Missing fields fall back to the infra defaults (serde `default`).
    pub fn from_path(path: &Path) -> anyhow::Result<ClickHouseSchema> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read schema map {}: {e}", path.display()))?;
        Self::from_str_auto(&text, path)
    }

    /// Parse from a string, choosing TOML vs JSON by extension (defaulting to JSON).
    pub fn from_str_auto(text: &str, path: &Path) -> anyhow::Result<ClickHouseSchema> {
        let is_toml = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("toml"))
            .unwrap_or(false);
        if is_toml {
            return toml::from_str(text).map_err(|e| anyhow::anyhow!("parse TOML schema map: {e}"));
        }
        serde_json::from_str(text).map_err(|e| anyhow::anyhow!("parse JSON schema map: {e}"))
    }

    /// Fully-qualified deltas table `database.table`.
    pub fn deltas_fqtn(&self) -> String {
        format!("{}.{}", self.database, self.deltas_table)
    }

    /// Fully-qualified trades table `database.table`.
    pub fn trades_fqtn(&self) -> String {
        format!("{}.{}", self.database, self.trades_table)
    }

    /// Build the deltas SELECT. Column expressions are aliased to fixed logical names
    /// (`ts_ns`, `instrument_id`, `action`, `side`, `price`, `size`, `sequence`, `is_snapshot`)
    /// so the row parser is schema-independent. The timestamp is normalized to epoch nanoseconds.
    pub fn deltas_sql(&self, instrument_like: &str, start: &str, end: &str) -> String {
        let c = &self.deltas;
        format!(
            "SELECT toUnixTimestamp64Nano({ts}) AS ts_ns, {inst} AS instrument_id, \
             {action} AS action, {side} AS side, {price} AS price, {size} AS size, \
             {seq} AS sequence, {snap} AS is_snapshot \
             FROM {table} \
             WHERE {inst} LIKE '{like}' AND {ts} >= '{start}' AND {ts} < '{end}' \
             ORDER BY {ts}, {seq} FORMAT JSONEachRow",
            ts = c.timestamp,
            inst = c.instrument_id,
            action = c.action,
            side = c.side,
            price = c.price,
            size = c.size,
            seq = c.sequence,
            snap = c.is_snapshot,
            table = self.deltas_fqtn(),
            like = esc(instrument_like),
            start = esc(start),
            end = esc(end),
        )
    }

    /// Build the trades SELECT, aliased to fixed logical names.
    pub fn trades_sql(&self, instrument_like: &str, start: &str, end: &str) -> String {
        let c = &self.trades;
        format!(
            "SELECT toUnixTimestamp64Nano({ts}) AS ts_ns, {inst} AS instrument_id, \
             {aggr} AS aggressor_side, {price} AS price, {size} AS size, {tid} AS trade_id \
             FROM {db}.{table} \
             WHERE {inst} LIKE '{like}' AND {ts} >= '{start}' AND {ts} < '{end}' \
             ORDER BY {ts} FORMAT JSONEachRow",
            ts = c.ts_event,
            inst = c.instrument_id,
            aggr = c.aggressor_side,
            price = c.price,
            size = c.size,
            tid = c.trade_id,
            db = self.database,
            table = self.trades_table,
            like = esc(instrument_like),
            start = esc(start),
            end = esc(end),
        )
    }
}

/// Escape single quotes for inline SQL literals.
pub fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

// ----------------------------------------------------------------------------
// Tolerant value parsing — action/side/timestamp may arrive in several shapes.
// ----------------------------------------------------------------------------

/// Parse a delta `action` that may be the enum string ("ADD"/"add"), an int (1/2/3), or short
/// forms. Mapping: 1=Add, 2=Update, 3=Delete (matches common ClickHouse Enum8 encodings).
pub fn parse_action(v: &serde_json::Value) -> Option<Action> {
    match v {
        serde_json::Value::String(s) => parse_action_str(s),
        serde_json::Value::Number(n) => n.as_i64().and_then(parse_action_int),
        _ => None,
    }
}

fn parse_action_str(s: &str) -> Option<Action> {
    // Numeric-in-string (e.g. "1") is common from JSONEachRow Enum columns.
    if let Ok(i) = s.trim().parse::<i64>() {
        return parse_action_int(i);
    }
    match s.trim().to_ascii_uppercase().as_str() {
        "ADD" | "A" | "INSERT" => Some(Action::Add),
        "UPDATE" | "U" | "MODIFY" | "CHANGE" => Some(Action::Update),
        "DELETE" | "D" | "REMOVE" => Some(Action::Delete),
        _ => None,
    }
}

fn parse_action_int(i: i64) -> Option<Action> {
    match i {
        1 => Some(Action::Add),
        2 => Some(Action::Update),
        3 => Some(Action::Delete),
        _ => None,
    }
}

/// Parse a `side` that may be "BUY"/"SELL", "buy"/"sell", "bid"/"ask", "yes"/"no", or ints
/// (1=BUY/Bid, 2=SELL/Ask). Returns the YES-native [`Side`].
///
/// YES-NATIVE INGEST — prices are NOT complemented here. The canonical warehouse data is already
/// YES-native: a NO bid at `q` was already stored as a YES ask at `1 − q` by the upstream
/// collector/converter. The loader therefore maps sides and copies prices through verbatim;
/// calling [`crate::types::Cents::complement`] here would DOUBLE-complement and corrupt prices.
pub fn parse_side(v: &serde_json::Value) -> Option<Side> {
    match v {
        serde_json::Value::String(s) => parse_side_str(s),
        serde_json::Value::Number(n) => n.as_i64().and_then(parse_side_int),
        _ => None,
    }
}

fn parse_side_str(s: &str) -> Option<Side> {
    if let Ok(i) = s.trim().parse::<i64>() {
        return parse_side_int(i);
    }
    match s.trim().to_ascii_uppercase().as_str() {
        "BUY" | "BID" | "B" | "YES" => Some(Side::Bid),
        "SELL" | "ASK" | "S" | "A" | "NO" => Some(Side::Ask),
        _ => None,
    }
}

fn parse_side_int(i: i64) -> Option<Side> {
    match i {
        1 => Some(Side::Bid),
        2 => Some(Side::Ask),
        _ => None,
    }
}

/// True if an `aggressor_side` value means the YES side took (aggressor bought YES).
pub fn aggressor_is_yes(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(s) => matches!(
            s.trim().to_ascii_uppercase().as_str(),
            "YES" | "BUY" | "BID" | "1"
        ),
        serde_json::Value::Number(n) => n.as_i64() == Some(1),
        serde_json::Value::Bool(b) => *b,
        // default to yes when missing/unknown (matches prior loader behavior)
        _ => true,
    }
}

/// Parse a timestamp that may already be epoch nanoseconds (number or numeric string), or an ISO
/// `DateTime64` string like `2026-06-04 12:34:56.789` / `2026-06-04T12:34:56.789Z`. Returns ns.
pub fn parse_ts_ns(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        serde_json::Value::String(s) => parse_ts_str(s),
        _ => None,
    }
}

fn parse_ts_str(s: &str) -> Option<i64> {
    let s = s.trim();
    // pure integer -> already epoch ns
    if let Ok(i) = s.parse::<i64>() {
        return Some(i);
    }
    // ISO datetime: accept "T" or space separator, optional fractional secs, optional trailing Z.
    let norm = s.replace('T', " ");
    let norm = norm.trim_end_matches('Z').trim();
    // Try with fractional seconds, then without.
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(norm, fmt) {
            return dt.and_utc().timestamp_nanos_opt();
        }
        if fmt == "%Y-%m-%d" {
            if let Ok(d) = chrono::NaiveDate::parse_from_str(norm, fmt) {
                return d
                    .and_hms_opt(0, 0, 0)
                    .and_then(|dt| dt.and_utc().timestamp_nanos_opt());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn default_sql_uses_infra_names() {
        let s = ClickHouseSchema::default();
        let sql = s.deltas_sql("KX%", "2026-01-01", "2026-01-02");
        assert!(sql.contains("FROM kalshi.orderbook_deltas"));
        assert!(sql.contains("instrument_id LIKE 'KX%'"));
        assert!(sql.contains("toUnixTimestamp64Nano(timestamp)"));
        let tsql = s.trades_sql("KX%", "2026-01-01", "2026-01-02");
        assert!(tsql.contains("FROM kalshi.trades"));
        assert!(tsql.contains("aggressor_side AS aggressor_side"));
    }

    #[test]
    fn custom_schema_json_changes_sql() {
        // A "slightly different" schema: different db, tables, and a couple of renamed columns.
        let json = r#"{
            "database": "md",
            "deltas_table": "book_deltas",
            "trades_table": "prints",
            "deltas": { "timestamp": "event_time", "instrument_id": "ticker", "sequence": "seq_no" },
            "trades": { "ts_event": "exec_time", "instrument_id": "ticker", "aggressor_side": "taker_side" }
        }"#;
        let s = ClickHouseSchema::from_str_auto(json, Path::new("custom.json")).unwrap();
        let sql = s.deltas_sql("ABC%", "2026-01-01", "2026-01-02");
        assert!(sql.contains("FROM md.book_deltas"), "sql: {sql}");
        assert!(sql.contains("toUnixTimestamp64Nano(event_time)"), "sql: {sql}");
        assert!(sql.contains("ticker AS instrument_id"), "sql: {sql}");
        assert!(sql.contains("seq_no AS sequence"), "sql: {sql}");
        // unspecified columns keep defaults
        assert!(sql.contains("action AS action"), "sql: {sql}");

        let tsql = s.trades_sql("ABC%", "2026-01-01", "2026-01-02");
        assert!(tsql.contains("FROM md.prints"), "tsql: {tsql}");
        assert!(tsql.contains("toUnixTimestamp64Nano(exec_time)"), "tsql: {tsql}");
        assert!(tsql.contains("taker_side AS aggressor_side"), "tsql: {tsql}");
    }

    #[test]
    fn example_schema_file_parses_to_defaults() {
        // The shipped example documents every field with the infra defaults; it must parse and
        // produce the same SQL as `default()` (the `_comment` keys are ignored).
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/clickhouse/schema_map.example.json"
        ));
        if path.exists() {
            let s = ClickHouseSchema::from_path(path).unwrap();
            assert_eq!(
                s.deltas_sql("KX%", "2026-01-01", "2026-01-02"),
                ClickHouseSchema::default().deltas_sql("KX%", "2026-01-01", "2026-01-02")
            );
        }
    }

    #[test]
    fn action_parsing_tolerant() {
        use serde_json::json;
        assert_eq!(parse_action(&json!("ADD")), Some(Action::Add));
        assert_eq!(parse_action(&json!("update")), Some(Action::Update));
        assert_eq!(parse_action(&json!(3)), Some(Action::Delete));
        assert_eq!(parse_action(&json!("1")), Some(Action::Add));
        assert_eq!(parse_action(&json!("bogus")), None);
    }

    #[test]
    fn side_parsing_tolerant() {
        use serde_json::json;
        assert_eq!(parse_side(&json!("BUY")), Some(Side::Bid));
        assert_eq!(parse_side(&json!("sell")), Some(Side::Ask));
        assert_eq!(parse_side(&json!("bid")), Some(Side::Bid));
        assert_eq!(parse_side(&json!(2)), Some(Side::Ask));
        assert_eq!(parse_side(&json!("YES")), Some(Side::Bid));
    }

    #[test]
    fn timestamp_parsing_tolerant() {
        use serde_json::json;
        // epoch ns as number and string
        assert_eq!(parse_ts_ns(&json!(1_700_000_000_000_000_000i64)), Some(1_700_000_000_000_000_000));
        assert_eq!(parse_ts_ns(&json!("1700000000000000000")), Some(1_700_000_000_000_000_000));
        // ISO strings
        let a = parse_ts_ns(&json!("2026-06-04T12:00:00Z")).unwrap();
        let b = parse_ts_ns(&json!("2026-06-04 12:00:00")).unwrap();
        assert_eq!(a, b);
        assert!(parse_ts_ns(&json!("2026-06-04 12:00:00.500")).unwrap() > b);
    }

    #[test]
    fn aggressor_parsing() {
        use serde_json::json;
        assert!(aggressor_is_yes(&json!("yes")));
        assert!(aggressor_is_yes(&json!("BUY")));
        assert!(!aggressor_is_yes(&json!("no")));
        assert!(aggressor_is_yes(&json!(1)));
        assert!(!aggressor_is_yes(&json!(2)));
    }
}
