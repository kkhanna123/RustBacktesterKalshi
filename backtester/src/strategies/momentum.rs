//! Momentum: go long when the mid has risen by at least `up_ticks` cents over a rolling window,
//! flatten (and optionally flip short) on a comparable reversal. Operates per-instrument and
//! uses immediate market orders for simplicity and determinism.

use crate::strategies::{Params, StrategyParams};
use crate::strategy::{Ctx, Strategy};
use crate::types::{MarketEvent, Side};
use std::collections::HashMap;
use std::collections::VecDeque;

pub struct Momentum {
    window: usize,
    /// Cents of rise/fall to trigger.
    threshold_cents: i32,
    /// Order size in contracts.
    qty: f64,
    /// Per-instrument mid history (in cents).
    history: HashMap<String, VecDeque<i32>>,
}

impl Default for Momentum {
    fn default() -> Self {
        Momentum {
            window: 10,
            threshold_cents: 2,
            qty: 10.0,
            history: HashMap::new(),
        }
    }
}

impl Momentum {
    /// Build from tunable params (keys: `window`, `up_ticks`, `size`). An empty map reproduces the
    /// hardcoded defaults exactly.
    pub fn from_params(params: &StrategyParams) -> Self {
        let p = Params::new(params);
        let d = Momentum::default();
        Momentum {
            window: p.get_usize("window", d.window),
            threshold_cents: p.get_i32("up_ticks", d.threshold_cents),
            qty: p.get("size", d.qty),
            history: HashMap::new(),
        }
    }
}

impl Strategy for Momentum {
    fn name(&self) -> &str {
        "momentum"
    }

    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
        let inst = ev.instrument().to_string();
        let mid = match ctx.mid(&inst) {
            Some(m) => (m * 100.0).round() as i32,
            None => return,
        };

        let hist = self.history.entry(inst.clone()).or_default();
        hist.push_back(mid);
        while hist.len() > self.window {
            hist.pop_front();
        }
        if hist.len() < self.window {
            return;
        }
        let first = *hist.front().unwrap();
        let change = mid - first;
        let pos = ctx.position(&inst);

        if change >= self.threshold_cents {
            // upward momentum: be long
            if pos <= 0.0 {
                ctx.place_market(&inst, Side::Bid, self.qty + pos.abs());
            }
        } else if change <= -self.threshold_cents {
            // downward momentum: flatten longs (don't short binary aggressively)
            if pos > 0.0 {
                ctx.place_market(&inst, Side::Ask, pos);
            }
        }
    }

    fn on_finish(&mut self, _ctx: &mut dyn Ctx) {}
}
