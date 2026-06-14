//! No-op baseline strategy: observes everything, trades nothing. Useful as a sanity check that
//! the engine, data path, and report all work with zero activity.

use crate::strategy::{Ctx, Strategy};
use crate::types::MarketEvent;

#[derive(Default)]
pub struct Noop;

impl Strategy for Noop {
    fn name(&self) -> &str {
        "noop"
    }
    fn on_event(&mut self, _ev: &MarketEvent, _ctx: &mut dyn Ctx) {}
}
