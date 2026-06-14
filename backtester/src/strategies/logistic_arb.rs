//! LOGISTIC STRIKE-CURVE ARBITRAGE — trade individual strikes toward a fitted ladder.
//!
//! # The idea
//!
//! For one settlement event, the engine fits a **logistic survival curve** across the
//! whole ladder of "above $K" binaries (see [`crate::fit_logistic`]). That fit gives,
//! for every strike, a *curve-implied fair price* `S(K)`, plus two ladder-level
//! features: the **implied fair value** `mu` (the median settlement) and an **implied
//! volatility** (`s·π/√3`, a `$`-dispersion of the implied settlement distribution).
//!
//! Individual strikes wiggle around that smooth curve. This strategy treats the
//! curve as the consensus and each strike's deviation from it as a tradeable
//! mispricing:
//!
//! * A strike trading **above** its fitted price is **RICH** → SELL it.
//! * A strike trading **below** its fitted price is **CHEAP** → BUY it.
//! * As the strike reverts to the curve (|edge| small), FLATTEN.
//!
//! # Why the implied-vol / fair-value gates matter
//!
//! The edge signal is only meaningful if the fitted curve is **trustworthy**. We use
//! the fit's diagnostics as guard rails before acting on any single-strike edge:
//!
//! * **`fit_quality` (RMSE) ≤ `max_rmse`** — if the logistic barely explains the
//!   ladder (high RMSE), the "fair price" is noise and so is every edge derived from
//!   it. We refuse to trade a curve that doesn't fit.
//! * **`implied_vol` within `[min_vol, max_vol]`** — the implied vol is the width of
//!   the fitted distribution. Too *small* a vol means a near-vertical step where tiny
//!   strike errors produce huge spurious edges (and `1/s` is ill-conditioned); too
//!   *large* a vol means an almost-flat curve with no real information. Only an
//!   in-band vol describes a sane, informative ladder we're willing to lean on.
//! * **`implied_fair_value` (`mu`)** must merely exist (a fittable ladder) — it is the
//!   anchor that the whole curve, and therefore every per-strike fair price, is built
//!   around. We require it as a final "the event is actually fittable" check and
//!   expose it for logging/feature use.
//!
//! Only once those say the curve is reliable do we act on the per-strike `fit_edge`.

use crate::strategies::toolkit::BaseStrategy;
use crate::strategies::{Params, StrategyParams};
use crate::strategy::{Ctx, Strategy};
use crate::types::{MarketEvent, Side};

/// Trade single strikes toward a fitted logistic ladder, gated by fit quality + implied vol.
pub struct LogisticArb {
    /// Shared knobs/helpers: max absolute position, base order size, instrument filter.
    base: BaseStrategy,
    /// Enter when |market_mid − fitted_price| ≥ this (dollars of edge vs the curve).
    entry_edge: f64,
    /// Flatten the strike once |edge| ≤ this (it has reverted to the curve).
    exit_edge: f64,
    /// Refuse to trade if the fit's RMSE exceeds this (the curve doesn't explain the ladder).
    max_rmse: f64,
    /// Lower bound on a trustworthy implied vol (below this the curve is a near-vertical step).
    min_vol: f64,
    /// Upper bound on a trustworthy implied vol (above this the curve is almost flat / uninformative).
    max_vol: f64,
}

impl Default for LogisticArb {
    fn default() -> Self {
        LogisticArb {
            // qty=10, max_inventory=40 (per the strategy spec).
            base: BaseStrategy::new(40.0, 10.0),
            entry_edge: 0.03,
            exit_edge: 0.01,
            max_rmse: 0.05,
            min_vol: 0.02,
            max_vol: 0.6,
        }
    }
}

impl LogisticArb {
    /// Build from tunable params (keys: `entry_edge`, `exit_edge`, `qty`, `max_inventory`,
    /// `max_rmse`, `min_vol`, `max_vol`). An empty map reproduces the hardcoded defaults exactly.
    pub fn from_params(params: &StrategyParams) -> Self {
        let p = Params::new(params);
        let d = LogisticArb::default();
        LogisticArb {
            base: BaseStrategy::new(
                p.get("max_inventory", d.base.max_position),
                p.get("qty", d.base.order_qty),
            ),
            entry_edge: p.get("entry_edge", d.entry_edge),
            exit_edge: p.get("exit_edge", d.exit_edge),
            max_rmse: p.get("max_rmse", d.max_rmse),
            min_vol: p.get("min_vol", d.min_vol),
            max_vol: p.get("max_vol", d.max_vol),
        }
    }
}

impl Strategy for LogisticArb {
    fn name(&self) -> &str {
        "logistic_arb"
    }

    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
        let inst = ev.instrument().to_string();
        // (1) respect the optional instrument filter.
        if !self.base.accepts(&inst) {
            return;
        }

        // (2) we need the curve's edge AT THIS STRIKE to decide anything. `fit_edge` is
        //     `market_mid − fitted_price`; it is `None` for non-ladder instruments, when there's no
        //     mid, or when the event can't be fitted — in all of which we simply don't trade.
        let edge = match ctx.fit_edge(&inst) {
            Some(e) => e,
            None => return,
        };

        // (3) GATE on fit trustworthiness BEFORE acting on the edge (see module docs).
        //     a. The curve must actually fit the ladder (low RMSE).
        match ctx.fit_quality(&inst) {
            Some(rmse) if rmse <= self.max_rmse => {}
            _ => return, // missing or too-poor fit ⇒ the edge is noise
        }
        //     b. The implied vol must be in a sane band — neither a near-vertical step (tiny vol,
        //        spurious edges) nor an almost-flat, uninformative curve (huge vol).
        match ctx.implied_vol(&inst) {
            Some(v) if v >= self.min_vol && v <= self.max_vol => {}
            _ => return,
        }
        //     c. The implied fair value (mu) must exist — final confirmation the ladder is fittable.
        if ctx.implied_fair_value(&inst).is_none() {
            return;
        }

        let pos = ctx.position(&inst);

        // (4) act on the (now trustworthy) per-strike edge.
        if edge.abs() <= self.exit_edge {
            // Reverted to the curve: flatten any position in this strike.
            self.base.desired_flatten(&inst, ctx);
            return;
        }

        if edge >= self.entry_edge {
            // RICH vs the curve (market mid above fitted) ⇒ SELL the overpriced binary.
            // Clamp so we never breach max_inventory on the short side.
            let add = self.base.clamp_to_max_position(pos, -self.base.order_qty);
            if add < 0.0 {
                ctx.place_market(&inst, Side::Ask, -add);
            }
        } else if edge <= -self.entry_edge {
            // CHEAP vs the curve (market mid below fitted) ⇒ BUY the underpriced binary.
            let add = self.base.clamp_to_max_position(pos, self.base.order_qty);
            if add > 0.0 {
                ctx.place_market(&inst, Side::Bid, add);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BacktestConfig;
    use crate::engine::Engine;
    use crate::types::{Action, BookDelta, Cents, MarketEvent, Side};

    /// Build a delta event for `inst` at `(side, price_cents)`.
    fn delta(ts: i64, inst: &str, side: Side, px: i32, sz: f64, seq: i64, snap: bool) -> MarketEvent {
        MarketEvent::Delta(BookDelta {
            ts_ns: ts,
            instrument: inst.to_string(),
            action: Action::Add,
            side,
            price: Cents(px),
            size: sz,
            sequence: seq,
            is_snapshot: snap,
        })
    }

    /// Construct a ladder whose mids sit ON a logistic curve, EXCEPT one strike that is deliberately
    /// rich (its mid pushed up). The strategy should then SELL that rich strike.
    #[test]
    fn sells_a_rich_strike_on_a_clean_ladder() {
        let (mu, s) = (3.0, 0.075);
        let event = "KXNATGASD-26JUN1517";
        let strikes: Vec<f64> = (0..28).map(|i| 2.7 + 0.6 * i as f64 / 27.0).collect();

        // Pick a strike near the middle and make it RICH: set its mid well above the curve.
        let rich_idx = 14usize;
        let rich_strike = strikes[rich_idx];

        let mut events = Vec::new();
        let mut seq = 1i64;
        // Seed every strike's two-sided book so each has a mid ON the curve (a tight 1-cent market).
        for (i, &k) in strikes.iter().enumerate() {
            let fair = 1.0 / (1.0 + ((k - mu) / s).exp());
            let mid_c = if i == rich_idx {
                // push the rich strike's mid ~8 cents above fair
                ((fair + 0.08).clamp(0.02, 0.97) * 100.0).round() as i32
            } else {
                (fair.clamp(0.02, 0.98) * 100.0).round() as i32
            };
            let inst = format!("{event}-T{k:.3}");
            // bid at mid-? and ask at mid+? so the book mid ≈ mid_c. Use a tight 1c market when
            // possible.
            let bid = (mid_c - 1).clamp(1, 98);
            let ask = (mid_c + 1).clamp(2, 99);
            events.push(delta(1_000 + seq, &inst, Side::Bid, bid, 100.0, seq, true));
            seq += 1;
            events.push(delta(1_000 + seq, &inst, Side::Ask, ask, 100.0, seq, false));
            seq += 1;
        }
        // A trailing delta on the rich strike to trigger a strategy hook after the whole ladder is
        // populated (so the fit sees every strike when the hook for the rich strike runs).
        let rich_inst = format!("{event}-T{rich_strike:.3}");
        let bid = ((1.0 / (1.0 + ((rich_strike - mu) / s).exp()) + 0.08) * 100.0).round() as i32 - 1;
        events.push(delta(1_000 + seq, &rich_inst, Side::Bid, bid.clamp(1, 98), 100.0, seq, false));

        let cfg = BacktestConfig::default();
        let eng = Engine::new(&cfg);
        let mut strat = LogisticArb::default();
        let out = eng.run_collecting(events.into_iter(), &mut strat, &cfg);

        // The strategy must have traded the rich strike on the SELL side.
        let sold_rich = out
            .portfolio
            .fills
            .iter()
            .any(|f| f.instrument == rich_inst && matches!(f.side, Side::Ask));
        assert!(
            sold_rich,
            "expected a SELL fill on the rich strike {rich_inst}; fills: {:?}",
            out.portfolio
                .fills
                .iter()
                .map(|f| (f.instrument.clone(), f.side, f.qty))
                .collect::<Vec<_>>()
        );
    }

    /// Empty params must reproduce the default behaviour (the registry's byte-for-byte contract).
    #[test]
    fn empty_params_match_default() {
        let d = LogisticArb::default();
        let e = LogisticArb::from_params(&StrategyParams::new());
        assert_eq!(d.entry_edge, e.entry_edge);
        assert_eq!(d.exit_edge, e.exit_edge);
        assert_eq!(d.max_rmse, e.max_rmse);
        assert_eq!(d.min_vol, e.min_vol);
        assert_eq!(d.max_vol, e.max_vol);
        assert_eq!(d.base.max_position, e.base.max_position);
        assert_eq!(d.base.order_qty, e.base.order_qty);
    }
}
