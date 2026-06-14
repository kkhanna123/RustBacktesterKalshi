//! A full, serializable backtest **run specification** — everything needed to reproduce a run in
//! one file: data source, instrument filter, strategy, date range, starting balance, and the
//! embedded [`ExecutionConfig`] (fees/latency/slippage/rewards).
//!
//! `kalshi-backtest backtest --config run.toml` loads a [`RunSpec`] and runs it. Individual CLI
//! flags still override any field of the loaded spec (see `main::merge_config_into_args`), so a
//! config file is a convenient *baseline* you can tweak from the command line.
//!
//! Both TOML and JSON are supported with no special build flags: the extension picks the parser
//! (`.toml` → TOML, anything else → JSON). [`init_config_toml`] / [`init_config_json`] emit a
//! fully-commented example file documenting every field with its default — the user on-ramp.

use crate::adapters::AdapterSpec;
use crate::config::ExecutionConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// A complete backtest run specification, loadable from a TOML or JSON file.
///
/// All fields are optional in the file (serde `default`), so a minimal config only needs to set
/// what differs from the defaults. The `execution` table is the full [`ExecutionConfig`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RunSpec {
    /// Data source: `"ndjson"` or `"clickhouse"` (both tick-level).
    pub source: String,
    /// NDJSON(.gz) path (used when `source = "ndjson"`).
    pub ndjson: Option<String>,
    /// ClickHouse base URL like `http://localhost:8123` (used when `source = "clickhouse"`).
    pub clickhouse: Option<String>,
    /// Optional ClickHouse schema-map file (JSON or TOML).
    pub ch_config: Option<String>,
    /// Instrument glob (exact, or a trailing `%`/`*` prefix match). None = all instruments.
    pub instrument: Option<String>,
    /// Inclusive start date `YYYY-MM-DD` (None = from the beginning of the data).
    pub start: Option<String>,
    /// Exclusive end date `YYYY-MM-DD` (None = to the end of the data).
    pub end: Option<String>,
    /// Strategy name (see `list-strategies`).
    pub strategy: String,
    /// Tunable strategy parameters, `name -> value` (e.g. `up_ticks = 2.0`). Keys match the names
    /// shown by `list-strategies`; any unset key uses the strategy's built-in default, so an empty
    /// map reproduces current behaviour exactly. CLI `--strategy-param key=value` overrides these.
    #[serde(default)]
    pub strategy_params: BTreeMap<String, f64>,
    /// Opening cash balance.
    pub starting_balance: f64,
    /// Tearsheet HTML output path.
    pub tearsheet: Option<String>,
    /// Directory for structured dashboard exports (report.json, equity.csv, …). None = skip.
    pub out_dir: Option<String>,
    /// MULTI-VENUE: a list of adapter-backed sources whose events are MERGED (time-ordered) into one
    /// backtest. When non-empty, this supersedes the single `source`/path fields above and enables
    /// cross-venue strategies. Each entry names an adapter (see the [`crate::adapters`] registry), the
    /// venue tag to stamp, the path, and optional filters/field-mapping. Empty = single-source run.
    #[serde(default)]
    pub sources: Vec<AdapterSpec>,
    /// Embedded execution-realism config (fees/latency/slippage/rewards).
    pub execution: ExecutionConfig,
}

impl Default for RunSpec {
    fn default() -> Self {
        RunSpec {
            source: "ndjson".into(),
            ndjson: None,
            clickhouse: None,
            ch_config: None,
            instrument: None,
            start: None,
            end: None,
            strategy: "market_maker".into(),
            strategy_params: BTreeMap::new(),
            starting_balance: 1000.0,
            tearsheet: None,
            out_dir: None,
            sources: Vec::new(),
            execution: ExecutionConfig::default(),
        }
    }
}

impl RunSpec {
    /// Load a [`RunSpec`] from a file. `.toml` is parsed as TOML; anything else as JSON. Friendly
    /// errors via [`anyhow::Context`] for missing files / parse failures.
    pub fn from_path(path: &Path) -> Result<RunSpec> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read --config file {}", path.display()))?;
        Self::from_str_auto(&text, path)
            .with_context(|| format!("could not parse --config file {}", path.display()))
    }

    /// Parse from a string, choosing TOML vs JSON by the path's extension (default JSON).
    pub fn from_str_auto(text: &str, path: &Path) -> Result<RunSpec> {
        let is_toml = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("toml"))
            .unwrap_or(false);
        if is_toml {
            toml::from_str(text).map_err(|e| anyhow::anyhow!("invalid TOML: {e}"))
        } else {
            serde_json::from_str(text).map_err(|e| anyhow::anyhow!("invalid JSON: {e}"))
        }
    }
}

/// A fully-commented example `run.toml` documenting every field with its default. Written by
/// `kalshi-backtest init-config <path>`.
pub fn init_config_toml() -> String {
    // Hand-written (rather than serialized) so we can document every field inline with comments.
    r#"# ============================================================================
# kalshi-backtest run specification (TOML)
#
#   Run it with:   kalshi-backtest backtest --config this-file.toml
#
# Every field is OPTIONAL and shown with its DEFAULT. Delete what you don't need.
# Any value here can be overridden on the command line, e.g. --strategy momentum.
# ============================================================================

# Data source (both TICK-LEVEL): "ndjson" | "clickhouse"
source = "ndjson"

# --- source paths/urls (set the one matching `source`) ---
# ndjson(.gz) tick capture from the collector (for source = "ndjson")
ndjson = "../data/tick/natgas_tick_demo.ndjson.gz"
# ClickHouse base URL (for source = "clickhouse"); reads kalshi.orderbook_deltas / trades
# clickhouse = "http://localhost:8123"
# optional ClickHouse schema-map file (JSON or TOML)
# ch_config = "../clickhouse/schema_map.example.json"

# Instrument glob: exact id, or a trailing % / * prefix wildcard. Omit for ALL instruments.
# instrument = "KXNATGASD-%"

# Date range (UTC). Omit either bound to run open-ended.
# start = "2026-06-04"   # inclusive YYYY-MM-DD
# end   = "2026-06-05"   # exclusive YYYY-MM-DD

# Strategy (run `kalshi-backtest list-strategies` to see them all).
strategy = "market_maker"

# Tunable strategy parameters (name = value). Keys are the ones shown by `list-strategies`
# for the chosen strategy; any key you omit uses that strategy's built-in default, so an
# empty table reproduces default behaviour exactly. Override from the CLI with repeatable
# --strategy-param key=value (CLI wins over this table). Example for market_maker:
# [strategy_params]
# half_spread_cents = 3
# quote_size = 20
# Example for avellaneda_stoikov:
# [strategy_params]
# gamma = 0.5
# kappa = 1.5
# sigma_window = 30

# Opening cash balance.
starting_balance = 1000.0

# Where to write the standalone HTML tearsheet.
# tearsheet = "../figures/tearsheet.html"

# Directory for structured dashboard exports (report.json, equity.csv, fills.csv, ...). Omit to skip.
# out_dir = "../figures/run1"

# ----------------------------------------------------------------------------
# MULTI-VENUE sources (optional). When present, these adapter-backed sources are
# MERGED (time-ordered) into ONE backtest, superseding the single `source` above —
# this is what enables cross-venue strategies like `cross_venue_arb`.
#
# Each [[sources]] entry: which adapter to use, the venue tag to stamp on its events,
# the path, and optional instrument/date filters. Run `list-strategies` for strategies
# and see src/adapters/ for adapter keys: kalshi_ndjson, generic_ndjson,
# generic_csv, polymarket, hyperliquid.
#
# [[sources]]
# adapter = "generic_ndjson"
# venue = "KALSHI"
# path = "../data/kalshi_syma.ndjson"
# instrument = "SYMA"        # matches the venue-native symbol (before VENUE: tagging)
#
# [[sources]]
# adapter = "generic_ndjson"
# venue = "POLYMARKET"
# path = "../data/poly_symb.ndjson"
# # generic adapters accept a column/field mapping so a new venue needs zero Rust:
# # [sources.mapping]
# # ts_ns = "timestamp"
# # instrument = "symbol"
# # price = "px"

# ----------------------------------------------------------------------------
# Execution realism. With everything at its default this section is a NO-OP
# (no latency, no slippage, fees ON, rewards OFF) so runs reproduce exactly.
# ----------------------------------------------------------------------------
[execution]
# Charge trading fees to PnL/cash. false = fees still recorded on fills but not charged.
include_fees = true
# Credit accrued Kalshi liquidity rewards into PnL/ending balance.
include_rewards = false

[execution.latency]
enabled = false                  # master switch; false => all latencies are zero
order_latency_ns = 0             # ns from placing an order to it becoming matchable (base/mean)
cancel_latency_ns = 0            # ns from requesting a cancel to it taking effect (flat; no dist)
market_data_latency_ns = 0       # ns the strategy's book view lags reality (added under every dist)
jitter_ns = 0                    # deterministic per-order activation jitter magnitude (no RNG; used by the `fixed` dist)
seed = 0                         # PRNG seed for the stochastic dists (same seed => identical run); unused by `fixed`
# STOCHASTIC LATENCY: model order latency as a DISTRIBUTION instead of a constant, to stress-test
# whether an edge survives realistic latency VARIANCE. The per-order latency is drawn from a SEEDED
# PRNG (see `seed` above) so a run is reproducible GIVEN the seed. The chosen distribution REPLACES
# the `order_latency_ns + jitter` term; `market_data_latency_ns` is still added on top. Tagged by
# `kind`. Omit this `[execution.latency.dist]` block for `fixed` (the default = today's behaviour).
# Pick ONE of:
#   [execution.latency.dist]            #   uniform in [min_ns, max_ns]
#   kind = "uniform"
#   min_ns = 200000000
#   max_ns = 800000000
#   [execution.latency.dist]            #   normal(mean_ns, std_ns), clamped >= 0
#   kind = "normal"
#   mean_ns = 500000000
#   std_ns  = 300000000
#   [execution.latency.dist]            #   exponential with the given mean (heavy-ish tail)
#   kind = "exponential"
#   mean_ns = 500000000
#   [execution.latency.dist]            #   replay measured latencies from a file (sample w/ replacement)
#   kind = "empirical"
#   path = "../data/kalshi_rest_latency_ns.txt"
[execution.latency.dist]
kind = "fixed"                   # default: deterministic order_latency_ns + jitter (no RNG)

[execution.slippage]
enabled = false                  # master switch; false => fills are exact
taker_ticks = 0                  # extra adverse cents on each taker fill
taker_bps = 0.0                  # extra adverse cost as a fraction of notional (0.0005 = 5 bps)
maker_adverse_selection_bps = 0.0 # adverse-selection cost on maker fills (fraction of notional)

[execution.rewards]
enabled = false                  # master switch; false => no rewards accrue
period_secs = 3600               # length of one reward window (seconds)
reward_per_period = 0.0          # dollars paid per period when fully qualifying
min_resting_size = 10.0          # min resting contracts (per required side) to qualify
max_spread_cents = 4             # quotes must rest within this many cents of the mid
both_sides_required = true       # must quote BOTH sides simultaneously to earn

# ----------------------------------------------------------------------------
# Maker-queue model. We can't observe our true FIFO position from L2 data, so
# these two settings bracket the extremes. Default `pessimistic` reproduces the
# original behaviour exactly. CLI flag --queue-model overrides this.
# ----------------------------------------------------------------------------
[execution.queue]
# pessimistic = cancellations ahead of you DON'T help; queue only burns down on
#               trades (conservative; you fill the latest). DEFAULT.
# optimistic  = when the resting size at YOUR price level shrinks via a cancel,
#               your queue_ahead drops by that amount (assume the cancelled size
#               was ahead of you), so you fill sooner.
model = "pessimistic"

# ----------------------------------------------------------------------------
# HARD, engine-enforced risk limits. Every key is OPTIONAL and DISABLED (None)
# by default, so omitting this whole block changes nothing. Order/position caps
# CLAMP outgoing orders (a position-reducing/flattening order is NEVER blocked);
# the equity limits HALT the run on breach (cancel all resting orders, flatten
# every position with market orders that BYPASS latency, then ignore all further
# strategy orders). CLI flags --max-order-qty / --max-position / --max-gross /
# --equity-floor / --max-drawdown-pct override these.
# ----------------------------------------------------------------------------
[execution.risk]
# max_order_qty = 100            # cap a single order's contract qty
# max_position_per_instrument = 500  # cap |net qty| for any one instrument
# max_gross_position = 1000      # cap Σ|net qty| across all instruments
# equity_floor = 0.0             # HALT if equity <= this at any step
# max_drawdown_pct = 50.0        # HALT if drawdown from the equity peak reaches this %

# ----------------------------------------------------------------------------
# BINARY SETTLEMENT AT EXPIRY (optional). Kalshi markets are cash-or-nothing:
# at resolution a YES contract pays $1 and a NO contract pays $0. Set `path` to
# a settlement file and any position still HELD at end-of-run whose instrument
# has a KNOWN outcome is SETTLED to its $1/$0 payoff (no fee) instead of being
# flattened at the last mid. Instruments NOT in the file still flatten at mid.
# Omitting this block (path unset) => flatten-at-mid as before (the default).
# The file is CSV `instrument_id,result` (result in yes/no/1/0/true/false/Y/N)
# or JSON ({"INST":"yes",...} or [{"instrument_id":"INST","result":"yes"}]).
# Build one from real Kalshi data with ../collector/fetch_settlements.py.
# CLI flag --settlements <path> overrides this.
# ----------------------------------------------------------------------------
[execution.settlement]
# path = "../data/settlements.csv"
"#
    .to_string()
}

/// A commented example `run.json` (JSON can't carry comments, so the documentation lives in
/// `_comment` keys that serde ignores). Written by `init-config --json`.
pub fn init_config_json() -> String {
    r#"{
  "_comment": "kalshi-backtest run spec (JSON). Run: kalshi-backtest backtest --config this.json",
  "_comment_source": "Data source (both tick-level): ndjson | clickhouse",
  "source": "ndjson",
  "ndjson": "../data/tick/natgas_tick_demo.ndjson.gz",
  "_comment_ndjson": "ndjson(.gz) tick capture path for source=ndjson",
  "_comment_clickhouse": "ClickHouse base url for source=clickhouse (kalshi.orderbook_deltas / trades), e.g. http://localhost:8123",
  "_comment_instrument": "Instrument glob (exact or trailing % / * prefix); omit for all",
  "instrument": null,
  "_comment_dates": "start inclusive, end exclusive, YYYY-MM-DD; null = open-ended",
  "start": null,
  "end": null,
  "strategy": "market_maker",
  "_comment_strategy_params": "Tunable strategy params (name->value). Keys from `list-strategies`; omitted keys use the strategy default (empty == default behaviour). CLI --strategy-param key=value overrides these.",
  "strategy_params": {},
  "_example_strategy_params": { "half_spread_cents": 3, "quote_size": 20 },
  "starting_balance": 1000.0,
  "_comment_outputs": "tearsheet HTML path and dashboard export dir; null to skip exports",
  "tearsheet": null,
  "out_dir": null,
  "_comment_sources": "MULTI-VENUE: adapter-backed sources merged time-ordered into one run (supersedes `source`). Enables cross-venue strategies. Adapter keys: kalshi_ndjson, generic_ndjson, generic_csv, polymarket, hyperliquid.",
  "sources": [],
  "_example_sources": [
    { "adapter": "generic_ndjson", "venue": "KALSHI", "path": "../data/kalshi_syma.ndjson", "instrument": "SYMA" },
    { "adapter": "generic_ndjson", "venue": "POLYMARKET", "path": "../data/poly_symb.ndjson", "mapping": { "ts_ns": "timestamp", "instrument": "symbol", "price": "px" } }
  ],
  "_comment_execution": "Execution realism. All-default = no-op (no latency/slippage, fees on, rewards off).",
  "execution": {
    "include_fees": true,
    "include_rewards": false,
    "_comment_latency": "Latency model. order_latency_ns is the base/mean; jitter_ns is the deterministic (no-RNG) per-order spread used by the default 'fixed' dist. STOCHASTIC LATENCY: set 'dist' to model order latency as a DISTRIBUTION drawn from a SEEDED PRNG ('seed'), so a run reproduces given the seed and you can stress-test latency VARIANCE. The dist is tagged by 'kind' and REPLACES the order_latency_ns+jitter term; market_data_latency_ns is still added on top. Kinds: {\"kind\":\"fixed\"} (default == today), {\"kind\":\"uniform\",\"min_ns\":..,\"max_ns\":..}, {\"kind\":\"normal\",\"mean_ns\":..,\"std_ns\":..}, {\"kind\":\"exponential\",\"mean_ns\":..}, {\"kind\":\"empirical\",\"path\":\"samples.txt\"} (sample with replacement; falls back to fixed if missing). CLI: --latency-dist/--latency-min-ns/--latency-max-ns/--latency-std-ns/--latency-mean-ns/--latency-empirical/--latency-seed.",
    "latency": {
      "enabled": false,
      "order_latency_ns": 0,
      "cancel_latency_ns": 0,
      "market_data_latency_ns": 0,
      "jitter_ns": 0,
      "dist": { "kind": "fixed" },
      "seed": 0
    },
    "slippage": {
      "enabled": false,
      "taker_ticks": 0,
      "taker_bps": 0.0,
      "maker_adverse_selection_bps": 0.0
    },
    "rewards": {
      "enabled": false,
      "period_secs": 3600,
      "reward_per_period": 0.0,
      "min_resting_size": 10.0,
      "max_spread_cents": 4,
      "both_sides_required": true
    },
    "_comment_queue": "Maker-queue model. We can't observe true FIFO position from L2 data; these bracket the extremes. 'pessimistic' (default) = cancellations ahead don't help, queue burns only on trades (== original behaviour). 'optimistic' = a cancellation at your price level moves you up the queue. CLI: --queue-model.",
    "queue": {
      "model": "pessimistic"
    },
    "_comment_risk": "HARD engine-enforced risk limits. Each key is null (disabled) by default => no-op. Order/position caps CLAMP outgoing orders (flattening is never blocked); equity_floor / max_drawdown_pct HALT the run (cancel + flatten bypassing latency, then ignore further orders). CLI: --max-order-qty/--max-position/--max-gross/--equity-floor/--max-drawdown-pct.",
    "risk": {
      "max_order_qty": null,
      "max_position_per_instrument": null,
      "max_gross_position": null,
      "equity_floor": null,
      "max_drawdown_pct": null
    },
    "_comment_settlement": "BINARY SETTLEMENT AT EXPIRY. Kalshi is cash-or-nothing: a YES pays $1, a NO pays $0. Set settlement.path to a settlement file and any position held at end-of-run whose instrument has a KNOWN outcome is SETTLED to $1/$0 (no fee) instead of flattened at mid; instruments absent from the file still flatten at mid. path=null (default) => flatten-at-mid as before. File = CSV 'instrument_id,result' (yes/no/1/0/true/false) or JSON object/array. Build one with ../collector/fetch_settlements.py. CLI: --settlements <path>.",
    "settlement": {
      "path": null
    }
  }
}
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn toml_roundtrip_preserves_fields() {
        let spec = RunSpec {
            source: "ndjson".into(),
            ndjson: Some("data.ndjson.gz".into()),
            instrument: Some("KX%".into()),
            strategy: "momentum".into(),
            starting_balance: 5000.0,
            ..Default::default()
        };
        let s = toml::to_string(&spec).unwrap();
        let back: RunSpec = RunSpec::from_str_auto(&s, Path::new("x.toml")).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn json_roundtrip_preserves_fields() {
        let spec = RunSpec {
            source: "ndjson".into(),
            ndjson: Some("cap.ndjson.gz".into()),
            strategy: "mean_reversion".into(),
            starting_balance: 2500.0,
            ..Default::default()
        };
        let s = serde_json::to_string(&spec).unwrap();
        let back: RunSpec = RunSpec::from_str_auto(&s, Path::new("x.json")).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        // Only set a couple of fields; serde defaults the rest from RunSpec::default().
        let text = r#"
            source = "ndjson"
            strategy = "breakout"
        "#;
        let spec = RunSpec::from_str_auto(text, Path::new("p.toml")).unwrap();
        assert_eq!(spec.strategy, "breakout");
        assert_eq!(spec.starting_balance, 1000.0); // defaulted
        assert!(spec.execution.include_fees); // defaulted true
        assert!(!spec.execution.rewards.enabled);
    }

    #[test]
    fn partial_json_fills_defaults_and_reads_execution() {
        let text = r#"{ "strategy": "imbalance",
                        "execution": { "include_rewards": true,
                                       "rewards": { "reward_per_period": 7.5 } } }"#;
        let spec = RunSpec::from_str_auto(text, Path::new("p.json")).unwrap();
        assert_eq!(spec.strategy, "imbalance");
        assert_eq!(spec.source, "ndjson"); // defaulted
        assert!(spec.execution.include_rewards);
        assert_eq!(spec.execution.rewards.reward_per_period, 7.5);
        assert!(spec.execution.include_fees); // defaulted true
    }

    #[test]
    fn emitted_example_toml_parses() {
        let text = init_config_toml();
        let spec = RunSpec::from_str_auto(&text, Path::new("run.toml")).unwrap();
        assert_eq!(spec.source, "ndjson");
        assert_eq!(spec.strategy, "market_maker");
        assert!(spec.execution.include_fees);
    }

    #[test]
    fn emitted_example_json_parses() {
        let text = init_config_json();
        let spec = RunSpec::from_str_auto(&text, Path::new("run.json")).unwrap();
        assert_eq!(spec.source, "ndjson");
        assert_eq!(spec.strategy, "market_maker");
        assert!(!spec.execution.include_rewards);
    }

    #[test]
    fn strategy_params_roundtrip_and_default_empty() {
        // Default spec has an empty param map.
        assert!(RunSpec::default().strategy_params.is_empty());
        // A TOML table is parsed into the map.
        let text = r#"
            strategy = "market_maker"
            [strategy_params]
            half_spread_cents = 3.0
            quote_size = 20.0
        "#;
        let spec = RunSpec::from_str_auto(text, Path::new("p.toml")).unwrap();
        assert_eq!(spec.strategy_params.get("half_spread_cents"), Some(&3.0));
        assert_eq!(spec.strategy_params.get("quote_size"), Some(&20.0));
        // Roundtrips through JSON.
        let s = serde_json::to_string(&spec).unwrap();
        let back: RunSpec = RunSpec::from_str_auto(&s, Path::new("x.json")).unwrap();
        assert_eq!(back.strategy_params, spec.strategy_params);
    }

    #[test]
    fn bad_toml_reports_friendly_error() {
        let err = RunSpec::from_str_auto("this is = = not toml", Path::new("bad.toml")).unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("toml"));
    }
}
