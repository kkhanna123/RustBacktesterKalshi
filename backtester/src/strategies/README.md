# strategies/ — where every strategy lives + how to write a new one

**All strategies live in this folder**, one `.rs` file each. They compile into the `kalshi-backtest` binary and
are selectable by name with `--strategy <name>`. This README is the quick guide; the full trait/API contract is
in [`../../../strategyFormat.md`](../../../strategyFormat.md).

## What's here

| File | `--strategy` name | Idea |
|---|---|---|
| `noop.rs` | `noop` | Trades nothing (baseline). |
| `momentum.rs` | `momentum` | Long when the mid rises N ticks; flatten on reversal. |
| `mean_reversion.rs` | `mean_reversion` | Buy a very negative mid z-score; flatten on revert. |
| `market_maker.rs` | `market_maker` | Quote bid+ask around mid with a half-spread + inventory skew. |
| `queue_probe.rs` | `queue_probe` | JOIN the touch (rest at best bid+ask, behind existing depth) — demonstrates `--queue-model`. |
| `avellaneda_stoikov.rs` | `avellaneda_stoikov` | Inventory-aware optimal MM (reservation price + optimal spread). |
| `imbalance.rs` | `imbalance` | Trade the top-of-book imbalance / microprice lean. |
| `breakout.rs` | `breakout` | Long an N-window range breakout; exit on revert/stop. |
| `cross_venue_arb.rs` | `cross_venue_arb` | Buy the cheaper venue, sell the richer; flatten on convergence. |
| `template.rs` | `template` | **Copy this** — minimal z-score example to start from. |
| `toolkit.rs` | — | Reusable building blocks (not a strategy). |
| `mod.rs` | — | The registry: `build()`, `ALL`, `INFO`. |

See each one's defaults with `kalshi-backtest list-strategies`. Tune any param at runtime with
`--strategy-param key=value` (an omitted key uses the default).

## Add a new strategy — 3 steps

1. **Create the file.** Copy `template.rs` → `my_idea.rs`. Rename the struct and the string returned by
   `Strategy::name()`, and write your logic in `on_event`. Lean on `toolkit.rs`:
   `RollingWindow` (mean/std/zscore), `Ema`, `RollingReturn`, `Signal`, `PositionSizer`, and `BaseStrategy`
   (max-position/order-size + `accepts`/`clamp_to_max_position`/`desired_flatten`). Give it a `Default` impl and
   a `from_params(&StrategyParams)` constructor so it's tunable (empty params == your defaults).
2. **Register it** in `mod.rs`: add `pub mod my_idea;` and a match arm in `build`:
   `"my_idea" => Some(Box::new(my_idea::MyIdea::from_params(params))),`.
3. **List it** in `mod.rs`: add `"my_idea"` to `ALL`, and a `StrategyInfo { name, description, key_params }`
   entry to `INFO` (same order — a test enforces it). `key_params` is the `"k=default, …"` string
   `list-strategies` prints.

No engine changes are ever needed — strategies only see the `Ctx` interface.

## The contract in one screen

```rust
pub trait Strategy {
    fn name(&self) -> &str;                                       // your --strategy id
    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx); // called per tick event, in time order
    fn on_finish(&mut self, _ctx: &mut dyn Ctx) {}               // optional end-of-data hook
}
```

Inside `on_event`, read state and act through `ctx` (no return value — you *emit* orders):

- **Read:** `ctx.mid(inst) -> Option<f64>` (dollars), `ctx.best_bid/ best_ask(inst) -> Option<(Cents,f64)>`,
  `ctx.imbalance(inst)`, `ctx.microprice(inst)`, `ctx.position(inst) -> f64` (+long/−short YES),
  `ctx.cash()`, `ctx.open_orders(inst) -> Vec<OrderView>`, `ctx.instruments()` /
  `ctx.instruments_for_venue(v)` (cross-venue discovery).
- **Act:** `ctx.place_limit(inst, side, price, qty)` (rests; fills as a **maker** when a trade crosses it),
  `ctx.place_market(inst, side, qty)` (**taker**, walks the opposing book), `ctx.cancel(order_id)`.
  `side` is `Side::Bid` (buy YES) / `Side::Ask` (sell YES); `price` is `Cents` (integer cents 1..99 — build with
  `Cents::from_dollars(0.42)`).
- **Order types / time-in-force:** `ctx.place_limit_ex(inst, side, price, qty, tif, post_only)` is the full limit
  API (`place_limit` is exactly `place_limit_ex(.., Tif::Gtc, false)`). Marketability is judged at the order's
  *activation time* (send + latency): a BUY at `P` is marketable if `P ≥ best ask`, a SELL if `P ≤ best bid`.
  - `Tif::Gtc` (default): a marketable order TAKES the crossing part (taker, bounded by `P`, never past it) and
    RESTS the remainder; a non-marketable one just rests.
  - `Tif::Ioc`: TAKES the crossing part only (bounded by `P`); the remainder is CANCELLED (never rests).
  - `post_only = true`: maker-only guarantee — a marketable order is REJECTED in full (counted in
    `summary.post_only_rejects`); a non-marketable one rests normally.
  One-liner: `ctx.place_limit_ex(inst, Side::Bid, Cents(42), 10.0, Tif::Ioc, false);` // take what crosses ≤ 42, cancel the rest.

**Latency note:** an order you place at tick `T` only becomes effective at `T + --latency-ns` — limits are
hittable only by later trades, and market orders execute against the book *as of* `T+latency`. Your strategy
code doesn't change for this; the engine applies it. Always test your edge with realistic `--latency-ns`.

A complete worked example (a microprice trend strategy, end to end) is in
[`../../../strategyFormat.md`](../../../strategyFormat.md) §7.
