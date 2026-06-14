# strategyFormat.md — how to write a strategy

This is the precise contract for authoring a backtester strategy: which functions you must implement, their
signatures and return values, the full `Ctx` API you act through, the helper toolkit, and how to register and
parameterize your strategy. Strategies live in `backtester/src/strategies/` and **only ever see the `Ctx`
interface** — you never touch engine internals.

A strategy works identically in batch backtests and live paper trading (same engine core).

---

## 1. The `Strategy` trait — what you must implement

```rust
pub trait Strategy {
    fn name(&self) -> &str;                                   // REQUIRED: your strategy's id string
    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx);  // REQUIRED: called per event, in time order
    fn on_finish(&mut self, _ctx: &mut dyn Ctx) {}            // OPTIONAL: end-of-data hook (default: no-op)
}
```

- **`name(&self) -> &str`** — return a stable string id (must equal the CLI name you register, e.g. `"my_idea"`).
- **`on_event(&mut self, ev, ctx)`** — your logic. Called for **every** `MarketEvent` in timestamp order. Read
  market state and your position from `ctx`, then place/cancel orders through `ctx`. **Returns nothing**: you
  act by calling `ctx.place_*` / `ctx.cancel`. The engine applies your queued actions after this returns.
- **`on_finish(&mut self, ctx)`** — optional; called once after the last event (e.g. to flatten). The engine
  already flattens at end-of-run by default (`flatten_at_end = true`), so most strategies leave this as the
  default no-op.

There is **no return value** from any hook. You never return orders; you *emit* them via `ctx`.

---

## 2. The event you receive: `MarketEvent`

```rust
pub enum MarketEvent { Delta(BookDelta), Trade(TradeEvent) }
ev.ts_ns()       -> i64     // event time (ns)
ev.instrument()  -> &str    // which instrument this event is for ("VENUE:symbol" for multi-venue)
```

You usually don't inspect the raw delta/trade — you read the *resulting* book state from `ctx`. A common first
line is `let inst = ev.instrument().to_string();`.

---

## 3. The `Ctx` API — everything you can read and do

The engine hands you `&mut dyn Ctx` each event. **Reads** (market + account state):

| Method | Returns | Meaning |
|---|---|---|
| `ctx.ts_ns()` | `i64` | Current event time (ns). |
| `ctx.best_bid(inst)` | `Option<(Cents, f64)>` | Best bid (price, size), if any. |
| `ctx.best_ask(inst)` | `Option<(Cents, f64)>` | Best ask (price, size), if any. |
| `ctx.mid(inst)` | `Option<f64>` | Mid in **dollars**, if both sides exist. |
| `ctx.imbalance(inst)` | `Option<f64>` | Top-of-book size imbalance in [-1,1]: `(bid_sz-ask_sz)/(bid_sz+ask_sz)`. |
| `ctx.microprice(inst)` | `Option<f64>` | Size-weighted mid (dollars), leans to the heavier side. |
| `ctx.position(inst)` | `f64` | Net signed position (contracts; **+ long YES, − short YES**). |
| `ctx.cash()` | `f64` | Free cash (account currency). |
| `ctx.open_orders(inst)` | `Vec<OrderView>` | Your currently-resting orders (`{id, side, price, remaining}`). |
| `ctx.instruments()` | `Vec<String>` | All instruments the engine has a book for (for discovery / cross-venue). |
| `ctx.instruments_for_venue(v)` | `Vec<String>` | Subset whose venue tag matches `v` (case-insensitive). |

**Actions** (you emit these; they're applied after `on_event` returns):

| Method | Effect |
|---|---|
| `ctx.place_limit(inst, side, price, qty)` | Rest a limit order. `side: Side`, `price: Cents`, `qty: f64`. Fills as a **maker** when the book trades through it. |
| `ctx.place_market(inst, side, qty)` | Immediate **taker** order that walks the opposing book. |
| `ctx.cancel(order_id)` | Cancel a resting order by its `id` (from `open_orders`). |

Where:
- `Side::Bid` = **buy YES**, `Side::Ask` = **sell YES**.
- `Cents` is integer cents in `1..=99`: build with `Cents::from_dollars(0.42)` → `Cents(42)`, read with
  `c.to_dollars()`. Clamp your computed prices into `[1, 99]` (place_limit at an out-of-range price is wasted).
- `qty` is a contract count (`f64`).

---

## 4. Parameters: `from_params` + `Params` (tunable, back-compatible)

Give your strategy a `Default` impl (the hardcoded defaults) **and** a `from_params(&StrategyParams)`
constructor so users can tune it via `--strategy-param key=value` / a `[strategy_params]` config table.
`StrategyParams = BTreeMap<String, f64>`. Read keys with the `Params` accessor; **an omitted key must fall back
to your default**, so empty params reproduce `Default` exactly:

```rust
use crate::strategies::{Params, StrategyParams};

pub fn from_params(params: &StrategyParams) -> Self {
    let p = Params::new(params);
    let d = Self::default();
    Self {
        window:  p.get_usize("window", d.window),     // usize key
        entry_z: p.get("entry_z", d.entry_z),         // f64 key
        max_spread_cents: p.get_i32("max_spread", d.max_spread_cents), // i32 key
        // ...
    }
}
```

`Params` methods: `get(key, default_f64) -> f64`, `get_usize(key, default) -> usize`,
`get_i32(key, default) -> i32`. **Use the same key names you'll advertise in `INFO.key_params`** (so
`list-strategies` and the docs agree).

---

## 5. The reusable toolkit (`strategies/toolkit.rs`)

Compose these instead of reinventing them:

- **`RollingWindow::new(capacity)`** — ring buffer. `push(x)`, `mean()`, `std()` (sample, n−1),
  `zscore(latest)`, `min()`, `max()`, `latest()`, `len()`, `is_full()`. All return `Option<f64>` where a value
  may be undefined.
- **`Ema::new(alpha)`** / **`Ema::from_period(n)`** — `update(x) -> f64`, `value() -> Option<f64>`.
- **`RollingReturn::new()`** — `update(x) -> Option<f64>` (simple return vs previous sample).
- **`Signal`** enum `{ Long, Short, Flat, None }` with `Signal::from_zscore_reversion(z, entry, exit)` (mean-
  reversion) and `Signal::from_zscore_trend(z, entry)` (trend).
- **`PositionSizer`** `{ Fixed(qty) | FractionOfCash(frac) }` — `qty(cash, price) -> f64`.
- **`BaseStrategy::new(max_position, order_qty)`** — embed by composition. Fields `max_position`, `order_qty`,
  `instrument_filter`. Methods: `.with_filter(glob)`, `.accepts(inst) -> bool` (honors the filter),
  `.desired_flatten(inst, ctx)` (market-flatten the current position), and
  `.clamp_to_max_position(current, delta) -> f64` (clamp an intended signed change so net stays within
  `[-max_position, max_position]`).

---

## 6. Registering a new strategy — the 3 steps

(verbatim from `strategies/mod.rs`)

1. **Create the file.** Copy `strategies/template.rs` → `strategies/my_idea.rs`, rename the struct and the
   string returned by `name()`, implement your logic in `on_event`.
2. **Declare it.** In `strategies/mod.rs`: add `pub mod my_idea;`, and a match arm in `build`:
   `"my_idea" => Some(Box::new(my_idea::MyIdea::from_params(params))),`.
3. **Register the name.** Add `"my_idea"` to the `ALL` slice (the CLI `--strategy` allowed values come from
   `ALL`), and add a `StrategyInfo { name, description, key_params }` entry to `INFO` in the **same order**
   (a test enforces `INFO` matches `ALL`). `key_params` is the string `list-strategies` prints, e.g.
   `"window=20, entry_z=1.5, size=10"`.

No engine changes are ever needed.

---

## 7. Complete worked example

A momentum-by-microprice idea, from scratch (`strategies/micro_trend.rs`):

```rust
use crate::strategies::toolkit::{BaseStrategy, Ema};
use crate::strategies::{Params, StrategyParams};
use crate::strategy::{Ctx, Strategy};
use crate::types::{MarketEvent, Side};
use std::collections::HashMap;

pub struct MicroTrend {
    base: BaseStrategy,
    ema_period: usize,
    edge: f64,                       // dollars the microprice must lead the EMA to trigger
    emas: HashMap<String, Ema>,
}

impl Default for MicroTrend {
    fn default() -> Self {
        MicroTrend { base: BaseStrategy::new(40.0, 10.0), ema_period: 20, edge: 0.01, emas: HashMap::new() }
    }
}

impl MicroTrend {
    pub fn from_params(params: &StrategyParams) -> Self {
        let p = Params::new(params);
        let d = MicroTrend::default();
        MicroTrend {
            base: BaseStrategy::new(p.get("max_inventory", d.base.max_position),
                                    p.get("size", d.base.order_qty)),
            ema_period: p.get_usize("ema_period", d.ema_period),
            edge: p.get("edge", d.edge),
            emas: HashMap::new(),
        }
    }
}

impl Strategy for MicroTrend {
    fn name(&self) -> &str { "micro_trend" }

    fn on_event(&mut self, ev: &MarketEvent, ctx: &mut dyn Ctx) {
        let inst = ev.instrument().to_string();
        if !self.base.accepts(&inst) { return; }

        let micro = match ctx.microprice(&inst) { Some(m) => m, None => return };
        let ema = self.emas.entry(inst.clone()).or_insert_with(|| Ema::from_period(self.ema_period));
        let avg = ema.update(micro);

        let pos = ctx.position(&inst);
        if micro > avg + self.edge && pos <= 0.0 {
            let add = self.base.clamp_to_max_position(pos, self.base.order_qty);
            if add > 0.0 { ctx.place_market(&inst, Side::Bid, add); }       // trend up -> buy YES
        } else if micro < avg - self.edge && pos > 0.0 {
            self.base.desired_flatten(&inst, ctx);                          // trend down -> exit
        }
    }
}
```

Then register: `pub mod micro_trend;`, a `build` arm, add `"micro_trend"` to `ALL`, and an `INFO` entry
`key_params: "ema_period=20, edge=0.01, size=10, max_inventory=40"`. Done — `--strategy micro_trend
--strategy-param edge=0.02` now works.

---

## 8. Conventions & gotchas

- **Position sign:** `+` is long YES, `−` is short YES. Buying (`Side::Bid`) increases it, selling
  (`Side::Ask`) decreases it.
- **Prices are integer cents `[1,99]`.** Convert from dollars with `Cents::from_dollars`; clamp before placing.
- **Maker fills need trades.** A resting limit only fills when a `TradeEvent` crosses it (after its queue). On
  data with no trades (e.g. overnight), a pure market-maker won't fill — that's correct, not a bug. Use
  `place_market` (taker) if you want to cross the book unconditionally.
- **Respect limits.** Use `BaseStrategy::clamp_to_max_position` so you don't run away inventory.
- **Cancel/replace** on book moves if you quote: read `ctx.open_orders(inst)` and `ctx.cancel(id)` before
  re-quoting (quoting strategies typically cancel-all then re-place each event).
- **Determinism:** no randomness — the whole sim is deterministic. Don't introduce RNG or wall-clock; derive
  everything from the event stream so runs reproduce byte-for-byte.
- **Multi-venue:** instrument ids are `"VENUE:symbol"`. To trade across venues, discover them with
  `ctx.instruments()` / `ctx.instruments_for_venue("POLYMARKET")` (see `cross_venue_arb.rs`).
- **Execution realism is orthogonal.** Latency, slippage, fees, and rewards are applied by the engine based on
  config — your strategy code doesn't change to account for them; just run with the toggles.

See `backtesterDescription.md` for the engine/execution internals and the full config reference.
