//! GENERIC CSV adapter — ingest ANY venue's tick data from a CSV with a configurable column
//! mapping. The other half of the "zero-Rust new venue" path: if a source can emit CSV rows, point
//! this adapter at it and (optionally) map column names.
//!
//! ## Default columns
//! `kind,ts_ns,instrument,action,side,price,size,sequence,is_snapshot,aggressor_side,trade_id`
//! A header row is REQUIRED so columns are addressed by name (not position), which makes mapping
//! trivial. Rows with `kind=trade` become [`TradeEvent`]s; everything else is a book delta.
//!
//! ## Column mapping (`AdapterSpec::mapping`)
//! `{"ts_ns":"timestamp","instrument":"symbol","price":"px"}` — keys are canonical names, values
//! are the actual header names in your file. Unmapped canonical names use their default header.
//! - `{"price_scale":"dollars|cents|bps|prob"}` controls the price unit (default `dollars`); the
//!   older `{"price_is_cents":"true"}` is an alias for `price_scale=cents`.
//! - `{"ts_unit":"s"}` (or `ms`/`us`/`ns`, default `ns`) if your timestamp isn't already nanoseconds.
//! - `{"side_from_sign":"<column>"}` derives the side from the SIGN of a signed size/qty column
//!   (positive → Bid, negative → Ask, |value| → size) when there is no explicit side column.

use crate::adapters::{date_to_ns_opt, finalize_events, AdapterSpec, DataAdapter, MappingKey, Venue};
use crate::data::glob_match;
use crate::types::{Action, BookDelta, MarketEvent, Side, TradeEvent};
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// The `mapping` keys the generic CSV adapter understands (surfaced by `list-adapters`).
pub const MAPPING_KEYS: &[MappingKey] = &[
    MappingKey { key: "ts_ns", note: "header for the timestamp column (default 'ts_ns')" },
    MappingKey { key: "instrument", note: "header for the symbol column (default 'instrument')" },
    MappingKey { key: "kind", note: "header for delta|trade routing (default 'kind'; default value 'delta')" },
    MappingKey { key: "price", note: "header for the price column (default 'price')" },
    MappingKey { key: "size", note: "header for the size column (default 'size')" },
    MappingKey { key: "side", note: "header for side BUY/SELL/YES/NO/bid/ask (default 'side')" },
    MappingKey { key: "action", note: "header for ADD|UPDATE|DELETE (default 'action' → ADD)" },
    MappingKey { key: "sequence", note: "header for the i64 sequence (default 'sequence'; auto-increments if absent)" },
    MappingKey { key: "is_snapshot", note: "header for the snapshot flag (default 'is_snapshot')" },
    MappingKey { key: "aggressor_side", note: "trade-only: header for the aggressor side (default 'aggressor_side')" },
    MappingKey { key: "trade_id", note: "trade-only: header for the trade id (default 'trade_id')" },
    MappingKey { key: "ts_unit", note: "timestamp unit: s|ms|us|ns (default 'ns')" },
    MappingKey { key: "price_scale", note: "price unit: dollars|cents|bps|prob (default 'dollars')" },
    MappingKey { key: "price_is_cents", note: "alias: 'true' == price_scale=cents (default 'false')" },
    MappingKey { key: "side_from_sign", note: "derive side from the SIGN of this signed column (no explicit side)" },
];

/// A configurable CSV adapter usable by any venue without recompiling.
pub struct GenericCsvAdapter;

impl GenericCsvAdapter {
    pub fn new() -> Self {
        GenericCsvAdapter
    }
}

impl Default for GenericCsvAdapter {
    fn default() -> Self {
        Self::new()
    }
}

fn col<'a>(mapping: &'a BTreeMap<String, String>, canonical: &'a str) -> &'a str {
    mapping.get(canonical).map(|s| s.as_str()).unwrap_or(canonical)
}

fn map_side(s: &str) -> Option<Side> {
    match s.to_uppercase().as_str() {
        "BUY" | "BID" | "YES" | "B" => Some(Side::Bid),
        "SELL" | "ASK" | "NO" | "S" | "A" => Some(Side::Ask),
        _ => None,
    }
}

fn map_action(s: &str) -> Action {
    match s.to_uppercase().as_str() {
        "DELETE" | "DEL" | "REMOVE" => Action::Delete,
        "UPDATE" | "CHANGE" => Action::Update,
        _ => Action::Add,
    }
}

/// Scale a raw timestamp in the configured unit up to nanoseconds.
fn ts_to_ns(raw: f64, unit: &str) -> i64 {
    let mult = match unit {
        "s" | "sec" | "seconds" => 1_000_000_000.0,
        "ms" | "milli" => 1_000_000.0,
        "us" | "micro" => 1_000.0,
        _ => 1.0, // ns
    };
    (raw * mult).round() as i64
}

impl DataAdapter for GenericCsvAdapter {
    fn name(&self) -> &str {
        "generic_csv"
    }
    fn default_venue(&self) -> Venue {
        Venue::Generic("GENERIC".into())
    }
    fn description(&self) -> &str {
        "Any venue's tick data as a CSV with a header row (delta/trade per row; zero Rust)."
    }
    fn mapping_keys(&self) -> &'static [MappingKey] {
        MAPPING_KEYS
    }
    fn load(&self, spec: &AdapterSpec) -> Result<Vec<MarketEvent>> {
        let venue = spec.resolved_venue(self.default_venue());
        let (start_ns, end_ns) = (date_to_ns_opt(&spec.start)?, date_to_ns_opt(&spec.end)?);
        let m = &spec.mapping;
        let price_scale = crate::adapters::PriceScale::from_mapping(m)
            .with_context(|| format!("invalid price_scale for adapter '{}'", spec.adapter))?;
        let ts_unit = m.get("ts_unit").map(|s| s.as_str()).unwrap_or("ns").to_string();
        let side_sign_col = m.get("side_from_sign").map(|s| s.to_string());

        let path = Path::new(&spec.path);
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .has_headers(true)
            .from_path(path)
            .with_context(|| format!("open csv {}", path.display()))?;

        // Map header name -> column index so we can address by configurable name.
        let headers = rdr.headers().context("reading CSV header")?.clone();
        let idx = |name: &str| headers.iter().position(|h| h == name);
        let inst_i = idx(col(m, "instrument"))
            .ok_or_else(|| anyhow!("CSV missing instrument column '{}'", col(m, "instrument")))?;
        let ts_i =
            idx(col(m, "ts_ns")).ok_or_else(|| anyhow!("CSV missing ts column '{}'", col(m, "ts_ns")))?;
        let kind_i = idx(col(m, "kind"));
        let action_i = idx(col(m, "action"));
        let side_i = idx(col(m, "side"));
        let price_i = idx(col(m, "price"));
        let size_i = idx(col(m, "size"));
        let seq_i = idx(col(m, "sequence"));
        let snap_i = idx(col(m, "is_snapshot"));
        let aggr_i = idx(col(m, "aggressor_side"));
        let tid_i = idx(col(m, "trade_id"));
        // Optional signed-size column for side_from_sign; error early if the header is absent.
        let sign_i = match &side_sign_col {
            Some(name) => Some(
                idx(name).ok_or_else(|| anyhow!("CSV missing side_from_sign column '{name}'"))?,
            ),
            None => None,
        };

        let get = |rec: &csv::StringRecord, i: Option<usize>| -> Option<String> {
            i.and_then(|i| rec.get(i)).map(|s| s.trim().to_string())
        };

        let mut deltas: Vec<BookDelta> = Vec::new();
        let mut trades: Vec<TradeEvent> = Vec::new();
        let mut seq_fallback: i64 = 0;

        for result in rdr.records() {
            let rec = match result {
                Ok(r) => r,
                Err(_) => continue, // skip malformed rows
            };
            let symbol = match rec.get(inst_i) {
                Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                _ => continue,
            };
            if let Some(pat) = &spec.instrument {
                if !glob_match(pat, &symbol) {
                    continue;
                }
            }
            let ts_raw = match rec.get(ts_i).and_then(|s| s.trim().parse::<f64>().ok()) {
                Some(t) => t,
                None => continue,
            };
            let ts_ns = ts_to_ns(ts_raw, &ts_unit);
            if let Some(s) = start_ns {
                if ts_ns < s {
                    continue;
                }
            }
            if let Some(e) = end_ns {
                if ts_ns >= e {
                    continue;
                }
            }

            let price = get(&rec, price_i)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let cents = price_scale.to_cents(price);
            let mut size = get(&rec, size_i).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            let kind = get(&rec, kind_i).unwrap_or_else(|| "delta".to_string());

            if kind.eq_ignore_ascii_case("trade") {
                let aggressor_yes = get(&rec, aggr_i)
                    .map(|s| matches!(s.to_uppercase().as_str(), "YES" | "BUY" | "BID"))
                    .unwrap_or(true);
                let trade_id = get(&rec, tid_i).unwrap_or_default();
                trades.push(TradeEvent {
                    ts_ns,
                    instrument: symbol,
                    aggressor_yes,
                    price: cents,
                    size,
                    trade_id,
                });
            } else {
                // Side: explicit `side` token wins; else derive from the SIGN of the
                // `side_from_sign` column (positive → Bid, negative → Ask, |value| → size).
                let side = match get(&rec, side_i).as_deref().and_then(map_side) {
                    Some(s) => s,
                    None => match sign_i {
                        Some(si) => {
                            let signed = match get(&rec, Some(si)).and_then(|s| s.parse::<f64>().ok()) {
                                Some(v) if v != 0.0 => v,
                                _ => continue, // no usable sign on this row → skip
                            };
                            size = signed.abs();
                            if signed > 0.0 { Side::Bid } else { Side::Ask }
                        }
                        None => continue, // a delta with no usable side is skipped
                    },
                };
                let action = get(&rec, action_i)
                    .as_deref()
                    .map(map_action)
                    .unwrap_or(Action::Add);
                let sequence = get(&rec, seq_i)
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or_else(|| {
                        seq_fallback += 1;
                        seq_fallback
                    });
                let is_snapshot = get(&rec, snap_i)
                    .map(|s| {
                        let s = s.to_lowercase();
                        s == "1" || s == "true" || s == "yes"
                    })
                    .unwrap_or(false);
                deltas.push(BookDelta {
                    ts_ns,
                    instrument: symbol,
                    action,
                    side,
                    price: cents,
                    size,
                    sequence,
                    is_snapshot,
                });
            }
        }
        Ok(finalize_events(deltas, trades, &venue))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Cents;
    use std::io::Write;

    fn tmp(tag: &str, content: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("gen_csv_{}_{tag}_{n}.csv", std::process::id()));
        std::fs::File::create(&p).unwrap().write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn loads_venue_tagged_events_default_columns() {
        let csv = "kind,ts_ns,instrument,action,side,price,size,sequence,is_snapshot,aggressor_side,trade_id\n\
                   delta,1000,ETH-PERP,ADD,BUY,0.40,100,1,1,,\n\
                   trade,2000,ETH-PERP,,,0.55,3,,,,t1\n";
        let p = tmp("a", csv);
        let spec = AdapterSpec {
            adapter: "generic_csv".into(),
            venue: "HYPERLIQUID".into(),
            path: p.display().to_string(),
            ..Default::default()
        };
        let evs = GenericCsvAdapter::new().load(&spec).unwrap();
        assert_eq!(evs.len(), 2);
        assert!(evs.iter().all(|e| e.instrument() == "HYPERLIQUID:ETH-PERP"));
        let trades = evs.iter().filter(|e| matches!(e, MarketEvent::Trade(_))).count();
        assert_eq!(trades, 1);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn price_scale_bps_and_prob() {
        let csv = "ts_ns,instrument,side,price,size\n\
                   1,S,BUY,4200,1\n";
        let p = tmp("bps", csv);
        let mut mapping = BTreeMap::new();
        mapping.insert("price_scale".to_string(), "bps".to_string());
        let spec = AdapterSpec {
            adapter: "generic_csv".into(),
            path: p.display().to_string(),
            mapping,
            ..Default::default()
        };
        let evs = GenericCsvAdapter::new().load(&spec).unwrap();
        match &evs[0] {
            MarketEvent::Delta(d) => assert_eq!(d.price, Cents(42)),
            _ => panic!(),
        }
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn side_from_sign_csv() {
        let csv = "ts_ns,instrument,price,signed_qty\n\
                   1,S,0.40,8\n\
                   2,S,0.60,-3\n";
        let p = tmp("sign", csv);
        let mut mapping = BTreeMap::new();
        mapping.insert("side_from_sign".to_string(), "signed_qty".to_string());
        let spec = AdapterSpec {
            adapter: "generic_csv".into(),
            path: p.display().to_string(),
            mapping,
            ..Default::default()
        };
        let evs = GenericCsvAdapter::new().load(&spec).unwrap();
        assert_eq!(evs.len(), 2);
        match (&evs[0], &evs[1]) {
            (MarketEvent::Delta(a), MarketEvent::Delta(b)) => {
                assert_eq!(a.side, Side::Bid);
                assert_eq!(a.size, 8.0);
                assert_eq!(b.side, Side::Ask);
                assert_eq!(b.size, 3.0);
            }
            _ => panic!(),
        }
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn honors_mapping_and_ts_unit_seconds() {
        let csv = "t,sym,s,px,q\n\
                   1780478100,SYMC,BUY,0.30,5\n";
        let p = tmp("b", csv);
        let mut mapping = BTreeMap::new();
        for (kk, vv) in [
            ("ts_ns", "t"),
            ("instrument", "sym"),
            ("side", "s"),
            ("price", "px"),
            ("size", "q"),
            ("ts_unit", "s"),
        ] {
            mapping.insert(kk.to_string(), vv.to_string());
        }
        let spec = AdapterSpec {
            adapter: "generic_csv".into(),
            venue: "POLYMARKET".into(),
            path: p.display().to_string(),
            mapping,
            ..Default::default()
        };
        let evs = GenericCsvAdapter::new().load(&spec).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].instrument(), "POLYMARKET:SYMC");
        // seconds were scaled to ns
        assert_eq!(evs[0].ts_ns(), 1780478100i64 * 1_000_000_000);
        std::fs::remove_file(&p).ok();
    }
}
