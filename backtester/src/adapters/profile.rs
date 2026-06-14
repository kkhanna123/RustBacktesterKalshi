//! ADAPTER PROFILE files — a reusable, on-disk mapping for the generic CSV/NDJSON adapters.
//!
//! Instead of cramming `mapping` `key=value` pairs onto the CLI, keep them in a small JSON or TOML
//! file and pass `--adapter-profile <path>`. A profile can also pin the `adapter`, `venue`, and
//! `instrument` so a whole source is described in one place. CLI flags always OVERRIDE the file.
//!
//! # Format
//! ```json
//! {
//!   "adapter": "generic_csv",
//!   "venue": "POLYMARKET",
//!   "instrument": "0x%",
//!   "mapping": {
//!     "ts_ns": "ts_ms", "ts_unit": "ms",
//!     "instrument": "symbol",
//!     "price": "price_cents", "price_scale": "cents",
//!     "size": "size", "side": "side"
//!   }
//! }
//! ```
//! The TOML form is the same shape (`[mapping]` table). Every field is OPTIONAL.
//!
//! # Relationship to `tools/to_canonical.py` profiles
//! The Python converter's profile uses `*_col` / `*_const` / `ts_unit` / `price_scale` keys. This
//! Rust profile expresses the SAME idea via the generic adapters' `mapping` (a canonical→source
//! name map) plus `ts_unit` / `price_scale` entries inside `mapping`. To make the two interchangeable
//! for the common case, a few top-level `to_canonical`-style keys are ALSO accepted here and folded
//! into `mapping`: `ts_unit`, `price_scale`, and any `*_col` key (e.g. `price_col = "px"` becomes
//! `mapping.price = "px"`). Differences: snapshot-mode/no-side/diffing and ISO timestamp parsing are
//! converter-only (use `to_canonical.py` for those); this profile targets the row-per-event adapters.

use crate::adapters::AdapterSpec;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// A loaded adapter profile. Mirrors the optional pieces of an [`AdapterSpec`] plus a `mapping`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AdapterProfile {
    /// Adapter key (e.g. `generic_csv`, `generic_ndjson`). Optional — CLI `--adapter` can supply it.
    pub adapter: Option<String>,
    /// Venue tag to stamp (e.g. `POLYMARKET`). Optional.
    pub venue: Option<String>,
    /// Instrument glob. Optional.
    pub instrument: Option<String>,
    /// The generic-adapter field mapping (canonical → source name, plus `ts_unit`/`price_scale`/…).
    pub mapping: BTreeMap<String, String>,

    // ---- convenience keys mirroring tools/to_canonical.py, folded into `mapping` on load. ----
    /// Timestamp unit (`s|ms|us|ns`); copied to `mapping.ts_unit` if `mapping` doesn't set it.
    pub ts_unit: Option<String>,
    /// Price unit (`dollars|cents|bps|prob`); copied to `mapping.price_scale` if unset there.
    pub price_scale: Option<String>,
    /// Catch-all for `*_col` (and other extra) string keys, e.g. `price_col`, `ts_col`. A `<x>_col`
    /// key becomes `mapping.<x>` (so `price_col = "px"` ⇒ `mapping.price = "px"`).
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

impl AdapterProfile {
    /// Load a profile from a `.toml` or `.json` file (extension picks the format; default JSON).
    pub fn from_path(path: &Path) -> Result<AdapterProfile> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read --adapter-profile {}", path.display()))?;
        let is_toml = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("toml"))
            .unwrap_or(false);
        let mut prof: AdapterProfile = if is_toml {
            toml::from_str(&text)
                .with_context(|| format!("invalid TOML in --adapter-profile {}", path.display()))?
        } else {
            serde_json::from_str(&text)
                .with_context(|| format!("invalid JSON in --adapter-profile {}", path.display()))?
        };
        prof.fold_convenience_keys()
            .with_context(|| format!("in --adapter-profile {}", path.display()))?;
        Ok(prof)
    }

    /// Fold the convenience keys (`ts_unit`, `price_scale`, and any `*_col`) into `mapping`. An
    /// explicit `mapping` entry always wins over the convenience form.
    fn fold_convenience_keys(&mut self) -> Result<()> {
        if let Some(u) = self.ts_unit.take() {
            self.mapping.entry("ts_unit".to_string()).or_insert(u);
        }
        if let Some(s) = self.price_scale.take() {
            self.mapping.entry("price_scale".to_string()).or_insert(s);
        }
        // Any `<x>_col = "src"` → mapping.<x> = "src". Reject other unexpected top-level keys so a
        // typo (e.g. `pirce_col`) is caught instead of silently ignored.
        let extra = std::mem::take(&mut self.extra);
        for (k, v) in extra {
            if let Some(canonical) = k.strip_suffix("_col") {
                let src = v
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("profile key '{k}' must be a string column name"))?;
                self.mapping
                    .entry(canonical.to_string())
                    .or_insert_with(|| src.to_string());
            } else {
                bail!(
                    "unknown profile key '{k}' — expected adapter|venue|instrument|mapping|ts_unit|price_scale or a <field>_col key"
                );
            }
        }
        Ok(())
    }

    /// Apply this profile onto an [`AdapterSpec`], filling only the fields the spec hasn't already
    /// set on the CLI (CLI wins). `mapping` entries from the profile fill keys not already present.
    pub fn apply_to(&self, spec: &mut AdapterSpec) {
        if spec.adapter.is_empty() {
            if let Some(a) = &self.adapter {
                spec.adapter = a.clone();
            }
        }
        if spec.venue.is_empty() {
            if let Some(vn) = &self.venue {
                spec.venue = vn.clone();
            }
        }
        if spec.instrument.is_none() {
            spec.instrument = self.instrument.clone();
        }
        for (k, v) in &self.mapping {
            spec.mapping.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(tag: &str, ext: &str, body: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("prof_{}_{tag}_{n}.{ext}", std::process::id()));
        std::fs::File::create(&p).unwrap().write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn json_profile_loads_and_applies() {
        let p = write(
            "j",
            "json",
            r#"{"adapter":"generic_csv","venue":"POLYMARKET","instrument":"0x%",
                "mapping":{"ts_ns":"ts_ms","ts_unit":"ms","price":"px","price_scale":"cents"}}"#,
        );
        let prof = AdapterProfile::from_path(&p).unwrap();
        let mut spec = AdapterSpec::default();
        prof.apply_to(&mut spec);
        assert_eq!(spec.adapter, "generic_csv");
        assert_eq!(spec.venue, "POLYMARKET");
        assert_eq!(spec.instrument.as_deref(), Some("0x%"));
        assert_eq!(spec.mapping.get("ts_ns").map(|s| s.as_str()), Some("ts_ms"));
        assert_eq!(spec.mapping.get("price_scale").map(|s| s.as_str()), Some("cents"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn toml_profile_and_convenience_col_keys() {
        // to_canonical-style: price_col + ts_unit at top level fold into mapping.
        let p = write(
            "t",
            "toml",
            "adapter = \"generic_ndjson\"\nprice_col = \"px\"\nts_unit = \"s\"\nprice_scale = \"prob\"\n",
        );
        let prof = AdapterProfile::from_path(&p).unwrap();
        let mut spec = AdapterSpec::default();
        prof.apply_to(&mut spec);
        assert_eq!(spec.adapter, "generic_ndjson");
        assert_eq!(spec.mapping.get("price").map(|s| s.as_str()), Some("px"));
        assert_eq!(spec.mapping.get("ts_unit").map(|s| s.as_str()), Some("s"));
        assert_eq!(spec.mapping.get("price_scale").map(|s| s.as_str()), Some("prob"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn cli_values_win_over_profile() {
        let p = write("o", "json", r#"{"adapter":"generic_csv","venue":"GENERIC"}"#);
        let prof = AdapterProfile::from_path(&p).unwrap();
        // Spec already has CLI-set adapter/venue; profile must not overwrite them.
        let mut spec = AdapterSpec {
            adapter: "generic_ndjson".into(),
            venue: "KALSHI".into(),
            ..Default::default()
        };
        prof.apply_to(&mut spec);
        assert_eq!(spec.adapter, "generic_ndjson");
        assert_eq!(spec.venue, "KALSHI");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn unknown_key_is_rejected() {
        // A key that is neither a known field nor a `*_col` convenience must error (typo guard).
        let p = write("bad", "json", r#"{"prce":"px"}"#);
        let err = AdapterProfile::from_path(&p).unwrap_err();
        assert!(format!("{err:#}").contains("unknown profile key"), "got: {err:#}");
        std::fs::remove_file(&p).ok();
    }
}
