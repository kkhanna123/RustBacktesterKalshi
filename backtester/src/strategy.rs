//! Strategy interface. Strategies are pure functions of the event stream: they observe `MarketEvent`s
//! and act through a `Ctx` (provided by the engine). This decouples strategies from engine internals.

use crate::types::{Cents, MarketEvent, OrderView, Side, Tif};

/// The execution/market context the engine hands to a strategy on every event.
/// The engine implements this trait; strategies only see the interface.
pub trait Ctx {
    /// Event timestamp (ns) currently being processed.
    fn ts_ns(&self) -> i64;

    /// Best bid (price, size) for an instrument, if the book has one.
    fn best_bid(&self, instrument: &str) -> Option<(Cents, f64)>;
    /// Best ask (price, size) for an instrument, if the book has one.
    fn best_ask(&self, instrument: &str) -> Option<(Cents, f64)>;
    /// Mid in dollars, if both sides exist.
    fn mid(&self, instrument: &str) -> Option<f64>;

    /// Top-of-book size imbalance in [-1, 1]: `(bid_sz - ask_sz)/(bid_sz + ask_sz)`.
    /// Default derives it from `best_bid`/`best_ask`; the engine may override for efficiency.
    fn imbalance(&self, instrument: &str) -> Option<f64> {
        match (self.best_bid(instrument), self.best_ask(instrument)) {
            (Some((_, bs)), Some((_, az))) if bs + az > 0.0 => Some((bs - az) / (bs + az)),
            _ => None,
        }
    }

    /// Size-weighted microprice in dollars (leans toward the heavier side). Default derives it
    /// from `best_bid`/`best_ask`.
    fn microprice(&self, instrument: &str) -> Option<f64> {
        match (self.best_bid(instrument), self.best_ask(instrument)) {
            (Some((b, bs)), Some((a, az))) if bs + az > 0.0 => {
                Some((b.to_dollars() * az + a.to_dollars() * bs) / (bs + az))
            }
            _ => None,
        }
    }

    // ---- LOGISTIC STRIKE-CURVE features (default `None`; the engine overrides) ----
    //
    // For a ladder of "above $K" binaries on one settlement event, the engine fits a logistic
    // survival `S(K) = 1/(1+exp((K-mu)/s))` across the strikes' mids (see `crate::fit_logistic`).
    // These accessors expose that fit per strike instrument, mirroring the `imbalance`/`microprice`
    // pattern: the default returns `None` (so strategies that ignore them are unaffected), and the
    // engine implements them by parsing the instrument, lazily fitting its event, and evaluating at
    // the strike. A `None` means "no trustworthy fit" (non-ladder instrument, too few strikes, or a
    // degenerate ladder) — strategies should treat that as "don't trade".

    /// The event's IMPLIED FAIR VALUE: the logistic median `mu` (the strike at which `S = 0.5`).
    /// In dollars of the settlement underlying. `None` if the event can't be fitted.
    fn implied_fair_value(&self, _instrument: &str) -> Option<f64> {
        None
    }

    /// The event's IMPLIED VOLATILITY: the logistic standard deviation `s·π/√3`, a `$`-dispersion
    /// of the implied settlement distribution (NOT annualized). `None` if the event can't be fitted.
    fn implied_vol(&self, _instrument: &str) -> Option<f64> {
        None
    }

    /// The fitted curve price `S(K)` for THIS strike instrument — the ladder's "fair" value for the
    /// "above $K" binary. `None` if the event can't be fitted or the id isn't a ladder strike.
    fn fitted_price(&self, _instrument: &str) -> Option<f64> {
        None
    }

    /// FIT EDGE at this strike: `market_mid − fitted_price`. `> 0` ⇒ the market is RICH (overpriced)
    /// vs the curve (sell edge); `< 0` ⇒ CHEAP (buy edge). `None` if there's no mid or no fit.
    fn fit_edge(&self, _instrument: &str) -> Option<f64> {
        None
    }

    /// FIT QUALITY: the fit's RMSE over the ladder (lower = better). A strategy filters on this so it
    /// only trades when the curve actually explains the ladder. `None` if the event can't be fitted.
    fn fit_quality(&self, _instrument: &str) -> Option<f64> {
        None
    }

    /// Net signed position (contracts; + long YES, - short YES) for an instrument.
    fn position(&self, instrument: &str) -> f64;
    /// Free cash in account currency.
    fn cash(&self) -> f64;

    /// All instruments the engine currently has a book for (canonical `"VENUE:symbol"` ids).
    /// Enables cross-venue strategies to discover what's trading without hard-coding ids. The
    /// default returns empty (sufficient for single-instrument strategies); the engine overrides it.
    fn instruments(&self) -> Vec<String> {
        Vec::new()
    }

    /// Convenience: the subset of [`Ctx::instruments`] whose venue tag matches `venue`
    /// (case-insensitive), e.g. `ctx.instruments_for_venue("POLYMARKET")`. Built on top of
    /// [`Ctx::instruments`], so cross-venue strategies get it for free.
    fn instruments_for_venue(&self, venue: &str) -> Vec<String> {
        let want = venue.to_uppercase();
        self.instruments()
            .into_iter()
            .filter(|id| {
                id.split_once(':')
                    .map(|(v, _)| v.eq_ignore_ascii_case(&want))
                    // a bare (untagged) id is treated as Kalshi (matches Instrument::parse)
                    .unwrap_or_else(|| want == "KALSHI")
            })
            .collect()
    }

    /// Currently-resting strategy orders for an instrument.
    fn open_orders(&self, instrument: &str) -> Vec<OrderView>;

    /// Place a plain resting limit order (GTC, not post-only). Filled by the execution model as the
    /// book trades through it. This is exactly `place_limit_ex(.., Tif::Gtc, post_only=false)` and is
    /// kept as a stable convenience so existing strategies are byte-for-byte unchanged.
    fn place_limit(&mut self, instrument: &str, side: Side, price: Cents, qty: f64) {
        self.place_limit_ex(instrument, side, price, qty, Tif::Gtc, false);
    }

    /// Place a limit order with an explicit time-in-force and post-only flag — the full limit-order
    /// API. Marketability is judged at the order's **activation time** (send + latency), against the
    /// book as of then. A BUY limit at price `P` is *marketable* if `P >= best ask`; a SELL limit at
    /// `P` is marketable if `P <= best bid`.
    ///
    /// Behaviour by mode (every take is bounded by the limit price and never fills past it; the
    /// crossing take reuses the same latency-deferred taker machinery as `place_market`):
    /// * `tif = Tif::Gtc`, `post_only = false` — the default limit. If marketable, the crossing
    ///   portion takes immediately (taker, bounded by `price`) and any remainder RESTS as a maker
    ///   order; if not marketable, the whole order just rests.
    /// * `tif = Tif::Ioc` — only the marketable portion fills (taker, bounded by `price`); the
    ///   unfilled remainder is CANCELLED and never rests. If nothing is marketable, nothing happens.
    /// * `post_only = true` — guarantees maker-only. If the order is marketable at activation it is
    ///   REJECTED in full (counted in `summary.post_only_rejects`) rather than allowed to cross; if
    ///   not marketable it rests normally. (`post_only` is incompatible with taking, so it overrides
    ///   the take behaviour of both GTC and IOC.)
    ///
    /// # Example
    /// ```ignore
    /// // Post-only maker bid — never crosses; rejected (and counted) if it would take.
    /// ctx.place_limit_ex(inst, Side::Bid, Cents(42), 10.0, Tif::Gtc, true);
    /// ```
    fn place_limit_ex(
        &mut self,
        instrument: &str,
        side: Side,
        price: Cents,
        qty: f64,
        tif: Tif,
        post_only: bool,
    );

    /// Place an immediate market order that walks the opposing book (taker).
    fn place_market(&mut self, instrument: &str, side: Side, qty: f64);
    /// Cancel a resting order by id.
    fn cancel(&mut self, order_id: u64);
}

/// A backtest strategy.
pub trait Strategy {
    fn name(&self) -> &str;
    /// Called for every market event in time order.
    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx);
    /// Called once at end-of-data (e.g. to flatten). Default no-op.
    fn on_finish(&mut self, _ctx: &mut dyn Ctx) {}
}
