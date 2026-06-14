//! Latency model: delay the *effect* of strategy actions relative to the event that triggered them.
//!
//! Real market-makers don't act instantaneously. An order placed at time `t` only becomes
//! matchable at `t + order_latency (+ market_data_latency) (+ jitter / sampled variance)`; a cancel
//! placed at `t` only removes the order at `t + cancel_latency`. The market-data latency models the
//! strategy reacting to a stale view of the book — we fold it into the order's activation delay (the
//! simplest acceptable model: the strategy decided based on data that was already `md_latency` old,
//! so the effective round-trip is `md_latency + order_latency`).
//!
//! ## Two ways to get the per-order order-latency
//! 1. **Fixed (the default)** — `order_latency = order_latency_ns + DETERMINISTIC hash_jitter(seq)`,
//!    where the jitter is derived purely from the order's monotonic sequence number (NO RNG). This
//!    is byte-for-byte the original model and is what you get when no distribution is configured.
//! 2. **A [`LatencyDist`] (Uniform / Normal / Exponential / Empirical)** — the per-order
//!    order-latency is instead *sampled* from a distribution using a SEEDED PRNG (see below). The
//!    sampled value REPLACES the `order_latency_ns + hash_jitter` term (it is the whole order
//!    latency); `market_data_latency_ns` is still added on top and the total is clamped to ≥ 0.
//!
//! In both cases the activation delay is `order_latency + market_data_latency`, and a cancel uses the
//! flat `cancel_latency_ns` (no distribution).
//!
//! ## Determinism
//! A run is reproducible given **inputs + flags + seed**:
//! * Under the default `Fixed` dist there is NO RNG at all — the per-order jitter is a pure function
//!   of the order's sequence number (a splitmix64 hash), so a run is bit-for-bit reproducible exactly
//!   as it always was (the `seed` field is unused on this path).
//! * Under a non-`Fixed` dist the per-order latency is drawn from a tiny SEEDED PRNG
//!   ([`SplitMix64`]). The PRNG advances once per order, so the sequence of sampled latencies is fully
//!   determined by `seed`: the SAME seed ⇒ an identical run; a DIFFERENT seed ⇒ a different (but
//!   itself reproducible) run. This lets a researcher stress-test whether an edge survives realistic
//!   latency *variance*, not just a fixed delay.
//!
//! When the model is disabled, every delay is zero and `activation_ts == placed_ts`, exactly
//! reproducing the original zero-latency engine behaviour. The engine gates fills on the activation
//! timestamp: a pending order is only eligible to fill once `activation_ts <= now`.

use crate::config::{LatencyConfig, LatencyDist};
use std::cell::Cell;

/// A tiny, dependency-free seeded PRNG: the **splitmix64** generator. ~10 lines, no external crate.
///
/// `splitmix64` is the standard seeder for the xoshiro/xoroshiro family; on its own it is a perfectly
/// good fast PRNG with a 2^64 period. We use it directly here because we only need a deterministic,
/// well-distributed stream of `u64`s from a seed — exactly what `splitmix64` provides.
///
/// The state advances by a fixed odd constant each `next_u64`, then the value is run through an
/// avalanche finalizer. Same seed ⇒ same stream, which is what gives us reproducible-given-seed runs.
#[derive(Debug, Clone, Copy)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Create a PRNG seeded with `seed`. Any `u64` is a valid seed.
    #[inline]
    pub fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    /// Draw the next raw `u64` in the stream, advancing the state.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        // Weyl-sequence increment by the golden-ratio odd constant, then splitmix64 finalizer.
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Draw a uniform `f64` in `[0, 1)` (53 bits of mantissa precision).
    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        // Take the top 53 bits and scale into [0, 1).
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }

    /// Draw a uniform `f64` in `(0, 1]` — strictly positive, so it is safe to take `ln(u)` for the
    /// exponential / Box-Muller transforms (which blow up at 0).
    #[inline]
    pub fn next_f64_open(&mut self) -> f64 {
        // Map [0,1) -> (0,1] by reflecting: 1 - u is in (0, 1].
        1.0 - self.next_f64()
    }
}

/// A latency model derived from a [`LatencyConfig`].
///
/// Holds the flat order/cancel/market-data latencies, the legacy hash-jitter magnitude, the chosen
/// per-order [`LatencyDist`], and (for the non-`Fixed` dists) a seeded [`SplitMix64`] PRNG that
/// advances once per order. The PRNG lives behind a [`Cell`] so sampling can happen through a shared
/// `&self` reference (the engine calls `order_activation_ts(&self, ...)`); this keeps the call-site
/// ergonomics identical to the old deterministic model while still advancing the seeded stream.
#[derive(Debug, Clone)]
pub struct LatencyModel {
    enabled: bool,
    order_latency_ns: i64,
    cancel_latency_ns: i64,
    market_data_latency_ns: i64,
    jitter_ns: i64,
    /// The per-order order-latency distribution. [`LatencyDist::Fixed`] = the legacy hash-jitter path.
    dist: LatencyDist,
    /// Pre-loaded empirical samples (only populated for [`LatencyDist::Empirical`]). Empty when the
    /// file was missing/empty/unreadable, in which case we fall back to the `Fixed` path.
    empirical: Vec<i64>,
    /// Seeded PRNG state, advanced once per sampled order. Behind a `Cell` so we can sample through a
    /// shared `&self`. Never touched on the `Fixed` path (so default runs stay RNG-free + unchanged).
    rng: Cell<SplitMix64>,
}

impl LatencyModel {
    /// Build from config. A disabled config yields a zero-latency (pass-through) model.
    ///
    /// The seeded PRNG is initialized from `cfg.seed`. For [`LatencyDist::Empirical`] the sample file
    /// is loaded ONCE here; a missing/empty/unreadable file warns to STDERR and degrades gracefully to
    /// the `Fixed` distribution (so a bad path never aborts a run).
    pub fn from_config(cfg: &LatencyConfig) -> Self {
        if !cfg.enabled {
            return LatencyModel {
                enabled: false,
                order_latency_ns: 0,
                cancel_latency_ns: 0,
                market_data_latency_ns: 0,
                jitter_ns: 0,
                dist: LatencyDist::Fixed,
                empirical: Vec::new(),
                rng: Cell::new(SplitMix64::new(cfg.seed)),
            };
        }

        // Resolve the distribution, loading empirical samples up-front and degrading to Fixed if the
        // file is unusable.
        let (dist, empirical) = match &cfg.dist {
            LatencyDist::Empirical { path } => match load_empirical_samples(path) {
                Some(samples) if !samples.is_empty() => {
                    (LatencyDist::Empirical { path: path.clone() }, samples)
                }
                _ => {
                    eprintln!(
                        "[kalshi-backtester] WARN: empirical latency file {path} missing/empty/unreadable \
                         — falling back to the Fixed latency distribution"
                    );
                    (LatencyDist::Fixed, Vec::new())
                }
            },
            other => (other.clone(), Vec::new()),
        };

        LatencyModel {
            enabled: true,
            order_latency_ns: cfg.order_latency_ns.max(0),
            cancel_latency_ns: cfg.cancel_latency_ns.max(0),
            market_data_latency_ns: cfg.market_data_latency_ns.max(0),
            jitter_ns: cfg.jitter_ns.max(0),
            dist,
            empirical,
            rng: Cell::new(SplitMix64::new(cfg.seed)),
        }
    }

    /// True if any latency is in effect (otherwise every `*_ts` equals its input).
    #[inline]
    pub fn is_active(&self) -> bool {
        self.enabled
    }

    /// Deterministic pseudo-jitter in `[-jitter_ns, +jitter_ns]` from a sequence number. No RNG:
    /// a cheap integer hash (splitmix64-style) keeps it well-spread yet fully reproducible. This is
    /// the LEGACY jitter used only by the [`LatencyDist::Fixed`] path.
    fn jitter_for(&self, seq: u64) -> i64 {
        if self.jitter_ns <= 0 {
            return 0;
        }
        // splitmix64 finalizer — deterministic avalanche on the sequence number.
        let mut z = seq.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let span = (self.jitter_ns as u64).wrapping_mul(2).wrapping_add(1);
        let r = (z % span) as i64; // 0..=2*jitter
        r - self.jitter_ns // -jitter..=+jitter
    }

    /// Sample ONE per-order **order-latency** (ns) under the configured distribution, advancing the
    /// seeded PRNG by one draw for the non-`Fixed` dists. The result is always clamped to ≥ 0.
    ///
    /// * `Fixed` — `order_latency_ns + hash_jitter(seq)` (NO RNG; the legacy deterministic path).
    /// * `Uniform { min, max }` — uniform in `[min, max]`.
    /// * `Normal { mean, std }` — a Box-Muller normal draw, clamped to ≥ 0.
    /// * `Exponential { mean }` — `-mean * ln(u)` for `u` in `(0,1]` (a heavy-ish tail).
    /// * `Empirical` — a uniformly-chosen sample WITH REPLACEMENT from the loaded file.
    ///
    /// Because this both reads and advances the seeded PRNG, the *order* in which orders are sampled
    /// is part of the determinism contract: same seed + same order stream ⇒ same latencies.
    fn sample_order_latency_ns(&self, seq: u64) -> i64 {
        match &self.dist {
            // Legacy deterministic path: no RNG, pure function of seq.
            LatencyDist::Fixed => (self.order_latency_ns + self.jitter_for(seq)).max(0),

            LatencyDist::Uniform { min_ns, max_ns } => {
                let lo = (*min_ns).min(*max_ns);
                let hi = (*min_ns).max(*max_ns);
                if hi <= lo {
                    return lo.max(0);
                }
                let mut rng = self.rng.get();
                let u = rng.next_f64(); // [0, 1)
                self.rng.set(rng);
                let span = (hi - lo) as f64;
                (lo as f64 + u * span).round().max(0.0) as i64
            }

            LatencyDist::Normal { mean_ns, std_ns } => {
                let mut rng = self.rng.get();
                // Box-Muller: two uniforms -> one standard normal.
                let u1 = rng.next_f64_open(); // (0, 1]
                let u2 = rng.next_f64(); // [0, 1)
                self.rng.set(rng);
                let z = (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos();
                let v = *mean_ns as f64 + z * (*std_ns as f64);
                v.round().max(0.0) as i64
            }

            LatencyDist::Exponential { mean_ns } => {
                let mut rng = self.rng.get();
                let u = rng.next_f64_open(); // (0, 1], avoids ln(0)
                self.rng.set(rng);
                let v = -(*mean_ns as f64) * u.ln();
                v.round().max(0.0) as i64
            }

            LatencyDist::Empirical { .. } => {
                // Should always have samples here (from_config degrades to Fixed otherwise), but guard.
                if self.empirical.is_empty() {
                    return (self.order_latency_ns + self.jitter_for(seq)).max(0);
                }
                let mut rng = self.rng.get();
                let idx = (rng.next_u64() % self.empirical.len() as u64) as usize;
                self.rng.set(rng);
                self.empirical[idx].max(0)
            }
        }
    }

    /// Activation timestamp of an order placed at `placed_ts` with monotonic `seq`. Equal to
    /// `placed_ts` when the model is disabled. Never precedes `placed_ts`.
    ///
    /// The delay is `sample_order_latency_ns(seq) + market_data_latency_ns`, clamped to ≥ 0. Under the
    /// default `Fixed` dist this is exactly `order_latency_ns + market_data_latency_ns + hash_jitter`
    /// — byte-for-byte the original formula.
    pub fn order_activation_ts(&self, placed_ts: i64, seq: u64) -> i64 {
        if !self.enabled {
            return placed_ts;
        }
        let order_lat = self.sample_order_latency_ns(seq);
        let delay = order_lat.saturating_add(self.market_data_latency_ns).max(0);
        placed_ts.saturating_add(delay)
    }

    /// Effective timestamp at which a cancel placed at `placed_ts` removes its order. Equal to
    /// `placed_ts` when disabled. Cancels always use the flat `cancel_latency_ns` (no distribution).
    pub fn cancel_effective_ts(&self, placed_ts: i64) -> i64 {
        if !self.enabled {
            return placed_ts;
        }
        placed_ts.saturating_add(self.cancel_latency_ns.max(0))
    }
}

/// Load a newline/CSV list of latency-ns samples from a file. Each token that parses as an `i64` is a
/// sample; whitespace, commas, and newlines all separate tokens, and lines starting with `#` are
/// treated as comments. Returns `None` only if the file cannot be read (the caller treats an empty
/// `Some(vec)` and `None` identically — both fall back to `Fixed`).
fn load_empirical_samples(path: &str) -> Option<Vec<i64>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        for tok in line.split(|c: char| c == ',' || c.is_whitespace()) {
            let tok = tok.trim();
            if tok.is_empty() {
                continue;
            }
            // Accept integers, and also floats (e.g. "1.5e6") by truncating toward zero.
            if let Ok(v) = tok.parse::<i64>() {
                out.push(v);
            } else if let Ok(f) = tok.parse::<f64>() {
                if f.is_finite() {
                    out.push(f as i64);
                }
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, order: i64, cancel: i64, md: i64, jitter: i64) -> LatencyConfig {
        LatencyConfig {
            enabled,
            order_latency_ns: order,
            cancel_latency_ns: cancel,
            market_data_latency_ns: md,
            jitter_ns: jitter,
            ..Default::default()
        }
    }

    #[test]
    fn disabled_is_pass_through() {
        let m = LatencyModel::from_config(&cfg(false, 999, 999, 999, 999));
        assert!(!m.is_active());
        assert_eq!(m.order_activation_ts(1000, 7), 1000);
        assert_eq!(m.cancel_effective_ts(1000), 1000);
    }

    #[test]
    fn order_latency_adds_delay() {
        let m = LatencyModel::from_config(&cfg(true, 500, 0, 100, 0));
        // base = order(500) + md(100) = 600, no jitter
        assert_eq!(m.order_activation_ts(1_000, 1), 1_600);
    }

    #[test]
    fn cancel_latency_adds_delay() {
        let m = LatencyModel::from_config(&cfg(true, 0, 250, 0, 0));
        assert_eq!(m.cancel_effective_ts(1_000), 1_250);
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        let m = LatencyModel::from_config(&cfg(true, 1000, 0, 0, 100));
        // same seq -> same activation
        let a = m.order_activation_ts(0, 42);
        let b = m.order_activation_ts(0, 42);
        assert_eq!(a, b);
        // bounded within base +/- jitter, and never negative
        for seq in 0..1000u64 {
            let act = m.order_activation_ts(0, seq);
            assert!((900..=1100).contains(&act), "seq {seq} -> {act}");
        }
    }

    #[test]
    fn jitter_varies_across_seqs() {
        let m = LatencyModel::from_config(&cfg(true, 1000, 0, 0, 500));
        let vals: std::collections::HashSet<i64> =
            (0..50u64).map(|s| m.order_activation_ts(0, s)).collect();
        // jitter should produce more than a couple distinct activation times
        assert!(vals.len() > 5, "jitter not varying: {} distinct", vals.len());
    }

    // ---- seeded PRNG ----

    #[test]
    fn splitmix64_same_seed_same_stream_diff_seed_differs() {
        let mut a = SplitMix64::new(1);
        let mut b = SplitMix64::new(1);
        let mut c = SplitMix64::new(2);
        let sa: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
        let sb: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
        let sc: Vec<u64> = (0..16).map(|_| c.next_u64()).collect();
        assert_eq!(sa, sb, "same seed must reproduce the stream");
        assert_ne!(sa, sc, "different seed must differ");
    }

    #[test]
    fn splitmix64_f64_in_unit_interval() {
        let mut r = SplitMix64::new(123);
        for _ in 0..10_000 {
            let u = r.next_f64();
            assert!((0.0..1.0).contains(&u), "f64 out of [0,1): {u}");
            let uo = r.next_f64_open();
            assert!(uo > 0.0 && uo <= 1.0, "f64_open out of (0,1]: {uo}");
        }
    }

    fn cfg_dist(dist: LatencyDist, seed: u64) -> LatencyConfig {
        LatencyConfig {
            enabled: true,
            order_latency_ns: 0,
            cancel_latency_ns: 0,
            market_data_latency_ns: 0,
            jitter_ns: 0,
            dist,
            seed,
        }
    }

    #[test]
    fn same_seed_same_latency_sequence_diff_seed_differs() {
        let dist = LatencyDist::Uniform {
            min_ns: 100,
            max_ns: 1_000_000,
        };
        let m1 = LatencyModel::from_config(&cfg_dist(dist.clone(), 1));
        let m2 = LatencyModel::from_config(&cfg_dist(dist.clone(), 1));
        let m3 = LatencyModel::from_config(&cfg_dist(dist, 2));
        let s1: Vec<i64> = (0..64).map(|i| m1.order_activation_ts(0, i)).collect();
        let s2: Vec<i64> = (0..64).map(|i| m2.order_activation_ts(0, i)).collect();
        let s3: Vec<i64> = (0..64).map(|i| m3.order_activation_ts(0, i)).collect();
        assert_eq!(s1, s2, "same seed -> identical latency sequence");
        assert_ne!(s1, s3, "different seed -> different latency sequence");
    }

    #[test]
    fn uniform_samples_land_in_range() {
        let m = LatencyModel::from_config(&cfg_dist(
            LatencyDist::Uniform {
                min_ns: 1_000,
                max_ns: 5_000,
            },
            7,
        ));
        for i in 0..5_000u64 {
            let act = m.order_activation_ts(0, i); // placed_ts 0, md 0 => activation == sampled latency
            assert!((1_000..=5_000).contains(&act), "uniform out of range: {act}");
        }
    }

    #[test]
    fn normal_is_nonneg_with_right_mean() {
        let mean = 1_000_000i64;
        let std = 200_000i64;
        let m = LatencyModel::from_config(&cfg_dist(
            LatencyDist::Normal {
                mean_ns: mean,
                std_ns: std,
            },
            42,
        ));
        let n = 50_000u64;
        let mut sum = 0i128;
        for i in 0..n {
            let v = m.order_activation_ts(0, i);
            assert!(v >= 0, "normal produced negative latency: {v}");
            sum += v as i128;
        }
        let avg = (sum / n as i128) as i64;
        // With mean >> std and clamping rare, the empirical mean should be close to `mean`.
        let tol = mean / 20; // 5%
        assert!((mean - tol..=mean + tol).contains(&avg), "normal mean off: {avg}");
    }

    #[test]
    fn exponential_is_nonneg_with_right_mean() {
        let mean = 500_000i64;
        let m = LatencyModel::from_config(&cfg_dist(LatencyDist::Exponential { mean_ns: mean }, 9));
        let n = 100_000u64;
        let mut sum = 0i128;
        for i in 0..n {
            let v = m.order_activation_ts(0, i);
            assert!(v >= 0, "exponential produced negative latency: {v}");
            sum += v as i128;
        }
        let avg = (sum / n as i128) as i64;
        // Exponential mean == rate parameter; allow 5% sampling tolerance over many draws.
        let tol = mean / 20;
        assert!((mean - tol..=mean + tol).contains(&avg), "exp mean off: {avg}");
    }

    #[test]
    fn empirical_loads_and_samples_from_file() {
        // Write a temp file of measured latencies (newline + CSV mix, a comment, a float).
        let dir = std::env::temp_dir();
        let path = dir.join(format!("lat_empirical_{}.txt", std::process::id()));
        std::fs::write(&path, "# measured ns\n1000, 2000\n3000\n4000.0\n").unwrap();
        let m = LatencyModel::from_config(&cfg_dist(
            LatencyDist::Empirical {
                path: path.display().to_string(),
            },
            3,
        ));
        let allowed = [1000i64, 2000, 3000, 4000];
        let mut seen = std::collections::HashSet::new();
        for i in 0..2_000u64 {
            let v = m.order_activation_ts(0, i);
            assert!(allowed.contains(&v), "empirical sampled out-of-set value: {v}");
            seen.insert(v);
        }
        // Sampling WITH REPLACEMENT over 2000 draws should hit every one of 4 values.
        assert_eq!(seen.len(), 4, "did not sample all empirical values: {seen:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empirical_missing_file_falls_back_to_fixed() {
        // A non-existent path degrades gracefully to Fixed (here order=1234, no jitter).
        let mut c = cfg_dist(
            LatencyDist::Empirical {
                path: "/definitely/missing/latencies-xyz.txt".to_string(),
            },
            1,
        );
        c.order_latency_ns = 1234;
        let m = LatencyModel::from_config(&c);
        // Fixed path with jitter 0 => exactly order_latency_ns for every order.
        assert_eq!(m.order_activation_ts(0, 0), 1234);
        assert_eq!(m.order_activation_ts(0, 999), 1234);
    }

    #[test]
    fn fixed_default_ignores_seed_and_is_unchanged() {
        // Two models with DIFFERENT seeds but the default Fixed dist must produce identical results
        // (the seed is unused on the Fixed path) — guaranteeing default runs are byte-for-byte stable.
        let mut c1 = cfg(true, 1000, 0, 0, 100);
        c1.seed = 1;
        let mut c2 = cfg(true, 1000, 0, 0, 100);
        c2.seed = 99999;
        let m1 = LatencyModel::from_config(&c1);
        let m2 = LatencyModel::from_config(&c2);
        for seq in 0..200u64 {
            assert_eq!(
                m1.order_activation_ts(0, seq),
                m2.order_activation_ts(0, seq),
                "Fixed dist must not depend on seed (seq {seq})"
            );
        }
    }
}
