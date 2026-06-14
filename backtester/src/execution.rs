//! Matching model: how resting strategy limit orders and immediate market orders fill.
//!
//! ## Resting limit orders (maker)
//! A resting order carries a `queue_ahead`: the number of contracts already resting at its price
//! level when it was placed (price-time priority approximation, FIFO). The order only starts
//! filling after `queue_ahead` contracts have traded at/through its level — this is the FIFO
//! queue burn-down model (a defensible approximation: we assume the trade consumes the queue
//! ahead of us in arrival order before reaching us).
//!
//! ### Aggressor gate (microstructure correctness)
//! A resting (maker) order only fills when the incoming trade's AGGRESSOR traded AGAINST it — you
//! cannot be filled by a print on your own side. The book is YES-native and `TradeEvent.aggressor_yes`
//! tells us which side lifted/hit:
//! - `aggressor_yes == true`  => the aggressor BOUGHT YES (lifted asks), so only resting **Asks**
//!   (we are selling YES) can be filled by this print;
//! - `aggressor_yes == false` => the aggressor SOLD YES (hit bids), so only resting **Bids**
//!   (we are buying YES) can be filled by this print.
//!
//! Combined with the price-cross condition, a resting order fills only when BOTH hold:
//! - a resting **Bid** at P fills iff `aggressor_yes == false` (a seller crossed the bid) AND the
//!   trade price ≤ P;
//! - a resting **Ask** at P fills iff `aggressor_yes == true` (a buyer lifted the ask) AND the
//!   trade price ≥ P.
//!
//! Available trade size first burns down `queue_ahead` (FIFO), then fills `min(remaining, leftover)`
//! as a MAKER fill (maker fee).
//!
//! ## Market orders (taker)
//! Walk the opposing book levels immediately, consuming size level-by-level, paying taker fees on
//! each slice. Deterministic and book-state-driven.

use crate::fees::FeeModel;
use crate::orderbook::OrderBook;
use crate::types::{Cents, Fill, Liquidity, Side, TradeEvent};

/// A resting strategy order tracked by the engine.
#[derive(Debug, Clone)]
pub struct RestingOrder {
    pub id: u64,
    pub instrument: String,
    pub side: Side,
    pub price: Cents,
    pub remaining: f64,
    /// Contracts ahead in queue still to be consumed before this order fills.
    pub queue_ahead: f64,
    /// Timestamp (ns) at/after which this order is matchable (placement ts + latency). Under the
    /// zero-latency default this equals the placement ts, so the order is immediately eligible.
    pub activation_ts: i64,
}

impl RestingOrder {
    /// Compute the queue depth at `price` on the side this order will rest on.
    /// A BUY (bid) rests on the bid side; a SELL (ask) rests on the ask side.
    pub fn queue_at_placement(book: &OrderBook, side: Side, price: Cents) -> f64 {
        let map = match side {
            Side::Bid => &book.bids,
            Side::Ask => &book.asks,
        };
        map.get(&price).copied().unwrap_or(0.0)
    }
}

/// Try to fill a resting order against an incoming trade. Mutates `order.remaining`/`queue_ahead`
/// and returns a [`Fill`] if any contracts filled.
pub fn match_resting_against_trade(
    order: &mut RestingOrder,
    trade: &TradeEvent,
    fees: &FeeModel,
) -> Option<Fill> {
    if order.remaining <= 0.0 || trade.instrument != order.instrument {
        return None;
    }
    // Aggressor gate + price cross. A resting maker order fills only when the trade's aggressor
    // traded AGAINST it (you can't be filled by a print on your own side), AND the trade price
    // crosses our limit. The book is YES-native, so `aggressor_yes == true` means a buyer lifted
    // asks (fills resting Asks) and `aggressor_yes == false` means a seller hit bids (fills resting
    // Bids). See the module-level "Aggressor gate" doc.
    let fills = match order.side {
        // A resting BUY (we buy YES) is on the bid: only a SELLING aggressor crossing down to our
        // price (trade price <= our price) can fill us.
        Side::Bid => !trade.aggressor_yes && trade.price.0 <= order.price.0,
        // A resting SELL (we sell YES) is on the ask: only a BUYING aggressor lifting up to our
        // price (trade price >= our price) can fill us.
        Side::Ask => trade.aggressor_yes && trade.price.0 >= order.price.0,
    };
    if !fills {
        return None;
    }

    let mut avail = trade.size;
    // Burn down the queue ahead of us first.
    if order.queue_ahead > 0.0 {
        let consumed = order.queue_ahead.min(avail);
        order.queue_ahead -= consumed;
        avail -= consumed;
    }
    if avail <= 0.0 {
        return None;
    }
    let qty = order.remaining.min(avail);
    if qty <= 0.0 {
        return None;
    }
    order.remaining -= qty;

    let price_d = order.price.to_dollars();
    let fee = fees.maker_fee(price_d, qty);
    Some(Fill {
        ts_ns: trade.ts_ns,
        order_id: order.id,
        instrument: order.instrument.clone(),
        side: order.side,
        price: order.price,
        qty,
        liquidity: Liquidity::Maker,
        fee,
    })
}

/// Execute a market order immediately by walking the opposing side of `book`. Returns the fills
/// produced (one per book level consumed). Does not mutate the book (the engine owns book state;
/// a market taker against a synthetic book consumes ephemeral liquidity).
pub fn execute_market(
    order_id: u64,
    instrument: &str,
    side: Side,
    qty: f64,
    book: &OrderBook,
    fees: &FeeModel,
    ts_ns: i64,
) -> Vec<Fill> {
    // An unbounded market order is the bounded walk with no price limit.
    execute_market_bounded(order_id, instrument, side, qty, None, book, fees, ts_ns)
}

/// Execute a taker order against the opposing side of `book`, optionally bounded by a limit price.
///
/// `price_bound` is the worst price the taker will accept (the limit price of a marketable limit
/// order): a BUY taker will not lift any ask priced **above** the bound, and a SELL taker will not
/// hit any bid priced **below** the bound. The walk stops at the first level past the bound,
/// producing a correct *partial* fill when liquidity inside the bound is thin. `price_bound = None`
/// is a plain unbounded market order (walks every level until `qty` is exhausted).
///
/// Returns the fills produced (one per level consumed); like [`execute_market`] it does not mutate
/// the book.
pub fn execute_market_bounded(
    order_id: u64,
    instrument: &str,
    side: Side,
    mut qty: f64,
    price_bound: Option<Cents>,
    book: &OrderBook,
    fees: &FeeModel,
    ts_ns: i64,
) -> Vec<Fill> {
    let mut fills = Vec::new();
    if qty <= 0.0 {
        return fills;
    }
    // A BUY taker walks the asks (low→high); a SELL taker walks the bids (high→low).
    match side {
        Side::Bid => {
            for (&px, &avail) in book.asks.iter() {
                if qty <= 0.0 {
                    break;
                }
                // bounded BUY: never lift an ask priced ABOVE our limit (stop the walk here).
                if let Some(bound) = price_bound {
                    if px.0 > bound.0 {
                        break;
                    }
                }
                let take = qty.min(avail);
                if take <= 0.0 {
                    continue;
                }
                let fee = fees.taker_fee(px.to_dollars(), take);
                fills.push(Fill {
                    ts_ns,
                    order_id,
                    instrument: instrument.to_string(),
                    side,
                    price: px,
                    qty: take,
                    liquidity: Liquidity::Taker,
                    fee,
                });
                qty -= take;
            }
        }
        Side::Ask => {
            for (&px, &avail) in book.bids.iter().rev() {
                if qty <= 0.0 {
                    break;
                }
                // bounded SELL: never hit a bid priced BELOW our limit (stop the walk here).
                if let Some(bound) = price_bound {
                    if px.0 < bound.0 {
                        break;
                    }
                }
                let take = qty.min(avail);
                if take <= 0.0 {
                    continue;
                }
                let fee = fees.taker_fee(px.to_dollars(), take);
                fills.push(Fill {
                    ts_ns,
                    order_id,
                    instrument: instrument.to_string(),
                    side,
                    price: px,
                    qty: take,
                    liquidity: Liquidity::Taker,
                    fee,
                });
                qty -= take;
            }
        }
    }
    fills
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BacktestConfig;
    use crate::types::Action;

    fn fees() -> FeeModel {
        FeeModel::from_config(&BacktestConfig::default())
    }

    /// Build a trade with an explicit aggressor direction. `aggressor_yes == false` is a SELLING
    /// aggressor (hits bids) — the direction that fills a resting BUY; `true` is a BUYING aggressor
    /// (lifts asks) — the direction that fills a resting SELL.
    fn trade_aggr(price: i32, size: f64, aggressor_yes: bool) -> TradeEvent {
        TradeEvent {
            ts_ns: 100,
            instrument: "X".to_string(),
            aggressor_yes,
            price: Cents(price),
            size,
            trade_id: "t".to_string(),
        }
    }

    /// A SELLING-aggressor trade (hits bids) — fills resting BUY orders.
    fn sell_trade(price: i32, size: f64) -> TradeEvent {
        trade_aggr(price, size, false)
    }

    /// A BUYING-aggressor trade (lifts asks) — fills resting SELL orders.
    fn buy_trade(price: i32, size: f64) -> TradeEvent {
        trade_aggr(price, size, true)
    }

    #[test]
    fn buy_limit_fills_when_trade_crosses() {
        let mut o = RestingOrder {
            id: 1,
            instrument: "X".to_string(),
            side: Side::Bid,
            price: Cents(40),
            remaining: 10.0,
            queue_ahead: 0.0,
            activation_ts: 0,
        };
        // a SELLING aggressor at 40 (<= 40) crosses our bid; 5 contracts trade.
        let f = match_resting_against_trade(&mut o, &sell_trade(40, 5.0), &fees()).unwrap();
        assert_eq!(f.qty, 5.0);
        assert_eq!(f.liquidity, Liquidity::Maker);
        assert_eq!(o.remaining, 5.0);
    }

    #[test]
    fn sell_limit_fills_when_buyer_lifts() {
        // A resting ASK (selling YES) fills only on a BUYING aggressor at/above our price.
        let mut o = RestingOrder {
            id: 2,
            instrument: "X".to_string(),
            side: Side::Ask,
            price: Cents(60),
            remaining: 10.0,
            queue_ahead: 0.0,
            activation_ts: 0,
        };
        let f = match_resting_against_trade(&mut o, &buy_trade(60, 4.0), &fees()).unwrap();
        assert_eq!(f.qty, 4.0);
        assert_eq!(f.liquidity, Liquidity::Maker);
        assert_eq!(o.remaining, 6.0);
    }

    #[test]
    fn same_direction_aggressor_does_not_fill() {
        // AGGRESSOR GATE: a same-direction print at the resting price must NOT fill the maker.
        // A resting BUY (bid) is NOT filled by a BUYING aggressor at our price.
        let mut bid = RestingOrder {
            id: 1,
            instrument: "X".to_string(),
            side: Side::Bid,
            price: Cents(40),
            remaining: 10.0,
            queue_ahead: 0.0,
            activation_ts: 0,
        };
        assert!(match_resting_against_trade(&mut bid, &buy_trade(40, 5.0), &fees()).is_none());
        assert_eq!(bid.remaining, 10.0); // untouched

        // A resting SELL (ask) is NOT filled by a SELLING aggressor at our price.
        let mut ask = RestingOrder {
            id: 2,
            instrument: "X".to_string(),
            side: Side::Ask,
            price: Cents(60),
            remaining: 10.0,
            queue_ahead: 0.0,
            activation_ts: 0,
        };
        assert!(match_resting_against_trade(&mut ask, &sell_trade(60, 5.0), &fees()).is_none());
        assert_eq!(ask.remaining, 10.0); // untouched
    }

    #[test]
    fn queue_ahead_blocks_then_fills() {
        let mut o = RestingOrder {
            id: 1,
            instrument: "X".to_string(),
            side: Side::Bid,
            price: Cents(40),
            remaining: 10.0,
            queue_ahead: 8.0,
            activation_ts: 0,
        };
        // 5 (selling aggressor) trade -> all burn queue, no fill
        assert!(match_resting_against_trade(&mut o, &sell_trade(40, 5.0), &fees()).is_none());
        assert_eq!(o.queue_ahead, 3.0);
        // 10 trade -> 3 burn queue, 7 fill
        let f = match_resting_against_trade(&mut o, &sell_trade(40, 10.0), &fees()).unwrap();
        assert_eq!(f.qty, 7.0);
        assert_eq!(o.remaining, 3.0);
    }

    #[test]
    fn no_fill_when_not_crossing() {
        let mut o = RestingOrder {
            id: 1,
            instrument: "X".to_string(),
            side: Side::Bid,
            price: Cents(40),
            remaining: 10.0,
            queue_ahead: 0.0,
            activation_ts: 0,
        };
        // a selling aggressor above our bid (45 > 40) does not cross our buy
        assert!(match_resting_against_trade(&mut o, &sell_trade(45, 5.0), &fees()).is_none());
    }

    #[test]
    fn market_walks_book() {
        let mut book = OrderBook::new();
        book.apply(&BookDelta_(Action::Add, Side::Ask, 60, 5.0));
        book.apply(&BookDelta_(Action::Add, Side::Ask, 62, 100.0));
        let fills = execute_market(9, "X", Side::Bid, 8.0, &book, &fees(), 1);
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0].price, Cents(60));
        assert_eq!(fills[0].qty, 5.0);
        assert_eq!(fills[1].price, Cents(62));
        assert_eq!(fills[1].qty, 3.0);
        assert!(fills[0].liquidity == Liquidity::Taker);
    }

    #[test]
    fn bounded_buy_walk_stops_at_limit_price() {
        // asks at 60 (5) and 62 (100). A BUY bounded at 60 may only lift the 60 level.
        let mut book = OrderBook::new();
        book.apply(&BookDelta_(Action::Add, Side::Ask, 60, 5.0));
        book.apply(&BookDelta_(Action::Add, Side::Ask, 62, 100.0));
        let fills =
            execute_market_bounded(9, "X", Side::Bid, 50.0, Some(Cents(60)), &book, &fees(), 1);
        // only the 60 ask is inside the bound -> a single partial fill of 5.
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].price, Cents(60));
        assert_eq!(fills[0].qty, 5.0);
    }

    #[test]
    fn bounded_sell_walk_stops_at_limit_price() {
        // bids at 40 (5) and 38 (100). A SELL bounded at 40 may only hit the 40 level.
        let mut book = OrderBook::new();
        book.apply(&BookDelta_(Action::Add, Side::Bid, 40, 5.0));
        book.apply(&BookDelta_(Action::Add, Side::Bid, 38, 100.0));
        let fills =
            execute_market_bounded(9, "X", Side::Ask, 50.0, Some(Cents(40)), &book, &fees(), 1);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].price, Cents(40));
        assert_eq!(fills[0].qty, 5.0);
    }

    // tiny helper to build a delta in tests
    #[allow(non_snake_case)]
    fn BookDelta_(action: Action, side: Side, price: i32, size: f64) -> crate::types::BookDelta {
        crate::types::BookDelta {
            ts_ns: 1,
            instrument: "X".to_string(),
            action,
            side,
            price: Cents(price),
            size,
            sequence: 1,
            is_snapshot: false,
        }
    }
}
