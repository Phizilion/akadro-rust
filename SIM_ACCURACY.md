# akadro Backtest Simulation — Accuracy & Honesty Audit

> Scope: does akadro's backtest simulator produce results **more favourable than
> reality**, i.e. can it "fake" good performance that would not hold live?
> Reconciled from a fan-out hunt + per-finding verification, then re-checked
> against source. Citations are `file:line`.

---

## Executive verdict

**Does akadro fake results? Verdict: MINOR optimistic bias on the conservative
default path — no systematic, hidden, or unbounded fakery.**

The core accounting and event-ordering machinery is honest and conservative:

- **No look-ahead in the engine.** `observe → push bar → on_bar` ordering
  (`engine.rs:235-282`) means a market order submitted in `on_bar` only fills at
  the *next* bar's open. The strategy never sees future data. (Confirmed; this is
  the project's kill feature and it holds.)
- **Realized-PnL accounting is exact and unbiased** — weighted-average entry,
  round-half-away-from-zero, correct flip handling, property-tested for
  conservation and round-trip neutrality (`portfolio.rs:44-95`,
  `portfolio.rs:417-446`).
- **Stops are conservatively modelled**: a gap through a stop fills at the
  *worse* of trigger/open (`exchange.rs:841-846`) — a deliberate safety margin,
  *harsher* than reality.
- **OCO double-execution is correctly prevented** (`exchange.rs:1006-1015,
  1088-1094`).
- Every realism cost (slippage, impact, latency, participation cap, funding,
  liquidation, cash guard, probabilistic maker fill) **exists and is correct when
  enabled** — the issue is purely that they default *off*.

The optimism that *does* exist is concentrated in **defaults that are zero/None**
(fees, spread, participation, cash, funding) and in a handful of **fixable
asymmetries** (limit-touch always-fills, MIT/trailing-stop fill price). None is
hidden: the `FillConfig` docstring (`exchange.rs:29-31`) lists every disabled
knob. The one genuine *documentation* defect is the module header
(`exchange.rs:7-11`) claiming "every fill pays a flat taker fee" while
`fee_bps` defaults to **0** — the code is more optimistic than the prose.

### Worst-case optimistic error on the DEFAULT path

**`>10%` for adversarial / high-turnover / large-size / leveraged strategies;
`1-10%` for a typical moderate-turnover strategy; `<1%` for low-frequency
buy-and-hold.**

The `>10%` tail is reachable only when a strategy *combines* several disabled
defaults — e.g. a high-turnover passive-limit strategy (zero fees + always-fill
touches + zero spread) or a large-size / leveraged strategy (no participation
cap + no cash guard + no funding). For a single moderate strategy honoring the
documented `SimulatedExchange::new(specs, fee_bps)` constructor with a realistic
fee, the residual default-path optimism is `1-10%`, dominated by the absent
bid/ask spread (a natural OHLCV limitation).

**fakes_results classification: `minor-bias`** — the simulator is honest and
conservative in structure; its optimism is opt-out friction that is fully
documented and, for the recommended API, mostly a function of the user not
enabling realism knobs.

---

## Severity + magnitude table

| Severity | Title | Area | Default path? | Error band | Natural flaw? |
|---|---|---|---|---|---|
| **major** | Limit touch always fills (no queue/depth model) | fill rate | yes | strategy-dependent (≤10-30% for passive strategies) | no (fixable; opt-in model exists) |
| **major** | Unlimited fill size per bar (no participation cap) | fill size | yes | strategy-dependent (>10% for large-size) | no (fixable) |
| **major** | No buying-power / cash enforcement by default | sizing | yes | strategy-dependent (>10% if pyramiding/DCA) | no (fixable; opt-in) |
| **major** | No short-sale / base-asset inventory or borrow cost | costs | yes | strategy-dependent (>10% for short-heavy spot) | no (D17 deferral) |
| **major** | No perpetual funding by default | costs | yes (perps) | >10% over long perp holds | no (fixable; opt-in) |
| **minor** | Zero taker fee by default (`fee_bps=0`) + doc contradiction | costs | yes | >10% high-turnover / 1-10% typical | no |
| **minor** | No bid/ask spread (slippage/impact default 0) | costs/price | yes | 1-10% typical / >10% HFT | no (data lacks bid/ask, but a default proxy is feasible) |
| **minor** | MIT fills at trigger (no worse-of-open, unlike stops) | price | yes | 1-10% for MIT-heavy | no (asymmetry vs stops) |
| **minor** | Limit price-improvement on gap-open (fill at open < limit) | price | yes | strategy-dependent (<1% typical) | partly (venue-dependent) |
| **minor** | Flat fee, no maker/taker distinction (no rebates) | costs | yes | strategy-dependent (<1-2%) | no |
| **minor** | OCO wide-bar tiebreaker = submission order (strategy-controllable) | price | yes | strategy-dependent (<1% typical) | partly |
| **info** | No bid/ask spread — fills at OHLC mid (core natural limitation) | price | yes | strategy-dependent | **yes** |
| **info** | Trailing-stop trigger from same-bar high/low (intra-bar ordering) | price/look-ahead | yes | strategy-dependent | **yes** |
| **info** | No queue position on decisive trade-through | fill rate | yes | strategy-dependent | **yes** |
| **info** | Mark-to-market / equity / unrealized PnL at bar close (not bid) | pnl | yes | <1% | **yes** |
| **info** | Equity curve hides intra-bar drawdown (sampled at close) | pnl | yes | strategy-dependent | **yes** |
| **info** | `mul_bps` truncates fee toward zero (≤1 raw unit/fill) | costs | yes | <1% | no (documented, negligible) |
| **info** | Unrealized PnL excludes entry fees (gross); net accessor exists | pnl | yes | <1% (no effect on reported equity) | no |
| **info** | Zero latency by default (`latency_bars=0`) | timing | yes | <1% (bar-cadence) | yes |
| **info** | Liquidation checked at bar.close, not intra-bar extreme | price | **opt-in** | <1-3% leveraged | yes (intra-bar) |
| **info** | Funding settled at bar.close (not TWAP mark) | costs | **opt-in** | <1% | yes |
| **info** | Cash guard skips market buys before first bar | sizing | **opt-in** | <1% (edge) | no |
| **info** | Stop fills at worse-of(trigger, open) — *conservative* | price | yes | <1% | n/a (safety margin) |
| **info** | Slippage never applied to passive limits — *correct* | price | yes | <1% | n/a (correct) |
| **info** | Participation cap depletes across orders — *conservative* | fill size | opt-in | <1% | n/a (correct) |
| **info** | Volume-share impact model — *opt-in, conservative (linear)* | costs | opt-in | <1% | n/a (correct) |
| **info** | Engine event ordering — *no look-ahead, correct* | look-ahead | yes | <1% | n/a (correct) |
| **info** | Realized-PnL / avg-entry / flip accounting — *correct, unbiased* | pnl | yes | <1% | n/a (correct) |
| **info** | OCO double-execution guard — *correct* | pnl | yes | <1% | n/a (correct) |

---

## Detailed findings (confirmed optimistic biases)

### MAJOR — default path

#### 1. Limit-order touch always fills (no queue position / depth model)
- **Mechanism:** `limit_fill` (`exchange.rs:822-835`) returns `Some(price)`
  whenever `bar.low <= limit` (buy) / `bar.high >= limit` (sell). The `<=`/`>=`
  includes an *exact touch* (`bar.low == limit`). By default
  (`maker_fill_prob_bps = None`), `maker_touch_is_uncertain`
  (`exchange.rs:528-539`) returns `false` and `draw_maker_fills`
  (`exchange.rs:545-547`) returns `true`, so the probabilistic gate at
  `exchange.rs:995` is never taken — **every touch fills with probability 1**.
- **Reality:** a touch fills only if the order is at/near the front of the queue;
  empirically 20-70% on a touch. The simulator overstates fill rate, turnover and
  PnL for passive-limit strategies.
- **Direction / magnitude:** optimistic; **strategy-dependent**. Mean-reversion /
  grid / market-making proxies that rely on touches can see 10-30%+ inflated fill
  counts → comparable PnL distortion. Trend strategies posting wide limits: <1%.
- **Natural flaw?** No — the opt-in `with_maker_fill_probability()` model proves
  the ambiguity is addressable. The default chose always-fill for bit-identical
  parity, not because the data forbids a probabilistic treatment.
- **Doc mismatch:** module header (`exchange.rs:9-10`) says limits "fill only when
  a later bar trades *through* the limit"; the code fills on touch too.
- **Mitigation:** set `with_maker_fill_probability(~5000)` for limit-heavy
  strategies. Evidence test: `exchange.rs:1363-1400`.

#### 2. Unlimited fill size per bar (no participation cap by default)
- **Mechanism:** `max_participation_bps = None` by default ⇒ `cap = None`
  (`exchange.rs:921-924`) ⇒ `want = remaining` (full order qty,
  `exchange.rs:1017`), regardless of `bar.volume`. A 10,000-unit order fills
  fully on a 5-unit-volume bar at the open.
- **Reality:** large orders fill partially, across many bars, walking the book.
- **Direction / magnitude:** optimistic; **strategy-dependent**. <1% for orders
  <1% of bar volume; **>10%** for strategies sizing 5-20%+ of bar volume (saves
  multi-bar adverse drift + extra fees). Strategy-exploitable: bigger orders get a
  bigger free advantage.
- **Natural flaw?** No — `bar.volume` is available (used at `exchange.rs:924`) and
  `with_participation_bps()` implements the cap. Acknowledged deferred work
  (AGENTS.md #15).
- **Mitigation:** `with_participation_bps(~1000-2000)` for any size >1% ADV.

#### 3. No buying-power / cash enforcement by default
- **Mechanism:** `starting_cash = None` by default ⇒ the buy guard at
  `exchange.rs:431-448` is skipped entirely (gated on `if let Some(cash) =
  self.cash`). Any buy is accepted regardless of capital; the mirrored cash can go
  arbitrarily negative.
- **Direction / magnitude:** optimistic; **strategy-dependent**. A disciplined
  sizer: <1%. A DCA-after-loss / pyramiding / grid strategy can hold positions
  many multiples of capital, inflating unrealized PnL and equity → **>10%**.
- **Natural flaw?** No. Opt-out via `with_starting_cash(initial_cash)`. Documented
  (AGENTS.md §12; the playground experiment that surfaced it).
- **Mitigation:** always call `with_starting_cash` with the engine's initial cash.

#### 4. No short-sale / base-asset inventory or borrow cost (spot)
- **Mechanism:** `rejection()` (`exchange.rs:384-449`) checks affordability for
  **buys only**; sells generate cash with no inventory/borrow check
  (comment `exchange.rs:428`). A spot strategy can short unlimited size at zero
  borrow cost.
- **Direction / magnitude:** optimistic; **strategy-dependent**. Long-only: 0%.
  Short-heavy spot held for weeks at 0.1-0.5%/day borrow: **>10%**.
- **Natural flaw?** No — a `borrow_cost` knob is feasible (mirrors funding).
  Documented deferral (D17, AGENTS.md §12).
- **Mitigation:** use perpetual instruments (with funding) for short exposure;
  D17 base-asset tracking is future work.

#### 5. No perpetual funding by default
- **Mechanism:** `funding_bps = 0`, `funding_interval_bars = 0` by default;
  `accrue_funding` early-returns when `interval == 0` (`exchange.rs:704-706`).
  A perp position carries zero funding.
- **Direction / magnitude:** optimistic; **>10%** for a long perp held across a
  contango year (~0.01%/8h ≈ ~11%/yr of notional drag omitted).
- **Natural flaw?** No — `with_funding` / `with_funding_schedule` implement it
  correctly; only the default is off and the constructor doc does not warn.
- **Mitigation:** enable `with_funding` with realistic rates for any perp
  strategy; ideally a recorded `with_funding_schedule`. Note: the funding
  *notional* uses `bar.close` not a TWAP mark (`exchange.rs:735`) — a separate,
  natural <1% approximation (see Natural section).

### MINOR — default path

#### 6. Zero taker fee by default + a self-contradicting docstring
- **Mechanism:** `FillConfig::default()` ⇒ `fee_bps = 0`; `fee_cost` only charges
  when `fee_bps != 0` (`exchange.rs:561`). The recommended constructor
  `SimulatedExchange::new(specs, fee_bps)` (`exchange.rs:191`) *requires* an
  explicit fee, but the `with_config(FillConfig::default())` path and the example
  `SimulatedExchange::new(vec![spec], 0)` (backtest `lib.rs:36`) run friction-free.
- **Direction / magnitude:** optimistic. **>10%** for high turnover (10 bps × 2 ×
  100 round-trips = 2000 bps); **1-10%** typical; <1% low-frequency.
- **Why minor (not major):** the *recommended* API forces a fee and the test
  fixtures use `FEE_BPS=5` (`tests/common/mod.rs:18`), so a user following docs is
  safe. The real defect is the **documentation contradiction**: header
  `exchange.rs:7-11` claims "every fill pays a flat taker fee" while the
  `FillConfig` doc (`exchange.rs:29-31`) correctly says "(no fee …)".
- **Mitigation:** fix the header; consider warning when `fee_bps == 0` for a
  CEX-kind instrument.

#### 7. No bid/ask spread modelled (`slippage_bps`/`impact_bps` default 0)
- **Mechanism:** market fills at `bar.open` (`exchange.rs:577`); `slipped` returns
  the base unchanged at `slippage_bps == 0` (`exchange.rs:498-499`); `fill_price`
  short-circuits at `impact_bps == 0` (`exchange.rs:512`). There is **no**
  `spread_bps` field. Every taker fill is implicitly at the mid.
- **Direction / magnitude:** optimistic; per leg ≈ half-spread (1-5 bps liquid,
  10-100 bps thin). **1-10%** for a moderate strategy; **>10%** for HFT on thin
  pairs; <1% buy-and-hold.
- **Natural flaw?** *Partly.* The bid/ask width is genuinely absent from OHLCV
  data (that part is the natural-flaw INFO item below). But *defaulting the
  proxy (`slippage_bps`) to 0 with no built-in spread* is a modelling choice —
  LEAN ships a nonzero default. Rated **minor** because the friction is
  predictable and `slippage_bps` is the documented opt-in.
- **Mitigation:** set `slippage_bps ≥ typical half-spread` for the venue (≈1-2 bps
  liquid crypto, 5-20 bps small caps). Consider a distinct `spread_bps`
  applied symmetrically (buys at open+½spread, sells at open−½spread).

#### 8. MIT fills exactly at trigger (no worse-of-open, unlike stops)
- **Mechanism:** `MarketIfTouched` returns `Outcome::Fill(trigger)`
  (`exchange.rs:619-628`) with no `stop_fill`-style gap correction. Compare the
  fixed stop, which fills at `worse-of(trigger, open)` (`exchange.rs:586` →
  `stop_fill` `exchange.rs:841-846`). With default `slippage_bps = 0`, a buy MIT
  at 90 on a bar that gaps to open 85 still fills at 90, not 85 / the worse open.
- **Direction / magnitude:** optimistic; **1-10%** for MIT-heavy strategies in
  volatile/gappy regimes; <1% otherwise. The test suite itself documents the bias
  (`exchange.rs:1786-1805`, comment: "fills at trigger (90), not at the worse open
  (85)").
- **Natural flaw?** No — the fix is trivial and mirrors stops: `max(trigger,open)`
  buy / `min(trigger,open)` sell.
- **Mitigation:** apply `slippage_bps` (already wired for MIT via `fill_price`,
  `exchange.rs:1065`); ideally apply the stop's worse-of-open rule.

#### 9. Limit price-improvement on a gap open (fill at open, better than limit)
- **Mechanism:** `limit_fill` fills a resting buy at `min(ref_open, limit)`
  (`exchange.rs:827-828`); on a gap-down open below the limit, it fills at the
  *open* (better than the stated limit). Verified by test
  `buy_limit_price_improves_on_gap_down_open` (`exchange.rs:1526-1543`).
- **Direction / magnitude:** optimistic for the holder. **<1%** for typical
  strategies (gaps are infrequent); higher for deliberate gap-fill strategies.
- **Natural flaw?** *Partly.* On continuous-matching crypto CEX a resting limit
  queued at L fills at L, not the better open; on auction venues price improvement
  is real. The convention is shared by LEAN/Zipline. The deviation from
  continuous-book behaviour is a choice → **minor**.
- **Mitigation:** an opt-in `no_limit_price_improvement` flag to fill exactly at L.

#### 10. Flat fee, no maker/taker distinction (no rebates)
- **Mechanism:** `fee_cost` always pushes `CostKind::Taker` and lacks `OrderKind`
  (`exchange.rs:559-571`, `1110`). `CostKind::Maker` exists but is never emitted.
- **Direction / magnitude:** optimistic when `fee_bps=0` (no friction);
  pessimistic for limit-heavy strategies when `fee_bps` is set to the taker rate
  (they pay taker on passive fills that would be maker/rebate live). Net
  **strategy-dependent**, typically <1-2%.
- **Natural flaw?** No — bar data suffices; needs `OrderKind` threaded into
  `fee_cost` + a `maker_fee_bps` field. **Minor.**

#### 11. OCO wide-bar tiebreaker = submission order (strategy-controllable)
- **Mechanism:** when both bracket legs are fillable on one wide bar, the
  first-submitted leg fills and the other cancels (`exchange.rs:1006-1015,
  1088-1094`; iteration is submission order). Submitting the take-profit first
  always wins on a wide bar.
- **Direction / magnitude:** optimistic and *exploitable by design*, but wide
  bars triggering both legs are rare → **<1%** typical; larger only for a strategy
  that deliberately orders legs and faces frequent wide bars.
- **Natural flaw?** *Partly* — the intra-bar path is unknowable from OHLC (the
  *ambiguity* is natural), but using submission order as the tiebreaker (vs a
  worst-price rule) is a fixable choice → **minor**.

---

## Natural / inherent simulation limitations (INFO)

These are honest, unavoidable consequences of bar-resolution (OHLCV) data with no
tick stream or order-book depth. They can be optimistic per-event, but **no
bar-based backtester (LEAN, Zipline, Nautilus, …) can avoid them**, so they are
INFO regardless of magnitude. akadro is transparent about each (AGENTS.md §4/§12:
"Real latency/fill fidelity is the irreducible accuracy ceiling").

- **No bid/ask spread in the data.** A `Bar` carries only OHLCV
  (`akadro-core/src/event.rs:36-51`) — there is no ask to fill a buy at or bid to
  fill a sell at. Fills at OHLC prices are mid/last, half-a-spread optimistic per
  leg. **Magnitude:** 1-5 bps/leg liquid, 10-100 bps thin; cumulative
  strategy-dependent. (The *choice to default the `slippage_bps` proxy to 0* is
  the separate MINOR item #7; the *absence of bid/ask in the data* is this INFO.)
- **Trailing-stop trigger from same-bar high/low** (`exchange.rs:644-667`). The
  trail ref is raised to `bar.high` (sell) then `bar.low` is checked against the
  derived trigger in one pass — implicitly assuming the high preceded the low. If
  the low came first in real ticks the stop would fire at a tighter trigger or
  rest. Optimistic, **strategy-dependent**; the code comment honestly flags it
  (`exchange.rs:650-651`). Irreducible without tick data.
- **No queue position on a decisive trade-through** (`exchange.rs:822-835`). Even
  a clean trade-through assumes front-of-queue best price; real orders may sit
  behind depth. **Strategy-dependent**; needs order-book data the format lacks.
- **Mark-to-market / equity / unrealized PnL at bar close** (`engine.rs:449-468`,
  `context.rs:440-456`). The close ≈ mid, not the bid you'd actually unwind a long
  at; equity is overstated by ≈ half-spread × notional. Direction-mixed
  (optimistic for longs, pessimistic for shorts); **<1%** liquid. Marking to close
  is the universal convention.
- **Equity curve hides intra-bar drawdown** — sampled only at bar close
  (`engine.rs:293-297`). Max-drawdown analytics understate the true intra-bar
  excursion; a position that nearly stopped out intra-bar but recovered looks
  healthier. **Strategy-dependent**; inherent to bar sampling.
- **Zero latency by default** (`latency_bars=0`, `exchange.rs:948-952`). One bar
  of delay is already baked into the event-ordering contract; sub-bar routing
  latency cannot be modelled at bar resolution anyway. **<1%** on daily/hourly.
- **Liquidation checked at bar.close** (opt-in, `exchange.rs:750-806`). Misses an
  intra-bar threshold breach that recovers by close (optimistic) / fills the
  forced close at close rather than the worse intra-bar extreme. **<1-3%**
  leveraged; opt-in; intra-bar ordering unknowable.
- **Funding settled at bar.close notional, not TWAP mark** (opt-in,
  `exchange.rs:735`). Direction-neutral, **<1%**; OHLC carries no separate mark.

### Conservative / correct behaviours (INFO — safety margins, not fakery)

- **Stops fill at worse-of(trigger, open)** (`exchange.rs:841-846`) — *harsher*
  than fill-at-trigger.
- **Slippage never applied to passive limits** (`exchange.rs:1062-1065`) —
  correct maker/taker asymmetry.
- **Participation cap depletes across orders in a bar** (`exchange.rs:921-924,
  1085-1087`) — conservative, opt-in.
- **Volume-share impact model is opt-in and (linearly) conservative**
  (`exchange.rs:510-523`).
- **Engine event ordering has no look-ahead** (`engine.rs:235-282`).
- **Realized-PnL / average-entry / flip accounting is exact** and property-tested
  (`portfolio.rs:44-95, 417-446`).
- **OCO double-execution is prevented** (`exchange.rs:1006-1015`).
- **`mul_bps` fee truncation** undercharges by ≤1 raw unit/fill
  (`fixed.rs:240-241`) — optimistic in direction but **<1%** and documented;
  effectively negligible.
- **Unrealized PnL is gross of entry fees** (`context.rs:440-456`) — but the
  reported equity curve deducts all fees from cash (`engine.rs:449-469`), and a
  `realized_pnl_net()` accessor exists; **no effect on reported backtest results**.

---

## Bottom line

akadro's simulator is **structurally honest**: correct accounting, no engine
look-ahead, conservative stops, and every realism cost implemented and correct
when enabled. It does **not** fake results in any hidden or systematic way. The
optimism is **opt-out friction concentrated in zero/None defaults** (fees, spread,
participation, cash, funding, short-inventory) plus a few **fixable fill
asymmetries** (limit-touch always-fills, MIT/trailing fill price, OCO tiebreaker).
Honoring the documented `SimulatedExchange::new(specs, fee_bps)` constructor and
enabling the realism knobs the docs already describe collapses the worst-case
default-path error from `>10%` to `<1-2%` for most strategies. The single concrete
correctness defect to fix is the **module-header claim that fills pay a fee by
default**, which the code contradicts.
