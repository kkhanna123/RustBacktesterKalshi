//! Kalshi liquidity-incentive (maker rewards) model.
//!
//! Kalshi runs liquidity programs that pay market-makers a share of a per-period reward pool for
//! keeping tight, sized, two-sided quotes resting on the book. This module models that accrual:
//!
//! * Each period is `period_secs` long and pays out up to `reward_per_period` dollars.
//! * At every event the engine reports whether the strategy is currently *qualifying* — resting at
//!   least `min_resting_size` contracts within `max_spread_cents` of the mid (on BOTH sides if
//!   `both_sides_required`).
//! * The model integrates qualifying *wall-clock time* and credits
//!   `reward_per_period * (qualifying_time_in_period / period_secs)` per period, capped at
//!   `reward_per_period` per period. (Simple single-participant share model: we credit the modeled
//!   participant's share directly rather than splitting a pool across competitors.)
//!
//! The accrued total is exposed via [`RewardsModel::accrued`]. The engine adds it to PnL only when
//! `include_rewards` is true; otherwise it is reported but not credited to cash. When the model is
//! disabled, nothing accrues.

use crate::config::RewardsConfig;
use crate::types::Side;

/// Qualifying state of the strategy's resting quotes at a point in time, as seen by the engine.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuoteState {
    /// Total resting size within `max_spread_cents` of the mid on the bid side.
    pub bid_size_in_band: f64,
    /// Total resting size within `max_spread_cents` of the mid on the ask side.
    pub ask_size_in_band: f64,
    /// True only if a mid exists (no mid => cannot judge the band => not qualifying).
    pub has_mid: bool,
}

/// A liquidity-rewards model derived from a [`RewardsConfig`].
#[derive(Debug, Clone)]
pub struct RewardsModel {
    enabled: bool,
    period_ns: i64,
    reward_per_period: f64,
    min_resting_size: f64,
    both_sides_required: bool,

    /// Wall-clock ns of the last `observe` call (start of the segment we're closing now).
    last_ts: Option<i64>,
    /// Whether the strategy was qualifying during the segment ending at `last_ts`.
    last_qualifying: bool,
    /// Qualifying nanoseconds accumulated within the *current* period window.
    qualifying_ns_in_period: i64,
    /// Index of the current period (ts / period_ns).
    cur_period: i64,
    /// Total accrued rewards (dollars) across all closed periods + the current partial period.
    accrued: f64,
    /// Accrued rewards already "locked in" from fully or partially elapsed prior periods.
    locked: f64,
}

impl RewardsModel {
    /// Build from config. A disabled config accrues nothing.
    pub fn from_config(cfg: &RewardsConfig) -> Self {
        let period_secs = cfg.period_secs.max(1);
        RewardsModel {
            enabled: cfg.enabled,
            period_ns: period_secs.saturating_mul(1_000_000_000),
            reward_per_period: cfg.reward_per_period.max(0.0),
            min_resting_size: cfg.min_resting_size.max(0.0),
            both_sides_required: cfg.both_sides_required,
            last_ts: None,
            last_qualifying: false,
            qualifying_ns_in_period: 0,
            cur_period: 0,
            accrued: 0.0,
            locked: 0.0,
        }
    }

    /// True if rewards are being modeled.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.enabled
    }

    /// The min-spread band qualification: per [`QuoteState`] passed by the engine, the engine has
    /// already filtered to in-band sizes, so here we only check the size + both-sides requirement.
    fn qualifies(&self, q: &QuoteState) -> bool {
        if !q.has_mid {
            return false;
        }
        let bid_ok = q.bid_size_in_band >= self.min_resting_size;
        let ask_ok = q.ask_size_in_band >= self.min_resting_size;
        if self.both_sides_required {
            bid_ok && ask_ok
        } else {
            bid_ok || ask_ok
        }
    }

    /// Advance the model to time `now`, attributing the just-elapsed segment
    /// `[last_ts, now)` to the qualifying state that held *during* it. Period boundaries are
    /// handled by splitting the segment so each period only ever counts up to its full length.
    ///
    /// `q` is the qualifying state observed *at* `now` (used for the next segment).
    pub fn observe(&mut self, now: i64, q: &QuoteState) {
        if !self.enabled {
            return;
        }
        if let Some(prev) = self.last_ts {
            if now > prev && self.last_qualifying {
                self.credit_segment(prev, now);
            } else if now > prev {
                // non-qualifying time still advances the period clock (handles boundary crossing)
                self.advance_period_clock(now);
            }
        } else {
            // first observation: initialize the period index
            self.cur_period = now.div_euclid(self.period_ns);
        }
        self.last_ts = Some(now);
        self.last_qualifying = self.qualifies(q);
    }

    /// Credit a fully-qualifying segment `[a, b)`, splitting across period boundaries.
    fn credit_segment(&mut self, a: i64, b: i64) {
        let mut t = a;
        while t < b {
            let period = t.div_euclid(self.period_ns);
            let period_end = (period + 1).saturating_mul(self.period_ns);
            let seg_end = b.min(period_end);
            self.roll_to_period(period);
            self.qualifying_ns_in_period = self
                .qualifying_ns_in_period
                .saturating_add(seg_end - t)
                .min(self.period_ns);
            t = seg_end;
        }
        self.recompute_accrued();
    }

    /// Advance the period clock up to `b` without crediting (non-qualifying time), so that a
    /// boundary crossing finalizes the prior period.
    fn advance_period_clock(&mut self, b: i64) {
        let end_period = (b.saturating_sub(1)).div_euclid(self.period_ns);
        if end_period != self.cur_period {
            self.roll_to_period(end_period);
            self.recompute_accrued();
        }
    }

    /// Move bookkeeping into `period`, locking in any completed prior period's reward.
    fn roll_to_period(&mut self, period: i64) {
        if period == self.cur_period {
            return;
        }
        // lock in the prior period's accrual
        self.locked += self.period_reward(self.qualifying_ns_in_period);
        self.cur_period = period;
        self.qualifying_ns_in_period = 0;
    }

    /// Reward earned for `qualifying_ns` of qualifying time within one period (capped).
    fn period_reward(&self, qualifying_ns: i64) -> f64 {
        if self.period_ns <= 0 {
            return 0.0;
        }
        let frac = (qualifying_ns as f64 / self.period_ns as f64).clamp(0.0, 1.0);
        self.reward_per_period * frac
    }

    /// accrued = locked (closed periods) + current partial period.
    fn recompute_accrued(&mut self) {
        self.accrued = self.locked + self.period_reward(self.qualifying_ns_in_period);
    }

    /// Total accrued rewards (dollars) so far.
    #[inline]
    pub fn accrued(&self) -> f64 {
        self.accrued
    }
}

/// Helper: given resting orders' (side, price_cents, remaining) and the mid in cents, compute the
/// in-band [`QuoteState`] the rewards model consumes. `max_spread_cents` is the max distance from
/// mid (in cents) a quote may sit and still qualify.
pub fn quote_state_from_resting<'a, I>(
    resting: I,
    mid_cents: Option<f64>,
    max_spread_cents: i32,
) -> QuoteState
where
    I: IntoIterator<Item = (Side, i32, f64)>,
{
    let mid = match mid_cents {
        Some(m) => m,
        None => return QuoteState::default(),
    };
    let mut bid = 0.0;
    let mut ask = 0.0;
    let band = max_spread_cents.max(0) as f64;
    for (side, price_cents, remaining) in resting {
        if (price_cents as f64 - mid).abs() <= band {
            match side {
                Side::Bid => bid += remaining,
                Side::Ask => ask += remaining,
            }
        }
    }
    QuoteState {
        bid_size_in_band: bid,
        ask_size_in_band: ask,
        has_mid: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, reward: f64, period_secs: i64, both: bool) -> RewardsConfig {
        RewardsConfig {
            enabled,
            period_secs,
            reward_per_period: reward,
            min_resting_size: 10.0,
            max_spread_cents: 4,
            both_sides_required: both,
        }
    }

    fn qualifying() -> QuoteState {
        QuoteState {
            bid_size_in_band: 10.0,
            ask_size_in_band: 10.0,
            has_mid: true,
        }
    }

    fn one_sided() -> QuoteState {
        QuoteState {
            bid_size_in_band: 10.0,
            ask_size_in_band: 0.0,
            has_mid: true,
        }
    }

    const S: i64 = 1_000_000_000;

    #[test]
    fn disabled_accrues_nothing() {
        let mut m = RewardsModel::from_config(&cfg(false, 5.0, 3600, true));
        m.observe(0, &qualifying());
        m.observe(3600 * S, &qualifying());
        assert_eq!(m.accrued(), 0.0);
    }

    #[test]
    fn full_period_qualifying_earns_full_reward() {
        let mut m = RewardsModel::from_config(&cfg(true, 5.0, 3600, true));
        m.observe(0, &qualifying());
        // qualifying the whole hour
        m.observe(3600 * S, &qualifying());
        assert!((m.accrued() - 5.0).abs() < 1e-6, "got {}", m.accrued());
    }

    #[test]
    fn half_period_qualifying_earns_half() {
        let mut m = RewardsModel::from_config(&cfg(true, 4.0, 3600, true));
        m.observe(0, &qualifying());
        m.observe(1800 * S, &qualifying()); // qualifying first half hour
        assert!((m.accrued() - 2.0).abs() < 1e-6, "got {}", m.accrued());
    }

    #[test]
    fn too_wide_one_sided_earns_zero_when_both_required() {
        let mut m = RewardsModel::from_config(&cfg(true, 5.0, 3600, true));
        m.observe(0, &one_sided());
        m.observe(3600 * S, &one_sided());
        assert_eq!(m.accrued(), 0.0);
    }

    #[test]
    fn one_sided_ok_when_both_not_required() {
        let mut m = RewardsModel::from_config(&cfg(true, 5.0, 3600, false));
        m.observe(0, &one_sided());
        m.observe(3600 * S, &one_sided());
        assert!((m.accrued() - 5.0).abs() < 1e-6, "got {}", m.accrued());
    }

    #[test]
    fn no_mid_does_not_qualify() {
        let mut m = RewardsModel::from_config(&cfg(true, 5.0, 3600, true));
        let nomid = QuoteState {
            bid_size_in_band: 100.0,
            ask_size_in_band: 100.0,
            has_mid: false,
        };
        m.observe(0, &nomid);
        m.observe(3600 * S, &nomid);
        assert_eq!(m.accrued(), 0.0);
    }

    #[test]
    fn multiple_periods_accumulate() {
        let mut m = RewardsModel::from_config(&cfg(true, 5.0, 3600, true));
        m.observe(0, &qualifying());
        // qualify two full hours
        m.observe(7200 * S, &qualifying());
        assert!((m.accrued() - 10.0).abs() < 1e-6, "got {}", m.accrued());
    }

    #[test]
    fn period_reward_capped_at_one_period() {
        let mut m = RewardsModel::from_config(&cfg(true, 5.0, 3600, true));
        m.observe(0, &qualifying());
        // one giant qualifying segment spanning 1 hour exactly within a single period
        m.observe(3600 * S, &qualifying());
        // re-observing the same instant must not double-credit
        m.observe(3600 * S, &qualifying());
        assert!((m.accrued() - 5.0).abs() < 1e-6, "got {}", m.accrued());
    }

    #[test]
    fn quote_state_filters_by_band() {
        // mid 50, band 4: a bid at 47 qualifies, an ask at 60 does not
        let resting = vec![(Side::Bid, 47, 10.0), (Side::Ask, 60, 10.0), (Side::Ask, 52, 5.0)];
        let q = quote_state_from_resting(resting, Some(50.0), 4);
        assert_eq!(q.bid_size_in_band, 10.0);
        assert_eq!(q.ask_size_in_band, 5.0); // only the 52 ask is within band
        assert!(q.has_mid);
    }

    #[test]
    fn quote_state_no_mid() {
        let resting = vec![(Side::Bid, 47, 10.0)];
        let q = quote_state_from_resting(resting, None, 4);
        assert!(!q.has_mid);
    }
}
