//! BINARY SETTLEMENT-AT-EXPIRY support: load each market's final outcome and look it up by
//! instrument id.
//!
//! Kalshi markets are BINARY (cash-or-nothing): at resolution a YES contract pays **$1** and a NO
//! contract pays **$0**. A net position of `q` YES contracts (q>0 long, q<0 short) HELD TO EXPIRY
//! therefore settles to a cash payoff of `q * payout`, where `payout = 1.0` if the market resolved
//! YES and `0.0` if NO. The realized PnL relative to cost basis is `q * (payout − avg_cost)`.
//! Settlement carries NO trading fee (Kalshi does not charge a fee on settlement).
//!
//! This module only deals with the *input*: parsing a settlement file into a map
//! `instrument_id -> Outcome` so the engine can settle held positions in `finalize` (see
//! [`crate::engine::Engine`]). Instruments absent from the map have an [`Outcome::Unknown`] result
//! and fall back to the existing flatten-at-mid behaviour.
//!
//! ## Supported file formats (auto-detected from contents, tolerant of common encodings)
//! * **CSV** — a `instrument_id,result` table (an optional header row is detected and skipped):
//!   ```text
//!   instrument_id,result
//!   KXNATGASD-26JUN05-T3.5,yes
//!   KXNATGASD-26JUN05-T4.0,no
//!   ```
//! * **JSON object** — `{"INSTRUMENT": "yes", "OTHER": "no", ...}`.
//! * **JSON array** — `[{"instrument_id":"INSTRUMENT","result":"yes"}, ...]`.
//!
//! In every format the *result* token is parsed leniently by [`Outcome::parse`]: `yes/no`, `YES/NO`,
//! `y/n`, `true/false`, and `1/0` all work (case-insensitively).

use std::collections::HashMap;
use std::path::Path;

/// The resolved outcome of a binary market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The event resolved YES — a YES contract pays $1.00 per contract.
    Yes,
    /// The event resolved NO — a YES contract pays $0.00 per contract.
    No,
    /// The outcome is not known (the instrument was absent from the settlement file). Positions in
    /// such instruments are NOT settled; they fall back to flatten-at-mid.
    Unknown,
}

impl Outcome {
    /// The per-contract YES payout in dollars for this outcome: `1.0` for YES, `0.0` for NO. Returns
    /// `None` for [`Outcome::Unknown`] (no settlement payout is defined).
    pub fn payout(self) -> Option<f64> {
        match self {
            Outcome::Yes => Some(1.0),
            Outcome::No => Some(0.0),
            Outcome::Unknown => None,
        }
    }

    /// Parse a result token leniently. Accepts (case-insensitively, trimmed):
    /// * YES: `yes`, `y`, `true`, `t`, `1`
    /// * NO:  `no`, `n`, `false`, `f`, `0`
    ///
    /// Anything else (including an empty string or a not-yet-finalized status like `active`) parses
    /// to [`Outcome::Unknown`] so unsettled markets are safely skipped rather than mis-settled.
    pub fn parse(s: &str) -> Outcome {
        match s.trim().to_ascii_lowercase().as_str() {
            "yes" | "y" | "true" | "t" | "1" => Outcome::Yes,
            "no" | "n" | "false" | "f" | "0" => Outcome::No,
            _ => Outcome::Unknown,
        }
    }
}

/// A loaded settlement map: `instrument_id -> Outcome`. Lookups for unknown instruments return
/// [`Outcome::Unknown`].
#[derive(Debug, Clone, Default)]
pub struct SettlementMap {
    map: HashMap<String, Outcome>,
}

impl SettlementMap {
    /// An empty map — every lookup is [`Outcome::Unknown`].
    pub fn new() -> Self {
        SettlementMap {
            map: HashMap::new(),
        }
    }

    /// Build directly from `(instrument, Outcome)` pairs (used by tests and programmatic callers).
    pub fn from_pairs<I, S>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (S, Outcome)>,
        S: Into<String>,
    {
        let mut map = HashMap::new();
        for (k, v) in pairs {
            map.insert(k.into(), v);
        }
        SettlementMap { map }
    }

    /// True if no outcomes are loaded (so settlement is effectively disabled).
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Number of instruments with a KNOWN (Yes/No) outcome.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// The outcome for `instrument`, or [`Outcome::Unknown`] if absent.
    pub fn outcome(&self, instrument: &str) -> Outcome {
        self.map.get(instrument).copied().unwrap_or(Outcome::Unknown)
    }

    /// Load a settlement map from a file, auto-detecting JSON vs CSV by the first non-whitespace
    /// byte (`{` or `[` => JSON, anything else => CSV). Only known (Yes/No) rows are stored;
    /// `Unknown`/blank result tokens are skipped, so a file that mixes finalized and not-yet-final
    /// markets only settles the finalized ones.
    pub fn from_path(path: &Path) -> std::io::Result<SettlementMap> {
        let text = std::fs::read_to_string(path)?;
        Ok(Self::from_str_auto(&text))
    }

    /// Parse a settlement map from a string, auto-detecting the format. See [`from_path`].
    ///
    /// [`from_path`]: SettlementMap::from_path
    pub fn from_str_auto(text: &str) -> SettlementMap {
        let trimmed = text.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            Self::from_json(text).unwrap_or_default()
        } else {
            Self::from_csv(text)
        }
    }

    /// Parse the simple CSV form `instrument_id,result`. A header row (whose first column equals
    /// `instrument_id`/`instrument`/`ticker`, case-insensitively) is detected and skipped. Blank
    /// lines, lines beginning with `#`, and rows whose result token is not a recognized Yes/No are
    /// ignored. Only the first two comma-separated columns are read (extra columns are tolerated).
    pub fn from_csv(text: &str) -> SettlementMap {
        let mut map = HashMap::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut cols = line.splitn(3, ',');
            let inst = cols.next().unwrap_or("").trim().trim_matches('"');
            let result = cols.next().unwrap_or("").trim().trim_matches('"');
            if inst.is_empty() {
                continue;
            }
            // Skip a header row like `instrument_id,result`.
            if i == 0 {
                let lower = inst.to_ascii_lowercase();
                if lower == "instrument_id" || lower == "instrument" || lower == "ticker" {
                    continue;
                }
            }
            match Outcome::parse(result) {
                Outcome::Unknown => {} // unrecognized / not-yet-final -> skip
                known => {
                    map.insert(inst.to_string(), known);
                }
            }
        }
        SettlementMap { map }
    }

    /// Parse either JSON shape: an object `{"INSTRUMENT": "yes", ...}` or an array of
    /// `{"instrument_id": "...", "result": "..."}` records. Result tokens are parsed leniently;
    /// unrecognized results are skipped. Returns `None` only if the text is not valid JSON of a
    /// supported shape.
    pub fn from_json(text: &str) -> Option<SettlementMap> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let mut map = HashMap::new();
        match v {
            // {"INSTRUMENT": "yes", ...}
            serde_json::Value::Object(obj) => {
                for (inst, val) in obj {
                    if let Some(tok) = json_scalar_str(&val) {
                        if let known @ (Outcome::Yes | Outcome::No) = Outcome::parse(&tok) {
                            map.insert(inst, known);
                        }
                    }
                }
            }
            // [{"instrument_id": "...", "result": "..."}, ...]
            serde_json::Value::Array(arr) => {
                for item in arr {
                    let obj = match item.as_object() {
                        Some(o) => o,
                        None => continue,
                    };
                    let inst = obj
                        .get("instrument_id")
                        .or_else(|| obj.get("instrument"))
                        .or_else(|| obj.get("ticker"))
                        .and_then(|x| x.as_str());
                    let result = obj
                        .get("result")
                        .or_else(|| obj.get("outcome"))
                        .or_else(|| obj.get("status"))
                        .and_then(json_scalar_str);
                    if let (Some(inst), Some(tok)) = (inst, result) {
                        if let known @ (Outcome::Yes | Outcome::No) = Outcome::parse(&tok) {
                            map.insert(inst.to_string(), known);
                        }
                    }
                }
            }
            _ => return None,
        }
        Some(SettlementMap { map })
    }
}

/// Render a JSON scalar (string / bool / number) as a result token for lenient parsing. Returns
/// `None` for null/array/object values.
fn json_scalar_str(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_parse_is_tolerant() {
        for y in ["yes", "YES", "Yes", "y", "Y", "true", "TRUE", "t", "1", " yes "] {
            assert_eq!(Outcome::parse(y), Outcome::Yes, "{y:?}");
        }
        for n in ["no", "NO", "No", "n", "N", "false", "f", "0", " no "] {
            assert_eq!(Outcome::parse(n), Outcome::No, "{n:?}");
        }
        for u in ["", "active", "settled", "maybe", "2"] {
            assert_eq!(Outcome::parse(u), Outcome::Unknown, "{u:?}");
        }
    }

    #[test]
    fn payout_values() {
        assert_eq!(Outcome::Yes.payout(), Some(1.0));
        assert_eq!(Outcome::No.payout(), Some(0.0));
        assert_eq!(Outcome::Unknown.payout(), None);
    }

    #[test]
    fn csv_with_header_and_mixed_results() {
        let csv = "instrument_id,result\nA,yes\nB,NO\nC,1\nD,0\n# a comment\nE,active\n\nF,true";
        let m = SettlementMap::from_csv(csv);
        assert_eq!(m.outcome("A"), Outcome::Yes);
        assert_eq!(m.outcome("B"), Outcome::No);
        assert_eq!(m.outcome("C"), Outcome::Yes);
        assert_eq!(m.outcome("D"), Outcome::No);
        assert_eq!(m.outcome("E"), Outcome::Unknown, "active is skipped");
        assert_eq!(m.outcome("F"), Outcome::Yes);
        assert_eq!(m.outcome("MISSING"), Outcome::Unknown);
    }

    #[test]
    fn csv_without_header() {
        let csv = "A,yes\nB,no";
        let m = SettlementMap::from_csv(csv);
        assert_eq!(m.outcome("A"), Outcome::Yes);
        assert_eq!(m.outcome("B"), Outcome::No);
    }

    #[test]
    fn json_object_form() {
        let j = r#"{"A": "yes", "B": "no", "C": true, "D": 0, "E": "active"}"#;
        let m = SettlementMap::from_json(j).unwrap();
        assert_eq!(m.outcome("A"), Outcome::Yes);
        assert_eq!(m.outcome("B"), Outcome::No);
        assert_eq!(m.outcome("C"), Outcome::Yes);
        assert_eq!(m.outcome("D"), Outcome::No);
        assert_eq!(m.outcome("E"), Outcome::Unknown);
    }

    #[test]
    fn json_array_form() {
        let j = r#"[{"instrument_id":"A","result":"YES"},
                    {"instrument_id":"B","result":"no"},
                    {"ticker":"C","outcome":"1"}]"#;
        let m = SettlementMap::from_json(j).unwrap();
        assert_eq!(m.outcome("A"), Outcome::Yes);
        assert_eq!(m.outcome("B"), Outcome::No);
        assert_eq!(m.outcome("C"), Outcome::Yes);
    }

    #[test]
    fn auto_detect_picks_format() {
        assert_eq!(
            SettlementMap::from_str_auto(r#"{"A":"yes"}"#).outcome("A"),
            Outcome::Yes
        );
        assert_eq!(
            SettlementMap::from_str_auto("A,no").outcome("A"),
            Outcome::No
        );
        assert_eq!(
            SettlementMap::from_str_auto(r#"[{"instrument_id":"A","result":"yes"}]"#).outcome("A"),
            Outcome::Yes
        );
    }

    #[test]
    fn empty_map_is_all_unknown() {
        let m = SettlementMap::new();
        assert!(m.is_empty());
        assert_eq!(m.outcome("anything"), Outcome::Unknown);
    }
}
