//! N-tick range-breakout strategy.
//!
//! Idea: track the rolling high/low of the mid over the last `window` observations. When the mid
//! breaks **above** the prior window high, momentum is up — go long. Exit when price reverses back
//! through the rolling mean (mean-reversion stop) or hits a fixed adverse stop in cents.
//!
//! Uses [`RollingWindow`] from the toolkit for the high/low/mean and [`BaseStrategy`] for sizing
//! and position clamping.

use crate::strategies::toolkit::{BaseStrategy, RollingWindow};
use crate::strategies::{Params, StrategyParams};
use crate::strategy::{Ctx, Strategy};
use crate::types::{MarketEvent, Side};
use std::collections::HashMap;

struct InstState {
    window: RollingWindow,
    /// Entry mid price (dollars) of the current long, for the stop.
    entry_mid: Option<f64>,
}

pub struct Breakout {
    base: BaseStrategy,
    window: usize,
    /// Adverse move (dollars) from entry that forces an exit (hard stop).
    stop_dollars: f64,
    state: HashMap<String, InstState>,
}

impl Default for Breakout {
    fn default() -> Self {
        Breakout {
            base: BaseStrategy::new(30.0, 10.0),
            window: 20,
            stop_dollars: 0.05, // 5 cents
            state: HashMap::new(),
        }
    }
}

impl Breakout {
    /// Build from tunable params (keys: `window`, `stop_dollars`, `size`, `max_inventory`). An empty
    /// map reproduces the hardcoded defaults exactly.
    pub fn from_params(params: &StrategyParams) -> Self {
        let p = Params::new(params);
        let d = Breakout::default();
        Breakout {
            base: BaseStrategy::new(
                p.get("max_inventory", d.base.max_position),
                p.get("size", d.base.order_qty),
            ),
            window: p.get_usize("window", d.window),
            stop_dollars: p.get("stop_dollars", d.stop_dollars),
            state: HashMap::new(),
        }
    }
}

impl Strategy for Breakout {
    fn name(&self) -> &str {
        "breakout"
    }

    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
        let inst = ev.instrument().to_string();
        if !self.base.accepts(&inst) {
            return;
        }
        let mid = match ctx.mid(&inst) {
            Some(m) => m,
            None => return,
        };

        let window = self.window;
        let st = self.state.entry(inst.clone()).or_insert_with(|| InstState {
            window: RollingWindow::new(window),
            entry_mid: None,
        });

        // Compute the PRIOR-window high/low/mean before incorporating the new mid.
        let prior_high = st.window.max();
        let prior_mean = st.window.mean();
        let ready = st.window.is_full();
        st.window.push(mid);

        let pos = ctx.position(&inst);

        // --- exits first (stop + mean reversion) ---
        if pos > 1e-9 {
            let mut exit = false;
            if let Some(entry) = st.entry_mid {
                if mid <= entry - self.stop_dollars {
                    exit = true; // hard stop
                }
            }
            if let Some(mean) = prior_mean {
                if mid < mean {
                    exit = true; // reverted below the mean -> bail
                }
            }
            if exit {
                st.entry_mid = None;
                self.base.desired_flatten(&inst, ctx);
                return;
            }
        }

        // --- entry: break above prior rolling high ---
        if ready && pos <= 1e-9 {
            if let Some(hi) = prior_high {
                if mid > hi {
                    let add = self.base.clamp_to_max_position(pos, self.base.order_qty);
                    if add > 0.0 {
                        st.entry_mid = Some(mid);
                        ctx.place_market(&inst, Side::Bid, add);
                    }
                }
            }
        }
    }
}
