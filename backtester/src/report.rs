//! Build the infra-orchestrator-compatible `Report`, print it between sentinels, and render a
//! standalone HTML tearsheet (inline SVG, no external assets).

use crate::config::BacktestConfig;
use crate::metrics;
use crate::portfolio::Portfolio;
use crate::types::{
    EquityPoint, InstrumentBreakdown, Report, Summary, REPORT_JSON_END, REPORT_JSON_START,
    TEARSHEET_B64_END, TEARSHEET_B64_START,
};
use std::io::Write;
use std::path::Path;

/// Large finite sentinel reported for `profit_factor` / `payoff_ratio` when a strategy has winners
/// but NO losing trades (the true ratio is `+inf`, which JSON cannot represent). Chosen far outside
/// any realistic ratio so a flawless strategy is unambiguously distinguishable from a no-winners
/// strategy (which reports `0.0`). See the `finite` coercion in [`build_report`].
pub const NO_LOSS_SENTINEL: f64 = 1e9;

/// Assemble the `Report` from final portfolio state + config.
pub fn build_report(plugin_name: &str, pf: &Portfolio, cfg: &BacktestConfig) -> Report {
    let curve = pf.equity_curve.clone();
    let ending = curve.last().map(|p| p.total).unwrap_or(cfg.starting_balance);
    let pnl_total = ending - cfg.starting_balance;
    let pnl_pct = if cfg.starting_balance.abs() > 1e-12 {
        pnl_total / cfg.starting_balance * 100.0
    } else {
        0.0
    };
    let (max_dd, max_dd_pct) = metrics::max_drawdown(&curve);
    let total_positions = pf
        .positions
        .values()
        .filter(|p| p.net_qty.abs() > 1e-12 || p.realized_pnl != 0.0)
        .count() as i64;

    // round-trip / trade analytics
    let ts = metrics::TradeStats::from_pnls(&pf.round_trip_pnls);
    // `profit_factor` (and `payoff_ratio`) are `+inf` for a FLAWLESS strategy with winners but NO
    // losers. JSON has no infinity, so serde would emit `null`. We coerce a NON-FINITE value to a
    // clear LARGE FINITE sentinel (`NO_LOSS_SENTINEL`, 1e9) — NOT `0.0`, because `0.0` is also the
    // value for a strategy with NO winners, making "flawless" and "all-losing" indistinguishable.
    // 1e9 is far outside any realistic ratio, so consumers can detect the no-loss case unambiguously
    // while keeping a finite, contract-friendly number. (`NaN` — neither winners nor losers — maps
    // to 0.0, the natural "no data" value.)
    let finite = |x: f64| {
        if x.is_finite() {
            x
        } else if x == f64::INFINITY {
            NO_LOSS_SENTINEL
        } else {
            0.0 // -inf or NaN: degenerate; report 0.0
        }
    };

    let total_snapshots = curve.len() as i64;
    let fees_pct_of_gross = if ts.gross_profit > 1e-12 {
        pf.total_fees / ts.gross_profit
    } else {
        0.0
    };

    // ---- execution-cost decomposition ----
    // `pnl_total` is ending - starting and already reflects: (a) fee debits (only if include_fees),
    // (b) slippage debits, and (c) the rewards credit (only if include_rewards). Reconstruct the
    // pure price-movement PnL by adding back the costs and removing any credited rewards.
    let credited_rewards = if cfg.execution.include_rewards {
        pf.liquidity_rewards
    } else {
        0.0
    };
    let gross_pnl_ex_costs =
        pnl_total - credited_rewards + pf.total_fees + pf.total_slippage_cost;

    let summary = Summary {
        currency: cfg.currency.clone(),
        starting_balance: cfg.starting_balance,
        ending_balance: ending,
        pnl_total,
        pnl_pct,
        total_orders: pf.total_orders,
        total_positions,
        avg_buy_price: pf.avg_buy_price(),
        avg_sell_price: pf.avg_sell_price(),
        num_trades: pf.round_trip_pnls.len() as i64,
        num_fills: pf.total_fills,
        win_rate: metrics::win_rate(&pf.round_trip_pnls),
        sharpe: metrics::sharpe(&curve),
        sortino: metrics::sortino(&curve),
        max_drawdown: max_dd,
        max_drawdown_pct: max_dd_pct,
        turnover: metrics::turnover(pf.buy_notional, pf.sell_notional, cfg.starting_balance),
        total_fees: pf.total_fees,
        // round-trip / trade analytics
        profit_factor: finite(ts.profit_factor),
        gross_profit: ts.gross_profit,
        gross_loss: ts.gross_loss,
        avg_win: ts.avg_win,
        avg_loss: ts.avg_loss,
        payoff_ratio: finite(ts.payoff_ratio),
        expectancy: ts.expectancy,
        num_round_trips: ts.num_round_trips,
        avg_trade_pnl: ts.avg_trade_pnl,
        largest_win: ts.largest_win,
        largest_loss: ts.largest_loss,
        max_consecutive_wins: ts.max_consecutive_wins,
        max_consecutive_losses: ts.max_consecutive_losses,
        // risk / return analytics
        calmar_ratio: metrics::calmar_ratio(pnl_pct, max_dd_pct),
        volatility: metrics::volatility(&curve),
        downside_volatility: metrics::downside_volatility(&curve),
        exposure_pct: metrics::exposure_pct(pf.snapshots_with_position, total_snapshots),
        avg_holding_secs: pf.avg_holding_secs(),
        fees_pct_of_gross,
        total_volume_contracts: pf.total_volume,
        // execution-cost decomposition
        total_slippage_cost: pf.total_slippage_cost,
        liquidity_rewards: pf.liquidity_rewards,
        gross_pnl_ex_costs,
        // binary settlement-at-expiry
        settled_pnl: pf.settled_pnl,
        num_settled: pf.num_settled,
        // risk-layer status
        halted: pf.halted,
        halt_reason: pf.halt_reason.clone(),
        risk_rejections: pf.risk_rejections,
        post_only_rejects: pf.post_only_rejects,
    };

    // per-instrument breakdown, sorted by instrument id for deterministic output
    let mut instrument_breakdown: Vec<InstrumentBreakdown> = pf
        .instrument_stats
        .iter()
        .map(|(inst, st)| InstrumentBreakdown {
            instrument: inst.clone(),
            pnl: st.pnl,
            num_fills: st.num_fills,
            num_round_trips: st.num_round_trips,
            net_position: st.net_position,
            volume: st.volume,
        })
        .collect();
    instrument_breakdown.sort_by(|a, b| a.instrument.cmp(&b.instrument));

    Report {
        plugin_name: plugin_name.to_string(),
        summary,
        equity_curve: curve,
        instrument_breakdown,
    }
}

/// Print the report JSON between the infra sentinels to stdout.
pub fn print_report(report: &Report) {
    let json = serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_string());
    println!("{REPORT_JSON_START}");
    println!("{json}");
    println!("{REPORT_JSON_END}");
}

/// Print the base64 of the tearsheet HTML between its sentinels.
pub fn print_tearsheet_b64(html: &str) {
    println!("{TEARSHEET_B64_START}");
    println!("{}", base64_encode(html.as_bytes()));
    println!("{TEARSHEET_B64_END}");
}

/// Render a standalone self-contained HTML tearsheet and write it to `path`. Returns the HTML.
pub fn write_tearsheet_html(report: &Report, path: &Path) -> std::io::Result<String> {
    let html = render_tearsheet(report);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::File::create(path)?;
    f.write_all(html.as_bytes())?;
    Ok(html)
}

fn render_tearsheet(report: &Report) -> String {
    let s = &report.summary;
    let curve = &report.equity_curve;

    let (equity_path, dd_path, vb_min, vb_max) = build_svg_paths(curve);

    let stat_row = |k: &str, v: String| -> String {
        format!("<tr><td class=\"k\">{}</td><td class=\"v\">{}</td></tr>", k, v)
    };

    let stats = [
        stat_row("Strategy", html_escape(&report.plugin_name)),
        stat_row("Currency", html_escape(&s.currency)),
        stat_row("Starting balance", format!("{:.2}", s.starting_balance)),
        stat_row("Ending balance", format!("{:.2}", s.ending_balance)),
        stat_row("PnL", format!("{:.2} ({:.2}%)", s.pnl_total, s.pnl_pct)),
        stat_row("Total orders", s.total_orders.to_string()),
        stat_row("Fills", s.num_fills.to_string()),
        stat_row("Round-trips", s.num_trades.to_string()),
        stat_row("Win rate", format!("{:.1}%", s.win_rate * 100.0)),
        stat_row("Avg buy price", format!("{:.4}", s.avg_buy_price)),
        stat_row("Avg sell price", format!("{:.4}", s.avg_sell_price)),
        stat_row("Sharpe (per-snap)", format!("{:.3}", s.sharpe)),
        stat_row("Sortino (per-snap)", format!("{:.3}", s.sortino)),
        stat_row("Max drawdown", format!("{:.2} ({:.1}%)", s.max_drawdown, s.max_drawdown_pct * 100.0)),
        stat_row("Calmar", format!("{:.3}", s.calmar_ratio)),
        stat_row("Volatility (per-snap)", format!("{:.5}", s.volatility)),
        stat_row("Profit factor", format!("{:.3}", s.profit_factor)),
        stat_row("Expectancy", format!("{:.4}", s.expectancy)),
        stat_row("Payoff ratio", format!("{:.3}", s.payoff_ratio)),
        stat_row("Avg win / loss", format!("{:.3} / {:.3}", s.avg_win, s.avg_loss)),
        stat_row("Largest win / loss", format!("{:.2} / {:.2}", s.largest_win, s.largest_loss)),
        stat_row("Max consec W / L", format!("{} / {}", s.max_consecutive_wins, s.max_consecutive_losses)),
        stat_row("Exposure", format!("{:.1}%", s.exposure_pct * 100.0)),
        stat_row("Avg holding (s)", format!("{:.1}", s.avg_holding_secs)),
        stat_row("Volume (contracts)", format!("{:.0}", s.total_volume_contracts)),
        stat_row("Turnover", format!("{:.2}x", s.turnover)),
        stat_row("Total fees", format!("{:.2}", s.total_fees)),
        stat_row("Fees % of gross", format!("{:.1}%", s.fees_pct_of_gross * 100.0)),
        stat_row("Slippage cost", format!("{:.2}", s.total_slippage_cost)),
        stat_row("Liquidity rewards", format!("{:.2}", s.liquidity_rewards)),
        stat_row("Gross PnL (ex costs)", format!("{:.2}", s.gross_pnl_ex_costs)),
    ]
    .join("\n");

    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Backtest Tearsheet — {strategy}</title>
<style>
  :root {{ --bg:#0d1117; --panel:#161b22; --fg:#e6edf3; --muted:#8b949e; --accent:#2f81f7; --red:#f85149; }}
  * {{ box-sizing:border-box; }}
  body {{ margin:0; background:var(--bg); color:var(--fg); font-family:ui-monospace,SFMono-Regular,Menlo,monospace; }}
  .wrap {{ max-width:980px; margin:0 auto; padding:24px; }}
  h1 {{ font-size:20px; margin:0 0 4px; }}
  .sub {{ color:var(--muted); margin-bottom:20px; font-size:13px; }}
  .grid {{ display:grid; grid-template-columns:1fr 320px; gap:20px; align-items:start; }}
  .panel {{ background:var(--panel); border:1px solid #30363d; border-radius:10px; padding:16px; }}
  .panel h2 {{ font-size:13px; color:var(--muted); margin:0 0 10px; text-transform:uppercase; letter-spacing:.05em; }}
  table {{ width:100%; border-collapse:collapse; font-size:13px; }}
  td {{ padding:5px 4px; border-bottom:1px solid #21262d; }}
  td.k {{ color:var(--muted); }}
  td.v {{ text-align:right; }}
  svg {{ width:100%; height:auto; display:block; }}
  .pnl-pos {{ color:#3fb950; }} .pnl-neg {{ color:var(--red); }}
  @media (max-width:760px) {{ .grid {{ grid-template-columns:1fr; }} }}
</style></head>
<body><div class="wrap">
  <h1>Backtest Tearsheet</h1>
  <div class="sub">strategy: <b>{strategy}</b> &middot; PnL <b class="{pnl_cls}">{pnl:.2} {ccy} ({pnl_pct:.2}%)</b> &middot; {npts} equity points</div>
  <div class="grid">
    <div>
      <div class="panel">
        <h2>Equity curve</h2>
        <svg viewBox="0 0 800 240" preserveAspectRatio="none">
          <rect x="0" y="0" width="800" height="240" fill="none"/>
          <path d="{eq_path}" fill="none" stroke="var(--accent)" stroke-width="2"/>
        </svg>
        <div class="sub" style="margin:6px 0 0">min {vbmin:.2} &middot; max {vbmax:.2}</div>
      </div>
      <div class="panel" style="margin-top:20px">
        <h2>Drawdown</h2>
        <svg viewBox="0 0 800 140" preserveAspectRatio="none">
          <path d="{dd_path}" fill="rgba(248,81,73,0.18)" stroke="var(--red)" stroke-width="1.5"/>
        </svg>
      </div>
    </div>
    <div class="panel">
      <h2>Statistics</h2>
      <table>{stats}</table>
    </div>
  </div>
</div></body></html>"#,
        strategy = html_escape(&report.plugin_name),
        pnl = s.pnl_total,
        ccy = html_escape(&s.currency),
        pnl_pct = s.pnl_pct,
        pnl_cls = if s.pnl_total >= 0.0 { "pnl-pos" } else { "pnl-neg" },
        npts = curve.len(),
        eq_path = equity_path,
        dd_path = dd_path,
        vbmin = vb_min,
        vbmax = vb_max,
        stats = stats,
    )
}

/// Build SVG path data for the equity curve (800x240) and drawdown (800x140).
fn build_svg_paths(curve: &[EquityPoint]) -> (String, String, f64, f64) {
    if curve.is_empty() {
        return ("M0 120 L800 120".into(), "M0 0 L800 0 L800 0 Z".into(), 0.0, 0.0);
    }
    let vals: Vec<f64> = curve.iter().map(|p| p.total).collect();
    let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = (max - min).max(1e-9);
    let n = vals.len();
    let xstep = if n > 1 { 800.0 / (n as f64 - 1.0) } else { 0.0 };

    // equity path (y flipped: high value = low y)
    let mut eq = String::new();
    for (i, &v) in vals.iter().enumerate() {
        let x = i as f64 * xstep;
        let y = 230.0 - ((v - min) / range) * 220.0;
        if i == 0 {
            eq.push_str(&format!("M{:.2} {:.2}", x, y));
        } else {
            eq.push_str(&format!(" L{:.2} {:.2}", x, y));
        }
    }

    // drawdown path: dd_i = peak_so_far - v_i, as fraction of peak, plotted downward from top.
    let mut peak = f64::NEG_INFINITY;
    let mut dds = Vec::with_capacity(n);
    for &v in &vals {
        if v > peak {
            peak = v;
        }
        let dd = if peak.abs() > 1e-9 {
            (peak - v) / peak
        } else {
            0.0
        };
        dds.push(dd);
    }
    let max_dd = dds.iter().cloned().fold(0.0_f64, f64::max).max(1e-9);
    let mut dd = String::from("M0 0");
    for (i, &d) in dds.iter().enumerate() {
        let x = i as f64 * xstep;
        let y = (d / max_dd) * 135.0;
        dd.push_str(&format!(" L{:.2} {:.2}", x, y));
    }
    dd.push_str(&format!(" L{:.2} 0 Z", (n as f64 - 1.0).max(0.0) * xstep));

    (eq, dd, min, max)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Minimal standard base64 encoder (no padding omission, no deps).
pub fn base64_encode(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TBL[((n >> 18) & 63) as usize] as char);
        out.push(TBL[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TBL[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TBL[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIX 4: a flawless strategy (winners, NO losers) must report `profit_factor` / `payoff_ratio`
    /// as the clear LARGE FINITE sentinel (1e9) in the *report* — not 0.0 (which would be
    /// indistinguishable from a no-winners strategy) and not `null`/inf. This exercises the report
    /// coercion, which the raw-`TradeStats`-only metric test does not cover.
    #[test]
    fn no_loss_profit_factor_is_large_sentinel_in_report() {
        let cfg = BacktestConfig::default();
        let mut pf = Portfolio::new(cfg.starting_balance, cfg.currency.clone(), 1);
        // two winning round trips, no losses -> raw profit_factor / payoff_ratio are +inf.
        pf.round_trip_pnls = vec![5.0, 7.0];
        let report = build_report("t", &pf, &cfg);
        assert_eq!(report.summary.profit_factor, NO_LOSS_SENTINEL);
        assert_eq!(report.summary.payoff_ratio, NO_LOSS_SENTINEL);
        assert!(report.summary.profit_factor.is_finite());

        // contrast: a strategy with NO winners reports 0.0, distinct from the sentinel.
        let mut pf_losers = Portfolio::new(cfg.starting_balance, cfg.currency.clone(), 1);
        pf_losers.round_trip_pnls = vec![-5.0, -7.0];
        let report_losers = build_report("t", &pf_losers, &cfg);
        assert_eq!(report_losers.summary.profit_factor, 0.0);
        assert!(report_losers.summary.profit_factor != NO_LOSS_SENTINEL);
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn tearsheet_renders_without_panic() {
        let report = Report {
            plugin_name: "test".into(),
            summary: Summary {
                currency: "USD".into(),
                starting_balance: 1000.0,
                ending_balance: 1010.0,
                pnl_total: 10.0,
                pnl_pct: 1.0,
                total_orders: 3,
                total_positions: 1,
                avg_buy_price: 0.4,
                avg_sell_price: 0.6,
                num_trades: 1,
                num_fills: 2,
                win_rate: 1.0,
                sharpe: 0.5,
                sortino: 0.8,
                max_drawdown: 5.0,
                max_drawdown_pct: 0.005,
                turnover: 0.1,
                total_fees: 0.2,
                profit_factor: 2.0,
                gross_profit: 30.0,
                gross_loss: 15.0,
                avg_win: 15.0,
                avg_loss: 15.0,
                payoff_ratio: 1.0,
                expectancy: 5.0,
                num_round_trips: 1,
                avg_trade_pnl: 10.0,
                largest_win: 30.0,
                largest_loss: -15.0,
                max_consecutive_wins: 1,
                max_consecutive_losses: 0,
                calmar_ratio: 2.0,
                volatility: 0.01,
                downside_volatility: 0.008,
                exposure_pct: 0.5,
                avg_holding_secs: 12.0,
                fees_pct_of_gross: 0.006,
                total_volume_contracts: 200.0,
                total_slippage_cost: 0.0,
                liquidity_rewards: 0.0,
                gross_pnl_ex_costs: 10.2,
                settled_pnl: 0.0,
                num_settled: 0,
                halted: false,
                halt_reason: String::new(),
                risk_rejections: 0,
                post_only_rejects: 0,
            },
            equity_curve: vec![
                EquityPoint { ts_ns: 1, total: 1000.0, currency: "USD".into() },
                EquityPoint { ts_ns: 2, total: 990.0, currency: "USD".into() },
                EquityPoint { ts_ns: 3, total: 1010.0, currency: "USD".into() },
            ],
            instrument_breakdown: vec![],
        };
        let html = render_tearsheet(&report);
        assert!(html.contains("Backtest Tearsheet"));
        assert!(html.contains("<svg"));
    }
}
