//! Kalshi trading-fee model.
//!
//! Kalshi's published general trading fee is `fee = round_up_to_cent(0.07 * C * p * (1-p))`
//! where `C` is the number of contracts and `p` is the price in dollars in [0,1]. The
//! `p*(1-p)` term peaks at p=0.5 (most expensive to trade a coin-flip) and vanishes at the
//! extremes. Maker orders are free by default.

use crate::config::BacktestConfig;

/// A fee schedule derived from a [`BacktestConfig`].
#[derive(Debug, Clone)]
pub struct FeeModel {
    use_formula: bool,
    maker_fee_per_contract: f64,
    taker_fee_rate: f64,
}

impl FeeModel {
    pub fn from_config(cfg: &BacktestConfig) -> Self {
        FeeModel {
            use_formula: cfg.fee_bps_formula,
            maker_fee_per_contract: cfg.maker_fee,
            taker_fee_rate: cfg.taker_fee_rate,
        }
    }

    /// Kalshi notional fee: `ceil(0.07 * C * p * (1-p) * 100) / 100`.
    pub fn kalshi_formula(price_dollars: f64, qty: f64) -> f64 {
        let p = price_dollars.clamp(0.0, 1.0);
        let raw = 0.07 * qty * p * (1.0 - p);
        // round up to the nearest cent, with a tiny epsilon so exact-cent values
        // (e.g. 1.75) are not bumped up by floating-point noise.
        ((raw * 100.0) - 1e-9).ceil() / 100.0
    }

    /// Taker fee for filling `qty` contracts at `price_dollars`.
    pub fn taker_fee(&self, price_dollars: f64, qty: f64) -> f64 {
        if self.use_formula {
            Self::kalshi_formula(price_dollars, qty)
        } else {
            let notional = price_dollars.clamp(0.0, 1.0) * qty;
            ((notional * self.taker_fee_rate * 100.0) - 1e-9).ceil() / 100.0
        }
    }

    /// Maker fee for resting `qty` contracts that get filled at `price_dollars`.
    pub fn maker_fee(&self, _price_dollars: f64, qty: f64) -> f64 {
        if self.use_formula {
            // Kalshi makers are free by default for the general schedule.
            0.0
        } else {
            self.maker_fee_per_contract * qty
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formula_peaks_at_half() {
        // 0.07 * 100 * 0.5 * 0.5 = 1.75 -> ceil to 1.75
        let f = FeeModel::kalshi_formula(0.50, 100.0);
        assert!((f - 1.75).abs() < 1e-9, "got {f}");
    }

    #[test]
    fn formula_rounds_up_to_cent() {
        // 0.07 * 1 * 0.4 * 0.6 = 0.0168 -> ceil to 0.02
        let f = FeeModel::kalshi_formula(0.40, 1.0);
        assert!((f - 0.02).abs() < 1e-9, "got {f}");
    }

    #[test]
    fn extremes_are_cheap() {
        let f = FeeModel::kalshi_formula(0.99, 10.0);
        // 0.07 * 10 * 0.99 * 0.01 = 0.00693 -> ceil 0.01
        assert!((f - 0.01).abs() < 1e-9, "got {f}");
    }

    #[test]
    fn maker_free_with_formula() {
        let cfg = BacktestConfig::default();
        let m = FeeModel::from_config(&cfg);
        assert_eq!(m.maker_fee(0.5, 100.0), 0.0);
        assert!(m.taker_fee(0.5, 100.0) > 0.0);
    }
}
