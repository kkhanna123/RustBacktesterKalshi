//! Pluggable MULTI-VENUE data-adapter framework.
//!
//! This is the extension point for ingesting tick-level data from venues *other* than Kalshi
//! (Polymarket, Hyperliquid, CME, …) and for running cross-venue strategies that observe and trade
//! several venues at once. It is intentionally **non-invasive**: the engine, [`crate::orderbook::BookSet`],
//! and [`crate::strategy::Ctx`] already key everything by an instrument **string**, so all we add is
//!
//! 1. a canonical, venue-tagged instrument id ([`Venue`] + [`Instrument`], rendered as `"VENUE:symbol"`),
//! 2. a [`DataAdapter`] trait every venue implements to normalize its native format into
//!    venue-tagged [`MarketEvent`]s, and
//! 3. an [`AdapterRegistry`] that resolves a venue/source name to its adapter.
//!
//! Because each adapter tags its events with a distinct `"VENUE:symbol"` instrument, multiple venues
//! coexist in one [`crate::orderbook::BookSet`] automatically (different keys → different books), and a
//! single backtest can MERGE events from several adapters into one time-ordered stream. A cross-venue
//! strategy then simply reads `ctx.best_bid("KALSHI:SYMA")` and `ctx.best_bid("POLYMARKET:SYMB")`.
//!
//! # How to add an adapter in 3 steps
//!
//! 1. **Copy the template.** Copy [`template`] (`src/adapters/template.rs`) to
//!    `src/adapters/my_venue.rs`. Rename the struct, set [`DataAdapter::name`] to your venue key
//!    (e.g. `"myvenue"`), and translate your venue's native records into venue-tagged
//!    [`MarketEvent`]s inside [`DataAdapter::load`] (use [`Instrument::new`] /
//!    [`tag_events_with_venue`] so every event's `instrument` is `"MYVENUE:symbol"`).
//! 2. **Declare the module.** Add `pub mod my_venue;` below and register it in
//!    [`AdapterRegistry::with_builtins`] (one `reg.register(Box::new(my_venue::MyVenueAdapter::new()))`).
//! 3. **Use it.** Run `kalshi-backtest backtest --source adapter --venue myvenue --adapter-path data.ndjson …`,
//!    or list it under `sources = [...]` in a `--config` run spec to merge it with other venues.
//!
//! That's it — no engine changes are ever needed.

use crate::data::sort_events;
use crate::types::{BookDelta, MarketEvent, TradeEvent};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

pub mod generic_csv;
pub mod generic_ndjson;
pub mod hyperliquid;
pub mod kalshi;
pub mod polymarket;
pub mod profile;
pub mod template;

// ============================================================================
// Venue + Instrument: the canonical venue-tagged instrument id.
// ============================================================================

/// A trading venue. The string form (used as the prefix in `"VENUE:symbol"`) is the UPPERCASE
/// variant name, except [`Venue::Generic`] which carries an arbitrary uppercase tag.
///
/// Adding a first-class venue is optional — [`Venue::Generic`] handles any venue by name — but a
/// named variant gives a stable, typo-proof key and a place to hang venue-specific behavior.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Venue {
    /// Kalshi binary event contracts (the original, built-in venue).
    Kalshi,
    /// Polymarket CLOB (binary outcome tokens).
    Polymarket,
    /// Hyperliquid perpetuals (L2 book).
    Hyperliquid,
    /// CME futures/options.
    Cme,
    /// Any other venue, identified by an uppercase name tag.
    Generic(String),
}

impl Venue {
    /// The canonical uppercase string tag for this venue (the prefix before `:` in an instrument id).
    pub fn tag(&self) -> String {
        match self {
            Venue::Kalshi => "KALSHI".to_string(),
            Venue::Polymarket => "POLYMARKET".to_string(),
            Venue::Hyperliquid => "HYPERLIQUID".to_string(),
            Venue::Cme => "CME".to_string(),
            Venue::Generic(s) => s.to_uppercase(),
        }
    }

    /// Parse a venue from its string tag (case-insensitive). Unknown tags become [`Venue::Generic`].
    pub fn from_tag(s: &str) -> Venue {
        match s.to_uppercase().as_str() {
            "KALSHI" => Venue::Kalshi,
            "POLYMARKET" | "POLY" => Venue::Polymarket,
            "HYPERLIQUID" | "HL" => Venue::Hyperliquid,
            "CME" => Venue::Cme,
            other => Venue::Generic(other.to_string()),
        }
    }
}

impl fmt::Display for Venue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.tag())
    }
}

impl FromStr for Venue {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Venue::from_tag(s))
    }
}

/// A canonical, venue-tagged instrument id. Renders as `"VENUE:symbol"` (e.g.
/// `"KALSHI:KXNATGASD-26JUN0417-T2.700"`, `"POLYMARKET:0xabc…"`).
///
/// This is the value carried in every [`MarketEvent`]'s `instrument` field once it has passed
/// through an adapter, and therefore the key under which its book lives in the
/// [`crate::orderbook::BookSet`] and the key a strategy uses with [`crate::strategy::Ctx`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instrument {
    pub venue: Venue,
    pub symbol: String,
}

impl Instrument {
    /// Build an instrument from a venue and a venue-native symbol.
    pub fn new(venue: Venue, symbol: impl Into<String>) -> Self {
        Instrument {
            venue,
            symbol: symbol.into(),
        }
    }

    /// The canonical `"VENUE:symbol"` string used as the engine/book/ctx key.
    pub fn id(&self) -> String {
        format!("{}:{}", self.venue.tag(), self.symbol)
    }

    /// Parse a canonical id. If there is no `':'`, the whole string is treated as a Kalshi symbol
    /// (so bare Kalshi ids like `"KXNATGASD-…"` keep working unchanged — backward compatible).
    /// Only the FIRST `':'` splits venue from symbol, so symbols may themselves contain colons.
    pub fn parse(id: &str) -> Instrument {
        match id.split_once(':') {
            Some((v, sym)) => Instrument::new(Venue::from_tag(v), sym),
            None => Instrument::new(Venue::Kalshi, id),
        }
    }

    /// True if `id` belongs to `venue` once parsed.
    pub fn id_is_venue(id: &str, venue: &Venue) -> bool {
        &Instrument::parse(id).venue == venue
    }
}

impl fmt::Display for Instrument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id())
    }
}

impl FromStr for Instrument {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Instrument::parse(s))
    }
}

// ============================================================================
// AdapterSpec: how a single source is configured (one entry in a multi-venue run).
// ============================================================================

/// Configuration for ONE data source in a (possibly multi-venue) backtest. Each spec names the
/// adapter to use, the venue tag to stamp on its events, the path/url to read, and the loader
/// knobs (instrument filter, date range). A run merges the events of one or more specs.
///
/// Field-mapping knobs (`mapping`) are consumed only by the schema-configurable generic adapters
/// ([`generic_csv`], [`generic_ndjson`]); first-class adapters ignore them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AdapterSpec {
    /// Adapter key (must match a registered [`DataAdapter::name`], e.g. `"kalshi_ndjson"`,
    /// `"generic_csv"`, `"polymarket"`).
    pub adapter: String,
    /// Venue tag to stamp on emitted events (e.g. `"KALSHI"`, `"POLYMARKET"`). If empty, the
    /// adapter's natural/default venue is used.
    pub venue: String,
    /// File path or URL the adapter reads.
    pub path: String,
    /// Instrument glob applied AFTER venue tagging is stripped (matches the venue-native symbol),
    /// exact or trailing `%`/`*`. None = all.
    pub instrument: Option<String>,
    /// Inclusive start date `YYYY-MM-DD` (None = open-ended).
    pub start: Option<String>,
    /// Exclusive end date `YYYY-MM-DD` (None = open-ended).
    pub end: Option<String>,
    /// Optional column/field name mapping for the generic CSV/NDJSON adapters (key → source column).
    pub mapping: BTreeMap<String, String>,
}

impl AdapterSpec {
    /// The resolved venue for this spec: the explicit `venue` tag if set, else `default`.
    pub fn resolved_venue(&self, default: Venue) -> Venue {
        if self.venue.trim().is_empty() {
            default
        } else {
            Venue::from_tag(self.venue.trim())
        }
    }
}

// ============================================================================
// DataAdapter: the extension point.
// ============================================================================

/// One mapping key an adapter understands, with a short note on what it does. Surfaced by
/// `kalshi-backtest list-adapters` so a new user can SEE which `mapping` knobs exist (and their
/// defaults) before they ever touch a file. Only the schema-configurable generic adapters populate
/// this; first-class adapters return an empty list.
#[derive(Debug, Clone, Copy)]
pub struct MappingKey {
    /// The canonical mapping key (what goes on the LEFT of `key=value`).
    pub key: &'static str,
    /// One-line description, ideally including the default / accepted values.
    pub note: &'static str,
}

/// Discoverability metadata for an adapter: a one-line description plus the `mapping` keys it
/// understands. Drives `list-adapters` exactly like [`crate::strategies::StrategyInfo`] drives
/// `list-strategies`. Defaulted on the trait so existing adapters need no change.
#[derive(Debug, Clone)]
pub struct AdapterInfo {
    /// Registry key (== [`DataAdapter::name`]).
    pub name: String,
    /// Default venue tag stamped on events unless the [`AdapterSpec`] overrides it.
    pub default_venue: String,
    /// One-line human description.
    pub description: String,
    /// The `mapping` keys this adapter reads (empty for adapters that ignore `mapping`).
    pub mapping_keys: Vec<MappingKey>,
}

/// A pluggable data adapter for ONE venue / wire format. Implementors normalize their native data
/// into venue-tagged [`MarketEvent`]s so the engine stays source-agnostic.
///
/// The contract:
/// - [`name`](DataAdapter::name) returns the adapter's registry key.
/// - [`default_venue`](DataAdapter::default_venue) is the venue this adapter tags with unless the
///   [`AdapterSpec`] overrides it.
/// - [`load`](DataAdapter::load) reads `spec.path` and returns a **time-sorted** `Vec<MarketEvent>`
///   whose `instrument` fields are already canonical `"VENUE:symbol"` ids.
/// - [`description`](DataAdapter::description) / [`mapping_keys`](DataAdapter::mapping_keys) power
///   `list-adapters` discoverability (both have sensible defaults, so adapters may skip them).
pub trait DataAdapter {
    /// Registry key for this adapter (matches `--adapter` / the `adapter` field of an [`AdapterSpec`]).
    fn name(&self) -> &str;

    /// The venue this adapter tags events with when the spec doesn't override it.
    fn default_venue(&self) -> Venue;

    /// Load and normalize events from `spec.path`, returning a time-sorted, venue-tagged stream.
    fn load(&self, spec: &AdapterSpec) -> Result<Vec<MarketEvent>>;

    /// One-line description for `list-adapters`. Defaults to a generic line; override for clarity.
    fn description(&self) -> &str {
        "(no description provided)"
    }

    /// The `mapping` keys this adapter understands, for `list-adapters`. Defaults to none (the
    /// first-class adapters ignore `mapping`); the generic adapters override this.
    fn mapping_keys(&self) -> &'static [MappingKey] {
        &[]
    }

    /// Bundle [`name`](DataAdapter::name) / [`default_venue`](DataAdapter::default_venue) /
    /// [`description`](DataAdapter::description) / [`mapping_keys`](DataAdapter::mapping_keys) into an
    /// [`AdapterInfo`] for the CLI. Rarely overridden.
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: self.name().to_string(),
            default_venue: self.default_venue().tag(),
            description: self.description().to_string(),
            mapping_keys: self.mapping_keys().to_vec(),
        }
    }
}

// ============================================================================
// Helpers for adapter authors: stamp the venue onto events.
// ============================================================================

/// Rewrite each event's `instrument` to the canonical `"VENUE:symbol"` form using `venue` and the
/// event's existing (venue-native) symbol. Idempotent-ish: if the symbol already carries the SAME
/// venue prefix it is left unchanged, so double-tagging is safe.
pub fn tag_events_with_venue(events: &mut [MarketEvent], venue: &Venue) {
    for ev in events.iter_mut() {
        match ev {
            MarketEvent::Delta(d) => d.instrument = retag(&d.instrument, venue),
            MarketEvent::Trade(t) => t.instrument = retag(&t.instrument, venue),
        }
    }
}

/// Build the canonical id for `symbol` under `venue`, avoiding a double `VENUE:VENUE:` prefix when
/// `symbol` is already tagged with the same venue.
fn retag(symbol: &str, venue: &Venue) -> String {
    let parsed = Instrument::parse(symbol);
    // If the symbol already parses to this venue with a non-trivial prefix, keep it.
    if symbol.contains(':') && &parsed.venue == venue {
        return symbol.to_string();
    }
    Instrument::new(venue.clone(), symbol).id()
}

// ============================================================================
// Shared price-scaling for the generic adapters.
// ============================================================================

use crate::types::Cents;

/// How a generic adapter's raw `price` number maps to internal cents. Configured by the
/// `price_scale` mapping key (`dollars|cents|bps|prob`), generalizing the older `price_is_cents`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceScale {
    /// 0.42 → 42¢ (the default; Kalshi/Polymarket dollar prices in [0,1]).
    Dollars,
    /// 42 → 42¢ (price already in integer cents).
    Cents,
    /// 4200 → 42¢ (basis points: 1¢ == 100 bps).
    Bps,
    /// 0.42 → 42¢ (probability in [0,1]; identical to `dollars` but named for clarity).
    Prob,
}

impl PriceScale {
    /// Resolve the price scale from a `mapping`: an explicit `price_scale` wins, else the legacy
    /// `price_is_cents=true|1` selects `Cents`, else the default `Dollars`. Errors on an unknown
    /// `price_scale` value so a typo is caught early rather than silently mis-scaling every price.
    pub fn from_mapping(mapping: &BTreeMap<String, String>) -> Result<PriceScale> {
        if let Some(raw) = mapping.get("price_scale") {
            return match raw.trim().to_lowercase().as_str() {
                "dollars" | "dollar" | "usd" => Ok(PriceScale::Dollars),
                "cents" | "cent" => Ok(PriceScale::Cents),
                "bps" | "bp" => Ok(PriceScale::Bps),
                "prob" | "probability" => Ok(PriceScale::Prob),
                other => Err(anyhow::anyhow!(
                    "unknown price_scale '{other}' — expected dollars|cents|bps|prob"
                )),
            };
        }
        let price_is_cents = mapping
            .get("price_is_cents")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        Ok(if price_is_cents { PriceScale::Cents } else { PriceScale::Dollars })
    }

    /// Convert a raw price number to internal [`Cents`] under this scale.
    pub fn to_cents(self, raw: f64) -> Cents {
        match self {
            PriceScale::Dollars | PriceScale::Prob => Cents::from_dollars(raw),
            PriceScale::Cents => Cents(raw.round() as i32),
            PriceScale::Bps => Cents((raw / 100.0).round() as i32),
        }
    }
}

/// Parse an optional `YYYY-MM-DD` date into epoch nanoseconds (UTC midnight). `None` → `None`.
/// Shared by every adapter so date-range filtering is consistent with the rest of the CLI.
pub fn date_to_ns_opt(date: &Option<String>) -> Result<Option<i64>> {
    match date {
        None => Ok(None),
        Some(d) => Ok(Some(date_to_ns(d)?)),
    }
}

/// Parse a `YYYY-MM-DD` date into epoch nanoseconds (UTC midnight).
pub fn date_to_ns(date: &str) -> Result<i64> {
    use chrono::{NaiveDate, NaiveTime};
    let d = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
        .map_err(|e| anyhow::anyhow!("bad date '{date}' (expected YYYY-MM-DD): {e}"))?;
    let dt = d.and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    Ok(dt.and_utc().timestamp_nanos_opt().unwrap_or(0))
}

/// Convenience for adapters that build raw `BookDelta`/`TradeEvent` lists: tag + sort in one call.
pub fn finalize_events(mut deltas: Vec<BookDelta>, trades: Vec<TradeEvent>, venue: &Venue) -> Vec<MarketEvent> {
    let mut events: Vec<MarketEvent> = Vec::with_capacity(deltas.len() + trades.len());
    for d in deltas.drain(..) {
        events.push(MarketEvent::Delta(d));
    }
    for t in trades {
        events.push(MarketEvent::Trade(t));
    }
    tag_events_with_venue(&mut events, venue);
    sort_events(events)
}

// ============================================================================
// AdapterRegistry: resolve venue/source name -> adapter.
// ============================================================================

/// Maps an adapter name to its [`DataAdapter`]. Built once (with all built-ins) and queried by the
/// CLI / config loader to resolve `--adapter` keys.
#[derive(Default)]
pub struct AdapterRegistry {
    adapters: Vec<Box<dyn DataAdapter>>,
}

impl AdapterRegistry {
    /// An empty registry (no adapters). Use [`with_builtins`](AdapterRegistry::with_builtins) for the
    /// usual case.
    pub fn new() -> Self {
        AdapterRegistry {
            adapters: Vec::new(),
        }
    }

    /// A registry pre-loaded with every built-in adapter (Kalshi tick NDJSON, generic CSV/NDJSON,
    /// and the Polymarket/Hyperliquid stubs). This is the single place new adapters are registered.
    pub fn with_builtins() -> Self {
        let mut reg = AdapterRegistry::new();
        reg.register(Box::new(kalshi::KalshiNdjsonAdapter::new()));
        reg.register(Box::new(generic_ndjson::GenericNdjsonAdapter::new()));
        reg.register(Box::new(generic_csv::GenericCsvAdapter::new()));
        reg.register(Box::new(polymarket::PolymarketAdapter::new()));
        reg.register(Box::new(hyperliquid::HyperliquidAdapter::new()));
        reg
    }

    /// Register an adapter under its [`DataAdapter::name`]. A later registration with the same name
    /// shadows the earlier one.
    pub fn register(&mut self, adapter: Box<dyn DataAdapter>) {
        self.adapters.push(adapter);
    }

    /// Resolve an adapter by name (last registration wins). None if unknown.
    pub fn get(&self, name: &str) -> Option<&dyn DataAdapter> {
        self.adapters
            .iter()
            .rev()
            .find(|a| a.name() == name)
            .map(|b| b.as_ref())
    }

    /// Discoverability [`AdapterInfo`] for every registered adapter (deduplicated by name, latest
    /// registration kept), in registration order. Powers `kalshi-backtest list-adapters`.
    pub fn infos(&self) -> Vec<AdapterInfo> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for a in &self.adapters {
            if seen.insert(a.name().to_string()) {
                out.push(a.info());
            }
        }
        out
    }

    /// All registered adapter names, in registration order, deduplicated (latest kept).
    pub fn names(&self) -> Vec<&str> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for a in &self.adapters {
            if seen.insert(a.name()) {
                out.push(a.name());
            }
        }
        out
    }

    /// Load a single source by spec, resolving its adapter. Errors if the adapter is unknown.
    pub fn load_spec(&self, spec: &AdapterSpec) -> Result<Vec<MarketEvent>> {
        let adapter = self.get(&spec.adapter).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown adapter '{}' — known adapters: {}",
                spec.adapter,
                self.names().join(", ")
            )
        })?;
        adapter.load(spec)
    }

    /// Load and MERGE several sources into ONE time-ordered, venue-tagged stream. Each spec's events
    /// are already tagged + sorted by its adapter; here we concatenate and stable-sort by timestamp
    /// so a single backtest sees a coherent multi-venue stream. This is what powers cross-venue
    /// strategies.
    pub fn load_merged(&self, specs: &[AdapterSpec]) -> Result<Vec<MarketEvent>> {
        let mut all: Vec<MarketEvent> = Vec::new();
        for spec in specs {
            let mut evs = self.load_spec(spec)?;
            all.append(&mut evs);
        }
        Ok(sort_events(all))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Action, Cents, Side};

    #[test]
    fn instrument_parse_display_roundtrip() {
        let cases = [
            ("KALSHI:KXNATGASD-26JUN0417-T2.700", Venue::Kalshi, "KXNATGASD-26JUN0417-T2.700"),
            ("POLYMARKET:0xabc123", Venue::Polymarket, "0xabc123"),
            ("HYPERLIQUID:BTC-PERP", Venue::Hyperliquid, "BTC-PERP"),
            ("CME:ESZ5", Venue::Cme, "ESZ5"),
            ("FOO:bar", Venue::Generic("FOO".into()), "bar"),
        ];
        for (id, venue, sym) in cases {
            let inst = Instrument::parse(id);
            assert_eq!(inst.venue, venue, "venue for {id}");
            assert_eq!(inst.symbol, sym, "symbol for {id}");
            // Display / id round-trips exactly.
            assert_eq!(inst.id(), id);
            assert_eq!(inst.to_string(), id);
            // FromStr matches parse.
            let viafs: Instrument = id.parse().unwrap();
            assert_eq!(viafs, inst);
        }
    }

    #[test]
    fn bare_id_defaults_to_kalshi_and_keeps_colons_in_symbol() {
        // A bare (untagged) id is treated as a Kalshi symbol for backward compatibility.
        let inst = Instrument::parse("KXNATGASD-X");
        assert_eq!(inst.venue, Venue::Kalshi);
        assert_eq!(inst.symbol, "KXNATGASD-X");
        // Only the first colon splits, so a symbol may contain further colons.
        let inst2 = Instrument::parse("POLYMARKET:cond:0xdead");
        assert_eq!(inst2.venue, Venue::Polymarket);
        assert_eq!(inst2.symbol, "cond:0xdead");
        assert_eq!(inst2.id(), "POLYMARKET:cond:0xdead");
    }

    #[test]
    fn venue_tag_roundtrip_and_aliases() {
        assert_eq!(Venue::from_tag("poly"), Venue::Polymarket);
        assert_eq!(Venue::from_tag("HL"), Venue::Hyperliquid);
        assert_eq!(Venue::Generic("xyz".into()).tag(), "XYZ");
        assert_eq!(Venue::Kalshi.to_string(), "KALSHI");
    }

    #[test]
    fn tag_events_stamps_and_is_idempotent() {
        let mut evs = vec![MarketEvent::Delta(BookDelta {
            ts_ns: 1,
            instrument: "SYMA".into(),
            action: Action::Add,
            side: Side::Bid,
            price: Cents(40),
            size: 10.0,
            sequence: 1,
            is_snapshot: false,
        })];
        tag_events_with_venue(&mut evs, &Venue::Polymarket);
        assert_eq!(evs[0].instrument(), "POLYMARKET:SYMA");
        // tagging again with the same venue does not double-prefix
        tag_events_with_venue(&mut evs, &Venue::Polymarket);
        assert_eq!(evs[0].instrument(), "POLYMARKET:SYMA");
    }

    #[test]
    fn registry_resolution() {
        let reg = AdapterRegistry::with_builtins();
        for name in [
            "kalshi_ndjson",
            "generic_ndjson",
            "generic_csv",
            "polymarket",
            "hyperliquid",
        ] {
            assert!(reg.get(name).is_some(), "adapter {name} should resolve");
        }
        assert!(reg.get("does_not_exist").is_none());
        // names() lists them
        assert!(reg.names().contains(&"generic_csv"));
    }

    #[test]
    fn cross_venue_merge_is_time_ordered_and_multi_venue() {
        use std::io::Write;
        // Two synthetic NDJSON files for two venues, with interleaved timestamps.
        fn write(tag: &str, lines: &str) -> std::path::PathBuf {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let mut p = std::env::temp_dir();
            p.push(format!("merge_{}_{tag}_{n}.ndjson", std::process::id()));
            std::fs::File::create(&p).unwrap().write_all(lines.as_bytes()).unwrap();
            p
        }
        let a = write(
            "kalshi",
            "{\"kind\":\"delta\",\"ts_ns\":1000,\"instrument\":\"SYMA\",\"action\":\"ADD\",\"side\":\"BUY\",\"price\":0.40,\"size\":10,\"sequence\":1,\"is_snapshot\":1}\n\
             {\"kind\":\"delta\",\"ts_ns\":3000,\"instrument\":\"SYMA\",\"action\":\"ADD\",\"side\":\"ASK\",\"price\":0.60,\"size\":10,\"sequence\":2}\n",
        );
        let b = write(
            "poly",
            "{\"kind\":\"delta\",\"ts_ns\":2000,\"instrument\":\"SYMB\",\"action\":\"ADD\",\"side\":\"BUY\",\"price\":0.45,\"size\":10,\"sequence\":1,\"is_snapshot\":1}\n\
             {\"kind\":\"delta\",\"ts_ns\":4000,\"instrument\":\"SYMB\",\"action\":\"ADD\",\"side\":\"ASK\",\"price\":0.55,\"size\":10,\"sequence\":2}\n",
        );
        let reg = AdapterRegistry::with_builtins();
        let specs = vec![
            AdapterSpec {
                adapter: "generic_ndjson".into(),
                venue: "KALSHI".into(),
                path: a.display().to_string(),
                ..Default::default()
            },
            AdapterSpec {
                adapter: "generic_ndjson".into(),
                venue: "POLYMARKET".into(),
                path: b.display().to_string(),
                ..Default::default()
            },
        ];
        let merged = reg.load_merged(&specs).unwrap();
        // 4 events, strictly time-ordered, interleaving the two venues.
        assert_eq!(merged.len(), 4);
        let ts: Vec<i64> = merged.iter().map(|e| e.ts_ns()).collect();
        assert_eq!(ts, vec![1000, 2000, 3000, 4000], "merged stream must be time-ordered");
        let venues: Vec<&str> = merged
            .iter()
            .map(|e| e.instrument().split_once(':').unwrap().0)
            .collect();
        assert_eq!(venues, vec!["KALSHI", "POLYMARKET", "KALSHI", "POLYMARKET"]);
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn id_is_venue_helper() {
        assert!(Instrument::id_is_venue("KALSHI:X", &Venue::Kalshi));
        assert!(!Instrument::id_is_venue("KALSHI:X", &Venue::Polymarket));
        assert!(Instrument::id_is_venue("X", &Venue::Kalshi)); // bare => kalshi
    }
}
