//! Data loaders. Every loader returns a time-sorted `Vec<MarketEvent>` so the engine is
//! source-agnostic.

pub mod ndjson;
pub mod summary;

/// ClickHouse schema mapping + SQL builder + tolerant value parsing. Always compiled (so it is
/// unit-testable without a live DB); the HTTP loader itself is feature-gated below.
pub mod clickhouse_schema;

#[cfg(feature = "clickhouse")]
pub mod clickhouse;

use crate::types::MarketEvent;

/// Sort events ascending by timestamp (stable, so same-ts deltas keep their input order, which
/// the candle loader relies on to put a snapshot BUY before its SELL).
pub fn sort_events(mut events: Vec<MarketEvent>) -> Vec<MarketEvent> {
    events.sort_by_key(|e| e.ts_ns());
    events
}

/// Simple glob matcher supporting a trailing `%` or `*` as a prefix wildcard, plus exact match.
/// `KXNATGASD-%` matches anything starting with `KXNATGASD-`; no wildcard means exact equality.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('%').or_else(|| pattern.strip_suffix('*')) {
        value.starts_with(prefix)
    } else {
        pattern == value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_prefix_and_exact() {
        assert!(glob_match("KXNATGASD-%", "KXNATGASD-26JUN0417-T2.700"));
        assert!(glob_match("KXNATGASD-*", "KXNATGASD-X"));
        assert!(glob_match("KXNATGASD-26JUN0417-T2.700", "KXNATGASD-26JUN0417-T2.700"));
        assert!(!glob_match("KXNATGASD-26JUN0417-T2.700", "KXNATGASD-26JUN0417-T2.701"));
        assert!(!glob_match("ABC-%", "XYZ-1"));
    }
}
