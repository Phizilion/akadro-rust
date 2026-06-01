# akadro — Final Bug-Hunt Report (Reconciled)

> Final pre-release verification. Every finding below cites specific
> `file:line` and survived adversarial verification. Findings were deduplicated
> by root cause, re-severitied by true impact on the project invariants
> (kill-feature soundness, backtest↔live parity, fixed-point money correctness,
> panics on real input), and dropped where verifier evidence showed no real
> production bug. The kill-feature itself (compile-time future-data protection)
> was re-confirmed sound; no safe-code in-band leak exists.

---

## (a) Executive summary

A total of **62 distinct confirmed findings** remain after deduplication.
**No critical-severity bug survived** reconciliation: the two findings originally
flagged "critical" (DEX `as i64` truncation and DEX `i64` add overflow) were
downgraded — the DEX connector is an explicitly experimental, simplified v1 with
documented precision limitations, and neither bug touches core/engine/parity or
the supported CEX venues. The kill feature, backtest↔live parity golden-master
path, and fixed-point money discipline in the core/engine are intact.

The most consequential confirmed bugs are **parity violations in the live venue
connectors** (Binance hard-coded `GTC`; MEXC/Binance dropped `post_only`; MEXC WS
`complete:true` on partials; MEXC `EXPIRED`/`FILLED`-after-partial order leaks;
MEXC partial-fill VWAP-vs-marginal price; MEXC stale-timestamp retry) and **stop-limit
same-bar arm+fill optimism** in the backtest exchange. Most are confined to
opt-in features or the live shell (which is itself partly deferred), but they are
real divergences between what a strategy sees in backtest and what it would see
live.

### Counts by severity

| Severity  | Count |
|-----------|-------|
| Critical  | 0     |
| Major     | 17    |
| Minor     | 31    |
| Info      | 14    |
| **Total** | **62** |

---

## (b) Findings by severity

### Critical
*(none)*

### Major

| # | Title | Location | Impact |
|---|-------|----------|--------|
| M1 | Binance spot hard-codes `timeInForce=GTC`, ignoring `order.tif` (and drops `post_only`) | `crates/akadro-venue-binance/src/lib.rs:681-685,696-698` | IOC/FOK limit orders rest as GTC live but expire/kill in backtest — direct parity break |
| M2 | MEXC spot drops `post_only` — maker-only limit sent as taker `LIMIT` | `crates/akadro-venue-mexc/src/client.rs:372-386` | Backtest cancels marketable post-only; live crosses as taker (advertised `Capability::PostOnly` is a false promise) |
| M3 | MEXC WS deal hard-codes `complete:true` — partial fills evict order from open-tracking | `crates/akadro-venue-mexc/src/ws_proto.rs:353` | `ctx.open_order_ids()` diverges WS-live vs REST-live/backtest after a partial fill |
| M4 | MEXC `is_terminal` omits `EXPIRED` — IOC/FOK orders leak in `pending` forever | `crates/akadro-venue-mexc/src/parse.rs:127-133` | No terminal event emitted; per-bar re-poll of dead orders; order-lifecycle parity break |
| M5 | MEXC order leaked in portfolio `open` when final qty already seen under `PARTIALLY_FILLED` | `crates/akadro-venue-mexc/src/client.rs:506-535` | Ghost order in `open_order_ids` permanently after FILLED/zero-delta poll race |
| M6 | MEXC partial-fill price reports cumulative VWAP, not marginal tranche price | `crates/akadro-venue-mexc/src/client.rs:506-523` | Wrong fill price + fee on every partial after the first; corrupts cost basis |
| M7 | MEXC stale signing timestamp on 429/418 retry → venue rejects retries | `crates/akadro-venue-mexc/src/net.rs:74-95` | Retry URL keeps original timestamp; after back-off > recvWindow all retries get error 700003 |
| M8 | Stop-limit same-bar arm+fill uses `bar.open` price-improvement (optimistic) | `crates/akadro-backtest/src/exchange.rs:456-470,640-653` | Fill at a price that predates order activation; inflates backtest PnL |
| M9 | Stop-limit same-bar post-only check uses `bar.open` instead of trigger | `crates/akadro-backtest/src/exchange.rs:463` | Wrongly cancels a valid post-only stop-limit on the arming bar |
| M10 | `reduce_only` does not enforce `qty ≤ |position|` — oversized order flips position | `crates/akadro-backtest/src/exchange.rs:331-338,870-877` | A reduce-only order can increase/flip exposure; wrong PnL & parity |
| M11 | Multiple concurrent `reduce_only` orders collectively exceed position size | `crates/akadro-backtest/src/exchange.rs:331-338` | Shadow not updated per-submit; same-bar reduce-only orders overshoot/flip |
| M12 | StopLimit bypasses `min_notional` check in `rejection()` | `crates/akadro-backtest/src/exchange.rs:323-327` | Tiny stop-limit accepted in backtest, rejected by real venue |
| M13 | `buy_cost_qty` omits volume-share impact → cash can go negative | `crates/akadro-backtest/src/exchange.rs:375-393,409-422,833,877` | Cash-enforcement guarantee violated when `impact_bps>0` + `starting_cash` |
| M14 | `buy_cost_qty` applies slippage to passive limit reservations → false `InsufficientFunds` | `crates/akadro-backtest/src/exchange.rs:379-392,830-833` | Affordable limit buys spuriously rejected when slippage+cash both on |
| M15 | Liquidation does not cancel resting orders → re-entry after forced close | `crates/akadro-backtest/src/exchange.rs:589-628` | Resting order re-opens position next bar with no strategy action |
| M16 | `Manifest::save` not atomic — crash mid-write permanently corrupts the manifest | `crates/akadro-data/src/manifest.rs:54-58` | Truncated JSON unparseable on next load; incremental-fetch state lost |
| M17 | `load_or_cache` returns stale data when cached metadata (instrument/scale) mismatches request | `crates/akadro-data/src/bars.rs:182-186` | Wrong-scale or wrong-instrument bars returned silently (e.g. 100× price error) |

### Minor

| # | Title | Location | Impact |
|---|-------|----------|--------|
| m1 | `post_only` silently ignored on non-Limit kinds in `SimulatedExchange` | `crates/akadro-backtest/src/exchange.rs:439-519` | Market/stop+post_only fills unconditionally; backtest↔live divergence |
| m2 | `OrderRequest::post_only()` callable on any kind with no guard | `crates/akadro-core/src/order.rs:316-321` | Nonsensical order state expressible via public API, no feedback |
| m3 | `settle_cash` uses unprotected i128 arithmetic (not saturating) | `crates/akadro-backtest/src/exchange.rs:296-303,360` | Debug panic / release wrap on extreme notional; inconsistent with `Portfolio` |
| m4 | `Shadow::apply` uses unchecked `i64`/`abs()` for net position | `crates/akadro-backtest/src/exchange.rs:96-98,103,105` | Diverges from `Portfolio::apply_fill` (saturating); panic/wrap at extreme qty |
| m5 | `accrue_funding` multiplies i128 notional by `rate` without saturation | `crates/akadro-backtest/src/exchange.rs:573-574` | Bypasses canonical `Price::notional`+`mul_bps`; i128 overflow at extreme rate |
| m6 | `round_price_up`/`round_price_down` unchecked i64 over/underflow | `crates/akadro-core/src/instrument.rs:207,223` | Panic/wrap near i64::MIN/MAX (no production callers today) |
| m7 | `meets_min_notional` returns true when `min_notional` is negative | `crates/akadro-core/src/instrument.rs:230-233` | Guard silently disabled if a negative threshold is constructed |
| m8 | TrailingStop trigger uses bare i64 arithmetic | `crates/akadro-backtest/src/exchange.rs:489,496,508` | `TrailKind::Percent` with bps>10_000 + large price overflows/wraps |
| m9 | `impact_coef_bps` negative inverts the DEX price-impact model | `crates/akadro-venue-dex/src/lib.rs:92` | Guard checks `==0` not `<0`; trades rewarded for size |
| m10 | DEX `impacted()` `as i64` truncation + unchecked i64 add | `crates/akadro-venue-dex/src/lib.rs:97-98` | Wrong/negative fill price under extreme config (experimental crate) |
| m11 | DEX impact can drive sell fill price negative — no non-negative guard | `crates/akadro-venue-dex/src/lib.rs:98` | Negative `Price` → negative notional → phantom profits (experimental crate) |
| m12 | `funding_interval_bars` is a single global, overwritten per call | `crates/akadro-backtest/src/exchange.rs:246-256,558` | `with_funding_schedule` changes interval for all instruments |
| m13 | MEXC `raw_to_decimal` / Binance `10u128.pow(scale)` unchecked | `crates/akadro-venue-mexc/src/convert.rs:96`, `crates/akadro-venue-binance/src/lib.rs:195` | Panic/wrap for scale≥39 (no real venue, but no `.min(38)` guard) |
| m14 | MEXC `avg_price()` unchecked `as i64` cast | `crates/akadro-venue-mexc/src/parse.rs:152` | Wrong/negative price if quotient > i64::MAX (economically extreme) |
| m15 | Binance signing timestamp uses event-time, no `set_clock_ms`/`sync_time` | `crates/akadro-venue-binance/src/lib.rs:675` | Clock drift / processing lag → API error 1021; weaker than MEXC |
| m16 | MEXC `clock_ms` initialises to 0 and stays 0 until `set_clock_ms` | `crates/akadro-venue-mexc/src/client.rs:295,302-304` | Silent venue rejection if driver forgets the call |
| m17 | MEXC backfill infinite loop when `with_limit(0)` | `crates/akadro-venue-mexc/src/client.rs:165-183` | No floor on limit; empty leading page never advances cursor |
| m18 | MEXC backfill silent data loss on mid-range page error | `crates/akadro-venue-mexc/src/client.rs:187-204` | Partial pages discarded; `next_event` returns None forever |
| m19 | MEXC `min_notional`/`lot_size` silent fallback on unparseable precision | `crates/akadro-venue-mexc/src/instrument.rs:104-119` | Non-empty unparseable field → guard disabled (`unwrap_or(0)`/`(1)`) |
| m20 | MEXC futures bars stamped at open-time vs close-time everywhere else | `crates/akadro-venue-mexc/src/futures.rs:250-296` | Cross-instrument event-time skew; equity-curve ts off by one interval |
| m21 | `Ctx::equity()` docstring claims wrong equivalence | `crates/akadro-engine/src/context.rs:425-428` | Misleads strategy authors; off by `avg_entry*net_qty` |
| m22 | Timer-submitted orders not routed between same-bar timer firings | `crates/akadro-engine/src/engine.rs:250-281` | `has_open_order` blind to first timer's order → double-submission |
| m23 | `OrderGate.submitted` grows unbounded; O(n) `has_open_order` scan | `crates/akadro-engine/src/context.rs:55-88,456-478` | Super-linear per-bar cost in long live sessions (no eviction) |
| m24 | `Event::Resync` instrument scope silently dropped | `crates/akadro-engine/src/engine.rs:290-302` | Per-instrument resync becomes global; over-reconciliation (live shell) |
| m25 | Weighted-average rounding wrong when `total_cost` negative | `crates/akadro-engine/src/portfolio.rs:67` | Rounding bias flips for negative prices (unreachable today) |
| m26 | TradeStats `Position::apply` divide-by-zero on zero-qty fill | `crates/akadro-analytics/src/trades.rs:82` | Panic on `from_fills` with a zero-qty `FillRecord` (public API) |
| m27 | TradeStats `Position::apply` unchecked i64 mul/add/abs | `crates/akadro-analytics/src/trades.rs:72,83,86,89` | Diverges from engine safety patterns; wrong stats at extreme qty |
| m28 | `PerformanceReport` NaN/Inf when `periods_per_year ≤ 0` | `crates/akadro-analytics/src/equity.rs:118-125` | Returns `Some(NaN…)` instead of `None` for invalid annualization |
| m29 | skewness/kurtosis labeled "Sample" but are population estimators | `crates/akadro-analytics/src/distribution.rs:91-108` | PSR/DSR overestimate probability for small n |
| m30 | `walk_forward_efficiency` inverts semantics for negative IS | `crates/akadro-analytics/src/distribution.rs:196-210` | Documented `<1`/`>1` convention violated; misranks strategies |
| m31 | RSI / MACD / SMA / EMA / Bollinger indicator i64 overflow & warm-up gaps | `crates/akadro-indicators/src/*` | See detailed §m31 cluster (warm-up off-by-one, sum/level overflow) |

### Info

| # | Title | Location | Impact |
|---|-------|----------|--------|
| i1 | `total_fees` can go negative when funding received; doc says "charged" | `crates/akadro-engine/src/portfolio.rs:187`, `engine.rs:82-83` | Signed net misnamed; diverges from `TradeStats::total_fees` |
| i2 | `realized_pnl` includes liquidation PnL but fills log does not | `crates/akadro-engine/src/engine.rs:81`, `analytics/src/trades.rs:7-18` | `TradeStats::from_fills` under-counts after liquidation; undocumented |
| i3 | `diff_reports` false-match/mismatch on funding/zero-fee-liquidation | `crates/akadro-backtest/src/replay.rs:239-247,312-319` | Oracle fidelity gaps + wrong docstring (funding ≠ realized_pnl) |
| i4 | `SimulatedExchange::config_seed()` always `Some(0)` | `crates/akadro-backtest/src/exchange.rs:722-724` | `RunReport.seed` ambiguous; seed never consumed by any model |
| i5 | `DeterministicRng` seeded but never consumed; `FillConfig.seed` dead | `crates/akadro-backtest/src/exchange.rs:61` | Unimplemented API promise (documented residual) |
| i6 | Six new Strategy hooks lack per-hook compile-fail brand proofs | `crates/akadro-compile-tests/tests/ui/` | Regression-lock gap (brand still sound); AGENTS.md stale |
| i7 | AGENTS.md compile-fail count stale (20 vs 21) + stale crate map | `AGENTS.md:176,282-301` | Doc accounting off |
| i8 | `LastN` Debug exposes full history slice | `crates/akadro-engine/src/series.rs:114` | Info-leak in debug output (historical only, not future) |
| i9 | `Brand` redundant on `Ctx` (defense-in-depth) | `crates/akadro-engine/src/context.rs:139-146` | Not a bug; document why `_brand` must stay |
| i10 | `ExecutionClient::submit`/`cancel` doc contracts incomplete | `crates/akadro-core/src/traits.rs:54,69-76` | New venue authors could leave order in limbo / hang strategy |
| i11 | `DataError::Io` uses `#[from]` (foreign-error policy deviation) | `crates/akadro-data/src/bars.rs:33` | Only `#[from]` in workspace; stdlib type (low risk) |
| i12 | Arrow Timestamp tz annotation not validated on read | `crates/akadro-data/src/bars.rs:287-293` | tz-naive external file silently accepted (numerically OK) |
| i13 | Live-shell robustness gaps (reconnect/listen-key/shutdown/silent-fail) | `crates/akadro-live/*` | See §i13 cluster (deferred subsystem) |
| i14 | `Capability::bit()` shifts by enum discriminant; `ratatui` off-workspace; TUI qty scale; prelude omissions; `display()` scale>38; mul_bps doc | misc | Latent/ergonomic notes (see §Info detail) |

---

## (c) Detailed confirmed findings

### M1 — Binance spot hard-codes `timeInForce=GTC` and drops `post_only`
**Where:** `crates/akadro-venue-binance/src/lib.rs:681-685` (TIF), `:696-698` (no `post_only` branch).
**What/why:** `BinanceSpotExec::submit` formats every Limit order with the literal
`timeInForce=GTC`; `order.tif` is never read. A `Limit+IOC` order is correctly
expired in backtest (`exchange.rs:790,820`) but rests as GTC on Binance; a
`Limit+FOK` is killed in backtest (`exchange.rs:814`) but rests live. `order.post_only`
has zero branches, so a marketable post-only limit (cancelled in backtest at
`exchange.rs:444,463`) fires as a plain taker GTC limit live. MEXC encodes all
three TIF variants (`client.rs:376-382`), proving the missing pattern.
**Fix:** Map `order.tif → "GTC"|"IOC"|"FOK"`; for `post_only`, emit `LIMIT_MAKER`
or locally reject, mirroring `futures.rs:61`.
**Evidence:** Verifier confirmed `order.tif`/`order.post_only` never referenced in `submit()`; `TimeInForce::{Ioc,Fok}` and `with_tif()` are public.

### M2 — MEXC spot drops `post_only` (LIMIT instead of LIMIT_MAKER)
**Where:** `crates/akadro-venue-mexc/src/client.rs:372-386`.
**What/why:** `order_type()` checks `reduce_only` (line 373) but never `post_only`.
A `Limit+GTC` post-only order matches line 378 → `("LIMIT", Some(limit))` and is
sent as a taker-eligible limit. Backtest cancels a marketable post-only
(`exchange.rs:444`). The catalog advertises `Capability::PostOnly`
(`instrument.rs:130-131`), making this a false promise. The futures connector
handles it correctly (`futures.rs:61`).
**Fix:** Add `if order.post_only { return Some(("LIMIT_MAKER", Some(limit))); }`
for limit kinds (reject for non-limit), before the existing match.
**Evidence:** Project research artifact `mexc.json` mandates "post-only → LIMIT_MAKER".

### M3 — MEXC WS `WsDeal::to_fill` hard-codes `complete:true`
**Where:** `crates/akadro-venue-mexc/src/ws_proto.rs:353`; consumed at
`crates/akadro-live/src/mexc.rs:176-187`.
**What/why:** Every private-deal frame emits `Fill{complete:true}`. `Portfolio::apply`
calls `close_order` on `complete==true` (`portfolio.rs:158-159`), so a partial
fill immediately evicts the order from `open_order_ids`. The REST path correctly
uses `complete: status=="FILLED"` (`client.rs:519`); Binance WS uses
`complete: status=="FILLED"` (`ws.rs:264`). The WS comment "tracked elsewhere" is
false — there is no follow-up mechanism.
**Fix:** Track cumulative-filled vs order-qty in the WS session and set `complete`
only when fully filled, or always emit `complete:false` and close on a separate
terminal status event.

### M4 — MEXC `is_terminal` omits `EXPIRED`
**Where:** `crates/akadro-venue-mexc/src/parse.rs:127-133`; observe loop
`client.rs:525-534`.
**What/why:** IOC/FOK orders map to `IMMEDIATE_OR_CANCEL`/`FILL_OR_KILL`
(`client.rs:379-382`) and MEXC returns `EXPIRED` when they cannot fill.
`is_terminal()` matches only `FILLED|CANCELED|PARTIALLY_CANCELED`, so `EXPIRED`
orders are pushed back into `pending` indefinitely, re-polled every bar, and the
strategy never receives a terminal event. A workspace grep for `"EXPIRED"`
returns zero hits.
**Fix:** Add `"EXPIRED"` to `is_terminal()` and emit `OrderCanceled`
(`CancelReason::VenueCanceled`) in the observe cancel branch.

### M5 — MEXC order leak when final qty seen under `PARTIALLY_FILLED`
**Where:** `crates/akadro-venue-mexc/src/client.rs:506-535`.
**What/why:** Poll 1 returns `PARTIALLY_FILLED` with full qty → `Fill{complete:false}`,
`prev_filled=N`, stays pending. Poll 2 returns `FILLED` with same qty → the
`filled > prev_filled` guard fails (no fill, no `complete:true`), the order is
dropped from `pending` but the CANCELED branch is not taken, so `close_order` is
never called. The id stays in `Portfolio::open` forever (ghost order).
**Fix:** When a terminal non-cancel status is reached with no delta, emit a
`complete:true` zero-qty fill (or a terminal lifecycle event) so `close_order` fires.

### M6 — MEXC partial-fill price = cumulative VWAP, not marginal tranche
**Where:** `crates/akadro-venue-mexc/src/client.rs:506-523`; `parse.rs:136-153`.
**What/why:** `Pending` stores `prev_filled` (qty) but no `prev_quote`, so the
incremental tranche price cannot be computed. Each delta fill uses
`avg_price()` = cumulative `cummulativeQuote/executedQty`. For tranches at
differing prices the reported price (and fee) is wrong for every fill after the
first. The test `observe_partial_then_full_fill` uses equal per-tranche prices,
masking the bug.
**Fix:** Add `prev_quote: i128` to `Pending`; compute
`marginal = (cumQ - prev_quote)*10^qty_scale / (filled - prev_filled)`; update both.

### M7 — MEXC stale signing timestamp on 429/418 retry
**Where:** `crates/akadro-venue-mexc/src/net.rs:74-95`; URL built at `client.rs:444`.
**What/why:** `ReqwestTransport::send` retries the identical pre-signed
`HttpRequest` after sleeping. The timestamp is baked into the URL. Cumulative 429
back-off (1+2+3 = 6 s) exceeds `recvWindow=5000` (`client.rs:292`); 418 sleeps 60 s
first. Every retry is then rejected with error 700003. Reachable in `observe()`
which polls once per pending order per bar.
**Fix:** Rebuild and re-sign the request inside the retry loop, or surface 429/418
to the caller to re-sign, or cap the signed-request sleep below `recvWindow`.

### M8 — Stop-limit same-bar arm+fill uses `bar.open` price-improvement
**Where:** `crates/akadro-backtest/src/exchange.rs:456-470`; `limit_fill` at `:640-653`.
**What/why:** When a stop-limit arms (`stop_triggered`) and fills in the same
`evaluate`, `limit_fill` is called with the full bar including `bar.open`. But the
order was not active at the open — it armed mid-bar when the high/low touched the
trigger. Buy example: trigger=110, limit=115, bar(open=100,high=125,low=103) →
fills at `min(open=100,limit=115)=100`, 15 ticks better than the limit and below
the trigger. Inflates backtest PnL.
**Fix:** For same-bar arming (`was_armed` is already computed at `:767`), use the
trigger as the price-improvement reference (or fill at the limit price directly).

### M9 — Stop-limit same-bar post-only check uses `bar.open`
**Where:** `crates/akadro-backtest/src/exchange.rs:463`.
**What/why:** For a just-armed stop-limit, the post-only marketability test uses
`bar.open`. Buy post-only trigger=110, limit=109, bar(open=108,high=115): arms,
then `marketable(Buy,109,open=108)`=true → wrongly cancelled. At the arming moment
(price≈110) limit 109 < trigger 110 is non-marketable and should rest.
**Fix:** For same-bar arming, use `trigger` (not `bar.open`) as the marketability reference.

### M10 — `reduce_only` does not enforce `qty ≤ |position|`
**Where:** `crates/akadro-backtest/src/exchange.rs:331-338` (check), `:870-877` (fill).
**What/why:** The guard checks only direction (`net.signum() != side.sign()`), not
size. A reduce-only buy of qty=10 against net=-5 passes, fills, and
`Shadow::apply` yields net=+5 — a flip, not a reduction. No fill-time cap exists.
**Fix:** Reject if `order.qty.raw() > net.abs()`, and cap the fill qty at `|net|`.

### M11 — Concurrent `reduce_only` orders collectively overshoot
**Where:** `crates/akadro-backtest/src/exchange.rs:331-338`.
**What/why:** `shadow` is updated only during `observe()` fills, never at `submit()`,
so two same-bar reduce-only buy(3) orders against net=-5 both pass, both fill,
net=+1. The cash guard correctly accumulates `reserved` over resting orders
(`:353-359`) but there is no analogous accumulation for reduce-only qty.
**Fix:** In the reduce-only check, add the unfilled qty of already-resting
reduce-only orders on the same side/instrument before deciding.

### M12 — StopLimit bypasses `min_notional`
**Where:** `crates/akadro-backtest/src/exchange.rs:323-327`.
**What/why:** `if let OrderKind::Limit { limit } = order.kind` matches only plain
Limit. StopLimit (which carries `limit: Price`, `order.rs:92-99`) skips the check;
`buy_cost_qty` at `:380` already handles both with `Limit{..}|StopLimit{..}`,
showing the oversight is localized.
**Fix:** Extend the pattern: `OrderKind::Limit { limit } | OrderKind::StopLimit { limit, .. }`.

### M13 — `buy_cost_qty` omits volume-share impact (cash can go negative)
**Where:** `crates/akadro-backtest/src/exchange.rs:375-393,409-422,833,877`.
**What/why:** The cash reservation uses only flat slippage (`slipped`), but the
actual fill (`fill_price`) adds `impact_bps × participation`. `settle_cash` debits
the full impacted price, so with `impact_bps>0` + `starting_cash` the guard
under-reserves and cash goes negative, violating the documented "never spend quote
it does not have." The accepted-residual comment at `:344-345` covers only gap-ups,
not impact.
**Fix:** Add a worst-case (100% participation) impact term to `buy_cost_qty`, or
share a single cost-estimation helper between the guard and `fill_price`.

### M14 — `buy_cost_qty` applies slippage to passive limit reservations
**Where:** `crates/akadro-backtest/src/exchange.rs:379-392`; fill at `:830-833`.
**What/why:** For Limit/StopLimit the reservation uses `slipped(limit)`, but the
fill uses bare `base` (no slippage). The over-reservation
(`limit × slippage_bps/10_000 × qty`) causes spurious `InsufficientFunds`
rejections of genuinely affordable orders when slippage+cash are both enabled.
**Fix:** Do not apply slippage to Limit/StopLimit kinds in `buy_cost_qty`; only
liquidity-taking kinds pay slippage.

### M15 — Liquidation does not cancel resting orders
**Where:** `crates/akadro-backtest/src/exchange.rs:589-628`.
**What/why:** `check_liquidation` closes the position and updates `shadow` but
leaves resting orders in `self.resting`. On the next bar a resting limit/stop can
fill and silently re-open a position the strategy did not request. Real venues
auto-cancel on liquidation. (Opt-in via `liquidation_bps`.)
**Fix:** On liquidation, emit `OrderCanceled{VenueCanceled}` for every resting
order on the instrument and remove them from `self.resting`.

### M16 — `Manifest::save` not atomic
**Where:** `crates/akadro-data/src/manifest.rs:54-58`.
**What/why:** Uses `std::fs::write` (truncate-then-fill). A crash mid-write leaves
truncated JSON; the next `load()` (`:42-48`) hits `serde_json::from_str` →
`DataError::Schema` with no recovery, clearing incremental-fetch state. The bars
writer already uses the temp-file+rename atomic pattern (`bars.rs:102-127`).
**Fix:** Serialize to a temp path, then `rename` (mirror `write_partition`).

### M17 — `load_or_cache` returns stale data on metadata mismatch
**Where:** `crates/akadro-data/src/bars.rs:182-186`.
**What/why:** On a cache hit it returns `read_partition(path)?.bars` and discards
the file's `instrument`/`price_scale`/`qty_scale` without comparing them to the
caller's args. A venue precision change (e.g. `price_scale` 2→4) yields bars whose
raw integers are interpreted at the wrong scale (100× price error). `Bar` carries
no embedded scale, so the corruption is silent. `load_or_cache_many` inherits it.
**Fix:** After the cache-hit read, return `DataError::Schema` if
`loaded.instrument/price_scale/qty_scale` differ from the requested values.

---

### m1 — `post_only` ignored on non-Limit kinds in backtest
**Where:** `crates/akadro-backtest/src/exchange.rs:439-519` (Market `:442`, Stop/MIT/TrailingStop arms); `rejection()` `:316-365` has no guard.
**What/why:** `post_only` is consulted only for Limit (`:444`) and StopLimit (`:463`).
`OrderRequest::market(...).post_only()` fills unconditionally — backtest↔live
divergence (MEXC futures rejects this combination). **Fix:** reject
`post_only && !matches!(kind, Limit|StopLimit)` in `rejection()`.

### m2 — `post_only()` builder unrestricted
**Where:** `crates/akadro-core/src/order.rs:316-321`.
**What/why:** Settable on any kind with no compile/runtime guard; combined with m1
a user gets no feedback for an invalid order. **Fix:** document it's Limit/StopLimit
only and/or `debug_assert`; the `rejection()` guard in m1 also covers it.

### m3 — `settle_cash` non-saturating i128 arithmetic
**Where:** `crates/akadro-backtest/src/exchange.rs:296-303,360`.
**What/why:** Plain `-=`/`+=` on raw i128 cash and `cash - reserved` at `:360`,
unlike `Portfolio::settle` which uses `saturating_*` (`portfolio.rs:186,215-216`).
Debug panic / release wrap on extreme notional. **Fix:** use `saturating_add/sub`.

### m4 — `Shadow::apply` unchecked `i64`/`abs()`
**Where:** `crates/akadro-backtest/src/exchange.rs:96-98,103,105`.
**What/why:** `i128::from(pos.abs())` aborts before widening (vs `i128::from(pos).abs()`
in `portfolio.rs:62-63`), and `pos + signed` is plain (vs `saturating_add`).
`signed.abs()` is safe (qty>0 enforced) but `pos.abs()` can hit `i64::MIN` if the
plain add wraps. **Fix:** widen-before-abs and use `saturating_add`, matching the engine.

### m5 — `accrue_funding` non-canonical i128 multiply
**Where:** `crates/akadro-backtest/src/exchange.rs:573-574`.
**What/why:** `notional(i128) * rate` bypasses `Price::notional`+`Money::mul_bps`
(which saturate) and can overflow i128 at extreme `funding_bps`. **Fix:** route via
`Price::notional` + `Money::mul_bps`.

### m6 — `round_price_up`/`round_price_down` over/underflow
**Where:** `crates/akadro-core/src/instrument.rs:223,207`.
**What/why:** `raw + (tick - rem)` and `raw - raw.rem_euclid(tick)` are unchecked;
overflow near i64::MAX / underflow near i64::MIN. No production callers today; no
`# Panics` doc. **Fix:** `checked_add`/`checked_sub` returning `price` unchanged on overflow.

### m7 — `meets_min_notional` true for negative `min_notional`
**Where:** `crates/akadro-core/src/instrument.rs:230-233`.
**What/why:** LHS is `.abs()` (≥0); if `min_notional<0` the guard is always true.
`InstrumentSpec::new` is an unvalidated `const fn`. Real connectors clamp to 0/positive.
**Fix:** `debug_assert!(min_notional.raw() >= 0)` or `.max(0)` in the compare.

### m8 — TrailingStop trigger bare i64 arithmetic
**Where:** `crates/akadro-backtest/src/exchange.rs:489,496,508`.
**What/why:** `TrailKind::Percent(bps)` is an unconstrained `u32`; with bps>10_000
and a large price, `r0 ± offset` overflows. **Fix:** `saturating_sub`/`saturating_add`;
document bps>10_000 is meaningless.

### m9 — Negative `impact_coef_bps` inverts DEX impact
**Where:** `crates/akadro-venue-dex/src/lib.rs:92`.
**What/why:** Guard is `impact_coef_bps == 0`, not `<= 0`. Negative coef gives buys a
discount and sells a premium — the opposite of impact. Same latent issue for
negative `lp_fee_bps`. **Fix:** change the fields to `u64` or validate in `new`/`impacted`.

### m10 — DEX `impacted()` `as i64` truncation + unchecked add
**Where:** `crates/akadro-venue-dex/src/lib.rs:97-98`.
**What/why:** `adj = (...i128...) as i64` wraps if > i64::MAX; then
`mid.raw() + side.sign()*adj` is unchecked i64 (panic debug / wrap release).
Reachable under extreme `DexConfig`. Experimental crate (DEX precision is documented
future work). **Fix:** clamp to `[i64::MIN,i64::MAX]` and `saturating_add`; keep math in i128.

### m11 — DEX sell impact can drive price negative
**Where:** `crates/akadro-venue-dex/src/lib.rs:98`.
**What/why:** For a sell, `mid - adj` can go below zero when impact > 100%
(`impact_coef_bps=10_000`, `depth=1`). `Price::from_raw` accepts negatives →
negative notional → phantom profits. **Fix:** clamp impacted price to `≥ 1` (or reject).

### m12 — `funding_interval_bars` global overwrite
**Where:** `crates/akadro-backtest/src/exchange.rs:246-256,558`.
**What/why:** `with_funding_schedule` unconditionally writes the single global
`funding_interval_bars` (`:252`), so a per-instrument schedule changes the interval
for all instruments. **Fix:** store interval per instrument in a map; look it up in
`accrue_funding`.

### m13 — Unchecked `10u128.pow(scale)` in connectors
**Where:** `crates/akadro-venue-mexc/src/convert.rs:96`, `crates/akadro-venue-binance/src/lib.rs:195`.
**What/why:** For scale≥39, `10u128.pow` overflows (panic debug / wrong divisor
release). `decimal_to_raw` already uses `checked_pow`, and `fixed.rs:284` already
clamps to `.min(38)` — the guard was not propagated to `raw_to_decimal`. No real
venue uses scale≥39. **Fix:** `let scale = scale.min(38);`.

### m14 — MEXC `avg_price()` unchecked `as i64`
**Where:** `crates/akadro-venue-mexc/src/parse.rs:152`.
**What/why:** `(scaled / q) as i64` wraps if the quotient exceeds i64::MAX
(`scaled = cumQ × 10^qty_scale` can reach ~10^26). Economically extreme but
unsound. **Fix:** `i64::try_from(scaled / i128::from(q)).ok().map(Price::from_raw)`.

### m15 — Binance signing uses event-time, no clock sync
**Where:** `crates/akadro-venue-binance/src/lib.rs:675`.
**What/why:** `ts_ms = now.as_nanos()/1e6` uses logical event-time; there is no
`set_clock_ms`/`sync_time` like MEXC. Clock drift or per-bar processing >recvWindow
→ API error 1021 and silent order-lifecycle divergence (live shell deferred).
**Fix:** add `clock_ms` + `set_clock_ms` + `sync_time` mirroring MEXC.

### m16 — MEXC `clock_ms` starts at 0
**Where:** `crates/akadro-venue-mexc/src/client.rs:295,302-304`.
**What/why:** If a driver never calls `set_clock_ms`, `sign_clock()` returns 0
(epoch 0) → every signed request rejected (700003), while `SimulatedExchange`
accepts. **Fix:** init `clock_ms` to wall-clock in `new`, or assert/`Option`-guard
on first use.

### m17 — MEXC backfill infinite loop with `with_limit(0)`
**Where:** `crates/akadro-venue-mexc/src/client.rs:165-183`.
**What/why:** `with_limit(0)` (no floor) makes the leading-gap cursor advance by 0,
so the loop re-requests forever. **Fix:** `self.limit = limit.clamp(1, MAX_KLINES_LIMIT)`.

### m18 — MEXC backfill silent data loss on mid-range error
**Where:** `crates/akadro-venue-mexc/src/client.rs:187-204`.
**What/why:** A page error `?`-propagates and discards all already-fetched pages;
`next_event` set `fetched=true` first, so subsequent calls return None forever — a
transient hiccup yields zero bars. **Fix:** buffer successful pages before the error
(extend `self.buffer` per page), or restructure to surface partial results.

### m19 — MEXC silent fallback on unparseable precision fields
**Where:** `crates/akadro-venue-mexc/src/instrument.rs:104-119`.
**What/why:** Non-empty but unparseable `quoteAmountPrecision`/`baseSizePrecision`
falls back to `min_notional=0` / `lot=1` via `unwrap_or`, silently disabling the
local guard (venue then rejects undersized orders). **Fix:** `?`-propagate the parse
error as `MexcError::Parse`, or log + use a conservative non-zero default.

### m20 — MEXC futures bars stamped at open-time
**Where:** `crates/akadro-venue-mexc/src/futures.rs:250-296`.
**What/why:** Futures REST bars use `time` (open, epoch-s) while spot REST
(`parse.rs:45-52`, closeTime), spot WS, and `HistoricalFeed` use close-time.
`ctx.now()` and the equity-curve ts then differ by one interval across instruments
when spot+futures are mixed. **Fix:** stamp at `(open + interval_secs) * 1e9`
(use `futures_interval_secs`), and adjust the pagination cursor.

### m21 — `Ctx::equity()` docstring wrong
**Where:** `crates/akadro-engine/src/context.rs:425-428`.
**What/why:** Claims `equity = cash + realized + Σ unrealized` for a flat start, but
the implementation (correct) computes `cash + Σ(net × mark)`; the two differ by
`avg_entry × net_qty` because realized is already reflected in cash. **Fix:** correct
the docstring (note this also overlaps the m25/order-money-flow rounding nuance).

### m22 — Same-bar timer orders invisible to `has_open_order`
**Where:** `crates/akadro-engine/src/engine.rs:250-281`.
**What/why:** All due timers fire before `route_pending` (`:271`), so a second
timer's `ctx.has_open_order()` cannot see the first timer's order (it's in
`gate.pending`, not yet `portfolio.open`), though `submitted_order` can. Risk:
double-submission. **Fix:** document the ordering on `on_timer`, or have
`has_open_order` also consult `gate.submitted`.

### m23 — `OrderGate.submitted` unbounded growth
**Where:** `crates/akadro-engine/src/context.rs:55-88,456-478`.
**What/why:** Every submitted order is pushed and never evicted; `submitted_order`
is a linear scan, so `has_open_order`/`open_order_ids` are O(|open|×|total|) — a
super-linear per-bar cost in long live sessions. `portfolio.open` is pruned but
`gate.submitted` is not. **Fix:** `HashMap<ClientOrderId, PlacedOrder>` with eviction
on terminal events.

### m24 — `Event::Resync` instrument scope dropped
**Where:** `crates/akadro-engine/src/engine.rs:290-302`.
**What/why:** `Event::Resync { ts, .. }` discards `instrument`; `AccountEvent::Resync`
has no instrument slot. Per-instrument resync becomes a global one (live shell).
**Fix:** add `instrument: Option<InstrumentId>` to `AccountEvent::Resync`
(`#[non_exhaustive]`, additive) and forward it.

### m25 — Weighted-average rounding wrong for negative `total_cost`
**Where:** `crates/akadro-engine/src/portfolio.rs:67`.
**What/why:** `(total_cost + total_qty/2)/total_qty` is round-half-up only for
non-negative inputs; i128 division truncates toward zero for negatives, flipping the
bias. Unreachable today (all real prices positive). **Fix:** `div_euclid`/`rem_euclid`
round-to-nearest, or `debug_assert!(price.raw() > 0)`.

### m26 — TradeStats divide-by-zero on zero-qty fill
**Where:** `crates/akadro-analytics/src/trades.rs:82`.
**What/why:** With `pos=0` and `qty=0`, `total_qty=0` → integer divide-by-zero
panic. The engine guards this (`portfolio.rs:51-53: if signed == 0 { return }`);
`from_fills` (public API) does not validate inputs. **Fix:** early-return when `qty==0`.

### m27 — TradeStats `Position::apply` unchecked i64
**Where:** `crates/akadro-analytics/src/trades.rs:72,83,86,89`.
**What/why:** Plain `side_sign*qty`, `pos+signed`, `signed.abs().min(pos.abs())`
diverge from `portfolio.rs` safety (`saturating_mul`, `i128::from().abs()`,
`saturating_add`). Wrong analytics stats / panic at extreme qty (analytics only).
**Fix:** mirror the engine's saturating/widen-before-abs patterns.

### m28 — `PerformanceReport` NaN/Inf when `periods_per_year ≤ 0`
**Where:** `crates/akadro-analytics/src/equity.rs:118-125`.
**What/why:** `ann = periods_per_year.sqrt()` is NaN for negatives → `Some(NaN…)`
for volatility/sharpe/sortino; `0.0` with a 2-point curve → `INF * 0 = NaN`. All
current callers pass positive values. **Fix:** `if periods_per_year <= 0.0 || !finite { return None; }`.

### m29 — skewness/kurtosis labeled "Sample" but population
**Where:** `crates/akadro-analytics/src/distribution.rs:91-108`.
**What/why:** `moments()` divides by `n` (population), so PSR/DSR
(`probabilistic_sharpe` `:140-155`) overestimate probability for small n (no
`sqrt(n(n-1))/(n-2)` correction). The earlier ddof fix covered `per_period_sharpe`
only. **Fix:** rename to population_* and document the small-n PSR bias, or apply
the bias correction.

### m30 — `walk_forward_efficiency` inverts for negative IS
**Where:** `crates/akadro-analytics/src/distribution.rs:196-210`.
**What/why:** Two branches compute identical `oos/is_`; the documented `<1`/`>1`
convention inverts when IS<0 (e.g. `WFE(0.05,-0.10)=-0.5` looks bad though OOS
improved). **Fix:** document the inversion and collapse the dead branch.

### m31 — Indicator overflow & warm-up cluster
**Where:** `crates/akadro-indicators/src/{oscillator,average,channel,range}.rs`.
Confirmed sub-findings:
- **RSI warm-up off-by-one** (`oscillator.rs:58-79`): `Rsi::new(p)` needs `p+1` calls;
  `ctx.is_warmed_up(inst,p)` is true at `p`, so the first signal is silently None →
  trading starts one bar late. **Fix:** add `warm_up_bars() = period+1` and document.
- **MACD warm-up** (`average.rs:150-167`): first Some on `slow+signal-1`, not `slow`;
  `is_warmed_up(inst, slow)` is insufficient (8 missed bars for 12/26/9). **Fix:** doc/`warm_up_bars()`.
- **RSI `level()` i64 overflow** (`oscillator.rs:50,82`): `10_000 * avg_gain` and
  `avg_gain * (p-1)` are bare i64. **Fix:** widen to i128.
- **SMA/Bollinger `sum: i64` overflow** (`average.rs:16,40`; `channel.rs:29,54`):
  unbounded accumulation; Bollinger already uses i128 for the variance accumulator,
  showing the inconsistency. **Fix:** i128 accumulator.
- **Bollinger variance i64 sub before widen** (`channel.rs:69`): `i128::from(x - mid)`
  subtracts in i64. **Fix:** `(i128::from(x) - i128::from(mid))`. (Unreachable for positive prices.)
- **Bollinger negative `k` inverts bands** (`channel.rs:38-46`): only `window>0`
  asserted; `k<0` yields `upper<lower`. **Fix:** `assert!(k>0)` or `k: usize`.
- **ATR no `high>=low` guard** (`range.rs:40-65`): inverted bar gives negative seed TR.
  **Fix:** `debug_assert!(high>=low)`.
- *(Bollinger truncated-mean variance bias and the SMA/Bollinger `with_capacity(window)`
  off-by-one realloc are info-level precision/perf notes.)*

---

## (c-info) Info-level confirmed findings (detail)

- **i1 — `total_fees` signed, doc says "charged":** `portfolio.rs:185-187` adds signed
  `cost.amount`; received funding makes `total_fees` negative. `engine.rs:82-83`'s
  "charged" implies non-negative. Diverges from `TradeStats::total_fees` (fills-only,
  ≥0). **Fix:** rename to `net_costs` (signed) or split `trading_fees` + `funding_net`.
- **i2 — `realized_pnl` includes liquidation but fills log doesn't:** `engine.rs:81`,
  `portfolio.rs:167-178` (liquidation calls `settle` → `realized` but pushes no
  `FillRecord`). `TradeStats::from_fills` under-counts. **Fix:** document; optionally add
  `liquidation_pnl` to `RunReport`.
- **i3 — `diff_reports` oracle fidelity:** `replay.rs:239-247,312-319`. (a) docstring
  falsely says funding lands in `realized_pnl` (it only touches cash/fees); (b)
  funded-but-not-liquidated runs wrongly skip the `realized_pnl` compare (false
  match); (c) zero-fee liquidation (and funding-cancels-liquidation-fee) trigger a
  false `realized_pnl` mismatch. **Fix:** gate the skip on liquidation presence (e.g. a
  `RunReport.has_liquidation` flag), not on the fee total; fix the docstring.
- **i4 — `config_seed()` always `Some(0)`:** `exchange.rs:722-724`; trait default is
  `None` (`traits.rs:87-88`). `RunReport.seed` cannot distinguish "seed 0" from "no
  RNG." **Fix:** return `None` unless an RNG was actually drawn.
- **i5 — `FillConfig.seed` dead:** `exchange.rs:61`; `DeterministicRng::seeded` never
  called in production. Documented residual. **Fix:** wire it to a model or drop the field.
- **i6 — Six new hooks lack brand proofs:** `strategy.rs:55-73`; `tests/ui/` covers only
  on_start/on_bar/on_account/on_stop. Brand is sound today (all use `&mut Ctx<'_>`), but
  the regression-lock is absent for on_fill/on_order_rejected/on_order_canceled/
  on_liquidation/on_funding/on_timer. **Fix:** add `stash_view_in_on_*.rs` per hook;
  update AGENTS.md §4/§12.
- **i7 — AGENTS.md stale:** §4/§12 say "20 cases" but 21 exist
  (`stash_signal_across_bars.rs`); §8 crate-map lists 8 of 19 members. **Fix:** update docs.
- **i8 — `LastN` Debug exposes full slice:** `series.rs:114` derives Debug printing
  `data: &[T]`; `Series` has a manual Debug hiding the slice. Historical only — not a
  kill-feature breach. **Fix:** manual Debug printing only `remaining`.
- **i9 — `Brand` redundant on `Ctx`:** `context.rs:139-146` — `gate: &'bar mut` already
  forces invariance; `_brand` is sound defense-in-depth (and is the *sole* invariance
  source on `MarketView`). Not a bug. **Fix:** add a comment so a refactor doesn't drop it.
- **i10 — Trait doc contracts incomplete:** `traits.rs:54` (submit must emit exactly one
  ack/reject, even on transport error, else id is in limbo) and `:69-76` (cancel should
  emit `OrderCancelRejected`, not "nothing", for unknown/terminal — `SimulatedExchange`
  does this). **Fix:** strengthen both docs.
- **i11 — `DataError::Io` uses `#[from]`:** `bars.rs:33` — only `#[from]` in the workspace;
  policy (§7) says wrap foreign errors. `std::io::Error` is stdlib (low risk). **Fix:** wrap
  as `Io(String)` (loses `ErrorKind` — weigh the trade-off) or document the exception.
- **i12 — Arrow tz annotation unvalidated:** `bars.rs:287-293` — `Timestamp(Nanosecond, None)`
  downcasts the same as UTC; numerically harmless for akadro's own files (gated by format
  version). **Fix:** check `data_type()` is `Timestamp(Nanosecond, Some("UTC"))`.
- **i13 — Live-shell robustness cluster** (`crates/akadro-live/*`, deferred subsystem):
  - Binance user-data: no listen-key keepalive → silent fill loss after 60 min
    (`binance.rs:118-156`). **Major within the live shell, but the live shell is partly
    deferred** — listed here as info because the subsystem is not yet release-critical.
  - Binance `run_klines`/`run_user_data`: no reconnect loop and silent return on initial
    connect failure / runtime-build failure (`binance.rs:88-156`).
  - `ChannelExec` unbounded fills channel (`channel_exec.rs:63-65`) vs bounded market
    bridge.
  - MEXC `interval`-first-tick PING-before-data (`mexc.rs:104-110`); silent-ignore of WS
    error/subscribe-NAK Text frames (`binance.rs:101-109`, `mexc.rs:112-124`); no graceful
    shutdown for klines threads (`binance.rs:88-95`, `mexc.rs:81-88`).
  - `run_paper` ignores `initial_cash` for the exchange cash guard and accepts only
    `fee_bps` (no `FillConfig`) (`paper.rs:19-27`).
  - MEXC 418 retry sleeps up to 10 min on the engine thread (`net.rs:76-83`).
- **i14 — Misc latent/ergonomic notes:**
  - `Capability::bit()` shifts by enum discriminant (`instrument.rs:40-43`) — panics if the
    `#[non_exhaustive]` enum ever reaches ≥64 variants, and mid-enum insertion silently
    shifts bits. **Fix:** explicit discriminants / `match`-based bits.
  - `ratatui` declared crate-local, bypassing `[workspace.dependencies]`
    (`akadro-tui/Cargo.toml:15`). **Fix:** move to workspace deps.
  - TUI fills table scales qty by `money_scale` (no `qty_scale`) (`akadro-tui/src/lib.rs:142`)
    — display-only.
  - Prelude omissions: `TriggerBy`/`TrailKind` (compile error for stop orders via
    `use akadro::prelude::*`), `CancelRejectReason`, `PlacedOrder`
    (`akadro-core/src/lib.rs:54-59`); crate doctest fails under `--no-default-features`
    (`akadro/src/lib.rs:20-35`). **Fix:** add to prelude; gate or rewrite the doctest.
  - `display()` silently clamps scale>38 (`fixed.rs:284,298`) — formatting-only.
  - `mul_bps` doc says "conservative underestimate" but truncation is *optimistic*
    (`fixed.rs:229-231`) — doc-only.
  - Test-quality gaps: dangling assertion comment
    (`akadro-venue-binance/tests/mock_connector.rs:103-106`) and the funding test
    discards the fill-bar settlement (`exchange.rs:1436`).

---

## (d) Uncertain — needs human judgment

These are confirmed-as-described but their *severity / whether to act* is a project-owner
decision (mostly opt-in features, experimental DEX, or deferred live shell):

1. **DEX arithmetic safety (m9/m10/m11)** — the verifier downgraded both originally-"critical"
   DEX casts to minor because `akadro-venue-dex` is an explicitly experimental, simplified
   v1 and DEX precision is documented future work. **Decision:** is the DEX in scope for
   release hardening now, or do these wait for the bignum numeric path? If kept, the
   clamp/saturate fixes are cheap and worth applying regardless.
2. **Live-shell gaps (i13)** — the Binance listen-key keepalive (silent fill loss after
   60 min) and the no-reconnect / silent-connect-failure issues are genuinely *major* in
   isolation but the live shell is partly deferred per the roadmap. **Decision:** are these
   blocking for any advertised live capability, or explicitly out-of-scope until
   `akadro-live` is finished?
3. **`total_fees` semantics (i1) and `realized_pnl` liquidation inclusion (i2)** — these are
   API/naming/documentation decisions: rename to signed `net_costs` / add separate fields,
   vs. document the current behavior. Both are additive (`#[non_exhaustive]`).
4. **MEXC POST-body params / per-asset fee scale** — already tracked as accepted deferrals
   in AGENTS.md §13; not re-reported, but noted for completeness.

---

## (e) Notes — build-hygiene items

- **`ratatui` off-workspace** (`crates/akadro-tui/Cargo.toml:15`): the only external
  production dep not in `[workspace.dependencies]`. If a second consumer adds it at a
  different version, `cargo-deny`'s `multiple-versions = "warn"` fires. **Fix:** add to
  `[workspace.dependencies]`, consume via `ratatui.workspace = true`.
- **AGENTS.md staleness** (`AGENTS.md:176,282-301`): compile-fail count (20 → 21) and the
  §8 crate-map (8 → 19 members). Documentation only.
- **`DataError::Io` `#[from]`** (`crates/akadro-data/src/bars.rs:33`): only `#[from]` in the
  workspace; a deliberate decision to keep or wrap (stdlib type, low semver risk).
- **Crate doctest under `--no-default-features`** (`crates/akadro/src/lib.rs:20-35`): fails
  to compile when the `backtest` feature is off. Affects `cargo test -p akadro
  --no-default-features` and any downstream CI disabling defaults. **Fix:** feature-gate or
  rewrite the doctest.

---

*Kill-feature verdict: HOLDS.* The four layers (structural append-before-call,
backward-only `Series`, invariant brand on `Ctx`/`Series`/`MarketView`, sealed
`pub(crate)` constructor) were re-confirmed against source. The only kill-feature
finding is the **regression-coverage gap** (i6) for the six newer hooks — the brand
is mechanically enforced today, but there is no trybuild lock to catch a future
signature slip.
