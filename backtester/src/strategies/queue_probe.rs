//! Queue-probe maker: JOINS the touch (rests at the current best bid AND best ask) rather than
//! quoting inside the spread. Because it rests *behind the existing depth* at those price levels, its
//! orders carry a nonzero `queue_ahead` — which makes it the natural strategy for observing the
//! maker-queue model (`--queue-model pessimistic` vs `optimistic`): under `optimistic`, cancellations
//! that shrink the depth ahead of it advance its queue position and let it fill sooner.
//!
//! It re-quotes whenever the touch moves, cancelling stale joins and re-posting at the new best
//! bid/ask, subject to a max-inventory guard so it stops adding to the heavy side.

use crate::strategies::{Params, StrategyParams};
use crate::strategy::{Ctx, Strategy};
use crate::types::{MarketEvent, Side};

pub struct QueueProbe {
    /// Contracts to rest on each side when joining the touch.
    qty: f64,
    /// Stop adding to a side once |inventory| reaches this.
    max_inventory: f64,
}

impl Default for QueueProbe {
    fn default() -> Self {
        QueueProbe {
            qty: 10.0,
            max_inventory: 100.0,
        }
    }
}

impl QueueProbe {
    /// Build from tunable params (keys: `quote_size`, `max_inventory`). An empty map reproduces the
    /// hardcoded defaults exactly.
    pub fn from_params(params: &StrategyParams) -> Self {
        let p = Params::new(params);
        let d = QueueProbe::default();
        QueueProbe {
            qty: p.get("quote_size", d.qty),
            max_inventory: p.get("max_inventory", d.max_inventory),
        }
    }

    /// Maintain a join at the touch on each side. Crucially, an order ALREADY resting at the current
    /// best price is LEFT IN PLACE (not cancelled and re-posted) — so it KEEPS its queue position as
    /// the book evolves. We only cancel a quote that is no longer at the touch, and only post a fresh
    /// quote on a side where we have none at the touch. This "leave it if it's still at the touch"
    /// behaviour is what lets the maker-queue model (`--queue-model`) actually matter: under the
    /// optimistic model a cancellation ahead of a kept order advances it; under pessimistic it does
    /// not.
    fn rejoin(&self, inst: &str, ctx: &mut dyn Ctx) {
        let bid = ctx.best_bid(inst).map(|(p, _)| p);
        let ask = ctx.best_ask(inst).map(|(p, _)| p);
        let pos = ctx.position(inst);

        // Inspect our current resting orders: do we already sit at the current best bid / ask?
        let mut have_bid_at_touch = false;
        let mut have_ask_at_touch = false;
        for o in ctx.open_orders(inst) {
            let at_touch = match o.side {
                Side::Bid => Some(o.price) == bid,
                Side::Ask => Some(o.price) == ask,
            };
            if at_touch {
                match o.side {
                    Side::Bid => have_bid_at_touch = true,
                    Side::Ask => have_ask_at_touch = true,
                }
            } else {
                // stale quote (touch moved away from it) -> cancel so we can re-join the new touch.
                ctx.cancel(o.id);
            }
        }

        if let Some(bid_px) = bid {
            if !have_bid_at_touch && pos < self.max_inventory {
                ctx.place_limit(inst, Side::Bid, bid_px, self.qty);
            }
        }
        if let Some(ask_px) = ask {
            if !have_ask_at_touch && pos > -self.max_inventory {
                ctx.place_limit(inst, Side::Ask, ask_px, self.qty);
            }
        }
    }
}

impl Strategy for QueueProbe {
    fn name(&self) -> &str {
        "queue_probe"
    }

    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
        if let MarketEvent::Delta(_) = ev {
            let inst = ev.instrument().to_string();
            self.rejoin(&inst, ctx);
        }
    }
}
