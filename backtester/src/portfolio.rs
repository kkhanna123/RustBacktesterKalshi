//! Cash, positions, realized/unrealized PnL, and the equity-curve accumulator.

use crate::orderbook::BookSet;
use crate::types::{EquityPoint, Fill, Side};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Net position in one instrument with average cost and realized PnL.
#[derive(Debug, Clone, Default)]
pub struct Position {
    /// Signed contracts: + long YES, - short YES.
    pub net_qty: f64,
    /// Average cost per contract (dollars) of the current open position.
    pub avg_cost: f64,
    /// Realized PnL (dollars) accumulated from offsetting fills (net of fees on those fills).
    pub realized_pnl: f64,
    /// Timestamp (ns) the *current* open position was first opened (for holding-time stats).
    /// Reset whenever the position returns to flat.
    pub opened_ts: i64,
    /// Average entry price weighted by the qty that opened the current position (dollars).
    /// Used to populate a round-trip's `entry_price` when the position closes.
    pub entry_price: f64,
}

/// A completed round-trip: a position opening and later returning to flat (or flipping sign).
/// One round trip is recorded per offsetting (closing) slice; a partial close emits a round trip
/// for the closed quantity only, leaving the remainder open.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundTrip {
    pub instrument: String,
    /// Timestamp (ns) the closed position was opened.
    pub entry_ts: i64,
    /// Timestamp (ns) of the closing fill.
    pub exit_ts: i64,
    /// Contracts closed in this round trip (always positive).
    pub qty: f64,
    /// Average entry price (dollars) of the closed quantity.
    pub entry_price: f64,
    /// Exit price (dollars) of the closing fill.
    pub exit_price: f64,
    /// Net PnL (dollars) for this round trip, net of the closing fill's fee.
    pub pnl: f64,
}

/// Per-instrument aggregate stats surfaced in exports and the report breakdown.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstrumentStat {
    /// Realized PnL (dollars) accumulated across all closes for this instrument.
    pub pnl: f64,
    /// Number of fills touching this instrument.
    pub num_fills: i64,
    /// Number of completed round trips for this instrument.
    pub num_round_trips: i64,
    /// Current net signed position (contracts).
    pub net_position: f64,
    /// Total traded volume (contracts, buys + sells) for this instrument.
    pub volume: f64,
}

/// The whole account.
#[derive(Debug, Clone)]
pub struct Portfolio {
    pub cash: f64,
    /// Open positions keyed by instrument.
    ///
    /// DETERMINISM FIX: this is a [`BTreeMap`] (NOT a `HashMap`) so iteration is in **sorted key
    /// order** everywhere, on every process. `equity()`, `unrealized()`, and the engine's
    /// gross-position sum all fold over these entries; float addition is non-associative, so a
    /// `HashMap`'s per-process-randomized iteration order would perturb the last bits of every
    /// equity snapshot. Those snapshots feed Sharpe / drawdown / turnover and the `optimize` /
    /// `walk-forward` ranking (which selects on a strict `>`), so with ≥2 instruments held
    /// simultaneously two byte-identical runs could otherwise rank differently. A `BTreeMap` makes
    /// the summation order a deterministic function of the instrument ids, killing that
    /// nondeterminism with no change to observable behaviour for single-instrument runs.
    pub positions: BTreeMap<String, Position>,

    // ---- running totals for the report ----
    pub total_orders: i64,
    pub total_fills: i64,
    pub total_fees: f64,
    /// Total adverse-slippage cost (dollars) charged on fills, tracked separately from price/fees.
    pub total_slippage_cost: f64,
    /// Total accrued Kalshi liquidity-incentive rewards (dollars). Only added to cash if credited.
    pub liquidity_rewards: f64,
    /// If false, fees are recorded on individual fills for inspection but are NOT debited from cash
    /// and do NOT accumulate into `total_fees` (so PnL is gross of fees).
    pub include_fees: bool,
    pub buy_notional: f64,
    pub buy_qty: f64,
    pub sell_notional: f64,
    pub sell_qty: f64,
    /// Realized PnL of each closed round-trip slice (for win-rate).
    pub round_trip_pnls: Vec<f64>,
    /// Full round-trip records (instrument, entry/exit ts, qty, prices, pnl).
    pub round_trips: Vec<RoundTrip>,
    /// Per-instrument aggregate stats.
    pub instrument_stats: HashMap<String, InstrumentStat>,
    /// Total traded volume across all instruments (contracts, buys + sells).
    pub total_volume: f64,
    /// Sum of holding times (seconds) across completed round trips (for avg_holding_secs).
    pub holding_secs_sum: f64,
    /// Count of round trips contributing to `holding_secs_sum`.
    pub holding_count: i64,
    /// Number of equity snapshots taken while holding a nonzero position (for exposure_pct).
    pub snapshots_with_position: i64,
    /// Full chronological log of strategy fills (for the dashboard `fills.csv` export).
    pub fills: Vec<Fill>,

    // ---- engine-enforced risk-layer status (set by the engine; reported) ----
    /// True if the risk layer HALTED the run (equity floor / max-drawdown breach).
    pub halted: bool,
    /// Human-readable halt reason (empty when not halted).
    pub halt_reason: String,
    /// Count of orders the risk layer dropped or clamped to zero qty.
    pub risk_rejections: i64,
    /// Count of `post_only` limit orders rejected because they were marketable at activation. Set by
    /// the engine (see [`crate::strategy::Ctx::place_limit_ex`]); 0 unless a strategy uses post-only.
    pub post_only_rejects: i64,

    // ---- binary settlement-at-expiry status (set by the engine in finalize; reported) ----
    /// Total realized PnL (dollars) booked from SETTLING open positions at expiry to their $1/$0
    /// binary payoff. 0 when no settlement map was provided. See [`Portfolio::settle_position`].
    pub settled_pnl: f64,
    /// Number of open positions SETTLED at expiry (rather than flattened at mid). 0 by default.
    pub num_settled: i64,

    // ---- equity-curve accumulator ----
    currency: String,
    snapshot_secs: i64,
    last_snapshot_ts: i64,
    pub equity_curve: Vec<EquityPoint>,
}

impl Portfolio {
    pub fn new(starting_cash: f64, currency: String, snapshot_secs: i64) -> Self {
        Portfolio {
            cash: starting_cash,
            positions: BTreeMap::new(),
            total_orders: 0,
            total_fills: 0,
            total_fees: 0.0,
            total_slippage_cost: 0.0,
            liquidity_rewards: 0.0,
            include_fees: true,
            buy_notional: 0.0,
            buy_qty: 0.0,
            sell_notional: 0.0,
            sell_qty: 0.0,
            round_trip_pnls: Vec::new(),
            round_trips: Vec::new(),
            instrument_stats: HashMap::new(),
            total_volume: 0.0,
            holding_secs_sum: 0.0,
            holding_count: 0,
            snapshots_with_position: 0,
            fills: Vec::new(),
            halted: false,
            halt_reason: String::new(),
            risk_rejections: 0,
            post_only_rejects: 0,
            settled_pnl: 0.0,
            num_settled: 0,
            currency,
            snapshot_secs: snapshot_secs.max(0),
            last_snapshot_ts: i64::MIN,
            equity_curve: Vec::new(),
        }
    }

    /// Apply a fill with no extra slippage cost. See [`Portfolio::apply_fill_ex`].
    pub fn apply_fill(&mut self, fill: &Fill) {
        self.apply_fill_ex(fill, 0.0);
    }

    /// Apply a fill: move cash, update position/avg-cost, realize PnL on offset, accrue totals,
    /// and record round-trips + per-instrument stats.
    ///
    /// `slippage_cost` is an extra adverse dollar cost (>= 0) that always reduces cash and is
    /// tracked on its own line ([`Portfolio::total_slippage_cost`]); it is also netted into the
    /// realized PnL of any closing slice this fill produces.
    ///
    /// When `include_fees` is false, `fill.fee` is left on the recorded fill (for inspection) but
    /// is NOT debited from cash and does NOT accumulate into `total_fees`, so PnL is gross of fees.
    pub fn apply_fill_ex(&mut self, fill: &Fill, slippage_cost: f64) {
        let price = fill.price.to_dollars();
        let qty = fill.qty;
        let notional = price * qty;
        let ts = fill.ts_ns;
        let slip = slippage_cost.max(0.0);

        // Effective fee actually charged to cash/PnL (zeroed when fees are excluded).
        let fee = if self.include_fees { fill.fee } else { 0.0 };

        self.total_fills += 1;
        self.total_fees += fee;
        self.total_slippage_cost += slip;
        self.total_volume += qty;
        self.fills.push(fill.clone());

        match fill.side {
            Side::Bid => {
                self.cash -= notional + fee + slip;
                self.buy_notional += notional;
                self.buy_qty += qty;
            }
            Side::Ask => {
                self.cash += notional - fee - slip;
                self.sell_notional += notional;
                self.sell_qty += qty;
            }
        }

        // per-instrument fill/volume accounting
        {
            let st = self
                .instrument_stats
                .entry(fill.instrument.clone())
                .or_default();
            st.num_fills += 1;
            st.volume += qty;
        }

        let pos = self.positions.entry(fill.instrument.clone()).or_default();
        // signed delta to the position
        let signed = match fill.side {
            Side::Bid => qty,
            Side::Ask => -qty,
        };
        let old_qty = pos.net_qty;
        let new_qty = old_qty + signed;

        let same_direction = old_qty == 0.0 || (old_qty > 0.0) == (signed > 0.0);
        if same_direction {
            // growing (or opening) the position: blend avg cost
            if old_qty.abs() < 1e-12 {
                // opening a fresh position: stamp the entry time/price
                pos.opened_ts = ts;
                pos.entry_price = price;
            } else {
                // blend the entry price by qty added
                let total_qty = old_qty.abs() + qty;
                pos.entry_price = (pos.entry_price * old_qty.abs() + price * qty) / total_qty;
            }
            let total_cost = pos.avg_cost * old_qty.abs() + price * qty;
            let total_qty = old_qty.abs() + qty;
            pos.avg_cost = if total_qty > 0.0 {
                total_cost / total_qty
            } else {
                0.0
            };
            pos.net_qty = new_qty;
        } else {
            // reducing / flipping: realize PnL on the offset portion
            let closing = qty.min(old_qty.abs());
            // For a long being sold: pnl = (sell_price - avg_cost) * closing.
            // For a short being bought: pnl = (avg_cost - buy_price) * closing.
            let pnl = if old_qty > 0.0 {
                (price - pos.avg_cost) * closing
            } else {
                (pos.avg_cost - price) * closing
            };
            pos.realized_pnl += pnl;
            // Net the effective (possibly fee-excluded) fee AND the slippage cost into round-trip PnL.
            let net_pnl = pnl - fee - slip;
            self.round_trip_pnls.push(net_pnl);

            // record the round-trip
            let entry_ts = pos.opened_ts;
            let entry_price = pos.entry_price;
            self.round_trips.push(RoundTrip {
                instrument: fill.instrument.clone(),
                entry_ts,
                exit_ts: ts,
                qty: closing,
                entry_price,
                exit_price: price,
                pnl: net_pnl,
            });
            if ts >= entry_ts {
                self.holding_secs_sum += (ts - entry_ts) as f64 / 1_000_000_000.0;
                self.holding_count += 1;
            }

            // per-instrument pnl + round-trip count
            {
                let st = self
                    .instrument_stats
                    .entry(fill.instrument.clone())
                    .or_default();
                st.pnl += net_pnl;
                st.num_round_trips += 1;
            }

            if qty.abs() <= old_qty.abs() + 1e-12 {
                // fully or partially closed, no flip
                pos.net_qty = new_qty;
                if pos.net_qty.abs() < 1e-12 {
                    pos.net_qty = 0.0;
                    pos.avg_cost = 0.0;
                    pos.opened_ts = 0;
                    pos.entry_price = 0.0;
                }
            } else {
                // flipped through zero: remaining opens a new position at this price/time
                pos.net_qty = new_qty;
                pos.avg_cost = price;
                pos.opened_ts = ts;
                pos.entry_price = price;
            }
        }

        // keep per-instrument net_position in sync with the position book
        let net = self
            .positions
            .get(&fill.instrument)
            .map(|p| p.net_qty)
            .unwrap_or(0.0);
        if let Some(st) = self.instrument_stats.get_mut(&fill.instrument) {
            st.net_position = net;
        }
    }

    /// BINARY SETTLEMENT AT EXPIRY of one open position.
    ///
    /// Settles the entire net position `q` (q>0 long YES, q<0 short YES) in `instrument` against the
    /// market's known per-contract `payout` (`1.0` if it resolved YES, `0.0` if NO). This credits
    /// cash `q * payout`, realizes PnL `q * (payout − avg_cost)` against the cost basis, records a
    /// settlement "fill" (liquidity [`Liquidity::Settle`], **fee 0** — Kalshi charges no settlement
    /// fee) at price = `payout` and timestamp `ts_ns` so exports / round-trip accounting capture it,
    /// and leaves the position FLAT.
    ///
    /// Returns the realized settlement PnL (`q * (payout − avg_cost)`), or `0.0` if the position was
    /// already flat. The settlement fill goes through the normal [`Portfolio::apply_fill_ex`] path
    /// (with zero fee and zero slippage), so it offsets the position, books the round-trip, updates
    /// per-instrument stats, and keeps cash/PnL consistent — exactly as a closing trade at the
    /// payout price would, just with no fee.
    pub fn settle_position(&mut self, instrument: &str, payout: f64, ts_ns: i64) -> f64 {
        let q = self
            .positions
            .get(instrument)
            .map(|p| p.net_qty)
            .unwrap_or(0.0);
        if q.abs() < 1e-12 {
            return 0.0;
        }
        let avg_cost = self
            .positions
            .get(instrument)
            .map(|p| p.avg_cost)
            .unwrap_or(0.0);
        // Realized settlement PnL relative to cost basis.
        let settled_pnl = q * (payout - avg_cost);

        // A settlement closes the position: a long (q>0) "sells" at the payout (Side::Ask), a short
        // (q<0) "buys back" at the payout (Side::Bid). Routing it through apply_fill_ex with the
        // payout price reproduces this PnL while keeping cash, the round-trip log, and per-instrument
        // stats consistent. Fee is ZERO (no settlement fee) and there is no slippage.
        let side = if q > 0.0 { Side::Ask } else { Side::Bid };
        let fill = Fill {
            ts_ns,
            order_id: 0,
            instrument: instrument.to_string(),
            side,
            price: crate::types::Cents::from_dollars(payout),
            qty: q.abs(),
            liquidity: crate::types::Liquidity::Settle,
            fee: 0.0,
        };
        self.apply_fill_ex(&fill, 0.0);
        settled_pnl
    }

    /// Unrealized PnL marked at mid (or best available) per instrument.
    ///
    /// DETERMINISM: iterates `positions` (a [`BTreeMap`]) in sorted key order, so the float
    /// summation order is fixed across runs. See the `positions` field doc.
    pub fn unrealized(&self, books: &BookSet) -> f64 {
        let mut total = 0.0;
        for (inst, pos) in &self.positions {
            if pos.net_qty == 0.0 {
                continue;
            }
            let mark = mark_price(books, inst);
            if let Some(m) = mark {
                total += pos.net_qty * (m - pos.avg_cost);
            }
        }
        total
    }

    /// Equity = cash + Σ position marked at mid.
    ///
    /// DETERMINISM: iterates `positions` (a [`BTreeMap`]) in sorted key order, so this Σ is computed
    /// in a fixed order every run — see the `positions` field doc for why that matters for ranking.
    pub fn equity(&self, books: &BookSet) -> f64 {
        let mut total = self.cash;
        for (inst, pos) in &self.positions {
            if pos.net_qty == 0.0 {
                continue;
            }
            if let Some(m) = mark_price(books, inst) {
                total += pos.net_qty * m;
            }
        }
        total
    }

    /// Record an equity point if at least `snapshot_secs` have elapsed since the last one.
    pub fn maybe_snapshot(&mut self, ts_ns: i64, books: &BookSet) {
        let secs = ts_ns / 1_000_000_000;
        let last_secs = if self.last_snapshot_ts == i64::MIN {
            i64::MIN
        } else {
            self.last_snapshot_ts / 1_000_000_000
        };
        if self.last_snapshot_ts == i64::MIN || secs - last_secs >= self.snapshot_secs {
            self.last_snapshot_ts = ts_ns;
            self.push_snapshot(ts_ns, books);
        }
    }

    /// Force a final equity point (used at end-of-run).
    pub fn force_snapshot(&mut self, ts_ns: i64, books: &BookSet) {
        self.push_snapshot(ts_ns, books);
    }

    /// Append one equity point and update exposure accounting.
    fn push_snapshot(&mut self, ts_ns: i64, books: &BookSet) {
        let has_position = self.positions.values().any(|p| p.net_qty.abs() > 1e-12);
        if has_position {
            self.snapshots_with_position += 1;
        }
        self.equity_curve.push(EquityPoint {
            ts_ns,
            total: self.equity(books),
            currency: self.currency.clone(),
        });
    }

    pub fn avg_buy_price(&self) -> f64 {
        if self.buy_qty > 0.0 {
            self.buy_notional / self.buy_qty
        } else {
            0.0
        }
    }

    pub fn avg_sell_price(&self) -> f64 {
        if self.sell_qty > 0.0 {
            self.sell_notional / self.sell_qty
        } else {
            0.0
        }
    }

    /// Average holding time (seconds) across completed round trips. Zero if none.
    pub fn avg_holding_secs(&self) -> f64 {
        if self.holding_count > 0 {
            self.holding_secs_sum / self.holding_count as f64
        } else {
            0.0
        }
    }

    /// Set the *total* accrued liquidity rewards and, if `credit_to_cash`, add the (newly increased)
    /// amount to cash. Called once at end-of-run with the rewards model's final accrual so the
    /// reward shows up in the ending balance only when `include_rewards` is true.
    pub fn set_liquidity_rewards(&mut self, total: f64, credit_to_cash: bool) {
        let delta = total - self.liquidity_rewards;
        self.liquidity_rewards = total;
        if credit_to_cash {
            self.cash += delta;
        }
    }
}

/// Best mark price for an instrument: mid if both sides exist, else best bid/ask, else None.
fn mark_price(books: &BookSet, inst: &str) -> Option<f64> {
    if let Some(b) = books.get(inst) {
        if let Some(m) = b.mid() {
            return Some(m);
        }
        if let Some((p, _)) = b.best_bid() {
            return Some(p.to_dollars());
        }
        if let Some((p, _)) = b.best_ask() {
            return Some(p.to_dollars());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Cents, Liquidity};

    fn fill(side: Side, price: i32, qty: f64, fee: f64) -> Fill {
        fill_ts(side, price, qty, fee, 1)
    }

    fn fill_ts(side: Side, price: i32, qty: f64, fee: f64, ts_ns: i64) -> Fill {
        Fill {
            ts_ns,
            order_id: 1,
            instrument: "X".to_string(),
            side,
            price: Cents(price),
            qty,
            liquidity: Liquidity::Taker,
            fee,
        }
    }

    #[test]
    fn records_round_trip_and_instrument_stats() {
        let mut p = Portfolio::new(1000.0, "USD".into(), 1);
        // open long 100 @ 0.40 at t=1s
        p.apply_fill(&fill_ts(Side::Bid, 40, 100.0, 0.0, 1_000_000_000));
        // close @ 0.60 at t=3s -> pnl = (0.60-0.40)*100 = 20
        p.apply_fill(&fill_ts(Side::Ask, 60, 100.0, 0.0, 3_000_000_000));
        assert_eq!(p.round_trips.len(), 1);
        let rt = &p.round_trips[0];
        assert!((rt.pnl - 20.0).abs() < 1e-9);
        assert!((rt.entry_price - 0.40).abs() < 1e-9);
        assert!((rt.exit_price - 0.60).abs() < 1e-9);
        assert_eq!(rt.qty, 100.0);
        assert_eq!(rt.entry_ts, 1_000_000_000);
        assert_eq!(rt.exit_ts, 3_000_000_000);
        // holding = 2 seconds
        assert!((p.holding_secs_sum - 2.0).abs() < 1e-9);
        // total volume = 200 contracts traded
        assert!((p.total_volume - 200.0).abs() < 1e-9);
        let st = &p.instrument_stats["X"];
        assert_eq!(st.num_fills, 2);
        assert_eq!(st.num_round_trips, 1);
        assert_eq!(st.net_position, 0.0);
        assert!((st.pnl - 20.0).abs() < 1e-9);
        assert!((st.volume - 200.0).abs() < 1e-9);
    }

    #[test]
    fn buy_then_sell_realizes_pnl() {
        let mut p = Portfolio::new(1000.0, "USD".into(), 1);
        p.apply_fill(&fill(Side::Bid, 40, 100.0, 0.0)); // buy 100 @ 0.40 -> -40 cash
        assert!((p.cash - 960.0).abs() < 1e-9);
        let pos = &p.positions["X"];
        assert_eq!(pos.net_qty, 100.0);
        assert!((pos.avg_cost - 0.40).abs() < 1e-9);

        p.apply_fill(&fill(Side::Ask, 60, 100.0, 0.0)); // sell 100 @ 0.60 -> +60 cash
        assert!((p.cash - 1020.0).abs() < 1e-9);
        let pos = &p.positions["X"];
        assert_eq!(pos.net_qty, 0.0);
        assert!((pos.realized_pnl - 20.0).abs() < 1e-9); // (0.60-0.40)*100
    }

    #[test]
    fn fees_excluded_when_include_fees_false() {
        // With fees included: buy 100 @ 0.40 paying $0.50 fee -> cash 1000 - 40 - 0.50 = 959.50
        let mut p_on = Portfolio::new(1000.0, "USD".into(), 1);
        p_on.include_fees = true;
        p_on.apply_fill(&fill(Side::Bid, 40, 100.0, 0.50));
        assert!((p_on.cash - 959.50).abs() < 1e-9, "got {}", p_on.cash);
        assert!((p_on.total_fees - 0.50).abs() < 1e-9);

        // With fees excluded: same fill, fee not charged -> cash 1000 - 40 = 960, total_fees 0
        let mut p_off = Portfolio::new(1000.0, "USD".into(), 1);
        p_off.include_fees = false;
        p_off.apply_fill(&fill(Side::Bid, 40, 100.0, 0.50));
        assert!((p_off.cash - 960.0).abs() < 1e-9, "got {}", p_off.cash);
        assert_eq!(p_off.total_fees, 0.0);
    }

    #[test]
    fn slippage_cost_reduces_cash_and_accumulates() {
        let mut p = Portfolio::new(1000.0, "USD".into(), 1);
        // buy 100 @ 0.40 with $1.00 slippage -> cash 1000 - 40 - 1 = 959
        p.apply_fill_ex(&fill(Side::Bid, 40, 100.0, 0.0), 1.00);
        assert!((p.cash - 959.0).abs() < 1e-9, "got {}", p.cash);
        assert!((p.total_slippage_cost - 1.0).abs() < 1e-9);
        // another slipping fill accumulates
        p.apply_fill_ex(&fill(Side::Ask, 60, 50.0, 0.0), 0.50);
        assert!((p.total_slippage_cost - 1.50).abs() < 1e-9);
    }

    #[test]
    fn liquidity_rewards_credited_only_when_enabled() {
        let mut credited = Portfolio::new(1000.0, "USD".into(), 1);
        credited.set_liquidity_rewards(5.0, true);
        assert!((credited.cash - 1005.0).abs() < 1e-9);
        assert!((credited.liquidity_rewards - 5.0).abs() < 1e-9);

        let mut not = Portfolio::new(1000.0, "USD".into(), 1);
        not.set_liquidity_rewards(5.0, false);
        assert!((not.cash - 1000.0).abs() < 1e-9); // reported, not credited
        assert!((not.liquidity_rewards - 5.0).abs() < 1e-9);
    }

    #[test]
    fn avg_cost_blends() {
        let mut p = Portfolio::new(1000.0, "USD".into(), 1);
        p.apply_fill(&fill(Side::Bid, 40, 100.0, 0.0));
        p.apply_fill(&fill(Side::Bid, 60, 100.0, 0.0));
        let pos = &p.positions["X"];
        assert_eq!(pos.net_qty, 200.0);
        assert!((pos.avg_cost - 0.50).abs() < 1e-9);
    }

    #[test]
    fn settle_long_to_one_on_yes() {
        // Buy 100 YES @ 0.40 -> cash 960, long 100 @ avg 0.40. Market resolves YES (payout $1).
        let mut p = Portfolio::new(1000.0, "USD".into(), 1);
        p.apply_fill(&fill(Side::Bid, 40, 100.0, 0.0));
        assert!((p.cash - 960.0).abs() < 1e-9);
        let pnl = p.settle_position("X", 1.0, 5_000_000_000);
        // realized PnL = q*(payout - avg) = 100*(1.0 - 0.40) = 60
        assert!((pnl - 60.0).abs() < 1e-9, "settle pnl {pnl}");
        // cash credited 100 * $1 = 100 -> 960 + 100 = 1060
        assert!((p.cash - 1060.0).abs() < 1e-9, "cash {}", p.cash);
        // position is flat
        assert!(p.positions["X"].net_qty.abs() < 1e-9);
        // the settlement fill is recorded with Liquidity::Settle, fee 0, at price $1.00
        let f = p.fills.last().unwrap();
        assert_eq!(f.liquidity, Liquidity::Settle);
        assert_eq!(f.fee, 0.0);
        assert_eq!(f.price, Cents(100));
        // no fees were charged at all
        assert_eq!(p.total_fees, 0.0);
    }

    #[test]
    fn settle_short_to_zero_on_no() {
        // Sell 100 YES @ 0.40 (open short) -> cash +40 = 1040, short -100 @ avg 0.40.
        let mut p = Portfolio::new(1000.0, "USD".into(), 1);
        p.apply_fill(&fill(Side::Ask, 40, 100.0, 0.0));
        assert!((p.cash - 1040.0).abs() < 1e-9, "cash {}", p.cash);
        assert!((p.positions["X"].net_qty + 100.0).abs() < 1e-9);
        // Market resolves NO (payout $0). A short profits: realized = q*(payout-avg) =
        // (-100)*(0.0 - 0.40) = +40.
        let pnl = p.settle_position("X", 0.0, 5_000_000_000);
        assert!((pnl - 40.0).abs() < 1e-9, "settle pnl {pnl}");
        // cash change from settling a short: buying back 100 @ $0 costs $0, so cash unchanged at 1040.
        assert!((p.cash - 1040.0).abs() < 1e-9, "cash {}", p.cash);
        assert!(p.positions["X"].net_qty.abs() < 1e-9);
        let f = p.fills.last().unwrap();
        assert_eq!(f.liquidity, Liquidity::Settle);
        assert_eq!(f.fee, 0.0);
        assert_eq!(f.price, Cents(0));
    }

    #[test]
    fn settle_flat_position_is_a_noop() {
        let mut p = Portfolio::new(1000.0, "USD".into(), 1);
        let pnl = p.settle_position("X", 1.0, 1);
        assert_eq!(pnl, 0.0);
        assert!(p.fills.is_empty());
        assert!((p.cash - 1000.0).abs() < 1e-9);
    }

    #[test]
    fn equity_marks_at_mid() {
        use crate::types::{Action, BookDelta};
        let mut p = Portfolio::new(1000.0, "USD".into(), 1);
        p.apply_fill(&fill(Side::Bid, 40, 100.0, 0.0)); // cash 960, long 100
        let mut bs = BookSet::new();
        let d = |s: Side, px: i32| BookDelta {
            ts_ns: 1,
            instrument: "X".into(),
            action: Action::Add,
            side: s,
            price: Cents(px),
            size: 100.0,
            sequence: 1,
            is_snapshot: false,
        };
        bs.apply(&d(Side::Bid, 50));
        bs.apply(&d(Side::Ask, 60));
        // mid = 0.55 -> equity = 960 + 100*0.55 = 1015
        assert!((p.equity(&bs) - 1015.0).abs() < 1e-9);
    }
}
