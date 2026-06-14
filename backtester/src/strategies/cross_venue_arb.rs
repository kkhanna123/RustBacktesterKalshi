//! CROSS-VENUE arbitrage strategy — observe the SAME underlying on two venues and trade the spread.
//!
//! This is the demonstration that a single backtest can watch and trade *multiple venues at once*.
//! It pairs one instrument on venue A with one on venue B (the same underlying, e.g. a Kalshi binary
//! and a Polymarket binary on the same event, or two correlated books). When their mids diverge by
//! more than a configurable edge, it BUYS the cheaper venue and SELLS the richer venue, then
//! flattens both legs once the mids re-converge. Each leg is placed on its OWN venue-tagged book, so
//! the engine routes fills correctly per venue.
//!
//! How it finds its pair (zero hard-coding needed):
//! - If `leg_a` / `leg_b` are set, they are used verbatim (canonical `"VENUE:symbol"` ids).
//! - Otherwise it auto-pairs the FIRST instrument it sees on `venue_a` with the first on `venue_b`
//!   (discovered via [`Ctx::instruments_for_venue`]). This makes the demo "just work" on any pair.
//!
//! Sizing/positioning is intentionally simple but real: it goes long the cheap leg and short the
//! rich leg up to `size` contracts, holds while the divergence persists, and flattens both when the
//! spread collapses below `exit_edge`. Binary venues cap the mid in [0,1], so the spread is in
//! dollars-per-contract and directly comparable across venues.

use crate::strategies::{Params, StrategyParams};
use crate::strategy::{Ctx, Strategy};
use crate::types::{MarketEvent, Side};

/// Trade the mid-price spread of the same underlying across two venues.
pub struct CrossVenueArb {
    /// Venue tag for leg A (e.g. "KALSHI"). Used for auto-pairing when `leg_a` is unset.
    venue_a: String,
    /// Venue tag for leg B (e.g. "POLYMARKET").
    venue_b: String,
    /// Explicit canonical id for leg A; auto-discovered from `venue_a` when None.
    leg_a: Option<String>,
    /// Explicit canonical id for leg B; auto-discovered from `venue_b` when None.
    leg_b: Option<String>,
    /// Mid divergence (dollars) at/above which we open the spread (buy cheap, sell rich).
    entry_edge: f64,
    /// Mid divergence (dollars) at/below which we flatten both legs.
    exit_edge: f64,
    /// Contracts per leg when the spread is open.
    size: f64,
}

impl Default for CrossVenueArb {
    fn default() -> Self {
        CrossVenueArb {
            venue_a: "KALSHI".to_string(),
            venue_b: "POLYMARKET".to_string(),
            leg_a: None,
            leg_b: None,
            entry_edge: 0.03, // 3 cents of mispricing
            exit_edge: 0.01,  // converged to within 1 cent
            size: 10.0,
        }
    }
}

impl CrossVenueArb {
    /// Build from tunable params (numeric keys: `entry_edge`, `exit_edge`, `size`). The venue tags
    /// and explicit legs stay at their defaults (they are strings, set via the config `sources`).
    /// An empty map reproduces the hardcoded defaults exactly.
    pub fn from_params(params: &StrategyParams) -> Self {
        let p = Params::new(params);
        let d = CrossVenueArb::default();
        CrossVenueArb {
            entry_edge: p.get("entry_edge", d.entry_edge),
            exit_edge: p.get("exit_edge", d.exit_edge),
            size: p.get("size", d.size),
            ..d
        }
    }

    /// Resolve the two legs, auto-discovering by venue when not explicitly configured. Returns None
    /// until both venues have a book.
    fn resolve_legs(&self, ctx: &dyn Ctx) -> Option<(String, String)> {
        let a = match &self.leg_a {
            Some(a) => a.clone(),
            None => ctx.instruments_for_venue(&self.venue_a).into_iter().min()?,
        };
        let b = match &self.leg_b {
            Some(b) => b.clone(),
            None => ctx.instruments_for_venue(&self.venue_b).into_iter().min()?,
        };
        Some((a, b))
    }
}

impl Strategy for CrossVenueArb {
    fn name(&self) -> &str {
        "cross_venue_arb"
    }

    fn on_event(&mut self, _ev: &MarketEvent, ctx: &mut dyn Ctx) {
        let (a, b) = match self.resolve_legs(ctx) {
            Some(p) => p,
            None => return, // both venues not yet present
        };

        // Need a mid on BOTH venues to compute the cross-venue spread.
        let (mid_a, mid_b) = match (ctx.mid(&a), ctx.mid(&b)) {
            (Some(x), Some(y)) => (x, y),
            _ => return,
        };
        let spread = mid_a - mid_b; // + => A richer than B
        let pos_a = ctx.position(&a);
        let pos_b = ctx.position(&b);
        let flat = pos_a.abs() < 1e-9 && pos_b.abs() < 1e-9;

        if flat {
            // Open the spread when divergence exceeds the entry edge: buy the cheaper, sell the richer.
            if spread.abs() >= self.entry_edge {
                if spread > 0.0 {
                    // A rich, B cheap: SELL A, BUY B.
                    ctx.place_market(&a, Side::Ask, self.size);
                    ctx.place_market(&b, Side::Bid, self.size);
                } else {
                    // B rich, A cheap: BUY A, SELL B.
                    ctx.place_market(&a, Side::Bid, self.size);
                    ctx.place_market(&b, Side::Ask, self.size);
                }
            }
        } else if spread.abs() <= self.exit_edge {
            // Converged — flatten both legs on their own venue books.
            flatten(ctx, &a, pos_a);
            flatten(ctx, &b, pos_b);
        }
    }

    fn on_finish(&mut self, ctx: &mut dyn Ctx) {
        // Close any residual legs at end-of-data so the spread book is square.
        if let Some((a, b)) = self.resolve_legs(ctx) {
            let (pa, pb) = (ctx.position(&a), ctx.position(&b));
            flatten(ctx, &a, pa);
            flatten(ctx, &b, pb);
        }
    }
}

/// Flatten a single venue leg with a market order (long → sell, short → buy).
fn flatten(ctx: &mut dyn Ctx, inst: &str, pos: f64) {
    if pos > 1e-9 {
        ctx.place_market(inst, Side::Ask, pos);
    } else if pos < -1e-9 {
        ctx.place_market(inst, Side::Bid, -pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Cents, OrderView};

    /// A tiny mock Ctx that exposes two venue-tagged books with configurable mids and records the
    /// market orders the strategy places, so we can assert cross-venue routing without the engine.
    #[derive(Default)]
    struct MockCtx {
        mids: std::collections::HashMap<String, f64>,
        positions: std::collections::HashMap<String, f64>,
        orders: Vec<(String, Side, f64)>,
    }
    impl Ctx for MockCtx {
        fn ts_ns(&self) -> i64 {
            0
        }
        fn best_bid(&self, _i: &str) -> Option<(Cents, f64)> {
            None
        }
        fn best_ask(&self, _i: &str) -> Option<(Cents, f64)> {
            None
        }
        fn mid(&self, i: &str) -> Option<f64> {
            self.mids.get(i).copied()
        }
        fn position(&self, i: &str) -> f64 {
            self.positions.get(i).copied().unwrap_or(0.0)
        }
        fn cash(&self) -> f64 {
            1000.0
        }
        fn instruments(&self) -> Vec<String> {
            self.mids.keys().cloned().collect()
        }
        fn open_orders(&self, _i: &str) -> Vec<OrderView> {
            Vec::new()
        }
        fn place_limit_ex(
            &mut self,
            _i: &str,
            _s: Side,
            _p: Cents,
            _q: f64,
            _tif: crate::types::Tif,
            _post_only: bool,
        ) {
        }
        fn place_market(&mut self, i: &str, s: Side, q: f64) {
            self.orders.push((i.to_string(), s, q));
        }
        fn cancel(&mut self, _id: u64) {}
    }

    #[test]
    fn opens_spread_buying_cheap_selling_rich_across_venues() {
        let mut ctx = MockCtx::default();
        ctx.mids.insert("KALSHI:SYMA".into(), 0.60); // A rich
        ctx.mids.insert("POLYMARKET:SYMB".into(), 0.50); // B cheap
        let mut s = CrossVenueArb::default();
        let ev = MarketEvent::Trade(crate::types::TradeEvent {
            ts_ns: 1,
            instrument: "KALSHI:SYMA".into(),
            aggressor_yes: true,
            price: Cents(60),
            size: 1.0,
            trade_id: "t".into(),
        });
        s.on_event(&ev, &mut ctx);
        // spread = +0.10 >= entry: SELL A (Ask), BUY B (Bid) — one order on EACH venue.
        assert_eq!(ctx.orders.len(), 2);
        let a = ctx.orders.iter().find(|o| o.0 == "KALSHI:SYMA").unwrap();
        let b = ctx.orders.iter().find(|o| o.0 == "POLYMARKET:SYMB").unwrap();
        assert_eq!(a.1, Side::Ask, "rich venue A is sold");
        assert_eq!(b.1, Side::Bid, "cheap venue B is bought");
    }

    #[test]
    fn produces_fills_on_both_venues_through_the_engine() {
        // Drive the REAL engine with a merged two-venue stream and assert fills land on each venue.
        use crate::config::BacktestConfig;
        use crate::engine::Engine;
        use crate::types::{Action, BookDelta};

        fn snap(ts: i64, inst: &str, bid: i32, ask: i32, seq: i64) -> Vec<MarketEvent> {
            vec![
                MarketEvent::Delta(BookDelta {
                    ts_ns: ts,
                    instrument: inst.into(),
                    action: Action::Add,
                    side: Side::Bid,
                    price: Cents(bid),
                    size: 500.0,
                    sequence: seq,
                    is_snapshot: true,
                }),
                MarketEvent::Delta(BookDelta {
                    ts_ns: ts + 1,
                    instrument: inst.into(),
                    action: Action::Add,
                    side: Side::Ask,
                    price: Cents(ask),
                    size: 500.0,
                    sequence: seq + 1,
                    is_snapshot: false,
                }),
            ]
        }
        let mut evs: Vec<MarketEvent> = Vec::new();
        // aligned
        evs.extend(snap(1_000, "KALSHI:SYMA", 54, 56, 1));
        evs.extend(snap(1_100, "POLYMARKET:SYMB", 54, 56, 1));
        // diverge: A rich (mid .60), B cheap (mid .55) -> open spread
        evs.extend(snap(5_000, "KALSHI:SYMA", 59, 61, 3));
        evs.extend(snap(5_100, "POLYMARKET:SYMB", 54, 56, 3));
        // converge -> flatten
        evs.extend(snap(10_000, "KALSHI:SYMA", 55, 56, 5));
        evs.extend(snap(10_100, "POLYMARKET:SYMB", 55, 56, 5));
        evs.sort_by_key(|e| e.ts_ns());

        let cfg = BacktestConfig::default();
        let eng = Engine::new(&cfg);
        let mut s = CrossVenueArb::default();
        let out = eng.run_collecting(evs.into_iter(), &mut s, &cfg);
        let venues: std::collections::BTreeSet<&str> = out
            .portfolio
            .fills
            .iter()
            .map(|f| f.instrument.split_once(':').unwrap().0)
            .collect();
        assert!(out.portfolio.fills.len() >= 2, "expected cross-venue fills");
        assert!(venues.contains("KALSHI") && venues.contains("POLYMARKET"),
            "fills must span BOTH venues, got {venues:?}");
    }

    #[test]
    fn flattens_both_legs_on_convergence() {
        let mut ctx = MockCtx::default();
        ctx.mids.insert("KALSHI:SYMA".into(), 0.55);
        ctx.mids.insert("POLYMARKET:SYMB".into(), 0.55); // converged
        ctx.positions.insert("KALSHI:SYMA".into(), -10.0); // short A
        ctx.positions.insert("POLYMARKET:SYMB".into(), 10.0); // long B
        let mut s = CrossVenueArb::default();
        let ev = MarketEvent::Trade(crate::types::TradeEvent {
            ts_ns: 2,
            instrument: "POLYMARKET:SYMB".into(),
            aggressor_yes: true,
            price: Cents(55),
            size: 1.0,
            trade_id: "t".into(),
        });
        s.on_event(&ev, &mut ctx);
        // both legs flattened: buy back the short A, sell the long B.
        assert_eq!(ctx.orders.len(), 2);
        let a = ctx.orders.iter().find(|o| o.0 == "KALSHI:SYMA").unwrap();
        let b = ctx.orders.iter().find(|o| o.0 == "POLYMARKET:SYMB").unwrap();
        assert_eq!(a.1, Side::Bid); // cover short
        assert_eq!(b.1, Side::Ask); // sell long
    }
}
