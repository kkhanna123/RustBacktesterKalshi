//! Logistic survival-curve fit for a ladder of "above $K" binary contracts.
//!
//! # The picture (read this first)
//!
//! Kalshi NatGas markets are a **ladder**: one settlement event (e.g.
//! `KXNATGASD-26JUN1517`) carries many binary contracts, each "will the settlement
//! be **above** strike `K`?" — `...-T2.850`, `...-T2.875`, `...-T2.900`, …
//!
//! For a single strike, the market mid price is (under risk-neutral pricing) an
//! estimate of the probability that the event settles above `K`:
//!
//! ```text
//!     mid(K)  ≈  P(settlement > K)  =:  S(K)        ("survival" at K)
//! ```
//!
//! As `K` increases, "settle above `K`" becomes less likely, so `S(K)` is a
//! **decreasing, S-shaped** curve running from ~1 (deep in the money, tiny strike)
//! down to ~0 (far out of the money, huge strike). That is exactly the shape of a
//! survival function of a probability distribution over the settlement price.
//!
//! # The model — a logistic survival
//!
//! We assume the implied settlement price is **logistic** with location `mu` and
//! scale `s > 0`. The survival function of a logistic random variable is
//!
//! ```text
//!     S(K) = 1 / (1 + exp((K - mu) / s))
//! ```
//!
//! Properties we lean on:
//! * `S` is strictly **decreasing** in `K` (because `s > 0`).
//! * `S(mu) = 1/2`. So `mu` is the **median** of the implied settlement
//!   distribution — our *implied fair value*: the strike at which the market is
//!   a coin flip.
//! * `s` sets the **width** of the transition from 1 to 0. A small `s` is a sharp
//!   step (the market is confident); a large `s` is a gentle slope (lots of
//!   uncertainty).
//!
//! ## Implied volatility from the scale
//!
//! The logistic distribution with scale `s` has standard deviation
//!
//! ```text
//!     sd = s · π / √3   ≈   1.8138 · s
//! ```
//!
//! We expose that as the **implied volatility**: a dollar-dispersion of the
//! implied settlement distribution. NOTE: this is a `$`-spread of the settlement
//! price, **not** an annualized Black-Scholes vol. Do not annualize it.
//!
//! # Fitting
//!
//! Given observed points `(K_i, mid_i)` we choose `(mu, s)` to minimise the sum of
//! squared residuals between the model survival and the observed mids:
//!
//! ```text
//!     minimise   Σ_i ( S(K_i) − mid_i )²
//! ```
//!
//! We do this in two stages, both implemented below and individually commented:
//!
//! 1. **Initialise** (`initial_guess`) with a robust, closed-form guess read
//!    straight off the ladder: `mu0` is the strike where the (decreasing) mids
//!    cross 0.5 (linearly interpolated); `s0` comes from how far apart the 25% and
//!    75% crossings are. These are good enough that the refinement almost always
//!    converges in a handful of steps.
//! 2. **Refine** (`fit`) with **Levenberg–Marquardt** (a damped Gauss–Newton).
//!    The residuals and the analytic Jacobian are cheap and exact (see
//!    `partials`), so each step is a tiny 2×2 linear solve.
//!
//! Everything here is pure `f64` arithmetic — no external dependencies — and is
//! written to be read top to bottom.

/// π / √3, the factor converting a logistic `scale` into a standard deviation.
/// Precomputed so `implied_vol` is a single multiply. (`std::f64::consts::PI` over
/// `3f64.sqrt()`.)
const PI_OVER_SQRT3: f64 = std::f64::consts::PI / 1.732_050_807_568_877_2; // √3

/// A fitted logistic survival curve over one event's strike ladder.
///
/// Build it with [`fit`]. All accessors are pure reads of the two parameters
/// `mu`/`s` (plus the recorded fit diagnostics).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogisticFit {
    /// Location = median of the implied settlement distribution = **implied fair
    /// value** (the strike at which `S = 1/2`).
    pub mu: f64,
    /// Scale `> 0`. Larger = a wider, gentler survival curve (more uncertainty).
    pub s: f64,
    /// Number of usable `(K, mid)` points the fit was computed from.
    pub n_points: usize,
    /// Root-mean-square residual of `S(K_i) − mid_i` over those points (lower is a
    /// better fit; this is the fit-quality signal a strategy filters on).
    pub rmse: f64,
    /// True if the Levenberg–Marquardt refinement reached its convergence
    /// tolerance (a tiny parameter step) rather than hitting the iteration cap.
    pub converged: bool,
}

impl LogisticFit {
    /// The model survival `S(K) = 1 / (1 + exp((K − mu) / s))` at strike `K`.
    /// This is the curve's "fair" price for the "above `K`" binary.
    #[inline]
    pub fn fitted(&self, k: f64) -> f64 {
        logistic_survival(k, self.mu, self.s)
    }

    /// Implied fair value = the median `mu` (where `S = 0.5`).
    #[inline]
    pub fn fair_value(&self) -> f64 {
        self.mu
    }

    /// Implied volatility = the logistic standard deviation `s · π / √3`. A
    /// `$`-dispersion of the implied settlement distribution; **not** annualized.
    #[inline]
    pub fn implied_vol(&self) -> f64 {
        self.s * PI_OVER_SQRT3
    }

    /// Pricing edge of the *market* versus the *curve* at strike `K`:
    /// `market_mid − S(K)`.
    ///
    /// * `edge > 0` ⇒ the market mid is **above** the fitted survival ⇒ the binary
    ///   is **RICH** (overpriced) relative to the ladder ⇒ a seller's edge.
    /// * `edge < 0` ⇒ the market is **CHEAP** (underpriced) ⇒ a buyer's edge.
    #[inline]
    pub fn edge(&self, k: f64, market_mid: f64) -> f64 {
        market_mid - self.fitted(k)
    }
}

/// The logistic survival function `1 / (1 + exp((k − mu) / s))`, written so it
/// never overflows: `exp` of a large positive argument is clamped by working with
/// the algebraically-equal `exp`-of-negative form on each branch.
#[inline]
fn logistic_survival(k: f64, mu: f64, s: f64) -> f64 {
    // z = (k − mu) / s. S = 1/(1+e^z). For large +z, e^z overflows; for large −z,
    // it underflows to 0 (fine). Use the numerically-stable sigmoid identity:
    //   z >= 0:  S = 1/(1+e^z)          = e^{-z}/(1+e^{-z})  (no overflow)
    //   z <  0:  S = e^{-z}/(e^{-z}+1)  -> rewrite as 1/(1+e^z) with small e^z
    let z = (k - mu) / s;
    if z >= 0.0 {
        let e = (-z).exp(); // in (0, 1]
        e / (1.0 + e)
    } else {
        let e = z.exp(); // in (0, 1)
        1.0 / (1.0 + e)
    }
}

/// Analytic partial derivatives of `S(K)` with respect to the parameters, at one
/// point. Returns `(S, ∂S/∂mu, ∂S/∂s)`.
///
/// Let `S = S(K)`. The logistic survival satisfies the clean identities
///
/// ```text
///     ∂S/∂mu = S(1 − S) / s
///     ∂S/∂s  = S(1 − S) · (K − mu) / s²
/// ```
///
/// (Both follow from `dS/dz = −S(1−S)` with `z = (K − mu)/s`, then chain rule:
/// `∂z/∂mu = −1/s`, `∂z/∂s = −(K−mu)/s²`, and the two minus signs cancel.)
#[inline]
fn partials(k: f64, mu: f64, s: f64) -> (f64, f64, f64) {
    let surv = logistic_survival(k, mu, s);
    let g = surv * (1.0 - surv); // S(1−S) ≥ 0
    let d_mu = g / s;
    let d_s = g * (k - mu) / (s * s);
    (surv, d_mu, d_s)
}

/// Smallest scale we ever allow. Keeps the curve from collapsing to a vertical step
/// (which would make `1/s` blow up) and guards every division by `s`.
const MIN_SCALE: f64 = 1e-4;

/// Fit a logistic survival `S(K) = 1/(1+exp((K−mu)/s))` to ladder points
/// `(strike K, observed survival = mid)`.
///
/// Contract:
/// * Only points with `mid` strictly in `(0, 1)` are usable (a 0 or 1 mid carries
///   no curvature information and would pin `S` to a boundary). After filtering we
///   require **≥ 4** usable points; otherwise return `None`.
/// * Points are sorted by `K` internally; the caller need not pre-sort.
/// * Returns `None` if the data is too degenerate to yield a sane fit (no usable
///   points, a non-finite guess, etc.). Never panics, never returns NaN/inf in the
///   parameters.
pub fn fit(points: &[(f64, f64)]) -> Option<LogisticFit> {
    // ---- 1. clean + sort ---------------------------------------------------
    // Keep only finite strikes with a mid strictly inside (0, 1).
    let mut pts: Vec<(f64, f64)> = points
        .iter()
        .copied()
        .filter(|&(k, m)| k.is_finite() && m.is_finite() && m > 0.0 && m < 1.0)
        .collect();
    if pts.len() < 4 {
        return None;
    }
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    // ---- 2. robust initial guess off the ladder ----------------------------
    let (mu0, s0) = initial_guess(&pts)?;
    if !mu0.is_finite() || !s0.is_finite() {
        return None;
    }
    let mut mu = mu0;
    let mut s = s0.max(MIN_SCALE);

    // ---- 3. Levenberg–Marquardt refinement ---------------------------------
    // We minimise f(mu, s) = Σ r_i² with r_i = S(K_i) − mid_i. LM solves, each
    // step, the damped normal equations (JᵀJ + λ·diag(JᵀJ)) δ = −Jᵀr, then accepts
    // δ only if it lowers the SSE (otherwise it raises λ toward gradient descent).
    let sse = |mu: f64, s: f64| -> f64 {
        pts.iter()
            .map(|&(k, m)| {
                let r = logistic_survival(k, mu, s) - m;
                r * r
            })
            .sum::<f64>()
    };

    let mut lambda = 1e-3; // LM damping; small = trust Gauss-Newton, large = steepest descent
    let mut cur_sse = sse(mu, s);
    let mut converged = false;

    for _ in 0..25 {
        // Accumulate the 2×2 Gauss–Newton system: A = JᵀJ, g = Jᵀr.
        let (mut a00, mut a01, mut a11) = (0.0f64, 0.0f64, 0.0f64);
        let (mut g0, mut g1) = (0.0f64, 0.0f64);
        for &(k, m) in &pts {
            let (surv, d_mu, d_s) = partials(k, mu, s);
            let r = surv - m;
            a00 += d_mu * d_mu;
            a01 += d_mu * d_s;
            a11 += d_s * d_s;
            g0 += d_mu * r;
            g1 += d_s * r;
        }

        // Try damped solves, increasing λ until we find a step that reduces the SSE
        // (classic LM accept/reject loop). Bounded so we never spin forever.
        let mut stepped = false;
        for _ in 0..12 {
            // Damp the diagonal: (A + λ·diag(A)).
            let m00 = a00 * (1.0 + lambda);
            let m11 = a11 * (1.0 + lambda);
            let m01 = a01;
            let det = m00 * m11 - m01 * m01;
            if det.abs() < 1e-18 {
                // Singular/near-singular normal matrix: bump damping and retry.
                lambda *= 10.0;
                if lambda > 1e12 {
                    break;
                }
                continue;
            }
            // Solve (A_damped) δ = −g for δ = (δmu, δs).
            let d_mu = -(m11 * g0 - m01 * g1) / det;
            let d_s = -(-m01 * g0 + m00 * g1) / det;

            let new_mu = mu + d_mu;
            let new_s = (s + d_s).max(MIN_SCALE); // keep scale strictly positive
            let new_sse = sse(new_mu, new_s);

            if new_sse.is_finite() && new_sse < cur_sse {
                // Accept the step; relax damping toward Gauss–Newton.
                let step_norm = d_mu.abs() + d_s.abs();
                mu = new_mu;
                s = new_s;
                cur_sse = new_sse;
                lambda = (lambda * 0.5).max(1e-9);
                stepped = true;
                // Converged if the parameters barely moved.
                if step_norm < 1e-9 {
                    converged = true;
                }
                break;
            } else {
                // Reject: tighten the trust region (more gradient-descent-like).
                lambda *= 10.0;
                if lambda > 1e12 {
                    break;
                }
            }
        }

        if converged {
            break;
        }
        if !stepped {
            // No λ produced an improving step: we're at a (local) minimum.
            converged = true;
            break;
        }
    }

    // ---- 4. diagnostics + sanity ------------------------------------------
    let rmse = (cur_sse / pts.len() as f64).sqrt();
    if !mu.is_finite() || !s.is_finite() || !rmse.is_finite() || s <= 0.0 {
        return None;
    }
    Some(LogisticFit {
        mu,
        s,
        n_points: pts.len(),
        rmse,
        converged,
    })
}

/// Robust closed-form starting point read straight off the (sorted) ladder.
///
/// * `mu0` = the strike where the decreasing mids cross **0.5** (the median), found
///   by linear interpolation between the two bracketing ladder points. If the mids
///   never cross 0.5 (the whole ladder sits above or below 0.5), we clamp to the
///   nearest end of the strike range — a deliberately conservative guess that the
///   refinement can still move.
/// * `s0` from the inter-quantile width. Inverting `S`, the strike at survival `p`
///   is `K(p) = mu − s·ln(p/(1−p))`. Hence
///   `K(0.25) − K(0.75) = −s·(ln(1/3) − ln(3)) = 2·s·ln 3`, so
///   `s0 ≈ (K@0.25 − K@0.75) / (2·ln 3)`. We locate `K@0.25`/`K@0.75` by the same
///   interpolated-crossing trick. If we can't bracket them (flat/short ladder) we
///   fall back to a fraction of the strike span.
///
/// Returns `None` only if the strike span is degenerate (all strikes equal).
fn initial_guess(pts: &[(f64, f64)]) -> Option<(f64, f64)> {
    let k_lo = pts.first()?.0;
    let k_hi = pts.last()?.0;
    let span = k_hi - k_lo;
    if !(span > 0.0) {
        return None; // all strikes identical — no curve to fit
    }

    // `mu0`: interpolated strike where mid crosses 0.5 (mids are decreasing in K).
    let mu0 = crossing_strike(pts, 0.5).unwrap_or_else(|| {
        // No 0.5 crossing: pick the end nearest 0.5 in mid-space.
        if pts.first().unwrap().1 < 0.5 {
            k_lo // even the smallest strike is already below 0.5
        } else {
            k_hi // even the largest strike is still above 0.5
        }
    });

    // `s0` from the 25%/75% crossing width, when both crossings exist.
    let s0 = match (crossing_strike(pts, 0.25), crossing_strike(pts, 0.75)) {
        (Some(k25), Some(k75)) => {
            // K@0.25 is at a *higher* strike than K@0.75 (mids decrease), so
            // (k25 − k75) > 0. Divide by 2·ln 3 (= ln 9).
            let width = (k25 - k75).abs();
            (width / (2.0 * 3.0f64.ln())).max(MIN_SCALE)
        }
        // Fallback: a modest fraction of the strike span keeps the curve from being
        // a near-vertical step while the refinement takes over.
        _ => (span / 6.0).max(MIN_SCALE),
    };

    Some((mu0, s0))
}

/// Linearly interpolate the strike `K` at which the (assumed decreasing) mids cross
/// the target survival `target`. Scans adjacent ladder points for a bracket where
/// the mid passes through `target` and interpolates inside it. Returns `None` if no
/// bracket contains the crossing (the whole ladder is on one side of `target`).
fn crossing_strike(pts: &[(f64, f64)], target: f64) -> Option<f64> {
    for w in pts.windows(2) {
        let (k0, m0) = w[0];
        let (k1, m1) = w[1];
        // Bracketed iff `target` lies (inclusively) between the two mids. Works for
        // either ordering of m0/m1, though for a clean decreasing ladder m0 ≥ m1.
        let lo = m0.min(m1);
        let hi = m0.max(m1);
        if target >= lo && target <= hi {
            let dm = m1 - m0;
            if dm.abs() < 1e-12 {
                // Flat segment exactly at the target: take its midpoint strike.
                return Some(0.5 * (k0 + k1));
            }
            // Linear interpolation in mid-space: fraction of the way from m0 to target.
            let frac = (target - m0) / dm;
            return Some(k0 + frac * (k1 - k0));
        }
    }
    None
}

/// Parse a Kalshi ladder instrument id into its `(event key, strike)`.
///
/// The id is an optional `"VENUE:"` prefix, then the event key, then a final
/// `"-T<number>"` strike tag. We strip the venue prefix (everything up to and
/// including the first `':'`), then split on the **last** `"-T"` so event keys that
/// themselves contain `-T...` aren't confused for the strike. The part after `-T`
/// must parse as an `f64`, otherwise this isn't a ladder instrument and we return
/// `None`.
///
/// # Examples
/// ```
/// use kalshi_backtester::fit_logistic::parse_event_strike;
/// assert_eq!(
///     parse_event_strike("KXNATGASD-26JUN1517-T3.085"),
///     Some(("KXNATGASD-26JUN1517", 3.085))
/// );
/// // venue-tagged ids work too (the prefix is stripped):
/// assert_eq!(
///     parse_event_strike("KALSHI:KXNATGASD-26JUN1517-T2.850"),
///     Some(("KXNATGASD-26JUN1517", 2.850))
/// );
/// // a non-ladder instrument has no -T<number> tail:
/// assert_eq!(parse_event_strike("KXNATGASD-26JUN1517"), None);
/// ```
pub fn parse_event_strike(instrument: &str) -> Option<(&str, f64)> {
    // Strip an optional "VENUE:" prefix.
    let bare = match instrument.split_once(':') {
        Some((_venue, rest)) => rest,
        None => instrument,
    };
    // Split on the LAST "-T" so the event key keeps any earlier dashes.
    let idx = bare.rfind("-T")?;
    let event = &bare[..idx];
    let strike_str = &bare[idx + 2..]; // skip the "-T"
    let strike: f64 = strike_str.parse().ok()?;
    if event.is_empty() || !strike.is_finite() {
        return None;
    }
    Some((event, strike))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a clean logistic ladder with known `(mu, s)` so the fit has a true
    /// answer to recover.
    fn synth_ladder(mu: f64, s: f64, k_lo: f64, k_hi: f64, n: usize) -> Vec<(f64, f64)> {
        (0..n)
            .map(|i| {
                let k = k_lo + (k_hi - k_lo) * i as f64 / (n - 1) as f64;
                (k, logistic_survival(k, mu, s))
            })
            .collect()
    }

    #[test]
    fn recovers_known_mu_and_s() {
        let (mu, s) = (3.00, 0.075);
        let pts = synth_ladder(mu, s, 2.7, 3.3, 28);
        let f = fit(&pts).expect("fit should succeed on clean logistic data");
        assert!((f.mu - mu).abs() < 1e-3, "mu off: got {}", f.mu);
        assert!((f.s - s).abs() < 1e-3, "s off: got {}", f.s);
        assert!(f.rmse < 1e-6, "rmse too high: {}", f.rmse);
        assert!(f.converged);
        assert_eq!(f.n_points, 28);
    }

    #[test]
    fn recovers_with_mild_noise() {
        // Deterministic pseudo-noise so the test is reproducible.
        let (mu, s) = (3.10, 0.09);
        let mut pts = synth_ladder(mu, s, 2.7, 3.5, 24);
        for (i, p) in pts.iter_mut().enumerate() {
            let n = ((i as f64 * 12.9898).sin() * 43758.5453).fract() - 0.5; // ~U(-0.5,0.5)
            p.1 = (p.1 + 0.01 * n).clamp(1e-3, 1.0 - 1e-3);
        }
        let f = fit(&pts).expect("fit should survive mild noise");
        assert!((f.mu - mu).abs() < 0.03, "mu off under noise: {}", f.mu);
        assert!((f.s - s).abs() < 0.03, "s off under noise: {}", f.s);
    }

    #[test]
    fn implied_vol_formula() {
        let f = LogisticFit {
            mu: 3.0,
            s: 0.075,
            n_points: 10,
            rmse: 0.0,
            converged: true,
        };
        // sd = s * pi/sqrt(3) = 0.075 * 1.8138... ≈ 0.13603
        let expected = 0.075 * std::f64::consts::PI / 3.0f64.sqrt();
        assert!((f.implied_vol() - expected).abs() < 1e-12);
        assert!((f.implied_vol() - 0.13603).abs() < 1e-4);
        assert_eq!(f.fair_value(), 3.0);
    }

    #[test]
    fn edge_sign_is_market_minus_curve() {
        let f = LogisticFit {
            mu: 3.0,
            s: 0.075,
            n_points: 10,
            rmse: 0.0,
            converged: true,
        };
        // At K = mu the fair value is 0.5. A market mid above 0.5 is RICH (edge>0).
        assert!(f.edge(3.0, 0.60) > 0.0);
        // A market mid below 0.5 is CHEAP (edge<0).
        assert!(f.edge(3.0, 0.40) < 0.0);
        // Exactly on the curve ⇒ zero edge.
        assert!(f.edge(3.0, f.fitted(3.0)).abs() < 1e-12);
    }

    #[test]
    fn too_few_points_returns_none() {
        // Only 3 usable points (< 4).
        let pts = vec![(2.9, 0.7), (3.0, 0.5), (3.1, 0.3)];
        assert!(fit(&pts).is_none());
        // Points get filtered to <4 usable when some are at the 0/1 boundary.
        let pts2 = vec![(2.9, 1.0), (3.0, 0.5), (3.1, 0.3), (3.2, 0.0)];
        assert!(fit(&pts2).is_none());
    }

    #[test]
    fn flat_ladder_does_not_panic() {
        // All mids identical (degenerate, no curvature). Must return Some-or-None
        // without panicking / NaN.
        let pts: Vec<(f64, f64)> = (0..10).map(|i| (2.8 + i as f64 * 0.05, 0.5)).collect();
        let r = fit(&pts);
        if let Some(f) = r {
            assert!(f.mu.is_finite() && f.s.is_finite() && f.rmse.is_finite());
        }
    }

    #[test]
    fn parser_basic_and_venue_tagged() {
        assert_eq!(
            parse_event_strike("KXNATGASD-26JUN1517-T3.085"),
            Some(("KXNATGASD-26JUN1517", 3.085))
        );
        assert_eq!(
            parse_event_strike("KALSHI:KXNATGASD-26JUN1517-T2.850"),
            Some(("KXNATGASD-26JUN1517", 2.850))
        );
        // non-ladder: no -T<number>
        assert_eq!(parse_event_strike("KXNATGASD-26JUN1517"), None);
        assert_eq!(parse_event_strike("RANDOM"), None);
        // -T present but not a number
        assert_eq!(parse_event_strike("FOO-Tbar"), None);
    }

    /// Finite-difference check of the analytic Jacobian: ∂S/∂mu and ∂S/∂s from
    /// `partials` must match central differences of `logistic_survival`.
    #[test]
    fn jacobian_matches_finite_differences() {
        let (mu, s) = (3.05, 0.08);
        let h = 1e-6;
        for &k in &[2.75, 2.9, 3.0, 3.05, 3.2, 3.4] {
            let (_, d_mu, d_s) = partials(k, mu, s);
            let fd_mu = (logistic_survival(k, mu + h, s) - logistic_survival(k, mu - h, s))
                / (2.0 * h);
            let fd_s = (logistic_survival(k, mu, s + h) - logistic_survival(k, mu, s - h))
                / (2.0 * h);
            assert!(
                (d_mu - fd_mu).abs() < 1e-5,
                "∂S/∂mu mismatch at k={k}: analytic {d_mu}, fd {fd_mu}"
            );
            assert!(
                (d_s - fd_s).abs() < 1e-5,
                "∂S/∂s mismatch at k={k}: analytic {d_s}, fd {fd_s}"
            );
        }
    }

    #[test]
    fn survival_is_decreasing_and_bounded() {
        let (mu, s) = (3.0, 0.1);
        let mut prev = 1.1;
        for i in 0..50 {
            let k = 2.5 + i as f64 * 0.02;
            let v = logistic_survival(k, mu, s);
            assert!(v > 0.0 && v < 1.0);
            assert!(v < prev, "not decreasing at k={k}");
            prev = v;
        }
        // extreme args don't overflow/NaN
        assert!(logistic_survival(1e6, mu, s).is_finite());
        assert!(logistic_survival(-1e6, mu, s).is_finite());
    }
}
