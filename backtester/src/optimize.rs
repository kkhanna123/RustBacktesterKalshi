//! Orchestration core shared by the `optimize` and `walk-forward` subcommands.
//!
//! This module factors out the three pieces both orchestration commands need:
//!
//! 1. **Grid expansion** ([`expand_grid`]) — turn a list of named parameter axes
//!    (`half_spread_cents = [1,2,3]`, `quote_size = [5,10]`, ...) into the full cartesian product
//!    of [`Combo`]s, in a STABLE, documented order. The product is capped (see [`MAX_COMBOS`]) with
//!    a clear error so a runaway grid can't silently launch millions of runs.
//!
//! 2. **Parallel, parse-once evaluation** ([`run_grid`]) — given a SINGLE shared `Vec<MarketEvent>`
//!    (the expensive gzip+JSON parse done exactly once, wrapped in an [`Arc`]), run every combo
//!    against the same shared events across a fixed pool of worker threads, and return the
//!    `(combo, Summary)` results in **config order** (not completion order) so the output is
//!    byte-for-byte identical run-to-run regardless of thread scheduling. Each engine run builds its
//!    own fresh state and clones the events it iterates (a `MarketEvent` clone is far cheaper than a
//!    re-parse), so the costly parse never repeats.
//!
//!    Parallelism uses **dependency-free `std::thread`**: a fixed pool of N worker threads pull job
//!    indices off a shared atomic counter inside a [`std::thread::scope`] so they can safely borrow
//!    the `Arc<[MarketEvent]>` and the immutable inputs. No `rayon` (or any new crate) is needed.
//!
//! 3. **The metric selector** ([`Metric`]) — map a metric NAME (`pnl_total`, `sharpe`, ...) to the
//!    corresponding [`Summary`] field and pick the MAXIMUM across combos (all supported metrics are
//!    "higher is better").
//!
//! The `walk-forward` command additionally uses [`split_segments`] to cut the shared events into
//! `K+1` contiguous, time-ordered segments, then reuses [`run_grid`] on a SLICE of the shared events
//! for each fold's training optimization and out-of-sample test.

use crate::config::BacktestConfig;
use crate::engine::Engine;
use crate::strategies::{self, StrategyParams};
use crate::types::{MarketEvent, Summary};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Hard cap on the size of the expanded parameter grid. A cartesian product larger than this is
/// rejected with a clear error rather than launching an unreasonable number of backtests. 5000 keeps
/// even a large sweep tractable (parse-once means each extra combo is just one in-memory engine run).
pub const MAX_COMBOS: usize = 5000;

/// One fully-resolved point in the parameter grid: a concrete assignment of every swept parameter to
/// a single `f64` value, ready to feed to [`strategies::build`] as a [`StrategyParams`] map.
///
/// `Combo` keeps the params in a [`BTreeMap`] so its key order is deterministic (alphabetical),
/// which makes CSV columns and printed tables stable.
#[derive(Debug, Clone, PartialEq)]
pub struct Combo {
    /// `param name -> chosen value` for this grid point.
    pub params: BTreeMap<String, f64>,
}

impl Combo {
    /// View this combo as the engine's [`StrategyParams`] map (they are the same shape).
    pub fn as_strategy_params(&self) -> StrategyParams {
        self.params.clone()
    }

    /// A compact, deterministic `k=v,k2=v2` label (params in alphabetical key order) for tables/logs.
    pub fn label(&self) -> String {
        self.params
            .iter()
            .map(|(k, v)| format!("{k}={}", fmt_num(*v)))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Format a grid value compactly: integers print without a trailing `.0`, others with up to 6
/// significant decimals trimmed of trailing zeros. Keeps labels/CSV tidy (e.g. `2` not `2.0000`).
pub fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.6}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        s.to_string()
    }
}

/// One parameter axis of the grid: a name and the ordered list of values to sweep over it. Built
/// from one `--param 'name=v1,v2,v3'` flag (see [`parse_param_axis`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ParamAxis {
    pub name: String,
    pub values: Vec<f64>,
}

/// Parse one `--param 'name=v1,v2,v3'` flag into a [`ParamAxis`]. The name is a non-empty identifier;
/// each comma-separated value parses as `f64`. Clear errors on a missing `=`, an empty key, an empty
/// value list, or an unparseable number.
pub fn parse_param_axis(raw: &str) -> Result<ParamAxis, String> {
    let (k, v) = raw
        .split_once('=')
        .ok_or_else(|| format!("bad --param '{raw}' (expected name=v1,v2,...)"))?;
    let name = k.trim().to_string();
    if name.is_empty() {
        return Err(format!("bad --param '{raw}' (empty parameter name)"));
    }
    let mut values = Vec::new();
    for part in v.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let val: f64 = part
            .parse()
            .map_err(|_| format!("bad --param '{raw}': value '{part}' is not a number"))?;
        values.push(val);
    }
    if values.is_empty() {
        return Err(format!(
            "bad --param '{raw}' (no values — expected name=v1,v2,...)"
        ));
    }
    Ok(ParamAxis { name, values })
}

/// Expand a list of parameter axes into the full cartesian product of [`Combo`]s.
///
/// ## Order (STABLE, documented)
/// The LAST axis varies fastest (row-major / odometer order). For axes `[a=[1,2], b=[10,20]]` the
/// output is exactly:
/// `{a=1,b=10}, {a=1,b=20}, {a=2,b=10}, {a=2,b=20}`.
/// This order is a pure function of the input axes, so [`run_grid`]'s config-ordered results are
/// fully reproducible.
///
/// ## Errors
/// * No axes => error (nothing to sweep).
/// * Product size > [`MAX_COMBOS`] => error naming the size and the cap.
pub fn expand_grid(axes: &[ParamAxis]) -> Result<Vec<Combo>, String> {
    if axes.is_empty() {
        return Err("no --param axes given (need at least one 'name=v1,v2,...')".to_string());
    }
    // Compute the product size up front so we can reject an oversized grid BEFORE allocating it.
    let mut total: usize = 1;
    for ax in axes {
        total = total
            .checked_mul(ax.values.len())
            .ok_or_else(|| "parameter grid is astronomically large (overflow)".to_string())?;
    }
    if total > MAX_COMBOS {
        return Err(format!(
            "parameter grid has {total} combinations, exceeding the cap of {MAX_COMBOS} — \
             reduce the number of --param values (or axes)"
        ));
    }

    // Odometer expansion: index `i` decodes into one value per axis, last axis fastest.
    let mut combos = Vec::with_capacity(total);
    for i in 0..total {
        let mut rem = i;
        // Build the per-axis selection from the LAST axis (fastest) to the first.
        let mut picks: Vec<(&str, f64)> = Vec::with_capacity(axes.len());
        for ax in axes.iter().rev() {
            let n = ax.values.len();
            let idx = rem % n;
            rem /= n;
            picks.push((ax.name.as_str(), ax.values[idx]));
        }
        // picks is reversed (last axis first); insert into the BTreeMap (key order is alphabetical
        // regardless, so the combo's identity is independent of axis order in the map).
        let mut params = BTreeMap::new();
        for (name, val) in picks {
            params.insert(name.to_string(), val);
        }
        combos.push(Combo { params });
    }
    Ok(combos)
}

/// A ranking metric: the name of the [`Summary`] field to maximize. All supported metrics are
/// "higher is better", so [`run_grid`] / [`best_index`] simply pick the maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    PnlTotal,
    Sharpe,
    Sortino,
    Calmar,
    WinRate,
    ProfitFactor,
    EndingBalance,
    Expectancy,
}

impl Metric {
    /// All metric names accepted on the CLI, in a stable order (used for `--metric` help/validation).
    pub const ALL: &'static [&'static str] = &[
        "pnl_total",
        "sharpe",
        "sortino",
        "calmar_ratio",
        "win_rate",
        "profit_factor",
        "ending_balance",
        "expectancy",
    ];

    /// Parse a CLI metric name (case-insensitive) into a [`Metric`]. A couple of friendly aliases are
    /// accepted (`pnl` => `pnl_total`, `calmar` => `calmar_ratio`).
    pub fn parse(name: &str) -> Result<Metric, String> {
        let n = name.trim().to_ascii_lowercase();
        Ok(match n.as_str() {
            "pnl_total" | "pnl" => Metric::PnlTotal,
            "sharpe" => Metric::Sharpe,
            "sortino" => Metric::Sortino,
            "calmar_ratio" | "calmar" => Metric::Calmar,
            "win_rate" | "winrate" => Metric::WinRate,
            "profit_factor" | "pf" => Metric::ProfitFactor,
            "ending_balance" | "balance" => Metric::EndingBalance,
            "expectancy" => Metric::Expectancy,
            other => {
                return Err(format!(
                    "unknown --metric '{other}' — supported: {}",
                    Metric::ALL.join(", ")
                ))
            }
        })
    }

    /// The canonical CLI name for this metric (the one used in CSV/table headers).
    pub fn name(self) -> &'static str {
        match self {
            Metric::PnlTotal => "pnl_total",
            Metric::Sharpe => "sharpe",
            Metric::Sortino => "sortino",
            Metric::Calmar => "calmar_ratio",
            Metric::WinRate => "win_rate",
            Metric::ProfitFactor => "profit_factor",
            Metric::EndingBalance => "ending_balance",
            Metric::Expectancy => "expectancy",
        }
    }

    /// Extract this metric's value from a finished run's [`Summary`].
    pub fn value(self, s: &Summary) -> f64 {
        match self {
            Metric::PnlTotal => s.pnl_total,
            Metric::Sharpe => s.sharpe,
            Metric::Sortino => s.sortino,
            Metric::Calmar => s.calmar_ratio,
            Metric::WinRate => s.win_rate,
            Metric::ProfitFactor => s.profit_factor,
            Metric::EndingBalance => s.ending_balance,
            Metric::Expectancy => s.expectancy,
        }
    }
}

impl Default for Metric {
    /// The default ranking metric is total PnL.
    fn default() -> Self {
        Metric::PnlTotal
    }
}

/// Pick the index of the BEST (maximum-metric) result from a `[(Combo, Summary)]` slice.
///
/// Ties are broken by the FIRST occurrence (lowest config index), so the choice is deterministic.
/// `NaN` metric values are treated as worse than any real number (never selected unless every value
/// is NaN, in which case index 0 is returned). Returns `None` only for an empty slice.
pub fn best_index(results: &[(Combo, Summary)], metric: Metric) -> Option<usize> {
    if results.is_empty() {
        return None;
    }
    let mut best_i = 0usize;
    let mut best_v = f64::NEG_INFINITY;
    for (i, (_, s)) in results.iter().enumerate() {
        let v = metric.value(s);
        // Strictly-greater keeps the first of equal values (stable tie-break). NaN never wins.
        if v > best_v {
            best_v = v;
            best_i = i;
        }
    }
    Some(best_i)
}

/// Run ONE backtest of `strategy` (configured by `combo`) over `events`, returning its [`Summary`].
///
/// The engine consumes the events by value, so this clones the shared slice into a fresh `Vec` for
/// this run (cheap relative to re-parsing). `base_cfg` carries the execution-realism + risk config
/// and starting balance shared by every run in the sweep; only the strategy params differ per combo.
fn run_one(events: &[MarketEvent], base_cfg: &BacktestConfig, strategy: &str, combo: &Combo) -> Summary {
    let params = combo.as_strategy_params();
    // Unknown strategy is validated by the caller before run_grid, so build() is expected to succeed.
    let mut strat = strategies::build(strategy, &params)
        .unwrap_or_else(|| panic!("unknown strategy '{strategy}' (should be validated before run_grid)"));
    let engine = Engine::new(base_cfg);
    // Clone the shared events for this run (a MarketEvent clone is far cheaper than a re-parse).
    let report = engine.run(events.iter().cloned(), strat.as_mut(), base_cfg);
    report.summary
}

/// Run EVERY combo against the SAME shared `events`, in parallel across `threads` worker threads,
/// returning the `(combo, summary)` results in **config order** (identical to `combos`' order),
/// regardless of which thread finished first.
///
/// ## Parse-once + sharing
/// `events` is an [`Arc`] over a single parsed `Vec<MarketEvent>` (the expensive gzip+JSON parse done
/// ONCE by the caller). Every worker borrows the same `Arc` inside a [`std::thread::scope`] and only
/// CLONES the events it iterates — the parse never repeats, which is the whole point of the design.
///
/// ## Determinism
/// Results are written into a pre-sized `Vec<Option<Summary>>` at each combo's ORIGINAL index, then
/// zipped back with `combos` in order. So the returned vector is a pure function of
/// `(events, base_cfg, strategy, combos, metric-independent)` — running with 1 thread or N threads
/// yields byte-for-byte identical output (see the `parallel_equals_serial` test). Each individual
/// backtest is itself deterministic.
///
/// `threads` is clamped to `[1, combos.len()]` (0 is treated as 1); there is no point spawning more
/// workers than there are jobs.
pub fn run_grid(
    events: &Arc<Vec<MarketEvent>>,
    base_cfg: &BacktestConfig,
    strategy: &str,
    combos: &[Combo],
    threads: usize,
) -> Vec<(Combo, Summary)> {
    let n = combos.len();
    if n == 0 {
        return Vec::new();
    }
    let n_threads = threads.max(1).min(n);

    // Pre-sized output slots, one per combo index. Each slot is filled exactly once by the worker
    // that runs that combo, so we can collect results in CONFIG ORDER (not completion order).
    let mut slots: Vec<Option<Summary>> = (0..n).map(|_| None).collect();

    // A single shared atomic cursor hands out the next job index to whichever worker asks first —
    // a simple, dependency-free work-stealing queue with perfect load balancing.
    let next = AtomicUsize::new(0);

    // Borrow the events Arc and inputs into scoped threads (no 'static bound needed thanks to scope).
    std::thread::scope(|scope| {
        // Each worker gets a disjoint set of output slots via raw pointers into `slots`. This is safe
        // because every index is claimed by EXACTLY ONE worker (the atomic cursor guarantees no two
        // workers ever take the same index), so there is no aliasing of the same slot.
        let slots_ptr = SlotsPtr(slots.as_mut_ptr());
        let next_ref = &next;
        let events_ref: &Arc<Vec<MarketEvent>> = events;

        let mut handles = Vec::with_capacity(n_threads);
        for _ in 0..n_threads {
            let handle = scope.spawn(move || {
                let slots_ptr = slots_ptr; // move the Copy wrapper into the closure
                loop {
                    let i = next_ref.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let summary = run_one(events_ref, base_cfg, strategy, &combos[i]);
                    // SAFETY: index `i` is claimed by exactly one worker, so this write does not race.
                    unsafe {
                        *slots_ptr.0.add(i) = Some(summary);
                    }
                }
            });
            handles.push(handle);
        }
        for h in handles {
            let _ = h.join();
        }
    });

    // Zip the now-filled slots back with their combos, in original config order.
    combos
        .iter()
        .cloned()
        .zip(slots)
        .map(|(c, s)| (c, s.expect("every combo slot is filled by exactly one worker")))
        .collect()
}

/// A `Send`-able, `Copy` wrapper over a raw `*mut Summary` so worker closures can write into disjoint
/// slots of the shared output `Vec` without a `Mutex`. Each index is claimed by exactly one worker
/// (via the atomic cursor), so the writes never alias — see the SAFETY note at the write site.
#[derive(Clone, Copy)]
struct SlotsPtr(*mut Option<Summary>);
// SAFETY: the pointer is only ever dereferenced at indices uniquely owned by a single worker, so no
// two threads touch the same slot; sharing the base pointer across threads is therefore sound.
unsafe impl Send for SlotsPtr {}
unsafe impl Sync for SlotsPtr {}

/// Split a slice of (time-ordered) events into `n` contiguous segments of (roughly) equal event
/// count, preserving order. Returns `n` index ranges `[start, end)` into `events` that exactly tile
/// `0..events.len()` with no gaps or overlaps.
///
/// Used by `walk-forward` to cut the shared events into `K+1` folds. When `events.len()` isn't
/// divisible by `n`, the first `len % n` segments get one extra event (so sizes differ by at most 1).
/// `n` is clamped to `>= 1`; segments past the available events are empty ranges at the end.
pub fn split_segments(len: usize, n: usize) -> Vec<(usize, usize)> {
    let n = n.max(1);
    let base = len / n;
    let extra = len % n;
    let mut ranges = Vec::with_capacity(n);
    let mut start = 0usize;
    for i in 0..n {
        // The first `extra` segments are one event longer to absorb the remainder.
        let size = base + if i < extra { 1 } else { 0 };
        let end = (start + size).min(len);
        ranges.push((start, end));
        start = end;
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Action, BookDelta, Cents, Side, TradeEvent};

    /// A small synthetic event stream that drives the market_maker / momentum strategies to actually
    /// trade, so different params produce different summaries. No external data needed.
    fn synthetic_stream() -> Vec<MarketEvent> {
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
        for (i, px) in [47, 49, 51, 53, 51, 49, 47, 50].iter().enumerate() {
            let ts = 2_000 + i as i64 * 1_000;
            v.push(d(ts, Side::Bid, px - 1, 100.0, 10 + i as i64 * 2, false));
            v.push(d(ts + 1, Side::Ask, px + 1, 100.0, 11 + i as i64 * 2, false));
            v.push(t(ts + 2, *px, 5.0));
        }
        v
    }

    // ---- (a) grid expansion: cartesian product correct + order stable ----

    #[test]
    fn grid_expansion_cartesian_and_order_stable() {
        let axes = vec![
            ParamAxis { name: "a".into(), values: vec![1.0, 2.0] },
            ParamAxis { name: "b".into(), values: vec![10.0, 20.0] },
        ];
        let combos = expand_grid(&axes).unwrap();
        // 2 x 2 = 4 combos, last axis (b) varies fastest.
        assert_eq!(combos.len(), 4);
        let labels: Vec<String> = combos.iter().map(|c| c.label()).collect();
        assert_eq!(labels, vec!["a=1,b=10", "a=1,b=20", "a=2,b=10", "a=2,b=20"]);
        // Re-expanding the same axes yields the identical order (pure function).
        let again = expand_grid(&axes).unwrap();
        assert_eq!(combos, again);
    }

    #[test]
    fn grid_expansion_three_axes_size_and_first_last() {
        let axes = vec![
            ParamAxis { name: "x".into(), values: vec![1.0, 2.0, 3.0] },
            ParamAxis { name: "y".into(), values: vec![1.0, 2.0] },
            ParamAxis { name: "z".into(), values: vec![7.0, 8.0, 9.0, 10.0] },
        ];
        let combos = expand_grid(&axes).unwrap();
        assert_eq!(combos.len(), 3 * 2 * 4);
        assert_eq!(combos.first().unwrap().label(), "x=1,y=1,z=7");
        assert_eq!(combos.last().unwrap().label(), "x=3,y=2,z=10");
    }

    // ---- (e) the product-cap error fires ----

    #[test]
    fn grid_expansion_caps_oversized_product() {
        // 10 axes of 4 values each = 4^10 ≈ 1.05M >> MAX_COMBOS.
        let axes: Vec<ParamAxis> = (0..10)
            .map(|i| ParamAxis {
                name: format!("p{i}"),
                values: vec![1.0, 2.0, 3.0, 4.0],
            })
            .collect();
        let err = expand_grid(&axes).unwrap_err();
        assert!(err.contains("exceeding the cap") || err.contains("combinations"), "got: {err}");
    }

    #[test]
    fn grid_expansion_rejects_empty_axes() {
        assert!(expand_grid(&[]).is_err());
    }

    #[test]
    fn parse_param_axis_parses_and_rejects() {
        let ax = parse_param_axis("half_spread_cents=1,2,3").unwrap();
        assert_eq!(ax.name, "half_spread_cents");
        assert_eq!(ax.values, vec![1.0, 2.0, 3.0]);
        // whitespace tolerated
        let ax = parse_param_axis(" size = 5 , 10 , 20 ").unwrap();
        assert_eq!(ax.name, "size");
        assert_eq!(ax.values, vec![5.0, 10.0, 20.0]);
        // missing '='
        assert!(parse_param_axis("no_equals").is_err());
        // empty value list
        assert!(parse_param_axis("k=").is_err());
        // non-numeric
        assert!(parse_param_axis("k=1,foo,3").is_err());
        // empty name
        assert!(parse_param_axis("=1,2").is_err());
    }

    // ---- (b) metric selector: names -> Summary fields, picks the max ----

    #[test]
    fn metric_parse_and_value_and_best_picks_max() {
        assert_eq!(Metric::parse("pnl_total").unwrap(), Metric::PnlTotal);
        assert_eq!(Metric::parse("PnL").unwrap(), Metric::PnlTotal);
        assert_eq!(Metric::parse("sharpe").unwrap(), Metric::Sharpe);
        assert_eq!(Metric::parse("calmar").unwrap(), Metric::Calmar);
        assert!(Metric::parse("not_a_metric").is_err());

        // Build three fake summaries differing only in pnl_total + sharpe; check selection.
        let mk = |pnl: f64, sharpe: f64| {
            let mut s = zero_summary();
            s.pnl_total = pnl;
            s.sharpe = sharpe;
            s
        };
        let results = vec![
            (combo_of(&[("a", 1.0)]), mk(10.0, 0.1)),
            (combo_of(&[("a", 2.0)]), mk(30.0, 0.9)), // best by both here
            (combo_of(&[("a", 3.0)]), mk(20.0, 0.5)),
        ];
        assert_eq!(best_index(&results, Metric::PnlTotal), Some(1));
        assert_eq!(best_index(&results, Metric::Sharpe), Some(1));

        // A tie on pnl: the FIRST (lowest index) wins (stable tie-break).
        let tie = vec![
            (combo_of(&[("a", 1.0)]), mk(42.0, 0.0)),
            (combo_of(&[("a", 2.0)]), mk(42.0, 0.0)),
        ];
        assert_eq!(best_index(&tie, Metric::PnlTotal), Some(0));
        let empty: Vec<(Combo, Summary)> = Vec::new();
        assert_eq!(best_index(&empty, Metric::PnlTotal), None);
    }

    // ---- (c) run_grid is deterministic + parallel == serial ----

    #[test]
    fn run_grid_deterministic_same_input_same_output() {
        let events = Arc::new(synthetic_stream());
        let cfg = BacktestConfig::default();
        let combos = expand_grid(&[
            ParamAxis { name: "half_spread_cents".into(), values: vec![1.0, 2.0, 3.0] },
            ParamAxis { name: "quote_size".into(), values: vec![5.0, 10.0] },
        ])
        .unwrap();

        let r1 = run_grid(&events, &cfg, "market_maker", &combos, 4);
        let r2 = run_grid(&events, &cfg, "market_maker", &combos, 4);
        // Same combos in same order, identical summaries (compare via JSON for a full field check).
        assert_eq!(r1.len(), combos.len());
        for ((c1, s1), (c2, s2)) in r1.iter().zip(r2.iter()) {
            assert_eq!(c1, c2);
            assert_eq!(
                serde_json::to_string(s1).unwrap(),
                serde_json::to_string(s2).unwrap()
            );
        }
    }

    #[test]
    fn run_grid_parallel_equals_serial() {
        let events = Arc::new(synthetic_stream());
        let cfg = BacktestConfig::default();
        let combos = expand_grid(&[
            ParamAxis { name: "half_spread_cents".into(), values: vec![1.0, 2.0, 3.0, 4.0] },
            ParamAxis { name: "quote_size".into(), values: vec![5.0, 10.0, 20.0] },
        ])
        .unwrap();

        let serial = run_grid(&events, &cfg, "market_maker", &combos, 1);
        let parallel = run_grid(&events, &cfg, "market_maker", &combos, 8);
        assert_eq!(serial.len(), parallel.len());
        for ((cs, ss), (cp, sp)) in serial.iter().zip(parallel.iter()) {
            assert_eq!(cs, cp, "combo order must match between serial and parallel");
            assert_eq!(
                serde_json::to_string(ss).unwrap(),
                serde_json::to_string(sp).unwrap(),
                "summary must be identical serial vs parallel for combo {}",
                cs.label()
            );
        }
    }

    // ---- (d) walk-forward split: K folds covering the timeline ----

    #[test]
    fn split_segments_tiles_the_timeline() {
        // 10 events into 3 segments => sizes 4,3,3 (first absorbs the remainder), covering 0..10.
        let ranges = split_segments(10, 3);
        assert_eq!(ranges, vec![(0, 4), (4, 7), (7, 10)]);
        // contiguous, no gaps/overlaps, covers everything.
        assert_eq!(ranges.first().unwrap().0, 0);
        assert_eq!(ranges.last().unwrap().1, 10);
        for w in ranges.windows(2) {
            assert_eq!(w[0].1, w[1].0);
        }
    }

    #[test]
    fn split_segments_k_plus_one_folds_for_walk_forward() {
        // walk-forward with K=3 windows splits into K+1 = 4 segments.
        let len = 100;
        let k = 3;
        let ranges = split_segments(len, k + 1);
        assert_eq!(ranges.len(), 4);
        // Every segment non-empty for a comfortably-sized stream, and they tile 0..len.
        assert_eq!(ranges.first().unwrap().0, 0);
        assert_eq!(ranges.last().unwrap().1, len);
        let covered: usize = ranges.iter().map(|(a, b)| b - a).sum();
        assert_eq!(covered, len);
        for (a, b) in &ranges {
            assert!(b > a, "segment {a}..{b} should be non-empty");
        }
    }

    #[test]
    fn split_segments_handles_indivisible_and_small() {
        // 7 into 4 => 2,2,2,1.
        assert_eq!(split_segments(7, 4), vec![(0, 2), (2, 4), (4, 6), (6, 7)]);
        // fewer events than segments => trailing empty ranges, still tiling.
        let r = split_segments(2, 4);
        assert_eq!(r, vec![(0, 1), (1, 2), (2, 2), (2, 2)]);
    }

    // ---- test helpers ----

    fn combo_of(kvs: &[(&str, f64)]) -> Combo {
        let mut params = BTreeMap::new();
        for (k, v) in kvs {
            params.insert((*k).to_string(), *v);
        }
        Combo { params }
    }

    /// A `Summary` with every field zeroed, for metric-selection tests.
    fn zero_summary() -> Summary {
        Summary {
            currency: "USD".into(),
            starting_balance: 1000.0,
            ending_balance: 1000.0,
            pnl_total: 0.0,
            pnl_pct: 0.0,
            total_orders: 0,
            total_positions: 0,
            avg_buy_price: 0.0,
            avg_sell_price: 0.0,
            num_trades: 0,
            num_fills: 0,
            win_rate: 0.0,
            sharpe: 0.0,
            sortino: 0.0,
            max_drawdown: 0.0,
            max_drawdown_pct: 0.0,
            turnover: 0.0,
            total_fees: 0.0,
            profit_factor: 0.0,
            gross_profit: 0.0,
            gross_loss: 0.0,
            avg_win: 0.0,
            avg_loss: 0.0,
            payoff_ratio: 0.0,
            expectancy: 0.0,
            num_round_trips: 0,
            avg_trade_pnl: 0.0,
            largest_win: 0.0,
            largest_loss: 0.0,
            max_consecutive_wins: 0,
            max_consecutive_losses: 0,
            calmar_ratio: 0.0,
            volatility: 0.0,
            downside_volatility: 0.0,
            exposure_pct: 0.0,
            avg_holding_secs: 0.0,
            fees_pct_of_gross: 0.0,
            total_volume_contracts: 0.0,
            total_slippage_cost: 0.0,
            liquidity_rewards: 0.0,
            gross_pnl_ex_costs: 0.0,
            settled_pnl: 0.0,
            num_settled: 0,
            halted: false,
            halt_reason: String::new(),
            risk_rejections: 0,
            post_only_rejects: 0,
        }
    }
}
