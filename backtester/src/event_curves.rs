//! Per-event strike ladders, fitted lazily to a logistic survival curve.
//!
//! The engine maintains one [`EventCurves`] holding, per settlement **event**, the
//! current mid of every "above $K" strike on its ladder. On every book delta the
//! engine calls [`EventCurves::update`] with the touched instrument's new mid — an
//! O(log n) `BTreeMap` write that just records the mid and marks the event
//! **dirty**. No fit happens there.
//!
//! A strategy (via the engine's `Ctx`) asks for the fit with [`EventCurves::fit`],
//! which re-fits the logistic across the ladder **only if dirty** and caches the
//! result. This gives the literal "fit per delta" semantics — the curve a strategy
//! reads always reflects the most recent delta — while paying **zero** cost when no
//! strategy ever asks. That zero-cost-when-unused property is what keeps
//! non-logistic backtests byte-for-byte identical: `update` only mutates this
//! side-state, never the book / fills / PnL, and `fit` is never invoked unless a
//! logistic `Ctx` method is called.

use crate::fit_logistic::{fit, parse_event_strike, LogisticFit};
use std::collections::{BTreeMap, HashMap};

/// Encode a dollar strike `K` as an exact integer `BTreeMap` key in 1e-4 units
/// (e.g. `2.850 → 28500`). Using an integer key keeps map ordering deterministic
/// and dodges `f64` `Ord`/`Hash` issues — no `OrderedFloat` dependency needed.
#[inline]
fn strike_key(k: f64) -> i64 {
    (k * 1e4).round() as i64
}

/// Decode an integer strike key back to dollars (inverse of [`strike_key`]).
#[inline]
fn strike_dollars(key: i64) -> f64 {
    key as f64 / 1e4
}

/// One event's strike ladder: the current mid at each strike, a dirty flag, and the
/// cached fit.
#[derive(Debug, Clone, Default)]
pub struct StrikeLadder {
    /// `strike (1e-4 units) -> current mid in (0,1)`. Sorted by strike (BTreeMap),
    /// which is exactly the order the logistic fit wants.
    mids: BTreeMap<i64, f64>,
    /// True when `mids` changed since the last fit (so the cache is stale).
    dirty: bool,
    /// Last computed fit (cleared/recomputed lazily). `None` until first fitted or
    /// when the ladder is too sparse/degenerate to fit.
    cached: Option<LogisticFit>,
}

impl StrikeLadder {
    /// Set (or replace) the mid at `strike`. A mid outside `(0, 1)` — or `None` —
    /// **removes** the strike, since a 0/1 (or absent) mid carries no curvature
    /// information for the fit. Marks the ladder dirty iff something actually
    /// changed.
    fn set(&mut self, strike: f64, mid: Option<f64>) {
        let key = strike_key(strike);
        let changed = match mid {
            Some(m) if m > 0.0 && m < 1.0 => {
                // Only dirty if the value is genuinely new (avoids needless refits
                // when an unrelated level of the same instrument's book ticks).
                match self.mids.get(&key) {
                    Some(&old) if old == m => false,
                    _ => {
                        self.mids.insert(key, m);
                        true
                    }
                }
            }
            _ => self.mids.remove(&key).is_some(),
        };
        if changed {
            self.dirty = true;
        }
    }

    /// The current `(strike, mid)` points, strike-ascending, for the fit.
    fn points(&self) -> Vec<(f64, f64)> {
        self.mids
            .iter()
            .map(|(&k, &m)| (strike_dollars(k), m))
            .collect()
    }

    /// Re-fit if dirty, then return the cached fit. Lazy: a clean ladder returns the
    /// cached result without recomputing.
    fn fit(&mut self) -> Option<&LogisticFit> {
        if self.dirty {
            self.cached = fit(&self.points());
            self.dirty = false;
        }
        self.cached.as_ref()
    }
}

/// All events' ladders, keyed by event id (the part of a ladder instrument before
/// its `-T<strike>` tag).
#[derive(Debug, Clone, Default)]
pub struct EventCurves {
    events: HashMap<String, StrikeLadder>,
}

impl EventCurves {
    pub fn new() -> Self {
        EventCurves::default()
    }

    /// Record a new mid for a ladder instrument. Parses the instrument into
    /// `(event, strike)`; **non-ladder instruments are silently ignored** (so a
    /// non-NatGas instrument, or one with no `-T<strike>` tag, never creates an
    /// event). Cheap: an O(log n) map write that marks the event dirty. No fit here.
    pub fn update(&mut self, instrument: &str, mid: Option<f64>) {
        let (event, strike) = match parse_event_strike(instrument) {
            Some(es) => es,
            None => return, // not a ladder strike — ignore
        };
        self.events
            .entry(event.to_string())
            .or_default()
            .set(strike, mid);
    }

    /// Lazily fit (if dirty) and return the cached logistic fit for `event`.
    /// Returns `None` if the event is unknown or its ladder can't be fitted (too few
    /// usable strikes / degenerate). The fit reflects every delta seen so far,
    /// because `update` marked the event dirty on each change.
    pub fn fit(&mut self, event: &str) -> Option<&LogisticFit> {
        self.events.get_mut(event)?.fit()
    }

    /// Convenience for the engine's `Ctx` methods: parse `instrument` to its event
    /// and return that event's fit (lazily computed). `None` for non-ladder
    /// instruments or unfittable ladders.
    pub fn fit_for_instrument(&mut self, instrument: &str) -> Option<&LogisticFit> {
        let event = parse_event_strike(instrument)?.0.to_string();
        self.fit(&event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean logistic ladder of mids for one event, as engine `update` calls.
    fn feed_logistic(ec: &mut EventCurves, event: &str, mu: f64, s: f64, n: usize) {
        let (k_lo, k_hi) = (mu - 4.0 * s, mu + 4.0 * s);
        for i in 0..n {
            let k = k_lo + (k_hi - k_lo) * i as f64 / (n - 1) as f64;
            let surv = 1.0 / (1.0 + ((k - mu) / s).exp());
            let inst = format!("{event}-T{k:.4}");
            ec.update(&inst, Some(surv));
        }
    }

    #[test]
    fn update_then_fit_recovers_mu_and_caches() {
        let mut ec = EventCurves::new();
        let (mu, s) = (3.0, 0.075);
        feed_logistic(&mut ec, "KXNATGASD-26JUN1517", mu, s, 28);

        let f = ec
            .fit("KXNATGASD-26JUN1517")
            .copied()
            .expect("ladder should fit");
        assert!((f.mu - mu).abs() < 1e-2, "mu off: {}", f.mu);
        assert!((f.s - s).abs() < 1e-2, "s off: {}", f.s);

        // Second fit with no intervening update returns the SAME cached object
        // (clean ladder ⇒ no refit). We can't observe "no recompute" directly, but
        // the result must be identical.
        let f2 = ec.fit("KXNATGASD-26JUN1517").copied().unwrap();
        assert_eq!(f, f2);
    }

    #[test]
    fn update_marks_dirty_and_changes_fit() {
        let mut ec = EventCurves::new();
        feed_logistic(&mut ec, "EVT", 3.0, 0.08, 24);
        let mu1 = ec.fit("EVT").unwrap().mu;

        // Shift the whole ladder's implied median up by re-feeding with a higher mu.
        feed_logistic(&mut ec, "EVT", 3.25, 0.08, 24);
        let mu2 = ec.fit("EVT").unwrap().mu;
        assert!(mu2 > mu1 + 0.1, "fit did not track the new ladder: {mu1} -> {mu2}");
    }

    #[test]
    fn non_ladder_instrument_is_ignored() {
        let mut ec = EventCurves::new();
        // No -T<strike> tag ⇒ no event is ever created.
        ec.update("KXNATGASD-26JUN1517", Some(0.5));
        ec.update("SOMETHING-ELSE", Some(0.4));
        assert!(ec.fit("KXNATGASD-26JUN1517").is_none());
        assert!(ec.events.is_empty());
    }

    #[test]
    fn mid_out_of_range_removes_strike() {
        let mut ec = EventCurves::new();
        feed_logistic(&mut ec, "EVT", 3.0, 0.08, 24);
        assert!(ec.fit("EVT").is_some());
        // Remove enough strikes (push mids to a boundary) to drop below 4 usable.
        for i in 0..22 {
            let k = 3.0 - 0.32 + 0.64 * i as f64 / 23.0;
            ec.update(&format!("EVT-T{k:.4}"), None);
        }
        // Now too few usable points ⇒ fit returns None.
        assert!(ec.fit("EVT").is_none());
    }

    #[test]
    fn fit_for_instrument_routes_to_event() {
        let mut ec = EventCurves::new();
        feed_logistic(&mut ec, "KXNATGASD-26JUN1517", 3.0, 0.075, 28);
        let f = ec
            .fit_for_instrument("KXNATGASD-26JUN1517-T3.000")
            .copied()
            .expect("instrument routes to its event fit");
        assert!((f.mu - 3.0).abs() < 1e-2);
        // venue-tagged id routes the same way
        assert!(ec
            .fit_for_instrument("KALSHI:KXNATGASD-26JUN1517-T2.900")
            .is_some());
    }
}
