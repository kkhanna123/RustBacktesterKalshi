//! Built-in **Kalshi** adapter — a thin wrapper over the tick NDJSON loader so nothing regresses.
//!
//! This wraps [`crate::data::ndjson`] in the [`DataAdapter`] trait and stamps the `KALSHI:` venue
//! prefix onto every emitted instrument. It is the reference example of "wrap an existing loader as
//! an adapter": no parsing logic lives here, only normalization + venue tagging.

use crate::adapters::{date_to_ns_opt, tag_events_with_venue, AdapterSpec, DataAdapter, Venue};
use crate::data::ndjson;
use crate::types::MarketEvent;
use anyhow::Result;
use std::path::Path;

/// Kalshi NDJSON(.gz) collector captures, tagged `KALSHI:`.
pub struct KalshiNdjsonAdapter;

impl KalshiNdjsonAdapter {
    pub fn new() -> Self {
        KalshiNdjsonAdapter
    }
}

impl Default for KalshiNdjsonAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl DataAdapter for KalshiNdjsonAdapter {
    fn name(&self) -> &str {
        "kalshi_ndjson"
    }
    fn default_venue(&self) -> Venue {
        Venue::Kalshi
    }
    fn description(&self) -> &str {
        "Kalshi tick NDJSON(.gz) capture (the native collector format), venue-tagged KALSHI."
    }
    fn load(&self, spec: &AdapterSpec) -> Result<Vec<MarketEvent>> {
        let venue = spec.resolved_venue(self.default_venue());
        let (start_ns, end_ns) = (date_to_ns_opt(&spec.start)?, date_to_ns_opt(&spec.end)?);
        let mut events = ndjson::load(
            Path::new(&spec.path),
            spec.instrument.as_deref(),
            start_ns,
            end_ns,
        )?;
        tag_events_with_venue(&mut events, &venue);
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(tag: &str, content: &str, ext: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("kalshi_adapter_{}_{tag}_{n}.{ext}", std::process::id()));
        std::fs::File::create(&p).unwrap().write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn ndjson_adapter_tags_venue() {
        let p = tmp(
            "nd",
            "{\"kind\":\"delta\",\"ts_ns\":1000,\"instrument\":\"KXX\",\"action\":\"ADD\",\"side\":\"BUY\",\"price\":0.40,\"size\":10,\"sequence\":1,\"is_snapshot\":1}\n",
            "ndjson",
        );
        let spec = AdapterSpec {
            adapter: "kalshi_ndjson".into(),
            path: p.display().to_string(),
            ..Default::default()
        };
        let evs = KalshiNdjsonAdapter::new().load(&spec).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].instrument(), "KALSHI:KXX");
        std::fs::remove_file(&p).ok();
    }
}
