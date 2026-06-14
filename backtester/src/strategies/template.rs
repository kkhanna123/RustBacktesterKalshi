//! TEMPLATE STRATEGY — copy this file to `src/strategies/my_idea.rs` to test a new idea.
//!
//! This is the **smallest possible** path to a working strategy. It keeps a [`RollingWindow`] of
//! recent mids per instrument, computes the z-score of the latest mid, and places a single market
//! order when the z-score is extreme (a tiny mean-reversion idea). Read the comments top-to-bottom;
//! every line you'd normally touch is called out.
//!
//! ## To make your own idea
//! 1. Copy this file, rename the struct (e.g. `MyIdea`) and `name()` return value.
//! 2. Change the state you accumulate (window, EMAs, returns — see `strategies::toolkit`).
//! 3. Change the logic in `on_event`: derive a `Signal`, size it, and place orders.
//! 4. Register it (3 steps) — see the "How to add a strategy" doc in `strategies/mod.rs`.

use crate::strategies::toolkit::{BaseStrategy, RollingWindow, Signal};
use crate::strategies::{Params, StrategyParams};
use crate::strategy::{Ctx, Strategy};
use crate::types::{MarketEvent, Side};
use std::collections::HashMap;

/// A minimal example strategy: z-score mean-reversion off the mid price.
pub struct Template {
    /// Shared knobs + helpers (max position, order size, instrument filter).
    base: BaseStrategy,
    /// How many mids to remember per instrument before we trust the z-score.
    window: usize,
    /// |z| threshold at which we open a position.
    entry_z: f64,
    /// |z| at/below which we flatten (consider ourselves "reverted to the mean").
    exit_z: f64,
    /// Per-instrument rolling window of recent mid prices.
    mids: HashMap<String, RollingWindow>,
}

impl Default for Template {
    fn default() -> Self {
        Template {
            base: BaseStrategy::new(30.0, 10.0),
            window: 20,
            entry_z: 1.5,
            exit_z: 0.25,
            mids: HashMap::new(),
        }
    }
}

impl Template {
    /// Build from tunable params (keys: `window`, `entry_z`, `exit_z`, `size`, `max_inventory`). An
    /// empty map reproduces the hardcoded defaults exactly.
    pub fn from_params(params: &StrategyParams) -> Self {
        let p = Params::new(params);
        let d = Template::default();
        Template {
            base: BaseStrategy::new(
                p.get("max_inventory", d.base.max_position),
                p.get("size", d.base.order_qty),
            ),
            window: p.get_usize("window", d.window),
            entry_z: p.get("entry_z", d.entry_z),
            exit_z: p.get("exit_z", d.exit_z),
            mids: HashMap::new(),
        }
    }
}

impl Strategy for Template {
    fn name(&self) -> &str {
        "template"
    }

    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
        let inst = ev.instrument().to_string();
        // (1) respect the optional instrument filter
        if !self.base.accepts(&inst) {
            return;
        }
        // (2) need a mid to do anything
        let mid = match ctx.mid(&inst) {
            Some(m) => m,
            None => return,
        };

        // (3) update our per-instrument state
        let win = self
            .mids
            .entry(inst.clone())
            .or_insert_with(|| RollingWindow::new(self.window));
        // z-score of the *new* mid against the window BEFORE we add it
        let z = win.zscore(mid);
        win.push(mid);
        if !win.is_full() {
            return; // not enough history yet
        }
        let z = match z {
            Some(z) => z,
            None => return, // flat window, no signal
        };

        // (4) turn the number into a decision
        let pos = ctx.position(&inst);
        match Signal::from_zscore_reversion(z, self.entry_z, self.exit_z) {
            Signal::Long if pos <= 0.0 => {
                let add = self.base.clamp_to_max_position(pos, self.base.order_qty);
                if add > 0.0 {
                    ctx.place_market(&inst, Side::Bid, add);
                }
            }
            Signal::Short | Signal::Flat if pos > 0.0 => {
                // simplest exit: flatten the long
                self.base.desired_flatten(&inst, ctx);
            }
            _ => {}
        }
    }
}
