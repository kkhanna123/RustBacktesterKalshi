//! Reusable building blocks for fast strategy prototyping.
//!
//! These are small, well-tested, composition-friendly primitives so a new idea is ~20 lines:
//! a [`RollingWindow`] for rolling stats, an [`Ema`], a [`RollingReturn`], a [`PositionSizer`],
//! [`Signal`] helpers, and a [`BaseStrategy`] that captures the boilerplate every strategy shares
//! (max position, order size, instrument filter) plus convenience methods to flatten and clamp.
//!
//! Design note: we deliberately use **composition, not inheritance**. A strategy *embeds* a
//! [`BaseStrategy`] field and calls its helpers; it does not subclass anything. This keeps the
//! `Strategy` trait tiny and lets each idea opt into exactly the helpers it needs.

use crate::strategy::Ctx;
use crate::types::Side;
use std::collections::VecDeque;

/// A fixed-capacity ring buffer of `f64` with O(1) rolling statistics.
///
/// Push values with [`RollingWindow::push`]; once full, the oldest value is evicted. All stats
/// (mean, std, zscore, min, max) are computed over the values currently held.
#[derive(Debug, Clone)]
pub struct RollingWindow {
    buf: VecDeque<f64>,
    cap: usize,
}

impl RollingWindow {
    /// Create a window holding up to `capacity` values. `capacity` is clamped to be >= 1.
    pub fn new(capacity: usize) -> Self {
        RollingWindow {
            buf: VecDeque::with_capacity(capacity.max(1)),
            cap: capacity.max(1),
        }
    }

    /// Push a value, evicting the oldest if at capacity.
    pub fn push(&mut self, x: f64) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(x);
    }

    /// Number of values currently held.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// True once the window holds `capacity` values.
    pub fn is_full(&self) -> bool {
        self.buf.len() == self.cap
    }

    /// Most-recently pushed value, if any.
    pub fn latest(&self) -> Option<f64> {
        self.buf.back().copied()
    }

    /// Arithmetic mean of held values (None if empty).
    pub fn mean(&self) -> Option<f64> {
        if self.buf.is_empty() {
            return None;
        }
        Some(self.buf.iter().sum::<f64>() / self.buf.len() as f64)
    }

    /// Sample standard deviation (n-1). None if fewer than 2 values.
    pub fn std(&self) -> Option<f64> {
        let n = self.buf.len();
        if n < 2 {
            return None;
        }
        let m = self.mean()?;
        let var = self.buf.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (n as f64 - 1.0);
        Some(var.sqrt())
    }

    /// Z-score of `latest` versus the window mean/std. None if std is undefined or ~0.
    pub fn zscore(&self, latest: f64) -> Option<f64> {
        let m = self.mean()?;
        let s = self.std()?;
        if s < 1e-12 {
            None
        } else {
            Some((latest - m) / s)
        }
    }

    /// Minimum held value (None if empty).
    pub fn min(&self) -> Option<f64> {
        self.buf.iter().cloned().reduce(f64::min)
    }

    /// Maximum held value (None if empty).
    pub fn max(&self) -> Option<f64> {
        self.buf.iter().cloned().reduce(f64::max)
    }
}

/// Exponential moving average. `alpha` in (0,1]; higher = faster reaction.
#[derive(Debug, Clone)]
pub struct Ema {
    alpha: f64,
    value: Option<f64>,
}

impl Ema {
    /// Build from a smoothing factor `alpha` in (0,1].
    pub fn new(alpha: f64) -> Self {
        Ema {
            alpha: alpha.clamp(1e-6, 1.0),
            value: None,
        }
    }

    /// Build from a span/period `n` using the standard `alpha = 2/(n+1)`.
    pub fn from_period(n: usize) -> Self {
        let n = n.max(1) as f64;
        Ema::new(2.0 / (n + 1.0))
    }

    /// Update with a new sample and return the current EMA.
    pub fn update(&mut self, x: f64) -> f64 {
        let next = match self.value {
            Some(v) => self.alpha * x + (1.0 - self.alpha) * v,
            None => x,
        };
        self.value = Some(next);
        next
    }

    /// Current EMA, if at least one sample has been seen.
    pub fn value(&self) -> Option<f64> {
        self.value
    }
}

/// Tracks the simple return between consecutive samples: `(x_t - x_{t-1}) / x_{t-1}`.
#[derive(Debug, Clone, Default)]
pub struct RollingReturn {
    prev: Option<f64>,
}

impl RollingReturn {
    pub fn new() -> Self {
        RollingReturn { prev: None }
    }

    /// Feed a new sample; returns the simple return from the previous sample (None on first).
    pub fn update(&mut self, x: f64) -> Option<f64> {
        let r = match self.prev {
            Some(p) if p.abs() > 1e-12 => Some((x - p) / p),
            _ => None,
        };
        self.prev = Some(x);
        r
    }
}

/// A discrete trading signal a strategy can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Want to be long.
    Long,
    /// Want to be short.
    Short,
    /// Want to be flat.
    Flat,
    /// No opinion this tick (do nothing).
    Hold,
}

impl Signal {
    /// Map a z-score to a contrarian (mean-reversion) signal: very low z = Long, very high = Short.
    pub fn from_zscore_reversion(z: f64, entry: f64, exit: f64) -> Signal {
        if z <= -entry {
            Signal::Long
        } else if z >= entry {
            Signal::Short
        } else if z.abs() <= exit {
            Signal::Flat
        } else {
            Signal::Hold
        }
    }

    /// Map a z-score to a trend-following signal: high z = Long, low z = Short.
    pub fn from_zscore_trend(z: f64, entry: f64) -> Signal {
        if z >= entry {
            Signal::Long
        } else if z <= -entry {
            Signal::Short
        } else {
            Signal::Hold
        }
    }
}

/// How a strategy sizes its orders.
#[derive(Debug, Clone, Copy)]
pub enum PositionSizer {
    /// Always trade a fixed number of contracts.
    Fixed(f64),
    /// Trade a fraction of available cash, sized by `price` (dollars/contract).
    FractionOfCash(f64),
}

impl PositionSizer {
    /// Desired contract quantity given current `cash` and a per-contract `price` in dollars.
    /// Returns a non-negative whole-ish contract count (floored, min 0).
    pub fn qty(&self, cash: f64, price: f64) -> f64 {
        match *self {
            PositionSizer::Fixed(q) => q.max(0.0),
            PositionSizer::FractionOfCash(frac) => {
                if price <= 1e-9 {
                    0.0
                } else {
                    (cash * frac.clamp(0.0, 1.0) / price).floor().max(0.0)
                }
            }
        }
    }
}

/// Common strategy configuration + helpers, embedded by composition into concrete strategies.
///
/// Holds the knobs every directional strategy needs (max absolute position, base order size,
/// optional instrument filter) and provides convenience routines for flattening and clamping
/// an intended position change so it never breaches `max_position`.
#[derive(Debug, Clone)]
pub struct BaseStrategy {
    /// Maximum absolute net position (contracts) the strategy will hold.
    pub max_position: f64,
    /// Base order size in contracts.
    pub order_qty: f64,
    /// Optional instrument filter: if set, only act on instruments matching this prefix glob
    /// (trailing `%`/`*`) or exact id. `None` = act on everything.
    pub instrument_filter: Option<String>,
}

impl Default for BaseStrategy {
    fn default() -> Self {
        BaseStrategy {
            max_position: 50.0,
            order_qty: 10.0,
            instrument_filter: None,
        }
    }
}

impl BaseStrategy {
    pub fn new(max_position: f64, order_qty: f64) -> Self {
        BaseStrategy {
            max_position: max_position.max(0.0),
            order_qty: order_qty.max(0.0),
            instrument_filter: None,
        }
    }

    /// Builder: restrict the strategy to instruments matching `filter` (prefix `%`/`*` or exact).
    pub fn with_filter(mut self, filter: impl Into<String>) -> Self {
        self.instrument_filter = Some(filter.into());
        self
    }

    /// True if this instrument passes the configured filter (always true when no filter set).
    pub fn accepts(&self, instrument: &str) -> bool {
        match &self.instrument_filter {
            None => true,
            Some(pat) => crate::data::glob_match(pat, instrument),
        }
    }

    /// Emit a market order to flatten the current position in `instrument`, if any.
    /// Long positions are sold (Ask); short positions are bought (Bid).
    pub fn desired_flatten(&self, instrument: &str, ctx: &mut dyn Ctx) {
        let pos = ctx.position(instrument);
        if pos > 1e-9 {
            ctx.place_market(instrument, Side::Ask, pos);
        } else if pos < -1e-9 {
            ctx.place_market(instrument, Side::Bid, -pos);
        }
    }

    /// Clamp a desired *additional* signed quantity so the resulting net position stays within
    /// `[-max_position, max_position]`. `current` is the current signed position; `delta` is the
    /// desired signed change. Returns the allowed signed change (may be 0).
    pub fn clamp_to_max_position(&self, current: f64, delta: f64) -> f64 {
        let target = current + delta;
        let clamped = target.clamp(-self.max_position, self.max_position);
        clamped - current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_window_stats() {
        let mut w = RollingWindow::new(3);
        assert!(!w.is_full());
        w.push(1.0);
        w.push(2.0);
        w.push(3.0);
        assert!(w.is_full());
        assert!((w.mean().unwrap() - 2.0).abs() < 1e-9);
        assert!((w.std().unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(w.min(), Some(1.0));
        assert_eq!(w.max(), Some(3.0));
        // eviction: pushing 4 drops 1 -> window {2,3,4}, mean 3
        w.push(4.0);
        assert!((w.mean().unwrap() - 3.0).abs() < 1e-9);
        // zscore of 5 vs {2,3,4}: mean 3, std 1 -> (5-3)/1 = 2
        assert!((w.zscore(5.0).unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn ema_reacts() {
        let mut e = Ema::new(0.5);
        assert_eq!(e.update(10.0), 10.0); // first sample seeds
        assert_eq!(e.update(20.0), 15.0); // 0.5*20 + 0.5*10
    }

    #[test]
    fn rolling_return_works() {
        let mut r = RollingReturn::new();
        assert_eq!(r.update(100.0), None);
        assert!((r.update(110.0).unwrap() - 0.10).abs() < 1e-9);
    }

    #[test]
    fn position_sizer_modes() {
        assert_eq!(PositionSizer::Fixed(10.0).qty(1000.0, 0.5), 10.0);
        // fraction: 50% of 1000 = 500 / 0.5 = 1000 contracts
        assert_eq!(PositionSizer::FractionOfCash(0.5).qty(1000.0, 0.5), 1000.0);
    }

    #[test]
    fn clamp_respects_max() {
        let b = BaseStrategy::new(50.0, 10.0);
        // currently +45, want +10 -> only +5 allowed (cap 50)
        assert!((b.clamp_to_max_position(45.0, 10.0) - 5.0).abs() < 1e-9);
        // currently -45, want -10 -> only -5 allowed
        assert!((b.clamp_to_max_position(-45.0, -10.0) + 5.0).abs() < 1e-9);
        // within bounds -> unchanged
        assert!((b.clamp_to_max_position(0.0, 10.0) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn filter_matches() {
        let b = BaseStrategy::default().with_filter("KX%");
        assert!(b.accepts("KXNATGAS-1"));
        assert!(!b.accepts("OTHER-1"));
        assert!(BaseStrategy::default().accepts("anything"));
    }

    #[test]
    fn signal_mappings() {
        assert_eq!(Signal::from_zscore_reversion(-2.0, 1.0, 0.2), Signal::Long);
        assert_eq!(Signal::from_zscore_reversion(2.0, 1.0, 0.2), Signal::Short);
        assert_eq!(Signal::from_zscore_reversion(0.1, 1.0, 0.2), Signal::Flat);
        assert_eq!(Signal::from_zscore_trend(2.0, 1.0), Signal::Long);
    }
}
