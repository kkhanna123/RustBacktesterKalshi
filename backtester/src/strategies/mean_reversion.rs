//! Mean reversion: compute the z-score of the mid over a rolling window; buy when z is very
//! negative (price unusually cheap) and sell/flatten when z is very positive. Mirrors the intent
//! of the infra vectorbt mean-reversion entries/exits.

use crate::strategies::{Params, StrategyParams};
use crate::strategy::{Ctx, Strategy};
use crate::types::{MarketEvent, Side};
use std::collections::{HashMap, VecDeque};

pub struct MeanReversion {
    window: usize,
    entry_z: f64,
    exit_z: f64,
    qty: f64,
    history: HashMap<String, VecDeque<f64>>,
}

impl Default for MeanReversion {
    fn default() -> Self {
        MeanReversion {
            window: 20,
            entry_z: 1.0,
            exit_z: 0.25,
            qty: 10.0,
            history: HashMap::new(),
        }
    }
}

impl MeanReversion {
    /// Build from tunable params (keys: `window`, `entry_z`, `exit_z`, `size`). An empty map
    /// reproduces the hardcoded defaults exactly.
    pub fn from_params(params: &StrategyParams) -> Self {
        let p = Params::new(params);
        let d = MeanReversion::default();
        MeanReversion {
            window: p.get_usize("window", d.window),
            entry_z: p.get("entry_z", d.entry_z),
            exit_z: p.get("exit_z", d.exit_z),
            qty: p.get("size", d.qty),
            history: HashMap::new(),
        }
    }
}

fn zscore(hist: &VecDeque<f64>, x: f64) -> Option<f64> {
    let n = hist.len();
    if n < 2 {
        return None;
    }
    let mean = hist.iter().sum::<f64>() / n as f64;
    let var = hist.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
    let sd = var.sqrt();
    if sd < 1e-9 {
        None
    } else {
        Some((x - mean) / sd)
    }
}

impl Strategy for MeanReversion {
    fn name(&self) -> &str {
        "mean_reversion"
    }

    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
        let inst = ev.instrument().to_string();
        let mid = match ctx.mid(&inst) {
            Some(m) => m,
            None => return,
        };

        let hist = self.history.entry(inst.clone()).or_default();
        let z = zscore(hist, mid);
        hist.push_back(mid);
        while hist.len() > self.window {
            hist.pop_front();
        }

        let z = match z {
            Some(z) if hist.len() >= self.window => z,
            _ => return,
        };

        let pos = ctx.position(&inst);

        if z <= -self.entry_z {
            // unusually cheap -> buy
            if pos <= 0.0 {
                ctx.place_market(&inst, Side::Bid, self.qty);
            }
        } else if z >= self.entry_z {
            // unusually rich -> reduce/flatten longs
            if pos > 0.0 {
                ctx.place_market(&inst, Side::Ask, pos);
            }
        } else if z.abs() <= self.exit_z && pos > 0.0 {
            // reverted to the mean -> take profit / flatten
            ctx.place_market(&inst, Side::Ask, pos);
        }
    }
}
