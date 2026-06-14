//! Shared vocabulary for the backtester. This is the FIXED CONTRACT every module builds against.
//! Prices are integer cents in [1, 99] internally (Kalshi binary contracts pay $1 / $0).

use serde::{Deserialize, Serialize};

/// Price level in integer cents, 1..=99. A YES contract at `Cents(c)` costs $c/100.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Cents(pub i32);

impl Cents {
    #[inline]
    pub fn to_dollars(self) -> f64 {
        self.0 as f64 / 100.0
    }
    /// Build from a dollar price like 0.42 -> Cents(42). Rounds to nearest cent.
    #[inline]
    pub fn from_dollars(p: f64) -> Cents {
        Cents((p * 100.0).round() as i32)
    }
    /// Complementary price: a NO level at price `p` maps to a YES level at `1 - p` (i.e. 100−p in
    /// cents), since a YES and NO contract on the same outcome always sum to $1.
    ///
    /// DESIGN NOTE — a helper for CONVERTERS/COLLECTORS, intentionally NOT applied in the loaders.
    /// The canonical/ingested data this backtester reads is already YES-NATIVE: a NO bid at `q` has
    /// already been stored as a YES ask at `1 − q` by the upstream collector/converter (see
    /// `src/data` and `src/adapters`). Applying `complement()` again inside a loader would
    /// DOUBLE-complement and corrupt every price. This method exists so adapters that ingest a
    /// NO-side feed can normalize it to the YES-native book at the boundary; it is deliberately
    /// unused by the loaders, which already receive complemented prices.
    #[inline]
    pub fn complement(self) -> Cents {
        Cents(100 - self.0)
    }
}

/// Side of the YES-native book. `Bid` = someone buying YES; `Ask` = someone selling YES.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    #[inline]
    pub fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

/// Delta action, mirroring infra-orchestrator `orderbook_deltas.action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Add,
    Update,
    Delete,
}

/// One orderbook delta row (== a row of the `orderbook_deltas` table).
#[derive(Debug, Clone, PartialEq)]
pub struct BookDelta {
    pub ts_ns: i64,
    pub instrument: String,
    pub action: Action,
    pub side: Side,
    pub price: Cents,
    /// Resting contracts AT this level after the update (Add/Update); ignored for Delete.
    pub size: f64,
    pub sequence: i64,
    /// True only for the first row of a full-book snapshot -> book must reset before applying.
    pub is_snapshot: bool,
}

/// One executed trade (== a row of the `trades` table).
#[derive(Debug, Clone, PartialEq)]
pub struct TradeEvent {
    pub ts_ns: i64,
    pub instrument: String,
    /// True if the aggressor took the YES side.
    pub aggressor_yes: bool,
    pub price: Cents,
    pub size: f64,
    pub trade_id: String,
}

/// The unified event stream the engine consumes (time-ordered).
#[derive(Debug, Clone)]
pub enum MarketEvent {
    Delta(BookDelta),
    Trade(TradeEvent),
}

impl MarketEvent {
    #[inline]
    pub fn ts_ns(&self) -> i64 {
        match self {
            MarketEvent::Delta(d) => d.ts_ns,
            MarketEvent::Trade(t) => t.ts_ns,
        }
    }
    #[inline]
    pub fn instrument(&self) -> &str {
        match self {
            MarketEvent::Delta(d) => &d.instrument,
            MarketEvent::Trade(t) => &t.instrument,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderKind {
    Limit,
    Market,
}

/// Time-in-force for a limit order — how long the order may live and whether its marketable
/// (crossing) portion is allowed to rest after taking.
///
/// * [`Tif::Gtc`] (Good-Til-Cancelled): the default limit semantics. If the order is marketable at
///   activation, its crossing portion takes immediately (as a taker, bounded by the limit price)
///   and any remainder RESTS as a maker order. If it is not marketable it simply rests.
/// * [`Tif::Ioc`] (Immediate-Or-Cancel): only the marketable portion fills immediately (taker,
///   bounded by the limit price); the unfilled remainder is CANCELLED and never rests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tif {
    /// Good-Til-Cancelled — take the crossing portion, rest the remainder (standard limit).
    Gtc,
    /// Immediate-Or-Cancel — take the crossing portion, cancel the remainder (never rests).
    Ioc,
}

impl Default for Tif {
    /// The default time-in-force is GTC, reproducing the historical `place_limit` behaviour.
    fn default() -> Self {
        Tif::Gtc
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Liquidity {
    Maker,
    Taker,
    /// A BINARY SETTLEMENT "fill" recorded when an open position is settled at expiry against the
    /// market's known outcome (a YES contract pays $1, a NO contract pays $0). It is not a real trade
    /// against the book: it has NO fee (Kalshi charges no settlement fee) and no slippage, and its
    /// price is the per-contract payout ($1.00 for YES, $0.00 for NO). See [`crate::engine::Engine`]
    /// `finalize` and [`crate::settlement`].
    Settle,
}

/// A read-only view of a resting strategy order, exposed to strategies via `Ctx::open_orders`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrderView {
    pub id: u64,
    pub side: Side,
    pub price: Cents,
    pub remaining: f64,
}

/// A fill produced by the execution model.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    pub ts_ns: i64,
    pub order_id: u64,
    pub instrument: String,
    pub side: Side,
    pub price: Cents,
    pub qty: f64,
    pub liquidity: Liquidity,
    pub fee: f64,
}

// ----------------------------------------------------------------------------
// report.json — MUST stay compatible with oracletrading/infra-orchestrator.
// Consumed by bot/graph_command.py: top-level {plugin_name, summary, equity_curve}.
// Printed between ===REPORT_JSON_START=== / ===REPORT_JSON_END=== sentinels.
// ----------------------------------------------------------------------------

pub const REPORT_JSON_START: &str = "===REPORT_JSON_START===";
pub const REPORT_JSON_END: &str = "===REPORT_JSON_END===";
pub const TEARSHEET_B64_START: &str = "===TEARSHEET_HTML_B64_START===";
pub const TEARSHEET_B64_END: &str = "===TEARSHEET_HTML_B64_END===";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EquityPoint {
    pub ts_ns: i64,
    pub total: f64,
    pub currency: String,
}

/// `summary` block. The first 9 fields are exactly what infra-orchestrator reads; the rest are
/// additive (their consumers ignore unknown keys) and give a real quant tearsheet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub currency: String,
    pub starting_balance: f64,
    pub ending_balance: f64,
    pub pnl_total: f64,
    pub pnl_pct: f64,
    pub total_orders: i64,
    pub total_positions: i64,
    pub avg_buy_price: f64,
    pub avg_sell_price: f64,
    // ---- additive analytics ----
    pub num_trades: i64,
    pub num_fills: i64,
    pub win_rate: f64,
    pub sharpe: f64,
    pub sortino: f64,
    pub max_drawdown: f64,
    pub max_drawdown_pct: f64,
    pub turnover: f64,
    pub total_fees: f64,
    // ---- round-trip / trade analytics (additive) ----
    pub profit_factor: f64,
    pub gross_profit: f64,
    pub gross_loss: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    pub payoff_ratio: f64,
    pub expectancy: f64,
    pub num_round_trips: i64,
    pub avg_trade_pnl: f64,
    pub largest_win: f64,
    pub largest_loss: f64,
    pub max_consecutive_wins: i64,
    pub max_consecutive_losses: i64,
    // ---- risk / return analytics (additive) ----
    pub calmar_ratio: f64,
    pub volatility: f64,
    pub downside_volatility: f64,
    pub exposure_pct: f64,
    pub avg_holding_secs: f64,
    pub fees_pct_of_gross: f64,
    pub total_volume_contracts: f64,
    // ---- execution-cost decomposition (additive) ----
    /// Total adverse-slippage cost (dollars) charged on fills (0 when slippage disabled).
    #[serde(default)]
    pub total_slippage_cost: f64,
    /// Total accrued Kalshi liquidity-incentive rewards (dollars). Reported even when not credited.
    #[serde(default)]
    pub liquidity_rewards: f64,
    /// Gross trading PnL before fees and slippage, and excluding rewards. This is the pure
    /// price-movement PnL: `pnl_total - liquidity_rewards (if credited) + total_fees + total_slippage_cost`.
    #[serde(default)]
    pub gross_pnl_ex_costs: f64,
    // ---- binary settlement-at-expiry (additive) ----
    /// Total realized PnL (dollars) booked from SETTLING open positions at expiry against their
    /// known outcome — i.e. `Σ q * (payout − avg_cost)` over every settled position, where
    /// `payout` is $1.00 for a YES resolution and $0.00 for NO. Zero when no settlement map was
    /// provided (the default), in which case positions are flattened at mid exactly as before.
    #[serde(default)]
    pub settled_pnl: f64,
    /// Number of open positions that were SETTLED at expiry (rather than flattened at mid). Zero
    /// when no settlement map was provided. See [`crate::engine::Engine`] `finalize`.
    #[serde(default)]
    pub num_settled: i64,
    // ---- risk-layer status (additive) ----
    /// True if the engine-enforced risk layer HALTED the run (equity floor or max-drawdown breach).
    #[serde(default)]
    pub halted: bool,
    /// Human-readable reason for the halt (empty when not halted).
    #[serde(default)]
    pub halt_reason: String,
    /// Count of orders the risk layer dropped or clamped to zero qty.
    #[serde(default)]
    pub risk_rejections: i64,
    /// Count of `post_only` limit orders REJECTED because they were marketable (would cross) at
    /// activation time. A post-only order guarantees maker-only execution, so a marketable one is
    /// dropped rather than allowed to take — this counts those drops. 0 unless a strategy uses
    /// `place_limit_ex(.., post_only=true)`.
    #[serde(default)]
    pub post_only_rejects: i64,
}

/// Per-instrument breakdown row in the report (mirrors `portfolio::InstrumentStat`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstrumentBreakdown {
    pub instrument: String,
    pub pnl: f64,
    pub num_fills: i64,
    pub num_round_trips: i64,
    pub net_position: f64,
    pub volume: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub plugin_name: String,
    pub summary: Summary,
    pub equity_curve: Vec<EquityPoint>,
    /// Additive: per-instrument PnL/fills/round-trips/position/volume breakdown.
    #[serde(default)]
    pub instrument_breakdown: Vec<InstrumentBreakdown>,
}
