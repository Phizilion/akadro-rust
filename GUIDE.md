# akadro strategy guide

Everything you need to write and run strategies on **akadro**. This guide is the
mental model + recipes + a cheatsheet of the real API; for exact signatures use the
API docs — `cargo doc --open -p akadro --all-features` from this repo, or the
pre-built `api/doc/` (HTML) + `api/json/` (machine-readable) inside a scaffolded
strategy lab (`agentic/scaffold-lab.sh`). Written for both humans and agents; an
agent works against the public API only and does not read akadro's source.

---

## 1. Mental model

akadro runs a strategy **unchanged** in backtest and live. One event loop pulls
events from a data feed, updates observed state, then calls your strategy. Your
strategy is **blind to which mode it's in** and is **deterministic** (no clocks,
no RNG, no I/O) — that's what makes backtest results trustworthy.

You implement the `Strategy` trait. The engine hands you a `Ctx` each bar; you read
the past/present through it and submit orders through it. You never touch the
exchange or the dataset directly.

```
data feed ─▶ Engine loop ─▶ your Strategy::on_bar(bar, ctx)
                  │                 │
                  └─ portfolio ◀────┘  (orders → fills → account events)
```

---

## 2. The event loop & fill timing — read this twice

Per incoming event at logical time `t`, the engine does, **in this order**:

1. **`observe`** — fills orders that were *already resting* (submitted on a prior
   bar). **A market order you submit on bar `i` fills at bar `i+1`'s open.** There
   is no same-bar fill — that would be execution look-ahead.
2. resulting **account events** update the portfolio and reach `on_account` /
   `on_fill` / etc.
3. **market state is appended** — so the `Series` you read in step 4 *includes*
   the current bar.
4. **`on_bar(bar, ctx)`** runs. Orders you submit here start resting and are
   acknowledged; they fill on the **next** bar (step 1 of the next iteration).

Consequences you must design around:
- In `on_bar`, `ctx.closes(inst).latest()` is **this** bar's close. `ago(1)` is the
  previous bar.
- A signal computed on bar `i` executes at bar `i+1`'s open. Budget for that lag.
- Account state (`net_qty`, `cash`, `equity`) reflects fills **up to and
  including** this bar's `observe`.

---

## 3. Writing a Strategy

```rust
use akadro::prelude::*;

struct MyStrat { inst: InstrumentId }

impl Strategy for MyStrat {
    fn on_bar(&mut self, bar: Bar, ctx: &mut Ctx<'_>) {
        // read past/present via ctx, decide, submit orders
    }
}
```

`on_bar` is the only required method. All other hooks default to no-ops — override
the ones you need:

| Hook | When |
|---|---|
| `on_start(ctx)` | once, before the first bar |
| `on_bar(bar, ctx)` | each completed bar **(required)** |
| `on_account(ev, ctx)` | any account event |
| `on_fill(fill, ctx)` | one of your orders filled (whole or partial) |
| `on_order_rejected(..)` / `on_order_canceled(..)` | order lifecycle |
| `on_liquidation(..)` / `on_funding(..)` | perp events |
| `on_timer(at, ctx)` | an event-time timer you set with `ctx.schedule(..)` fired |
| `on_stop(ctx)` | once, after the last bar |

The `Bar` is passed **by value** (it's `Copy`-cheap). You cannot stash `ctx` or a
`Series` (see §6).

---

## 4. Reading market data (`Ctx` + `Series`)

`Ctx` is your per-bar handle. Market history comes as a `Series`, which **only
goes backward**:

```rust
let closes = ctx.closes(self.inst);        // Option<Series<Price>>
if let Some(s) = closes {
    let now   = s.latest();                // Option<Price> — this bar
    let prev  = s.ago(1);                  // Option<Price> — 1 bar ago (None if OOB)
    let last5 = s.last_n(5);               // iterator of the most recent ≤5 values
    let n     = s.len();                   // how many bars observed
}
```

`Ctx` accessors (all read-only, no look-ahead):

| Group | Methods |
|---|---|
| Series | `closes`, `opens`, `highs`, `lows`, `volumes` (each `Option<Series<…>>`) |
| Bar count | `bar_count`, `is_warmed_up` |
| Time | `now()` → logical event time (never wall-clock; equal across two reads) |
| Position | `net_qty(inst)`, `avg_entry(inst)` |
| Money | `cash()`, `equity()`, `realized_pnl()`, `realized_pnl_net()`, `unrealized_pnl(inst)` |
| Orders | `has_open_order(inst)`, `open_order_ids()`, `submitted_order(id)` |
| Instrument | `instrument_spec(inst)` (tick/lot/min-notional/caps) |

`Series` only exposes `latest`, `ago(n)`, `last_n(n)`, `len`, `is_empty`. There is
**no forward accessor and no indexing** — reading the future is not expressible.

---

## 5. Submitting orders & venue commands

Order through `ctx` only. `OrderRequest` constructors:

```rust
ctx.submit(OrderRequest::market(inst, Side::Buy, qty));
ctx.submit(OrderRequest::limit(inst, Side::Sell, qty, price));
// also: stop, stop_limit, market_if_touched, trailing_stop / _pct / _kind,
//       post_only, oco, reduce_only, with_tif
ctx.cancel(order_id);                       // request a cancel (may race a fill live)
ctx.command(VenueCommand::…);               // leverage / margin-mode / etc.
ctx.schedule(at_timestamp);                 // fire on_timer at an event time
```

Order ids: `ctx.submit` returns a `ClientOrderId` you can track via
`has_open_order` / `open_order_ids` / `submitted_order`.

---

## 6. The kill feature — what compiles and what doesn't

Reading future market data is a **compile error**, by construction. You don't have
to remember not to — you *can't* express it.

**Won't compile** (don't try; these are the proofs in `api/examples/` and the
denied source's trybuild suite):
- Storing a `Ctx` or `Series` in `self`, a `Vec`, `RefCell`, `Rc`, `Arc<Mutex>`,
  `Cell`, `OnceCell`, `Box`, a closure, or across a `thread::spawn`.
- Any forward accessor / `series[i]` indexing (there is none).

**Legal patterns** (see `api/examples/legal_*.rs`):
- **Copy scalars out** of the series into your own state: `let c = s.latest()?;`
  then keep `c` (a `Price`, which is `Copy`). You keep *values*, not the view.
- **Collect past copies**: `let v: Vec<Price> = s.last_n(20).collect();` — `last_n`
  is a backward iterator yielding copied-out `Price` values; keeping them is fine.
  (Or keep a `Vec<Price>` in `self` and push `s.latest()` each bar — see
  `api/examples/legal_collect_past_copies.rs`.)

If you hit a lifetime error mentioning `'bar`, you tried to keep a `Ctx`/`Series`
alive past the call. Fix: copy the scalar(s) you need *out*, and store those.

---

## 7. Fixed-point money — no floats, ever

Money math is exact integers (so fills/PnL are bit-reproducible):

| Type | Repr | Notes |
|---|---|---|
| `Price` | `i64` | scaled by the instrument's `price_scale` |
| `Qty` | `i64` | scaled by `qty_scale` |
| `Money` | `i128` | scaled by `price_scale + qty_scale` |

```rust
let p = Price::from_raw(104_876_69);       // raw scaled integer in, .raw() out
let notional: Money = p.notional(qty);     // Price × Qty, widened to i128 — use THIS
let fee = notional.mul_bps(5);             // 5 bps, saturating
let s = p.display(price_scale);            // human string for printing only
```

Key methods: `Price`/`Qty`/`Money`: `from_raw`, `raw`, `ZERO`, `display`,
`checked_add`/`checked_sub`; `Price::notional`, `Price::diff`; `Money::mul_bps`,
`saturating_add/sub`, `is_positive/is_negative`, `neg`. **Never** cast to `f64` for
decisions — only for display.

**Equity-based sizing** (compounding, keeps every buy affordable):
```rust
let deploy = ctx.equity().raw() * 90 / 100;          // 90% of current equity
let qty_raw = (deploy / bar.close.raw()) as i64;     // money / price → qty
if qty_raw > 0 { ctx.submit(OrderRequest::market(self.inst, Side::Buy, Qty::from_raw(qty_raw))); }
```

---

## 8. Getting data — only through akadro (rule)

Never load your own data. Get the engine's feed ONLY from the akadro data API — it
returns a ready `DataSource`, so you never touch a raw `Vec<Bar>`:

```rust
use akadro::data::load_or_cache_feed;       // download-once / replay-from-cache → feed
use akadro_venue_mexc::MexcKlineFeed;        // (and other akadro_venue_* connector feeds)

// Cache miss → fetch via the connector; cache hit → replay. Returns a DataSource.
let feed = load_or_cache_feed(
    cache_path, instrument, price_scale, qty_scale,
    || MexcKlineFeed::new(/* base_url, symbol, ... */).with_range(start_ms, end_ms),
)?;
```

The raw `HistoricalFeed::from_bars`/`new` Vec-injection constructors are **gated off**
(feature `import-bars`, off by default), and a **custom `DataSource` is disallowed** in
the lab — those are the data-leakage doors. `api/examples/full_pipeline.rs` shows the
complete sanctioned flow: exchangeInfo → klines → Feather cache → backtest → analytics
→ replay oracle. Do **not** write your own HTTP fetcher or read foreign data files.

---

## 9. Running a backtest

```rust
let exchange = SimulatedExchange::new(specs.clone(), fee_bps)
    .with_starting_cash(Money::from_raw(cash_raw));   // optional cash guard
let report = Engine::new(&specs, Money::from_raw(cash_raw),
                         feed, exchange, strategy)     // `feed` from load_or_cache_feed
    .expect("engine")
    .run();                                            // -> RunReport
```

`RunReport` carries: `bars_processed`, `orders_submitted`, `fills`,
`realized_pnl`, `total_fees`, `equity_curve`, `seed`. Persist with
`report.save(path)` / `RunReport::load(path)` (needs the `serde` feature).

**Fill realism** (opt-in builders on `SimulatedExchange`, conservative defaults
keep parity): `with_slippage_bps`, `with_latency_bars`, `with_participation_bps`,
`with_funding`, `with_liquidation_bps`, `with_starting_cash`, `with_seed`.

---

## 10. Analytics

```rust
use akadro::analytics::{PerformanceReport, TradeStats};
let perf = PerformanceReport::from_equity(&report.equity_curve, periods_per_year); // Option
let trades = TradeStats::from_fills(&report.fills);
```

`PerformanceReport`: total/annualized return, Sharpe, Sortino, max drawdown,
Calmar, volatility (annualized). Conventions: sample stdev (ddof=1); `±∞`/`0` at a
zero denominator; returns `None` if equity ever ≤ 0. `TradeStats`: win rate, profit
factor (`+∞` if no losses), avg win/loss, payoff ratio, expectancy — **net of
fees**, reconstructed from the fill log.

---

## 11. Determinism & parity (why your code is constrained)

- No wall-clock: `ctx.now()` is logical event time, identical in backtest and live.
- No RNG in strategies; probabilistic fills draw only from the engine's seeded RNG.
- No `Instant::now`, no ambient I/O in strategy logic.
- All account state comes from the event stream — there is no "query my balance"
  call. This is what makes the *same source* behave identically in both modes.

Keep a **fresh engine per run**; don't stash state in `static`/`thread_local`
across runs (out-of-band state — defeats determinism).

---

## 12. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| lifetime error mentioning `'bar` | You kept a `Ctx`/`Series` past the call. Copy the scalar(s) out and store those (§6). |
| `error: usage of an unsafe block` | `unsafe` is forbidden crate-wide. Don't use it (§rules in `CLAUDE.md`). |
| order never fills | Fills are next-bar-open (§2). A market order on the last bar never fills. |
| equity goes negative / `InsufficientFunds` | Size off `ctx.equity()` (§7); the cash guard rejects unaffordable buys. |
| `from_equity` returns `None` | Equity hit ≤ 0 — analytics are undefined; the run blew up. |
| no such method `ahead`/`next`; `series[i]` won't compile | By design — there is no forward access (§6). |

Exact signatures: `api/doc/akadro/index.html` and `api/json/*.json`
(`api/json/README.md` maps each type to its crate).
