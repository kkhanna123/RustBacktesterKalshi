//! Backtest configuration. All knobs with sensible defaults so a bare run "just works".
//!
//! The execution-realism knobs live in [`ExecutionConfig`] and its three sub-configs
//! ([`LatencyConfig`], [`SlippageConfig`], [`RewardsConfig`]). Every effect is independently
//! toggleable so a researcher can isolate the impact of latency, slippage, fees, or
//! liquidity-rewards one at a time. With the defaults, the execution config is a no-op
//! (no latency, no slippage, fees on, rewards off) so existing runs reproduce exactly.

use serde::{Deserialize, Serialize};

/// Tunable parameters for a backtest run.
#[derive(Debug, Clone)]
pub struct BacktestConfig {
    /// Opening cash balance (account currency).
    pub starting_balance: f64,
    /// Account currency label (purely cosmetic / reported).
    pub currency: String,
    /// If true, fees use the Kalshi notional formula `ceil(0.07 * C * p * (1-p) * 100)/100`.
    /// If false, fees are a flat `taker_fee_rate` fraction of notional (and `maker_fee` per contract).
    pub fee_bps_formula: bool,
    /// Flat maker fee per contract (dollars) when `fee_bps_formula` is false. Default 0.0.
    pub maker_fee: f64,
    /// Flat taker fee as a fraction of notional when `fee_bps_formula` is false.
    pub taker_fee_rate: f64,
    /// Minimum seconds between equity-curve snapshots.
    pub equity_snapshot_secs: i64,
    /// Simulated order latency in nanoseconds (legacy field; superseded by
    /// `execution.latency.order_latency_ns`. Kept for source compatibility, 0 = instantaneous).
    pub order_latency_ns: i64,
    /// If true, flatten all open positions at end-of-data (market) before reporting.
    pub flatten_at_end: bool,
    /// Execution-realism configuration (fees toggle, latency, slippage, liquidity rewards).
    pub execution: ExecutionConfig,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        BacktestConfig {
            starting_balance: 1000.0,
            currency: "USD".to_string(),
            fee_bps_formula: true,
            maker_fee: 0.0,
            taker_fee_rate: 0.01,
            equity_snapshot_secs: 1,
            order_latency_ns: 0,
            flatten_at_end: true,
            execution: ExecutionConfig::default(),
        }
    }
}

impl BacktestConfig {
    pub fn with_starting_balance(mut self, bal: f64) -> Self {
        self.starting_balance = bal;
        self
    }
}

/// Master switch-board for execution realism. Each effect is independently toggleable.
///
/// PnL decomposition (see [`crate::report::build_report`]):
/// `pnl_total = gross_trading_pnl - (fees if include_fees) - (slippage if slippage.enabled)
///              + (rewards if include_rewards)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionConfig {
    /// If true (default), trading fees reduce realized PnL / ending balance. If false, fees are
    /// still *recorded* on fills for reporting but are *zeroed out of the cash flow* (and of
    /// `total_fees`), so a researcher can see PnL gross of fees.
    #[serde(default = "default_true")]
    pub include_fees: bool,
    /// If true, accrued Kalshi liquidity-incentive rewards are *added* into PnL / ending balance.
    /// Default false (rewards are still accrued and reported, just not credited to cash).
    #[serde(default)]
    pub include_rewards: bool,
    /// Order/cancel/market-data latency model.
    #[serde(default)]
    pub latency: LatencyConfig,
    /// Adverse-slippage model applied to taker fills (and optional maker adverse selection).
    #[serde(default)]
    pub slippage: SlippageConfig,
    /// Kalshi liquidity-incentive (maker rewards) model.
    #[serde(default)]
    pub rewards: RewardsConfig,
    /// Hard, engine-enforced risk limits (order/position caps + equity-floor / drawdown HALT).
    /// All fields `None` (the default) => no checks => identical behaviour to a run with no risk
    /// layer at all. See [`RiskConfig`].
    #[serde(default)]
    pub risk: RiskConfig,
    /// Maker-queue model: how a resting order's `queue_ahead` evolves as the book changes.
    /// The default ([`QueueModel::Pessimistic`]) reproduces the historical behaviour exactly.
    /// See [`QueueConfig`].
    #[serde(default)]
    pub queue: QueueConfig,
    /// BINARY SETTLEMENT-AT-EXPIRY: an optional path to a settlement file mapping each instrument to
    /// its resolved outcome (YES/NO). When `path` is `None` (the default) settlement is DISABLED and
    /// end-of-run positions are flattened at mid exactly as before. See [`SettlementConfig`].
    #[serde(default)]
    pub settlement: SettlementConfig,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        ExecutionConfig {
            include_fees: true,
            include_rewards: false,
            latency: LatencyConfig::default(),
            slippage: SlippageConfig::default(),
            rewards: RewardsConfig::default(),
            risk: RiskConfig::default(),
            queue: QueueConfig::default(),
            settlement: SettlementConfig::default(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// BINARY SETTLEMENT-AT-EXPIRY configuration (the `[execution.settlement]` block).
///
/// Settlement is enabled purely by the PRESENCE of `path`: when `path` is `Some(file)`, the engine
/// loads a `instrument_id -> {yes,no}` map from that file (CSV or JSON; see
/// [`crate::settlement::SettlementMap`]) and, at end-of-run, SETTLES each open position whose
/// instrument has a known outcome to its $1/$0 binary payoff instead of flattening it at mid.
/// Positions in instruments NOT present in the file (Unknown outcome) still flatten at mid. When
/// `path` is `None` (the default) settlement is a complete no-op and runs reproduce exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementConfig {
    /// Path to the settlement file (CSV `instrument_id,result` or JSON). `None` (default) disables
    /// settlement entirely (flatten-at-mid as before).
    #[serde(default)]
    pub path: Option<String>,
}

impl Default for SettlementConfig {
    fn default() -> Self {
        SettlementConfig { path: None }
    }
}

impl SettlementConfig {
    /// True if a settlement file path is configured (so the engine should load + settle).
    pub fn is_enabled(&self) -> bool {
        self.path.is_some()
    }
}

/// How a resting order's queue position (`queue_ahead`) responds to the book changing.
///
/// We CANNOT observe our true position in the exchange's FIFO queue from L2 (aggregated) data — we
/// only see the total resting size at each price level. These two models bracket the extremes:
///
/// * [`QueueModel::Pessimistic`] — assume every cancellation ahead of us is actually *behind* us, so
///   a level shrinking via a cancel does NOT improve our position. Our `queue_ahead` only burns down
///   when trades print. This is the conservative assumption (you wait the longest to fill) and is the
///   DEFAULT, reproducing the original engine behaviour byte-for-byte.
/// * [`QueueModel::Optimistic`] — assume every cancellation ahead of us was actually *ahead* of us, so
///   when the resting size at our level DECREASES via a book update/delete we reduce our `queue_ahead`
///   by that decrease (floored at 0), moving us up the queue. Trades still burn the queue as usual.
///   This is the favourable assumption (you fill the soonest).
///
/// Reality lies somewhere between the two; running both brackets the impact of queue position on a
/// maker strategy's fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueModel {
    /// Cancellations ahead do NOT help; queue only burns on trades. Default (= original behaviour).
    Pessimistic,
    /// Cancellations at our level move us up the queue (assume the cancelled size was ahead of us).
    Optimistic,
}

impl Default for QueueModel {
    fn default() -> Self {
        QueueModel::Pessimistic
    }
}

/// Configuration for the maker-queue model (the `[execution.queue]` block). With the default
/// ([`QueueModel::Pessimistic`]) this is a complete no-op and runs reproduce exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueConfig {
    /// Which queue-position model to use. Default `pessimistic` (original behaviour).
    #[serde(default)]
    pub model: QueueModel,
}

impl Default for QueueConfig {
    fn default() -> Self {
        QueueConfig {
            model: QueueModel::default(),
        }
    }
}

/// Hard, engine-enforced risk limits. Every field is an `Option<_>` defaulting to `None`, meaning
/// "disabled": with all fields `None` the risk layer is a complete no-op and a run reproduces the
/// pre-risk-layer behaviour byte-for-byte.
///
/// There are two kinds of control:
/// * **Order/position clamps** ([`max_order_qty`], [`max_position_per_instrument`],
///   [`max_gross_position`]): applied as each strategy order is drained. They *reduce* an order's
///   qty (or drop it, counting a rejection) so a cap can never be breached. An order that only
///   *reduces* / flattens an existing position is NEVER blocked — de-risking is always allowed.
/// * **Equity HALT** ([`equity_floor`], [`max_drawdown_pct`]): checked every step. On breach the
///   engine HALTS — cancels all resting orders, flattens every open position with latency-bypassing
///   market orders, and ignores all further strategy orders for the rest of the run.
///
/// [`max_order_qty`]: RiskConfig::max_order_qty
/// [`max_position_per_instrument`]: RiskConfig::max_position_per_instrument
/// [`max_gross_position`]: RiskConfig::max_gross_position
/// [`equity_floor`]: RiskConfig::equity_floor
/// [`max_drawdown_pct`]: RiskConfig::max_drawdown_pct
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Cap a single order's contract qty. `None` = uncapped.
    #[serde(default)]
    pub max_order_qty: Option<f64>,
    /// Cap `|net_qty|` for any one instrument. An order that would push `|net|` past this is reduced
    /// so the resulting `|net|` exactly hits the cap; position-reducing orders are exempt.
    #[serde(default)]
    pub max_position_per_instrument: Option<f64>,
    /// Cap `Σ |net_qty|` (gross) across all instruments. Opening orders are reduced to keep gross at
    /// or below this; position-reducing orders are exempt.
    #[serde(default)]
    pub max_gross_position: Option<f64>,
    /// If equity ≤ this at any step, HALT (cancel + flatten + ignore further strategy orders).
    #[serde(default)]
    pub equity_floor: Option<f64>,
    /// If drawdown from the running equity peak ≥ this percent (e.g. `50.0` = 50%) at any step, HALT.
    #[serde(default)]
    pub max_drawdown_pct: Option<f64>,
}

impl Default for RiskConfig {
    fn default() -> Self {
        RiskConfig {
            max_order_qty: None,
            max_position_per_instrument: None,
            max_gross_position: None,
            equity_floor: None,
            max_drawdown_pct: None,
        }
    }
}

impl RiskConfig {
    /// True if any limit is set (so the engine should run its risk checks at all). With everything
    /// `None` this is false and the engine takes the original, check-free path.
    pub fn any_enabled(&self) -> bool {
        self.max_order_qty.is_some()
            || self.max_position_per_instrument.is_some()
            || self.max_gross_position.is_some()
            || self.equity_floor.is_some()
            || self.max_drawdown_pct.is_some()
    }
}

/// STOCHASTIC (distributional) per-order LATENCY model. Each variant describes how the *order*
/// latency (ns) for a single order is produced. The chosen variant REPLACES the
/// `order_latency_ns + hash_jitter` term of the model; `market_data_latency_ns` is always still
/// added on top, and the total is clamped to ≥ 0. See [`crate::latency::LatencyModel`].
///
/// Tagged serde representation: in TOML/JSON a dist is written as
/// `{ "kind": "uniform", "min_ns": ..., "max_ns": ... }` (the tag field is `kind`). The default is
/// [`LatencyDist::Fixed`], which is the legacy deterministic hash-jitter model (NO RNG) — so omitting
/// the dist entirely reproduces today's behaviour byte-for-byte.
///
/// Real order latency is not a constant — it varies. Modeling it as a distribution (with a `seed` for
/// reproducibility, see [`LatencyConfig::seed`]) lets a researcher stress-test whether an edge
/// survives realistic latency *variance*, not just a fixed delay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LatencyDist {
    /// DEFAULT — the legacy deterministic model: `order_latency_ns + hash_jitter(seq)`, where the
    /// jitter is a pure function of the order's sequence number (NO RNG). Byte-for-byte identical to
    /// the original. This is what you get when no distribution is configured.
    Fixed,
    /// Sample the order latency uniformly in `[min_ns, max_ns]` (inclusive; swapped if reversed).
    Uniform {
        #[serde(default)]
        min_ns: i64,
        #[serde(default)]
        max_ns: i64,
    },
    /// Sample from a Normal(mean_ns, std_ns) via Box-Muller, clamped to ≥ 0.
    Normal {
        #[serde(default)]
        mean_ns: i64,
        #[serde(default)]
        std_ns: i64,
    },
    /// Sample from an Exponential with the given mean (heavy-ish tail; realistic for network latency).
    Exponential {
        #[serde(default)]
        mean_ns: i64,
    },
    /// Load a newline/CSV list of latency-ns samples from `path` ONCE and sample WITH REPLACEMENT
    /// (replays real measured latencies). Falls back to [`LatencyDist::Fixed`] if the file is
    /// missing/empty/unreadable.
    Empirical {
        #[serde(default)]
        path: String,
    },
}

impl Default for LatencyDist {
    fn default() -> Self {
        LatencyDist::Fixed
    }
}

/// Default seed for the stochastic-latency PRNG. A fixed constant so that, absent an explicit
/// `--latency-seed`, a distributional run is still reproducible (and the same across invocations).
/// Kept within `i64::MAX` so it round-trips through TOML (whose integers are signed 64-bit).
pub const DEFAULT_LATENCY_SEED: u64 = 0x0123_4567_89AB_CDEF;

fn default_latency_seed() -> u64 {
    DEFAULT_LATENCY_SEED
}

/// Latency model parameters. When `enabled` is false (default) every latency is zero and orders
/// are matchable immediately, exactly reproducing the original zero-latency behaviour.
///
/// `jitter_ns` adds a *deterministic* pseudo-random spread to each order's activation time under the
/// default [`LatencyDist::Fixed`] distribution; it is derived from the order's sequence number (NO
/// RNG), so default runs are fully reproducible.
///
/// To model latency as a DISTRIBUTION instead, set `dist` to a non-`Fixed` [`LatencyDist`]. Then the
/// per-order latency is *sampled* from that distribution using a SEEDED PRNG keyed off `seed`, so a
/// run is "deterministic given inputs + flags + seed": the same seed reproduces the run exactly, a
/// different seed gives a different (but itself reproducible) run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyConfig {
    /// Master enable. False => all latencies treated as zero.
    #[serde(default)]
    pub enabled: bool,
    /// Nanoseconds between a strategy placing an order and it becoming matchable. Used as the base/
    /// mean by the [`LatencyDist::Fixed`] distribution; ignored by the other distributions, which
    /// carry their own parameters.
    #[serde(default)]
    pub order_latency_ns: i64,
    /// Nanoseconds between a strategy requesting a cancel and it taking effect. Always a flat value
    /// (cancels do not use a distribution).
    #[serde(default)]
    pub cancel_latency_ns: i64,
    /// Nanoseconds the strategy's view of the book lags reality (market-data latency). Modeled by
    /// adding this to the effective activation delay of orders the strategy places in reaction —
    /// under EVERY distribution.
    #[serde(default)]
    pub market_data_latency_ns: i64,
    /// Peak magnitude (ns) of the deterministic pseudo-jitter added to each order's activation under
    /// the default [`LatencyDist::Fixed`] distribution. Ignored by the other distributions.
    #[serde(default)]
    pub jitter_ns: i64,
    /// The per-order latency DISTRIBUTION. Default [`LatencyDist::Fixed`] (the legacy hash-jitter
    /// model) reproduces today's behaviour exactly. See [`LatencyDist`].
    #[serde(default)]
    pub dist: LatencyDist,
    /// Seed for the stochastic-latency PRNG. Same seed => identical sampled-latency sequence (and
    /// hence an identical run); a different seed => a different but reproducible run. Unused by the
    /// default `Fixed` distribution (which has no RNG). Defaults to [`DEFAULT_LATENCY_SEED`].
    #[serde(default = "default_latency_seed")]
    pub seed: u64,
}

impl Default for LatencyConfig {
    fn default() -> Self {
        LatencyConfig {
            enabled: false,
            order_latency_ns: 0,
            cancel_latency_ns: 0,
            market_data_latency_ns: 0,
            jitter_ns: 0,
            dist: LatencyDist::Fixed,
            seed: DEFAULT_LATENCY_SEED,
        }
    }
}

/// Adverse-slippage model. When `enabled` is false (default) fills are exactly as today.
///
/// Taker fills are worsened by `taker_ticks` cents and/or `taker_bps` of notional. The extra cost
/// is tracked separately (see [`crate::portfolio::Portfolio::total_slippage_cost`]) rather than
/// silently folded into the fill price, so it can be reported on its own line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlippageConfig {
    /// Master enable. False => no slippage applied.
    #[serde(default)]
    pub enabled: bool,
    /// Extra cents of adverse price movement on each taker fill (a "tick" = 1 cent on Kalshi).
    #[serde(default)]
    pub taker_ticks: i32,
    /// Extra adverse cost as a fraction of notional on each taker fill (e.g. 0.0005 = 5 bps).
    #[serde(default)]
    pub taker_bps: f64,
    /// Adverse-selection cost charged on maker fills, as a fraction of notional. Models the fact
    /// that a resting quote tends to get filled exactly when the market is about to move against it.
    #[serde(default)]
    pub maker_adverse_selection_bps: f64,
}

impl Default for SlippageConfig {
    fn default() -> Self {
        SlippageConfig {
            enabled: false,
            taker_ticks: 0,
            taker_bps: 0.0,
            maker_adverse_selection_bps: 0.0,
        }
    }
}

/// Kalshi liquidity-incentive (maker rewards) model. When `enabled` is false (default) no rewards
/// accrue. Modeled on Kalshi's published liquidity programs: a market-maker earns a share of a
/// per-period reward pool while resting at least `min_resting_size` contracts within
/// `max_spread_cents` of the mid (optionally on BOTH sides), pro-rated by qualifying time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RewardsConfig {
    /// Master enable. False => no rewards accrue.
    #[serde(default)]
    pub enabled: bool,
    /// Length of one reward window in seconds (e.g. 3600 for hourly).
    #[serde(default = "default_period_secs")]
    pub period_secs: i64,
    /// Total reward (dollars) paid out per period to the modeled participant when fully qualifying.
    #[serde(default)]
    pub reward_per_period: f64,
    /// Minimum resting contracts required (on each required side) to qualify.
    #[serde(default = "default_min_resting")]
    pub min_resting_size: f64,
    /// Quotes must rest within this many cents of the mid to qualify.
    #[serde(default = "default_max_spread")]
    pub max_spread_cents: i32,
    /// If true, must rest qualifying size on BOTH bid and ask simultaneously to earn.
    #[serde(default = "default_true")]
    pub both_sides_required: bool,
}

impl Default for RewardsConfig {
    fn default() -> Self {
        RewardsConfig {
            enabled: false,
            period_secs: default_period_secs(),
            reward_per_period: 0.0,
            min_resting_size: default_min_resting(),
            max_spread_cents: default_max_spread(),
            both_sides_required: true,
        }
    }
}

fn default_period_secs() -> i64 {
    3600
}
fn default_min_resting() -> f64 {
    10.0
}
fn default_max_spread() -> i32 {
    4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_execution_is_a_noop() {
        let e = ExecutionConfig::default();
        assert!(e.include_fees);
        assert!(!e.include_rewards);
        assert!(!e.latency.enabled);
        assert!(!e.slippage.enabled);
        assert!(!e.rewards.enabled);
        assert!(!e.risk.any_enabled());
        // queue model defaults to pessimistic (== original behaviour)
        assert_eq!(e.queue.model, QueueModel::Pessimistic);
        // settlement disabled by default (no path) == flatten-at-mid as before
        assert!(!e.settlement.is_enabled());
        assert!(e.settlement.path.is_none());
    }

    #[test]
    fn settlement_config_defaults_and_roundtrips() {
        // omitted [execution.settlement] block => disabled
        let e: ExecutionConfig = serde_json::from_str(r#"{}"#).unwrap();
        assert!(!e.settlement.is_enabled());
        // an explicit path enables it and roundtrips
        let e: ExecutionConfig =
            serde_json::from_str(r#"{"settlement":{"path":"s.csv"}}"#).unwrap();
        assert!(e.settlement.is_enabled());
        assert_eq!(e.settlement.path.as_deref(), Some("s.csv"));
        let s = serde_json::to_string(&e).unwrap();
        let back: ExecutionConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.settlement, e.settlement);
    }

    #[test]
    fn queue_config_defaults_and_roundtrips() {
        // default is pessimistic
        assert_eq!(QueueConfig::default().model, QueueModel::Pessimistic);
        // an omitted [execution.queue] block defaults to pessimistic
        let e: ExecutionConfig = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(e.queue.model, QueueModel::Pessimistic);
        // explicit optimistic parses (lowercase serde rename)
        let e: ExecutionConfig =
            serde_json::from_str(r#"{"queue":{"model":"optimistic"}}"#).unwrap();
        assert_eq!(e.queue.model, QueueModel::Optimistic);
        // full roundtrip
        let s = serde_json::to_string(&e).unwrap();
        let back: ExecutionConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.queue, e.queue);
    }

    #[test]
    fn default_risk_is_all_none_and_disabled() {
        let r = RiskConfig::default();
        assert!(r.max_order_qty.is_none());
        assert!(r.max_position_per_instrument.is_none());
        assert!(r.max_gross_position.is_none());
        assert!(r.equity_floor.is_none());
        assert!(r.max_drawdown_pct.is_none());
        assert!(!r.any_enabled());
    }

    #[test]
    fn risk_config_roundtrips_and_partial_fills_none() {
        // A partial [execution.risk] block: only set two fields; the rest default to None.
        let e: ExecutionConfig = serde_json::from_str(
            r#"{"risk":{"max_gross_position":100.0,"equity_floor":0.0}}"#,
        )
        .unwrap();
        assert_eq!(e.risk.max_gross_position, Some(100.0));
        assert_eq!(e.risk.equity_floor, Some(0.0));
        assert!(e.risk.max_order_qty.is_none());
        assert!(e.risk.any_enabled());
        // full roundtrip
        let s = serde_json::to_string(&e).unwrap();
        let back: ExecutionConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.risk, e.risk);
    }

    #[test]
    fn execution_config_roundtrips_through_json() {
        let e = ExecutionConfig {
            include_fees: false,
            include_rewards: true,
            latency: LatencyConfig {
                enabled: true,
                order_latency_ns: 500_000_000,
                cancel_latency_ns: 100,
                market_data_latency_ns: 200,
                jitter_ns: 50,
                ..Default::default()
            },
            slippage: SlippageConfig {
                enabled: true,
                taker_ticks: 1,
                taker_bps: 0.0005,
                maker_adverse_selection_bps: 0.0002,
            },
            rewards: RewardsConfig {
                enabled: true,
                period_secs: 3600,
                reward_per_period: 5.0,
                min_resting_size: 10.0,
                max_spread_cents: 4,
                both_sides_required: true,
            },
            risk: RiskConfig::default(),
            queue: QueueConfig::default(),
            settlement: SettlementConfig::default(),
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: ExecutionConfig = serde_json::from_str(&s).unwrap();
        assert!(!back.include_fees);
        assert!(back.include_rewards);
        assert_eq!(back.latency.order_latency_ns, 500_000_000);
        assert_eq!(back.slippage.taker_ticks, 1);
        assert_eq!(back.rewards.reward_per_period, 5.0);
    }

    #[test]
    fn partial_json_fills_defaults() {
        // Only set one field; serde defaults the rest.
        let e: ExecutionConfig = serde_json::from_str(r#"{"include_rewards":true}"#).unwrap();
        assert!(e.include_fees); // defaulted true
        assert!(e.include_rewards);
        assert_eq!(e.rewards.period_secs, 3600);
    }

    #[test]
    fn latency_dist_defaults_to_fixed_and_seed_to_constant() {
        // An omitted [execution.latency] block => Fixed dist + default seed.
        let e: ExecutionConfig = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(e.latency.dist, LatencyDist::Fixed);
        assert_eq!(e.latency.seed, DEFAULT_LATENCY_SEED);
        // A latency block that omits dist/seed still defaults them.
        let e: ExecutionConfig =
            serde_json::from_str(r#"{"latency":{"enabled":true,"order_latency_ns":5}}"#).unwrap();
        assert_eq!(e.latency.dist, LatencyDist::Fixed);
        assert_eq!(e.latency.seed, DEFAULT_LATENCY_SEED);
    }

    #[test]
    fn latency_dist_tagged_serde_roundtrips() {
        // Each variant parses from its tagged form and roundtrips.
        let cases = [
            r#"{"latency":{"enabled":true,"dist":{"kind":"fixed"}}}"#,
            r#"{"latency":{"enabled":true,"dist":{"kind":"uniform","min_ns":1,"max_ns":9}}}"#,
            r#"{"latency":{"enabled":true,"dist":{"kind":"normal","mean_ns":5,"std_ns":2}}}"#,
            r#"{"latency":{"enabled":true,"dist":{"kind":"exponential","mean_ns":7}}}"#,
            r#"{"latency":{"enabled":true,"dist":{"kind":"empirical","path":"x.txt"},"seed":42}}"#,
        ];
        for c in cases {
            let e: ExecutionConfig = serde_json::from_str(c).unwrap();
            let s = serde_json::to_string(&e).unwrap();
            let back: ExecutionConfig = serde_json::from_str(&s).unwrap();
            assert_eq!(back.latency, e.latency, "roundtrip mismatch for {c}");
        }
        // Spot-check the parsed values + an explicit seed.
        let e: ExecutionConfig = serde_json::from_str(
            r#"{"latency":{"enabled":true,"dist":{"kind":"uniform","min_ns":1,"max_ns":9},"seed":42}}"#,
        )
        .unwrap();
        assert_eq!(
            e.latency.dist,
            LatencyDist::Uniform { min_ns: 1, max_ns: 9 }
        );
        assert_eq!(e.latency.seed, 42);
    }
}
