//! Polymarket CLOB adapter (binary outcome tokens).
//!
//! STATUS: **stub + working generic path.** Polymarket's CLOB exposes an order book per ERC-1155
//! outcome token (a `tokenId` under a `conditionId`/market). Prices are in [0,1] (probability of the
//! outcome), which maps cleanly onto our YES-native cents book: a token bid at 0.42 → `Cents(42)`
//! on the [`Side::Bid`] side.
//!
//! Today this adapter is fully usable by ingesting Polymarket data that has been exported to the
//! generic NDJSON capture schema (one delta/trade object per line) — it simply delegates to
//! [`crate::adapters::generic_ndjson`] and stamps the `POLYMARKET:` venue. The symbol should be the
//! Polymarket `tokenId` (or a human alias).
//!
//! TODO: real wire format. To make this a first-class adapter, implement either:
//!   * a loader for Polymarket's CLOB REST `/book` + `/trades` snapshots, or
//!   * a tick NDJSON capture of the CLOB websocket `market` channel (`book`, `price_change`, `last_trade_price`
//!     messages). Map: `book` → snapshot deltas; `price_change` → ADD/UPDATE/DELETE deltas;
//!     `last_trade_price` → trades. Outcome `BUY`/`SELL` → [`Side::Bid`]/[`Side::Ask`]; `size` is in
//!     token units. See the `// TODO: real wire format` block below.

use crate::adapters::generic_ndjson::GenericNdjsonAdapter;
use crate::adapters::{AdapterSpec, DataAdapter, Venue};
use crate::types::MarketEvent;
use anyhow::Result;

/// Polymarket adapter. Currently ingests Polymarket data via the generic NDJSON schema; the real
/// CLOB REST/WS wire format is a TODO (see module docs).
pub struct PolymarketAdapter {
    inner: GenericNdjsonAdapter,
}

impl PolymarketAdapter {
    pub fn new() -> Self {
        PolymarketAdapter {
            inner: GenericNdjsonAdapter::new(),
        }
    }
}

impl Default for PolymarketAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl DataAdapter for PolymarketAdapter {
    fn name(&self) -> &str {
        "polymarket"
    }

    fn default_venue(&self) -> Venue {
        Venue::Polymarket
    }

    fn description(&self) -> &str {
        "Polymarket CLOB (binary outcome tokens, prob prices); reads canonical NDJSON, venue POLYMARKET."
    }

    fn load(&self, spec: &AdapterSpec) -> Result<Vec<MarketEvent>> {
        // Force the Polymarket venue (unless the spec explicitly overrode it) and reuse the generic
        // NDJSON normalizer. When the real CLOB parser lands, branch on the path/extension here.
        //
        // TODO: real wire format —
        //   if spec.path looks like a CLOB endpoint/url, fetch `/book` + `/trades` (or open the WS
        //   `market` channel) and translate Polymarket's outcome-token order book into BookDelta /
        //   TradeEvent instead of delegating to generic_ndjson.
        let mut s = spec.clone();
        if s.venue.trim().is_empty() {
            s.venue = self.default_venue().tag();
        }
        self.inner.load(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn polymarket_stub_loads_generic_ndjson_tagged() {
        let mut p = std::env::temp_dir();
        p.push(format!("poly_stub_{}.ndjson", std::process::id()));
        std::fs::File::create(&p)
            .unwrap()
            .write_all(
                b"{\"kind\":\"delta\",\"ts_ns\":1000,\"instrument\":\"0xtoken\",\"action\":\"ADD\",\"side\":\"BUY\",\"price\":0.42,\"size\":50,\"sequence\":1,\"is_snapshot\":1}\n",
            )
            .unwrap();
        let spec = AdapterSpec {
            adapter: "polymarket".into(),
            path: p.display().to_string(),
            ..Default::default()
        };
        let evs = PolymarketAdapter::new().load(&spec).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].instrument(), "POLYMARKET:0xtoken");
        std::fs::remove_file(&p).ok();
    }
}
