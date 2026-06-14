//! Structured exports for the dashboard.
//!
//! This is a **shared contract** with a dashboard built in parallel: the file names and CSV
//! headers/columns below must match EXACTLY. Everything is written into `--out-dir` after a run.
//!
//! Files written:
//! - `report.json` — the full [`Report`] (pretty JSON).
//! - `equity.csv` — `ts_ns,total,currency,drawdown`
//! - `fills.csv` — `ts_ns,instrument,order_id,side,price,qty,liquidity,fee`
//! - `trades.csv` — `ts_ns,instrument,aggressor_side,price,size`
//! - `round_trips.csv` — `instrument,entry_ts_ns,exit_ts_ns,qty,entry_price,exit_price,pnl`
//! - `instrument_stats.csv` — `instrument,pnl,num_fills,num_round_trips,net_position,volume`
//! - `meta.json` — run metadata (generated_unix_ns is 0; the runner stamps it).
//!
//! All paths are built with [`Path`]/[`PathBuf`] and parents are created via `create_dir_all`, so
//! this is cross-platform (Windows-safe).

use crate::portfolio::Portfolio;
use crate::types::{Liquidity, Report, Side, TradeEvent};
use serde::Serialize;
use std::io::Write;
use std::path::Path;

/// Metadata about the run, serialized to `meta.json`.
#[derive(Debug, Clone, Serialize)]
pub struct ExportMeta {
    pub strategy: String,
    pub source: String,
    pub instrument_filter: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub starting_balance: f64,
    pub currency: String,
    /// 0 here; the runner stamps the real time (Date is unavailable in this crate).
    pub generated_unix_ns: i64,
}

/// Write all dashboard export files into `out_dir` (created if missing).
pub fn write_exports(
    out_dir: &Path,
    report: &Report,
    portfolio: &Portfolio,
    observed_trades: &[TradeEvent],
    meta: &ExportMeta,
) -> std::io::Result<()> {
    std::fs::create_dir_all(out_dir)?;

    write_report_json(&out_dir.join("report.json"), report)?;
    write_equity_csv(&out_dir.join("equity.csv"), report)?;
    write_fills_csv(&out_dir.join("fills.csv"), portfolio)?;
    write_trades_csv(&out_dir.join("trades.csv"), observed_trades)?;
    write_round_trips_csv(&out_dir.join("round_trips.csv"), portfolio)?;
    write_instrument_stats_csv(&out_dir.join("instrument_stats.csv"), report)?;
    write_meta_json(&out_dir.join("meta.json"), meta)?;
    Ok(())
}

fn create(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::File::create(path)
}

fn write_report_json(path: &Path, report: &Report) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(report)
        .unwrap_or_else(|_| "{}".to_string());
    let mut f = create(path)?;
    f.write_all(json.as_bytes())
}

fn write_meta_json(path: &Path, meta: &ExportMeta) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(meta).unwrap_or_else(|_| "{}".to_string());
    let mut f = create(path)?;
    f.write_all(json.as_bytes())
}

fn write_equity_csv(path: &Path, report: &Report) -> std::io::Result<()> {
    let mut f = create(path)?;
    writeln!(f, "ts_ns,total,currency,drawdown")?;
    let mut peak = f64::NEG_INFINITY;
    for p in &report.equity_curve {
        if p.total > peak {
            peak = p.total;
        }
        let drawdown = peak - p.total; // running peak - total, in currency
        writeln!(
            f,
            "{},{},{},{}",
            p.ts_ns,
            fmt(p.total),
            csv_field(&p.currency),
            fmt(drawdown)
        )?;
    }
    Ok(())
}

fn write_fills_csv(path: &Path, portfolio: &Portfolio) -> std::io::Result<()> {
    let mut f = create(path)?;
    writeln!(f, "ts_ns,instrument,order_id,side,price,qty,liquidity,fee")?;
    for fill in &portfolio.fills {
        let side = match fill.side {
            Side::Bid => "BUY",
            Side::Ask => "SELL",
        };
        let liq = match fill.liquidity {
            Liquidity::Maker => "Maker",
            Liquidity::Taker => "Taker",
            Liquidity::Settle => "Settle",
        };
        writeln!(
            f,
            "{},{},{},{},{},{},{},{}",
            fill.ts_ns,
            csv_field(&fill.instrument),
            fill.order_id,
            side,
            fmt(fill.price.to_dollars()),
            fmt(fill.qty),
            liq,
            fmt(fill.fee),
        )?;
    }
    Ok(())
}

fn write_trades_csv(path: &Path, trades: &[TradeEvent]) -> std::io::Result<()> {
    let mut f = create(path)?;
    writeln!(f, "ts_ns,instrument,aggressor_side,price,size")?;
    for t in trades {
        let aggr = if t.aggressor_yes { "yes" } else { "no" };
        writeln!(
            f,
            "{},{},{},{},{}",
            t.ts_ns,
            csv_field(&t.instrument),
            aggr,
            fmt(t.price.to_dollars()),
            fmt(t.size),
        )?;
    }
    Ok(())
}

fn write_round_trips_csv(path: &Path, portfolio: &Portfolio) -> std::io::Result<()> {
    let mut f = create(path)?;
    writeln!(
        f,
        "instrument,entry_ts_ns,exit_ts_ns,qty,entry_price,exit_price,pnl"
    )?;
    for rt in &portfolio.round_trips {
        writeln!(
            f,
            "{},{},{},{},{},{},{}",
            csv_field(&rt.instrument),
            rt.entry_ts,
            rt.exit_ts,
            fmt(rt.qty),
            fmt(rt.entry_price),
            fmt(rt.exit_price),
            fmt(rt.pnl),
        )?;
    }
    Ok(())
}

fn write_instrument_stats_csv(path: &Path, report: &Report) -> std::io::Result<()> {
    let mut f = create(path)?;
    writeln!(
        f,
        "instrument,pnl,num_fills,num_round_trips,net_position,volume"
    )?;
    for st in &report.instrument_breakdown {
        writeln!(
            f,
            "{},{},{},{},{},{}",
            csv_field(&st.instrument),
            fmt(st.pnl),
            st.num_fills,
            st.num_round_trips,
            fmt(st.net_position),
            fmt(st.volume),
        )?;
    }
    Ok(())
}

/// Format a float without scientific notation, trimming trailing zeros for compactness.
fn fmt(x: f64) -> String {
    if x == x.trunc() && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        // up to 6 decimal places, trim trailing zeros
        let s = format!("{:.6}", x);
        let s = s.trim_end_matches('0').trim_end_matches('.');
        s.to_string()
    }
}

/// Quote a CSV field if it contains a comma, quote, or newline (RFC-4180 style).
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Cents, Fill, InstrumentBreakdown, Summary};

    fn empty_summary() -> Summary {
        // all-zero summary for a minimal report fixture
        serde_json::from_str(
            r#"{"currency":"USD","starting_balance":1000,"ending_balance":1000,"pnl_total":0,
                "pnl_pct":0,"total_orders":0,"total_positions":0,"avg_buy_price":0,"avg_sell_price":0,
                "num_trades":0,"num_fills":0,"win_rate":0,"sharpe":0,"sortino":0,"max_drawdown":0,
                "max_drawdown_pct":0,"turnover":0,"total_fees":0,"profit_factor":0,"gross_profit":0,
                "gross_loss":0,"avg_win":0,"avg_loss":0,"payoff_ratio":0,"expectancy":0,
                "num_round_trips":0,"avg_trade_pnl":0,"largest_win":0,"largest_loss":0,
                "max_consecutive_wins":0,"max_consecutive_losses":0,"calmar_ratio":0,"volatility":0,
                "downside_volatility":0,"exposure_pct":0,"avg_holding_secs":0,"fees_pct_of_gross":0,
                "total_volume_contracts":0}"#,
        )
        .unwrap()
    }

    #[test]
    fn writes_all_seven_files_with_headers() {
        let dir = std::env::temp_dir().join(format!("exp_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut pf = Portfolio::new(1000.0, "USD".into(), 1);
        pf.fills.push(Fill {
            ts_ns: 5,
            order_id: 1,
            instrument: "X".into(),
            side: Side::Bid,
            price: Cents(40),
            qty: 10.0,
            liquidity: Liquidity::Taker,
            fee: 0.01,
        });

        let report = Report {
            plugin_name: "t".into(),
            summary: empty_summary(),
            equity_curve: vec![crate::types::EquityPoint {
                ts_ns: 1,
                total: 1000.0,
                currency: "USD".into(),
            }],
            instrument_breakdown: vec![InstrumentBreakdown {
                instrument: "X".into(),
                pnl: 1.0,
                num_fills: 1,
                num_round_trips: 0,
                net_position: 10.0,
                volume: 10.0,
            }],
        };
        let trades = vec![TradeEvent {
            ts_ns: 7,
            instrument: "X".into(),
            aggressor_yes: false,
            price: Cents(55),
            size: 3.0,
            trade_id: "t1".into(),
        }];
        let meta = ExportMeta {
            strategy: "t".into(),
            source: "ndjson".into(),
            instrument_filter: None,
            start: None,
            end: None,
            starting_balance: 1000.0,
            currency: "USD".into(),
            generated_unix_ns: 0,
        };

        write_exports(&dir, &report, &pf, &trades, &meta).unwrap();

        let head = |name: &str| {
            let txt = std::fs::read_to_string(dir.join(name)).unwrap();
            txt.lines().next().unwrap().to_string()
        };
        assert_eq!(head("equity.csv"), "ts_ns,total,currency,drawdown");
        assert_eq!(
            head("fills.csv"),
            "ts_ns,instrument,order_id,side,price,qty,liquidity,fee"
        );
        assert_eq!(head("trades.csv"), "ts_ns,instrument,aggressor_side,price,size");
        assert_eq!(
            head("round_trips.csv"),
            "instrument,entry_ts_ns,exit_ts_ns,qty,entry_price,exit_price,pnl"
        );
        assert_eq!(
            head("instrument_stats.csv"),
            "instrument,pnl,num_fills,num_round_trips,net_position,volume"
        );
        assert!(dir.join("report.json").exists());
        assert!(dir.join("meta.json").exists());

        // fills row content sanity
        let fills = std::fs::read_to_string(dir.join("fills.csv")).unwrap();
        assert!(fills.contains("5,X,1,BUY,0.4,10,Taker,0.01"), "fills:\n{fills}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
