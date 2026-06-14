//! Performance metrics computed over the equity curve and trade stats.
//!
//! Returns are per-snapshot (consecutive equity-point deltas). Sharpe/Sortino are reported
//! *unannualized* (per-snapshot) for determinism — multiply by sqrt(periods_per_year) if you know
//! your snapshot cadence. All functions are pure over their inputs.

use crate::types::EquityPoint;

/// Per-snapshot simple returns from an equity curve.
pub fn returns(curve: &[EquityPoint]) -> Vec<f64> {
    let mut r = Vec::new();
    for w in curve.windows(2) {
        let prev = w[0].total;
        let cur = w[1].total;
        if prev.abs() > 1e-12 {
            r.push((cur - prev) / prev);
        } else {
            r.push(0.0);
        }
    }
    r
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn std_dev(xs: &[f64], m: f64) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let var = xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() as f64 - 1.0);
    var.sqrt()
}

/// Unannualized Sharpe: mean(returns) / std(returns). Zero if undefined.
pub fn sharpe(curve: &[EquityPoint]) -> f64 {
    let r = returns(curve);
    let m = mean(&r);
    let s = std_dev(&r, m);
    if s > 1e-12 {
        m / s
    } else {
        0.0
    }
}

/// Unannualized Sortino: mean(returns) / downside-std(returns).
pub fn sortino(curve: &[EquityPoint]) -> f64 {
    let r = returns(curve);
    if r.is_empty() {
        return 0.0;
    }
    let m = mean(&r);
    let downside: Vec<f64> = r.iter().copied().filter(|x| *x < 0.0).collect();
    if downside.len() < 1 {
        return 0.0;
    }
    // downside deviation around 0
    let dvar = downside.iter().map(|x| x * x).sum::<f64>() / downside.len() as f64;
    let dstd = dvar.sqrt();
    if dstd > 1e-12 {
        m / dstd
    } else {
        0.0
    }
}

/// Max drawdown in absolute currency and as a fraction of the running peak.
/// Returns `(abs_drawdown, pct_drawdown)` where both are non-negative.
pub fn max_drawdown(curve: &[EquityPoint]) -> (f64, f64) {
    let mut peak = f64::NEG_INFINITY;
    let mut max_abs = 0.0;
    let mut max_pct = 0.0;
    for p in curve {
        if p.total > peak {
            peak = p.total;
        }
        let dd = peak - p.total;
        if dd > max_abs {
            max_abs = dd;
        }
        if peak > 1e-12 {
            let pct = dd / peak;
            if pct > max_pct {
                max_pct = pct;
            }
        }
    }
    (max_abs, max_pct)
}

/// Win rate = fraction of closed round-trip PnLs that are strictly positive.
pub fn win_rate(round_trip_pnls: &[f64]) -> f64 {
    if round_trip_pnls.is_empty() {
        return 0.0;
    }
    let wins = round_trip_pnls.iter().filter(|&&p| p > 0.0).count();
    wins as f64 / round_trip_pnls.len() as f64
}

/// Turnover = total traded notional / starting balance.
pub fn turnover(buy_notional: f64, sell_notional: f64, starting_balance: f64) -> f64 {
    if starting_balance.abs() < 1e-12 {
        0.0
    } else {
        (buy_notional + sell_notional) / starting_balance
    }
}

/// Volatility = sample standard deviation of per-snapshot returns.
pub fn volatility(curve: &[EquityPoint]) -> f64 {
    let r = returns(curve);
    let m = mean(&r);
    std_dev(&r, m)
}

/// Downside volatility = standard deviation of the *negative* per-snapshot returns (around 0).
pub fn downside_volatility(curve: &[EquityPoint]) -> f64 {
    let r = returns(curve);
    let downside: Vec<f64> = r.iter().copied().filter(|x| *x < 0.0).collect();
    if downside.is_empty() {
        return 0.0;
    }
    let dvar = downside.iter().map(|x| x * x).sum::<f64>() / downside.len() as f64;
    dvar.sqrt()
}

/// Calmar ratio = total return (as a fraction) / max drawdown (as a fraction). Zero if undefined.
pub fn calmar_ratio(total_return_pct: f64, max_drawdown_pct: f64) -> f64 {
    if max_drawdown_pct.abs() < 1e-12 {
        0.0
    } else {
        // total_return_pct is a percentage (e.g. 5.0 == 5%); convert to fraction.
        (total_return_pct / 100.0) / max_drawdown_pct
    }
}

/// Exposure = fraction of equity snapshots taken while holding a nonzero position.
pub fn exposure_pct(snapshots_with_position: i64, total_snapshots: i64) -> f64 {
    if total_snapshots <= 0 {
        0.0
    } else {
        snapshots_with_position as f64 / total_snapshots as f64
    }
}

/// Round-trip-level trade statistics derived from realized round-trip PnLs.
#[derive(Debug, Clone, Default)]
pub struct TradeStats {
    pub num_round_trips: i64,
    pub gross_profit: f64,
    pub gross_loss: f64,
    pub profit_factor: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    pub payoff_ratio: f64,
    pub expectancy: f64,
    pub avg_trade_pnl: f64,
    pub largest_win: f64,
    pub largest_loss: f64,
    pub max_consecutive_wins: i64,
    pub max_consecutive_losses: i64,
}

impl TradeStats {
    /// Compute all round-trip stats from a slice of per-round-trip net PnLs.
    pub fn from_pnls(pnls: &[f64]) -> TradeStats {
        let n = pnls.len();
        if n == 0 {
            return TradeStats::default();
        }
        let wins: Vec<f64> = pnls.iter().copied().filter(|&p| p > 0.0).collect();
        let losses: Vec<f64> = pnls.iter().copied().filter(|&p| p < 0.0).collect();

        let gross_profit: f64 = wins.iter().sum();
        let gross_loss: f64 = losses.iter().sum::<f64>().abs();
        let profit_factor = if gross_loss > 1e-12 {
            gross_profit / gross_loss
        } else if gross_profit > 0.0 {
            // no losing trades: treat as infinitely good but report a finite sentinel.
            f64::INFINITY
        } else {
            0.0
        };

        let avg_win = if !wins.is_empty() {
            gross_profit / wins.len() as f64
        } else {
            0.0
        };
        let avg_loss = if !losses.is_empty() {
            // reported as a positive magnitude
            gross_loss / losses.len() as f64
        } else {
            0.0
        };
        // payoff_ratio = avg_win / avg_loss. With NO losing trades the denominator is 0: report
        // `+inf` when there ARE winning trades (a flawless payoff, mirroring `profit_factor`) — the
        // report layer coerces this `+inf` to a large finite sentinel so it stays distinguishable
        // from the genuine no-data 0.0 (no winners at all). See `report::NO_LOSS_SENTINEL`.
        let payoff_ratio = if avg_loss > 1e-12 {
            avg_win / avg_loss
        } else if avg_win > 0.0 {
            f64::INFINITY
        } else {
            0.0
        };

        let win_rate = wins.len() as f64 / n as f64;
        // expectancy per trade = win_rate*avg_win - loss_rate*avg_loss
        let expectancy = win_rate * avg_win - (1.0 - win_rate) * avg_loss;
        let avg_trade_pnl = pnls.iter().sum::<f64>() / n as f64;

        let largest_win = wins.iter().cloned().fold(0.0_f64, f64::max);
        let largest_loss = losses.iter().cloned().fold(0.0_f64, f64::min);

        let (mcw, mcl) = max_consecutive(pnls);

        TradeStats {
            num_round_trips: n as i64,
            gross_profit,
            gross_loss,
            profit_factor,
            avg_win,
            avg_loss,
            payoff_ratio,
            expectancy,
            avg_trade_pnl,
            largest_win,
            largest_loss,
            max_consecutive_wins: mcw,
            max_consecutive_losses: mcl,
        }
    }
}

/// Longest run of consecutive winning and losing round trips. Zero-PnL trades break both streaks.
/// Returns `(max_consecutive_wins, max_consecutive_losses)`.
pub fn max_consecutive(pnls: &[f64]) -> (i64, i64) {
    let mut max_w = 0i64;
    let mut max_l = 0i64;
    let mut cur_w = 0i64;
    let mut cur_l = 0i64;
    for &p in pnls {
        if p > 0.0 {
            cur_w += 1;
            cur_l = 0;
        } else if p < 0.0 {
            cur_l += 1;
            cur_w = 0;
        } else {
            cur_w = 0;
            cur_l = 0;
        }
        max_w = max_w.max(cur_w);
        max_l = max_l.max(cur_l);
    }
    (max_w, max_l)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(t: i64, v: f64) -> EquityPoint {
        EquityPoint {
            ts_ns: t,
            total: v,
            currency: "USD".into(),
        }
    }

    #[test]
    fn drawdown_basic() {
        // 100 -> 120 -> 90 -> 110 : peak 120, trough 90 -> dd abs 30, pct 25%
        let c = vec![pt(0, 100.0), pt(1, 120.0), pt(2, 90.0), pt(3, 110.0)];
        let (abs, pct) = max_drawdown(&c);
        assert!((abs - 30.0).abs() < 1e-9, "abs {abs}");
        assert!((pct - 0.25).abs() < 1e-9, "pct {pct}");
    }

    #[test]
    fn drawdown_monotonic_up_is_zero() {
        let c = vec![pt(0, 100.0), pt(1, 110.0), pt(2, 130.0)];
        let (abs, pct) = max_drawdown(&c);
        assert_eq!(abs, 0.0);
        assert_eq!(pct, 0.0);
    }

    #[test]
    fn sharpe_constant_growth() {
        // steady +10% each step -> zero variance -> sharpe defined as 0 (guarded)
        let c = vec![pt(0, 100.0), pt(1, 110.0), pt(2, 121.0)];
        assert_eq!(sharpe(&c), 0.0);
    }

    #[test]
    fn sharpe_positive_for_noisy_uptrend() {
        let c = vec![pt(0, 100.0), pt(1, 105.0), pt(2, 104.0), pt(3, 110.0)];
        assert!(sharpe(&c) > 0.0);
    }

    #[test]
    fn win_rate_counts() {
        assert!((win_rate(&[1.0, -1.0, 2.0, 0.0]) - 0.5).abs() < 1e-9);
        assert_eq!(win_rate(&[]), 0.0);
    }

    #[test]
    fn turnover_ratio() {
        assert!((turnover(500.0, 500.0, 1000.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn profit_factor_and_gross() {
        // wins 10 + 30 = 40 gross profit; losses -20 -> gross loss 20; pf = 2.0
        let ts = TradeStats::from_pnls(&[10.0, -20.0, 30.0]);
        assert!((ts.gross_profit - 40.0).abs() < 1e-9);
        assert!((ts.gross_loss - 20.0).abs() < 1e-9);
        assert!((ts.profit_factor - 2.0).abs() < 1e-9);
        assert!((ts.avg_win - 20.0).abs() < 1e-9);
        assert!((ts.avg_loss - 20.0).abs() < 1e-9);
        assert!((ts.largest_win - 30.0).abs() < 1e-9);
        assert!((ts.largest_loss + 20.0).abs() < 1e-9);
    }

    #[test]
    fn profit_factor_infinite_when_no_losses() {
        let ts = TradeStats::from_pnls(&[5.0, 7.0]);
        assert!(ts.profit_factor.is_infinite());
        // payoff_ratio is likewise +inf with winners but no losers (report coerces to a sentinel).
        assert!(ts.payoff_ratio.is_infinite());
    }

    #[test]
    fn expectancy_value() {
        // pnls: +10, +10, -10 -> win_rate=2/3, avg_win=10, avg_loss=10
        // expectancy = (2/3)*10 - (1/3)*10 = 10/3
        let ts = TradeStats::from_pnls(&[10.0, 10.0, -10.0]);
        assert!((ts.expectancy - 10.0 / 3.0).abs() < 1e-9, "got {}", ts.expectancy);
        assert!((ts.avg_trade_pnl - 10.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn max_consecutive_runs() {
        // W W L W L L L W -> max wins 2, max losses 3
        let (w, l) = max_consecutive(&[1.0, 1.0, -1.0, 1.0, -1.0, -1.0, -1.0, 1.0]);
        assert_eq!(w, 2);
        assert_eq!(l, 3);
        // zero breaks streaks
        let (w2, l2) = max_consecutive(&[1.0, 0.0, 1.0]);
        assert_eq!(w2, 1);
        assert_eq!(l2, 0);
    }

    #[test]
    fn calmar_basic() {
        // 20% total return, 10% max drawdown -> calmar 2.0
        assert!((calmar_ratio(20.0, 0.10) - 2.0).abs() < 1e-9);
        // undefined drawdown -> 0
        assert_eq!(calmar_ratio(20.0, 0.0), 0.0);
    }

    #[test]
    fn exposure_fraction() {
        assert!((exposure_pct(3, 4) - 0.75).abs() < 1e-9);
        assert_eq!(exposure_pct(0, 0), 0.0);
    }

    #[test]
    fn volatility_nonzero_for_noisy() {
        let c = vec![pt(0, 100.0), pt(1, 110.0), pt(2, 100.0), pt(3, 120.0)];
        assert!(volatility(&c) > 0.0);
        assert!(downside_volatility(&c) > 0.0);
    }
}
