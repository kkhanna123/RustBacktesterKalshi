//! Cheap "what's in this data source" summaries that power the `describe-data` and
//! `list-instruments` CLI commands. We compute everything from an already-loaded
//! `Vec<MarketEvent>` so the ndjson loader is reused verbatim (one code path, no schema drift).
//! ClickHouse has its own GROUP-BY path (feature-gated) to avoid pulling whole days.

use crate::types::MarketEvent;
use std::collections::BTreeMap;

/// Per-instrument counts and time span derived from an event stream.
#[derive(Debug, Clone, Default)]
pub struct InstrumentSummary {
    pub instrument: String,
    /// Total events (deltas + trades) for this instrument.
    pub events: u64,
    /// Number of snapshot deltas (`is_snapshot == true`).
    pub snapshots: u64,
    /// Number of incremental deltas (`is_snapshot == false`).
    pub deltas: u64,
    /// Number of trade events.
    pub trades: u64,
    /// Earliest event timestamp (ns), if any.
    pub first_ns: i64,
    /// Latest event timestamp (ns), if any.
    pub last_ns: i64,
}

/// A whole-source summary: totals plus a per-instrument breakdown (sorted by instrument id).
#[derive(Debug, Clone, Default)]
pub struct DataSummary {
    pub total_events: u64,
    pub total_snapshots: u64,
    pub total_deltas: u64,
    pub total_trades: u64,
    pub first_ns: i64,
    pub last_ns: i64,
    pub instruments: Vec<InstrumentSummary>,
}

/// Build a [`DataSummary`] from a slice of (time-sorted) events.
pub fn summarize(events: &[MarketEvent]) -> DataSummary {
    let mut by_inst: BTreeMap<String, InstrumentSummary> = BTreeMap::new();
    let mut total = DataSummary {
        first_ns: i64::MAX,
        last_ns: i64::MIN,
        ..Default::default()
    };

    for ev in events {
        let inst = ev.instrument().to_string();
        let ts = ev.ts_ns();
        let s = by_inst.entry(inst.clone()).or_insert_with(|| InstrumentSummary {
            instrument: inst,
            first_ns: i64::MAX,
            last_ns: i64::MIN,
            ..Default::default()
        });
        s.events += 1;
        total.total_events += 1;
        match ev {
            MarketEvent::Delta(d) => {
                if d.is_snapshot {
                    s.snapshots += 1;
                    total.total_snapshots += 1;
                } else {
                    s.deltas += 1;
                    total.total_deltas += 1;
                }
            }
            MarketEvent::Trade(_) => {
                s.trades += 1;
                total.total_trades += 1;
            }
        }
        s.first_ns = s.first_ns.min(ts);
        s.last_ns = s.last_ns.max(ts);
        total.first_ns = total.first_ns.min(ts);
        total.last_ns = total.last_ns.max(ts);
    }

    if total.total_events == 0 {
        total.first_ns = 0;
        total.last_ns = 0;
    }
    total.instruments = by_inst.into_values().collect();
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Action, BookDelta, Cents, Side, TradeEvent};

    fn snap(inst: &str, ts: i64) -> MarketEvent {
        MarketEvent::Delta(BookDelta {
            ts_ns: ts,
            instrument: inst.into(),
            action: Action::Add,
            side: Side::Bid,
            price: Cents(40),
            size: 10.0,
            sequence: 1,
            is_snapshot: true,
        })
    }
    fn trade(inst: &str, ts: i64) -> MarketEvent {
        MarketEvent::Trade(TradeEvent {
            ts_ns: ts,
            instrument: inst.into(),
            aggressor_yes: true,
            price: Cents(50),
            size: 1.0,
            trade_id: "t".into(),
        })
    }

    #[test]
    fn summarize_counts_and_spans() {
        let evs = vec![
            snap("A", 100),
            trade("A", 150),
            snap("B", 200),
            snap("A", 300),
        ];
        let s = summarize(&evs);
        assert_eq!(s.total_events, 4);
        assert_eq!(s.total_snapshots, 3);
        assert_eq!(s.total_trades, 1);
        assert_eq!(s.first_ns, 100);
        assert_eq!(s.last_ns, 300);
        assert_eq!(s.instruments.len(), 2);
        // sorted by instrument id: A then B
        assert_eq!(s.instruments[0].instrument, "A");
        assert_eq!(s.instruments[0].events, 3);
        assert_eq!(s.instruments[0].trades, 1);
        assert_eq!(s.instruments[0].first_ns, 100);
        assert_eq!(s.instruments[0].last_ns, 300);
        assert_eq!(s.instruments[1].instrument, "B");
        assert_eq!(s.instruments[1].events, 1);
    }

    #[test]
    fn summarize_empty_is_zeroed() {
        let s = summarize(&[]);
        assert_eq!(s.total_events, 0);
        assert_eq!(s.first_ns, 0);
        assert_eq!(s.last_ns, 0);
        assert!(s.instruments.is_empty());
    }
}
