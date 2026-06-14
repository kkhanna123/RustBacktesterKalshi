//! NDJSON loader for our collector's raw captures (`.ndjson` or `.ndjson.gz`).
//!
//! Each line is one JSON object, either a `delta` or a `trade` (see the schema in the module
//! doc). `BUY`→`Side::Bid`, `SELL`→`Side::Ask`; dollar prices map to `Cents::from_dollars`.

use crate::data::{glob_match, sort_events};
use crate::types::{Action, BookDelta, Cents, MarketEvent, Side, TradeEvent};
use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// Load events from an ndjson(.gz) file, filtered by instrument glob and optional date range.
pub fn load(
    path: &Path,
    instrument: Option<&str>,
    start_ns: Option<i64>,
    end_ns: Option<i64>,
) -> Result<Vec<MarketEvent>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader: Box<dyn Read> = if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        Box::new(GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let buf = BufReader::new(reader);

    let mut events = Vec::new();
    for line in buf.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(line).with_context(|| format!("bad json line: {line}"))?;
        if let Some(ev) = parse_event(&v)? {
            if let Some(pat) = instrument {
                if !glob_match(pat, ev.instrument()) {
                    continue;
                }
            }
            let ts = ev.ts_ns();
            if let Some(s) = start_ns {
                if ts < s {
                    continue;
                }
            }
            if let Some(e) = end_ns {
                if ts >= e {
                    continue;
                }
            }
            events.push(ev);
        }
    }
    Ok(sort_events(events))
}

fn parse_event(v: &serde_json::Value) -> Result<Option<MarketEvent>> {
    let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    match kind {
        "delta" => Ok(Some(MarketEvent::Delta(parse_delta(v)?))),
        "trade" => Ok(Some(MarketEvent::Trade(parse_trade(v)?))),
        other => Err(anyhow!("unknown kind: {other}")),
    }
}

fn parse_delta(v: &serde_json::Value) -> Result<BookDelta> {
    let ts_ns = v.get("ts_ns").and_then(|x| x.as_i64()).context("ts_ns")?;
    let instrument = v
        .get("instrument")
        .and_then(|x| x.as_str())
        .context("instrument")?
        .to_string();
    let action = match v.get("action").and_then(|x| x.as_str()).unwrap_or("") {
        "ADD" => Action::Add,
        "UPDATE" => Action::Update,
        "DELETE" => Action::Delete,
        a => return Err(anyhow!("bad action {a}")),
    };
    let side = match v.get("side").and_then(|x| x.as_str()).unwrap_or("") {
        "BUY" => Side::Bid,
        "SELL" => Side::Ask,
        s => return Err(anyhow!("bad side {s}")),
    };
    let price = Cents::from_dollars(v.get("price").and_then(|x| x.as_f64()).context("price")?);
    let size = v.get("size").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let sequence = v.get("sequence").and_then(|x| x.as_i64()).unwrap_or(0);
    let is_snapshot = v
        .get("is_snapshot")
        .map(|x| x.as_i64().unwrap_or(0) != 0 || x.as_bool().unwrap_or(false))
        .unwrap_or(false);
    Ok(BookDelta {
        ts_ns,
        instrument,
        action,
        side,
        price,
        size,
        sequence,
        is_snapshot,
    })
}

fn parse_trade(v: &serde_json::Value) -> Result<TradeEvent> {
    let ts_ns = v.get("ts_ns").and_then(|x| x.as_i64()).context("ts_ns")?;
    let instrument = v
        .get("instrument")
        .and_then(|x| x.as_str())
        .context("instrument")?
        .to_string();
    let aggressor_yes = matches!(
        v.get("aggressor_side").and_then(|x| x.as_str()).unwrap_or("yes"),
        "yes" | "YES" | "BUY"
    );
    let price = Cents::from_dollars(v.get("price").and_then(|x| x.as_f64()).context("price")?);
    let size = v.get("size").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let trade_id = v
        .get("trade_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Ok(TradeEvent {
        ts_ns,
        instrument,
        aggressor_yes,
        price,
        size,
        trade_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_delta_and_trade() {
        let d: serde_json::Value = serde_json::from_str(
            r#"{"kind":"delta","ts_ns":1000,"instrument":"X","action":"ADD","side":"BUY","price":0.42,"size":100.0,"sequence":1,"is_snapshot":1}"#,
        )
        .unwrap();
        let ev = parse_event(&d).unwrap().unwrap();
        match ev {
            MarketEvent::Delta(d) => {
                assert_eq!(d.price, Cents(42));
                assert_eq!(d.side, Side::Bid);
                assert!(d.is_snapshot);
            }
            _ => panic!(),
        }

        let t: serde_json::Value = serde_json::from_str(
            r#"{"kind":"trade","ts_ns":2000,"instrument":"X","aggressor_side":"no","price":0.55,"size":3.0,"trade_id":"t1"}"#,
        )
        .unwrap();
        match parse_event(&t).unwrap().unwrap() {
            MarketEvent::Trade(tr) => {
                assert_eq!(tr.price, Cents(55));
                assert!(!tr.aggressor_yes);
            }
            _ => panic!(),
        }
    }
}
