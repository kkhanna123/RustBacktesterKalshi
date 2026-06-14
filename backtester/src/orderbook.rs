//! L2 orderbook reconstruction from `BookDelta` rows.
//!
//! Each instrument has its own [`OrderBook`] with two `BTreeMap<Cents, f64>` sides: bids
//! (iterated high→low for best) and asks (iterated low→high). The book is YES-native: a `Bid`
//! is someone buying YES, an `Ask` is someone selling YES.
//!
//! ## Book resets come from `is_snapshot`, NOT from sequence gaps
//! Book RESETS are driven authoritatively by [`BookDelta::is_snapshot`] (see [`OrderBook::apply`]):
//! the first row of a full-book snapshot clears both sides before being applied. The `sequence`
//! field is INFORMATIONAL only. In this system the sequence may be VENUE-GLOBAL — Kalshi's
//! websocket `seq` increments across ALL markets on one subscription — so a per-instrument gap is
//! EXPECTED and benign, not a sign of lost data. We therefore do NOT use [`OrderBook::seq_gap`] to
//! auto-clear or re-snapshot a book; doing so would wrongly nuke a correct book on every
//! cross-market increment. See [`OrderBook::seq_gap`] for the kept-but-unwired helper.

use crate::types::{Action, BookDelta, Cents, Side};
use std::collections::{BTreeMap, HashMap};

/// A single-instrument two-sided book.
#[derive(Debug, Clone, Default)]
pub struct OrderBook {
    /// Resting YES-buy interest, price -> size.
    pub bids: BTreeMap<Cents, f64>,
    /// Resting YES-sell interest, price -> size.
    pub asks: BTreeMap<Cents, f64>,
    /// Last applied Kalshi sequence number (0 if none).
    pub last_seq: i64,
}

impl OrderBook {
    pub fn new() -> Self {
        OrderBook::default()
    }

    /// Apply one delta. If `is_snapshot`, both sides are cleared *before* this row is applied
    /// (the snapshot's first row carries the flag; subsequent snapshot rows have it unset).
    pub fn apply(&mut self, d: &BookDelta) {
        if d.is_snapshot {
            self.bids.clear();
            self.asks.clear();
        }
        let side_map = match d.side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        match d.action {
            Action::Delete => {
                side_map.remove(&d.price);
            }
            Action::Add | Action::Update => {
                if d.size <= 0.0 {
                    side_map.remove(&d.price);
                } else {
                    side_map.insert(d.price, d.size);
                }
            }
        }
        self.last_seq = d.sequence;
    }

    /// True if `incoming` sequence is not exactly `last_seq + 1` (a skip or backwards duplicate).
    /// The very first event (last_seq==0) is never a gap.
    ///
    /// DESIGN NOTE — intentionally NOT wired into reset logic. Book resets in this engine come
    /// authoritatively from [`BookDelta::is_snapshot`] (handled in [`OrderBook::apply`]), never from
    /// sequence gaps. The `sequence` is informational and may be VENUE-GLOBAL (Kalshi's WS `seq`
    /// increments across every market on one subscription), so per-instrument "gaps" are EXPECTED
    /// and benign — auto-clearing the book on them would corrupt a perfectly good book. This helper
    /// is kept (and tested) as a diagnostic for callers that want to *observe* sequencing, but it
    /// MUST NOT drive a resnapshot here. See the module-level doc.
    pub fn seq_gap(&self, incoming: i64) -> bool {
        if self.last_seq == 0 {
            return false;
        }
        incoming != self.last_seq + 1
    }

    /// Best bid (highest price someone will buy YES at).
    pub fn best_bid(&self) -> Option<(Cents, f64)> {
        self.bids.iter().next_back().map(|(&p, &s)| (p, s))
    }

    /// Best ask (lowest price someone will sell YES at).
    pub fn best_ask(&self) -> Option<(Cents, f64)> {
        self.asks.iter().next().map(|(&p, &s)| (p, s))
    }

    /// Mid price in dollars (average of best bid/ask), if both sides exist.
    pub fn mid(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some((b, _)), Some((a, _))) => Some((b.to_dollars() + a.to_dollars()) / 2.0),
            _ => None,
        }
    }

    /// Spread in dollars (best_ask - best_bid), if both sides exist.
    pub fn spread(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some((b, _)), Some((a, _))) => Some(a.to_dollars() - b.to_dollars()),
            _ => None,
        }
    }

    /// Order-flow imbalance at top of book in [-1, 1]: (bidsz - asksz)/(bidsz + asksz).
    pub fn imbalance(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some((_, bs)), Some((_, as_))) if bs + as_ > 0.0 => Some((bs - as_) / (bs + as_)),
            _ => None,
        }
    }

    /// Size-weighted microprice in dollars: a fairer mid that leans toward the heavier side.
    pub fn microprice(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some((b, bs)), Some((a, as_))) if bs + as_ > 0.0 => {
                Some((b.to_dollars() * as_ + a.to_dollars() * bs) / (bs + as_))
            }
            _ => None,
        }
    }
}

/// A collection of books keyed by instrument id.
#[derive(Debug, Clone, Default)]
pub struct BookSet {
    pub books: HashMap<String, OrderBook>,
}

impl BookSet {
    pub fn new() -> Self {
        BookSet::default()
    }

    /// Apply a delta to the relevant instrument's book (creating it if needed).
    pub fn apply(&mut self, d: &BookDelta) {
        self.books
            .entry(d.instrument.clone())
            .or_default()
            .apply(d);
    }

    pub fn get(&self, instrument: &str) -> Option<&OrderBook> {
        self.books.get(instrument)
    }

    pub fn best_bid(&self, instrument: &str) -> Option<(Cents, f64)> {
        self.books.get(instrument).and_then(|b| b.best_bid())
    }

    pub fn best_ask(&self, instrument: &str) -> Option<(Cents, f64)> {
        self.books.get(instrument).and_then(|b| b.best_ask())
    }

    pub fn mid(&self, instrument: &str) -> Option<f64> {
        self.books.get(instrument).and_then(|b| b.mid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(action: Action, side: Side, price: i32, size: f64, seq: i64, snap: bool) -> BookDelta {
        BookDelta {
            ts_ns: seq * 1_000,
            instrument: "X".to_string(),
            action,
            side,
            price: Cents(price),
            size,
            sequence: seq,
            is_snapshot: snap,
        }
    }

    #[test]
    fn snapshot_resets() {
        let mut b = OrderBook::new();
        b.apply(&delta(Action::Add, Side::Bid, 40, 100.0, 1, false));
        b.apply(&delta(Action::Add, Side::Ask, 60, 100.0, 2, false));
        assert_eq!(b.best_bid(), Some((Cents(40), 100.0)));
        // a new snapshot resets prior levels
        b.apply(&delta(Action::Add, Side::Bid, 30, 50.0, 3, true));
        assert_eq!(b.best_bid(), Some((Cents(30), 50.0)));
        assert_eq!(b.best_ask(), None);
    }

    #[test]
    fn add_update_delete() {
        let mut b = OrderBook::new();
        b.apply(&delta(Action::Add, Side::Bid, 40, 100.0, 1, false));
        b.apply(&delta(Action::Update, Side::Bid, 40, 250.0, 2, false));
        assert_eq!(b.best_bid(), Some((Cents(40), 250.0)));
        // update to 0 removes
        b.apply(&delta(Action::Update, Side::Bid, 40, 0.0, 3, false));
        assert_eq!(b.best_bid(), None);
        // re-add then delete
        b.apply(&delta(Action::Add, Side::Bid, 41, 10.0, 4, false));
        b.apply(&delta(Action::Delete, Side::Bid, 41, 0.0, 5, false));
        assert_eq!(b.best_bid(), None);
    }

    #[test]
    fn gap_detection() {
        let mut b = OrderBook::new();
        assert!(!b.seq_gap(1)); // first event
        b.apply(&delta(Action::Add, Side::Bid, 40, 100.0, 1, false));
        assert!(!b.seq_gap(2));
        assert!(b.seq_gap(5));
    }

    #[test]
    fn best_mid_micro() {
        let mut b = OrderBook::new();
        b.apply(&delta(Action::Add, Side::Bid, 40, 300.0, 1, false));
        b.apply(&delta(Action::Add, Side::Ask, 60, 100.0, 2, false));
        assert!((b.mid().unwrap() - 0.50).abs() < 1e-9);
        assert!((b.spread().unwrap() - 0.20).abs() < 1e-9);
        // imbalance leans to bids
        assert!(b.imbalance().unwrap() > 0.0);
        // microprice leans toward the ask price (heavier bid pushes price up)
        let mp = b.microprice().unwrap();
        assert!(mp > 0.50 && mp < 0.60, "got {mp}");
    }

    #[test]
    fn bookset_keys_by_instrument() {
        let mut bs = BookSet::new();
        bs.apply(&delta(Action::Add, Side::Bid, 40, 100.0, 1, false));
        assert_eq!(bs.best_bid("X"), Some((Cents(40), 100.0)));
        assert_eq!(bs.best_bid("Y"), None);
    }
}
