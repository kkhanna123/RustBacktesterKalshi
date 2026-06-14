//! TEMPLATE ADAPTER — copy this file to `src/adapters/my_venue.rs` to add a new venue.
//!
//! This is the **smallest possible** path to a working data adapter. Read the comments
//! top-to-bottom; every line you'd normally touch is called out. The whole job of an adapter is:
//!
//!   read your venue's native records  →  build `BookDelta` / `TradeEvent`  →  tag with your venue.
//!
//! ## To make your own adapter (3 steps — see `adapters/mod.rs` for the full version)
//! 1. Copy this file, rename the struct (e.g. `MyVenueAdapter`) and the [`DataAdapter::name`]
//!    return value (your registry key, e.g. `"myvenue"`).
//! 2. Set [`DataAdapter::default_venue`] (use a [`Venue`] variant, or `Venue::Generic("MYVENUE")`).
//! 3. Implement [`DataAdapter::load`]: parse `spec.path`, push `BookDelta`/`TradeEvent`s, and return
//!    [`finalize_events`] (which stamps the venue prefix and time-sorts). Then register it in
//!    [`crate::adapters::AdapterRegistry::with_builtins`].
//!
//! The example below "parses" a trivial in-memory format to show the shape; replace `parse_records`
//! with your real reader (a file, an HTTP fetch, a WS capture, …).

use crate::adapters::{finalize_events, AdapterSpec, DataAdapter, Venue};
use crate::types::{Action, BookDelta, Cents, MarketEvent, Side, TradeEvent};
use anyhow::Result;

/// An example adapter for a fictional venue. Copy + rename for a real one.
pub struct TemplateAdapter;

impl TemplateAdapter {
    pub fn new() -> Self {
        TemplateAdapter
    }
}

impl Default for TemplateAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl DataAdapter for TemplateAdapter {
    /// (1) Your registry key — what `--adapter` / the `adapter` config field selects.
    fn name(&self) -> &str {
        "template"
    }

    /// (2) The venue this adapter stamps onto events (unless the spec overrides `venue`).
    fn default_venue(&self) -> Venue {
        Venue::Generic("TEMPLATE".into())
    }

    /// (3) Read `spec.path`, normalize to deltas/trades, tag with the venue, and return time-sorted.
    fn load(&self, spec: &AdapterSpec) -> Result<Vec<MarketEvent>> {
        // The venue to tag with: explicit spec.venue if set, else our default.
        let venue = spec.resolved_venue(self.default_venue());

        // Replace this with your real reader (open spec.path, fetch a url, decode a WS capture …).
        // Here we just synthesize one snapshot bid + one trade so the template is runnable.
        let raw = parse_records(&spec.path);

        let mut deltas: Vec<BookDelta> = Vec::new();
        let mut trades: Vec<TradeEvent> = Vec::new();
        for r in raw {
            match r {
                Record::Quote { ts_ns, symbol, bid_dollars, size } => {
                    deltas.push(BookDelta {
                        ts_ns,
                        instrument: symbol, // venue prefix added by finalize_events
                        action: Action::Add,
                        side: Side::Bid,
                        price: Cents::from_dollars(bid_dollars),
                        size,
                        sequence: ts_ns, // any monotonic-ish value works
                        is_snapshot: true,
                    });
                }
                Record::Trade { ts_ns, symbol, price_dollars, size } => {
                    trades.push(TradeEvent {
                        ts_ns,
                        instrument: symbol,
                        aggressor_yes: true,
                        price: Cents::from_dollars(price_dollars),
                        size,
                        trade_id: format!("{ts_ns}"),
                    });
                }
            }
        }

        // finalize_events stamps "VENUE:symbol" on every event and returns a time-sorted Vec.
        Ok(finalize_events(deltas, trades, &venue))
    }
}

/// A toy native record. Replace with your venue's real message/row type.
enum Record {
    Quote { ts_ns: i64, symbol: String, bid_dollars: f64, size: f64 },
    Trade { ts_ns: i64, symbol: String, price_dollars: f64, size: f64 },
}

/// Stand-in for "read and decode the source". A real adapter reads `path` here.
fn parse_records(_path: &str) -> Vec<Record> {
    vec![
        Record::Quote { ts_ns: 1_000, symbol: "DEMO".into(), bid_dollars: 0.40, size: 100.0 },
        Record::Trade { ts_ns: 2_000, symbol: "DEMO".into(), price_dollars: 0.41, size: 5.0 },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_adapter_tags_and_sorts() {
        let spec = AdapterSpec {
            adapter: "template".into(),
            venue: "TEMPLATE".into(),
            path: "ignored".into(),
            ..Default::default()
        };
        let evs = TemplateAdapter::new().load(&spec).unwrap();
        assert_eq!(evs.len(), 2);
        assert!(evs.iter().all(|e| e.instrument() == "TEMPLATE:DEMO"));
        assert!(evs[0].ts_ns() <= evs[1].ts_ns());
    }
}
