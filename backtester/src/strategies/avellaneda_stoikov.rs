//! Avellaneda–Stoikov optimal market maker (Avellaneda & Stoikov, 2008), adapted to the Kalshi
//! binary book.
//!
//! # The math
//!
//! The A–S model derives the *optimal* bid/ask quotes for an inventory-averse market maker. Two
//! quantities drive every quote:
//!
//! 1. **Reservation price** — the inventory-adjusted "fair value" the MM is indifferent to:
//!
//!    ```text
//!    r = s - q * gamma * sigma^2 * (T - t)
//!    ```
//!
//!    where `s` = mid, `q` = current signed inventory (+ long YES), `gamma` = risk aversion,
//!    `sigma` = volatility of the mid, and `(T - t)` = normalized time remaining. When the MM is
//!    long (`q > 0`) the reservation price sits *below* the mid, so both quotes shift down and the
//!    ask becomes more aggressive — the book naturally leans to *sell* and bring inventory back to
//!    flat. Short inventory shifts quotes up symmetrically. This is where the inventory skew of a
//!    naive market maker falls out *optimally* rather than being a hand-tuned constant.
//!
//! 2. **Optimal half-spread** — the total bid-ask spread the MM should show:
//!
//!    ```text
//!    delta = 0.5 * gamma * sigma^2 * (T - t) + (1 / gamma) * ln(1 + gamma / kappa)
//!    ```
//!
//!    The first term widens the spread with risk aversion, volatility, and time-at-risk; the second
//!    is the liquidity / order-arrival term, where `kappa` is the order-arrival intensity (a larger
//!    `kappa` = more competitive book = tighter spread).
//!
//! Quotes are then `bid = r - delta`, `ask = r + delta`.
//!
//! # Binary-market adaptation (Kalshi)
//!
//! Kalshi prices are integer cents in `[1, 99]` and the mid is a probability-like value in dollars
//! `[0, 1]`. We compute `r` and `delta` in **dollars**, convert the bid/ask to integer cents
//! (`round(x * 100)`), and clamp to `[1, 99]`. Inventory is capped at `max_inventory` (we stop
//! adding to the heavy side, exactly like the simpler `market_maker`).
//!
//! ## Time-to-horizon `(T - t)` on intraday Kalshi events
//!
//! The textbook model assumes a known terminal time `T` (e.g. end of the trading session). Kalshi
//! event contracts settle at a known expiry, but the backtest does not always know it from the data
//! stream. We therefore use a configurable `horizon_secs`: from the FIRST observed timestamp we let
//! `(T - t)` decay linearly from `1.0` toward `0.0` over `horizon_secs`, then **clamp at a small
//! floor** (it never reaches exactly 0, so the spread never collapses to the pure-liquidity term).
//! If you don't have a meaningful horizon, set `horizon_secs` very large and `(T - t)` stays ~`1.0`
//! — i.e. a constant time-at-risk, which is a sane default for a continuously-quoting MM.

use crate::strategies::toolkit::RollingWindow;
use crate::strategies::{Params, StrategyParams};
use crate::strategy::{Ctx, Strategy};
use crate::types::{Cents, MarketEvent, Side};
use std::collections::HashMap;

/// Per-instrument running state: rolling mid window (for `sigma`) and the first-seen timestamp.
struct InstState {
    mids: RollingWindow,
    first_ns: Option<i64>,
}

/// Avellaneda–Stoikov optimal market maker.
pub struct AvellanedaStoikov {
    /// Risk-aversion `gamma` (>0). Higher = wider spread and stronger inventory skew.
    gamma: f64,
    /// Order-arrival / liquidity intensity `kappa` (>0). Higher = tighter optimal spread.
    kappa: f64,
    /// Rolling window length used to estimate `sigma` (stdev of the mid, in dollars).
    sigma_window: usize,
    /// Horizon in seconds over which `(T - t)` decays from 1 → ~0.
    horizon_secs: f64,
    /// Quote size in contracts per side.
    quote_size: f64,
    /// Max absolute inventory before we stop adding to the heavy side.
    max_inventory: f64,
    state: HashMap<String, InstState>,
}

impl Default for AvellanedaStoikov {
    fn default() -> Self {
        AvellanedaStoikov {
            gamma: 0.1,
            kappa: 1.5,
            sigma_window: 30,
            horizon_secs: 3600.0,
            quote_size: 10.0,
            max_inventory: 50.0,
            state: HashMap::new(),
        }
    }
}

/// The two A–S outputs in dollars: the inventory-adjusted reservation price and the optimal
/// half-spread. Factored out as a pure function so it is directly unit-testable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AsQuote {
    pub reservation: f64,
    pub half_spread: f64,
}

/// Compute the A–S reservation price and optimal half-spread (all in dollars).
///
/// * `mid` (`s`)        — current mid in dollars.
/// * `inventory` (`q`)  — signed position (+ long YES).
/// * `gamma`            — risk aversion (>0).
/// * `kappa`            — order-arrival intensity (>0).
/// * `sigma`            — volatility of the mid (dollars).
/// * `time_left` (`T-t`)— normalized time remaining in `(0, 1]`.
pub fn as_quote(
    mid: f64,
    inventory: f64,
    gamma: f64,
    kappa: f64,
    sigma: f64,
    time_left: f64,
) -> AsQuote {
    let g = gamma.max(1e-9);
    let k = kappa.max(1e-9);
    let var = sigma * sigma;
    // Reservation price: skews AGAINST inventory (long => below mid).
    let reservation = mid - inventory * g * var * time_left;
    // Optimal half-spread: risk term + liquidity term. ln(1 + g/k) > 0 always.
    let half_spread = 0.5 * g * var * time_left + (1.0 / g) * (1.0 + g / k).ln();
    AsQuote {
        reservation,
        half_spread,
    }
}

impl AvellanedaStoikov {
    /// Build from tunable params (keys: `gamma`, `kappa`, `sigma_window`, `horizon_secs`,
    /// `quote_size`, `max_inventory`). An empty map reproduces the defaults documented above.
    pub fn from_params(params: &StrategyParams) -> Self {
        let p = Params::new(params);
        let d = AvellanedaStoikov::default();
        AvellanedaStoikov {
            gamma: p.get("gamma", d.gamma),
            kappa: p.get("kappa", d.kappa),
            sigma_window: p.get_usize("sigma_window", d.sigma_window).max(2),
            horizon_secs: p.get("horizon_secs", d.horizon_secs),
            quote_size: p.get("quote_size", d.quote_size),
            max_inventory: p.get("max_inventory", d.max_inventory),
            state: HashMap::new(),
        }
    }

    /// Normalized time-to-horizon `(T - t)` in `(floor, 1]`, decaying linearly from the first-seen
    /// timestamp over `horizon_secs`. Floored at a small positive value so the spread never collapses
    /// to the pure-liquidity term (and never goes negative).
    fn time_left(&self, first_ns: i64, now_ns: i64) -> f64 {
        if self.horizon_secs <= 0.0 {
            return 1.0;
        }
        let elapsed_secs = (now_ns - first_ns).max(0) as f64 / 1e9;
        (1.0 - elapsed_secs / self.horizon_secs).clamp(0.05, 1.0)
    }

    fn requote(&mut self, inst: &str, ctx: &mut dyn Ctx) {
        let mid = match ctx.mid(inst) {
            Some(m) => m,
            None => return,
        };
        let now = ctx.ts_ns();

        // Update rolling mid window and learn the first-seen timestamp.
        let sigma_window = self.sigma_window;
        let st = self.state.entry(inst.to_string()).or_insert_with(|| InstState {
            mids: RollingWindow::new(sigma_window),
            first_ns: None,
        });
        if st.first_ns.is_none() {
            st.first_ns = Some(now);
        }
        st.mids.push(mid);
        // Volatility estimate: stdev of the mid (dollars). Until the window has >=2 samples, fall
        // back to 0 (then the spread is just the liquidity term and there is no inventory skew).
        let sigma = st.mids.std().unwrap_or(0.0);
        let first_ns = st.first_ns.unwrap_or(now);

        let inventory = ctx.position(inst);
        let time_left = self.time_left(first_ns, now);
        let q = as_quote(mid, inventory, self.gamma, self.kappa, sigma, time_left);

        // Convert reservation ± half-spread (dollars) to integer cents and clamp to [1, 99].
        let bid_c = ((q.reservation - q.half_spread) * 100.0).round() as i32;
        let ask_c = ((q.reservation + q.half_spread) * 100.0).round() as i32;
        let bid = bid_c.clamp(1, 99);
        let mut ask = ask_c.clamp(1, 99);
        // Keep a strictly-positive spread after clamping.
        if ask <= bid {
            ask = (bid + 1).min(99);
        }

        // Cancel/replace on every book move.
        for o in ctx.open_orders(inst) {
            ctx.cancel(o.id);
        }

        // Post bid unless we are already at the long inventory cap.
        if inventory < self.max_inventory {
            ctx.place_limit(inst, Side::Bid, Cents(bid), self.quote_size);
        }
        // Post ask unless we are already at the short inventory cap.
        if ask > bid && inventory > -self.max_inventory {
            ctx.place_limit(inst, Side::Ask, Cents(ask), self.quote_size);
        }
    }
}

impl Strategy for AvellanedaStoikov {
    fn name(&self) -> &str {
        "avellaneda_stoikov"
    }

    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
        // Re-quote on any book update for the instrument that moved (cancel/replace).
        if let MarketEvent::Delta(_) = ev {
            let inst = ev.instrument().to_string();
            self.requote(&inst, ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_skews_against_inventory() {
        let mid = 0.50;
        // Long inventory pushes the reservation price BELOW the mid (lean to sell).
        let long = as_quote(mid, 20.0, 0.1, 1.5, 0.02, 1.0);
        assert!(long.reservation < mid, "long => reservation below mid");
        // Short inventory pushes it ABOVE the mid (lean to buy).
        let short = as_quote(mid, -20.0, 0.1, 1.5, 0.02, 1.0);
        assert!(short.reservation > mid, "short => reservation above mid");
        // Flat => reservation == mid.
        let flat = as_quote(mid, 0.0, 0.1, 1.5, 0.02, 1.0);
        assert!((flat.reservation - mid).abs() < 1e-12);
        // The skew is symmetric around the mid for ±q.
        assert!(((mid - long.reservation) - (short.reservation - mid)).abs() < 1e-12);
    }

    #[test]
    fn spread_widens_with_gamma_and_sigma() {
        // Use a meaningful volatility so the inventory/risk term `0.5*gamma*sigma^2*(T-t)` is the
        // dominant driver — this is the regime where the "wider with gamma/sigma" intuition holds.
        // (At tiny sigma the liquidity term `(1/gamma)*ln(1+gamma/kappa)`, which DECREASES in gamma,
        // dominates; the risk term is what makes the spread widen with risk aversion.)
        let sigma = 1.0;
        let base = as_quote(0.5, 0.0, 1.0, 1.5, sigma, 1.0);
        // Higher risk aversion => wider spread (risk term grows linearly in gamma).
        let hi_gamma = as_quote(0.5, 0.0, 3.0, 1.5, sigma, 1.0);
        assert!(hi_gamma.half_spread > base.half_spread, "higher gamma => wider");
        // Higher volatility => wider spread (risk term grows in sigma^2).
        let hi_sigma = as_quote(0.5, 0.0, 1.0, 1.5, 1.5, 1.0);
        assert!(hi_sigma.half_spread > base.half_spread, "higher sigma => wider");
        // Higher kappa (more liquidity / competition) => tighter spread.
        let hi_kappa = as_quote(0.5, 0.0, 1.0, 5.0, sigma, 1.0);
        assert!(hi_kappa.half_spread < base.half_spread, "higher kappa => tighter");
    }

    #[test]
    fn half_spread_is_always_positive() {
        // Even with zero vol the liquidity term keeps the spread positive.
        let q = as_quote(0.5, 0.0, 0.1, 1.5, 0.0, 1.0);
        assert!(q.half_spread > 0.0);
    }

    #[test]
    fn from_params_overrides_and_defaults() {
        let mut m = StrategyParams::new();
        m.insert("gamma".into(), 0.5);
        m.insert("quote_size".into(), 20.0);
        let s = AvellanedaStoikov::from_params(&m);
        assert_eq!(s.gamma, 0.5);
        assert_eq!(s.quote_size, 20.0);
        assert_eq!(s.kappa, 1.5); // defaulted
        assert_eq!(s.sigma_window, 30); // defaulted
        // Empty params == defaults.
        let d = AvellanedaStoikov::from_params(&StrategyParams::new());
        assert_eq!(d.gamma, 0.1);
        assert_eq!(d.kappa, 1.5);
        assert_eq!(d.max_inventory, 50.0);
    }

    #[test]
    fn time_left_decays_then_floors() {
        let s = AvellanedaStoikov::default();
        // At t0, full time remaining.
        assert!((s.time_left(0, 0) - 1.0).abs() < 1e-9);
        // Half the horizon elapsed => ~0.5.
        let half = (s.horizon_secs / 2.0 * 1e9) as i64;
        assert!((s.time_left(0, half) - 0.5).abs() < 1e-3);
        // Far past the horizon => floored, never 0 or negative.
        let far = (s.horizon_secs * 10.0 * 1e9) as i64;
        let tl = s.time_left(0, far);
        assert!(tl > 0.0 && tl <= 0.05 + 1e-9);
    }
}
