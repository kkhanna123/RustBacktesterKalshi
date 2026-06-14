//! `kalshi-backtest` CLI — a discoverable, ergonomic front-end to the tick-level Kalshi backtester.
//!
//! Subcommands (see `--help`):
//!   * `backtest`         — run a backtest (flags, or a `--config run.toml`/`.json` run spec).
//!   * `list-strategies`  — every strategy with a one-line description and its key params.
//!   * `list-instruments` — distinct instruments in a data source (+ row counts and time span).
//!   * `describe-data`     — a quick "what's in here" summary of a data source.
//!   * `init-config`      — write a fully-commented example run spec to copy-edit.
//!
//! Design: a [`Cli`] struct holds global flags + a [`Commands`] enum; one handler fn per
//! subcommand. Failures are wrapped with [`anyhow::Context`] for clear, actionable messages, and
//! arguments are validated early. The machine-readable `report.json` still prints between sentinels
//! on STDOUT; the friendly human summary goes to STDERR so STDOUT stays parseable.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

use kalshi_backtester::adapters::{profile::AdapterProfile, AdapterRegistry, AdapterSpec};
use kalshi_backtester::config::{BacktestConfig, ExecutionConfig, LatencyDist, QueueModel};
use kalshi_backtester::data;
use kalshi_backtester::engine::Engine;
use kalshi_backtester::exports::{write_exports, ExportMeta};
use kalshi_backtester::optimize::{
    best_index, expand_grid, parse_param_axis, run_grid, split_segments, Combo, Metric, ParamAxis,
};
use kalshi_backtester::report::{print_report, print_tearsheet_b64, write_tearsheet_html};
use kalshi_backtester::runspec::{init_config_json, init_config_toml, RunSpec};
use kalshi_backtester::strategies;
use kalshi_backtester::types::{MarketEvent, Report};

const AFTER_HELP: &str = "\
EXAMPLES:
  # 1. Quickest possible run — a TICK NDJSON capture + a strategy, everything else defaults:
  kalshi-backtest backtest --source ndjson --ndjson ../data/tick/natgas_tick_demo.ndjson.gz --strategy imbalance

  # 2. Model realistic LATENCY — orders only fill at order_send + latency (the headline feature):
  kalshi-backtest backtest --source ndjson --ndjson ../data/tick/natgas_tick_demo.ndjson.gz \\
      --instrument 'KXNATGASD-%' --strategy imbalance --latency-ns 1000000000

  # 3. Reproduce a whole run from a config file (make one with `init-config`):
  kalshi-backtest init-config run.toml
  kalshi-backtest backtest --config run.toml

  # 4. Same, but override one field from the CLI (flags beat the file):
  kalshi-backtest backtest --config run.toml --strategy momentum --starting-balance 5000

  # 5. With the Kalshi liquidity-rewards model enabled:
  kalshi-backtest backtest --source ndjson --ndjson cap.ndjson.gz --strategy market_maker \\
      --rewards --reward-per-period 5 --max-spread-cents 4

  # 6. From ClickHouse (build once with the feature), filtered to a series and date range:
  cargo build --release --features clickhouse
  kalshi-backtest backtest --source clickhouse --clickhouse http://localhost:8123 \\
      --instrument 'KXNATGASD-%' --start 2026-06-04 --end 2026-06-05 --strategy imbalance

  # 7. CROSS-VENUE: merge two venues via the generic adapters and trade the spread:
  kalshi-backtest backtest \\
      --source adapter --adapter generic_ndjson --venue KALSHI --adapter-path kalshi_syma.ndjson \\
      --extra-source 'adapter=generic_ndjson,venue=POLYMARKET,path=poly_symb.ndjson' \\
      --strategy cross_venue_arb
  # (or list multiple [[sources]] in a --config run.toml; see `init-config`)

  # Discover what's available:
  kalshi-backtest list-strategies
  kalshi-backtest describe-data    --source ndjson --ndjson cap.ndjson.gz
  kalshi-backtest list-instruments --source ndjson --ndjson cap.ndjson.gz
";

/// Top-level CLI: global verbosity flags + a subcommand.
#[derive(Parser)]
#[command(
    name = "kalshi-backtest",
    about = "Tick-level Kalshi backtester — backtest strategies over NDJSON tick captures or ClickHouse, with a realistic latency fill model.",
    version,
    after_help = AFTER_HELP
)]
struct Cli {
    /// Increase log verbosity (progress + per-instrument detail). Repeatable: -vv.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Near-silent: suppress the informational STDERR logs (errors still print). Wins over -v.
    #[arg(short, long, global = true, default_value_t = false)]
    quiet: bool,

    #[command(subcommand)]
    cmd: Commands,
}

/// Resolved log level, derived from `-v`/`-q`. Controls only the human STDERR logging; STDOUT
/// (report JSON) is unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verbosity {
    Quiet,
    Normal,
    Verbose,
}

impl Cli {
    fn verbosity(&self) -> Verbosity {
        if self.quiet {
            Verbosity::Quiet
        } else if self.verbose > 0 {
            Verbosity::Verbose
        } else {
            Verbosity::Normal
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Run a backtest over a data source with a chosen strategy.
    Backtest(BacktestArgs),
    /// PARALLEL PARAMETER OPTIMIZER: sweep a strategy's parameter grid over ONE parsed dataset and
    /// rank every combination by a chosen metric. The expensive gzip+JSON parse happens exactly
    /// ONCE; every combo runs against the same in-memory events, spread across worker threads, with
    /// results reported in deterministic config order. See `optimize --help`.
    Optimize(OptimizeArgs),
    /// WALK-FORWARD ANALYZER: split the dataset into K+1 time-ordered folds; for each fold optimize
    /// the param grid on the training segment, then measure the chosen config's OUT-OF-SAMPLE
    /// performance on the next segment. Reports per-fold + an aggregate "does it generalize" number.
    /// Reuses the optimizer's parse-once + parallel machinery. See `walk-forward --help`.
    WalkForward(WalkForwardArgs),
    /// List every available strategy with a one-line description and key parameters.
    ListStrategies,
    /// List every registered DATA ADAPTER: name, default venue, description, and the mapping keys it
    /// understands (with defaults). The discoverability companion to `list-strategies`.
    ListAdapters,
    /// VALIDATE + PREVIEW a data source before a full run: load it with the real loaders, print a
    /// summary, show the first N parsed events in human form, and run data-quality WARNINGS so you can
    /// eyeball that your mapping/profile is right. Exits 0 unless the source can't be loaded at all.
    Validate(ValidateArgs),
    /// List the distinct instruments in a data source (with row counts and time span).
    ListInstruments(DataSourceArgs),
    /// Summarize a data source: rows/events, instruments, time span, snapshots/deltas/trades.
    DescribeData(DataSourceArgs),
    /// Write a fully-commented example run-config file to copy-edit (TOML by default, or --json).
    InitConfig(InitConfigArgs),
}

/// Shared data-source selection flags, reused by `list-instruments` and `describe-data`.
#[derive(Args, Clone)]
struct DataSourceArgs {
    /// Data source (both tick-level).
    #[arg(long, value_parser = ["ndjson", "clickhouse"])]
    source: String,

    /// NDJSON(.gz) tick capture path (for --source ndjson).
    #[arg(long)]
    ndjson: Option<PathBuf>,

    /// ClickHouse base URL like http://localhost:8123 (for --source clickhouse).
    #[arg(long)]
    clickhouse: Option<String>,

    /// Optional ClickHouse schema-map file (JSON or TOML).
    #[arg(long)]
    ch_config: Option<PathBuf>,

    /// Instrument glob (exact, or trailing `%`/`*` prefix match). Default: all.
    #[arg(long)]
    instrument: Option<String>,

    /// Inclusive start date YYYY-MM-DD.
    #[arg(long)]
    start: Option<String>,

    /// Exclusive end date YYYY-MM-DD.
    #[arg(long)]
    end: Option<String>,
}

/// Arguments for `validate` — the transparency/debug centerpiece. Mirrors the data-source flags of
/// `backtest` (ndjson / clickhouse / adapter, with `--adapter-profile`) plus a `--preview` count.
#[derive(Args, Clone)]
struct ValidateArgs {
    /// Data source: `ndjson`, `clickhouse`, or `adapter` (resolve a venue adapter via the registry).
    #[arg(long, value_parser = ["ndjson", "clickhouse", "adapter"])]
    source: String,

    /// NDJSON(.gz) tick capture path (for --source ndjson).
    #[arg(long)]
    ndjson: Option<PathBuf>,

    /// ClickHouse base URL like http://localhost:8123 (for --source clickhouse).
    #[arg(long)]
    clickhouse: Option<String>,

    /// Optional ClickHouse schema-map file (JSON or TOML).
    #[arg(long)]
    ch_config: Option<PathBuf>,

    /// Adapter key for `--source adapter` (e.g. generic_ndjson, generic_csv). See `list-adapters`.
    #[arg(long)]
    adapter: Option<String>,

    /// Venue tag to stamp on `--source adapter` events (e.g. KALSHI). Defaults to the adapter's venue.
    #[arg(long)]
    venue: Option<String>,

    /// Path the chosen adapter reads (for `--source adapter`).
    #[arg(long)]
    adapter_path: Option<PathBuf>,

    /// Inline generic-adapter mapping `key=value` (repeatable), e.g. `--map price=px --map ts_unit=ms`.
    /// MERGES OVER an `--adapter-profile`. See `list-adapters` for the keys each adapter understands.
    #[arg(long = "map", value_name = "KEY=VALUE")]
    map: Vec<String>,

    /// Reusable adapter mapping/profile file (JSON or TOML) for the generic adapters (see
    /// `--adapter-profile` on `backtest`). CLI flags / `--map` override file values.
    #[arg(long)]
    adapter_profile: Option<PathBuf>,

    /// Instrument glob (exact, or trailing `%`/`*` prefix match). Default: all.
    #[arg(long)]
    instrument: Option<String>,

    /// Inclusive start date YYYY-MM-DD.
    #[arg(long)]
    start: Option<String>,

    /// Exclusive end date YYYY-MM-DD.
    #[arg(long)]
    end: Option<String>,

    /// How many parsed events to PREVIEW in human-readable form (default 10).
    #[arg(long, default_value_t = 10)]
    preview: usize,
}

/// Arguments for `init-config`.
#[derive(Args)]
struct InitConfigArgs {
    /// Where to write the example config (e.g. run.toml). The extension picks the format unless
    /// --json is given. Parent directories are created if needed.
    path: PathBuf,

    /// Emit JSON instead of TOML (also implied by a `.json` path extension).
    #[arg(long, default_value_t = false)]
    json: bool,

    /// Overwrite the file if it already exists.
    #[arg(long, default_value_t = false)]
    force: bool,
}

/// CLI choice for the maker-queue model (`--queue-model`). Maps to [`QueueModel`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum QueueModelArg {
    /// Cancellations ahead don't help; queue burns only on trades (default = original behaviour).
    Pessimistic,
    /// A cancellation at your price level moves you up the queue.
    Optimistic,
}

impl From<QueueModelArg> for QueueModel {
    fn from(a: QueueModelArg) -> Self {
        match a {
            QueueModelArg::Pessimistic => QueueModel::Pessimistic,
            QueueModelArg::Optimistic => QueueModel::Optimistic,
        }
    }
}

/// CLI choice for the STOCHASTIC latency distribution (`--latency-dist`). Maps (with the param flags)
/// to a [`LatencyDist`]. `fixed` is the default deterministic hash-jitter model.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum LatencyDistArg {
    /// Default — deterministic `order_latency_ns + hash_jitter` (no RNG; byte-for-byte the original).
    Fixed,
    /// Uniform in `[--latency-min-ns, --latency-max-ns]`.
    Uniform,
    /// Normal(`--latency-ns` mean, `--latency-std-ns`), clamped ≥ 0.
    Normal,
    /// Exponential with mean `--latency-mean-ns` (or `--latency-ns`).
    Exponential,
    /// Replay measured samples from `--latency-empirical <path>` (sample with replacement).
    Empirical,
}

#[derive(Args)]
struct BacktestArgs {
    /// Load a full run spec (source, instrument, strategy, dates, starting_balance, and the
    /// embedded execution config) from a TOML or JSON file. Individual CLI flags below OVERRIDE
    /// matching fields of the file. Make one with `kalshi-backtest init-config run.toml`.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Data source (tick-level). Required unless provided by --config. `adapter` resolves a venue
    /// adapter via the registry (use with --venue/--adapter-path), enabling MULTI-VENUE runs.
    #[arg(long, value_parser = ["ndjson", "clickhouse", "adapter"])]
    source: Option<String>,

    /// Adapter key for `--source adapter` (e.g. generic_ndjson, generic_csv, polymarket,
    /// hyperliquid, kalshi_ndjson). See `list-strategies`/docs for the full set.
    #[arg(long)]
    adapter: Option<String>,

    /// Venue tag to stamp on `--source adapter` events (e.g. KALSHI, POLYMARKET). Defaults to the
    /// adapter's natural venue.
    #[arg(long)]
    venue: Option<String>,

    /// Path/URL the chosen adapter reads (for `--source adapter`).
    #[arg(long)]
    adapter_path: Option<PathBuf>,

    /// Reusable adapter mapping/PROFILE file (JSON or TOML) for the generic CSV/NDJSON adapters,
    /// applied to the `--source adapter` primary source. Keeps a stable field mapping out of the CLI;
    /// it may also pin adapter/venue/instrument. CLI flags OVERRIDE file values. See `list-adapters`
    /// for the mapping keys, and the docs for the file format (mirrors `tools/to_canonical.py`).
    #[arg(long)]
    adapter_profile: Option<PathBuf>,

    /// Add an EXTRA adapter source to MERGE into the run (repeatable). Format:
    /// `adapter=<key>,venue=<TAG>,path=<file>[,instrument=<glob>]`. Combined with the primary
    /// source, all events are merged time-ordered so a cross-venue strategy sees every venue.
    #[arg(long = "extra-source", value_name = "SPEC")]
    extra_source: Vec<String>,

    /// NDJSON(.gz) tick capture path (for --source ndjson).
    #[arg(long)]
    ndjson: Option<PathBuf>,

    /// ClickHouse base URL like http://localhost:8123 (for --source clickhouse).
    #[arg(long)]
    clickhouse: Option<String>,

    /// Optional ClickHouse schema-map file (JSON or TOML). Lets you point the loader at a
    /// slightly-different schema (renamed db/tables/columns) without recompiling.
    #[arg(long)]
    ch_config: Option<PathBuf>,

    /// Instrument glob (exact, or trailing `%`/`*` prefix match).
    #[arg(long)]
    instrument: Option<String>,

    /// Inclusive start date YYYY-MM-DD.
    #[arg(long)]
    start: Option<String>,

    /// Exclusive end date YYYY-MM-DD.
    #[arg(long)]
    end: Option<String>,

    /// Strategy name (see `list-strategies`). Required unless provided by --config.
    #[arg(long, value_parser = strategies::ALL.to_vec())]
    strategy: Option<String>,

    /// Tune a strategy parameter: `key=value` (repeatable). Keys are the ones shown by
    /// `list-strategies`, e.g. `--strategy-param half_spread_cents=3`. These MERGE OVER and OVERRIDE
    /// any `[strategy_params]` from a --config file. Alias: `--param`.
    #[arg(long = "strategy-param", visible_alias = "param", value_name = "KEY=VALUE")]
    strategy_param: Vec<String>,

    /// Starting balance.
    #[arg(long)]
    starting_balance: Option<f64>,

    /// Tearsheet HTML output path.
    #[arg(long)]
    tearsheet: Option<PathBuf>,

    /// Also print the tearsheet HTML as base64 between sentinels.
    #[arg(long, default_value_t = false)]
    emit_tearsheet_b64: bool,

    /// Directory for structured dashboard exports. When set, writes report.json, equity.csv,
    /// fills.csv, trades.csv, round_trips.csv, instrument_stats.csv, and meta.json into it.
    #[arg(long)]
    out_dir: Option<PathBuf>,

    // ---- execution-realism toggles (all default to current behaviour) ----
    /// Load a whole ExecutionConfig from a JSON file (a reusable preset). Individual flags below
    /// override fields of the loaded preset. (`--config` is the newer, fuller alternative.)
    #[arg(long)]
    exec_config: Option<PathBuf>,

    /// Exclude trading fees from PnL (fees still recorded on fills, but not charged to cash).
    #[arg(long, default_value_t = false)]
    no_fees: bool,

    /// Enable the Kalshi liquidity-rewards model and credit accrued rewards to the ending balance.
    #[arg(long, default_value_t = false)]
    rewards: bool,
    /// Reward pool paid per period (dollars).
    #[arg(long)]
    reward_per_period: Option<f64>,
    /// Reward period length in seconds.
    #[arg(long)]
    reward_period_secs: Option<i64>,
    /// Minimum resting size (contracts) to qualify for rewards.
    #[arg(long)]
    min_resting_size: Option<f64>,
    /// Max distance from mid (cents) a quote may sit and still qualify for rewards.
    #[arg(long)]
    max_spread_cents: Option<i32>,

    /// Order latency in nanoseconds (enables the latency model when > 0).
    #[arg(long)]
    latency_ns: Option<i64>,
    /// Cancel latency in nanoseconds.
    #[arg(long)]
    cancel_latency_ns: Option<i64>,
    /// Market-data latency in nanoseconds.
    #[arg(long)]
    md_latency_ns: Option<i64>,
    /// Deterministic activation jitter magnitude in nanoseconds (used by the `fixed` distribution).
    #[arg(long)]
    jitter_ns: Option<i64>,

    // ---- STOCHASTIC (distributional) latency ----
    /// Model order latency as a DISTRIBUTION (default `fixed` = deterministic hash-jitter, byte-for-
    /// byte the original). Setting any non-fixed value enables the latency model and draws each
    /// order's latency from a SEEDED PRNG so runs vary realistically yet reproduce given the seed.
    /// Params: uniform => --latency-min-ns/--latency-max-ns; normal => --latency-ns (mean) +
    /// --latency-std-ns; exponential => --latency-mean-ns (or --latency-ns); empirical =>
    /// --latency-empirical <file>. CLI overrides [execution.latency].dist from --config.
    #[arg(long, value_enum)]
    latency_dist: Option<LatencyDistArg>,
    /// Min latency (ns) for `--latency-dist uniform`.
    #[arg(long)]
    latency_min_ns: Option<i64>,
    /// Max latency (ns) for `--latency-dist uniform`.
    #[arg(long)]
    latency_max_ns: Option<i64>,
    /// Std-dev (ns) for `--latency-dist normal` (its mean comes from --latency-ns).
    #[arg(long)]
    latency_std_ns: Option<i64>,
    /// Mean latency (ns) for `--latency-dist exponential` (falls back to --latency-ns if unset).
    #[arg(long)]
    latency_mean_ns: Option<i64>,
    /// Path to a newline/CSV file of measured latency-ns samples for `--latency-dist empirical`
    /// (sampled WITH REPLACEMENT; falls back to `fixed` if missing/empty).
    #[arg(long)]
    latency_empirical: Option<PathBuf>,
    /// Seed (u64) for the stochastic-latency PRNG. Same seed => identical run; different seed =>
    /// different but reproducible run. Ignored by the default `fixed` distribution.
    #[arg(long)]
    latency_seed: Option<u64>,

    /// Extra adverse taker slippage in ticks/cents (enables slippage when > 0).
    #[arg(long)]
    slippage_ticks: Option<i32>,
    /// Extra adverse taker slippage as a fraction of notional (bps as a fraction, e.g. 0.0005).
    #[arg(long)]
    slippage_bps: Option<f64>,

    /// Maker-queue model: `pessimistic` (default — cancellations ahead don't help; queue burns only
    /// on trades) or `optimistic` (a cancellation at your price level moves you up the queue).
    /// Overrides [execution.queue].model from --config.
    #[arg(long, value_enum)]
    queue_model: Option<QueueModelArg>,

    /// BINARY SETTLEMENT AT EXPIRY: path to a settlement file mapping each instrument to its
    /// resolved outcome (CSV `instrument_id,result` with result in {yes,no,1,0,true,false,...}, or
    /// JSON object/array). When set, end-of-run positions in instruments with a KNOWN outcome are
    /// SETTLED to their $1 (YES) / $0 (NO) binary payoff instead of being flattened at mid (no
    /// settlement fee). Instruments absent from the file still flatten at mid. Overrides
    /// [execution.settlement].path from --config. See `../collector/fetch_settlements.py` to build one.
    #[arg(long)]
    settlements: Option<PathBuf>,

    // ---- engine-enforced risk limits (all default to OFF / no-op) ----
    /// Cap a single order's contract qty (orders larger than this are clamped down).
    #[arg(long)]
    max_order_qty: Option<f64>,
    /// Cap |net position| for any one instrument (opening orders clamped; flattening always allowed).
    #[arg(long)]
    max_position: Option<f64>,
    /// Cap total |net position| summed across all instruments (gross exposure).
    #[arg(long)]
    max_gross: Option<f64>,
    /// HALT the run if equity ever drops to/below this value (cancels + flattens, ignores later orders).
    #[arg(long)]
    equity_floor: Option<f64>,
    /// HALT the run if drawdown from the equity peak reaches this percent (e.g. 50 = 50%).
    #[arg(long)]
    max_drawdown_pct: Option<f64>,
}

/// Shared execution-realism + risk flags reused by the `optimize` and `walk-forward` orchestration
/// subcommands. These map onto the SAME [`ExecutionConfig`] overrides as `backtest`'s flags (via
/// [`apply_exec_overrides_orch`]), so every run in a sweep gets identical execution realism. Kept as
/// its own `#[command(flatten)]` struct so both orchestration commands stay in sync.
#[derive(Args, Clone)]
struct ExecFlags {
    /// Exclude trading fees from PnL (fees still recorded on fills, but not charged to cash).
    #[arg(long, default_value_t = false)]
    no_fees: bool,

    /// Enable the Kalshi liquidity-rewards model and credit accrued rewards to the ending balance.
    #[arg(long, default_value_t = false)]
    rewards: bool,
    /// Reward pool paid per period (dollars).
    #[arg(long)]
    reward_per_period: Option<f64>,
    /// Reward period length in seconds.
    #[arg(long)]
    reward_period_secs: Option<i64>,
    /// Minimum resting size (contracts) to qualify for rewards.
    #[arg(long)]
    min_resting_size: Option<f64>,
    /// Max distance from mid (cents) a quote may sit and still qualify for rewards.
    #[arg(long)]
    max_spread_cents: Option<i32>,

    /// Order latency in nanoseconds (enables the latency model when > 0).
    #[arg(long)]
    latency_ns: Option<i64>,
    /// Cancel latency in nanoseconds.
    #[arg(long)]
    cancel_latency_ns: Option<i64>,
    /// Market-data latency in nanoseconds.
    #[arg(long)]
    md_latency_ns: Option<i64>,
    /// Deterministic activation jitter magnitude in nanoseconds (used by the `fixed` distribution).
    #[arg(long)]
    jitter_ns: Option<i64>,

    /// Extra adverse taker slippage in ticks/cents (enables slippage when > 0).
    #[arg(long)]
    slippage_ticks: Option<i32>,
    /// Extra adverse taker slippage as a fraction of notional (bps as a fraction, e.g. 0.0005).
    #[arg(long)]
    slippage_bps: Option<f64>,

    /// Maker-queue model: `pessimistic` (default) or `optimistic`.
    #[arg(long, value_enum)]
    queue_model: Option<QueueModelArg>,

    /// BINARY SETTLEMENT path (CSV/JSON instrument->outcome); end-of-run positions in known
    /// instruments settle to their $1/$0 payoff instead of flattening at mid. See `backtest --help`.
    #[arg(long)]
    settlements: Option<PathBuf>,

    // ---- engine-enforced risk limits (all default to OFF / no-op) ----
    /// Cap a single order's contract qty (orders larger than this are clamped down).
    #[arg(long)]
    max_order_qty: Option<f64>,
    /// Cap |net position| for any one instrument (opening orders clamped; flattening always allowed).
    #[arg(long)]
    max_position: Option<f64>,
    /// Cap total |net position| summed across all instruments (gross exposure).
    #[arg(long)]
    max_gross: Option<f64>,
    /// HALT the run if equity ever drops to/below this value (cancels + flattens, ignores later orders).
    #[arg(long)]
    equity_floor: Option<f64>,
    /// HALT the run if drawdown from the equity peak reaches this percent (e.g. 50 = 50%).
    #[arg(long)]
    max_drawdown_pct: Option<f64>,
}

/// Data-source + strategy + grid flags shared by `optimize` and `walk-forward`. The grid is given as
/// one repeatable `--param 'name=v1,v2,v3'` per swept axis; everything else mirrors `backtest`.
#[derive(Args, Clone)]
struct OrchCommonArgs {
    /// Data source (tick-level). `adapter` resolves a venue adapter via the registry (use with
    /// --adapter/--venue/--adapter-path), enabling MULTI-VENUE sweeps.
    #[arg(long, value_parser = ["ndjson", "clickhouse", "adapter"])]
    source: String,

    /// Adapter key for `--source adapter` (e.g. generic_ndjson, polymarket, kalshi_ndjson).
    #[arg(long)]
    adapter: Option<String>,
    /// Venue tag to stamp on `--source adapter` events (e.g. KALSHI, POLYMARKET).
    #[arg(long)]
    venue: Option<String>,
    /// Path/URL the chosen adapter reads (for `--source adapter`).
    #[arg(long)]
    adapter_path: Option<PathBuf>,
    /// Reusable adapter mapping/PROFILE file (JSON or TOML) for the generic adapters, applied to the
    /// `--source adapter` primary source. CLI flags override file values. See `backtest --help`.
    #[arg(long)]
    adapter_profile: Option<PathBuf>,
    /// Add an EXTRA adapter source to MERGE into the run (repeatable). Same format as `backtest`:
    /// `adapter=<key>,venue=<TAG>,path=<file>[,instrument=<glob>]`.
    #[arg(long = "extra-source", value_name = "SPEC")]
    extra_source: Vec<String>,

    /// NDJSON(.gz) tick capture path (for --source ndjson).
    #[arg(long)]
    ndjson: Option<PathBuf>,
    /// ClickHouse base URL like http://localhost:8123 (for --source clickhouse).
    #[arg(long)]
    clickhouse: Option<String>,
    /// Optional ClickHouse schema-map file (JSON or TOML).
    #[arg(long)]
    ch_config: Option<PathBuf>,

    /// Instrument glob (exact, or trailing `%`/`*` prefix match).
    #[arg(long)]
    instrument: Option<String>,
    /// Inclusive start date YYYY-MM-DD.
    #[arg(long)]
    start: Option<String>,
    /// Exclusive end date YYYY-MM-DD.
    #[arg(long)]
    end: Option<String>,

    /// Strategy name to sweep (see `list-strategies`).
    #[arg(long, value_parser = strategies::ALL.to_vec())]
    strategy: String,

    /// One sweep AXIS: `--param 'name=v1,v2,v3'` (repeatable, one per parameter). Each value parses
    /// as f64; the optimizer runs the full CARTESIAN PRODUCT of all axes (capped at 5000 combos).
    /// Names are the strategy params shown by `list-strategies`, e.g.
    /// `--param 'half_spread_cents=1,2,3' --param 'quote_size=5,10,20'`.
    #[arg(long = "param", value_name = "NAME=V1,V2,...", required = true)]
    param: Vec<String>,

    /// Metric to MAXIMIZE when ranking combos. One of: pnl_total (default), sharpe, sortino,
    /// calmar_ratio, win_rate, profit_factor, ending_balance, expectancy.
    #[arg(long, default_value = "pnl_total")]
    metric: String,

    /// Opening cash balance for every run in the sweep.
    #[arg(long, default_value_t = 1000.0)]
    starting_balance: f64,

    /// Worker threads for the parallel sweep. Defaults to the machine's available parallelism.
    #[arg(long)]
    threads: Option<usize>,

    /// Output directory. When set, writes the per-combo CSV (+ best report.json for `optimize`, or
    /// the per-fold CSV + combined OOS JSON for `walk-forward`). Omit to print results only.
    #[arg(long)]
    out_dir: Option<PathBuf>,

    #[command(flatten)]
    exec: ExecFlags,
}

/// Arguments for the `optimize` subcommand (a parallel parameter sweep + ranking).
#[derive(Args)]
struct OptimizeArgs {
    #[command(flatten)]
    common: OrchCommonArgs,
}

/// Arguments for the `walk-forward` subcommand. Adds `--windows K` on top of the shared grid flags.
#[derive(Args)]
struct WalkForwardArgs {
    #[command(flatten)]
    common: OrchCommonArgs,

    /// Number of walk-forward FOLDS (K). The dataset is split into K+1 contiguous, equal-size,
    /// time-ordered segments; fold `i` TRAINS (optimizes the grid) on segment `i` and TESTS the
    /// chosen config OUT-OF-SAMPLE on segment `i+1` (rolling: single previous segment as train).
    #[arg(long)]
    windows: usize,
}

/// A fully-resolved backtest plan: the merge of (defaults ← --config file ← CLI flags), with all
/// validation done. Decouples "what to run" from "how we parsed it".
///
/// `clickhouse` / `ch_config` are consumed only by the feature-gated ClickHouse loader, so they are
/// `allow(dead_code)` on the default (clickhouse-off) build.
#[derive(Debug)]
#[cfg_attr(not(feature = "clickhouse"), allow(dead_code))]
struct ResolvedRun {
    source: String,
    ndjson: Option<PathBuf>,
    clickhouse: Option<String>,
    ch_config: Option<PathBuf>,
    instrument: Option<String>,
    start: Option<String>,
    end: Option<String>,
    strategy: String,
    /// Tunable strategy params (merged: config file ← CLI `--strategy-param`).
    strategy_params: std::collections::BTreeMap<String, f64>,
    starting_balance: f64,
    tearsheet: PathBuf,
    emit_tearsheet_b64: bool,
    out_dir: Option<PathBuf>,
    /// MULTI-VENUE adapter sources. When non-empty, events are loaded+merged via the adapter
    /// registry instead of the single legacy `source` loader.
    sources: Vec<AdapterSpec>,
    execution: ExecutionConfig,
}

fn main() {
    // Manual run + uniform error formatting: print a friendly chain to STDERR and exit non-zero,
    // never a raw panic/backtrace.
    let cli = Cli::parse();
    let v = cli.verbosity();
    if let Err(err) = run(cli) {
        eprintln!("error: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        let _ = v; // verbosity already applied inside run()
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let v = cli.verbosity();
    match cli.cmd {
        Commands::Backtest(args) => cmd_backtest(args, v),
        Commands::Optimize(args) => cmd_optimize(args, v),
        Commands::WalkForward(args) => cmd_walk_forward(args, v),
        Commands::ListStrategies => cmd_list_strategies(),
        Commands::ListAdapters => cmd_list_adapters(),
        Commands::Validate(args) => cmd_validate(args, v),
        Commands::ListInstruments(args) => cmd_list_instruments(args, v),
        Commands::DescribeData(args) => cmd_describe_data(args, v),
        Commands::InitConfig(args) => cmd_init_config(args),
    }
}

// ============================================================================
// list-strategies
// ============================================================================

fn cmd_list_strategies() -> Result<()> {
    println!("Available strategies ({}):\n", strategies::INFO.len());
    let name_w = strategies::INFO.iter().map(|i| i.name.len()).max().unwrap_or(4);
    for inf in strategies::INFO {
        println!("  {:<width$}  {}", inf.name, inf.description, width = name_w);
        println!("  {:<width$}  params: {}", "", inf.key_params, width = name_w);
    }
    println!("\nUse one with:  kalshi-backtest backtest --strategy <name> ...");
    Ok(())
}

// ============================================================================
// list-adapters
// ============================================================================

/// Print every registered data adapter: name, default venue, a one-line description, and the
/// `mapping` keys it understands (with defaults). Driven from the [`AdapterRegistry`], exactly like
/// `list-strategies` is driven from the strategy registry.
fn cmd_list_adapters() -> Result<()> {
    let reg = AdapterRegistry::with_builtins();
    let infos = reg.infos();
    println!("Available data adapters ({}):\n", infos.len());
    let name_w = infos.iter().map(|i| i.name.len()).max().unwrap_or(6);
    for inf in &infos {
        println!("  {:<name_w$}  [{}]  {}", inf.name, inf.default_venue, inf.description);
        if inf.mapping_keys.is_empty() {
            println!("  {:<name_w$}  mapping: (none — this adapter ignores --map/profile)", "");
        } else {
            println!("  {:<name_w$}  mapping keys:", "");
            let key_w = inf.mapping_keys.iter().map(|k| k.key.len()).max().unwrap_or(4);
            for k in &inf.mapping_keys {
                println!("  {:<name_w$}    {:<key_w$}  {}", "", k.key, k.note);
            }
        }
        println!();
    }
    println!("Use one with:  kalshi-backtest backtest --source adapter --adapter <name> \\");
    println!("                 --venue <TAG> --adapter-path <file> [--adapter-profile p.json]");
    println!("Preview a source first:  kalshi-backtest validate --source adapter --adapter <name> ...");
    Ok(())
}

// ============================================================================
// init-config
// ============================================================================

fn cmd_init_config(args: InitConfigArgs) -> Result<()> {
    let as_json = args.json
        || args
            .path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("json"))
            .unwrap_or(false);

    if args.path.exists() && !args.force {
        bail!(
            "{} already exists — pass --force to overwrite",
            args.path.display()
        );
    }
    if let Some(dir) = args.path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating directory {}", dir.display()))?;
        }
    }
    let body = if as_json { init_config_json() } else { init_config_toml() };
    std::fs::write(&args.path, body)
        .with_context(|| format!("writing example config to {}", args.path.display()))?;

    let fmt = if as_json { "JSON" } else { "TOML" };
    eprintln!("Wrote example {fmt} run config to {}", args.path.display());
    eprintln!(
        "Edit it, then run:  kalshi-backtest backtest --config {}",
        args.path.display()
    );
    Ok(())
}

// ============================================================================
// describe-data / list-instruments
// ============================================================================

fn cmd_describe_data(args: DataSourceArgs, v: Verbosity) -> Result<()> {
    validate_data_source(&args)?;

    if args.source == "clickhouse" {
        return describe_clickhouse(&args);
    }

    let events = load_events_for_source(&args, v)?;
    let sum = data::summary::summarize(&events);

    println!("Data source: {}", source_label(&args));
    println!("  total events : {}", sum.total_events);
    println!("  snapshots    : {}", sum.total_snapshots);
    println!("  deltas       : {}", sum.total_deltas);
    println!("  trades       : {}", sum.total_trades);
    println!("  instruments  : {}", sum.instruments.len());
    println!("  time span    : {}", fmt_span(sum.first_ns, sum.last_ns));
    if !sum.instruments.is_empty() {
        println!("\nPer instrument:");
        print_instrument_table(sum.instruments.iter().map(|i| {
            InstrumentRow {
                instrument: i.instrument.clone(),
                events: i.events,
                snapshots: i.snapshots,
                deltas: i.deltas,
                trades: i.trades,
                first_ns: i.first_ns,
                last_ns: i.last_ns,
            }
        }));
    }
    Ok(())
}

fn cmd_list_instruments(args: DataSourceArgs, v: Verbosity) -> Result<()> {
    validate_data_source(&args)?;

    if args.source == "clickhouse" {
        return list_instruments_clickhouse(&args);
    }

    let events = load_events_for_source(&args, v)?;
    let sum = data::summary::summarize(&events);
    println!("Instruments in {} ({}):\n", source_label(&args), sum.instruments.len());
    if sum.instruments.is_empty() {
        println!("  (none — check your --instrument filter or date range)");
        return Ok(());
    }
    print_instrument_table(sum.instruments.iter().map(|i| InstrumentRow {
        instrument: i.instrument.clone(),
        events: i.events,
        snapshots: i.snapshots,
        deltas: i.deltas,
        trades: i.trades,
        first_ns: i.first_ns,
        last_ns: i.last_ns,
    }));
    Ok(())
}

#[cfg(feature = "clickhouse")]
fn list_instruments_clickhouse(args: &DataSourceArgs) -> Result<()> {
    let rows = clickhouse_instruments(args)?;
    println!("Instruments in clickhouse {} ({}):\n", args.clickhouse.as_deref().unwrap_or(""), rows.len());
    let name_w = rows.iter().map(|r| r.instrument.len()).max().unwrap_or(10).max(10);
    println!("  {:<name_w$}  {:>12}  time span", "instrument", "rows");
    for r in &rows {
        println!(
            "  {:<name_w$}  {:>12}  {}",
            r.instrument,
            r.rows,
            fmt_span(r.first_ns, r.last_ns)
        );
    }
    Ok(())
}

#[cfg(feature = "clickhouse")]
fn describe_clickhouse(args: &DataSourceArgs) -> Result<()> {
    let rows = clickhouse_instruments(args)?;
    let total: u64 = rows.iter().map(|r| r.rows).sum();
    let first = rows.iter().map(|r| r.first_ns).filter(|&n| n > 0).min().unwrap_or(0);
    let last = rows.iter().map(|r| r.last_ns).max().unwrap_or(0);
    println!("Data source: clickhouse {}", args.clickhouse.as_deref().unwrap_or(""));
    println!("  delta rows  : {total}");
    println!("  instruments : {}", rows.len());
    println!("  time span   : {}", fmt_span(first, last));
    println!("\nPer instrument (delta rows):");
    let name_w = rows.iter().map(|r| r.instrument.len()).max().unwrap_or(10).max(10);
    for r in &rows {
        println!("  {:<name_w$}  {:>12}  {}", r.instrument, r.rows, fmt_span(r.first_ns, r.last_ns));
    }
    Ok(())
}

#[cfg(feature = "clickhouse")]
fn clickhouse_instruments(
    args: &DataSourceArgs,
) -> Result<Vec<kalshi_backtester::data::clickhouse::ChInstrumentRow>> {
    use kalshi_backtester::data::clickhouse_schema::ClickHouseSchema;
    let url = args
        .clickhouse
        .as_deref()
        .ok_or_else(|| anyhow!("--clickhouse <url> required for --source clickhouse"))?;
    let schema = match &args.ch_config {
        Some(p) => ClickHouseSchema::from_path(p)?,
        None => ClickHouseSchema::default(),
    };
    let like = args.instrument.as_deref().unwrap_or("%");
    data::clickhouse::list_instruments(url, like, args.start.as_deref(), args.end.as_deref(), &schema)
        .context("could not enumerate instruments from ClickHouse (is it reachable?)")
}

#[cfg(not(feature = "clickhouse"))]
fn list_instruments_clickhouse(_args: &DataSourceArgs) -> Result<()> {
    bail!(clickhouse_disabled_msg())
}

#[cfg(not(feature = "clickhouse"))]
fn describe_clickhouse(_args: &DataSourceArgs) -> Result<()> {
    bail!(clickhouse_disabled_msg())
}

#[cfg_attr(feature = "clickhouse", allow(dead_code))]
fn clickhouse_disabled_msg() -> String {
    "The `clickhouse` source requires building with `--features clickhouse`.\n  \
     Rebuild: cargo build --release --features clickhouse"
        .to_string()
}

// ============================================================================
// validate — load a source, summarize it, preview events, and warn on data-quality issues.
// ============================================================================

/// `validate` handler. Loads the chosen source with the REAL loaders, prints a summary, previews the
/// first `--preview N` parsed events in human-readable form, and runs non-fatal data-quality checks.
/// Exits 0 even with warnings; only a load failure is an error (exit non-zero) — so a newcomer can
/// drop a file, run `validate --preview`, and immediately see whether the backtester understands it.
fn cmd_validate(args: ValidateArgs, v: Verbosity) -> Result<()> {
    // For `--source adapter`, resolve the spec FIRST (so the labels show the profile-resolved adapter
    // name and so an obviously-broken spec errors before we attempt the load).
    let adapter_spec = if args.source == "adapter" {
        Some(build_adapter_spec_from_cli(
            &args.adapter,
            &args.venue,
            &args.adapter_path,
            &args.instrument,
            &args.start,
            &args.end,
            &args.map,
            &args.adapter_profile,
        )?)
    } else {
        None
    };

    let events = load_events_for_validate(&args, adapter_spec.as_ref(), v)
        .with_context(|| "could not load the data source for validation")?;

    // ---- SUMMARY (reuse describe-data's summarizer). ----
    let sum = data::summary::summarize(&events);
    println!("Validation of {}", validate_source_label(&args, adapter_spec.as_ref()));
    println!("  total events : {}", fmt_int(sum.total_events as i64));
    println!("  deltas       : {}", fmt_int(sum.total_deltas as i64));
    println!("  trades       : {}", fmt_int(sum.total_trades as i64));
    println!("  snapshot rows: {}", fmt_int(sum.total_snapshots as i64));
    println!("  instruments  : {}", sum.instruments.len());
    if !sum.instruments.is_empty() {
        let preview_ids: Vec<&str> = sum
            .instruments
            .iter()
            .take(5)
            .map(|i| i.instrument.as_str())
            .collect();
        let more = sum.instruments.len().saturating_sub(preview_ids.len());
        let suffix = if more > 0 { format!(", … (+{more} more)") } else { String::new() };
        println!("    first ids  : {}{}", preview_ids.join(", "), suffix);
    }
    println!("  time span    : {}", fmt_span(sum.first_ns, sum.last_ns));
    if !sum.instruments.is_empty() {
        println!("\nPer instrument:");
        print_instrument_table(sum.instruments.iter().map(|i| InstrumentRow {
            instrument: i.instrument.clone(),
            events: i.events,
            snapshots: i.snapshots,
            deltas: i.deltas,
            trades: i.trades,
            first_ns: i.first_ns,
            last_ns: i.last_ns,
        }));
    }

    // ---- PREVIEW the first N parsed events in human form. ----
    let n = args.preview.min(events.len());
    println!("\nFirst {n} parsed event(s):");
    if events.is_empty() {
        println!("  (none — the source produced zero events)");
    }
    for ev in events.iter().take(n) {
        println!("  {}", describe_event(ev));
    }

    // ---- DATA-QUALITY WARNINGS (non-fatal, specific, actionable). ----
    let warnings = data_quality_warnings(&events, &sum);
    if warnings.is_empty() {
        println!("\nData-quality checks: OK (no warnings).");
    } else {
        println!("\nData-quality WARNINGS ({}):", warnings.len());
        for w in &warnings {
            println!("  ! {w}");
        }
        println!("\n(These are warnings, not failures — the source still loaded. Fix your mapping/");
        println!(" profile if any look wrong, then re-run `validate --preview`.)");
    }
    println!("\nLooks right? Run it:  kalshi-backtest backtest {} --strategy <name>", validate_backtest_hint(&args, adapter_spec.as_ref()));
    Ok(())
}

/// Build the [`AdapterSpec`] for a `--source adapter` validate/run from a profile + CLI flags.
/// CLI flags override the profile; inline `--map key=value` merges over the profile's mapping.
fn build_adapter_spec_from_cli(
    adapter: &Option<String>,
    venue: &Option<String>,
    adapter_path: &Option<PathBuf>,
    instrument: &Option<String>,
    start: &Option<String>,
    end: &Option<String>,
    map: &[String],
    adapter_profile: &Option<PathBuf>,
) -> Result<AdapterSpec> {
    let mut spec = AdapterSpec {
        adapter: adapter.clone().unwrap_or_default(),
        venue: venue.clone().unwrap_or_default(),
        path: adapter_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        instrument: instrument.clone(),
        start: start.clone(),
        end: end.clone(),
        mapping: Default::default(),
    };
    // Inline --map entries take precedence over the profile, so insert them first.
    for raw in map {
        let (k, val) = raw
            .split_once('=')
            .ok_or_else(|| anyhow!("bad --map '{raw}' (expected key=value)"))?;
        spec.mapping.insert(k.trim().to_string(), val.trim().to_string());
    }
    if let Some(path) = adapter_profile {
        let prof = AdapterProfile::from_path(path)?;
        prof.apply_to(&mut spec); // fills only fields/keys the CLI hasn't set
    }
    if spec.adapter.is_empty() {
        bail!("--adapter <key> required for --source adapter (or set `adapter` in --adapter-profile); see `list-adapters`");
    }
    if spec.path.is_empty() {
        bail!("--adapter-path <file> required for --source adapter");
    }
    Ok(spec)
}

/// Load events for the `validate` command, reusing the real ndjson / clickhouse / adapter loaders.
/// `adapter_spec` is the pre-resolved spec for `--source adapter` (None for other sources).
fn load_events_for_validate(
    args: &ValidateArgs,
    adapter_spec: Option<&AdapterSpec>,
    v: Verbosity,
) -> Result<Vec<MarketEvent>> {
    let start_ns = args.start.as_deref().map(date_to_ns).transpose()?;
    let end_ns = args.end.as_deref().map(date_to_ns).transpose()?;
    match args.source.as_str() {
        "ndjson" => {
            let path = args
                .ndjson
                .as_ref()
                .ok_or_else(|| anyhow!("--ndjson <path> required for --source ndjson"))?;
            ensure_file_exists(path, "ndjson")?;
            data::ndjson::load(path, args.instrument.as_deref(), start_ns, end_ns)
                .with_context(|| format!("loading ndjson from {}", path.display()))
        }
        "clickhouse" => {
            let ds = DataSourceArgs {
                source: "clickhouse".into(),
                ndjson: None,
                clickhouse: args.clickhouse.clone(),
                ch_config: args.ch_config.clone(),
                instrument: args.instrument.clone(),
                start: args.start.clone(),
                end: args.end.clone(),
            };
            load_clickhouse_for_validate(&ds)
        }
        "adapter" => {
            let spec = adapter_spec.expect("adapter spec resolved by caller");
            if !spec.path.contains("://") {
                ensure_file_exists(Path::new(&spec.path), &format!("adapter '{}'", spec.adapter))?;
            }
            let reg = AdapterRegistry::with_builtins();
            if reg.get(&spec.adapter).is_none() {
                bail!(
                    "unknown adapter '{}' — known: {}",
                    spec.adapter,
                    reg.names().join(", ")
                );
            }
            if v == Verbosity::Verbose {
                eprintln!("[kalshi-backtest] validate: adapter={} venue={} mapping={:?}", spec.adapter, spec.venue, spec.mapping);
            }
            reg.load_spec(spec)
                .with_context(|| format!("adapter '{}' failed to load {}", spec.adapter, spec.path))
        }
        other => bail!("unknown source {other}"),
    }
}

#[cfg(feature = "clickhouse")]
fn load_clickhouse_for_validate(ds: &DataSourceArgs) -> Result<Vec<MarketEvent>> {
    use kalshi_backtester::data::clickhouse_schema::ClickHouseSchema;
    let url = ds
        .clickhouse
        .as_deref()
        .ok_or_else(|| anyhow!("--clickhouse <url> required for --source clickhouse"))?;
    let like = ds.instrument.as_deref().unwrap_or("%");
    let schema = match &ds.ch_config {
        Some(p) => ClickHouseSchema::from_path(p)?,
        None => ClickHouseSchema::default(),
    };
    data::clickhouse::load(url, like, ds.start.as_deref(), ds.end.as_deref(), &schema)
        .context("could not load from ClickHouse (is the server reachable at the given URL?)")
}

#[cfg(not(feature = "clickhouse"))]
fn load_clickhouse_for_validate(_ds: &DataSourceArgs) -> Result<Vec<MarketEvent>> {
    bail!(clickhouse_disabled_msg())
}

/// A short label naming the validated source (for the summary header).
fn validate_source_label(args: &ValidateArgs, spec: Option<&AdapterSpec>) -> String {
    match args.source.as_str() {
        "ndjson" => format!("ndjson {}", args.ndjson.as_ref().map(|p| p.display().to_string()).unwrap_or_default()),
        "clickhouse" => format!("clickhouse {}", args.clickhouse.as_deref().unwrap_or("")),
        "adapter" => match spec {
            Some(s) => format!(
                "adapter {} (venue {}, {})",
                s.adapter,
                s.resolved_venue(kalshi_backtester::adapters::Venue::Generic("GENERIC".into())).tag(),
                s.path
            ),
            None => "adapter".to_string(),
        },
        other => other.to_string(),
    }
}

/// Reconstruct the source flags as a backtest hint string (so the final "run it" line is copyable).
fn validate_backtest_hint(args: &ValidateArgs, spec: Option<&AdapterSpec>) -> String {
    match args.source.as_str() {
        "ndjson" => format!(
            "--source ndjson --ndjson {}",
            args.ndjson.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
        ),
        "clickhouse" => format!("--source clickhouse --clickhouse {}", args.clickhouse.as_deref().unwrap_or("...")),
        "adapter" => {
            let adapter = spec.map(|s| s.adapter.as_str()).unwrap_or("...");
            let mut s = format!(
                "--source adapter --adapter {} --adapter-path {}",
                adapter,
                args.adapter_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
            );
            if let Some(p) = &args.adapter_profile {
                s.push_str(&format!(" --adapter-profile {}", p.display()));
            }
            s
        }
        _ => String::new(),
    }
}

/// Human-readable one-line rendering of a [`MarketEvent`] for the validate preview: the timestamp as
/// UTC, the instrument, and the delta/trade-specific fields, so a user can eyeball their mapping.
fn describe_event(ev: &MarketEvent) -> String {
    match ev {
        MarketEvent::Delta(d) => {
            let side = match d.side {
                kalshi_backtester::types::Side::Bid => "BID",
                kalshi_backtester::types::Side::Ask => "ASK",
            };
            let action = match d.action {
                kalshi_backtester::types::Action::Add => "ADD",
                kalshi_backtester::types::Action::Update => "UPD",
                kalshi_backtester::types::Action::Delete => "DEL",
            };
            format!(
                "{}  DELTA  {:<24}  {} {} @ {:>5} ({:.2})  size={}  seq={}{}",
                fmt_ts_full(d.ts_ns),
                d.instrument,
                action,
                side,
                format!("{}c", d.price.0),
                d.price.to_dollars(),
                trim_f(d.size),
                d.sequence,
                if d.is_snapshot { "  [SNAPSHOT]" } else { "" },
            )
        }
        MarketEvent::Trade(t) => format!(
            "{}  TRADE  {:<24}  {} @ {:>5} ({:.2})  size={}  aggressor={}{}",
            fmt_ts_full(t.ts_ns),
            t.instrument,
            "X",
            format!("{}c", t.price.0),
            t.price.to_dollars(),
            trim_f(t.size),
            if t.aggressor_yes { "YES" } else { "NO" },
            if t.trade_id.is_empty() { String::new() } else { format!("  id={}", t.trade_id) },
        ),
    }
}

/// Format a timestamp to full `YYYY-MM-DD HH:MM:SS` UTC for the event preview.
fn fmt_ts_full(ns: i64) -> String {
    use chrono::DateTime;
    match DateTime::from_timestamp(ns / 1_000_000_000, (ns % 1_000_000_000) as u32) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => ns.to_string(),
    }
}

/// Trim a float for display (whole numbers drop the decimal: 10.0 → "10", 1.5 → "1.5").
fn trim_f(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

/// Run the non-fatal DATA-QUALITY checks and return a list of specific, actionable warning strings.
/// These never fail the command; they help a user catch a wrong mapping/scale before a full run.
fn data_quality_warnings(events: &[MarketEvent], sum: &data::summary::DataSummary) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut warnings = Vec::new();

    if events.is_empty() {
        warnings.push(
            "the source produced 0 events — check the path, the --instrument filter, the date range, \
             and (for adapters) your mapping (e.g. a wrong `instrument` column drops every row)."
                .to_string(),
        );
        return warnings;
    }

    // Price/size scans over deltas; trades scanned for price/size too.
    let mut delta_price_oob = 0u64; // price outside (0,1] dollars, i.e. cents outside 1..=100
    let mut trade_price_oob = 0u64;
    let mut zero_neg_size = 0u64;
    let mut out_of_order = 0u64;
    let mut last_ts = i64::MIN;
    for ev in events {
        let ts = ev.ts_ns();
        if ts < last_ts {
            out_of_order += 1;
        }
        last_ts = ts;
        match ev {
            MarketEvent::Delta(d) => {
                // valid YES-native cents are 1..=99 (a price of 0 or ≥100 is suspicious for a binary).
                if d.price.0 <= 0 || d.price.0 > 100 {
                    delta_price_oob += 1;
                }
                if d.size <= 0.0 && d.action != kalshi_backtester::types::Action::Delete {
                    zero_neg_size += 1;
                }
            }
            MarketEvent::Trade(t) => {
                if t.price.0 <= 0 || t.price.0 > 100 {
                    trade_price_oob += 1;
                }
                if t.size <= 0.0 {
                    zero_neg_size += 1;
                }
            }
        }
    }

    if delta_price_oob > 0 {
        warnings.push(format!(
            "{} delta row(s) have a price outside (0,1] (≤0¢ or >100¢) — likely a price_scale/price column \
             mistake (use price_scale=cents|bps|prob to match your source units).",
            fmt_int(delta_price_oob as i64)
        ));
    }
    if trade_price_oob > 0 {
        warnings.push(format!(
            "{} trade row(s) have a price outside (0,1] — check the price column / price_scale.",
            fmt_int(trade_price_oob as i64)
        ));
    }
    if zero_neg_size > 0 {
        warnings.push(format!(
            "{} row(s) have zero/negative size — check the size column (and price_scale won't fix this); \
             a signed-size source may need `side_from_sign=<column>`.",
            fmt_int(zero_neg_size as i64)
        ));
    }
    if out_of_order > 0 {
        warnings.push(format!(
            "{} event(s) arrived OUT OF TIME ORDER after loading — unusual (loaders sort by ts); check for \
             a wrong ts column or a ts_unit mismatch producing collisions.",
            fmt_int(out_of_order as i64)
        ));
    }

    // Instruments with no snapshot row (book may be incomplete for these).
    let no_snapshot: Vec<&str> = sum
        .instruments
        .iter()
        .filter(|i| i.snapshots == 0)
        .map(|i| i.instrument.as_str())
        .collect();
    if !no_snapshot.is_empty() {
        let shown: Vec<&str> = no_snapshot.iter().take(3).copied().collect();
        let more = no_snapshot.len().saturating_sub(shown.len());
        let suffix = if more > 0 { format!(", … (+{more} more)") } else { String::new() };
        warnings.push(format!(
            "{} instrument(s) have NO snapshot row (e.g. {}{}) — their book is built from incremental \
             deltas only, which may be incomplete; confirm your is_snapshot mapping.",
            no_snapshot.len(),
            shown.join(", "),
            suffix
        ));
    }

    // Suspicious all-same timestamps (a broken ts mapping collapses everything to one instant).
    let distinct_ts: BTreeSet<i64> = events.iter().map(|e| e.ts_ns()).collect();
    if events.len() > 1 && distinct_ts.len() == 1 {
        warnings.push(format!(
            "every event shares the SAME timestamp ({}) — almost certainly a wrong ts column or ts_unit; \
             the engine needs real time ordering.",
            fmt_ts_full(*distinct_ts.iter().next().unwrap())
        ));
    }

    warnings
}

/// Load events for a `DataSourceArgs` (ndjson) reusing the engine's loaders.
fn load_events_for_source(args: &DataSourceArgs, v: Verbosity) -> Result<Vec<MarketEvent>> {
    let start_ns = args.start.as_deref().map(date_to_ns).transpose()?;
    let end_ns = args.end.as_deref().map(date_to_ns).transpose()?;
    let inst = args.instrument.as_deref();
    let events = match args.source.as_str() {
        "ndjson" => {
            let path = args
                .ndjson
                .as_ref()
                .ok_or_else(|| anyhow!("--ndjson <path> required for --source ndjson"))?;
            data::ndjson::load(path, inst, start_ns, end_ns)
                .with_context(|| format!("loading ndjson from {}", path.display()))?
        }
        other => bail!("unsupported source {other} for this command"),
    };
    if v == Verbosity::Verbose {
        eprintln!("[kalshi-backtest] loaded {} events", events.len());
    }
    Ok(events)
}

// ============================================================================
// backtest
// ============================================================================

fn cmd_backtest(args: BacktestArgs, v: Verbosity) -> Result<()> {
    let plan = resolve_run(&args)?;
    run_backtest(plan, v)
}

/// Merge defaults ← --config ← CLI flags, then validate, producing a [`ResolvedRun`].
fn resolve_run(args: &BacktestArgs) -> Result<ResolvedRun> {
    // Start from the config file (if any). With NO config file, start from blank source/strategy so
    // that omitting them on the CLI produces a clear "required" error rather than silently using a
    // default source. (The RunSpec defaults exist for the config-file path, where they document the
    // common case.) Other fields keep their defaults (e.g. starting_balance = 1000).
    let mut spec = match &args.config {
        Some(path) => RunSpec::from_path(path)?,
        None => RunSpec {
            source: String::new(),
            strategy: String::new(),
            ..Default::default()
        },
    };

    // Apply CLI overrides onto the spec (flags win over the file). For `source`/`strategy`, the
    // RunSpec default is only used when neither file nor flag set them, so require them explicitly
    // when there is no config file.
    if let Some(s) = &args.source {
        spec.source = s.clone();
    }
    macro_rules! ov_opt_str {
        ($field:ident) => {
            if let Some(x) = &args.$field {
                spec.$field = Some(x.clone());
            }
        };
    }
    macro_rules! ov_opt_path {
        ($field:ident) => {
            if let Some(x) = &args.$field {
                spec.$field = Some(x.display().to_string());
            }
        };
    }
    ov_opt_path!(ndjson);
    ov_opt_str!(clickhouse);
    ov_opt_path!(ch_config);
    ov_opt_str!(instrument);
    ov_opt_str!(start);
    ov_opt_str!(end);
    if let Some(s) = &args.strategy {
        spec.strategy = s.clone();
    }
    // Strategy params: CLI `--strategy-param key=value` MERGE OVER (and override) the config file's
    // `[strategy_params]` table. Empty on both => the strategy's built-in defaults are used.
    for raw in &args.strategy_param {
        let (k, v) = parse_strategy_param(raw)?;
        spec.strategy_params.insert(k, v);
    }
    if let Some(b) = args.starting_balance {
        spec.starting_balance = b;
    }
    if let Some(t) = &args.tearsheet {
        spec.tearsheet = Some(t.display().to_string());
    }
    if let Some(o) = &args.out_dir {
        spec.out_dir = Some(o.display().to_string());
    }

    // ---- MULTI-VENUE sources ------------------------------------------------
    // Build the merged-source list from (config `sources` ← `--source adapter` primary ←
    // `--extra-source` repeats). If `--source adapter` is given, it becomes the (first) primary
    // adapter source; each `--extra-source` is parsed and appended. CLI sources REPLACE the
    // config's `sources` list when any CLI adapter flag is present, so the two don't silently merge.
    let cli_has_adapter_sources =
        args.source.as_deref() == Some("adapter") || !args.extra_source.is_empty();
    if cli_has_adapter_sources {
        let mut sources: Vec<AdapterSpec> = Vec::new();
        if args.source.as_deref() == Some("adapter") {
            // Build the primary adapter source from CLI flags, then fill the gaps (adapter/venue/
            // instrument/mapping) from an optional --adapter-profile (CLI wins). This is the single
            // place backtest applies a profile.
            let spec = build_adapter_spec_from_cli(
                &args.adapter,
                &args.venue,
                &args.adapter_path,
                &args.instrument,
                &args.start,
                &args.end,
                &[],
                &args.adapter_profile,
            )?;
            sources.push(spec);
        } else if args.adapter_profile.is_some() {
            bail!("--adapter-profile only applies to the primary `--source adapter` source");
        }
        for raw in &args.extra_source {
            sources.push(parse_extra_source(raw)?);
        }
        spec.sources = sources;
        // Stamp a non-empty `source` so downstream required-field validation passes; the merged
        // adapter path ignores the legacy single-source value.
        spec.source = "adapter".to_string();
    }

    // Execution config: start from the spec's embedded execution (already merged from the file or
    // defaults), optionally replaced by --exec-config preset, then apply individual flag overrides.
    if let Some(path) = &args.exec_config {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("could not read --exec-config {}", path.display()))?;
        spec.execution = serde_json::from_str::<ExecutionConfig>(&raw)
            .with_context(|| format!("invalid --exec-config {}", path.display()))?;
    }
    apply_execution_flag_overrides(&mut spec.execution, args);

    // Validate required fields and source ↔ path consistency.
    validate_resolved(&spec, args.config.is_some())?;

    let tearsheet = spec
        .tearsheet
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../figures/tearsheet.html"));

    Ok(ResolvedRun {
        source: spec.source,
        ndjson: spec.ndjson.map(PathBuf::from),
        clickhouse: spec.clickhouse,
        ch_config: spec.ch_config.map(PathBuf::from),
        instrument: spec.instrument,
        start: spec.start,
        end: spec.end,
        strategy: spec.strategy,
        strategy_params: spec.strategy_params,
        starting_balance: spec.starting_balance,
        tearsheet,
        emit_tearsheet_b64: args.emit_tearsheet_b64,
        out_dir: spec.out_dir.map(PathBuf::from),
        sources: spec.sources,
        execution: spec.execution,
    })
}

/// Parse one `--extra-source` value `adapter=<key>,venue=<TAG>,path=<file>[,instrument=<glob>,start=..,end=..]`
/// into an [`AdapterSpec`]. Keys are comma-separated `k=v` pairs; `adapter` and `path` are required.
fn parse_extra_source(raw: &str) -> Result<AdapterSpec> {
    let mut spec = AdapterSpec::default();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, val) = part
            .split_once('=')
            .ok_or_else(|| anyhow!("bad --extra-source segment '{part}' (expected key=value)"))?;
        let val = val.trim().to_string();
        match k.trim() {
            "adapter" => spec.adapter = val,
            "venue" => spec.venue = val,
            "path" => spec.path = val,
            "instrument" => spec.instrument = Some(val),
            "start" => spec.start = Some(val),
            "end" => spec.end = Some(val),
            other => bail!("unknown --extra-source key '{other}' (expected adapter|venue|path|instrument|start|end)"),
        }
    }
    if spec.adapter.is_empty() {
        bail!("--extra-source missing adapter=<key>: {raw}");
    }
    if spec.path.is_empty() {
        bail!("--extra-source missing path=<file>: {raw}");
    }
    Ok(spec)
}

/// Parse one `--strategy-param key=value` into `(key, value)`. The key is a non-empty identifier;
/// the value parses as an `f64`. Clear errors on a missing `=` or an unparseable number.
fn parse_strategy_param(raw: &str) -> Result<(String, f64)> {
    let (k, v) = raw
        .split_once('=')
        .ok_or_else(|| anyhow!("bad --strategy-param '{raw}' (expected key=value)"))?;
    let key = k.trim();
    if key.is_empty() {
        bail!("bad --strategy-param '{raw}' (empty key)");
    }
    let val: f64 = v
        .trim()
        .parse()
        .map_err(|_| anyhow!("bad --strategy-param '{raw}': value '{}' is not a number", v.trim()))?;
    Ok((key.to_string(), val))
}

/// Apply the individual execution-realism flags onto `e`. With no flags set, `e` is unchanged.
fn apply_execution_flag_overrides(e: &mut ExecutionConfig, args: &BacktestArgs) {
    if args.no_fees {
        e.include_fees = false;
    }
    if args.rewards {
        e.include_rewards = true;
        e.rewards.enabled = true;
    }
    if let Some(x) = args.reward_per_period {
        e.rewards.reward_per_period = x;
        e.rewards.enabled = true;
    }
    if let Some(x) = args.reward_period_secs {
        e.rewards.period_secs = x;
    }
    if let Some(x) = args.min_resting_size {
        e.rewards.min_resting_size = x;
    }
    if let Some(x) = args.max_spread_cents {
        e.rewards.max_spread_cents = x;
    }

    if let Some(x) = args.latency_ns {
        e.latency.order_latency_ns = x;
        e.latency.enabled = true;
    }
    if let Some(x) = args.cancel_latency_ns {
        e.latency.cancel_latency_ns = x;
        e.latency.enabled = true;
    }
    if let Some(x) = args.md_latency_ns {
        e.latency.market_data_latency_ns = x;
        e.latency.enabled = true;
    }
    if let Some(x) = args.jitter_ns {
        e.latency.jitter_ns = x;
        e.latency.enabled = true;
    }

    // ---- STOCHASTIC latency distribution (CLI overrides [execution.latency].dist + seed). ----
    // A `--latency-seed` always overrides the configured seed (whether or not a dist is chosen).
    if let Some(seed) = args.latency_seed {
        e.latency.seed = seed;
    }
    // Selecting a non-fixed `--latency-dist` builds the dist from the param flags and enables the
    // latency model. `--latency-ns` doubles as the mean for normal/exponential where natural.
    if let Some(kind) = args.latency_dist {
        e.latency.dist = build_latency_dist(kind, args);
        // Enable the latency model whenever a distribution is explicitly chosen (even `fixed`, so
        // `--latency-dist fixed` together with `--latency-ns` behaves intuitively).
        e.latency.enabled = true;
    }

    if let Some(x) = args.slippage_ticks {
        e.slippage.taker_ticks = x;
        e.slippage.enabled = true;
    }
    if let Some(x) = args.slippage_bps {
        e.slippage.taker_bps = x;
        e.slippage.enabled = true;
    }

    // ---- maker-queue model: --queue-model overrides [execution.queue].model from --config. ----
    if let Some(x) = args.queue_model {
        e.queue.model = x.into();
    }

    // ---- binary settlement: --settlements <path> overrides [execution.settlement].path. ----
    if let Some(p) = &args.settlements {
        e.settlement.path = Some(p.display().to_string());
    }

    // ---- risk limits: each flag, when present, OVERRIDES the corresponding [execution.risk] key
    //      from --config. Absent flags leave the config (or default None) untouched. ----
    if let Some(x) = args.max_order_qty {
        e.risk.max_order_qty = Some(x);
    }
    if let Some(x) = args.max_position {
        e.risk.max_position_per_instrument = Some(x);
    }
    if let Some(x) = args.max_gross {
        e.risk.max_gross_position = Some(x);
    }
    if let Some(x) = args.equity_floor {
        e.risk.equity_floor = Some(x);
    }
    if let Some(x) = args.max_drawdown_pct {
        e.risk.max_drawdown_pct = Some(x);
    }
}

/// Build a [`LatencyDist`] from the chosen `--latency-dist` kind plus the relevant param flags.
///
/// Param precedence / reuse (documented so the flags read intuitively):
/// * `uniform` => `[--latency-min-ns, --latency-max-ns]` (default 0 each).
/// * `normal` => mean from `--latency-ns` (reused as the base/mean), std from `--latency-std-ns`.
/// * `exponential` => mean from `--latency-mean-ns`, falling back to `--latency-ns`.
/// * `empirical` => `--latency-empirical <path>` (empty path => degrades to fixed at model-build time).
/// * `fixed` => the deterministic hash-jitter model (uses `order_latency_ns` + `jitter_ns`).
fn build_latency_dist(kind: LatencyDistArg, args: &BacktestArgs) -> LatencyDist {
    match kind {
        LatencyDistArg::Fixed => LatencyDist::Fixed,
        LatencyDistArg::Uniform => LatencyDist::Uniform {
            min_ns: args.latency_min_ns.unwrap_or(0),
            max_ns: args.latency_max_ns.unwrap_or(0),
        },
        LatencyDistArg::Normal => LatencyDist::Normal {
            mean_ns: args.latency_ns.unwrap_or(0),
            std_ns: args.latency_std_ns.unwrap_or(0),
        },
        LatencyDistArg::Exponential => LatencyDist::Exponential {
            mean_ns: args.latency_mean_ns.or(args.latency_ns).unwrap_or(0),
        },
        LatencyDistArg::Empirical => LatencyDist::Empirical {
            path: args
                .latency_empirical
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        },
    }
}

/// Validate a resolved spec early with friendly, actionable messages.
fn validate_resolved(spec: &RunSpec, had_config: bool) -> Result<()> {
    let hint = if had_config {
        "set it in the --config file or pass the flag"
    } else {
        "pass the flag"
    };
    // MULTI-VENUE path: when `sources` is populated, validate the adapter specs and skip the
    // single-source path checks entirely.
    if !spec.sources.is_empty() {
        let reg = AdapterRegistry::with_builtins();
        for s in &spec.sources {
            if reg.get(&s.adapter).is_none() {
                bail!(
                    "unknown adapter '{}' in sources — known: {}",
                    s.adapter,
                    reg.names().join(", ")
                );
            }
            // generic file adapters read a local path; check it exists (urls/remote adapters skip).
            if !s.path.contains("://") {
                ensure_file_exists(Path::new(&s.path), &format!("adapter '{}'", s.adapter))?;
            }
            if let Some(d) = &s.start {
                date_to_ns(d).with_context(|| "invalid source start date")?;
            }
            if let Some(d) = &s.end {
                date_to_ns(d).with_context(|| "invalid source end date")?;
            }
        }
        if spec.strategy.is_empty() {
            bail!("no strategy — {hint} (--strategy <name>; see `list-strategies`)");
        }
        if !strategies::ALL.contains(&spec.strategy.as_str()) {
            bail!(
                "unknown strategy '{}' — run `kalshi-backtest list-strategies` for the full list",
                spec.strategy
            );
        }
        return Ok(());
    }

    if spec.source.is_empty() {
        bail!("no data source — {hint} (--source ndjson|clickhouse|adapter)");
    }
    if !["ndjson", "clickhouse"].contains(&spec.source.as_str()) {
        bail!(
            "unknown source '{}' — expected ndjson, clickhouse, or adapter",
            spec.source
        );
    }
    if spec.strategy.is_empty() {
        bail!("no strategy — {hint} (--strategy <name>; see `list-strategies`)");
    }
    if !strategies::ALL.contains(&spec.strategy.as_str()) {
        bail!(
            "unknown strategy '{}' — run `kalshi-backtest list-strategies` for the full list",
            spec.strategy
        );
    }
    match spec.source.as_str() {
        "ndjson" => {
            let p = spec
                .ndjson
                .as_ref()
                .ok_or_else(|| anyhow!("--ndjson <path> required for --source ndjson"))?;
            ensure_file_exists(Path::new(p), "ndjson")?;
        }
        "clickhouse" => {
            if spec.clickhouse.as_deref().unwrap_or("").is_empty() {
                bail!(
                    "--clickhouse <url> required for --source clickhouse (e.g. http://localhost:8123)"
                );
            }
            #[cfg(not(feature = "clickhouse"))]
            {
                bail!(clickhouse_disabled_msg());
            }
        }
        _ => unreachable!(),
    }
    // date sanity
    if let Some(d) = &spec.start {
        date_to_ns(d).with_context(|| "invalid --start date")?;
    }
    if let Some(d) = &spec.end {
        date_to_ns(d).with_context(|| "invalid --end date")?;
    }
    Ok(())
}

fn ensure_file_exists(p: &Path, what: &str) -> Result<()> {
    if !p.exists() {
        bail!(
            "{what} file not found: {} — check the path (it is resolved relative to the current directory)",
            p.display()
        );
    }
    Ok(())
}

fn load_events_for_run(run: &ResolvedRun, v: Verbosity) -> Result<Vec<MarketEvent>> {
    // MULTI-VENUE: merge all adapter sources time-ordered via the registry.
    if !run.sources.is_empty() {
        let reg = AdapterRegistry::with_builtins();
        let events = reg.load_merged(&run.sources)?;
        if v == Verbosity::Verbose {
            let venues: std::collections::BTreeSet<String> = events
                .iter()
                .filter_map(|e| e.instrument().split_once(':').map(|(vn, _)| vn.to_string()))
                .collect();
            eprintln!(
                "[kalshi-backtest] merged {} sources -> {} events across venues: {}",
                run.sources.len(),
                events.len(),
                venues.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        return Ok(events);
    }

    let start_ns = run.start.as_deref().map(date_to_ns).transpose()?;
    let end_ns = run.end.as_deref().map(date_to_ns).transpose()?;
    let inst = run.instrument.as_deref();

    match run.source.as_str() {
        "ndjson" => {
            let path = run.ndjson.as_ref().expect("validated");
            data::ndjson::load(path, inst, start_ns, end_ns)
                .with_context(|| format!("loading ndjson from {}", path.display()))
        }
        "clickhouse" => load_clickhouse(run, v),
        other => bail!("unknown source {other}"),
    }
}

#[cfg(feature = "clickhouse")]
fn load_clickhouse(run: &ResolvedRun, _v: Verbosity) -> Result<Vec<MarketEvent>> {
    use kalshi_backtester::data::clickhouse_schema::ClickHouseSchema;
    let url = run.clickhouse.as_deref().expect("validated");
    let like = run.instrument.as_deref().unwrap_or("%");
    let schema = match &run.ch_config {
        Some(path) => ClickHouseSchema::from_path(path)?,
        None => ClickHouseSchema::default(),
    };
    data::clickhouse::load(url, like, run.start.as_deref(), run.end.as_deref(), &schema)
        .context("could not load from ClickHouse (is the server reachable at the given URL?)")
}

#[cfg(not(feature = "clickhouse"))]
fn load_clickhouse(_run: &ResolvedRun, _v: Verbosity) -> Result<Vec<MarketEvent>> {
    bail!(clickhouse_disabled_msg())
}

fn run_backtest(run: ResolvedRun, v: Verbosity) -> Result<()> {
    let events = load_events_for_run(&run, v)?;
    if v != Verbosity::Quiet {
        eprintln!(
            "[kalshi-backtest] loaded {} events from {} source",
            events.len(),
            run.source
        );
    }
    if events.is_empty() {
        eprintln!(
            "[kalshi-backtest] WARN: no events matched (source={}, instrument={:?}, start={:?}, end={:?})",
            run.source, run.instrument, run.start, run.end
        );
    }

    let mut strat = strategies::build(&run.strategy, &run.strategy_params)
        .ok_or_else(|| anyhow!("unknown strategy {}", run.strategy))?;

    let mut cfg = BacktestConfig::default().with_starting_balance(run.starting_balance);
    cfg.execution = run.execution.clone();
    if v == Verbosity::Verbose {
        eprintln!(
            "[kalshi-backtest] execution: fees={} rewards={} latency={} slippage={} queue={:?}",
            cfg.execution.include_fees,
            cfg.execution.include_rewards,
            cfg.execution.latency.enabled,
            cfg.execution.slippage.enabled,
            cfg.execution.queue.model,
        );
    }

    let n_events = events.len();
    let engine = Engine::new(&cfg);
    let out = engine.run_collecting(events.into_iter(), strat.as_mut(), &cfg);
    let report = out.report;

    // 1. report JSON between sentinels (STDOUT — machine-parseable).
    print_report(&report);

    // 2. tearsheet HTML file.
    let mut tearsheet_path: Option<PathBuf> = None;
    match write_tearsheet_html(&report, &run.tearsheet) {
        Ok(html) => {
            tearsheet_path = Some(run.tearsheet.clone());
            if v == Verbosity::Verbose {
                eprintln!("[kalshi-backtest] wrote tearsheet to {}", run.tearsheet.display());
            }
            if run.emit_tearsheet_b64 {
                print_tearsheet_b64(&html);
            }
        }
        Err(e) => eprintln!("[kalshi-backtest] WARN: could not write tearsheet: {e}"),
    }

    // 3. structured dashboard exports.
    let mut export_dir: Option<PathBuf> = None;
    if let Some(out_dir) = &run.out_dir {
        let meta = ExportMeta {
            strategy: run.strategy.clone(),
            source: run.source.clone(),
            instrument_filter: run.instrument.clone(),
            start: run.start.clone(),
            end: run.end.clone(),
            starting_balance: run.starting_balance,
            currency: cfg.currency.clone(),
            generated_unix_ns: 0,
        };
        match write_exports(out_dir, &report, &out.portfolio, &out.observed_trades, &meta) {
            Ok(()) => {
                export_dir = Some(out_dir.clone());
                if v == Verbosity::Verbose {
                    eprintln!("[kalshi-backtest] wrote dashboard exports to {}", out_dir.display());
                }
            }
            Err(e) => eprintln!("[kalshi-backtest] WARN: could not write exports: {e}"),
        }
    }

    // 4. friendly human summary table (STDERR — keeps STDOUT clean).
    if v != Verbosity::Quiet {
        print_human_summary(&report, &run, n_events, tearsheet_path.as_deref(), export_dir.as_deref());
    }

    Ok(())
}

// ============================================================================
// optimize / walk-forward — shared orchestration plumbing (parse-once, parallel)
// ============================================================================

/// Apply the shared [`ExecFlags`] onto an [`ExecutionConfig`], mirroring `backtest`'s
/// [`apply_execution_flag_overrides`] so a sweep gets identical execution realism. Factored out
/// because the orchestration commands carry the flags on a flattened struct rather than on
/// [`BacktestArgs`].
fn apply_exec_overrides_orch(e: &mut ExecutionConfig, f: &ExecFlags) {
    if f.no_fees {
        e.include_fees = false;
    }
    if f.rewards {
        e.include_rewards = true;
        e.rewards.enabled = true;
    }
    if let Some(x) = f.reward_per_period {
        e.rewards.reward_per_period = x;
        e.rewards.enabled = true;
    }
    if let Some(x) = f.reward_period_secs {
        e.rewards.period_secs = x;
    }
    if let Some(x) = f.min_resting_size {
        e.rewards.min_resting_size = x;
    }
    if let Some(x) = f.max_spread_cents {
        e.rewards.max_spread_cents = x;
    }
    if let Some(x) = f.latency_ns {
        e.latency.order_latency_ns = x;
        e.latency.enabled = true;
    }
    if let Some(x) = f.cancel_latency_ns {
        e.latency.cancel_latency_ns = x;
        e.latency.enabled = true;
    }
    if let Some(x) = f.md_latency_ns {
        e.latency.market_data_latency_ns = x;
        e.latency.enabled = true;
    }
    if let Some(x) = f.jitter_ns {
        e.latency.jitter_ns = x;
        e.latency.enabled = true;
    }
    if let Some(x) = f.slippage_ticks {
        e.slippage.taker_ticks = x;
        e.slippage.enabled = true;
    }
    if let Some(x) = f.slippage_bps {
        e.slippage.taker_bps = x;
        e.slippage.enabled = true;
    }
    if let Some(x) = f.queue_model {
        e.queue.model = x.into();
    }
    if let Some(p) = &f.settlements {
        e.settlement.path = Some(p.display().to_string());
    }
    if let Some(x) = f.max_order_qty {
        e.risk.max_order_qty = Some(x);
    }
    if let Some(x) = f.max_position {
        e.risk.max_position_per_instrument = Some(x);
    }
    if let Some(x) = f.max_gross {
        e.risk.max_gross_position = Some(x);
    }
    if let Some(x) = f.equity_floor {
        e.risk.equity_floor = Some(x);
    }
    if let Some(x) = f.max_drawdown_pct {
        e.risk.max_drawdown_pct = Some(x);
    }
}

/// Turn the shared [`OrchCommonArgs`] into a [`ResolvedRun`] (so we can reuse `load_events_for_run`
/// and all its source/adapter/clickhouse handling) plus the parsed grid axes + metric + thread count.
/// Validates the data source, strategy, metric, and grid up front with friendly errors.
struct OrchPlan {
    run: ResolvedRun,
    axes: Vec<ParamAxis>,
    metric: Metric,
    threads: usize,
}

fn resolve_orch(common: &OrchCommonArgs) -> Result<OrchPlan> {
    // ---- data source: build the same source/adapter plumbing as `backtest`. ----
    // Multi-venue adapter sources (primary --source adapter + repeated --extra-source).
    let mut sources: Vec<AdapterSpec> = Vec::new();
    let cli_has_adapter_sources = common.source == "adapter" || !common.extra_source.is_empty();
    if cli_has_adapter_sources {
        if common.source == "adapter" {
            let spec = build_adapter_spec_from_cli(
                &common.adapter,
                &common.venue,
                &common.adapter_path,
                &common.instrument,
                &common.start,
                &common.end,
                &[],
                &common.adapter_profile,
            )?;
            sources.push(spec);
        } else if common.adapter_profile.is_some() {
            bail!("--adapter-profile only applies to the primary `--source adapter` source");
        }
        for raw in &common.extra_source {
            sources.push(parse_extra_source(raw)?);
        }
    }

    // Build the execution config from defaults + the shared flags.
    let mut execution = ExecutionConfig::default();
    apply_exec_overrides_orch(&mut execution, &common.exec);

    let run = ResolvedRun {
        source: if cli_has_adapter_sources { "adapter".to_string() } else { common.source.clone() },
        ndjson: common.ndjson.clone(),
        clickhouse: common.clickhouse.clone(),
        ch_config: common.ch_config.clone(),
        instrument: common.instrument.clone(),
        start: common.start.clone(),
        end: common.end.clone(),
        strategy: common.strategy.clone(),
        strategy_params: Default::default(),
        starting_balance: common.starting_balance,
        tearsheet: PathBuf::from("../figures/tearsheet.html"),
        emit_tearsheet_b64: false,
        out_dir: common.out_dir.clone(),
        sources,
        execution,
    };

    // Validate the resolved data source + strategy via the SAME validator `backtest` uses, by
    // projecting the ResolvedRun back into a RunSpec shell (only the fields the validator reads).
    let spec_shell = RunSpec {
        source: run.source.clone(),
        ndjson: run.ndjson.as_ref().map(|p| p.display().to_string()),
        clickhouse: run.clickhouse.clone(),
        ch_config: run.ch_config.as_ref().map(|p| p.display().to_string()),
        instrument: run.instrument.clone(),
        start: run.start.clone(),
        end: run.end.clone(),
        strategy: run.strategy.clone(),
        sources: run.sources.clone(),
        ..Default::default()
    };
    validate_resolved(&spec_shell, false)?;

    // ---- grid axes ----
    let mut axes: Vec<ParamAxis> = Vec::with_capacity(common.param.len());
    for raw in &common.param {
        axes.push(parse_param_axis(raw).map_err(|e| anyhow!(e))?);
    }

    // ---- metric ----
    let metric = Metric::parse(&common.metric).map_err(|e| anyhow!(e))?;

    // ---- threads: default to available parallelism. ----
    let threads = common.threads.unwrap_or_else(default_threads).max(1);

    Ok(OrchPlan { run, axes, metric, threads })
}

/// Default worker-thread count = the machine's available parallelism (falling back to 1).
fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Build the per-run [`BacktestConfig`] shared by every combo in a sweep (starting balance +
/// execution config). Only the strategy params differ per combo.
fn orch_base_cfg(run: &ResolvedRun) -> BacktestConfig {
    let mut cfg = BacktestConfig::default().with_starting_balance(run.starting_balance);
    cfg.execution = run.execution.clone();
    cfg
}

// ============================================================================
// optimize
// ============================================================================

/// `optimize` handler: parse the events ONCE, expand the grid, run every combo in parallel against
/// the shared events (deterministic config-order results), rank by `--metric`, print a ranked table
/// to STDERR, the BEST config + its full report.json to STDOUT, and (with `--out-dir`) write
/// `optimize_results.csv` + the best combo's `report.json`.
fn cmd_optimize(args: OptimizeArgs, v: Verbosity) -> Result<()> {
    let plan = resolve_orch(&args.common)?;
    let combos = expand_grid(&plan.axes).map_err(|e| anyhow!(e))?;

    // Parse the tick events EXACTLY once, then share immutably across all worker threads.
    let events = std::sync::Arc::new(load_events_for_run(&plan.run, v)?);
    if v != Verbosity::Quiet {
        eprintln!(
            "[kalshi-backtest] optimize: {} events parsed once; running {} combos of '{}' on {} threads (metric={})",
            events.len(),
            combos.len(),
            plan.run.strategy,
            plan.threads,
            plan.metric.name(),
        );
    }
    if events.is_empty() {
        bail!("no events matched the source/instrument/date filters — nothing to optimize");
    }

    let base_cfg = orch_base_cfg(&plan.run);
    let results = run_grid(&events, &base_cfg, &plan.run.strategy, &combos, plan.threads);

    // Rank a copy of the indices by the chosen metric (descending), stable on ties (index order).
    let mut order: Vec<usize> = (0..results.len()).collect();
    order.sort_by(|&a, &b| {
        let va = plan.metric.value(&results[a].1);
        let vb = plan.metric.value(&results[b].1);
        vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(&b))
    });
    let best_i = best_index(&results, plan.metric)
        .ok_or_else(|| anyhow!("optimization produced no results"))?;

    // ---- ranked table to STDERR (keeps STDOUT clean for the report JSON). ----
    if v != Verbosity::Quiet {
        print_optimize_table(&results, &order, plan.metric);
        let (best_combo, best_sum) = &results[best_i];
        eprintln!(
            "\nBEST by {}: {}  ->  {} = {}  (pnl_total={}, sharpe={:.3}, win_rate={:.1}%, ending_balance={})",
            plan.metric.name(),
            best_combo.label(),
            plan.metric.name(),
            fmt_metric(plan.metric.value(best_sum)),
            fmt_metric(best_sum.pnl_total),
            best_sum.sharpe,
            best_sum.win_rate * 100.0,
            fmt_metric(best_sum.ending_balance),
        );
    }

    // ---- BEST combo's full report.json to STDOUT (between the infra sentinels). ----
    // Re-run the best combo to rebuild its full Report (run_grid keeps only the Summary to stay
    // memory-light across thousands of combos); this single extra run is negligible.
    let best_report = run_one_report(&events, &base_cfg, &plan.run.strategy, &results[best_i].0);
    print_report(&best_report);

    // ---- optional CSV + best report.json artifacts. ----
    if let Some(dir) = &plan.run.out_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating --out-dir {}", dir.display()))?;
        let csv_path = dir.join("optimize_results.csv");
        write_optimize_csv(&csv_path, &results, &plan.axes, plan.metric)
            .with_context(|| format!("writing {}", csv_path.display()))?;
        let report_path = dir.join("report.json");
        std::fs::write(
            &report_path,
            serde_json::to_string_pretty(&best_report).unwrap_or_else(|_| "{}".into()),
        )
        .with_context(|| format!("writing {}", report_path.display()))?;
        if v != Verbosity::Quiet {
            eprintln!(
                "[kalshi-backtest] wrote {} ({} rows) and the best combo's report.json to {}",
                csv_path.display(),
                results.len(),
                dir.display()
            );
        }
    }

    Ok(())
}

/// Print a ranked optimize table to STDERR: rank, the combo label, and the key metrics, ordered by
/// the chosen metric (descending). Capped to the top ~25 rows so a huge sweep stays readable (the
/// full set goes to the CSV).
fn print_optimize_table(results: &[(Combo, kalshi_backtester::types::Summary)], order: &[usize], metric: Metric) {
    let max_rows = 25usize;
    let shown = order.len().min(max_rows);
    eprintln!("\nRanked results (top {shown} of {}, by {}):", order.len(), metric.name());
    eprintln!(
        "  {:>4}  {:<32}  {:>12}  {:>10}  {:>9}  {:>9}  {:>12}",
        "rank", "params", "pnl_total", "sharpe", "win_rate", "p.factor", metric.name()
    );
    for (rank, &i) in order.iter().take(max_rows).enumerate() {
        let (c, s) = &results[i];
        eprintln!(
            "  {:>4}  {:<32}  {:>12}  {:>10.3}  {:>8.1}%  {:>9.3}  {:>12}",
            rank + 1,
            truncate(&c.label(), 32),
            fmt_metric(s.pnl_total),
            s.sharpe,
            s.win_rate * 100.0,
            s.profit_factor,
            fmt_metric(metric.value(s)),
        );
    }
    if order.len() > max_rows {
        eprintln!("  ... ({} more rows in optimize_results.csv if --out-dir set)", order.len() - max_rows);
    }
}

/// Run ONE combo and rebuild its full [`Report`] (not just the Summary), for the best-config output.
fn run_one_report(
    events: &std::sync::Arc<Vec<MarketEvent>>,
    base_cfg: &BacktestConfig,
    strategy: &str,
    combo: &Combo,
) -> Report {
    let params = combo.as_strategy_params();
    let mut strat = strategies::build(strategy, &params).expect("validated strategy");
    let engine = Engine::new(base_cfg);
    let out = engine.run_collecting(events.as_ref().clone().into_iter(), strat.as_mut(), base_cfg);
    out.report
}

/// Write `optimize_results.csv`: one row per combo with every swept param column followed by the key
/// metrics. Columns are stable (axis order, then a fixed metric set) so downstream tooling is happy.
fn write_optimize_csv(
    path: &Path,
    results: &[(Combo, kalshi_backtester::types::Summary)],
    axes: &[ParamAxis],
    _metric: Metric,
) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    // header: param axes (in flag order) then the metric columns.
    let mut header: Vec<String> = axes.iter().map(|a| a.name.clone()).collect();
    header.extend(
        [
            "pnl_total",
            "pnl_pct",
            "ending_balance",
            "sharpe",
            "sortino",
            "calmar_ratio",
            "win_rate",
            "profit_factor",
            "expectancy",
            "max_drawdown_pct",
            "num_trades",
            "num_fills",
            "total_fees",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    writeln!(f, "{}", header.join(","))?;
    for (c, s) in results {
        let mut row: Vec<String> = axes
            .iter()
            .map(|a| {
                c.params
                    .get(&a.name)
                    .map(|v| kalshi_backtester::optimize::fmt_num(*v))
                    .unwrap_or_default()
            })
            .collect();
        row.extend([
            fmt_csv(s.pnl_total),
            fmt_csv(s.pnl_pct),
            fmt_csv(s.ending_balance),
            fmt_csv(s.sharpe),
            fmt_csv(s.sortino),
            fmt_csv(s.calmar_ratio),
            fmt_csv(s.win_rate),
            fmt_csv(s.profit_factor),
            fmt_csv(s.expectancy),
            fmt_csv(s.max_drawdown_pct),
            s.num_trades.to_string(),
            s.num_fills.to_string(),
            fmt_csv(s.total_fees),
        ]);
        writeln!(f, "{}", row.join(","))?;
    }
    Ok(())
}

// ============================================================================
// walk-forward
// ============================================================================

/// One walk-forward fold's outcome: which segment trained/tested, the chosen params, and the
/// IN-SAMPLE (train) vs OUT-OF-SAMPLE (test) metric values.
struct FoldResult {
    fold: usize,
    train_range: (usize, usize),
    test_range: (usize, usize),
    chosen: Combo,
    in_sample_metric: f64,
    oos_metric: f64,
    oos_pnl: f64,
}

/// `walk-forward` handler: parse ONCE, split into K+1 time-ordered segments, and for each fold
/// optimize the grid on the train segment (reusing [`run_grid`]) then measure the chosen config's
/// OUT-OF-SAMPLE performance on the next segment. Reports per-fold + an aggregate OOS number to
/// STDERR; with `--out-dir`, writes `walk_forward.csv` + a combined OOS summary JSON.
///
/// Scheme: ROLLING with a SINGLE previous segment as the training window (train = segment i, test =
/// segment i+1). Simple, honest, and avoids look-ahead: the test segment is strictly after train.
fn cmd_walk_forward(args: WalkForwardArgs, v: Verbosity) -> Result<()> {
    let k = args.windows;
    if k == 0 {
        bail!("--windows must be >= 1 (it is the number of train/test folds)");
    }
    let plan = resolve_orch(&args.common)?;
    let combos = expand_grid(&plan.axes).map_err(|e| anyhow!(e))?;

    // Parse the tick events EXACTLY once, share across folds + threads.
    let events = std::sync::Arc::new(load_events_for_run(&plan.run, v)?);
    if events.len() < (k + 1) {
        bail!(
            "only {} events — need at least K+1={} to form {} walk-forward folds (try fewer --windows)",
            events.len(),
            k + 1,
            k
        );
    }
    if v != Verbosity::Quiet {
        eprintln!(
            "[kalshi-backtest] walk-forward: {} events parsed once; {} folds (K={}), {} combos/fold of '{}' on {} threads (metric={})",
            events.len(),
            k,
            k,
            combos.len(),
            plan.run.strategy,
            plan.threads,
            plan.metric.name(),
        );
    }

    // K+1 contiguous, time-ordered segments. Fold i trains on segment i, tests on segment i+1.
    let segments = split_segments(events.len(), k + 1);
    let base_cfg = orch_base_cfg(&plan.run);

    let mut folds: Vec<FoldResult> = Vec::with_capacity(k);
    for i in 0..k {
        let (ts, te) = segments[i];
        let (vs, ve) = segments[i + 1];
        // TRAIN: optimize the grid on segment i (a slice of the shared events, cloned into an Arc so
        // run_grid can share it across threads — still parse-once; this is an in-memory clone).
        let train_events = std::sync::Arc::new(events[ts..te].to_vec());
        let train_results = run_grid(&train_events, &base_cfg, &plan.run.strategy, &combos, plan.threads);
        let best_i = best_index(&train_results, plan.metric)
            .ok_or_else(|| anyhow!("fold {i}: training produced no results"))?;
        let chosen = train_results[best_i].0.clone();
        let in_sample_metric = plan.metric.value(&train_results[best_i].1);

        // TEST: run the chosen config OUT-OF-SAMPLE on segment i+1.
        let test_events = std::sync::Arc::new(events[vs..ve].to_vec());
        let test_results = run_grid(&test_events, &base_cfg, &plan.run.strategy, std::slice::from_ref(&chosen), 1);
        let test_sum = &test_results[0].1;

        folds.push(FoldResult {
            fold: i,
            train_range: (ts, te),
            test_range: (vs, ve),
            chosen,
            in_sample_metric,
            oos_metric: plan.metric.value(test_sum),
            oos_pnl: test_sum.pnl_total,
        });
    }

    // Aggregate OOS numbers: combined out-of-sample PnL and the mean OOS metric (the honest
    // "does it generalize" figure).
    let combined_oos_pnl: f64 = folds.iter().map(|f| f.oos_pnl).sum();
    let mean_oos_metric: f64 = if folds.is_empty() {
        0.0
    } else {
        folds.iter().map(|f| f.oos_metric).sum::<f64>() / folds.len() as f64
    };

    // ---- per-fold + aggregate report to STDERR. ----
    if v != Verbosity::Quiet {
        eprintln!("\nWalk-forward folds (train = segment i, test = segment i+1; metric = {}):", plan.metric.name());
        eprintln!(
            "  {:>4}  {:<30}  {:>14}  {:>14}  {:>12}  {:>14}",
            "fold", "chosen params", "train events", "test events", "in-sample", "out-of-sample"
        );
        for f in &folds {
            eprintln!(
                "  {:>4}  {:<30}  {:>14}  {:>14}  {:>12}  {:>14}",
                f.fold + 1,
                truncate(&f.chosen.label(), 30),
                format!("{}..{}", f.train_range.0, f.train_range.1),
                format!("{}..{}", f.test_range.0, f.test_range.1),
                fmt_metric(f.in_sample_metric),
                fmt_metric(f.oos_metric),
            );
        }
        eprintln!(
            "\nAGGREGATE out-of-sample: combined OOS pnl_total = {}, mean OOS {} = {}  ({} folds)",
            fmt_metric(combined_oos_pnl),
            plan.metric.name(),
            fmt_metric(mean_oos_metric),
            folds.len()
        );
    }

    // ---- combined OOS summary JSON to STDOUT (between the infra sentinels). ----
    let combined = serde_json::json!({
        "plugin_name": format!("walk_forward:{}", plan.run.strategy),
        "metric": plan.metric.name(),
        "windows": k,
        "combined_oos_pnl": combined_oos_pnl,
        "mean_oos_metric": mean_oos_metric,
        "folds": folds.iter().map(|f| serde_json::json!({
            "fold": f.fold,
            "train_range": [f.train_range.0, f.train_range.1],
            "test_range": [f.test_range.0, f.test_range.1],
            "chosen_params": f.chosen.params,
            "in_sample_metric": f.in_sample_metric,
            "oos_metric": f.oos_metric,
            "oos_pnl": f.oos_pnl,
        })).collect::<Vec<_>>(),
    });
    println!("{}", kalshi_backtester::types::REPORT_JSON_START);
    println!("{}", serde_json::to_string_pretty(&combined).unwrap_or_else(|_| "{}".into()));
    println!("{}", kalshi_backtester::types::REPORT_JSON_END);

    // ---- optional CSV + combined OOS JSON artifacts. ----
    if let Some(dir) = &plan.run.out_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating --out-dir {}", dir.display()))?;
        let csv_path = dir.join("walk_forward.csv");
        write_walk_forward_csv(&csv_path, &folds, plan.metric)
            .with_context(|| format!("writing {}", csv_path.display()))?;
        let json_path = dir.join("walk_forward_oos.json");
        std::fs::write(
            &json_path,
            serde_json::to_string_pretty(&combined).unwrap_or_else(|_| "{}".into()),
        )
        .with_context(|| format!("writing {}", json_path.display()))?;
        if v != Verbosity::Quiet {
            eprintln!(
                "[kalshi-backtest] wrote {} ({} folds) and combined OOS summary to {}",
                csv_path.display(),
                folds.len(),
                dir.display()
            );
        }
    }

    Ok(())
}

/// Write `walk_forward.csv`: one row per fold with the chosen params (as a single `k=v;..` cell),
/// the train/test event ranges, and the in-sample vs out-of-sample metric + OOS pnl.
fn write_walk_forward_csv(path: &Path, folds: &[FoldResult], metric: Metric) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    writeln!(
        f,
        "fold,chosen_params,train_start,train_end,test_start,test_end,in_sample_{m},oos_{m},oos_pnl",
        m = metric.name()
    )?;
    for fr in folds {
        // params joined with ';' so commas in the CSV stay column separators.
        let params = fr
            .chosen
            .params
            .iter()
            .map(|(k, v)| format!("{k}={}", kalshi_backtester::optimize::fmt_num(*v)))
            .collect::<Vec<_>>()
            .join(";");
        writeln!(
            f,
            "{},{},{},{},{},{},{},{},{}",
            fr.fold + 1,
            params,
            fr.train_range.0,
            fr.train_range.1,
            fr.test_range.0,
            fr.test_range.1,
            fmt_csv(fr.in_sample_metric),
            fmt_csv(fr.oos_metric),
            fmt_csv(fr.oos_pnl),
        )?;
    }
    Ok(())
}

/// Format a metric value for human tables: 4 dp, trimmed (so 0 reads "0", 12.5 reads "12.5").
fn fmt_metric(v: f64) -> String {
    if !v.is_finite() {
        return "n/a".to_string();
    }
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Format a numeric CSV cell: full precision but finite-only (non-finite => empty cell).
fn fmt_csv(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        String::new()
    }
}

/// Truncate a string to `max` chars (adding an ellipsis) for fixed-width table cells.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        let mut out: String = s.chars().take(keep).collect();
        out.push('…');
        out
    }
}

// ============================================================================
// Human-readable summary table (STDERR)
// ============================================================================

fn print_human_summary(
    report: &Report,
    run: &ResolvedRun,
    n_events: usize,
    tearsheet: Option<&Path>,
    export_dir: Option<&Path>,
) {
    let s = &report.summary;
    let e = &run.execution;
    // Fee/slippage/reward decomposition. `gross_pnl_ex_costs` is pre-fees/slippage and excludes
    // credited rewards; show how the pieces add back up to pnl_total.
    let fees = s.total_fees;
    let slip = s.total_slippage_cost;
    let rewards_credited = if e.include_rewards { s.liquidity_rewards } else { 0.0 };

    let instruments: Vec<&str> = report
        .instrument_breakdown
        .iter()
        .map(|i| i.instrument.as_str())
        .collect();
    let inst_label = if instruments.is_empty() {
        run.instrument.clone().unwrap_or_else(|| "(all)".to_string())
    } else if instruments.len() <= 3 {
        instruments.join(", ")
    } else {
        format!("{} instruments (e.g. {}, {})", instruments.len(), instruments[0], instruments[1])
    };

    let mut rows: Vec<(String, String)> = Vec::new();
    rows.push(("Strategy".into(), run.strategy.clone()));
    rows.push(("Instrument(s)".into(), inst_label));
    rows.push(("Events processed".into(), fmt_int(n_events as i64)));
    rows.push((
        "PnL total".into(),
        format!("{} {} ({:+.2}%)", money(s.pnl_total), s.currency, s.pnl_pct),
    ));
    rows.push((
        "Balance".into(),
        format!("{} -> {}", money(s.starting_balance), money(s.ending_balance)),
    ));
    rows.push(("Sharpe (per-snap)".into(), format!("{:.3}", s.sharpe)));
    rows.push((
        "Max drawdown".into(),
        format!("{} ({:.2}%)", money(s.max_drawdown), s.max_drawdown_pct * 100.0),
    ));
    rows.push(("Win rate".into(), format!("{:.1}%", s.win_rate * 100.0)));
    rows.push((
        "Fills / round-trips".into(),
        format!("{} / {}", fmt_int(s.num_fills), fmt_int(s.num_trades)),
    ));
    rows.push((
        "Gross PnL (ex costs)".into(),
        format!("{} {}", money(s.gross_pnl_ex_costs), s.currency),
    ));
    rows.push((
        "  - fees".into(),
        format!("{} {}{}", money(fees), s.currency, if e.include_fees { "" } else { " (not charged)" }),
    ));
    rows.push((
        "  - slippage".into(),
        format!("{} {}{}", money(slip), s.currency, if e.slippage.enabled { "" } else { " (off)" }),
    ));
    rows.push((
        "  + rewards".into(),
        format!(
            "{} {}{}",
            money(s.liquidity_rewards),
            s.currency,
            if e.include_rewards {
                ""
            } else if e.rewards.enabled {
                " (accrued, not credited)"
            } else {
                " (off)"
            }
        ),
    ));
    let _ = rewards_credited;
    rows.push(("Volume (contracts)".into(), format!("{:.0}", s.total_volume_contracts)));

    // Latency line: only surfaced when the latency model is active, so ordinary (zero-latency) runs
    // keep the original summary. Shows the chosen distribution + seed (the seed only matters for the
    // non-fixed, stochastic distributions).
    if e.latency.enabled {
        rows.push(("Latency model".into(), describe_latency(&e.latency)));
    }

    // Binary settlement-at-expiry: only surfaced when a settlement file was used or anything settled,
    // so ordinary (flatten-at-mid) runs keep the original summary unchanged.
    let settlement_used = e.settlement.is_enabled();
    if settlement_used || s.num_settled > 0 {
        rows.push((
            "Settled (expiry)".into(),
            format!(
                "{} {} from {} position{}",
                money(s.settled_pnl),
                s.currency,
                fmt_int(s.num_settled),
                if s.num_settled == 1 { "" } else { "s" }
            ),
        ));
    }

    // Risk-layer status: only surfaced when a limit was active or it actually did something, so
    // ordinary runs keep the original summary.
    let risk_active = e.risk.any_enabled();
    if risk_active || s.halted || s.risk_rejections > 0 {
        rows.push((
            "Risk halt".into(),
            if s.halted {
                format!("HALTED — {}", s.halt_reason)
            } else {
                "no (within limits)".into()
            },
        ));
        rows.push(("Risk rejections".into(), fmt_int(s.risk_rejections)));
    }

    // Outputs.
    if let Some(t) = tearsheet {
        rows.push(("Tearsheet".into(), t.display().to_string()));
    }
    if let Some(d) = export_dir {
        rows.push(("Exports".into(), d.display().to_string()));
    }
    rows.push(("report.json".into(), "printed above between sentinels (stdout)".into()));

    // Box-drawing aligned table.
    let key_w = rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    let val_w = rows.iter().map(|(_, v)| v.chars().count()).max().unwrap_or(0);
    let title = " BACKTEST SUMMARY ";
    let inner = key_w + 3 + val_w; // key + " : " + val
    let inner = inner.max(title.len());
    let bar = "─".repeat(inner + 2);

    eprintln!();
    eprintln!("┌{bar}┐");
    eprintln!("│ {:^width$} │", title.trim(), width = inner);
    eprintln!("├{bar}┤");
    for (k, val) in &rows {
        let line = format!("{:<kw$} : {:<vw$}", k, val, kw = key_w, vw = val_w);
        eprintln!("│ {:<width$} │", line, width = inner);
    }
    eprintln!("└{bar}┘");
}

// ============================================================================
// Small shared helpers
// ============================================================================

/// Validate that the `--source` matches a provided path/url for the data-source subcommands.
fn validate_data_source(args: &DataSourceArgs) -> Result<()> {
    match args.source.as_str() {
        "ndjson" => {
            let p = args
                .ndjson
                .as_ref()
                .ok_or_else(|| anyhow!("--ndjson <path> required for --source ndjson"))?;
            ensure_file_exists(p, "ndjson")
        }
        "clickhouse" => {
            if args.clickhouse.as_deref().unwrap_or("").is_empty() {
                bail!("--clickhouse <url> required for --source clickhouse");
            }
            #[cfg(not(feature = "clickhouse"))]
            {
                bail!(clickhouse_disabled_msg());
            }
            #[cfg(feature = "clickhouse")]
            Ok(())
        }
        other => bail!("unknown source {other}"),
    }
}

fn source_label(args: &DataSourceArgs) -> String {
    match args.source.as_str() {
        "ndjson" => format!("ndjson {}", args.ndjson.as_ref().map(|p| p.display().to_string()).unwrap_or_default()),
        "clickhouse" => format!("clickhouse {}", args.clickhouse.as_deref().unwrap_or("")),
        other => other.to_string(),
    }
}

struct InstrumentRow {
    instrument: String,
    events: u64,
    snapshots: u64,
    deltas: u64,
    trades: u64,
    first_ns: i64,
    last_ns: i64,
}

fn print_instrument_table(rows: impl Iterator<Item = InstrumentRow>) {
    let rows: Vec<InstrumentRow> = rows.collect();
    let name_w = rows
        .iter()
        .map(|r| r.instrument.len())
        .max()
        .unwrap_or(10)
        .max("instrument".len());
    println!(
        "  {:<name_w$}  {:>10}  {:>9}  {:>8}  {:>8}  time span",
        "instrument", "events", "snapshots", "deltas", "trades"
    );
    for r in &rows {
        println!(
            "  {:<name_w$}  {:>10}  {:>9}  {:>8}  {:>8}  {}",
            r.instrument,
            r.events,
            r.snapshots,
            r.deltas,
            r.trades,
            fmt_span(r.first_ns, r.last_ns)
        );
    }
}

/// Parse a YYYY-MM-DD date into nanoseconds since epoch (UTC midnight).
fn date_to_ns(date: &str) -> Result<i64> {
    use chrono::{NaiveDate, NaiveTime};
    let d = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
        .map_err(|e| anyhow!("bad date '{date}' (expected YYYY-MM-DD): {e}"))?;
    let dt = d.and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    Ok(dt.and_utc().timestamp_nanos_opt().unwrap_or(0))
}

/// Format a [first_ns, last_ns] span as `YYYY-MM-DD HH:MM .. YYYY-MM-DD HH:MM` (UTC).
fn fmt_span(first_ns: i64, last_ns: i64) -> String {
    if first_ns == 0 && last_ns == 0 {
        return "(empty)".to_string();
    }
    format!("{} .. {}", fmt_ts(first_ns), fmt_ts(last_ns))
}

fn fmt_ts(ns: i64) -> String {
    use chrono::DateTime;
    match DateTime::from_timestamp(ns / 1_000_000_000, (ns % 1_000_000_000) as u32) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => ns.to_string(),
    }
}

fn money(x: f64) -> String {
    format!("{:+.2}", x).trim_start_matches('+').to_string()
}

/// One-line, human-readable description of the active latency model for the summary table: the
/// chosen distribution + its params, plus market-data/cancel latencies and (for stochastic dists)
/// the PRNG seed.
fn describe_latency(cfg: &kalshi_backtester::config::LatencyConfig) -> String {
    use kalshi_backtester::config::LatencyDist::*;
    let dist = match &cfg.dist {
        Fixed => format!("fixed(order={}ns, jitter=±{}ns)", cfg.order_latency_ns, cfg.jitter_ns),
        Uniform { min_ns, max_ns } => format!("uniform[{min_ns}, {max_ns}]ns"),
        Normal { mean_ns, std_ns } => format!("normal(mean={mean_ns}ns, std={std_ns}ns)"),
        Exponential { mean_ns } => format!("exponential(mean={mean_ns}ns)"),
        Empirical { path } => format!("empirical({path})"),
    };
    // The seed only affects the stochastic (non-fixed) distributions; tag it on for those.
    let seed = if matches!(cfg.dist, Fixed) {
        String::new()
    } else {
        format!(", seed={}", cfg.seed)
    };
    format!("{dist}, md={}ns, cancel={}ns{}", cfg.market_data_latency_ns, cfg.cancel_latency_ns, seed)
}

fn fmt_int(n: i64) -> String {
    // thousands separators for readability
    let neg = n < 0;
    let mut s = n.abs().to_string();
    let mut out = String::new();
    while s.len() > 3 {
        let split = s.len() - 3;
        out = format!(",{}{}", &s[split..], out);
        s.truncate(split);
    }
    let res = format!("{s}{out}");
    if neg {
        format!("-{res}")
    } else {
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_to_ns_parses_and_rejects() {
        assert!(date_to_ns("2026-06-04").is_ok());
        assert!(date_to_ns("not-a-date").is_err());
        assert!(date_to_ns("2026/06/04").is_err());
    }

    #[test]
    fn fmt_int_thousands() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(999), "999");
        assert_eq!(fmt_int(1000), "1,000");
        assert_eq!(fmt_int(1234567), "1,234,567");
        assert_eq!(fmt_int(-12345), "-12,345");
    }

    #[test]
    fn money_formats_two_dp() {
        assert_eq!(money(0.0), "0.00");
        assert_eq!(money(12.5), "12.50");
        assert_eq!(money(-3.1), "-3.10");
    }

    fn base_args() -> BacktestArgs {
        BacktestArgs {
            config: None,
            source: None,
            adapter: None,
            venue: None,
            adapter_path: None,
            adapter_profile: None,
            extra_source: Vec::new(),
            ndjson: None,
            clickhouse: None,
            ch_config: None,
            instrument: None,
            start: None,
            end: None,
            strategy: None,
            strategy_param: Vec::new(),
            starting_balance: None,
            tearsheet: None,
            emit_tearsheet_b64: false,
            out_dir: None,
            exec_config: None,
            no_fees: false,
            rewards: false,
            reward_per_period: None,
            reward_period_secs: None,
            min_resting_size: None,
            max_spread_cents: None,
            latency_ns: None,
            cancel_latency_ns: None,
            md_latency_ns: None,
            jitter_ns: None,
            latency_dist: None,
            latency_min_ns: None,
            latency_max_ns: None,
            latency_std_ns: None,
            latency_mean_ns: None,
            latency_empirical: None,
            latency_seed: None,
            slippage_ticks: None,
            slippage_bps: None,
            queue_model: None,
            settlements: None,
            max_order_qty: None,
            max_position: None,
            max_gross: None,
            equity_floor: None,
            max_drawdown_pct: None,
        }
    }

    #[test]
    fn missing_source_is_a_clear_error() {
        let mut a = base_args();
        a.strategy = Some("market_maker".into());
        // no source -> validation error
        let err = resolve_run(&a).unwrap_err();
        assert!(format!("{err}").contains("data source"), "got: {err}");
    }

    #[test]
    fn unknown_strategy_in_config_is_rejected() {
        let spec = RunSpec {
            source: "ndjson".into(),
            ndjson: Some("/definitely/missing.ndjson.gz".into()),
            strategy: "no_such_strategy".into(),
            ..Default::default()
        };
        let err = validate_resolved(&spec, true).unwrap_err();
        assert!(format!("{err}").contains("unknown strategy"), "got: {err}");
    }

    #[test]
    fn missing_ndjson_file_is_a_clear_error() {
        let spec = RunSpec {
            source: "ndjson".into(),
            ndjson: Some("/definitely/missing-file-xyz.ndjson.gz".into()),
            strategy: "market_maker".into(),
            ..Default::default()
        };
        let err = validate_resolved(&spec, false).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"), "got: {msg}");
    }

    #[test]
    fn cli_flags_override_config_values() {
        // Simulate: config has strategy=market_maker, balance=1000; CLI overrides both.
        let mut a = base_args();
        a.source = Some("ndjson".into());
        a.strategy = Some("momentum".into());
        a.starting_balance = Some(5000.0);
        a.ndjson = Some(PathBuf::from("/missing.ndjson.gz")); // validation will fail later, but merge first
        // Build the spec merge manually (skip file load by leaving config None => defaults).
        // Defaults: strategy=market_maker, balance=1000. After overrides: momentum / 5000.
        let mut spec = RunSpec::default();
        spec.source = a.source.clone().unwrap();
        if let Some(s) = &a.strategy {
            spec.strategy = s.clone();
        }
        if let Some(b) = a.starting_balance {
            spec.starting_balance = b;
        }
        assert_eq!(spec.strategy, "momentum");
        assert_eq!(spec.starting_balance, 5000.0);
    }

    #[test]
    fn execution_flag_overrides_enable_models() {
        let mut a = base_args();
        a.rewards = true;
        a.slippage_ticks = Some(2);
        a.latency_ns = Some(1000);
        let mut e = ExecutionConfig::default();
        apply_execution_flag_overrides(&mut e, &a);
        assert!(e.include_rewards && e.rewards.enabled);
        assert!(e.slippage.enabled && e.slippage.taker_ticks == 2);
        assert!(e.latency.enabled && e.latency.order_latency_ns == 1000);
        assert!(e.include_fees); // unchanged
    }

    #[test]
    fn strategy_param_parses_and_rejects() {
        assert_eq!(
            parse_strategy_param("up_ticks=2").unwrap(),
            ("up_ticks".to_string(), 2.0)
        );
        assert_eq!(
            parse_strategy_param("  gamma = 0.5 ").unwrap(),
            ("gamma".to_string(), 0.5)
        );
        // missing '='
        assert!(parse_strategy_param("just_a_key").is_err());
        // empty key
        assert!(parse_strategy_param("=3").is_err());
        // non-numeric value
        let err = parse_strategy_param("size=abc").unwrap_err();
        assert!(format!("{err}").contains("not a number"), "got: {err}");
    }

    #[test]
    fn cli_strategy_params_override_config_and_build() {
        use kalshi_backtester::strategies::{self, StrategyParams};
        // Empty params == default build (back-compat): both build successfully.
        let empty = StrategyParams::new();
        assert!(strategies::build("market_maker", &empty).is_some());
        // A CLI override map is honoured by build().
        let mut m = StrategyParams::new();
        m.insert("half_spread_cents".into(), 3.0);
        assert!(strategies::build("market_maker", &m).is_some());
        // Merge semantics: CLI value overrides a config value for the same key.
        let mut merged: StrategyParams = [("half_spread_cents".to_string(), 2.0)]
            .into_iter()
            .collect();
        let (k, v) = parse_strategy_param("half_spread_cents=3").unwrap();
        merged.insert(k, v);
        assert_eq!(merged.get("half_spread_cents"), Some(&3.0));
    }

    #[test]
    fn latency_dist_flags_build_distribution_and_seed() {
        // --latency-dist normal --latency-ns 500 --latency-std-ns 300 --latency-seed 7
        let mut a = base_args();
        a.latency_dist = Some(LatencyDistArg::Normal);
        a.latency_ns = Some(500);
        a.latency_std_ns = Some(300);
        a.latency_seed = Some(7);
        let mut e = ExecutionConfig::default();
        apply_execution_flag_overrides(&mut e, &a);
        assert!(e.latency.enabled, "choosing a dist enables the latency model");
        assert_eq!(
            e.latency.dist,
            LatencyDist::Normal { mean_ns: 500, std_ns: 300 }
        );
        assert_eq!(e.latency.seed, 7);

        // uniform: min/max wire through.
        let mut a = base_args();
        a.latency_dist = Some(LatencyDistArg::Uniform);
        a.latency_min_ns = Some(100);
        a.latency_max_ns = Some(900);
        let mut e = ExecutionConfig::default();
        apply_execution_flag_overrides(&mut e, &a);
        assert_eq!(
            e.latency.dist,
            LatencyDist::Uniform { min_ns: 100, max_ns: 900 }
        );

        // exponential: mean reuses --latency-ns when --latency-mean-ns is absent.
        let mut a = base_args();
        a.latency_dist = Some(LatencyDistArg::Exponential);
        a.latency_ns = Some(1234);
        let mut e = ExecutionConfig::default();
        apply_execution_flag_overrides(&mut e, &a);
        assert_eq!(e.latency.dist, LatencyDist::Exponential { mean_ns: 1234 });
    }

    #[test]
    fn latency_seed_alone_overrides_without_changing_dist() {
        // A bare --latency-seed updates the seed but leaves dist at the default Fixed.
        let mut a = base_args();
        a.latency_seed = Some(98765);
        let mut e = ExecutionConfig::default();
        apply_execution_flag_overrides(&mut e, &a);
        assert_eq!(e.latency.seed, 98765);
        assert_eq!(e.latency.dist, LatencyDist::Fixed);
    }

    #[test]
    fn no_flags_leaves_execution_default() {
        let a = base_args();
        let mut e = ExecutionConfig::default();
        apply_execution_flag_overrides(&mut e, &a);
        assert!(e.include_fees);
        assert!(!e.include_rewards);
        assert!(!e.latency.enabled);
        assert!(!e.slippage.enabled);
    }

    // ========================================================================
    // optimize / walk-forward orchestration
    // ========================================================================

    /// An all-default [`ExecFlags`] (no overrides), the orchestration analogue of `base_args`.
    fn base_exec_flags() -> ExecFlags {
        ExecFlags {
            no_fees: false,
            rewards: false,
            reward_per_period: None,
            reward_period_secs: None,
            min_resting_size: None,
            max_spread_cents: None,
            latency_ns: None,
            cancel_latency_ns: None,
            md_latency_ns: None,
            jitter_ns: None,
            slippage_ticks: None,
            slippage_bps: None,
            queue_model: None,
            settlements: None,
            max_order_qty: None,
            max_position: None,
            max_gross: None,
            equity_floor: None,
            max_drawdown_pct: None,
        }
    }

    /// Common args for an ndjson optimize/walk-forward run over `path` with the given grid + metric.
    fn base_common(path: &Path, strategy: &str, params: Vec<String>, metric: &str) -> OrchCommonArgs {
        OrchCommonArgs {
            source: "ndjson".into(),
            adapter: None,
            venue: None,
            adapter_path: None,
            adapter_profile: None,
            extra_source: Vec::new(),
            ndjson: Some(path.to_path_buf()),
            clickhouse: None,
            ch_config: None,
            instrument: None,
            start: None,
            end: None,
            strategy: strategy.into(),
            param: params,
            metric: metric.into(),
            starting_balance: 1000.0,
            threads: Some(2),
            out_dir: None,
            exec: base_exec_flags(),
        }
    }

    /// Write a tiny inline ndjson tick capture (deltas + a couple of trades) to a temp file so the
    /// CLI integration tests don't depend on external data. Returns the path.
    fn write_tiny_ndjson() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir();
        // Unique per call (PID + atomic counter) so parallel tests never share/remove each other's file.
        let uid = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("kb_orch_test_{}_{uid}.ndjson", std::process::id()));
        // A snapshot, some book moves, and trades that cross resting/taker orders.
        fn delta(ts: i64, side: &str, price: f64, size: f64, snap: i64) -> String {
            format!(
                "{{\"kind\":\"delta\",\"ts_ns\":{ts},\"instrument\":\"KXTEST-A\",\"action\":\"ADD\",\"side\":\"{side}\",\"price\":{price},\"size\":{size},\"sequence\":{ts},\"is_snapshot\":{snap}}}\n"
            )
        }
        let mut lines = String::new();
        lines.push_str(&delta(1000, "BUY", 0.45, 200.0, 1));
        lines.push_str(&delta(1001, "SELL", 0.55, 200.0, 0));
        for (i, px) in [0.47, 0.49, 0.51, 0.53, 0.51, 0.49, 0.47, 0.50].iter().enumerate() {
            let ts = 2000 + i as i64 * 1000;
            lines.push_str(&delta(ts, "BUY", px - 0.01, 100.0, 0));
            lines.push_str(&delta(ts + 1, "SELL", px + 0.01, 100.0, 0));
            lines.push_str(&format!(
                "{{\"kind\":\"trade\",\"ts_ns\":{},\"instrument\":\"KXTEST-A\",\"aggressor_side\":\"yes\",\"price\":{px},\"size\":5.0,\"trade_id\":\"t{ts}\"}}\n",
                ts + 2
            ));
        }
        std::fs::write(&path, lines).unwrap();
        path
    }

    #[test]
    fn exec_flags_orch_enable_models_like_backtest() {
        let mut f = base_exec_flags();
        f.rewards = true;
        f.slippage_ticks = Some(2);
        f.latency_ns = Some(1000);
        let mut e = ExecutionConfig::default();
        apply_exec_overrides_orch(&mut e, &f);
        assert!(e.include_rewards && e.rewards.enabled);
        assert!(e.slippage.enabled && e.slippage.taker_ticks == 2);
        assert!(e.latency.enabled && e.latency.order_latency_ns == 1000);
        assert!(e.include_fees); // unchanged
    }

    #[test]
    fn resolve_orch_parses_grid_metric_and_threads() {
        let path = write_tiny_ndjson();
        let common = base_common(
            &path,
            "market_maker",
            vec!["half_spread_cents=1,2,3".into(), "quote_size=5,10".into()],
            "sharpe",
        );
        let plan = resolve_orch(&common).expect("resolve");
        assert_eq!(plan.axes.len(), 2);
        assert_eq!(plan.axes[0].name, "half_spread_cents");
        assert_eq!(plan.axes[0].values, vec![1.0, 2.0, 3.0]);
        assert_eq!(plan.metric, Metric::Sharpe);
        assert!(plan.threads >= 1);
        // grid expands to the cartesian product (3 x 2 = 6).
        let combos = expand_grid(&plan.axes).unwrap();
        assert_eq!(combos.len(), 6);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn resolve_orch_rejects_bad_metric_and_missing_file() {
        // bad metric
        let path = write_tiny_ndjson();
        let mut common = base_common(&path, "market_maker", vec!["quote_size=5,10".into()], "not_a_metric");
        assert!(resolve_orch(&common).is_err());
        // good metric but missing data file
        common.metric = "pnl_total".into();
        common.ndjson = Some(PathBuf::from("/definitely/missing-xyz.ndjson.gz"));
        let err = match resolve_orch(&common) {
            Ok(_) => panic!("expected an error for a missing data file"),
            Err(e) => e,
        };
        assert!(format!("{err}").contains("not found"), "got: {err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn optimize_end_to_end_runs_and_writes_csv() {
        let path = write_tiny_ndjson();
        let out = std::env::temp_dir().join(format!("kb_opt_out_{}", std::process::id()));
        let mut common = base_common(
            &path,
            "market_maker",
            vec!["half_spread_cents=1,2".into(), "quote_size=5,10".into()],
            "pnl_total",
        );
        common.out_dir = Some(out.clone());
        let args = OptimizeArgs { common };
        cmd_optimize(args, Verbosity::Quiet).expect("optimize runs");
        // CSV + best report.json written; CSV has a header + one row per combo (4).
        let csv = std::fs::read_to_string(out.join("optimize_results.csv")).unwrap();
        let rows: Vec<&str> = csv.lines().collect();
        assert_eq!(rows.len(), 1 + 4, "header + 4 combo rows");
        assert!(rows[0].starts_with("half_spread_cents,quote_size,pnl_total"));
        assert!(out.join("report.json").exists());
        std::fs::remove_dir_all(&out).ok();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn walk_forward_end_to_end_folds_cover_timeline() {
        let path = write_tiny_ndjson();
        let out = std::env::temp_dir().join(format!("kb_wf_out_{}", std::process::id()));
        let mut common = base_common(&path, "market_maker", vec!["half_spread_cents=1,2".into()], "pnl_total");
        common.out_dir = Some(out.clone());
        let args = WalkForwardArgs { common, windows: 3 };
        cmd_walk_forward(args, Verbosity::Quiet).expect("walk-forward runs");
        // walk_forward.csv has a header + K=3 fold rows; OOS JSON written.
        let csv = std::fs::read_to_string(out.join("walk_forward.csv")).unwrap();
        assert_eq!(csv.lines().count(), 1 + 3, "header + 3 fold rows");
        let oos = std::fs::read_to_string(out.join("walk_forward_oos.json")).unwrap();
        assert!(oos.contains("combined_oos_pnl"));
        assert!(oos.contains("mean_oos_metric"));
        std::fs::remove_dir_all(&out).ok();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn list_adapters_registry_has_descriptions_and_mapping_keys() {
        let reg = AdapterRegistry::with_builtins();
        let infos = reg.infos();
        // every adapter has a non-empty description
        assert!(infos.iter().all(|i| !i.description.is_empty()));
        // the generic adapters expose mapping keys; first-class ones don't.
        let gen = infos.iter().find(|i| i.name == "generic_csv").unwrap();
        assert!(gen.mapping_keys.iter().any(|k| k.key == "price_scale"));
        assert!(gen.mapping_keys.iter().any(|k| k.key == "side_from_sign"));
        let kalshi = infos.iter().find(|i| i.name == "kalshi_ndjson").unwrap();
        assert!(kalshi.mapping_keys.is_empty());
    }

    #[test]
    fn data_quality_warnings_flag_obvious_problems() {
        use kalshi_backtester::types::{Action, BookDelta, Cents, Side};
        // A delta with price=0 (out of (0,1]) and a zero-size add, all sharing one timestamp.
        let evs = vec![
            MarketEvent::Delta(BookDelta {
                ts_ns: 5,
                instrument: "X".into(),
                action: Action::Add,
                side: Side::Bid,
                price: Cents(0),
                size: 0.0,
                sequence: 1,
                is_snapshot: false,
            }),
            MarketEvent::Delta(BookDelta {
                ts_ns: 5,
                instrument: "X".into(),
                action: Action::Add,
                side: Side::Ask,
                price: Cents(60),
                size: 10.0,
                sequence: 2,
                is_snapshot: false,
            }),
        ];
        let sum = data::summary::summarize(&evs);
        let w = data_quality_warnings(&evs, &sum);
        let joined = w.join(" | ");
        assert!(joined.contains("outside (0,1]"), "price warning missing: {joined}");
        assert!(joined.contains("zero/negative size"), "size warning missing: {joined}");
        assert!(joined.contains("NO snapshot row"), "snapshot warning missing: {joined}");
        assert!(joined.contains("SAME timestamp"), "same-ts warning missing: {joined}");
    }

    #[test]
    fn data_quality_warnings_empty_source() {
        let sum = data::summary::summarize(&[]);
        let w = data_quality_warnings(&[], &sum);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("0 events"), "got: {}", w[0]);
    }

    #[test]
    fn build_adapter_spec_profile_fills_then_cli_overrides() {
        use std::io::Write;
        let mut p = std::env::temp_dir();
        p.push(format!("kb_prof_{}.json", std::process::id()));
        std::fs::File::create(&p)
            .unwrap()
            .write_all(br#"{"adapter":"generic_csv","venue":"DEMO","mapping":{"price":"px","price_scale":"cents"}}"#)
            .unwrap();
        // CLI sets only the path; the profile supplies adapter/venue/mapping.
        let spec = build_adapter_spec_from_cli(
            &None,
            &None,
            &Some(PathBuf::from("/tmp/x.csv")),
            &None,
            &None,
            &None,
            &[],
            &Some(p.clone()),
        )
        .unwrap();
        assert_eq!(spec.adapter, "generic_csv");
        assert_eq!(spec.venue, "DEMO");
        assert_eq!(spec.mapping.get("price").map(|s| s.as_str()), Some("px"));

        // CLI --map overrides the profile mapping for the same key.
        let spec2 = build_adapter_spec_from_cli(
            &Some("generic_ndjson".into()), // CLI adapter wins over profile's generic_csv
            &None,
            &Some(PathBuf::from("/tmp/x.csv")),
            &None,
            &None,
            &None,
            &["price=other".to_string()],
            &Some(p.clone()),
        )
        .unwrap();
        assert_eq!(spec2.adapter, "generic_ndjson");
        assert_eq!(spec2.mapping.get("price").map(|s| s.as_str()), Some("other"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn walk_forward_zero_windows_is_rejected() {
        let path = write_tiny_ndjson();
        let common = base_common(&path, "market_maker", vec!["half_spread_cents=1,2".into()], "pnl_total");
        let args = WalkForwardArgs { common, windows: 0 };
        let err = cmd_walk_forward(args, Verbosity::Quiet).unwrap_err();
        assert!(format!("{err}").contains("windows"), "got: {err}");
        std::fs::remove_file(&path).ok();
    }
}
