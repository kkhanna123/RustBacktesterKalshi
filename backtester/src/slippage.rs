//! Slippage model: extra adverse cost beyond walking the book, charged on fills.
//!
//! Walking the book already captures the *mechanical* cost of consuming multiple levels. The
//! slippage model captures the *residual* adverse cost a real taker pays on top of that: latency
//! between decision and arrival, partial book staleness, and adverse selection. It is expressed as
//!
//! * `taker_ticks` — a fixed number of cents of adverse price movement per taker fill, and/or
//! * `taker_bps`   — a fraction of the fill's notional.
//!
//! Maker fills can optionally be charged `maker_adverse_selection_bps` of notional, modeling the
//! fact that a resting quote tends to fill precisely when the market is about to run it over.
//!
//! Crucially, the slippage cost is **not folded silently into the fill price**. The model returns
//! the dollar cost so the engine can debit cash *and* accumulate it on a separate reporting line
//! ([`crate::portfolio::Portfolio::total_slippage_cost`]). When disabled, every cost is zero and
//! fills are exactly as today.

use crate::config::SlippageConfig;
use crate::types::{Cents, Liquidity, Side};

/// A slippage model derived from a [`SlippageConfig`].
#[derive(Debug, Clone)]
pub struct SlippageModel {
    enabled: bool,
    taker_ticks: i32,
    taker_bps: f64,
    maker_adverse_selection_bps: f64,
}

impl SlippageModel {
    /// Build from config. A disabled config yields a zero-cost (pass-through) model.
    pub fn from_config(cfg: &SlippageConfig) -> Self {
        if !cfg.enabled {
            return SlippageModel {
                enabled: false,
                taker_ticks: 0,
                taker_bps: 0.0,
                maker_adverse_selection_bps: 0.0,
            };
        }
        SlippageModel {
            enabled: true,
            taker_ticks: cfg.taker_ticks.max(0),
            taker_bps: cfg.taker_bps.max(0.0),
            maker_adverse_selection_bps: cfg.maker_adverse_selection_bps.max(0.0),
        }
    }

    /// True if any slippage is in effect.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.enabled
    }

    /// Dollar slippage cost for filling `qty` contracts at `fill_price` on `side` as `liquidity`.
    ///
    /// The cost is always non-negative (it always *worsens* the trade for the strategy). For a
    /// taker it is `ticks_cost + bps_cost`; for a maker it is the adverse-selection bps only.
    /// Returns 0.0 when the model is disabled.
    pub fn cost(&self, side: Side, fill_price: Cents, qty: f64, liquidity: Liquidity) -> f64 {
        let _ = side; // cost magnitude is side-independent; price already encodes direction
        if !self.enabled || qty <= 0.0 {
            return 0.0;
        }
        let notional = fill_price.to_dollars() * qty;
        match liquidity {
            Liquidity::Taker => {
                let tick_cost = (self.taker_ticks as f64 / 100.0) * qty;
                let bps_cost = self.taker_bps * notional;
                (tick_cost + bps_cost).max(0.0)
            }
            Liquidity::Maker => (self.maker_adverse_selection_bps * notional).max(0.0),
            // Settlement is not a market trade — it never incurs slippage.
            Liquidity::Settle => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, ticks: i32, bps: f64, maker_bps: f64) -> SlippageConfig {
        SlippageConfig {
            enabled,
            taker_ticks: ticks,
            taker_bps: bps,
            maker_adverse_selection_bps: maker_bps,
        }
    }

    #[test]
    fn disabled_costs_nothing() {
        let m = SlippageModel::from_config(&cfg(false, 5, 0.01, 0.01));
        assert_eq!(m.cost(Side::Bid, Cents(50), 100.0, Liquidity::Taker), 0.0);
    }

    #[test]
    fn taker_ticks_worsen_fill() {
        // 1 tick = $0.01 per contract; 100 contracts -> $1.00
        let m = SlippageModel::from_config(&cfg(true, 1, 0.0, 0.0));
        let c = m.cost(Side::Bid, Cents(50), 100.0, Liquidity::Taker);
        assert!((c - 1.00).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn taker_bps_worsen_fill() {
        // 0.001 of notional; notional = 0.50 * 100 = 50 -> 0.05
        let m = SlippageModel::from_config(&cfg(true, 0, 0.001, 0.0));
        let c = m.cost(Side::Ask, Cents(50), 100.0, Liquidity::Taker);
        assert!((c - 0.05).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn ticks_and_bps_accumulate() {
        // ticks: 2 * 0.01 * 100 = 2.00 ; bps: 0.001 * (0.50*100=50) = 0.05 ; total 2.05
        let m = SlippageModel::from_config(&cfg(true, 2, 0.001, 0.0));
        let c = m.cost(Side::Bid, Cents(50), 100.0, Liquidity::Taker);
        assert!((c - 2.05).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn maker_adverse_selection_only_on_maker() {
        let m = SlippageModel::from_config(&cfg(true, 5, 0.01, 0.002));
        // taker path ignores maker bps; maker path ignores taker ticks/bps
        let maker = m.cost(Side::Bid, Cents(50), 100.0, Liquidity::Maker);
        // 0.002 * 50 = 0.10
        assert!((maker - 0.10).abs() < 1e-9, "got {maker}");
    }

    #[test]
    fn accumulating_two_fills_sums() {
        let m = SlippageModel::from_config(&cfg(true, 1, 0.0, 0.0));
        let total: f64 = (0..3)
            .map(|_| m.cost(Side::Bid, Cents(50), 10.0, Liquidity::Taker))
            .sum();
        // 3 fills * (1 tick * 10) = 3 * 0.10 = 0.30
        assert!((total - 0.30).abs() < 1e-9, "got {total}");
    }
}
