//! Built-in strategies, selectable by name from the CLI.
//!
//! # How to add a strategy
//!
//! Adding a new idea is **three steps**:
//!
//! 1. **Create the file.** Copy `strategies/template.rs` to `strategies/my_idea.rs`, rename the
//!    struct and the string returned by `Strategy::name()`, and implement your logic in
//!    `on_event` (lean on the reusable primitives in [`crate::strategies::toolkit`]:
//!    `RollingWindow`, `Ema`, `RollingReturn`, `Signal`, `PositionSizer`, `BaseStrategy`).
//! 2. **Declare the module.** Add `pub mod my_idea;` below, and a match arm in [`build`]
//!    returning `Box::new(my_idea::MyIdea::default())`.
//! 3. **Register the name.** Add `"my_idea"` to the [`ALL`] slice so the CLI `--strategy` flag
//!    accepts it (the flag's allowed values come straight from `ALL`).
//!
//! That's it — no engine changes are ever needed; strategies only see the `Ctx` interface.

pub mod avellaneda_stoikov;
pub mod breakout;
pub mod cross_venue_arb;
pub mod imbalance;
pub mod logistic_arb;
pub mod market_maker;
pub mod mean_reversion;
pub mod momentum;
pub mod noop;
pub mod queue_probe;
pub mod template;
pub mod toolkit;

use crate::strategy::Strategy;
use std::collections::BTreeMap;

/// Tunable strategy parameters, keyed by name (the SAME names listed in [`StrategyInfo`]).
///
/// A strategy reads each value with `params.get("key").copied().unwrap_or(<its hardcoded default>)`,
/// so an **empty** map reproduces the strategy's original hardcoded behaviour exactly — this is what
/// keeps `build(name, &StrategyParams::new())` byte-for-byte backward compatible with the old
/// no-argument constructors. The param-sweep / optimization workflow fills this map to tune a run
/// without any code change.
pub type StrategyParams = BTreeMap<String, f64>;

/// Small typed accessor over a [`StrategyParams`] map for ergonomic, well-documented reads.
///
/// `get(key, default)` returns the configured value or the supplied default; `get_usize` / `get_i32`
/// round and clamp to the right integer domain. Strategies use this so the default they pass here is
/// the exact value they used to hardcode.
pub struct Params<'a>(pub &'a StrategyParams);

impl<'a> Params<'a> {
    pub fn new(p: &'a StrategyParams) -> Self {
        Params(p)
    }
    /// Float param `key`, or `default` if unset.
    pub fn get(&self, key: &str, default: f64) -> f64 {
        self.0.get(key).copied().unwrap_or(default)
    }
    /// `usize` param (rounded, floored at 0), or `default` if unset.
    pub fn get_usize(&self, key: &str, default: usize) -> usize {
        match self.0.get(key) {
            Some(v) => v.round().max(0.0) as usize,
            None => default,
        }
    }
    /// `i32` param (rounded), or `default` if unset.
    pub fn get_i32(&self, key: &str, default: i32) -> i32 {
        match self.0.get(key) {
            Some(v) => v.round() as i32,
            None => default,
        }
    }
}

/// Build a strategy by name, configured from `params`. Returns None for an unknown name.
///
/// Passing an empty `params` map yields the strategy's original hardcoded defaults (see
/// [`StrategyParams`]). Param keys match the names shown by `list-strategies` / [`StrategyInfo`].
pub fn build(name: &str, params: &StrategyParams) -> Option<Box<dyn Strategy>> {
    match name {
        "noop" => Some(Box::new(noop::Noop::default())),
        "momentum" => Some(Box::new(momentum::Momentum::from_params(params))),
        "mean_reversion" => Some(Box::new(mean_reversion::MeanReversion::from_params(params))),
        "market_maker" => Some(Box::new(market_maker::MarketMaker::from_params(params))),
        "queue_probe" => Some(Box::new(queue_probe::QueueProbe::from_params(params))),
        "imbalance" => Some(Box::new(imbalance::Imbalance::from_params(params))),
        "breakout" => Some(Box::new(breakout::Breakout::from_params(params))),
        "cross_venue_arb" => Some(Box::new(cross_venue_arb::CrossVenueArb::from_params(params))),
        "logistic_arb" => Some(Box::new(logistic_arb::LogisticArb::from_params(params))),
        "avellaneda_stoikov" => Some(Box::new(
            avellaneda_stoikov::AvellanedaStoikov::from_params(params),
        )),
        "template" => Some(Box::new(template::Template::from_params(params))),
        _ => None,
    }
}

/// Convenience: build a strategy with EMPTY params (== its original hardcoded defaults). Equivalent
/// to `build(name, &StrategyParams::new())`. Kept so call sites that don't tune params stay terse.
pub fn build_default(name: &str) -> Option<Box<dyn Strategy>> {
    build(name, &StrategyParams::new())
}

/// Names of all available strategies (for CLI help / validation).
pub const ALL: &[&str] = &[
    "noop",
    "momentum",
    "mean_reversion",
    "market_maker",
    "queue_probe",
    "imbalance",
    "breakout",
    "cross_venue_arb",
    "avellaneda_stoikov",
    "logistic_arb",
    "template",
];

/// A one-line description plus the key tunable parameters (and their defaults) for a strategy.
/// Used by the `list-strategies` command so a new user can see at a glance what each does.
#[derive(Debug, Clone, Copy)]
pub struct StrategyInfo {
    /// CLI name (matches an entry in [`ALL`] and [`build`]).
    pub name: &'static str,
    /// One-line human description.
    pub description: &'static str,
    /// Key parameters with their defaults, e.g. `"up_ticks=2, size=10"`.
    pub key_params: &'static str,
}

/// Static registry of strategy descriptions, in [`ALL`] order. Kept here (rather than on each
/// strategy struct) so the CLI can list every idea without instantiating it.
pub const INFO: &[StrategyInfo] = &[
    StrategyInfo {
        name: "noop",
        description: "Baseline that observes everything and trades nothing (sanity check).",
        key_params: "(none)",
    },
    StrategyInfo {
        name: "momentum",
        description: "Go long when the mid rises by N ticks over a rolling window; flatten on reversal.",
        key_params: "up_ticks=2, size=10, window=10",
    },
    StrategyInfo {
        name: "mean_reversion",
        description: "Buy when the mid z-score is very negative (cheap), flatten when it reverts.",
        key_params: "window=20, entry_z=1, exit_z=0.25, size=10",
    },
    StrategyInfo {
        name: "market_maker",
        description: "Quote a resting bid+ask around the mid with a half-spread and inventory skew.",
        key_params: "half_spread_cents=2, quote_size=10, skew=0.05, max_inventory=50",
    },
    StrategyInfo {
        name: "queue_probe",
        description: "JOINS the touch (rests at best bid+ask, behind existing depth) — the natural strategy for observing the --queue-model pessimistic/optimistic difference.",
        key_params: "quote_size=10, max_inventory=100",
    },
    StrategyInfo {
        name: "imbalance",
        description: "Trade the persistent top-of-book imbalance / microprice lean (EMA-smoothed).",
        key_params: "imbalance_threshold=0.35, micro_edge=0, ema_period=10, size=10, max_inventory=40",
    },
    StrategyInfo {
        name: "breakout",
        description: "Go long on an N-window range breakout; exit on mean-revert or a hard stop.",
        key_params: "window=20, stop_dollars=0.05, size=10, max_inventory=30",
    },
    StrategyInfo {
        name: "cross_venue_arb",
        description: "CROSS-VENUE: watch the same underlying on two venues; buy the cheaper, sell the richer, flatten on convergence.",
        key_params: "entry_edge=0.03, exit_edge=0.01, size=10",
    },
    StrategyInfo {
        name: "avellaneda_stoikov",
        description: "Avellaneda-Stoikov optimal MM: inventory-aware reservation price + optimal spread on the Kalshi binary book.",
        key_params: "gamma=0.1, kappa=1.5, sigma_window=30, horizon_secs=3600, quote_size=10, max_inventory=50",
    },
    StrategyInfo {
        name: "logistic_arb",
        description: "LADDER FIT: fit a logistic survival across an event's strikes; sell strikes RICH vs the curve, buy CHEAP, gated by fit RMSE + implied vol. Uses the implied fair value (mu) and implied vol as trust filters.",
        key_params: "entry_edge=0.03, exit_edge=0.01, qty=10, max_inventory=40, max_rmse=0.05, min_vol=0.02, max_vol=0.6",
    },
    StrategyInfo {
        name: "template",
        description: "Minimal copy-me example: z-score mean-reversion off the mid (a starting point).",
        key_params: "window=20, entry_z=1.5, exit_z=0.25, size=10",
    },
];

/// Look up the [`StrategyInfo`] for a strategy name, if known.
pub fn info(name: &str) -> Option<&'static StrategyInfo> {
    INFO.iter().find(|i| i.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_strategy_builds() {
        let empty = StrategyParams::new();
        for name in ALL {
            assert!(build(name, &empty).is_some(), "strategy {name} failed to build");
            assert!(build_default(name).is_some(), "strategy {name} build_default failed");
        }
        assert!(build("does_not_exist", &empty).is_none());
        assert!(build_default("does_not_exist").is_none());
    }

    #[test]
    fn avellaneda_stoikov_is_registered() {
        assert!(ALL.contains(&"avellaneda_stoikov"));
        assert!(build_default("avellaneda_stoikov").is_some());
        let inf = info("avellaneda_stoikov").expect("A-S info present");
        assert!(inf.key_params.contains("gamma"));
        assert!(inf.key_params.contains("kappa"));
    }

    /// Empty params must reproduce the hardcoded defaults EXACTLY: driving the engine with
    /// `build(name, empty)` yields the same fills/cash as the strategy's `Default` constructor.
    #[test]
    fn empty_params_match_default_on_fixed_stream() {
        use crate::config::BacktestConfig;
        use crate::engine::Engine;
        use crate::types::{Action, BookDelta, Cents, MarketEvent, Side, TradeEvent};

        fn stream() -> Vec<MarketEvent> {
            let d = |ts: i64, side: Side, px: i32, sz: f64, seq: i64, snap: bool| {
                MarketEvent::Delta(BookDelta {
                    ts_ns: ts,
                    instrument: "KXTEST-A".into(),
                    action: Action::Add,
                    side,
                    price: Cents(px),
                    size: sz,
                    sequence: seq,
                    is_snapshot: snap,
                })
            };
            let t = |ts: i64, px: i32, sz: f64| {
                MarketEvent::Trade(TradeEvent {
                    ts_ns: ts,
                    instrument: "KXTEST-A".into(),
                    aggressor_yes: true,
                    price: Cents(px),
                    size: sz,
                    trade_id: format!("t{ts}"),
                })
            };
            let mut v = vec![
                d(1_000, Side::Bid, 45, 200.0, 1, true),
                d(1_001, Side::Ask, 55, 200.0, 2, false),
            ];
            // a sequence of book moves to trigger requote / momentum decisions
            for (i, px) in [47, 49, 51, 53, 51, 49].iter().enumerate() {
                let ts = 2_000 + i as i64 * 1_000;
                v.push(d(ts, Side::Bid, px - 1, 100.0, 10 + i as i64 * 2, false));
                v.push(d(ts + 1, Side::Ask, px + 1, 100.0, 11 + i as i64 * 2, false));
                v.push(t(ts + 2, *px, 5.0));
            }
            v
        }

        for name in ["momentum", "market_maker", "queue_probe", "mean_reversion", "breakout", "template", "avellaneda_stoikov"] {
            let cfg = BacktestConfig::default();

            let eng_a = Engine::new(&cfg);
            let mut s_def = build_default(name).unwrap();
            let out_def = eng_a.run_collecting(stream().into_iter(), s_def.as_mut(), &cfg);

            let eng_b = Engine::new(&cfg);
            let mut s_emp = build(name, &StrategyParams::new()).unwrap();
            let out_emp = eng_b.run_collecting(stream().into_iter(), s_emp.as_mut(), &cfg);

            let ja = serde_json::to_string(&out_def.report).unwrap();
            let jb = serde_json::to_string(&out_emp.report).unwrap();
            assert_eq!(ja, jb, "{name}: empty-params report != default report");
            assert_eq!(
                out_def.portfolio.fills.len(),
                out_emp.portfolio.fills.len(),
                "{name}: fill count differs"
            );
        }
    }

    #[test]
    fn params_accessor_reads_defaults_and_overrides() {
        let mut m = StrategyParams::new();
        m.insert("gamma".to_string(), 0.5);
        let p = Params::new(&m);
        assert_eq!(p.get("gamma", 0.1), 0.5); // overridden
        assert_eq!(p.get("kappa", 1.5), 1.5); // default
        assert_eq!(p.get_usize("window", 30), 30); // default
        m.insert("window".to_string(), 12.0);
        let p = Params::new(&m);
        assert_eq!(p.get_usize("window", 30), 12);
    }

    #[test]
    fn every_strategy_has_info_and_matches_all() {
        // The INFO registry must cover exactly the strategies in ALL, in the same order, and each
        // described name must actually build.
        assert_eq!(INFO.len(), ALL.len());
        for (i, name) in ALL.iter().enumerate() {
            assert_eq!(INFO[i].name, *name, "INFO order must match ALL");
            let inf = info(name).expect("info present");
            assert!(!inf.description.is_empty());
            assert!(build_default(name).is_some());
        }
        assert!(info("does_not_exist").is_none());
    }
}
