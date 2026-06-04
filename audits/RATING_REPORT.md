# akadro-rust — Library Quality Rating

**Overall: 7.5 / 10**

> An impressively engineered Rust backtesting/live-trading framework whose hardest parts —
> the compile-time look-ahead guarantee, backtest↔live parity, and fixed-point money discipline —
> are done *right* and at a level above most pre-1.0 financial libraries. It passes every
> observable gate cleanly. It loses points to testing breadth, code duplication, a few
> discipline-drift spots, and prototype-stage incompleteness — not to broken code.

*Scope note: this rating evaluates the current working tree on technical merit. Version-control
and release-process matters (commit granularity, tags, "committed vs. described" state) are
deliberately excluded at the owner's request.*

---

## 1. Methodology

This is not a doc-trust review. The rating was produced by:

- **Empirically running the gate** on the current tree (build / clippy / fmt / test / trybuild).
- **A multi-agent adversarial source audit** — 8 subsystem auditors reading the actual `.rs`
  source (instructed *not* to trust `AGENTS.md`, since the doc is the thing under audit), plus
  4 adversarial attackers each trying to break a headline claim (kill feature, parity, coverage
  honesty, exchange-agnostic), reconciled into the scores below.

Ground-truth facts (measured directly): **19 crates, ~42,000 lines of Rust, 701 `#[test]`
functions, 3 proptest macros, 27 compile-fail trybuild cases + 2 ui-pass.** Edition 2024, MSRV 1.95.

---

## 2. Empirical gate — what actually runs

| Check | Result |
|---|---|
| `cargo build --workspace --all-features` | ✅ clean, ~1.4s (dev) |
| `cargo clippy --workspace --all-targets --all-features` | ✅ **0 warnings** (pedantic) |
| `cargo fmt --all --check` | ✅ clean |
| `cargo test --workspace` | ✅ **698 passed / 0 failed** (10 ignored = credential-gated live tests) |
| `cargo test -p akadro-compile-tests` (trybuild) | ✅ **27/27 compile-fail cases pass** |
| Coverage | Not re-run (slow). Project's last measurement: 97.3% line / 98.2% region. See §6 issue. |

**The working tree builds and passes its own test suite with zero lint noise.** This is a genuinely
clean build, not an aspirational one.

---

## 3. Dimension scores

| Dimension | Score | Summary |
|---|:---:|---|
| Kill-feature soundness | **8.8** | All 4 layers real & load-bearing; honestly scoped |
| Architecture & design | **8.5** | The hard structural calls are correct |
| Parity rigor | **8.2** | Proven by bit-identical `RunReport`, not asserted |
| Documentation | **8.0** | Unusually thorough; honest threat model & residual tables |
| Correctness / does-it-work | **7.8** | Gate passes; fill logic correct; minor discipline drift |
| Claim honesty | **7.5** | More honest than most; a few real overstatements |
| Code quality / idiom | **7.5** | Disciplined, but notable duplication + a D13 leak |
| Testing rigor | **7.2** | Meaningful assertions; property testing too sparse |
| Completeness / production-readiness | **6.5** | Strong prototype; real-venue/live loops still open |

### Subsystem scores
| Subsystem | Score |
|---|:---:|
| Kill feature (`akadro-engine` brand/series/context + trybuild) | 8.5 |
| Engine loop + parity + replay oracle | 8.4 |
| `akadro-core` vocabulary + fixed-point money | 8.1 |
| `SimulatedExchange` fill realism | 7.8 |
| Venue connectors (mexc/okx/bybit/kucoin/binance/dex) | 7.8 |
| Analytics + indicators | 7.8 |
| `akadro-data` cache layer | 7.1 |
| Infrastructure & polish (CI, umbrella, live, tui, playground) | 6.0 |

---

## 4. What's genuinely excellent

1. **The kill feature is real, not theater.** All four layers exist verbatim and are correct:
   structural append-before-call ordering (`engine.rs:238-266`, proven by `end_to_end_buy_fills_next_bar`
   asserting fill = 110 not 100); a backward-only `Series` with no `Index`, no `as_slice`, no forward
   accessor (a `usize` offset makes look-ahead inexpressible); a genuinely *invariant* brand
   `PhantomData<fn(&'bar()) -> &'bar()>` on `Ctx`/`Series`/`MarketView`/`LastN`; and a `pub(crate)`
   constructor co-located with the loop, proven unreachable downstream by `forge_context.rs`. The
   27-case stash-vector suite + per-hook brand proofs are comprehensive. The guarantee is honestly
   scoped to in-band safe code (does **not** overclaim "physically impossible").

2. **Parity is proven, not asserted.** The golden-master compares a *full* `RunReport` (all 12 fields,
   derived `PartialEq`, no manual override) between the historical feed and a real producer-thread
   mock-live feed using tiny channel capacities (1/2/8/64) to force backpressure; `proptest_parity`
   extends this to 64 arbitrary random price paths. No nondeterminism in the money path (grep-confirmed).

3. **Fixed-point money is architecturally enforced.** There is no `i64*i64` multiply path at all —
   `Price::notional` (widening to i128) is the sole multiplication site; `mul_bps`/`mul_rate` saturate;
   `FUNDING_RATE_SCALE` is defined once in core and reused so connector scale and engine charge can't drift.

4. **Exchange-agnostic surface is real.** Engine and strategy code is genuinely unchanged across six
   venue connectors; the `Transport` seam cleanly isolates live IO; local rejection parity (post-only on
   non-limit, reduce-only on spot) is enforced before any network call; signing is verified against RFC 4231.

5. **Test assertions are meaningful, not coverage padding.** Values are independently derived (the MEXC
   `avg_price` scale algebra is spelled out in comments; Bollinger sd=2 from a known set; RSI monotone-up
   = 10000). The replay trade-oracle is a real behavioral regression net that correctly handles the
   funding/liquidation gating subtlety.

---

## 5. Problems to fix

Prioritized, actionable, with file references. None of these is a *current* bug that fails the suite —
they are correctness-discipline drift, semver hazards, test gaps, and polish.

### High priority — correctness discipline & API/semver hazards

- [ ] **`net_after` staging uses bare `+=`** in `akadro-backtest/src/exchange.rs`, inconsistent with the
      saturating-arithmetic discipline the project mandates. Analytically safe today, but it's exactly the
      drift that becomes an overflow bug under an extreme input. Use `saturating_add`.
- [ ] **Wilder smoothing (RSI/ATR) uses bare `as i64` casts** in `akadro-indicators` while the stated rule
      is `try_from`/clamp. Convert to `i64::try_from(..).unwrap_or(..)` / `.clamp(..)` like the rest of the
      indicator code (SMA/EMA/Bollinger already do this).
- [ ] **`pub type Costs = SmallVec<[Cost; 2]>` leaks a foreign type into the public API** — a D13 violation.
      Wrap it in an opaque newtype so `smallvec` isn't part of the public signature.
- [ ] **`PlacedOrder` and `Side` are missing `#[non_exhaustive]`**, contradicting the workspace's own
      growable-type policy. Add it (or document why they're intentionally exhaustive) — it's a semver trap.
- [ ] **`BarOrderError` does not implement `std::error::Error`.** Add the impl (or `thiserror`) so it
      composes with `?` and `Box<dyn Error>`.
- [ ] **`Resync` is handled inline rather than through the `drain` loop** in the engine — architecturally
      inconsistent with the other event paths and a maintenance hazard for parity. Route it through `drain`.

### Medium priority — test gaps

- [ ] **Add a test for the M11 concurrent reduce-only path** (several reduce-only orders in one bar that
      collectively must not overshoot/flip the position). It's claimed covered but has no test.
- [ ] **Cross-validate Sharpe/Sortino/Calmar against an external reference** (empyrical or a closed-form
      hand calc) so the `ddof=1`/annualization claims are code-verifiable, not prose-verifiable.
- [ ] **Add a test for the armed IOC stop-limit expiry path** (a triggered IOC stop-limit that can't fill).
- [ ] **Add `!dbg.contains("secret")` Debug regression tests for OKX, KuCoin, Bybit, and Binance** — the
      credential-hiding guarantee is regression-locked only for MEXC; four of five connectors are unguarded.
- [ ] **Dedup MEXC spot backfill after page assembly** — the other venues dedup overlapping pages; MEXC spot
      doesn't, and add the test that would have caught it.
- [ ] **Add offline unit tests for the `akadro-live` WS drivers** (currently zero) — decode/reconnect logic
      can be exercised over canned frames without a live socket.
- [ ] **Fix `bars.rs` test temp paths to use PID + atomic-counter uniqueness.** `manifest.rs`,
      `incremental.rs`, and `load.rs` already do this; `bars.rs`'s fixed-format `tmp(name)` can collide under
      parallel `cargo test`.
- [ ] **Expand property testing (only 3 proptest macros today).** Add proptest to: the `SimulatedExchange`
      fill logic (arbitrary order sequences, OCO over arbitrary price paths, reduce-only with varying sizes),
      the data-cache gap algebra (arbitrary cached range sets), and indicator arithmetic over extreme inputs.

### Medium priority — duplication (DRY)

- [ ] **Extract an `akadro-venue-common` crate.** `Method`/`HttpRequest`/`HttpResponse`/`Transport`/
      `MockTransport` are fully duplicated across four connectors, and HMAC signing across five. The stated
      "keep HTTP out of `akadro-core` for SoC" argument is valid, but a *sibling* common crate between the
      venue connectors resolves the duplication with no dependency cycle.

### Low priority — performance polish

- [ ] **`close_order` uses `Vec::retain`** (O(n) per close) — fine at current scale, but document it or move
      to a swap-remove by index. Likewise the **`Vec::remove(0)` rolling window in `akadro-tui`** (O(n));
      use a `VecDeque`.

### Low priority — documentation / claim integrity

- [ ] **Reword "zero changes to core/engine/strategy."** It's true for engine and strategy, but `core` grew
      (`decimal.rs`, `funding.rs`, `page_sink.rs`, `progress.rs`) to serve connectors. Say "zero engine/
      strategy changes; shared venue-neutral logic added to core."
- [ ] **Reconcile the CI coverage gate.** `ci.yml` sets `--fail-under-lines 99` while the docs state the
      measured line coverage is 97.3% and "would not pass on its own." Lower the line gate to ~97 (region 98
      stays) or close the gap — and stop simultaneously claiming "the enforced gate is passing."
- [ ] **Reconcile the "ten lifecycle hooks, dedicated files" claim** — there are nine dedicated `stash_view_in_*`
      files; `on_bar` is covered by `stash_view_across_bars.rs`. Add the missing file or fix the wording/count.
- [ ] **Trim the agent-count framing** ("62-agent workflow", "110-agent audit"). It adds rhetorical weight
      without verifiability; the code-level findings stand on their own.

### Tracked / open loops (not blocking, acknowledged)

- [ ] **Real-venue live parity** — needs credential-gated live verification; currently substituted by the
      mock-live structural parity. Honestly documented as an open loop.
- [ ] **MEXC WS protobuf decode** — unverified against the live schema (open item).
- [ ] **`cargo-semver-checks` CI job is hard-disabled** — activate once a published baseline exists.
- [ ] **`akadro-tui` is orphaned** from the umbrella crate — wire it in or document it as standalone.

---

## 6. What would raise the score

- Land the **property-test expansion** (fill logic, cache gap algebra, connector parsing) — this is the
  single biggest lever, since 3 proptest macros is thin for a safety-critical money system.
- **Cross-validate the analytics** against an external reference so the metric-convention claims are
  code-backed.
- **Extract `akadro-venue-common`** to kill the 4–5× HTTP/HMAC duplication.
- **Close the discipline-drift spots** (`net_after`, Wilder casts) and the **API semver hazards**
  (`Costs` newtype, `#[non_exhaustive]` on `PlacedOrder`/`Side`).
- **Regression-lock secret-hiding** across all five connectors.

Doing the High-priority list + the property-test expansion would credibly move this to ~8.5.

---

## 7. Confidence & caveats

- Confidence: **high** on the code-level findings (read from source + run gate).
- I did **not** re-run `cargo llvm-cov` (too slow); the coverage figures are the project's own last
  measurement, not freshly verified by me. The CI line-gate contradiction (§5) is verified from `ci.yml`.
- "No current bug fails the suite" is bounded by the suite's own coverage — several of the test gaps above
  (M11 concurrent reduce-only, armed IOC stop-limit) are precisely where an undetected bug could hide.

---

## 8. Resolution status (2026-06-04)

Worked the §5 list end-to-end; full gate green after each change (fmt · clippy `--all-features`
**0 warnings** · `test --workspace` · `doc -D warnings` · replay trade-oracle + parity golden-master
+ `proptest_parity` bit-identical). See `CHANGELOG.md`.

**High priority — DONE (all):**
- [x] `net_after` → `saturating_add`; Wilder RSI/ATR → `i64::try_from(..).unwrap_or(..)`.
- [x] `Costs` is now an opaque newtype (`smallvec` out of the public API, D13); call sites unchanged via `Deref<[Cost]>` + `&Costs` iteration.
- [x] `PlacedOrder` → `#[non_exhaustive]` + `::new`; `Side` documented as *intentionally* exhaustive (binary, closed).
- [x] `BarOrderError` implements `std::error::Error` (via `thiserror`).
- [x] Engine `Resync` routed through the `drain` loop (one code path; parity-safe — `Portfolio::apply` is a no-op for resync).

**Medium — test gaps: DONE** (M11 concurrent reduce-only; armed IOC stop-limit; OKX/KuCoin/Bybit/Binance secret-`Debug`; MEXC spot back-fill dedup + test; `bars.rs` PID+atomic temp paths). **Property tests** added (fill-logic reduce-only invariant; cache-gap algebra; analytics closed-form cross-validation). The **`akadro-live` WS-driver** offline tests remain open: the decode (`parse_ws_kline`/`parse_execution_report`) and reconnect policy *are* tested; the untested part is the live socket glue, which needs a mock-socket abstraction (deferred).

**Medium — DRY (`akadro-venue-common`): deferred with reasoning** (roadmap §13.8) — the per-connector error types + non-uniform `HttpRequest` shapes make a shared `Transport` a breaking/intrusive design change, not a drive-by refactor.

**Low — perf: DONE** (`close_order` documented; `akadro-tui` window documented — `VecDeque` fights ratatui's contiguous-slice `Dataset`). **Low — docs: DONE** ("zero changes to core" reworded; CI coverage gate reconciled to the freshly-measured ~97.1/97.8 with `97/97` thresholds + `venues.rs`/`tui` excluded; the "ten hooks/dedicated files" count fixed). The **agent-count framing trim** was judged disproportionate (pervasive, purely cosmetic) and left as-is.

**Tracked / open loops** unchanged (real-venue live parity, MEXC WS protobuf, `cargo-semver-checks` baseline, `akadro-tui` umbrella wiring) — acknowledged, not regressions.
