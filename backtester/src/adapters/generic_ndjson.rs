//! GENERIC NDJSON adapter — ingest ANY venue's tick data from newline-delimited JSON with a
//! configurable field mapping. This is the "super easy to add a venue" path: if a new source can
//! emit one JSON object per line, it needs **zero Rust** — just point this adapter at it (and,
//! if the field names differ from the defaults, supply a `mapping`).
//!
//! ## Record shape (defaults)
//! Each line is a JSON object describing either a book delta or a trade:
//! ```json
//! {"kind":"delta","ts_ns":1000,"instrument":"BTC-PERP","action":"ADD","side":"BUY","price":0.42,"size":100,"sequence":1,"is_snapshot":1}
//! {"kind":"trade","ts_ns":2000,"instrument":"BTC-PERP","price":0.55,"size":3,"aggressor_side":"yes","trade_id":"t1"}
//! ```
//! This is exactly the Kalshi capture schema, so the generic adapter is a strict superset of
//! [`crate::data::ndjson`] with the venue prefix added and the field names made configurable.
//!
//! ## Field mapping (`AdapterSpec::mapping`)
//! Override any source field name. Keys are the *canonical* names; values are the *source* keys:
//! `{"ts_ns":"timestamp","instrument":"symbol","price":"px","size":"qty"}`. Unmapped keys use the
//! default name. Source keys may be **dotted paths** to read nested JSON, e.g. `price = "data.px"`
//! reads `{"data":{"px":0.42}}`.
//!
//! ## Price unit (`price_scale`)
//! `price_scale` controls how the raw `price` number maps to internal cents:
//! `dollars` (default, 0.42 → 42¢), `cents` (42 → 42¢), `bps` (4200 → 42¢), `prob` (0.42 → 42¢).
//! The older `price_is_cents=true` still works (it is an alias for `price_scale=cents`).
//!
//! ## Side / action / aggressor vocab
//! `side`: BUY/BID/YES → Bid, SELL/ASK/NO → Ask. `action`: ADD/UPDATE/DELETE (default ADD).
//! `aggressor_side`: yes/buy/bid → aggressor took YES. All case-insensitive.
//!
//! ## Sign-derived side (`side_from_sign`)
//! When there is no explicit side column, set `side_from_sign=<column>` to derive the side from the
//! SIGN of a signed size/qty column: a positive value → Bid, negative → Ask, and the size used is
//! the absolute value. (If both an explicit `side` and `side_from_sign` resolve, the explicit side
//! wins.)

use crate::adapters::{date_to_ns_opt, finalize_events, AdapterSpec, DataAdapter, MappingKey, Venue};
use crate::data::glob_match;
use crate::types::{Action, BookDelta, MarketEvent, Side, TradeEvent};
use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// The `mapping` keys the generic NDJSON adapter understands (surfaced by `list-adapters`).
pub const MAPPING_KEYS: &[MappingKey] = &[
    MappingKey { key: "ts_ns", note: "source key for the timestamp in nanoseconds (default 'ts_ns')" },
    MappingKey { key: "instrument", note: "source key for the symbol (default 'instrument'); dotted paths ok" },
    MappingKey { key: "kind", note: "source key for delta|trade routing (default 'kind'; default value 'delta')" },
    MappingKey { key: "price", note: "source key for price (default 'price'); dotted paths ok" },
    MappingKey { key: "size", note: "source key for size (default 'size')" },
    MappingKey { key: "side", note: "source key for side BUY/SELL/YES/NO/bid/ask (default 'side')" },
    MappingKey { key: "action", note: "source key for ADD|UPDATE|DELETE (default 'action' → ADD)" },
    MappingKey { key: "sequence", note: "source key for the i64 sequence (default 'sequence'; auto-increments if absent)" },
    MappingKey { key: "is_snapshot", note: "source key for the snapshot flag (default 'is_snapshot')" },
    MappingKey { key: "aggressor_side", note: "trade-only: source key for the aggressor side (default 'aggressor_side')" },
    MappingKey { key: "trade_id", note: "trade-only: source key for the trade id (default 'trade_id')" },
    MappingKey { key: "price_scale", note: "price unit: dollars|cents|bps|prob (default 'dollars')" },
    MappingKey { key: "price_is_cents", note: "alias: 'true' == price_scale=cents (default 'false')" },
    MappingKey { key: "side_from_sign", note: "derive side from the SIGN of this signed column (no explicit side)" },
];

/// A configurable NDJSON adapter that works for any venue out of the box.
pub struct GenericNdjsonAdapter;

impl GenericNdjsonAdapter {
    pub fn new() -> Self {
        GenericNdjsonAdapter
    }
}

impl Default for GenericNdjsonAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve a canonical field name to its source key via the mapping (default = canonical name).
fn key<'a>(mapping: &'a BTreeMap<String, String>, canonical: &'a str) -> &'a str {
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

impl DataAdapter for GenericNdjsonAdapter {
    fn name(&self) -> &str {
        "generic_ndjson"
    }
    fn default_venue(&self) -> Venue {
        Venue::Generic("GENERIC".into())
    }
    fn description(&self) -> &str {
        "Any venue's tick data as newline-delimited JSON (one delta/trade per line; zero Rust)."
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
        // Optional: derive the side from the SIGN of a signed size/qty column.
        let side_sign_key = m.get("side_from_sign").map(|s| s.as_str());

        let path = Path::new(&spec.path);
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let reader: Box<dyn Read> = if path.extension().and_then(|e| e.to_str()) == Some("gz") {
            Box::new(GzDecoder::new(file))
        } else {
            Box::new(file)
        };
        let buf = BufReader::new(reader);

        let mut deltas: Vec<BookDelta> = Vec::new();
        let mut trades: Vec<TradeEvent> = Vec::new();
        let mut seq_fallback: i64 = 0;

        for line in buf.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: serde_json::Value =
                serde_json::from_str(line).with_context(|| format!("bad json line: {line}"))?;

            let symbol = get_path(&v, key(m, "instrument"))
                .and_then(|x| x.as_str())
                .ok_or_else(|| anyhow!("missing instrument field in: {line}"))?
                .to_string();
            // instrument filter applies to the venue-native symbol BEFORE tagging
            if let Some(pat) = &spec.instrument {
                if !glob_match(pat, &symbol) {
                    continue;
                }
            }
            let ts_ns = num_i64(&v, key(m, "ts_ns")).ok_or_else(|| anyhow!("missing ts_ns in: {line}"))?;
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

            let price = num_f64(&v, key(m, "price")).unwrap_or(0.0);
            let cents = price_scale.to_cents(price);
            let mut size = num_f64(&v, key(m, "size")).unwrap_or(0.0);

            let kind = get_path(&v, key(m, "kind")).and_then(|k| k.as_str()).unwrap_or("delta");
            match kind {
                "trade" => {
                    let aggressor_yes = get_path(&v, key(m, "aggressor_side"))
                        .and_then(|x| x.as_str())
                        .map(|s| matches!(s.to_uppercase().as_str(), "YES" | "BUY" | "BID"))
                        .unwrap_or(true);
                    let trade_id = get_path(&v, key(m, "trade_id"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    trades.push(TradeEvent {
                        ts_ns,
                        instrument: symbol,
                        aggressor_yes,
                        price: cents,
                        size,
                        trade_id,
                    });
                }
                _ => {
                    // Side: an explicit `side` token wins; otherwise derive it from the SIGN of the
                    // `side_from_sign` column (positive → Bid, negative → Ask, |value| → size).
                    let explicit_side = get_path(&v, key(m, "side"))
                        .and_then(|x| x.as_str())
                        .and_then(map_side);
                    let side = match explicit_side {
                        Some(s) => s,
                        None => match side_sign_key {
                            Some(sk) => {
                                let signed = num_f64(&v, sk).ok_or_else(|| {
                                    anyhow!("side_from_sign column '{sk}' missing/non-numeric in: {line}")
                                })?;
                                if signed == 0.0 {
                                    bail!("side_from_sign column '{sk}' is 0 (no sign) in: {line}");
                                }
                                size = signed.abs();
                                if signed > 0.0 { Side::Bid } else { Side::Ask }
                            }
                            None => bail!("bad/missing side in: {line}"),
                        },
                    };
                    let action = get_path(&v, key(m, "action"))
                        .and_then(|x| x.as_str())
                        .map(map_action)
                        .unwrap_or(Action::Add);
                    let sequence = num_i64(&v, key(m, "sequence")).unwrap_or_else(|| {
                        seq_fallback += 1;
                        seq_fallback
                    });
                    let is_snapshot = get_path(&v, key(m, "is_snapshot"))
                        .map(|x| x.as_i64().unwrap_or(0) != 0 || x.as_bool().unwrap_or(false))
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
        }
        Ok(finalize_events(deltas, trades, &venue))
    }
}

/// Look up a (possibly DOTTED) key path in a JSON object, e.g. `"data.px"` reads `v["data"]["px"]`.
/// A key with no `.` is a plain top-level lookup, so existing flat mappings are unchanged.
fn get_path<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    if !path.contains('.') {
        return v.get(path);
    }
    let mut cur = v;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

fn num_i64(v: &serde_json::Value, k: &str) -> Option<i64> {
    match get_path(v, k) {
        Some(serde_json::Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn num_f64(v: &serde_json::Value, k: &str) -> Option<f64> {
    match get_path(v, k) {
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        _ => None,
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
        p.push(format!("gen_ndjson_{}_{tag}_{n}.ndjson", std::process::id()));
        std::fs::File::create(&p).unwrap().write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn loads_venue_tagged_events_default_schema() {
        let p = tmp(
            "a",
            "{\"kind\":\"delta\",\"ts_ns\":1000,\"instrument\":\"BTC-PERP\",\"action\":\"ADD\",\"side\":\"BUY\",\"price\":0.42,\"size\":100,\"sequence\":1,\"is_snapshot\":1}\n\
             {\"kind\":\"trade\",\"ts_ns\":2000,\"instrument\":\"BTC-PERP\",\"price\":0.55,\"size\":3,\"aggressor_side\":\"no\",\"trade_id\":\"t1\"}\n",
        );
        let spec = AdapterSpec {
            adapter: "generic_ndjson".into(),
            venue: "HYPERLIQUID".into(),
            path: p.display().to_string(),
            ..Default::default()
        };
        let evs = GenericNdjsonAdapter::new().load(&spec).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].instrument(), "HYPERLIQUID:BTC-PERP");
        match &evs[0] {
            MarketEvent::Delta(d) => {
                assert_eq!(d.price, Cents(42));
                assert_eq!(d.side, Side::Bid);
                assert!(d.is_snapshot);
            }
            _ => panic!("expected delta first"),
        }
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn price_scale_variants() {
        // bps: 4200 bps -> 42c ; prob: 0.42 -> 42c.
        for (scale, raw, want) in [("bps", "4200", 42), ("prob", "0.42", 42), ("cents", "55", 55)] {
            let p = tmp(
                "scale",
                &format!(
                    "{{\"kind\":\"delta\",\"ts_ns\":1,\"instrument\":\"S\",\"side\":\"BUY\",\"price\":{raw},\"size\":1}}\n"
                ),
            );
            let mut mapping = BTreeMap::new();
            mapping.insert("price_scale".to_string(), scale.to_string());
            let spec = AdapterSpec {
                adapter: "generic_ndjson".into(),
                path: p.display().to_string(),
                mapping,
                ..Default::default()
            };
            let evs = GenericNdjsonAdapter::new().load(&spec).unwrap();
            match &evs[0] {
                MarketEvent::Delta(d) => assert_eq!(d.price, Cents(want), "scale {scale}"),
                _ => panic!(),
            }
            std::fs::remove_file(&p).ok();
        }
    }

    #[test]
    fn side_from_sign_derives_side_and_abs_size() {
        let p = tmp(
            "sign",
            "{\"ts_ns\":1,\"instrument\":\"S\",\"price\":0.40,\"signed_qty\":7}\n\
             {\"ts_ns\":2,\"instrument\":\"S\",\"price\":0.60,\"signed_qty\":-5}\n",
        );
        let mut mapping = BTreeMap::new();
        mapping.insert("side_from_sign".to_string(), "signed_qty".to_string());
        let spec = AdapterSpec {
            adapter: "generic_ndjson".into(),
            path: p.display().to_string(),
            mapping,
            ..Default::default()
        };
        let evs = GenericNdjsonAdapter::new().load(&spec).unwrap();
        assert_eq!(evs.len(), 2);
        match (&evs[0], &evs[1]) {
            (MarketEvent::Delta(a), MarketEvent::Delta(b)) => {
                assert_eq!(a.side, Side::Bid);
                assert_eq!(a.size, 7.0);
                assert_eq!(b.side, Side::Ask);
                assert_eq!(b.size, 5.0); // abs value
            }
            _ => panic!(),
        }
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn dotted_key_paths_read_nested_json() {
        let p = tmp(
            "dotted",
            "{\"meta\":{\"sym\":\"NEST\"},\"t\":9,\"data\":{\"px\":0.33},\"side\":\"SELL\",\"size\":2}\n",
        );
        let mut mapping = BTreeMap::new();
        mapping.insert("instrument".to_string(), "meta.sym".to_string());
        mapping.insert("ts_ns".to_string(), "t".to_string());
        mapping.insert("price".to_string(), "data.px".to_string());
        let spec = AdapterSpec {
            adapter: "generic_ndjson".into(),
            venue: "POLYMARKET".into(),
            path: p.display().to_string(),
            mapping,
            ..Default::default()
        };
        let evs = GenericNdjsonAdapter::new().load(&spec).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].instrument(), "POLYMARKET:NEST");
        match &evs[0] {
            MarketEvent::Delta(d) => {
                assert_eq!(d.price, Cents(33));
                assert_eq!(d.side, Side::Ask);
            }
            _ => panic!(),
        }
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn price_is_cents_alias_still_works() {
        let p = tmp(
            "alias",
            "{\"ts_ns\":1,\"instrument\":\"S\",\"side\":\"BUY\",\"price\":55,\"size\":1}\n",
        );
        let mut mapping = BTreeMap::new();
        mapping.insert("price_is_cents".to_string(), "true".to_string());
        let spec = AdapterSpec {
            adapter: "generic_ndjson".into(),
            path: p.display().to_string(),
            mapping,
            ..Default::default()
        };
        let evs = GenericNdjsonAdapter::new().load(&spec).unwrap();
        match &evs[0] {
            MarketEvent::Delta(d) => assert_eq!(d.price, Cents(55)),
            _ => panic!(),
        }
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn honors_field_mapping_and_cents() {
        // Source uses different field names and integer cents.
        let p = tmp(
            "b",
            "{\"k\":\"delta\",\"t\":5000,\"sym\":\"SYMB\",\"act\":\"ADD\",\"s\":\"SELL\",\"px\":55,\"qty\":7,\"seq\":9}\n",
        );
        let mut mapping = BTreeMap::new();
        for (kk, vv) in [
            ("kind", "k"),
            ("ts_ns", "t"),
            ("instrument", "sym"),
            ("action", "act"),
            ("side", "s"),
            ("price", "px"),
            ("size", "qty"),
            ("sequence", "seq"),
            ("price_is_cents", "true"),
        ] {
            mapping.insert(kk.to_string(), vv.to_string());
        }
        let spec = AdapterSpec {
            adapter: "generic_ndjson".into(),
            venue: "POLYMARKET".into(),
            path: p.display().to_string(),
            mapping,
            ..Default::default()
        };
        let evs = GenericNdjsonAdapter::new().load(&spec).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].instrument(), "POLYMARKET:SYMB");
        match &evs[0] {
            MarketEvent::Delta(d) => {
                assert_eq!(d.price, Cents(55)); // integer cents respected
                assert_eq!(d.side, Side::Ask);
            }
            _ => panic!(),
        }
        std::fs::remove_file(&p).ok();
    }
}
