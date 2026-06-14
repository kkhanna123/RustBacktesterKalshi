//! The event-driven backtest engine.
//!
//! The engine owns the [`BookSet`], [`Portfolio`], resting strategy orders, and a queue of
//! pending strategy [`Action`]s. The borrow-safe pattern: `Ctx` methods only *record* actions
//! into a `Vec<EngineAction>` and *read* snapshots of the book / portfolio. After the strategy
//! hook returns, the engine drains the queued actions (placing resting orders or executing market
//! orders immediately).
//!
//! Per event, in time order:
//! 0. Execute any in-flight (latency-deferred) market orders whose `activation_ts <= now`, walking
//!    the book as it stands at this step, and apply due cancels.
//! 1. If a Delta: apply to the book.
//! 2. Run execution matching of resting orders against this event (trades trigger maker fills).
//! 3. Apply resulting fills to the portfolio.
//! 4. Call `strat.on_event` with a `Ctx`.
//! 5. Drain queued actions into resting orders / market orders (latency-gated, see below).
//! 6. Periodically snapshot equity.
//!
//! ## Latency fill model (the headline feature)
//! Every order SENT at tick time `T` carries an order latency `L` (`order_latency_ns` +
//! `market_data_latency_ns` + deterministic `jitter_ns`, via [`LatencyModel`]) and so an
//! `activation_ts = T + L`.
//!
//! * A **resting limit** order is only matchable by a [`TradeEvent`] whose `ts >= activation_ts`
//!   (gated in [`Engine::step`] step 2).
//! * A **market/taker** order does NOT execute immediately when latency is in effect. On draining
//!   actions (step 5), if its `activation_ts <= current_event_ts` it executes right then (so under
//!   the **zero-latency default** `activation_ts == T` and the order fills during the same event —
//!   exactly the original immediate behaviour). Otherwise it becomes a PENDING in-flight order, and
//!   at the START of each later [`Engine::step`] (step 0) any pending market whose
//!   `activation_ts <= current_event_ts` executes against the book **as of that step** (the
//!   latency-delayed book), producing taker fills stamped at that event's ts. So a market order sent
//!   at `T` with latency `L` fills at the first event `>= T + L`, against the future book — not at
//!   `T`. Slippage and fees apply to these taker fills exactly as before.

use crate::config::{BacktestConfig, QueueModel, RiskConfig};
use crate::event_curves::EventCurves;
use crate::execution::{execute_market_bounded, match_resting_against_trade, RestingOrder};
use crate::fees::FeeModel;
use crate::latency::LatencyModel;
use crate::orderbook::BookSet;
use crate::portfolio::Portfolio;
use crate::report::build_report;
use crate::rewards::{quote_state_from_resting, RewardsModel};
use crate::settlement::SettlementMap;
use crate::slippage::SlippageModel;
use crate::strategy::{Ctx, Strategy};
use crate::types::{Cents, Liquidity, MarketEvent, OrderView, Report, Side, Tif, TradeEvent};

/// An action a strategy requested during a hook, applied after the hook returns.
enum EngineAction {
    PlaceLimit {
        instrument: String,
        side: Side,
        price: Cents,
        qty: f64,
        /// Time-in-force (GTC rests the remainder; IOC cancels it).
        tif: Tif,
        /// If true, a marketable order is rejected (maker-only guarantee) instead of taking.
        post_only: bool,
    },
    PlaceMarket {
        instrument: String,
        side: Side,
        qty: f64,
    },
    Cancel {
        order_id: u64,
    },
}

/// Everything a finished backtest produced, for reporting + dashboard exports.
pub struct RunOutput {
    /// The infra-compatible report.
    pub report: Report,
    /// Final portfolio (fills, round-trips, instrument stats, equity curve).
    pub portfolio: Portfolio,
    /// Market trades observed in the input stream (for the `trades.csv` export).
    pub observed_trades: Vec<TradeEvent>,
}

/// A queued cancel awaiting its (possibly latency-delayed) effective timestamp.
struct PendingCancel {
    order_id: u64,
    effective_ts: i64,
}

/// A market (taker) order in flight: it was SENT at some tick but its latency activation time has
/// not yet been reached, so it has not executed. It fills at the first engine step whose event
/// `ts >= activation_ts`, walking the book as of that step.
struct PendingMarket {
    order_id: u64,
    instrument: String,
    side: Side,
    qty: f64,
    /// Tick time (ns) at/after which this market order executes (`send_ts + latency`).
    activation_ts: i64,
    /// Optional limit-price bound (set when this taker is the crossing portion of a marketable
    /// limit order): the walk stops at this price and never fills past it. `None` = plain market.
    price_bound: Option<Cents>,
}

/// A limit order in flight: it was SENT at some tick but its latency `activation_ts` has not yet been
/// reached, so its marketability (and hence whether it takes / rests / is rejected) has not yet been
/// decided. At the first engine step whose event `ts >= activation_ts` it is RESOLVED against the
/// book as of that step (see [`Engine::resolve_limit`]). Under the zero-latency default a limit is
/// resolved immediately on the same step it is placed, so it never enters this queue.
struct PendingLimit {
    order_id: u64,
    instrument: String,
    side: Side,
    price: Cents,
    qty: f64,
    tif: Tif,
    post_only: bool,
    /// Tick time (ns) at/after which this limit's marketability is judged (`send_ts + latency`).
    activation_ts: i64,
}

/// The engine; also implements [`Ctx`] for the duration of a strategy hook.
pub struct Engine {
    pub books: BookSet,
    pub portfolio: Portfolio,
    resting: Vec<RestingOrder>,
    pending: Vec<EngineAction>,
    /// Cancels scheduled to take effect at/after their `effective_ts` (cancel latency).
    pending_cancels: Vec<PendingCancel>,
    /// Market (taker) orders in flight, awaiting their latency `activation_ts` before executing
    /// against the book as of the activating step.
    pending_market: Vec<PendingMarket>,
    /// Limit orders in flight, awaiting their latency `activation_ts` before their marketability is
    /// judged (and they take / rest / are rejected). Empty under the zero-latency default.
    pending_limit: Vec<PendingLimit>,
    /// Maker-queue model controlling how a resting order's `queue_ahead` reacts to book changes.
    /// Default [`QueueModel::Pessimistic`] => identical to the original behaviour.
    queue_model: QueueModel,
    fees: FeeModel,
    latency: LatencyModel,
    slippage: SlippageModel,
    rewards: RewardsModel,
    /// Whether accrued liquidity rewards should be credited to the ending balance.
    include_rewards: bool,
    /// Max distance from mid (cents) a quote may sit to count toward rewards.
    reward_band_cents: i32,
    next_order_id: u64,
    cur_ts: i64,
    /// Market trades observed in the stream (collected for exports).
    observed_trades: Vec<TradeEvent>,
    // ---- engine-enforced risk layer ----
    /// Hard risk limits (all `None` => no-op). See [`RiskConfig`].
    risk: RiskConfig,
    /// True once an equity HALT has fired. While halted, all strategy orders are ignored and only
    /// the risk layer's own flatten orders are placed.
    halted: bool,
    /// Running peak equity (for the max-drawdown HALT check). Seeded to the starting balance.
    equity_peak: f64,
    /// Count of orders the risk layer dropped or clamped to zero qty.
    risk_rejections: i64,
    /// BINARY SETTLEMENT-AT-EXPIRY map (`instrument -> Yes/No`). Empty when no settlement file was
    /// provided, in which case `finalize` flattens at mid exactly as before. See [`Engine::finalize`].
    settlement: SettlementMap,
    /// PER-EVENT LOGISTIC STRIKE-CURVE state. Each delta records the touched strike's mid here
    /// (cheap, O(log n)) and marks its event dirty; the logistic fit is computed LAZILY the first
    /// time a strategy reads one of the fit-based `Ctx` methods (`implied_fair_value`, `implied_vol`,
    /// `fitted_price`, `fit_edge`, `fit_quality`) for that event. Wrapped in a `RefCell` because
    /// those `Ctx` methods take `&self` yet the lazy fit must mutate the cache.
    ///
    /// BYTE-FOR-BYTE GUARANTEE: this is pure side-state. `update` never touches the book, fills,
    /// cash, or PnL, and the fit is never computed unless a strategy explicitly asks — so strategies
    /// that don't call the new `Ctx` methods produce identical reports (see the no-op / determinism
    /// tests).
    event_curves: std::cell::RefCell<EventCurves>,
}

impl Engine {
    pub fn new(cfg: &BacktestConfig) -> Self {
        let mut portfolio = Portfolio::new(
            cfg.starting_balance,
            cfg.currency.clone(),
            cfg.equity_snapshot_secs,
        );
        portfolio.include_fees = cfg.execution.include_fees;
        // Load the binary-settlement map if a path is configured. A missing/unreadable file warns to
        // STDERR and falls back to an empty map (== flatten-at-mid), so a bad path never aborts a run.
        let settlement = match &cfg.execution.settlement.path {
            Some(path) => match SettlementMap::from_path(std::path::Path::new(path)) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "[kalshi-backtester] WARN: could not read settlements file {path}: {e} — \
                         falling back to flatten-at-mid"
                    );
                    SettlementMap::new()
                }
            },
            None => SettlementMap::new(),
        };
        Engine {
            books: BookSet::new(),
            portfolio,
            resting: Vec::new(),
            pending: Vec::new(),
            pending_cancels: Vec::new(),
            pending_market: Vec::new(),
            pending_limit: Vec::new(),
            queue_model: cfg.execution.queue.model,
            fees: FeeModel::from_config(cfg),
            latency: LatencyModel::from_config(&cfg.execution.latency),
            slippage: SlippageModel::from_config(&cfg.execution.slippage),
            rewards: RewardsModel::from_config(&cfg.execution.rewards),
            include_rewards: cfg.execution.include_rewards,
            reward_band_cents: cfg.execution.rewards.max_spread_cents,
            next_order_id: 1,
            cur_ts: 0,
            observed_trades: Vec::new(),
            risk: cfg.execution.risk.clone(),
            halted: false,
            equity_peak: cfg.starting_balance,
            risk_rejections: 0,
            settlement,
            event_curves: std::cell::RefCell::new(EventCurves::new()),
        }
    }

    /// Inject a settlement map programmatically (used by tests and callers that build the map
    /// in-memory rather than from a file). Overrides any map loaded from the config path.
    pub fn set_settlement_map(&mut self, map: SettlementMap) {
        self.settlement = map;
    }

    /// Run the full backtest and return just the infra-compatible report. Thin wrapper over
    /// [`Engine::run_collecting`] for callers that don't need export artifacts.
    pub fn run(
        self,
        events: impl Iterator<Item = MarketEvent>,
        strat: &mut dyn Strategy,
        cfg: &BacktestConfig,
    ) -> Report {
        self.run_collecting(events, strat, cfg).report
    }

    /// Run the full backtest and return the report plus the portfolio and observed trades, so the
    /// caller can write structured dashboard exports.
    ///
    /// This is now a thin driver over the incremental core: it feeds events one-by-one through
    /// [`Engine::step`] (with periodic snapshots), then calls [`Engine::finalize`] to flatten,
    /// credit rewards, and build the report. Batch and live trading therefore share **exactly** the
    /// same per-event logic; see the `incremental_core_matches_batch_run` test for the equivalence
    /// guarantee.
    pub fn run_collecting(
        mut self,
        events: impl Iterator<Item = MarketEvent>,
        strat: &mut dyn Strategy,
        cfg: &BacktestConfig,
    ) -> RunOutput {
        for ev in events {
            self.step(&ev, strat);
            // periodic equity snapshot (mirrors live `snapshot_equity` cadence)
            let now = self.cur_ts;
            self.snapshot_maybe(now);
        }
        self.finalize(strat, cfg.flatten_at_end);
        let report = build_report(strat.name(), &self.portfolio, cfg);
        RunOutput {
            report,
            portfolio: self.portfolio,
            observed_trades: self.observed_trades,
        }
    }

    /// Process exactly one [`MarketEvent`] against the live state: apply due cancels, update the
    /// book, match resting orders against trades, accrue rewards, run the strategy hook, and drain
    /// the queued actions. This is the shared incremental core that both the batch [`run_collecting`]
    /// loop and the live paper-trading loop drive, guaranteeing identical fill/PnL logic.
    ///
    /// Unlike [`run_collecting`], this does **not** take an equity snapshot (the caller decides
    /// snapshot cadence) and does not finalize the run.
    ///
    /// [`run_collecting`]: Engine::run_collecting
    pub fn step(&mut self, ev: &MarketEvent, strat: &mut dyn Strategy) {
        self.cur_ts = ev.ts_ns();

        // 0. apply any cancels whose latency has now elapsed, and execute any in-flight market
        //    orders whose latency activation time has been reached (taker fills against the book as
        //    it stands at the START of this step — the latency-delayed book).
        //
        //    DOC NOTE (pre-event activation): this runs BEFORE step 1 applies the current event's
        //    delta, so a latency-deferred limit/market activating at this event resolves against the
        //    PRE-event book — it does NOT see the same-timestamp delta that arrives in step 1. This
        //    is intentional: an order whose latency elapses "at" ts T acts on the book state as of T
        //    before T's own update lands, a conservative one-step-stale view consistent with the
        //    latency model. See `resolve_due_limit_orders` / `execute_due_market_orders`.
        self.process_due_cancels();
        self.resolve_due_limit_orders(self.cur_ts);
        self.execute_due_market_orders(self.cur_ts);

        // 1. update book on deltas. Under the OPTIMISTIC queue model, a delta that shrinks the
        //    resting size at a price level where we have orders is treated as cancellations AHEAD of
        //    us, so we advance our queue position by the decrease (see `apply_optimistic_queue`).
        if let MarketEvent::Delta(d) = ev {
            let size_before = if matches!(self.queue_model, QueueModel::Optimistic) {
                self.level_size(&d.instrument, d.side, d.price)
            } else {
                0.0
            };
            self.books.apply(d);
            if matches!(self.queue_model, QueueModel::Optimistic) {
                let size_after = self.level_size(&d.instrument, d.side, d.price);
                if size_after < size_before {
                    self.apply_optimistic_queue(
                        &d.instrument,
                        d.side,
                        d.price,
                        size_before - size_after,
                    );
                }
            }
            // LOGISTIC STRIKE-CURVE: record this strike's fresh mid into its event ladder and mark
            // the event dirty. Cheap O(log n) side-state; the fit itself is computed lazily only if a
            // strategy reads a fit-based `Ctx` method (so non-logistic runs stay byte-for-byte). The
            // updated curve is therefore "current as of the latest delta" — literal fit-per-delta.
            self.event_curves
                .borrow_mut()
                .update(&d.instrument, self.books.mid(&d.instrument));
        }

        // 2-3. match resting orders against trades, apply fills (only orders whose latency
        // activation time has been reached are eligible to fill).
        if let MarketEvent::Trade(t) = ev {
            self.observed_trades.push(t.clone());
            let now = self.cur_ts;
            let mut produced = Vec::new();
            for o in self.resting.iter_mut() {
                if o.activation_ts > now {
                    continue; // not yet matchable (latency gating)
                }
                if let Some(f) = match_resting_against_trade(o, t, &self.fees) {
                    produced.push(f);
                }
            }
            for f in produced {
                // maker fills may carry an adverse-selection slippage cost
                let slip = self.slippage.cost(f.side, f.price, f.qty, Liquidity::Maker);
                self.portfolio.apply_fill_ex(&f, slip);
            }
            self.resting.retain(|o| o.remaining > 1e-12);
        }

        // 4. accrue liquidity rewards for the segment ending at this event.
        self.accrue_rewards();

        // 5. strategy hook (skipped once halted — see `risk_check_and_maybe_halt`; we still call it
        //    so a strategy can keep its own bookkeeping, but its actions are dropped on drain).
        strat.on_event(ev, self);

        // 6. drain queued actions
        self.apply_pending();

        // 7. risk layer: equity is now markable against the current book. Check the equity floor /
        //    max-drawdown limits and HALT (cancel + flatten, bypassing latency) on breach. No-op
        //    when no equity limits are configured.
        self.risk_check_and_maybe_halt();
    }

    /// Take an equity snapshot at `now_ns` only if the configured `equity_snapshot_secs` cadence
    /// has elapsed since the last one (the same cadence the batch loop uses per event).
    #[inline]
    pub fn snapshot_maybe(&mut self, now_ns: i64) {
        self.portfolio.maybe_snapshot(now_ns, &self.books);
    }

    /// Take an equity snapshot at `now_ns`, forcing a point onto the curve. Returns the equity
    /// (cash + positions marked at mid). Used by the live loop to drive the equity curve on a
    /// wall-clock cadence independent of event arrival.
    pub fn snapshot_equity(&mut self, now_ns: i64) -> f64 {
        self.portfolio.force_snapshot(now_ns.max(1), &self.books);
        self.portfolio.equity(&self.books)
    }

    /// Finalize a run: end-of-data strategy hook, flush pending cancels, a final rewards
    /// observation, BINARY SETTLEMENT of held positions (or a fall-back flatten), reward crediting,
    /// and a final equity point. Shared by the batch run and the live loop's clean shutdown. After
    /// this, [`into_report`] / [`build_report`] yields the contract-compatible report.
    ///
    /// ## End-of-run position handling — settlement vs flatten (precedence)
    /// For each instrument that still holds an open position:
    /// * If the BINARY SETTLEMENT map has a KNOWN outcome for it, the position is **settled** to its
    ///   $1/$0 binary payoff via [`Portfolio::settle_position`] (no fee, no slippage), regardless of
    ///   the `flatten` flag — settlement is the correct end state for a contract held to expiry, so it
    ///   takes PRECEDENCE over flatten-at-mid for known instruments.
    /// * Otherwise (the instrument has an UNKNOWN outcome, or no settlement map was provided at all)
    ///   the position is **flattened at mid** if `flatten` is true, exactly as before; if `flatten`
    ///   is false it is simply left open (marked at mid for equity), also exactly as before.
    ///
    /// Thus with NO settlement file the behaviour is byte-for-byte the original flatten-at-mid.
    ///
    /// [`into_report`]: Engine::into_report
    pub fn finalize(&mut self, strat: &mut dyn Strategy, flatten: bool) {
        strat.on_finish(self);
        self.apply_pending();
        // flush remaining cancels and a final rewards observation at the last timestamp
        self.process_all_remaining_cancels();
        // resolve any still in-flight limit orders (their latency window ran past the end of data)
        // BEFORE flushing markets, so a marketable-limit take becomes a pending taker that is then
        // flushed too — no limit take or resting remainder is silently dropped.
        self.flush_all_pending_limit_orders();
        // execute any still in-flight market orders at the final timestamp (their latency window
        // ran past the end of data) so no taker order is silently dropped.
        self.flush_all_pending_market_orders();
        self.accrue_rewards();

        // BINARY SETTLEMENT first: settle every held position whose instrument has a KNOWN outcome to
        // its $1/$0 payoff (no fee, no slippage). This empties those positions so the subsequent
        // flatten only touches UNKNOWN instruments. A no-op when no settlement map was provided.
        self.settle_known_positions();

        if flatten {
            // Only UNKNOWN-outcome instruments remain open here (settled ones are already flat), so
            // this preserves the original flatten-at-mid behaviour for everything not settled.
            self.flatten_all();
            // flatten places market orders; under latency they are deferred, so flush them too.
            self.flush_all_pending_market_orders();
        }

        // credit accrued liquidity rewards (only added to cash when include_rewards is true)
        if self.rewards.is_active() {
            self.portfolio
                .set_liquidity_rewards(self.rewards.accrued(), self.include_rewards);
        }

        // surface the risk-layer rejection count to the report (halt flag/reason were stamped at
        // halt time).
        self.portfolio.risk_rejections = self.risk_rejections;

        // final equity point
        self.portfolio.force_snapshot(self.cur_ts.max(1), &self.books);
    }

    /// Build the infra-compatible [`Report`] from the current portfolio state, consuming the engine.
    /// The caller is expected to have called [`finalize`] first for a clean end-of-run report.
    ///
    /// [`finalize`]: Engine::finalize
    pub fn into_report(self, plugin_name: &str, cfg: &BacktestConfig) -> RunOutput {
        let report = build_report(plugin_name, &self.portfolio, cfg);
        RunOutput {
            report,
            portfolio: self.portfolio,
            observed_trades: self.observed_trades,
        }
    }

    /// Current engine timestamp (ns) — the ts of the last event processed by [`step`].
    ///
    /// [`step`]: Engine::step
    #[inline]
    pub fn now_ns(&self) -> i64 {
        self.cur_ts
    }

    /// Read-only access to the reconstructed books (for live status rendering).
    #[inline]
    pub fn books(&self) -> &BookSet {
        &self.books
    }

    /// Read-only access to the portfolio (cash, positions, fills, totals) for live status.
    #[inline]
    pub fn portfolio(&self) -> &Portfolio {
        &self.portfolio
    }

    /// Mutable access to the portfolio, used by the live loop to reseed cash/positions on `--resume`.
    #[inline]
    pub fn portfolio_mut(&mut self) -> &mut Portfolio {
        &mut self.portfolio
    }

    /// Total accrued liquidity rewards so far (dollars), reported live even before crediting.
    #[inline]
    pub fn accrued_rewards(&self) -> f64 {
        self.rewards.accrued()
    }

    /// Snapshot of currently-resting strategy orders across all instruments (for status display).
    pub fn resting_orders(&self) -> Vec<OrderView> {
        self.resting
            .iter()
            .map(|o| OrderView {
                id: o.id,
                side: o.side,
                price: o.price,
                remaining: o.remaining,
            })
            .collect()
    }

    /// Seed the engine's clock so the first live snapshot has a sane timestamp before any event has
    /// been stepped (e.g. when resuming or rendering an empty status). No-op once events flow.
    #[inline]
    pub fn seed_clock(&mut self, ts_ns: i64) {
        if self.cur_ts == 0 {
            self.cur_ts = ts_ns;
        }
    }

    /// Drain and apply all queued strategy actions.
    fn apply_pending(&mut self) {
        let actions = std::mem::take(&mut self.pending);
        for a in actions {
            match a {
                EngineAction::PlaceLimit {
                    instrument,
                    side,
                    price,
                    qty,
                    tif,
                    post_only,
                } => {
                    if let Some(q) = self.risk_clamp_order(&instrument, side, qty) {
                        self.submit_limit(instrument, side, price, q, tif, post_only);
                    }
                }
                EngineAction::PlaceMarket {
                    instrument,
                    side,
                    qty,
                } => {
                    if let Some(q) = self.risk_clamp_order(&instrument, side, qty) {
                        self.do_market(instrument, side, q);
                    }
                }
                EngineAction::Cancel { order_id } => {
                    if self.latency.is_active() {
                        // schedule the cancel for its latency-delayed effective time
                        self.pending_cancels.push(PendingCancel {
                            order_id,
                            effective_ts: self.latency.cancel_effective_ts(self.cur_ts),
                        });
                    } else {
                        self.resting.retain(|o| o.id != order_id);
                    }
                }
            }
        }
    }

    // ========================================================================
    // Engine-enforced risk layer
    // ========================================================================

    /// Apply the order-level risk clamps to one outgoing strategy order, in the spec'd order:
    /// 1. if HALTED, drop entirely; 2. clamp to `max_order_qty`; 3. clamp against
    /// `max_position_per_instrument`; 4. clamp against `max_gross_position`; 5. if qty ≤ 0 after
    /// clamping, drop and count a rejection.
    ///
    /// Returns `Some(clamped_qty)` to place, or `None` to drop the order (a dropped/zeroed order
    /// increments [`Self::risk_rejections`]). An order that only REDUCES the instrument's net
    /// position (a flatten / de-risk) is never reduced by the position caps.
    ///
    /// With no risk limits configured this returns `Some(qty)` unchanged (the original behaviour).
    fn risk_clamp_order(&mut self, instrument: &str, side: Side, qty: f64) -> Option<f64> {
        // Fast path: nothing configured AND not halted => behave exactly as before.
        if !self.halted && !self.risk.any_enabled() {
            return Some(qty);
        }
        // 1. halted: drop every strategy order.
        if self.halted {
            self.risk_rejections += 1;
            return None;
        }
        if qty <= 0.0 {
            return None;
        }

        let mut q = qty;

        // 2. single-order qty cap.
        if let Some(cap) = self.risk.max_order_qty {
            if cap >= 0.0 {
                q = q.min(cap);
            }
        }

        // Signed direction of this order (+ buys YES, - sells YES).
        let dir = match side {
            Side::Bid => 1.0,
            Side::Ask => -1.0,
        };
        let cur_net = self.position_net(instrument);
        // Is this order reducing |net| for this instrument? (opposite sign to the open position).
        // A reducing/flattening order is ALWAYS allowed through the position caps.
        let reduces_instrument = cur_net != 0.0 && (cur_net > 0.0) != (dir > 0.0);

        // 3. per-instrument |net| cap.
        if let Some(cap) = self.risk.max_position_per_instrument {
            if !reduces_instrument {
                // would-be |net| if the whole order filled.
                let allowed = (cap.max(0.0) - cur_net.abs()).max(0.0);
                q = q.min(allowed);
            }
        }

        // 4. gross |net| cap across all instruments. An opening order can add at most
        //    (cap - current_gross) of fresh same-direction exposure.
        if let Some(cap) = self.risk.max_gross_position {
            if !reduces_instrument {
                let gross = self.gross_position();
                let allowed = (cap.max(0.0) - gross).max(0.0);
                q = q.min(allowed);
            }
        }

        // 5. fully clamped away => drop + count a rejection.
        if q <= 1e-12 {
            self.risk_rejections += 1;
            return None;
        }
        Some(q)
    }

    /// Current signed net position for one instrument (0 if none).
    fn position_net(&self, instrument: &str) -> f64 {
        self.portfolio
            .positions
            .get(instrument)
            .map(|p| p.net_qty)
            .unwrap_or(0.0)
    }

    /// Σ |net_qty| across all instruments (gross exposure).
    ///
    /// DETERMINISM: `positions` is a [`std::collections::BTreeMap`], so this fold runs in sorted
    /// instrument order on every process. Float addition is non-associative, so summing a
    /// `HashMap` in randomized order would otherwise make this risk-clamp input run-dependent. See
    /// [`crate::portfolio::Portfolio::positions`].
    fn gross_position(&self) -> f64 {
        self.portfolio
            .positions
            .values()
            .map(|p| p.net_qty.abs())
            .sum()
    }

    /// After equity is markable against the current book, enforce the equity-floor / max-drawdown
    /// HALT limits. On the FIRST breach: record the halt (flag + reason + ts on the portfolio),
    /// cancel all resting orders, and flatten every open position with latency-BYPASSING market
    /// orders (a risk stop must act now, not at T+latency). Idempotent once halted.
    ///
    /// DOC NOTE (bounded one-step leak): this floor check runs at the END of [`Engine::step`]
    /// (step 7), AFTER this event's resting-order matching (steps 2-3) and action drain (step 5).
    /// So an order already in-flight, or one placed/filled earlier in the SAME step that pushes
    /// equity through the floor, can fill BEFORE the halt fires. The breach is then caught at this
    /// step's check and the halt flattens immediately, so the leak is bounded to at most one step —
    /// a deliberate, documented modeling choice (we check equity once per event, post-fill).
    fn risk_check_and_maybe_halt(&mut self) {
        if self.halted {
            return;
        }
        let floor = self.risk.equity_floor;
        let dd_pct = self.risk.max_drawdown_pct;
        if floor.is_none() && dd_pct.is_none() {
            return;
        }

        let equity = self.portfolio.equity(&self.books);
        if equity > self.equity_peak {
            self.equity_peak = equity;
        }

        let mut reason: Option<String> = None;
        if let Some(f) = floor {
            if equity <= f {
                reason = Some(format!(
                    "equity {:.2} <= equity_floor {:.2}",
                    equity, f
                ));
            }
        }
        if reason.is_none() {
            if let Some(p) = dd_pct {
                if self.equity_peak.abs() > 1e-12 {
                    let dd = (self.equity_peak - equity) / self.equity_peak * 100.0;
                    if dd >= p {
                        reason = Some(format!(
                            "drawdown {:.2}% >= max_drawdown_pct {:.2}% (peak {:.2}, equity {:.2})",
                            dd, p, self.equity_peak, equity
                        ));
                    }
                }
            }
        }

        if let Some(r) = reason {
            self.trigger_halt(r);
        }
    }

    /// Fire the HALT: mark state, cancel all resting + pending orders, and flatten all positions
    /// with latency-bypassing market orders (exempt from the position caps since they reduce). After
    /// this, [`risk_clamp_order`] drops every further strategy order.
    ///
    /// [`risk_clamp_order`]: Engine::risk_clamp_order
    fn trigger_halt(&mut self, reason: String) {
        self.halted = true;
        self.portfolio.halted = true;
        self.portfolio.halt_reason = reason;
        // cancel ALL resting orders and any in-flight cancels/market orders — the strategy is done.
        self.resting.clear();
        self.pending_cancels.clear();
        self.pending_market.clear();
        self.pending_limit.clear();
        // drop any actions the strategy queued this step (they must not place new orders).
        self.pending.clear();
        // flatten every open position NOW, bypassing latency, walking the current book.
        let insts: Vec<(String, f64)> = self
            .portfolio
            .positions
            .iter()
            .filter(|(_, p)| p.net_qty.abs() > 1e-12)
            .map(|(k, p)| (k.clone(), p.net_qty))
            .collect();
        for (inst, net) in insts {
            let side = if net > 0.0 { Side::Ask } else { Side::Bid };
            // execute immediately at cur_ts regardless of the latency model.
            let id = self.next_order_id;
            self.next_order_id += 1;
            self.portfolio.total_orders += 1;
            self.execute_market_now(id, &inst, side, net.abs(), self.cur_ts);
        }
    }

    /// Remove resting orders whose scheduled cancels have reached their effective time.
    fn process_due_cancels(&mut self) {
        if self.pending_cancels.is_empty() {
            return;
        }
        let now = self.cur_ts;
        let mut due: Vec<u64> = Vec::new();
        self.pending_cancels.retain(|c| {
            if c.effective_ts <= now {
                due.push(c.order_id);
                false
            } else {
                true
            }
        });
        if !due.is_empty() {
            self.resting.retain(|o| !due.contains(&o.id));
        }
    }

    /// At end-of-run, apply every still-pending cancel regardless of effective time.
    fn process_all_remaining_cancels(&mut self) {
        if self.pending_cancels.is_empty() {
            return;
        }
        let due: Vec<u64> = self.pending_cancels.drain(..).map(|c| c.order_id).collect();
        self.resting.retain(|o| !due.contains(&o.id));
    }

    /// Observe the current resting-quote state vs the mid and advance the rewards model.
    fn accrue_rewards(&mut self) {
        if !self.rewards.is_active() {
            return;
        }
        // Build a per-instrument view; reward each qualifying instrument. We model a single book
        // here (the common single-instrument backtest), accruing the union of resting quotes.
        // Group resting orders by instrument and feed each instrument's mid + in-band sizes.
        use std::collections::HashMap;
        let mut by_inst: HashMap<&str, Vec<(Side, i32, f64)>> = HashMap::new();
        for o in &self.resting {
            by_inst
                .entry(o.instrument.as_str())
                .or_default()
                .push((o.side, o.price.0, o.remaining));
        }
        // Determine aggregate qualifying state: qualifies if ANY instrument qualifies. For a
        // single-instrument run this is exact; for multi-instrument it credits the modeled
        // participant for keeping at least one book quoted (documented simplification).
        let mut best = crate::rewards::QuoteState::default();
        for (inst, quotes) in by_inst {
            let mid_cents = self.books.mid(inst).map(|m| m * 100.0);
            let qs = quote_state_from_resting(quotes, mid_cents, self.reward_band_cents);
            // keep the strongest (most-qualifying) instrument's state
            if qs.has_mid
                && (qs.bid_size_in_band + qs.ask_size_in_band
                    > best.bid_size_in_band + best.ask_size_in_band)
            {
                best = qs;
            }
        }
        self.rewards.observe(self.cur_ts, &best);
    }

    /// Submit a limit order (the full TIF / post-only path). Counts the order ONCE (`total_orders`),
    /// then either resolves its marketability immediately (zero-latency default) or parks it in
    /// `pending_limit` to be resolved at its activation step. A single limit order — whatever it does
    /// (take, rest, both, reject) — is counted exactly once here.
    fn submit_limit(
        &mut self,
        instrument: String,
        side: Side,
        price: Cents,
        qty: f64,
        tif: Tif,
        post_only: bool,
    ) {
        if qty <= 0.0 {
            return;
        }
        self.portfolio.total_orders += 1;
        let id = self.next_order_id;
        self.next_order_id += 1;
        let activation_ts = self.latency.order_activation_ts(self.cur_ts, id);
        if activation_ts <= self.cur_ts {
            // zero-latency (or already-due) path: resolve right now against the current book.
            self.resolve_limit(id, &instrument, side, price, qty, tif, post_only, activation_ts);
        } else {
            self.pending_limit.push(PendingLimit {
                order_id: id,
                instrument,
                side,
                price,
                qty,
                tif,
                post_only,
                activation_ts,
            });
        }
    }

    /// Resolve a limit order's marketability against the CURRENT book (called at the order's
    /// activation time) and act on it:
    /// * `post_only` + marketable => REJECT in full (count a `post_only_reject`); else rest if any.
    /// * `Gtc` => take the marketable (crossing) portion bounded by the limit price, then REST the
    ///   remainder as a maker order.
    /// * `Ioc` => take the marketable portion bounded by the limit price, then CANCEL the remainder.
    ///
    /// The crossing take reuses the taker machinery ([`execute_taker_now`]) with the limit price as a
    /// bound, so it never fills past the limit. `activation_ts` is threaded through so a resting
    /// remainder keeps the correct latency-gating activation timestamp.
    #[allow(clippy::too_many_arguments)]
    fn resolve_limit(
        &mut self,
        id: u64,
        instrument: &str,
        side: Side,
        price: Cents,
        qty: f64,
        tif: Tif,
        post_only: bool,
        activation_ts: i64,
    ) {
        let marketable = self.marketable_qty(instrument, side, price, qty);

        // post-only: a marketable order would cross, breaking the maker-only guarantee -> reject it
        // entirely and count the rejection. A non-marketable post-only order rests normally.
        if post_only {
            if marketable > 0.0 {
                self.portfolio.post_only_rejects += 1;
                return;
            }
            self.rest_remainder(id, instrument, side, price, qty, activation_ts);
            return;
        }

        // take the crossing portion immediately (bounded by the limit price) ...
        if marketable > 0.0 {
            self.execute_taker_now(id, instrument, side, marketable, Some(price), self.cur_ts);
        }
        // ... then handle the remainder per TIF.
        let remainder = (qty - marketable).max(0.0);
        match tif {
            // GTC: rest the remainder as a maker order (standard limit semantics).
            Tif::Gtc => {
                if remainder > 1e-12 {
                    self.rest_remainder(id, instrument, side, price, remainder, activation_ts);
                }
            }
            // IOC: never rests — the unfilled remainder is simply cancelled (dropped).
            Tif::Ioc => {}
        }
    }

    /// How much of a limit order at `price` is MARKETABLE right now: a BUY limit crosses if its price
    /// is >= the best ask (it can lift `min(qty, ask liquidity up to price)`); a SELL limit crosses
    /// if its price is <= the best bid. Returns the contracts that would take against the opposing
    /// book (bounded by `qty` and by liquidity inside the limit price), or 0 if not marketable.
    fn marketable_qty(&self, instrument: &str, side: Side, price: Cents, qty: f64) -> f64 {
        let book = match self.books.get(instrument) {
            Some(b) => b,
            None => return 0.0,
        };
        match side {
            // BUY: marketable iff price >= best ask. Available = Σ ask size at levels <= price.
            Side::Bid => match book.best_ask() {
                Some((ask_px, _)) if price.0 >= ask_px.0 => {
                    let avail: f64 = book
                        .asks
                        .iter()
                        .take_while(|(&px, _)| px.0 <= price.0)
                        .map(|(_, &s)| s)
                        .sum();
                    qty.min(avail)
                }
                _ => 0.0,
            },
            // SELL: marketable iff price <= best bid. Available = Σ bid size at levels >= price.
            Side::Ask => match book.best_bid() {
                Some((bid_px, _)) if price.0 <= bid_px.0 => {
                    let avail: f64 = book
                        .bids
                        .iter()
                        .filter(|(&px, _)| px.0 >= price.0)
                        .map(|(_, &s)| s)
                        .sum();
                    qty.min(avail)
                }
                _ => 0.0,
            },
        }
    }

    /// Rest `qty` of an order as a maker [`RestingOrder`] at `price`, seeding its `queue_ahead` from
    /// the current book depth and stamping the given `activation_ts` (so a take+rest GTC remainder
    /// keeps the original order's latency gating). Does NOT count a new order (the caller counted it).
    fn rest_remainder(
        &mut self,
        id: u64,
        instrument: &str,
        side: Side,
        price: Cents,
        qty: f64,
        activation_ts: i64,
    ) {
        if qty <= 0.0 {
            return;
        }
        let queue_ahead = self
            .books
            .get(instrument)
            .map(|b| RestingOrder::queue_at_placement(b, side, price))
            .unwrap_or(0.0);
        self.resting.push(RestingOrder {
            id,
            instrument: instrument.to_string(),
            side,
            price,
            remaining: qty,
            queue_ahead,
            activation_ts,
        });
    }

    /// Resolve every in-flight limit order whose `activation_ts <= now`, against the book as it
    /// currently stands. In-flight limits not yet activated are retained.
    fn resolve_due_limit_orders(&mut self, now: i64) {
        if self.pending_limit.is_empty() {
            return;
        }
        let mut due: Vec<PendingLimit> = Vec::new();
        let mut still: Vec<PendingLimit> = Vec::new();
        for pl in self.pending_limit.drain(..) {
            if pl.activation_ts <= now {
                due.push(pl);
            } else {
                still.push(pl);
            }
        }
        self.pending_limit = still;
        for pl in due {
            self.resolve_limit(
                pl.order_id,
                &pl.instrument,
                pl.side,
                pl.price,
                pl.qty,
                pl.tif,
                pl.post_only,
                pl.activation_ts,
            );
        }
    }

    /// At end-of-run, resolve every still in-flight limit order against the current book regardless
    /// of activation time, so no marketable-limit take or resting remainder is silently dropped.
    fn flush_all_pending_limit_orders(&mut self) {
        if self.pending_limit.is_empty() {
            return;
        }
        for pl in std::mem::take(&mut self.pending_limit) {
            self.resolve_limit(
                pl.order_id,
                &pl.instrument,
                pl.side,
                pl.price,
                pl.qty,
                pl.tif,
                pl.post_only,
                pl.activation_ts,
            );
        }
    }

    /// Current resting size at a (side, price) level for an instrument (0 if absent). Used by the
    /// optimistic queue model to detect cancellations at our levels.
    fn level_size(&self, instrument: &str, side: Side, price: Cents) -> f64 {
        self.books
            .get(instrument)
            .map(|b| {
                let map = match side {
                    Side::Bid => &b.bids,
                    Side::Ask => &b.asks,
                };
                map.get(&price).copied().unwrap_or(0.0)
            })
            .unwrap_or(0.0)
    }

    /// OPTIMISTIC queue model: a `decrease` in resting size at (`instrument`, `side`, `price`) is
    /// assumed to be cancellations AHEAD of our resting orders at that exact level, so we advance
    /// each such order up the queue by reducing its `queue_ahead` by `decrease` (floored at 0).
    /// Only orders whose side AND price match the shrinking level are affected. No-op under the
    /// pessimistic default (this is never called then). See [`QueueModel`] for the assumption.
    fn apply_optimistic_queue(&mut self, instrument: &str, side: Side, price: Cents, decrease: f64) {
        if decrease <= 0.0 {
            return;
        }
        for o in self.resting.iter_mut() {
            if o.instrument == instrument && o.side == side && o.price == price && o.queue_ahead > 0.0
            {
                o.queue_ahead = (o.queue_ahead - decrease).max(0.0);
            }
        }
    }

    /// Place a market (taker) order. With latency in effect the order is SENT at `cur_ts` but only
    /// executes once its `activation_ts = cur_ts + latency` is reached: if that is still in the
    /// future it is parked in `pending_market` and executed by [`execute_due_market_orders`] at the
    /// first later step whose event ts has caught up (against the book as of THAT step). Under the
    /// zero-latency default `activation_ts == cur_ts`, so it executes immediately right here —
    /// preserving the original behaviour exactly.
    fn do_market(&mut self, instrument: String, side: Side, qty: f64) {
        if qty <= 0.0 {
            return;
        }
        self.portfolio.total_orders += 1;
        let id = self.next_order_id;
        self.next_order_id += 1;
        let activation_ts = self.latency.order_activation_ts(self.cur_ts, id);
        self.dispatch_taker(id, instrument, side, qty, None, activation_ts);
    }

    /// Route a taker order (a plain market order, or the crossing portion of a marketable limit) to
    /// either immediate execution (if its `activation_ts` has been reached) or the in-flight
    /// `pending_market` queue. `price_bound` bounds the walk for a marketable-limit take (never fill
    /// past the limit price); `None` is a plain market order. Does NOT increment `total_orders` (the
    /// caller owns order-counting), so a single limit order is never double-counted.
    fn dispatch_taker(
        &mut self,
        id: u64,
        instrument: String,
        side: Side,
        qty: f64,
        price_bound: Option<Cents>,
        activation_ts: i64,
    ) {
        if activation_ts <= self.cur_ts {
            self.execute_taker_now(id, &instrument, side, qty, price_bound, self.cur_ts);
        } else {
            self.pending_market.push(PendingMarket {
                order_id: id,
                instrument,
                side,
                qty,
                activation_ts,
                price_bound,
            });
        }
    }

    /// Walk the current book for one market order and apply its taker fills (with slippage + fees)
    /// to the portfolio, stamping fills at `ts`. Unbounded (plain market) variant.
    fn execute_market_now(&mut self, id: u64, instrument: &str, side: Side, qty: f64, ts: i64) {
        self.execute_taker_now(id, instrument, side, qty, None, ts);
    }

    /// Walk the current book for one taker order, optionally bounded by `price_bound` (the limit
    /// price of a marketable limit's crossing portion — never fills past it), applying the taker
    /// fills (with slippage + fees) to the portfolio, stamped at `ts`.
    fn execute_taker_now(
        &mut self,
        id: u64,
        instrument: &str,
        side: Side,
        qty: f64,
        price_bound: Option<Cents>,
        ts: i64,
    ) {
        let fills = if let Some(book) = self.books.get(instrument) {
            execute_market_bounded(id, instrument, side, qty, price_bound, book, &self.fees, ts)
        } else {
            Vec::new()
        };
        for f in fills {
            // taker fills pay adverse slippage on top of walking the book
            let slip = self
                .slippage
                .cost(f.side, f.price, f.qty, Liquidity::Taker);
            self.portfolio.apply_fill_ex(&f, slip);
        }
    }

    /// Execute every in-flight market order whose `activation_ts <= now`, against the book as it
    /// currently stands, stamping fills at `now`. In-flight orders not yet activated are retained.
    fn execute_due_market_orders(&mut self, now: i64) {
        if self.pending_market.is_empty() {
            return;
        }
        let mut due: Vec<PendingMarket> = Vec::new();
        let mut still_pending: Vec<PendingMarket> = Vec::new();
        for pm in self.pending_market.drain(..) {
            if pm.activation_ts <= now {
                due.push(pm);
            } else {
                still_pending.push(pm);
            }
        }
        self.pending_market = still_pending;
        for pm in due {
            self.execute_taker_now(
                pm.order_id,
                &pm.instrument,
                pm.side,
                pm.qty,
                pm.price_bound,
                now,
            );
        }
    }

    /// At end-of-run, execute every still in-flight market order regardless of activation time,
    /// against the current book, stamped at the final timestamp. Ensures no taker order is dropped.
    fn flush_all_pending_market_orders(&mut self) {
        if self.pending_market.is_empty() {
            return;
        }
        let now = self.cur_ts;
        for pm in std::mem::take(&mut self.pending_market) {
            self.execute_taker_now(
                pm.order_id,
                &pm.instrument,
                pm.side,
                pm.qty,
                pm.price_bound,
                now,
            );
        }
    }

    /// BINARY SETTLEMENT: settle every open position whose instrument has a KNOWN outcome in the
    /// settlement map to its $1/$0 binary payoff, stamping the settlement fill at the final
    /// timestamp. Positions in instruments with an UNKNOWN outcome are left untouched (they flatten
    /// at mid afterwards if `flatten_at_end`). Accumulates `settled_pnl` / `num_settled` onto the
    /// portfolio for the report. No-op when the settlement map is empty (no file provided).
    fn settle_known_positions(&mut self) {
        if self.settlement.is_empty() {
            return;
        }
        let ts = self.cur_ts.max(1);
        // Snapshot the instruments to settle first (so we don't borrow the position map while
        // mutating it). Only positions with a known YES/NO payout are settled here.
        let to_settle: Vec<(String, f64)> = self
            .portfolio
            .positions
            .iter()
            .filter(|(_, p)| p.net_qty.abs() > 1e-12)
            .filter_map(|(inst, _)| {
                self.settlement
                    .outcome(inst)
                    .payout()
                    .map(|payout| (inst.clone(), payout))
            })
            .collect();
        for (inst, payout) in to_settle {
            let pnl = self.portfolio.settle_position(&inst, payout, ts);
            self.portfolio.settled_pnl += pnl;
            self.portfolio.num_settled += 1;
        }
    }

    /// Flatten every open position via market orders against the current book.
    fn flatten_all(&mut self) {
        let insts: Vec<(String, f64)> = self
            .portfolio
            .positions
            .iter()
            .filter(|(_, p)| p.net_qty.abs() > 1e-12)
            .map(|(k, p)| (k.clone(), p.net_qty))
            .collect();
        for (inst, net) in insts {
            // long -> sell (Ask), short -> buy (Bid)
            let side = if net > 0.0 { Side::Ask } else { Side::Bid };
            self.do_market(inst, side, net.abs());
        }
    }
}

impl Ctx for Engine {
    fn ts_ns(&self) -> i64 {
        self.cur_ts
    }

    fn best_bid(&self, instrument: &str) -> Option<(Cents, f64)> {
        self.books.best_bid(instrument)
    }

    fn best_ask(&self, instrument: &str) -> Option<(Cents, f64)> {
        self.books.best_ask(instrument)
    }

    fn mid(&self, instrument: &str) -> Option<f64> {
        self.books.mid(instrument)
    }

    fn imbalance(&self, instrument: &str) -> Option<f64> {
        self.books.get(instrument).and_then(|b| b.imbalance())
    }

    fn microprice(&self, instrument: &str) -> Option<f64> {
        self.books.get(instrument).and_then(|b| b.microprice())
    }

    // ---- LOGISTIC STRIKE-CURVE features ----
    // Each of these lazily fits the instrument's event ladder (the fit is cached and only recomputed
    // when a new delta marked the event dirty) and reads off the requested quantity. The `RefCell`
    // borrow is what lets us mutate the fit cache through an `&self` `Ctx` method.

    fn implied_fair_value(&self, instrument: &str) -> Option<f64> {
        self.event_curves
            .borrow_mut()
            .fit_for_instrument(instrument)
            .map(|f| f.fair_value())
    }

    fn implied_vol(&self, instrument: &str) -> Option<f64> {
        self.event_curves
            .borrow_mut()
            .fit_for_instrument(instrument)
            .map(|f| f.implied_vol())
    }

    fn fitted_price(&self, instrument: &str) -> Option<f64> {
        // Need this strike's `K` to evaluate `S(K)`.
        let (_event, k) = crate::fit_logistic::parse_event_strike(instrument)?;
        self.event_curves
            .borrow_mut()
            .fit_for_instrument(instrument)
            .map(|f| f.fitted(k))
    }

    fn fit_edge(&self, instrument: &str) -> Option<f64> {
        // edge = market mid − fitted_price; needs both a live mid and a fit at this strike.
        let mid = self.books.mid(instrument)?;
        let (_event, k) = crate::fit_logistic::parse_event_strike(instrument)?;
        self.event_curves
            .borrow_mut()
            .fit_for_instrument(instrument)
            .map(|f| f.edge(k, mid))
    }

    fn fit_quality(&self, instrument: &str) -> Option<f64> {
        self.event_curves
            .borrow_mut()
            .fit_for_instrument(instrument)
            .map(|f| f.rmse)
    }

    fn position(&self, instrument: &str) -> f64 {
        self.portfolio
            .positions
            .get(instrument)
            .map(|p| p.net_qty)
            .unwrap_or(0.0)
    }

    fn cash(&self) -> f64 {
        self.portfolio.cash
    }

    fn instruments(&self) -> Vec<String> {
        self.books.books.keys().cloned().collect()
    }

    fn open_orders(&self, instrument: &str) -> Vec<OrderView> {
        self.resting
            .iter()
            .filter(|o| o.instrument == instrument)
            .map(|o| OrderView {
                id: o.id,
                side: o.side,
                price: o.price,
                remaining: o.remaining,
            })
            .collect()
    }

    fn place_limit_ex(
        &mut self,
        instrument: &str,
        side: Side,
        price: Cents,
        qty: f64,
        tif: Tif,
        post_only: bool,
    ) {
        self.pending.push(EngineAction::PlaceLimit {
            instrument: instrument.to_string(),
            side,
            price,
            qty,
            tif,
            post_only,
        });
    }

    fn place_market(&mut self, instrument: &str, side: Side, qty: f64) {
        self.pending.push(EngineAction::PlaceMarket {
            instrument: instrument.to_string(),
            side,
            qty,
        });
    }

    fn cancel(&mut self, order_id: u64) {
        self.pending.push(EngineAction::Cancel { order_id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategies::noop::Noop;
    use crate::types::{Action, BookDelta};

    fn delta(ts: i64, side: Side, price: i32, size: f64) -> MarketEvent {
        MarketEvent::Delta(BookDelta {
            ts_ns: ts,
            instrument: "X".into(),
            action: Action::Add,
            side,
            price: Cents(price),
            size,
            sequence: ts,
            is_snapshot: false,
        })
    }

    /// The logistic strike-curve `Ctx` methods surface a sane implied fair value (μ) and implied vol
    /// after a ladder of strikes streams through `step`. Drives a clean logistic ladder, then reads
    /// the fit through the engine's own `Ctx` impl (the engine IS the `Ctx`).
    #[test]
    fn ctx_exposes_sane_mu_on_ladder() {
        let cfg = BacktestConfig::default();
        let mut eng = Engine::new(&cfg);
        let mut s = Noop::default();
        let (mu, scale) = (3.0_f64, 0.075_f64);
        let event = "KXNATGASD-26JUN1517";
        // Stream a two-sided book for each of 28 strikes whose mid sits on the logistic curve.
        let mut seq = 1i64;
        let mut sample_inst = String::new();
        for i in 0..28 {
            let k = 2.7 + 0.6 * i as f64 / 27.0;
            let fair = 1.0 / (1.0 + ((k - mu) / scale).exp());
            let mid_c = (fair.clamp(0.02, 0.98) * 100.0).round() as i32;
            let inst = format!("{event}-T{k:.3}");
            if i == 14 {
                sample_inst = inst.clone();
            }
            let bid = (mid_c - 1).clamp(1, 98);
            let ask = (mid_c + 1).clamp(2, 99);
            let bd = |side: Side, px: i32, seq: i64, snap: bool| {
                MarketEvent::Delta(BookDelta {
                    ts_ns: 1_000 + seq,
                    instrument: inst.clone(),
                    action: Action::Add,
                    side,
                    price: Cents(px),
                    size: 100.0,
                    sequence: seq,
                    is_snapshot: snap,
                })
            };
            eng.step(&bd(Side::Bid, bid, seq, true), &mut s);
            seq += 1;
            eng.step(&bd(Side::Ask, ask, seq, false), &mut s);
            seq += 1;
        }
        // Read the fit through the Ctx interface.
        let fv = Ctx::implied_fair_value(&eng, &sample_inst).expect("μ should be available");
        assert!((fv - mu).abs() < 0.03, "implied fair value off: {fv}");
        let iv = Ctx::implied_vol(&eng, &sample_inst).expect("implied vol available");
        // s·π/√3 ≈ 0.075·1.8138 ≈ 0.136
        assert!((iv - scale * std::f64::consts::PI / 3.0f64.sqrt()).abs() < 0.02, "iv off: {iv}");
        let fp = Ctx::fitted_price(&eng, &sample_inst).expect("fitted price available");
        assert!(fp > 0.0 && fp < 1.0);
        assert!(Ctx::fit_quality(&eng, &sample_inst).unwrap() < 0.02);
        // a non-ladder instrument has no fit
        assert!(Ctx::implied_fair_value(&eng, "NOT-A-LADDER").is_none());
    }

    #[test]
    fn empty_run_produces_flat_report() {
        let cfg = BacktestConfig::default();
        let eng = Engine::new(&cfg);
        let mut s = Noop::default();
        let r = eng.run(std::iter::empty(), &mut s, &cfg);
        assert_eq!(r.summary.starting_balance, 1000.0);
        assert!((r.summary.ending_balance - 1000.0).abs() < 1e-9);
    }

    /// A SELLING-aggressor trade (`aggressor_yes == false`): the aggressor hit bids, so this print
    /// can fill resting BUY (bid) orders that it crosses. This is the default `trade()` used by most
    /// tests, which quote/fill on the bid side.
    fn trade(ts: i64, price: i32, size: f64) -> MarketEvent {
        MarketEvent::Trade(TradeEvent {
            ts_ns: ts,
            instrument: "X".into(),
            aggressor_yes: false,
            price: Cents(price),
            size,
            trade_id: "t".into(),
        })
    }

    /// A BUYING-aggressor trade (`aggressor_yes == true`): the aggressor lifted asks, so this print
    /// can fill resting SELL (ask) orders that it crosses. Used by tests whose strategy quotes/fills
    /// on the ask side (see the aggressor gate in `match_resting_against_trade`).
    fn buy_trade(ts: i64, price: i32, size: f64) -> MarketEvent {
        MarketEvent::Trade(TradeEvent {
            ts_ns: ts,
            instrument: "X".into(),
            aggressor_yes: true,
            price: Cents(price),
            size,
            trade_id: "t".into(),
        })
    }

    /// A test strategy that places a single resting bid once and never re-quotes.
    struct PlaceOnce {
        placed: bool,
    }
    impl Strategy for PlaceOnce {
        fn name(&self) -> &str {
            "place_once"
        }
        fn on_event(&mut self, _ev: &MarketEvent, ctx: &mut dyn Ctx) {
            if !self.placed {
                // rest a bid at 45 (inside the 40/60 spread) so queue_ahead is 0
                ctx.place_limit("X", Side::Bid, Cents(45), 10.0);
                self.placed = true;
            }
        }
    }

    #[test]
    fn latency_gates_fill_until_activation() {
        // order placed at t=1s with 1s latency -> activation 2s. A trade at t=1.5s must NOT fill;
        // a trade at t=2.5s must fill.
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = false;
        cfg.execution.latency.enabled = true;
        cfg.execution.latency.order_latency_ns = 1_000_000_000; // 1s

        // seed a book so the order has liquidity context, then trades.
        let evs = vec![
            delta(1_000_000_000, Side::Bid, 40, 100.0),
            delta(1_000_000_000, Side::Ask, 60, 100.0),
            trade(1_500_000_000, 40, 10.0), // before activation: no fill
            trade(2_500_000_000, 40, 10.0), // after activation: fills
        ];
        let eng = Engine::new(&cfg);
        let mut s = PlaceOnce { placed: false };
        let r = eng.run(evs.into_iter(), &mut s, &cfg);
        // exactly one maker fill of 10 contracts
        assert_eq!(r.summary.num_fills, 1, "should fill once, after activation");

        // control: same data, NO latency -> the 1.5s trade fills (so still 1 fill total since the
        // order is fully consumed by the first trade).
        let mut cfg2 = BacktestConfig::default();
        cfg2.flatten_at_end = false;
        let evs2 = vec![
            delta(1_000_000_000, Side::Bid, 40, 100.0),
            delta(1_000_000_000, Side::Ask, 60, 100.0),
            trade(1_500_000_000, 40, 10.0),
        ];
        let eng2 = Engine::new(&cfg2);
        let mut s2 = PlaceOnce { placed: false };
        let r2 = eng2.run(evs2.into_iter(), &mut s2, &cfg2);
        assert_eq!(r2.summary.num_fills, 1, "no-latency fills on first crossing trade");
    }

    /// A two-sided requoting strategy: cancels its quotes and re-places them around the mid on every
    /// trade. This generates a STREAM of orders (each getting its own sampled latency), so a
    /// stochastic latency distribution can actually shift fills relative to the fixed model.
    struct Requoter {
        bid: Option<u64>,
        ask: Option<u64>,
    }
    impl Strategy for Requoter {
        fn name(&self) -> &str {
            "requoter"
        }
        fn on_event(&mut self, _ev: &MarketEvent, ctx: &mut dyn Ctx) {
            if let Some(mid) = ctx.mid("X") {
                let mid_c = (mid * 100.0).round() as i32;
                if let Some(id) = self.bid.take() {
                    ctx.cancel(id);
                }
                if let Some(id) = self.ask.take() {
                    ctx.cancel(id);
                }
                ctx.place_limit("X", Side::Bid, Cents(mid_c - 2), 5.0);
                ctx.place_limit("X", Side::Ask, Cents(mid_c + 2), 5.0);
            }
        }
    }

    /// Build a small synthetic event stream with several trades that walk the book — enough for the
    /// requoting strategy to place many orders and (under a wide latency distribution) fill at
    /// different times than the fixed model.
    fn busy_stream() -> Vec<MarketEvent> {
        let s = 1_000_000_000i64;
        let mut evs = vec![
            delta(s, Side::Bid, 40, 100.0),
            delta(s, Side::Ask, 60, 100.0),
        ];
        for k in 0..40i64 {
            let ts = (2 + k) * s;
            // alternate trades at the bid and ask so the requoter both buys and sells.
            let px = if k % 2 == 0 { 40 } else { 60 };
            evs.push(trade(ts, px, 5.0));
        }
        evs
    }

    /// SPEC 5(a) — the default (no dist configured) and an explicit `Fixed` dist must produce a
    /// BYTE-FOR-BYTE identical report, and (since Fixed has no RNG) the seed must not matter. This is
    /// the no-op / unchanged-default guarantee.
    #[test]
    fn fixed_dist_is_byte_for_byte_default_and_seed_independent() {
        // Default config: latency on, fixed model with order latency + jitter (the legacy path).
        let mut base = BacktestConfig::default();
        base.flatten_at_end = false;
        base.execution.latency.enabled = true;
        base.execution.latency.order_latency_ns = 500_000_000;
        base.execution.latency.jitter_ns = 100_000_000;

        let report_for = |cfg: &BacktestConfig| {
            let eng = Engine::new(cfg);
            let mut s = Requoter { bid: None, ask: None };
            let r = eng.run(busy_stream().into_iter(), &mut s, cfg);
            serde_json::to_string(&r).unwrap()
        };

        // 1. default dist (Fixed implicitly) vs explicitly Fixed => identical.
        let mut explicit = base.clone();
        explicit.execution.latency.dist = crate::config::LatencyDist::Fixed;
        assert_eq!(
            report_for(&base),
            report_for(&explicit),
            "explicit Fixed must equal the implicit default"
        );

        // 2. Fixed must NOT depend on the seed (no RNG on this path).
        let mut seeded = base.clone();
        seeded.execution.latency.seed = 999_999;
        assert_eq!(
            report_for(&base),
            report_for(&seeded),
            "Fixed dist must be seed-independent (byte-for-byte stable)"
        );
    }

    /// SPEC 5(b)+5(e) — the seeded PRNG is reproducible (same seed => identical report), different
    /// seeds differ, and a stochastic (Uniform) run differs from the Fixed run: latencies actually
    /// vary and that changes outcomes.
    #[test]
    fn uniform_dist_is_seed_reproducible_and_differs_from_fixed() {
        let mk = |dist: crate::config::LatencyDist, seed: u64| {
            let mut cfg = BacktestConfig::default();
            cfg.flatten_at_end = false;
            cfg.execution.latency.enabled = true;
            cfg.execution.latency.order_latency_ns = 500_000_000; // base/mean for fixed
            cfg.execution.latency.dist = dist;
            cfg.execution.latency.seed = seed;
            let eng = Engine::new(&cfg);
            let mut s = Requoter { bid: None, ask: None };
            let r = eng.run(busy_stream().into_iter(), &mut s, &cfg);
            serde_json::to_string(&r).unwrap()
        };

        // A WIDE uniform so sampled latencies span several event gaps (1s each).
        let uni = || crate::config::LatencyDist::Uniform {
            min_ns: 0,
            max_ns: 5_000_000_000,
        };

        // same seed => identical report.
        assert_eq!(mk(uni(), 1), mk(uni(), 1), "same seed must reproduce");
        // different seed => different report.
        assert_ne!(mk(uni(), 1), mk(uni(), 2), "different seeds must differ");
        // a Uniform run differs from the Fixed run on the same data (latency variance changes fills).
        let fixed = mk(crate::config::LatencyDist::Fixed, 1);
        assert_ne!(fixed, mk(uni(), 1), "Uniform must differ from Fixed");
    }

    /// A market-making-ish strategy that quotes a two-sided book once it sees a mid, used to
    /// exercise placements + fills in the equivalence test below.
    struct QuoteBoth {
        quoted: bool,
    }
    impl Strategy for QuoteBoth {
        fn name(&self) -> &str {
            "quote_both"
        }
        fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
            if self.quoted {
                return;
            }
            if let Some(mid) = ctx.mid("X") {
                let mid_c = (mid * 100.0).round() as i32;
                ctx.place_limit("X", Side::Bid, Cents(mid_c - 2), 20.0);
                ctx.place_limit("X", Side::Ask, Cents(mid_c + 2), 20.0);
                self.quoted = true;
                let _ = ev;
            }
        }
    }

    /// THE KEY DESIGN GUARANTEE: feeding events one-by-one through the incremental core
    /// (`step` + `snapshot` + `finalize` + `into_report`) produces a byte-identical report to the
    /// batch `run_collecting`. This is what lets batch and live paper-trading share one engine.
    #[test]
    fn incremental_core_matches_batch_run() {
        // A representative dataset: a snapshot, several deltas, and crossing trades so the strategy
        // both quotes and fills, plus rewards + slippage + latency-off defaults.
        let evs = || {
            vec![
                delta(1_000_000_000, Side::Bid, 40, 100.0),
                delta(1_000_000_000, Side::Ask, 60, 100.0),
                trade(1_200_000_000, 60, 5.0),
                delta(1_300_000_000, Side::Bid, 48, 50.0),
                delta(1_300_000_000, Side::Ask, 52, 50.0),
                trade(1_400_000_000, 46, 10.0), // crosses a resting bid at 48
                buy_trade(2_000_000_000, 54, 8.0),  // crosses a resting ask at 52
                delta(2_500_000_000, Side::Bid, 50, 30.0),
            ]
        };

        let cfg = BacktestConfig::default();

        // Batch path.
        let eng_a = Engine::new(&cfg);
        let mut s_a = QuoteBoth { quoted: false };
        let batch = eng_a.run_collecting(evs().into_iter(), &mut s_a, &cfg);

        // Incremental path: drive step() per event with the SAME snapshot cadence the batch loop
        // uses (maybe_snapshot after each event), then finalize + into_report.
        let mut eng_b = Engine::new(&cfg);
        let mut s_b = QuoteBoth { quoted: false };
        for ev in evs() {
            eng_b.step(&ev, &mut s_b);
            let now = eng_b.now_ns();
            eng_b.snapshot_maybe(now);
        }
        eng_b.finalize(&mut s_b, cfg.flatten_at_end);
        let incr = eng_b.into_report(s_b.name(), &cfg);

        // Reports must be byte-identical when serialized.
        let ja = serde_json::to_string(&batch.report).unwrap();
        let jb = serde_json::to_string(&incr.report).unwrap();
        assert_eq!(ja, jb, "incremental core report != batch run report");

        // And the structured artifacts (fills, round-trips, observed trades) must match too.
        assert_eq!(batch.portfolio.fills.len(), incr.portfolio.fills.len());
        assert_eq!(
            batch.portfolio.round_trips.len(),
            incr.portfolio.round_trips.len()
        );
        assert_eq!(batch.observed_trades.len(), incr.observed_trades.len());
        assert!((batch.portfolio.cash - incr.portfolio.cash).abs() < 1e-12);
    }

    /// A strategy that sends exactly one MARKET buy on the first event it sees, then never again.
    /// Used to prove the latency-deferred taker execution.
    struct MarketOnce {
        sent: bool,
        side: Side,
        qty: f64,
    }
    impl Strategy for MarketOnce {
        fn name(&self) -> &str {
            "market_once"
        }
        fn on_event(&mut self, _ev: &MarketEvent, ctx: &mut dyn Ctx) {
            if !self.sent {
                ctx.place_market("X", self.side, self.qty);
                self.sent = true;
            }
        }
    }

    /// (a) A market order placed at T with latency L produces NO fill before T+L, and fills at the
    /// first event whose ts >= T+L, against the book AS OF that future step.
    #[test]
    fn market_order_defers_until_activation_under_latency() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = false;
        cfg.execution.latency.enabled = true;
        cfg.execution.latency.order_latency_ns = 1_000_000_000; // 1s

        // Book seeded at t=1s with asks at 60. Strategy sends a market BUY on the first event (t=1s),
        // so activation = 2s. There is an event at 1.5s (before activation -> still no fill) and an
        // event at 2.5s (>= activation -> fills). We MOVE the book between 1.5s and 2.5s so we can
        // prove the fill walks the FUTURE book (ask 70), not the book at send time (ask 60).
        let evs = vec![
            delta(1_000_000_000, Side::Ask, 60, 100.0), // first event: order sent here
            delta(1_500_000_000, Side::Bid, 50, 10.0),  // before activation: no taker fill yet
            // the 60 ask is replaced by a 70 ask before activation
            MarketEvent::Delta(BookDelta {
                ts_ns: 1_600_000_000,
                instrument: "X".into(),
                action: Action::Update,
                side: Side::Ask,
                price: Cents(60),
                size: 0.0,
                sequence: 99,
                is_snapshot: false,
            }),
            delta(1_700_000_000, Side::Ask, 70, 100.0),
            delta(2_500_000_000, Side::Bid, 55, 5.0), // >= activation: market BUY executes now
        ];
        let eng = Engine::new(&cfg);
        let mut s = MarketOnce {
            sent: false,
            side: Side::Bid,
            qty: 10.0,
        };
        let out = eng.run_collecting(evs.into_iter(), &mut s, &cfg);
        // Exactly one taker fill of 10 contracts.
        assert_eq!(out.report.summary.num_fills, 1, "one taker fill after activation");
        let fill = &out.portfolio.fills[0];
        assert_eq!(fill.liquidity, Liquidity::Taker);
        assert_eq!(fill.qty, 10.0);
        // It filled at/after activation (2s), at the 2.5s event...
        assert_eq!(fill.ts_ns, 2_500_000_000, "fill stamped at the activating event ts");
        // ...and against the FUTURE book (price 70, not the send-time 60).
        assert_eq!(fill.price, Cents(70), "walked the latency-delayed book");
    }

    /// (b) At ZERO latency (the default), a market order still fills IMMEDIATELY during the same
    /// event it was placed — identical to the original behaviour.
    #[test]
    fn market_order_fills_immediately_at_zero_latency() {
        let cfg = BacktestConfig::default(); // latency disabled by default
        assert!(!cfg.execution.latency.enabled);

        // One event seeds the book with asks at 60; the strategy markets a BUY on that same event,
        // which must fill right then, walking the book at 60.
        let evs = vec![delta(1_000_000_000, Side::Ask, 60, 100.0)];
        let eng = Engine::new(&cfg);
        let mut s = MarketOnce {
            sent: false,
            side: Side::Bid,
            qty: 10.0,
        };
        let out = eng.run_collecting(evs.into_iter(), &mut s, &cfg);
        assert_eq!(out.report.summary.num_fills, 1, "immediate taker fill at zero latency");
        let fill = &out.portfolio.fills[0];
        assert_eq!(fill.liquidity, Liquidity::Taker);
        assert_eq!(fill.price, Cents(60));
        assert_eq!(fill.ts_ns, 1_000_000_000, "filled during the placing event");
    }

    /// (c) A resting limit order isn't hit by a trade before its activation (latency gating on the
    /// maker side) — complements the existing `latency_gates_fill_until_activation` test.
    #[test]
    fn resting_limit_not_hit_before_activation() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = false;
        cfg.execution.latency.enabled = true;
        cfg.execution.latency.order_latency_ns = 1_000_000_000; // 1s

        // Order placed at t=1s -> activation 2s. A crossing trade at 1.5s (before activation) must
        // NOT fill it; nothing later crosses, so the order ends the run unfilled.
        let evs = vec![
            delta(1_000_000_000, Side::Bid, 40, 100.0),
            delta(1_000_000_000, Side::Ask, 60, 100.0),
            trade(1_500_000_000, 40, 10.0), // before activation: must NOT fill the resting bid
        ];
        let eng = Engine::new(&cfg);
        let mut s = PlaceOnce { placed: false };
        let r = eng.run(evs.into_iter(), &mut s, &cfg);
        assert_eq!(r.summary.num_fills, 0, "resting order must not fill before activation");
    }

    // ========================================================================
    // Risk-layer tests
    // ========================================================================

    /// A strategy that markets a fixed BUY qty on EVERY event (used to push position up so the caps
    /// can clamp it). With `side`, it can also be pointed the other way to flatten.
    struct MarketEvery {
        side: Side,
        qty: f64,
        instrument: String,
    }
    impl Strategy for MarketEvery {
        fn name(&self) -> &str {
            "market_every"
        }
        fn on_event(&mut self, _ev: &MarketEvent, ctx: &mut dyn Ctx) {
            ctx.place_market(&self.instrument, self.side, self.qty);
        }
    }

    fn delta_inst(inst: &str, ts: i64, side: Side, price: i32, size: f64) -> MarketEvent {
        MarketEvent::Delta(BookDelta {
            ts_ns: ts,
            instrument: inst.into(),
            action: Action::Add,
            side,
            price: Cents(price),
            size,
            sequence: ts,
            is_snapshot: false,
        })
    }

    /// (a) `max_position_per_instrument` clamps an OPENING order, and never blocks a FLATTENING one.
    #[test]
    fn max_position_clamps_open_but_not_flatten() {
        // Cap |net| at 30. A book with deep asks at 60 so market BUYs fill.
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = false;
        cfg.execution.risk.max_position_per_instrument = Some(30.0);

        // Strategy buys 100 each event; the first buy must be clamped to 30 (hitting the cap), and
        // subsequent buys clamped to 0 (rejected).
        let evs = vec![
            delta(1_000_000_000, Side::Ask, 60, 1000.0),
            delta(1_100_000_000, Side::Ask, 60, 1000.0),
            delta(1_200_000_000, Side::Ask, 60, 1000.0),
        ];
        let eng = Engine::new(&cfg);
        let mut s = MarketEvery {
            side: Side::Bid,
            qty: 100.0,
            instrument: "X".into(),
        };
        let out = eng.run_collecting(evs.into_iter(), &mut s, &cfg);
        // net long must be exactly the cap.
        assert!((out.portfolio.positions["X"].net_qty - 30.0).abs() < 1e-9,
            "net should be clamped to 30, got {}", out.portfolio.positions["X"].net_qty);
        // later over-cap buys were rejected.
        assert!(out.report.summary.risk_rejections >= 1);

        // Now prove a FLATTENING order is never blocked even though |net| is at the cap: a strategy
        // that sells (reduces) should be allowed to fully flatten.
        let mut cfg2 = BacktestConfig::default();
        cfg2.flatten_at_end = false;
        cfg2.execution.risk.max_position_per_instrument = Some(30.0);
        // Seed a long 30 by buying, then sell 30 to flatten — the sell must pass unclamped.
        struct OpenThenFlatten { opened: bool, flattened: bool }
        impl Strategy for OpenThenFlatten {
            fn name(&self) -> &str { "open_then_flatten" }
            fn on_event(&mut self, _ev: &MarketEvent, ctx: &mut dyn Ctx) {
                // need a two-sided book so both the open and the flatten actually fill.
                if ctx.best_bid("X").is_none() || ctx.best_ask("X").is_none() {
                    return;
                }
                if !self.opened {
                    ctx.place_market("X", Side::Bid, 30.0); // open to the cap
                    self.opened = true;
                } else if !self.flattened {
                    ctx.place_market("X", Side::Ask, 30.0); // flatten — must NOT be clamped
                    self.flattened = true;
                }
            }
        }
        let evs2 = vec![
            delta(1_000_000_000, Side::Bid, 40, 1000.0),
            delta(1_000_000_000, Side::Ask, 60, 1000.0),
            delta(1_100_000_000, Side::Bid, 40, 1000.0),
            delta(1_200_000_000, Side::Bid, 40, 1000.0),
        ];
        let eng2 = Engine::new(&cfg2);
        let mut s2 = OpenThenFlatten { opened: false, flattened: false };
        let out2 = eng2.run_collecting(evs2.into_iter(), &mut s2, &cfg2);
        let net = out2.portfolio.positions.get("X").map(|p| p.net_qty).unwrap_or(0.0);
        assert!(net.abs() < 1e-9, "flattening order must fully close the position, net={net}");
    }

    /// (b) `max_gross_position` caps the TOTAL |net| across two instruments.
    #[test]
    fn max_gross_caps_total_across_instruments() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = false;
        cfg.execution.risk.max_gross_position = Some(50.0);

        // Buy 40 of A then try to buy 40 of B: gross is capped at 50, so B is clamped to 10.
        struct TwoInst { step: i32 }
        impl Strategy for TwoInst {
            fn name(&self) -> &str { "two_inst" }
            fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
                // only act on A's first event and B's first event to keep it deterministic.
                if ev.instrument() == "A" && self.step == 0 {
                    ctx.place_market("A", Side::Bid, 40.0);
                    self.step = 1;
                } else if ev.instrument() == "B" && self.step == 1 {
                    ctx.place_market("B", Side::Bid, 40.0);
                    self.step = 2;
                }
            }
        }
        let evs = vec![
            delta_inst("A", 1_000_000_000, Side::Ask, 60, 1000.0),
            delta_inst("B", 1_100_000_000, Side::Ask, 60, 1000.0),
        ];
        let eng = Engine::new(&cfg);
        let mut s = TwoInst { step: 0 };
        let out = eng.run_collecting(evs.into_iter(), &mut s, &cfg);
        let a = out.portfolio.positions["A"].net_qty;
        let b = out.portfolio.positions["B"].net_qty;
        assert!((a - 40.0).abs() < 1e-9, "A net {a}");
        assert!((b - 10.0).abs() < 1e-9, "B clamped to gross headroom, net {b}");
        assert!((a.abs() + b.abs() - 50.0).abs() < 1e-9, "gross must equal cap");
    }

    /// (c) `equity_floor` HALTs: cancels resting orders, flattens, and ignores later orders. No fills
    /// occur after the halt ts, and the ending position is flat.
    #[test]
    fn equity_floor_halts_and_flattens() {
        // Start small; buy into a long, then the mark collapses so equity drops below the floor.
        let mut cfg = BacktestConfig::default();
        cfg.starting_balance = 100.0;
        cfg.flatten_at_end = true;
        cfg.execution.risk.equity_floor = Some(50.0);

        // Buy 100 @ 0.60 (cash 100 -> 40, long 100 marked ~0.60 => equity ~100). Then the book
        // collapses to mid ~0.05 so equity ~= 40 + 100*0.05 = 45 < 50 => HALT + flatten at ~0.05.
        struct BuyOnce { done: bool }
        impl Strategy for BuyOnce {
            fn name(&self) -> &str { "buy_once" }
            fn on_event(&mut self, _ev: &MarketEvent, ctx: &mut dyn Ctx) {
                if !self.done {
                    if ctx.best_ask("X").is_some() {
                        ctx.place_market("X", Side::Bid, 100.0);
                        self.done = true;
                    }
                }
            }
        }
        // Keep a two-sided book throughout (so equity always marks at mid AND the flatten has
        // liquidity). We collapse the mid by adding new, much lower bid/ask levels and removing the
        // high ones — but in an order that never leaves the book one-sided at a step boundary.
        let evs = vec![
            delta(1_000_000_000, Side::Bid, 58, 1000.0),
            delta(1_000_000_000, Side::Ask, 60, 1000.0), // buy fills here at 60
            // add the low levels FIRST (book now has bids {58,4}, asks {60,6}; mid still ~0.59)...
            delta(1_900_000_000, Side::Bid, 4, 1000.0),
            delta(1_900_000_000, Side::Ask, 6, 1000.0),
            // ...then remove the HIGH levels so best bid/ask become 4/6 -> mid ~0.05 -> HALT.
            MarketEvent::Delta(BookDelta {
                ts_ns: 2_000_000_000, instrument: "X".into(), action: Action::Update,
                side: Side::Bid, price: Cents(58), size: 0.0, sequence: 10, is_snapshot: false,
            }),
            MarketEvent::Delta(BookDelta {
                ts_ns: 2_000_000_000, instrument: "X".into(), action: Action::Update,
                side: Side::Ask, price: Cents(60), size: 0.0, sequence: 11, is_snapshot: false,
            }),
            // a later event: strategy would buy again but must be ignored (halted).
            delta(3_000_000_000, Side::Ask, 6, 1000.0),
        ];
        let eng = Engine::new(&cfg);
        let mut s = BuyOnce { done: false };
        let out = eng.run_collecting(evs.into_iter(), &mut s, &cfg);
        assert!(out.report.summary.halted, "run should have halted");
        assert!(out.report.summary.halt_reason.contains("equity_floor"), "{}", out.report.summary.halt_reason);
        // ending position flat (flattened at halt).
        let net = out.portfolio.positions.get("X").map(|p| p.net_qty).unwrap_or(0.0);
        assert!(net.abs() < 1e-9, "must be flat after halt, net={net}");
        // No fill is stamped after the halt timestamp (2s): the only fills are the buy at 1s and the
        // flatten at 2s.
        let halt_ts = 2_000_000_000;
        let fills_after: Vec<_> = out.portfolio.fills.iter().filter(|f| f.ts_ns > halt_ts).collect();
        assert!(fills_after.is_empty(), "no fills after the halt ts, got {}", fills_after.len());
        // exactly two fills: the open and the flatten.
        assert_eq!(out.portfolio.fills.len(), 2, "open + flatten only");
    }

    /// (d) `max_drawdown_pct` triggers a HALT once drawdown from the peak crosses the threshold.
    #[test]
    fn max_drawdown_pct_halts() {
        let mut cfg = BacktestConfig::default();
        cfg.starting_balance = 100.0;
        cfg.flatten_at_end = true;
        cfg.execution.risk.max_drawdown_pct = Some(40.0); // halt at >=40% off the peak

        // Buy 100 @ 0.60; equity stays ~100 (peak). Then mid collapses to ~0.05 -> equity ~45 ->
        // drawdown ~55% from the 100 peak -> HALT.
        struct BuyOnce { done: bool }
        impl Strategy for BuyOnce {
            fn name(&self) -> &str { "buy_once" }
            fn on_event(&mut self, _ev: &MarketEvent, ctx: &mut dyn Ctx) {
                if !self.done && ctx.best_ask("X").is_some() {
                    ctx.place_market("X", Side::Bid, 100.0);
                    self.done = true;
                }
            }
        }
        let evs = vec![
            delta(1_000_000_000, Side::Bid, 58, 1000.0),
            delta(1_000_000_000, Side::Ask, 60, 1000.0),
            delta(1_900_000_000, Side::Bid, 4, 1000.0),
            delta(1_900_000_000, Side::Ask, 6, 1000.0),
            MarketEvent::Delta(BookDelta {
                ts_ns: 2_000_000_000, instrument: "X".into(), action: Action::Update,
                side: Side::Bid, price: Cents(58), size: 0.0, sequence: 10, is_snapshot: false,
            }),
            MarketEvent::Delta(BookDelta {
                ts_ns: 2_000_000_000, instrument: "X".into(), action: Action::Update,
                side: Side::Ask, price: Cents(60), size: 0.0, sequence: 11, is_snapshot: false,
            }),
        ];
        let eng = Engine::new(&cfg);
        let mut s = BuyOnce { done: false };
        let out = eng.run_collecting(evs.into_iter(), &mut s, &cfg);
        assert!(out.report.summary.halted, "drawdown should halt");
        assert!(out.report.summary.halt_reason.contains("drawdown"), "{}", out.report.summary.halt_reason);
        let net = out.portfolio.positions.get("X").map(|p| p.net_qty).unwrap_or(0.0);
        assert!(net.abs() < 1e-9, "flat after halt");
    }

    /// (e) NO-OP GUARANTEE: with all risk fields None, the report is BYTE-FOR-BYTE identical to a
    /// run built with a config that has no risk layer at all, on a fixed event stream.
    #[test]
    fn risk_defaults_reproduce_report_byte_for_byte() {
        let evs = || {
            vec![
                delta(1_000_000_000, Side::Bid, 40, 100.0),
                delta(1_000_000_000, Side::Ask, 60, 100.0),
                trade(1_200_000_000, 60, 5.0),
                delta(1_300_000_000, Side::Bid, 48, 50.0),
                delta(1_300_000_000, Side::Ask, 52, 50.0),
                trade(1_400_000_000, 46, 10.0),
                buy_trade(2_000_000_000, 54, 8.0),
                delta(2_500_000_000, Side::Bid, 50, 30.0),
            ]
        };

        // Baseline: plain default config (risk all None).
        let cfg_base = BacktestConfig::default();
        let eng_a = Engine::new(&cfg_base);
        let mut s_a = QuoteBoth { quoted: false };
        let base = eng_a.run_collecting(evs().into_iter(), &mut s_a, &cfg_base);

        // Explicitly-default risk config — must be identical.
        let mut cfg_risk = BacktestConfig::default();
        cfg_risk.execution.risk = RiskConfig::default();
        let eng_b = Engine::new(&cfg_risk);
        let mut s_b = QuoteBoth { quoted: false };
        let risk = eng_b.run_collecting(evs().into_iter(), &mut s_b, &cfg_risk);

        let ja = serde_json::to_string(&base.report).unwrap();
        let jb = serde_json::to_string(&risk.report).unwrap();
        assert_eq!(ja, jb, "risk-default report must be byte-identical to the no-risk baseline");
        // And the new fields are at their inert defaults.
        assert!(!risk.report.summary.halted);
        assert!(risk.report.summary.halt_reason.is_empty());
        assert_eq!(risk.report.summary.risk_rejections, 0);
    }

    #[test]
    fn noop_does_not_trade() {
        let cfg = BacktestConfig::default();
        let eng = Engine::new(&cfg);
        let mut s = Noop::default();
        let evs = vec![
            delta(1_000_000_000, Side::Bid, 40, 100.0),
            delta(1_000_000_000, Side::Ask, 60, 100.0),
        ];
        let r = eng.run(evs.into_iter(), &mut s, &cfg);
        assert_eq!(r.summary.total_orders, 0);
        assert!((r.summary.ending_balance - 1000.0).abs() < 1e-9);
    }

    // ========================================================================
    // Order types / time-in-force tests (place_limit_ex)
    // ========================================================================

    /// A one-shot strategy that places a single `place_limit_ex` order on the first event, so we can
    /// exercise TIF / post-only against a pre-seeded book.
    struct LimitExOnce {
        placed: bool,
        side: Side,
        price: Cents,
        qty: f64,
        tif: Tif,
        post_only: bool,
    }
    impl Strategy for LimitExOnce {
        fn name(&self) -> &str {
            "limit_ex_once"
        }
        fn on_event(&mut self, _ev: &MarketEvent, ctx: &mut dyn Ctx) {
            if self.placed {
                return;
            }
            // Place only once the OPPOSING side exists, so marketability is well-defined at send
            // time (a BUY needs an ask to cross; a SELL needs a bid).
            let ready = match self.side {
                Side::Bid => ctx.best_ask("X").is_some(),
                Side::Ask => ctx.best_bid("X").is_some(),
            };
            if ready {
                ctx.place_limit_ex("X", self.side, self.price, self.qty, self.tif, self.post_only);
                self.placed = true;
            }
        }
    }

    /// post_only: a MARKETABLE buy limit (price >= best ask) is rejected in full and counted; nothing
    /// fills and nothing rests.
    #[test]
    fn post_only_marketable_is_rejected_and_counted() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = false;
        // book: best ask 60. A post-only BUY at 60 is marketable (60 >= 60) -> reject.
        let evs = vec![
            delta(1_000_000_000, Side::Bid, 40, 100.0),
            delta(1_000_000_000, Side::Ask, 60, 100.0),
        ];
        // Drive via step() so we can inspect resting orders directly (the engine is consumed by
        // run_collecting otherwise).
        let mut eng2 = Engine::new(&cfg);
        let mut s2 = LimitExOnce {
            placed: false,
            side: Side::Bid,
            price: Cents(60),
            qty: 10.0,
            tif: Tif::Gtc,
            post_only: true,
        };
        for ev in &evs {
            eng2.step(ev, &mut s2);
        }
        assert!(eng2.resting_orders().is_empty(), "post-only marketable must not rest");
        assert_eq!(eng2.portfolio().post_only_rejects, 1, "must count the rejection");
        assert_eq!(eng2.portfolio().total_fills, 0, "post-only marketable must not fill");
        assert!(eng2.portfolio().positions.get("X").map(|p| p.net_qty).unwrap_or(0.0).abs() < 1e-9);
    }

    /// post_only: a NON-marketable buy limit (price < best ask) rests fine and later fills as a maker.
    #[test]
    fn post_only_non_marketable_rests_and_fills_as_maker() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = false;
        // book: ask 60. A post-only BUY at 45 is NOT marketable -> rests. Then a trade at 45 fills it.
        let evs = vec![
            delta(1_000_000_000, Side::Bid, 40, 100.0),
            delta(1_000_000_000, Side::Ask, 60, 100.0),
            trade(1_500_000_000, 45, 10.0),
        ];
        let eng = Engine::new(&cfg);
        let mut s = LimitExOnce {
            placed: false,
            side: Side::Bid,
            price: Cents(45),
            qty: 10.0,
            tif: Tif::Gtc,
            post_only: true,
        };
        let out = eng.run_collecting(evs.into_iter(), &mut s, &cfg);
        assert_eq!(out.report.summary.post_only_rejects, 0, "non-marketable: no rejection");
        assert_eq!(out.report.summary.num_fills, 1, "should rest then fill as maker");
        assert_eq!(out.portfolio.fills[0].liquidity, Liquidity::Maker);
        assert_eq!(out.portfolio.fills[0].price, Cents(45));
    }

    /// IOC: the marketable portion fills immediately (taker, bounded by the limit price) and the
    /// remainder is cancelled — it never rests.
    #[test]
    fn ioc_fills_crossable_part_and_does_not_rest() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = false;
        // asks: 5 @ 60, 100 @ 62. An IOC BUY of 50 @ 60 may lift only the 60 level (5 contracts);
        // the 45 remaining is cancelled (the 62 ask is past the limit).
        let evs = vec![
            delta(1_000_000_000, Side::Ask, 60, 5.0),
            delta(1_000_000_000, Side::Ask, 62, 100.0),
        ];
        let mut eng = Engine::new(&cfg);
        let mut s = LimitExOnce {
            placed: false,
            side: Side::Bid,
            price: Cents(60),
            qty: 50.0,
            tif: Tif::Ioc,
            post_only: false,
        };
        for ev in &evs {
            eng.step(ev, &mut s);
        }
        let pf = eng.portfolio();
        assert_eq!(pf.total_fills, 1, "one taker fill of the crossable part");
        assert_eq!(pf.fills[0].liquidity, Liquidity::Taker);
        assert_eq!(pf.fills[0].qty, 5.0, "bounded by liquidity inside the limit price");
        assert_eq!(pf.fills[0].price, Cents(60));
        assert!((pf.positions["X"].net_qty - 5.0).abs() < 1e-9);
        // remainder cancelled -> nothing resting.
        assert!(eng.resting_orders().is_empty(), "IOC must not rest the remainder");
    }

    /// GTC marketable: the crossing portion takes (bounded by the limit price) and the remainder
    /// RESTS as a maker order.
    #[test]
    fn gtc_marketable_takes_then_rests_remainder() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = false;
        // ask: 5 @ 60 (the only liquidity inside the limit). A GTC BUY of 20 @ 60 lifts 5 as a taker,
        // then the remaining 15 RESTS as a bid at 60. A later trade at 60 fills the resting 15.
        let evs = vec![
            delta(1_000_000_000, Side::Bid, 40, 100.0),
            delta(1_000_000_000, Side::Ask, 60, 5.0),
            trade(1_500_000_000, 60, 15.0), // hits the resting bid at 60
        ];
        let eng = Engine::new(&cfg);
        let mut s = LimitExOnce {
            placed: false,
            side: Side::Bid,
            price: Cents(60),
            qty: 20.0,
            tif: Tif::Gtc,
            post_only: false,
        };
        let out = eng.run_collecting(evs.into_iter(), &mut s, &cfg);
        // fill 1: taker 5 @ 60. fill 2: maker 15 @ 60 from the later trade.
        assert_eq!(out.report.summary.num_fills, 2);
        assert_eq!(out.portfolio.fills[0].liquidity, Liquidity::Taker);
        assert_eq!(out.portfolio.fills[0].qty, 5.0);
        assert_eq!(out.portfolio.fills[1].liquidity, Liquidity::Maker);
        assert_eq!(out.portfolio.fills[1].qty, 15.0);
        // exactly one order was counted (a limit order is one order whether or not it splits).
        assert_eq!(out.report.summary.total_orders, 1);
    }

    /// Latency defers the marketable take: a GTC marketable limit sent at T with latency L does NOT
    /// take at T; it is judged at activation (T+L) against the FUTURE book.
    #[test]
    fn latency_defers_marketable_limit_take() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = false;
        cfg.execution.latency.enabled = true;
        cfg.execution.latency.order_latency_ns = 1_000_000_000; // 1s

        // First event (t=1s): ask 60, strategy sends a GTC BUY 10 @ 60 -> activation 2s. Between
        // 1s and 2s the 60 ask is replaced by a 70 ask, so at activation the order is NO LONGER
        // marketable (60 < 70) and simply rests -> no taker fill.
        let evs = vec![
            delta(1_000_000_000, Side::Ask, 60, 100.0),
            MarketEvent::Delta(BookDelta {
                ts_ns: 1_500_000_000,
                instrument: "X".into(),
                action: Action::Update,
                side: Side::Ask,
                price: Cents(60),
                size: 0.0,
                sequence: 50,
                is_snapshot: false,
            }),
            delta(1_600_000_000, Side::Ask, 70, 100.0),
            delta(2_500_000_000, Side::Bid, 55, 5.0), // >= activation: resolve here -> rests, no take
        ];
        let mut eng = Engine::new(&cfg);
        let mut s = LimitExOnce {
            placed: false,
            side: Side::Bid,
            price: Cents(60),
            qty: 10.0,
            tif: Tif::Gtc,
            post_only: false,
        };
        for ev in &evs {
            eng.step(ev, &mut s);
        }
        assert_eq!(eng.portfolio().total_fills, 0, "no take: marketability judged at activation");
        // it rested instead (one open order remains, non-marketable at activation).
        assert_eq!(eng.resting_orders().len(), 1, "non-marketable-at-activation order should rest");
    }

    // ========================================================================
    // Maker-queue model tests (pessimistic default vs optimistic)
    // ========================================================================

    /// A strategy that rests a single bid at a fixed price once, behind some queue.
    struct RestBidOnce {
        placed: bool,
        price: Cents,
        qty: f64,
    }
    impl Strategy for RestBidOnce {
        fn name(&self) -> &str {
            "rest_bid_once"
        }
        fn on_event(&mut self, _ev: &MarketEvent, ctx: &mut dyn Ctx) {
            if !self.placed {
                ctx.place_limit("X", Side::Bid, self.price, self.qty);
                self.placed = true;
            }
        }
    }

    /// Shared event stream: seed a bid at 45 with 100 already resting (so our order queues behind
    /// 100), then a cancellation shrinks that level to 30 (a 70-contract cancel ahead of us), then a
    /// trade of 50 prints at 45.
    fn queue_test_events() -> Vec<MarketEvent> {
        vec![
            delta(1_000_000_000, Side::Bid, 45, 100.0), // 100 ahead of us at 45
            delta(1_000_000_000, Side::Ask, 60, 100.0), // give the book a mid
            // (strategy rests its 10 @ 45 here on the first event; queue_ahead = 100)
            MarketEvent::Delta(BookDelta {
                ts_ns: 1_500_000_000,
                instrument: "X".into(),
                action: Action::Update,
                side: Side::Bid,
                price: Cents(45),
                size: 30.0, // shrink 100 -> 30: a 70-contract cancellation at our level
                sequence: 10,
                is_snapshot: false,
            }),
            trade(2_000_000_000, 45, 50.0), // 50 contracts trade at 45
        ]
    }

    /// PESSIMISTIC (default): the 70-contract cancellation does NOT improve our queue position, so a
    /// 50-contract trade only burns queue (100 -> 50) and we do NOT fill.
    #[test]
    fn pessimistic_queue_does_not_advance_on_cancel() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = false;
        assert_eq!(cfg.execution.queue.model, QueueModel::Pessimistic);
        let eng = Engine::new(&cfg);
        let mut s = RestBidOnce { placed: false, price: Cents(45), qty: 10.0 };
        let out = eng.run_collecting(queue_test_events().into_iter(), &mut s, &cfg);
        assert_eq!(out.report.summary.num_fills, 0, "pessimistic: trade only burns queue, no fill");
    }

    /// OPTIMISTIC: the 70-contract cancellation moves us up (queue_ahead 100 -> 30), so the same
    /// 50-contract trade burns the remaining 30 and fills our 10 (with 10 left of the trade).
    #[test]
    fn optimistic_queue_advances_on_cancel_and_fills_sooner() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = false;
        cfg.execution.queue.model = QueueModel::Optimistic;
        let eng = Engine::new(&cfg);
        let mut s = RestBidOnce { placed: false, price: Cents(45), qty: 10.0 };
        let out = eng.run_collecting(queue_test_events().into_iter(), &mut s, &cfg);
        assert_eq!(out.report.summary.num_fills, 1, "optimistic: cancel advances queue -> fill");
        assert_eq!(out.portfolio.fills[0].liquidity, Liquidity::Maker);
        assert_eq!(out.portfolio.fills[0].qty, 10.0);
    }

    /// Queue-model DEFAULT no-op guarantee: a pessimistic run is byte-for-byte identical to a run
    /// built before the queue field existed (same default config), on a fixed stream.
    #[test]
    fn queue_default_reproduces_report_byte_for_byte() {
        let evs = || {
            vec![
                delta(1_000_000_000, Side::Bid, 40, 100.0),
                delta(1_000_000_000, Side::Ask, 60, 100.0),
                trade(1_200_000_000, 60, 5.0),
                delta(1_300_000_000, Side::Bid, 48, 50.0),
                delta(1_300_000_000, Side::Ask, 52, 50.0),
                trade(1_400_000_000, 46, 10.0),
                buy_trade(2_000_000_000, 54, 8.0),
                delta(2_500_000_000, Side::Bid, 50, 30.0),
            ]
        };
        let cfg_a = BacktestConfig::default();
        let eng_a = Engine::new(&cfg_a);
        let mut s_a = QuoteBoth { quoted: false };
        let a = eng_a.run_collecting(evs().into_iter(), &mut s_a, &cfg_a);

        let mut cfg_b = BacktestConfig::default();
        cfg_b.execution.queue = crate::config::QueueConfig::default(); // explicit pessimistic
        let eng_b = Engine::new(&cfg_b);
        let mut s_b = QuoteBoth { quoted: false };
        let b = eng_b.run_collecting(evs().into_iter(), &mut s_b, &cfg_b);

        let ja = serde_json::to_string(&a.report).unwrap();
        let jb = serde_json::to_string(&b.report).unwrap();
        assert_eq!(ja, jb, "pessimistic (default) must reproduce the baseline byte-for-byte");
        assert_eq!(a.report.summary.post_only_rejects, 0);
    }

    // ========================================================================
    // Binary settlement-at-expiry tests
    // ========================================================================

    use crate::settlement::{Outcome, SettlementMap};

    /// A strategy that markets a single BUY (or SELL) once a two-sided book exists, then holds — so a
    /// position remains OPEN at end-of-run and is either settled or flattened.
    struct BuyAndHold {
        done: bool,
        side: Side,
        qty: f64,
    }
    impl Strategy for BuyAndHold {
        fn name(&self) -> &str {
            "buy_and_hold"
        }
        fn on_event(&mut self, _ev: &MarketEvent, ctx: &mut dyn Ctx) {
            if !self.done && ctx.best_bid("X").is_some() && ctx.best_ask("X").is_some() {
                ctx.place_market("X", self.side, self.qty);
                self.done = true;
            }
        }
    }

    /// (a) A LONG position held to expiry SETTLES to $1 on a YES resolution: cash/PnL correct, the
    /// settlement fill has fee 0, and the ending balance reflects the $1 payoff (not the mid).
    #[test]
    fn long_settles_to_one_on_yes() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = true; // settlement still takes precedence for known instruments
        cfg.execution.include_fees = false; // isolate settlement math from the opening fill's fee
        // Book with a low mid (~0.06) so we can prove settlement ($1) != flatten-at-mid (~0.06).
        let evs = vec![
            delta(1_000_000_000, Side::Bid, 5, 1000.0),
            delta(1_000_000_000, Side::Ask, 7, 1000.0),
            delta(2_000_000_000, Side::Bid, 5, 1000.0),
        ];
        let mut eng = Engine::new(&cfg);
        eng.set_settlement_map(SettlementMap::from_pairs([("X", Outcome::Yes)]));
        let mut s = BuyAndHold {
            done: false,
            side: Side::Bid,
            qty: 100.0,
        };
        let out = eng.run_collecting(evs.into_iter(), &mut s, &cfg);
        // bought 100 @ 0.07 -> cash 1000 - 7 = 993, long 100 @ 0.07.
        // settle YES: credit 100*$1 = 100 -> cash 1093; realized settle pnl = 100*(1-0.07) = 93.
        assert!((out.report.summary.settled_pnl - 93.0).abs() < 1e-9, "settled_pnl {}", out.report.summary.settled_pnl);
        assert_eq!(out.report.summary.num_settled, 1);
        assert!((out.portfolio.cash - 1093.0).abs() < 1e-9, "cash {}", out.portfolio.cash);
        // ending balance ~1093 (payoff), NOT ~999 (flatten at mid 0.06).
        assert!((out.report.summary.ending_balance - 1093.0).abs() < 1e-6,
            "ending balance should reflect $1 payoff, got {}", out.report.summary.ending_balance);
        // position flat, settlement fill has fee 0 and is a Settle liquidity.
        assert!(out.portfolio.positions["X"].net_qty.abs() < 1e-9);
        let last = out.portfolio.fills.last().unwrap();
        assert_eq!(last.liquidity, Liquidity::Settle);
        assert_eq!(last.fee, 0.0);
        assert_eq!(out.report.summary.total_fees, 0.0, "settlement charges no fee");
    }

    /// (b) A SHORT position held to expiry SETTLES to $0 on a NO resolution, with the right sign.
    #[test]
    fn short_settles_to_zero_on_no() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = true;
        cfg.execution.include_fees = false; // isolate settlement math from the opening fill's fee
        // High mid (~0.94) so flatten-at-mid would be very different from the $0 settlement.
        let evs = vec![
            delta(1_000_000_000, Side::Bid, 93, 1000.0),
            delta(1_000_000_000, Side::Ask, 95, 1000.0),
            delta(2_000_000_000, Side::Bid, 93, 1000.0),
        ];
        let mut eng = Engine::new(&cfg);
        eng.set_settlement_map(SettlementMap::from_pairs([("X", Outcome::No)]));
        // SELL 100 YES to open a short, then hold.
        let mut s = BuyAndHold {
            done: false,
            side: Side::Ask,
            qty: 100.0,
        };
        let out = eng.run_collecting(evs.into_iter(), &mut s, &cfg);
        // sold 100 @ 0.93 -> cash 1000 + 93 = 1093, short -100 @ 0.93.
        // settle NO ($0): buy back 100 @ $0 costs 0 -> cash stays 1093; settle pnl = (-100)*(0-0.93)=93.
        assert!((out.report.summary.settled_pnl - 93.0).abs() < 1e-9, "settled_pnl {}", out.report.summary.settled_pnl);
        assert_eq!(out.report.summary.num_settled, 1);
        assert!((out.portfolio.cash - 1093.0).abs() < 1e-9, "cash {}", out.portfolio.cash);
        assert!((out.report.summary.ending_balance - 1093.0).abs() < 1e-6, "ending {}", out.report.summary.ending_balance);
        assert!(out.portfolio.positions["X"].net_qty.abs() < 1e-9);
        let last = out.portfolio.fills.last().unwrap();
        assert_eq!(last.liquidity, Liquidity::Settle);
        assert_eq!(last.price, Cents(0));
    }

    /// (c) An UNKNOWN instrument (not in the settlement map) still FLATTENS at mid — settlement only
    /// applies to known outcomes, everything else keeps the original behaviour.
    #[test]
    fn unknown_instrument_still_flattens_at_mid() {
        let mut cfg = BacktestConfig::default();
        cfg.flatten_at_end = true;
        let evs = vec![
            delta(1_000_000_000, Side::Bid, 40, 1000.0),
            delta(1_000_000_000, Side::Ask, 60, 1000.0),
            delta(2_000_000_000, Side::Bid, 40, 1000.0),
        ];
        let mut eng = Engine::new(&cfg);
        // Map covers a DIFFERENT instrument, so "X" is Unknown.
        eng.set_settlement_map(SettlementMap::from_pairs([("OTHER", Outcome::Yes)]));
        let mut s = BuyAndHold {
            done: false,
            side: Side::Bid,
            qty: 100.0,
        };
        let out = eng.run_collecting(evs.into_iter(), &mut s, &cfg);
        // nothing settled; X was flattened at mid via a market SELL (Taker), so the last fill is Taker.
        assert_eq!(out.report.summary.num_settled, 0, "unknown instrument must not settle");
        assert!((out.report.summary.settled_pnl).abs() < 1e-12);
        assert!(out.portfolio.positions["X"].net_qty.abs() < 1e-9, "flattened to zero");
        let last = out.portfolio.fills.last().unwrap();
        assert_ne!(last.liquidity, Liquidity::Settle, "should be a flatten (Taker), not a Settle");
    }

    /// (e) THE NO-OP GUARANTEE: with NO settlement map, the report is BYTE-FOR-BYTE identical to a
    /// plain default run — settlement adds nothing when no outcomes are provided.
    #[test]
    fn no_settlements_reproduce_report_byte_for_byte() {
        let evs = || {
            vec![
                delta(1_000_000_000, Side::Bid, 40, 100.0),
                delta(1_000_000_000, Side::Ask, 60, 100.0),
                trade(1_200_000_000, 60, 5.0),
                delta(1_300_000_000, Side::Bid, 48, 50.0),
                delta(1_300_000_000, Side::Ask, 52, 50.0),
                trade(1_400_000_000, 46, 10.0),
                buy_trade(2_000_000_000, 54, 8.0),
                delta(2_500_000_000, Side::Bid, 50, 30.0),
            ]
        };
        // Baseline: plain default config (no settlement path).
        let cfg = BacktestConfig::default();
        let eng_a = Engine::new(&cfg);
        let mut s_a = QuoteBoth { quoted: false };
        let base = eng_a.run_collecting(evs().into_iter(), &mut s_a, &cfg);

        // Same config; engine with an explicitly-EMPTY settlement map (== no file provided).
        let mut eng_b = Engine::new(&cfg);
        eng_b.set_settlement_map(SettlementMap::new());
        let mut s_b = QuoteBoth { quoted: false };
        let empty = eng_b.run_collecting(evs().into_iter(), &mut s_b, &cfg);

        let ja = serde_json::to_string(&base.report).unwrap();
        let jb = serde_json::to_string(&empty.report).unwrap();
        assert_eq!(ja, jb, "no-settlement report must be byte-identical to the baseline");
        // new fields are inert.
        assert_eq!(base.report.summary.num_settled, 0);
        assert!((base.report.summary.settled_pnl).abs() < 1e-12);
    }

    // ========================================================================
    // FIX 1 — multi-instrument determinism
    // ========================================================================

    /// A strategy that opens a long position in EVERY instrument it sees (one market buy each), so
    /// the portfolio holds ≥2 simultaneous open positions whose marks are SUMMED into every equity
    /// snapshot. This is exactly the case that, with a `HashMap` positions map, would sum in a
    /// process-randomized order and perturb the equity bits run-to-run.
    struct BuyEachInstrument {
        bought: std::collections::BTreeSet<String>,
    }
    impl Strategy for BuyEachInstrument {
        fn name(&self) -> &str {
            "buy_each"
        }
        fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
            let inst = ev.instrument().to_string();
            // buy once per instrument, only when its book has an ask to lift.
            if !self.bought.contains(&inst) && ctx.best_ask(&inst).is_some() {
                ctx.place_market(&inst, Side::Bid, 7.0);
                self.bought.insert(inst);
            }
        }
    }

    /// Build a deterministic multi-instrument event stream: two instruments ("AAA" and "BBB") each
    /// get a two-sided book and a couple of book updates, interleaved in time, so both carry an open
    /// position that is marked into the same equity snapshots.
    fn multi_instrument_stream() -> Vec<MarketEvent> {
        let d = |ts: i64, inst: &str, side: Side, price: i32, size: f64| {
            MarketEvent::Delta(BookDelta {
                ts_ns: ts,
                instrument: inst.into(),
                action: Action::Add,
                side,
                price: Cents(price),
                size,
                sequence: ts,
                is_snapshot: false,
            })
        };
        vec![
            d(1_000_000_000, "AAA", Side::Bid, 40, 100.0),
            d(1_000_000_000, "AAA", Side::Ask, 60, 100.0),
            d(1_000_000_000, "BBB", Side::Bid, 30, 100.0),
            d(1_000_000_000, "BBB", Side::Ask, 70, 100.0),
            // let the strategy buy on the next events (after books exist)
            d(2_000_000_000, "AAA", Side::Bid, 41, 50.0),
            d(2_000_000_000, "BBB", Side::Bid, 31, 50.0),
            d(3_000_000_000, "AAA", Side::Ask, 59, 50.0),
            d(3_000_000_000, "BBB", Side::Ask, 69, 50.0),
            d(4_000_000_000, "AAA", Side::Bid, 42, 25.0),
            d(4_000_000_000, "BBB", Side::Ask, 68, 25.0),
        ]
    }

    /// FIX 1: a run holding ≥2 instruments simultaneously must produce a BYTE-FOR-BYTE identical
    /// `report.json` across two independent runs. With the old `HashMap` positions map the equity
    /// Σ-over-positions ran in a per-process-randomized order, so the last float bits of every
    /// snapshot (and hence Sharpe / drawdown / ranking) could differ run-to-run. The `BTreeMap`
    /// makes the summation order a fixed function of the instrument ids. (The pre-existing
    /// determinism tests only quote ONE instrument, so they could not catch this.)
    #[test]
    fn multi_instrument_run_is_byte_for_byte_deterministic() {
        let cfg = BacktestConfig::default();

        let run = || {
            let eng = Engine::new(&cfg);
            let mut s = BuyEachInstrument {
                bought: std::collections::BTreeSet::new(),
            };
            let out = eng.run_collecting(multi_instrument_stream().into_iter(), &mut s, &cfg);
            out
        };

        let a = run();
        let b = run();

        // Both instruments must actually carry a position so the equity Σ is non-trivial.
        assert!(a.report.summary.total_positions >= 2, "expected ≥2 instruments held");

        let ja = serde_json::to_string(&a.report).unwrap();
        let jb = serde_json::to_string(&b.report).unwrap();
        assert_eq!(
            ja, jb,
            "multi-instrument report must be byte-for-byte identical across runs"
        );

        // Sorted-sum reference: the deterministic (BTreeMap) iteration order IS the sorted-key
        // order, so the equity sum-over-positions is the sorted-key sum. Confirm the position keys
        // iterate in sorted order — the property the equity sum relies on for run-to-run stability.
        let keys: Vec<String> = a.portfolio.positions.keys().cloned().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "positions must iterate in sorted (deterministic) key order");
        assert!(keys.len() >= 2, "expected at least 2 instruments in the positions map");
    }
}
