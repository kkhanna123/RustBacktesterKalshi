//! Order-book imbalance / microprice strategy.
//!
//! Idea: when the top of book is heavily bid (positive imbalance) and the microprice sits above
//! the mid, near-term pressure is upward — go long. When it flips heavily offered, flatten/short.
//! We smooth the raw imbalance with an [`Ema`] so we trade the *persistent* lean, not single-tick
//! noise. Entries/exits are immediate market orders for determinism.
//!
//! Built almost entirely from `strategies::toolkit`, showing how little code an idea needs.

use crate::strategies::toolkit::{BaseStrategy, Ema};
use crate::strategies::{Params, StrategyParams};
use crate::strategy::{Ctx, Strategy};
use crate::types::{MarketEvent, Side};
use std::collections::HashMap;

pub struct Imbalance {
    base: BaseStrategy,
    /// Smoothed imbalance must exceed this magnitude to trigger.
    entry_threshold: f64,
    /// Microprice must lead the mid by at least this many dollars to confirm the lean.
    micro_edge: f64,
    /// Span of the imbalance-smoothing EMA (period; `alpha = 2/(n+1)`).
    ema_period: usize,
    /// Per-instrument EMA of top-of-book imbalance.
    imb: HashMap<String, Ema>,
}

impl Default for Imbalance {
    fn default() -> Self {
        Imbalance {
            base: BaseStrategy::new(40.0, 10.0),
            entry_threshold: 0.35,
            micro_edge: 0.0,
            ema_period: 10,
            imb: HashMap::new(),
        }
    }
}

impl Imbalance {
    /// Build from tunable params (keys: `imbalance_threshold`, `micro_edge`, `ema_period`, `size`,
    /// `max_inventory`). An empty map reproduces the hardcoded defaults exactly.
    pub fn from_params(params: &StrategyParams) -> Self {
        let p = Params::new(params);
        let d = Imbalance::default();
        Imbalance {
            base: BaseStrategy::new(
                p.get("max_inventory", d.base.max_position),
                p.get("size", d.base.order_qty),
            ),
            entry_threshold: p.get("imbalance_threshold", d.entry_threshold),
            micro_edge: p.get("micro_edge", d.micro_edge),
            ema_period: p.get_usize("ema_period", d.ema_period),
            imb: HashMap::new(),
        }
    }
}

impl Strategy for Imbalance {
    fn name(&self) -> &str {
        "imbalance"
    }

    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
        // Only react to book updates (imbalance is a book signal).
        if !matches!(ev, MarketEvent::Delta(_)) {
            return;
        }
        let inst = ev.instrument().to_string();
        if !self.base.accepts(&inst) {
            return;
        }

        let raw_imb = match ctx.imbalance(&inst) {
            Some(i) => i,
            None => return,
        };
        let mid = ctx.mid(&inst);
        let micro = ctx.microprice(&inst);

        // smooth the imbalance
        let ema_period = self.ema_period;
        let ema = self
            .imb
            .entry(inst.clone())
            .or_insert_with(|| Ema::from_period(ema_period));
        let smooth = ema.update(raw_imb);

        // microprice edge over mid confirms direction (defaults to 0 = no confirmation needed)
        let edge = match (micro, mid) {
            (Some(mp), Some(m)) => mp - m,
            _ => 0.0,
        };

        let pos = ctx.position(&inst);

        if smooth >= self.entry_threshold && edge >= self.micro_edge {
            // upward pressure -> be long up to max
            if pos < self.base.max_position - 1e-9 {
                let add = self.base.clamp_to_max_position(pos, self.base.order_qty);
                if add > 0.0 {
                    ctx.place_market(&inst, Side::Bid, add);
                }
            }
        } else if smooth <= -self.entry_threshold && edge <= -self.micro_edge {
            // downward pressure -> flatten any long (binary markets: avoid aggressive shorts)
            if pos > 1e-9 {
                self.base.desired_flatten(&inst, ctx);
            }
        }
    }
}
