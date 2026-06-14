//! Market maker: quote a resting bid and ask around the mid with a configurable half-spread and
//! inventory skew. On each book move it cancels stale quotes and re-posts. Inventory skew shifts
//! quotes down when long (to sell) and up when short (to buy), nudging position back toward flat.

use crate::strategies::{Params, StrategyParams};
use crate::strategy::{Ctx, Strategy};
use crate::types::{Cents, MarketEvent, Side};

pub struct MarketMaker {
    /// Half-spread in cents on each side of the (skewed) mid.
    half_spread_cents: i32,
    /// Quote size in contracts per side.
    qty: f64,
    /// Cents of quote shift per contract of inventory (skew strength).
    skew_per_contract: f64,
    /// Max absolute inventory before we stop adding to the heavy side.
    max_inventory: f64,
}

impl Default for MarketMaker {
    fn default() -> Self {
        MarketMaker {
            half_spread_cents: 2,
            qty: 10.0,
            skew_per_contract: 0.05,
            max_inventory: 50.0,
        }
    }
}

impl MarketMaker {
    /// Build from tunable params (keys: `half_spread_cents`, `quote_size`, `skew`, `max_inventory`).
    /// An empty map reproduces the hardcoded defaults exactly.
    pub fn from_params(params: &StrategyParams) -> Self {
        let p = Params::new(params);
        let d = MarketMaker::default();
        MarketMaker {
            half_spread_cents: p.get_i32("half_spread_cents", d.half_spread_cents),
            qty: p.get("quote_size", d.qty),
            skew_per_contract: p.get("skew", d.skew_per_contract),
            max_inventory: p.get("max_inventory", d.max_inventory),
        }
    }
}

impl MarketMaker {
    fn requote(&self, inst: &str, ctx: &mut dyn Ctx) {
        let mid = match ctx.mid(inst) {
            Some(m) => m * 100.0, // work in cents
            None => return,
        };
        let pos = ctx.position(inst);
        // skew: long inventory -> lower quotes (cents), short -> higher
        let skew = -pos * self.skew_per_contract;
        let center = mid + skew;

        let bid = (center - self.half_spread_cents as f64).round() as i32;
        let ask = (center + self.half_spread_cents as f64).round() as i32;

        // cancel existing quotes for this instrument first
        for o in ctx.open_orders(inst) {
            ctx.cancel(o.id);
        }

        // post bid if not too long
        if bid >= 1 && bid <= 99 && pos < self.max_inventory {
            ctx.place_limit(inst, Side::Bid, Cents(bid), self.qty);
        }
        // post ask if not too short
        if ask >= 1 && ask <= 99 && ask > bid && pos > -self.max_inventory {
            ctx.place_limit(inst, Side::Ask, Cents(ask), self.qty);
        }
    }
}

impl Strategy for MarketMaker {
    fn name(&self) -> &str {
        "market_maker"
    }

    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
        // Re-quote on any book update for the instrument that moved.
        if let MarketEvent::Delta(_) = ev {
            let inst = ev.instrument().to_string();
            self.requote(&inst, ctx);
        }
    }
}
