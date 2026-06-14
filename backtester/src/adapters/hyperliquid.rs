//! Hyperliquid perpetuals adapter (L2 book).
//!
//! STATUS: **stub + working generic path.** Hyperliquid streams an L2 order book per perp coin
//! (e.g. `BTC`, `ETH`) as price/size levels. Unlike Kalshi/Polymarket, perp prices are NOT in [0,1]:
//! they're dollar prices that can be far above 1 (e.g. 65000.0). Our internal book is integer-cents
//! `Cents(1..99)` and YES-native, so a faithful perps integration needs a price encoding decision
//! (the `// TODO` below). For now the generic path treats incoming `price` as already in the cents
//! domain when `mapping["price_is_cents"]="true"`, or rescales dollar prices, so correlated-book and
//! spread strategies work on relative moves.
//!
//! Today this adapter is usable by ingesting Hyperliquid data exported to the generic NDJSON schema;
//! it delegates to [`crate::adapters::generic_ndjson`] and stamps the `HYPERLIQUID:` venue. The
//! symbol should be the coin / `BTC-PERP`-style id.
//!
//! TODO: real wire format. Implement a tick NDJSON loader over the Hyperliquid `info` websocket
//! `l2Book` subscription: each message carries `levels: [bids, asks]` of `{px, sz, n}`. Map a full
//! `l2Book` message to a snapshot (first delta `is_snapshot=1`, then one ADD per level per side),
//! and `trades` messages to [`crate::types::TradeEvent`]. Decide a stable px→Cents encoding (e.g.
//! a per-instrument tick origin) before wiring real perps prices. See the `// TODO` block below.

use crate::adapters::generic_ndjson::GenericNdjsonAdapter;
use crate::adapters::{AdapterSpec, DataAdapter, Venue};
use crate::types::MarketEvent;
use anyhow::Result;

/// Hyperliquid adapter. Currently ingests Hyperliquid data via the generic NDJSON schema; the real
/// `l2Book` WS wire format + perps price encoding are a TODO (see module docs).
pub struct HyperliquidAdapter {
    inner: GenericNdjsonAdapter,
}

impl HyperliquidAdapter {
    pub fn new() -> Self {
        HyperliquidAdapter {
            inner: GenericNdjsonAdapter::new(),
        }
    }
}

impl Default for HyperliquidAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl DataAdapter for HyperliquidAdapter {
    fn name(&self) -> &str {
        "hyperliquid"
    }

    fn default_venue(&self) -> Venue {
        Venue::Hyperliquid
    }

    fn description(&self) -> &str {
        "Hyperliquid perpetuals L2 book (dollar prices); reads canonical NDJSON, venue HYPERLIQUID."
    }

    fn load(&self, spec: &AdapterSpec) -> Result<Vec<MarketEvent>> {
        // TODO: real wire format —
        //   if spec.path is a Hyperliquid `info` endpoint, subscribe to `l2Book`/`trades` and
        //   translate `levels: [bids, asks]` of `{px, sz, n}` into snapshot + incremental BookDeltas,
        //   choosing a px→Cents encoding for non-[0,1] perp prices. For now, delegate to the generic
        //   NDJSON normalizer so exported captures work immediately.
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
    fn hyperliquid_stub_loads_generic_ndjson_tagged() {
        let mut p = std::env::temp_dir();
        p.push(format!("hl_stub_{}.ndjson", std::process::id()));
        std::fs::File::create(&p)
            .unwrap()
            .write_all(
                b"{\"kind\":\"delta\",\"ts_ns\":1000,\"instrument\":\"BTC-PERP\",\"action\":\"ADD\",\"side\":\"SELL\",\"price\":55,\"size\":2,\"sequence\":1,\"is_snapshot\":1,\"price_is_cents\":1}\n",
            )
            .unwrap();
        let mut mapping = std::collections::BTreeMap::new();
        mapping.insert("price_is_cents".to_string(), "true".to_string());
        let spec = AdapterSpec {
            adapter: "hyperliquid".into(),
            path: p.display().to_string(),
            mapping,
            ..Default::default()
        };
        let evs = HyperliquidAdapter::new().load(&spec).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].instrument(), "HYPERLIQUID:BTC-PERP");
        std::fs::remove_file(&p).ok();
    }
}
