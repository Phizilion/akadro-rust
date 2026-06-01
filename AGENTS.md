# AGENTS.md — akadro-rust

> Engineering guide and architecture decision record for **akadro**, an
> exchange-agnostic backtesting + live trading framework for Rust whose
> defining features are a **compile-time guarantee against look-ahead** and
> **strict backtest↔live parity**.
>
> `CLAUDE.md` is a symlink to this file. Read this first before changing anything.

---

## 1. What this project is (and the five goals)

akadro lets users write a trading strategy **once** and run it unchanged in
backtest and in live trading. It is built around five non-negotiable goals, in
the priority the project owner set them:

1. **Exchange-agnostic.** Add any venue (CEX spot, CEX perp/futures, on-chain
   perp, AMM/DEX) by implementing a *small* trait set in a new crate, with
   **zero changes** to core/engine/strategy code (open/closed). Exchange
   specifics never leak into the core domain types.
2. **Backtest↔live parity (make-or-break).** The *same strategy source* runs in
   both modes with identical behaviour. If a strategy is profitable in backtest
   we want strong reason to believe it behaves the same live.
3. **Kill feature — compile-time future-data protection.** It is a **compile
   error** for strategy code to read future market data during a backtest.
4. **Performance.** Optimize any CPU/memory/disk operation that would save >10%
   wall-clock; otherwise keep it simple. No premature micro-optimization.
5. **Quality.** Idiomatic, maintainable, semver-stable, documented, not
   embarrassing. Edition 2024, MSRV 1.95.

### Status (this is an MVP that *proves the architecture*)

Working and verified today:

- The kill feature, proven by `trybuild` compile-fail tests (§4).
- The single event loop, the `Strategy`/`Ctx`/`Series` API, and a conservative
  deterministic `SimulatedExchange`.
- Backtest↔live **parity golden-master**: the same strategy yields a
  bit-identical `RunReport` from the historical feed and a threaded mock-live
  feed, for the fixture **and for arbitrary random price paths** (proptest §8).
- A worked sample strategy (`sma-crossover`).
- **The first real venue connector — `akadro-venue-mexc` (MEXC spot, REST):**
  HMAC-SHA256 signing (verified against an RFC 4231 vector), `exchangeInfo` →
  `InstrumentSpec`, `klines` → bars, signed order submit, and REST-poll fills,
  normalized to akadro types and tested end-to-end through the real engine with
  recorded fixtures via a mockable `Transport`. The `net` feature adds the live
  `reqwest` transport; the live round-trip is a credential-gated `#[ignore]` test.
  Built with **zero changes** to `akadro-core`/`akadro-engine`.

Deliberately deferred (see §13 roadmap; all reserved via `#[non_exhaustive]` so
they are additive, non-breaking): the async live shell (`akadro-live`) and
lower-latency **WebSocket** market/user-data streams, MEXC **futures**, the
Parquet/Arrow data layer, indicators, analytics/metrics, advanced order types,
margin/funding/liquidation realism, DEX precision.

---

## 2. How we got here (research + adversarial verification)

The architecture was not guessed. It was produced by a 14-agent dynamic
workflow: 6 web-backed research agents → 1 synthesis → **6 adversarial verifiers
each attacking a different goal** (leak-hunter, parity-auditor, exchange-
integrator, performance-skeptic, ergonomics/maintainability, completeness) → 1
reconciliation. The reviewers found **8 critical and 24 major** issues; all were
resolved or accepted as documented residual risk. The 17 hardened decisions
(`D1`–`D17`) are summarized in §11 and referenced inline as `(Dn)`.

The single most important finding: the originally-drafted crate split was
**unsound** — see `D1` below.

---

## 3. Architecture overview & crate map

A Cargo workspace (`resolver = "3"`, edition 2024). Dependencies flow strictly
downward; no cycles.

```
akadro-core        vocabulary + the exchange-extension traits. No I/O, no async,
                   no foreign types in its public API. The stable foundation.
   ▲
akadro-engine      the event loop + the look-ahead-safe Ctx/Series/Strategy and
                   the invariant brand. THE KILL FEATURE LIVES HERE.
   ▲          ▲
akadro-backtest  …  HistoricalFeed (DataSource) + SimulatedExchange
                   (ExecutionClient): conservative deterministic fills.
   ▲          ▲
akadro-testkit     MockLiveFeed (threaded bounded-channel transport) + helpers
                   for parity/strategy testing.
   ▲
akadro             umbrella: re-exports under tidy paths + a prelude + features.
sma-crossover      worked example strategy (its own crate, forbid(unsafe_code)).
akadro-venue-mexc  the first real venue connector (MEXC spot, REST) — depends ONLY
                   on akadro-core; implements DataSource/ExecutionClient/
                   InstrumentCatalog. `net` feature adds the reqwest transport.
akadro-compile-tests  trybuild proofs of the kill feature (a genuinely DOWNSTREAM
                   consumer of akadro-engine — see D1).
```

### D1 — why `Ctx` lives in `akadro-engine`, not `akadro-core`

The natural instinct is to put the domain types (including `Ctx`) in `core` and
the loop in `engine`. **This is unsound and the leak-hunter proved it on the
real toolchain.** The look-ahead guarantee requires that no one outside the
engine can *construct* a `Ctx` (otherwise they forge one over data they control
and the brand is meaningless). Rust has **no "pub to one specific external
crate" visibility**:

- A truly-private `Ctx::new` in `core` is **uncallable** from a separate
  `engine` crate (`error[E0624]`).
- A `pub`/`#[doc(hidden)] pub` constructor lets *any* downstream crate call
  `Ctx::__new(&my_own_future_data)` — verified to compile and run.

The only sound resolution: **define `Ctx`/`MarketView`/`Series`/the brand and
the event loop in the same crate** (`akadro-engine`), with a `pub(crate)`
constructor. `akadro-core` therefore holds only *vocabulary* + the *extension
trait signatures*. The `Strategy` trait also lives in `engine` because its
methods take `&mut Ctx<'_>`, so putting it in `core` would create a dependency
cycle. The `forge_context` trybuild case (in the separate `akadro-compile-tests`
crate) is the permanent regression proof that the private constructor is
unreachable downstream.

---

## 4. The kill feature — four layers + honest threat model

Reading future data during a backtest is a **compile error**, enforced by four
independent layers (all verified on rustc 1.95):

1. **Structural boundary.** A strategy never receives the dataset. The engine
   pulls one event at a time from the `DataSource` and appends it to observed
   state *before* invoking the strategy. The future is never in scope.
2. **Backward-only `Series`.** `Series` exposes `latest()`, `ago(n: usize)`
   (n bars *ago*), and `last_n(n)` — and nothing else. There is no forward
   accessor, no `Index`, no slice escape. "Read n bars ahead" is **inexpressible**
   (a missing method / a `usize` that cannot be negative), so it is a type error,
   not a runtime panic. `ago` returns `Option` (never panics) for out-of-range.
3. **Invariant generative brand.** `Ctx<'bar>` and `Series<'bar, T>` carry
   `Brand<'bar> = PhantomData<fn(&'bar ()) -> &'bar ()>` — **invariant** in
   `'bar`. Strategy callbacks receive a fresh per-call `'bar` (elided, late-bound:
   `fn on_bar(&mut self, bar: Bar, ctx: &mut Ctx<'_>)`). Stashing a `Ctx`/`Series`
   (in `self`, a `Vec`, a `RefCell`, an `Rc`, a closure, …) fails to compile: the
   per-call `'bar` cannot be unified with any longer-lived lifetime. Invariance
   is essential — a covariant marker could be silently shrunk and would give false
   safety.
4. **Sealed construction.** `Ctx` has only private fields and a `pub(crate)`
   constructor, co-located with the loop (see D1).

### What is NOT protected (threat model — D2)

These layers stop a **safe** strategy from reading the future **through the
`Ctx` API** — the realistic bug class. They do **not** stop:

- `unsafe` code in the user's strategy crate (e.g. `mem::transmute` to launder
  `'bar` to `'static`). Mitigation (D3): the strategy-crate template sets
  `#![forbid(unsafe_code)]` (as `sma-crossover` does); recommend a `cargo-geiger`
  CI gate.
- **Out-of-band** look-ahead: a strategy loading its *own* copy of the data and
  indexing `data[t+1]`, or stashing state in a `static`/`thread_local` across
  backtest passes. No type system can prevent this; it is the user's
  responsibility. Mitigation: fresh engine per run, documented contract.

The claim is therefore the precise, defensible one — *"no safe strategy code can
read future market data through the `Ctx` API"* — not "physically impossible".

### How it's proven (closed loop)

`crates/akadro-compile-tests/tests/ui/*.rs` are programs that **must not
compile**; `tests/ui-pass/*.rs` are legitimate patterns that **must** compile.
The committed `.stderr` snapshots pin the exact diagnostics, so a future rustc
wording change or an accidental API loosening fails CI. They are toolchain-
pinned (rustc 1.95); regenerate with `TRYBUILD=overwrite cargo test -p
akadro-compile-tests`.

The compile-fail suite is **27 cases** (hardened by the 2026-05-30 wide audit,
§12, then extended by the 2026-06-01 fix pass): the forge/index/coerce originals
plus every realistic stash vector — `RefCell`, `Rc<RefCell>`, `Arc<Mutex>`,
`Cell`, `OnceCell`, `Box`, a boxed closure, `thread::spawn`, a stashed `LastN`,
and the `&mut Ctx` itself — the two layer-2 sub-properties (`series.ahead(1)` →
no such method; `series[i]` → no `Index`), and a per-hook brand proof on **all
ten** lifecycle hooks (`on_start`/`on_bar`/`on_account`/`on_stop` and the six
event hooks `on_fill`/`on_order_rejected`/`on_order_canceled`/`on_liquidation`/
`on_funding`/`on_timer`), so a per-hook signature slip can't silently weaken the
brand. `LastN` now carries the invariant `Brand<'bar>` too (defense-in-depth: it
stays invariant even if a by-reference accessor is ever added).

---

## 5. Backtest↔live parity contract

Parity is achieved by making the strategy **blind to its environment** and
**deterministic**, and by sharing one event loop:

- **One loop** (`Engine::run`), generic over `Strategy`/`DataSource`/
  `ExecutionClient`. Only the injected feed and execution client differ between
  modes; the strategy cannot tell which it is in.
- **Event-time clock (D4).** `Ctx::now()` returns the *logical event time* of the
  current handler in both modes — never wall-clock. Two reads within one handler
  are equal in both modes (kills the mode-detection oracle). There is no
  wall-clock reachable from `Ctx`.
- **State only via events (D5).** All account state (positions, cash, fills) is
  derived from the async `AccountEvent` stream by a read-only portfolio. There is
  **no** synchronous "what's my balance/position?" venue query anywhere, so the
  two modes are structurally identical.
- **Deterministic RNG (D6).** Strategies get **no** randomness. Probabilistic
  models draw only from an engine-owned seeded `ChaCha8` (`DeterministicRng`);
  conservative deterministic fills are the default and have a live analog.
- **Lossless-or-fail bridge (D7).** The (future) live async→sync bridge must
  never drop/coalesce events; on overflow it aborts (`AkadroError::LiveBackpressure`).
- **Fixed-point money (D12).** No floats. `Price`/`Qty` are `i64`, `Money` is
  `i128`, and every `Price*Qty` widens to `i128` *before* multiplying. This makes
  the fill/PnL log bit-for-bit reproducible — the basis of the golden-master.

**Event ordering** (the parity contract), per incoming event at time `t`:
1. `ExecutionClient::observe` fills orders that were *already resting* (a market
   order submitted on bar *i* fills at bar *i+1*'s open — no execution look-ahead);
2. resulting `AccountEvent`s update the portfolio and reach `on_account`;
3. observed market state is appended (so a `Series` now includes this bar);
4. `on_bar` runs; orders it submits are routed and acknowledged.

The **golden-master** (`crates/akadro/tests/parity_golden_master.rs` +
`proptest_parity.rs`) asserts the backtest feed and the threaded mock-live feed
produce a bit-identical `RunReport`, across channel capacities and arbitrary
price paths. *Open loop:* real-venue parity (latency/fill fidelity) cannot be
tested without a live venue — see §12.

---

## 6. Exchange-agnostic extension model

To add a venue you implement, in a new `akadro-venue-*` crate depending only on
`akadro-core`:

- **`DataSource`** — `fn next_event(&mut self) -> Option<Event>`: normalize the
  venue's market data into ordered `Event`s.
- **`ExecutionClient`** — `submit(...)` + `observe(...)`: translate `OrderRequest`
  to the venue and venue fills/acks back into `AccountEvent`s via the `EventSink`.
- **`InstrumentCatalog`** — map venue symbols to `InstrumentSpec`.

Everything that is *not* placing an order (leverage, margin mode, token
approvals) flows through the venue-neutral `VenueCommand` enum (D9), not bespoke
trait methods. Costs are a `SmallVec<[Cost; 2]>` so a DEX swap can charge an LP
fee *and* gas in different assets (D10). The exchange-integrator verified this
trait set against Binance-USDⓈM-perp, Hyperliquid and a Uniswap-style AMM and
found nothing that forces a core change (DEX *precision* is de-scoped for v1 —
§12).

**MEXC is the first concrete venue** (CEX spot + futures); see §13. The async
plumbing (websockets/REST) will live in `akadro-live`, and tokio types must
never escape that crate.

---

## 7. Code style & conventions

- **Edition 2024, MSRV 1.95.** Shared metadata/lints/profiles in the root
  `[workspace.*]`.
- **Lints (workspace-wide, members opt in via `[lints] workspace = true`):**
  `unsafe_code = "forbid"` everywhere (the future `akadro-data` will be the only
  crate allowed `unsafe`, behind audited `// SAFETY:` modules); `missing_docs`,
  `missing_debug_implementations`, `unreachable_pub` warn; `clippy::pedantic`
  warns. A handful of pedantic lints are allowed with justification in the root
  manifest (e.g. `cast_lossless` clashes with `const fn` money math).
- **No floats for money.** Use `Price`/`Qty`/`Money`; multiply via
  `Price::notional` (widens to i128).
- **No foreign types in public signatures (D13).** Wrap them (e.g. `CapSet` over
  a `u64` bitset instead of exposing `enumset`; the engine implements `EventSink`
  rather than handing out a `Vec`).
- **Future-proofing.** Public enums/structs that may grow are `#[non_exhaustive]`;
  errors are `thiserror` enums, foreign errors wrapped (never `#[from]`-leaked).
- **Errors over panics in library paths.** `ago` returns `Option`; engine
  construction returns `Result`. `assert!`/`panic!` only for genuine programmer
  errors (e.g. `SmaCross::new` window validation) and documented with `# Panics`.
- **Determinism.** No `Instant::now`, no `thread_rng`, no ambient nondeterminism
  in library logic; everything nondeterministic is injected.
- Run `cargo fmt` (config: edition 2024, width 100) and keep `clippy::pedantic`
  clean before committing.

---

## 8. Directory / file structure

```
akadro-rust/
├── Cargo.toml                      workspace: members, shared package/deps/lints/profiles
├── AGENTS.md                       this file
├── CLAUDE.md -> AGENTS.md          symlink
├── README.md                       user-facing intro + quickstart
├── CHANGELOG.md                    keep-a-changelog
├── rustfmt.toml                    edition 2024, width 100
├── .gitignore
├── .github/workflows/ci.yml        fmt / clippy / test / trybuild / coverage / deny / semver
├── crates/
│   ├── akadro-core/                fixed.rs ids.rs instrument.rs order.rs event.rs traits.rs error.rs
│   ├── akadro-engine/              brand.rs series.rs market.rs portfolio.rs rng.rs context.rs strategy.rs observer.rs engine.rs
│   ├── akadro-backtest/            feed.rs exchange.rs replay.rs (record/replay trade oracle)
│   ├── akadro-testkit/             lib.rs (MockLiveFeed)
│   ├── akadro/                     umbrella + prelude + features; tests/{backtest_e2e,parity_golden_master,determinism,proptest_parity,analytics_pipeline,two_week_15m_backtest}.rs
│   ├── sma-crossover/              example strategy (forbid unsafe)
│   ├── akadro-indicators/          Indicator trait + average.rs channel.rs extremes.rs oscillator.rs range.rs (SMA/EMA/MACD/Bollinger/RSI/ATR, warm_up_bars)
│   ├── akadro-analytics/           equity.rs trades.rs distribution.rs cross_section.rs grid.rs walk_forward.rs (Sharpe/Sortino/maxDD, PSR/DSR, walk-forward)
│   ├── akadro-data/                bars.rs (Arrow/Feather cache, atomic write) manifest.rs merge.rs signal.rs trades.rs
│   ├── akadro-live/                bridge.rs (BoundedBridge, lossless-or-fail) reconnect.rs channel_exec.rs paper.rs + binance.rs mexc.rs (WS drivers, live-IO)
│   ├── akadro-venue-mexc/          spot+futures: sign.rs convert.rs request.rs transport.rs instrument.rs parse.rs client.rs futures.rs ws.rs ws_proto.rs net.rs(feat)
│   ├── akadro-venue-binance/       spot+USDⓈ-M futures + WS: lib.rs futures.rs ws.rs
│   ├── akadro-venue-okx/           spot+swap (REST): lib.rs net.rs(feat)  [base64 HMAC + passphrase]
│   ├── akadro-venue-bybit/         v5 spot+linear-perp (REST): lib.rs net.rs(feat)  [hex HMAC v5]
│   ├── akadro-venue-kucoin/        spot (REST): lib.rs net.rs(feat)  [v2 encrypted passphrase]
│   ├── akadro-venue-dex/           simplified AMM/DEX swap connector (LP-fee + gas multi-cost)
│   ├── akadro-tui/                 live terminal dashboard via the Observer interface (ratatui)
│   ├── playground/                 scratch real-data experiment binary (NOT instrumented for coverage)
│   └── akadro-compile-tests/       tests/trybuild.rs + tests/ui/*.rs (27 compile-fail) + tests/ui-pass/*.rs
└── benches/                        merge.rs (k-way merge perf budget — stub)
```

`~/.cargo/config.toml` on the dev host caps `build.jobs = 4` (host-specific, NOT
in the repo) — use ≤4 cores for compiling here.

---

## 9. Dependencies & rationale

Lean and credible. Required: `thiserror` (error enums), `smallvec` (alloc-free
cost/scratch lists), `rand_chacha`+`rand_core` (seeded deterministic RNG).
Optional/feature-gated or future: `arrow`+`parquet` (columnar data, behind
`akadro-data`, never in public signatures), `tokio` (async live shell, confined
to `akadro-live`), `serde` (run-manifest), proc-macro crates (a future
`#[strategy]` attribute). Dev: `trybuild` (kill-feature proofs), `proptest`
(property tests), `criterion` (benches), `dhat` (alloc gate, future).

---

## 10. Testing & verification (closed-loop principle)

Every claim is backed by an automated check whose output we observe; where a
loop can't be closed we say so (§12).

- **Unit tests** in every module, happy and unhappy paths.
- **Property tests** (proptest): fixed-point round-trip / `notional` / `mul_bps`
  / `diff`; portfolio position conservation and flat-round-trip PnL neutrality;
  **parity for arbitrary price paths**; determinism for arbitrary inputs.
- **Semantic / E2E:** trybuild compile-fail kill-feature proofs; the parity
  golden-master; determinism (rerun + parallel sweep); SMA end-to-end.
- **Doctests** on public items.
- **Lints:** `clippy::pedantic` clean.

Run it all:

```sh
cargo test --workspace                 # unit + integration + doctests + trybuild
cargo clippy --workspace --all-targets # must be clean
cargo fmt --all --check
# Region-based coverage (correctly attributes const-fn getters). net.rs is live
# network IO, excluded from the gate and exercised only by the #[ignore] live tests.
cargo llvm-cov --workspace --all-features --exclude akadro-compile-tests \
  --ignore-filename-regex '(venue-(mexc|binance|okx|bybit|kucoin)/src/net|akadro-live/src/(binance|mexc))\.rs' --show-missing-lines
```

**Coverage = 99.04% line / 98.35% region (measured, `cargo-llvm-cov`).**

**Philosophy:** the number is a *proxy*, not the goal — the goal is reliability.
So we write a real, asserting test for **every branch we can actually exercise**
(error paths, fill-realism branches, the new accessors/events, edge cases) and
skip only the **genuinely-untestable**: paths that need infrastructure we can't
cheaply build (a write that fails mid-`rename`, a malformed Arrow file with the
wrong column count, a real-time reconnect sleep, a thread race) or a defensive
guard no realistic input reaches. We do **not** game the metric (no assertion-free
"coverage" tests, no lowering the gate). The gate is `--fail-under-lines 99
--fail-under-regions 98` (CI; `akadro-compile-tests` and the scratch `playground`
are excluded from instrumentation, `net.rs` from the gate). The residual lines,
itemized so the open loop is explicit (closed-loop principle):

| Line(s) | Why uncovered (genuinely-untestable) |
|---|---|
| `backtest/exchange.rs` defensive arms (`_ => Outcome::Rest`, `_ => 0` in the `TrailKind` offset) | `#[non_exhaustive]` catch-alls; every current variant is handled explicitly above them. |
| `data/bars.rs` format-version + column-count guards | Need a **malformed Arrow file** (wrong cache version / ≠6 columns) — the writer never produces one, so they require hand-crafting an Arrow IPC file we don't build in-tree. The atomic-write *cleanup* branch and the ascending-ts guard *are* tested. |
| `engine/context.rs` `unrealized_pnl` no-mark branch | A position with no observed bar — forbidden by the `ExecutionClient` contract (a fill before the instrument's first bar) and already guarded by a `debug_assert` in `mark_to_market`. |
| `venue-mexc/parse.rs` arg-line attribution artifacts | The `price_scale,` *argument* lines inside `decimal_to_raw(...)` whose decode path *is* exercised; llvm maps them to a region that doesn't independently register. |
| `testkit/lib.rs` 41 | **Non-deterministic** thread race (`break` on consumer-dropped); covering it would need a `sleep`-based race that contradicts the suite's determinism. |
| test-only `_ => None` arms in `filter_map` helpers (`exchange.rs`, `venue-dex`, `client.rs`) | Test scaffolding, not library behaviour. |

To reproduce: `cargo llvm-cov ... --show-missing-lines` (command above).

---

## 11. Hardened decisions D1–D17 (index)

| #   | Decision |
|-----|----------|
| D1  | Co-locate `Ctx`/`Series`/brand/loop in `akadro-engine` (private ctor reachable only there). §3 |
| D2  | Down-scope the kill-feature claim to in-band/safe-code; publish a threat model. §4 |
| D3  | Strategy-crate template sets `#![forbid(unsafe_code)]`; recommend cargo-geiger. |
| D4  | `Clock::now()` = logical event-time in both modes; no wall-clock; two reads equal. §5 |
| D5  | All account state via `AccountEvent`s only; no synchronous venue query. §5 |
| D6  | Engine-owned seeded `ChaCha8`; strategies get no RNG; probabilistic fills are backtest-only. |
| D7  | Live async→sync bridge is lossless-or-fail (overflow aborts). |
| D8  | Gaps/reconnects are first-class replayable `Event::Resync`/synthetic `AccountEvent`s. |
| D9  | Venue-neutral `VenueCommand` envelope for non-order actions (leverage, margin, approvals). |
| D10 | `Fill` carries `client_order_id` + `SmallVec<[Cost; 2]>` (DEX = LP fee + gas). |
| D11 | Complete order-lifecycle events (accepted/rejected/filled/canceled) reserved. |
| D12 | Fixed-point money; widen `Price*Qty` to i128 before multiplying (verified faster than f64). §5 |
| D13 | No foreign types in public signatures (`CapSet`, `EventSink`, no `Index`). §7 |
| D14 | Elided-lifetime strategy impls work today; a `#[strategy]` macro is future ergonomics. |
| D15 | k-way merge should be a loser-tree (the real >10% hotspot); benches gate it. §13 |
| D16 | Sealed vs open trait table: venue traits are the extension surface; model traits will be sealed. |
| D17 | Reserve fee-tier/margin/liquidation/trigger-price seams now (additive later). |

---

## 12. Residual risks & open loops (told, not hidden)

- **Real-venue live parity** — cannot be empirically tested without a venue
  (credentials/network). Substitute: structural parity vs the mock-live feed.
  Real latency/fill fidelity is the irreducible accuracy ceiling.
- **Out-of-band look-ahead & `unsafe`** — not preventable by types (§4 D2).
- **Kill-feature wide audit (2026-05-30) — verdict HOLDS; hardening applied.** A
  62-agent dynamic workflow (35 Sonnet hunters on distinct attack angles → a
  Sonnet verifier per candidate → one Opus reconciler) attacked the compile-time
  future-data guarantee. **No safe-code, in-scope leak was found**; all four
  layers were independently re-verified against source (structural append-before-
  call ordering, zero forward accessors / no `Index`, a genuinely invariant brand
  on `Ctx`/`Series`/`MarketView`, `pub(crate)` sealed constructors). 22 of 26
  verified candidates dismissed (notably: `LastN` covariance is a non-exploit —
  covariance only *shrinks* `'bar` and `next()` yields `Copy` scalars; the
  clock/`ts` oracle conveys no OHLCV and is mode-identical per D4; a forged
  `Series` is a dead end since no public API consumes one). The 4 confirmed
  findings were all **minor closed-loop/regression-coverage gaps, not soundness
  holes**, and are now **fixed**: (1) `LastN` gained the invariant `Brand`; (2)
  ~14 escape-pattern proofs that lived only in a gitignored `wip/` are now
  committed `tests/ui/*.rs` (interior-mutability/closure/thread/`LastN`/`&mut Ctx`
  stashes); (3) the two layer-2 sub-properties (no forward accessor, no `Index`)
  now have compile-fail proofs; (4) every lifecycle hook (`on_start`/`on_account`/
  `on_stop`, not just `on_bar`) has a per-hook brand proof — extended on 2026-06-01
  to the six event hooks too (`on_fill`/`on_order_rejected`/`on_order_canceled`/
  `on_liquidation`/`on_funding`/`on_timer`). The compile-fail suite grew from 5 to
  27 cases (§4). **Residual (future work):** `LastN` is the first
  place to re-audit if a by-reference accessor (`as_slice -> &'bar [T]`) is ever
  added; the D2 out-of-band/`unsafe` ceiling is unchanged (mitigated by the
  `#![forbid(unsafe_code)]` strategy template + recommended `cargo-geiger` gate).
- **Coverage metric = literal 100%** — measured 99.04% line / 98.35% region with
  `cargo-llvm-cov`; the gap is a defensive `#[non_exhaustive]` catch-all, live-IO
  (`net.rs`, excluded), test-only helper arms, and two argument-line attribution
  artifacts. No untested production behavior. Itemized in §10. This is the
  irreducible honest ceiling without weakening the code or writing racy tests.
- **Account solvency / analytics on blowups (addressed, opt-in).** A real-data
  experiment (`crates/playground`, BTCUSDT 5m) surfaced two gaps, both since
  fixed: (1) the spot `SimulatedExchange` did not enforce a cash balance, so a
  fee-heavy strategy could drive equity negative — now opt-in via
  `FillConfig.starting_cash` / `with_starting_cash` (rejects unaffordable buys
  with `InsufficientFunds`, reserving the cost of already-resting buys; default
  off keeps parity bit-identical); (2) `PerformanceReport::from_equity` returned
  `NaN` ratios when equity crossed zero — now returns `None` for any non-positive
  equity. Still out of scope: base-asset/short-inventory and true cross-margin
  limits (D17).
- **Deep bug-hunt (2026-05-30, all fixed).** A 62-agent workflow (8 Opus subsystem
  auditors → a Sonnet verifier per finding → an Opus reconciler) found 16 confirmed
  bugs (1 critical, 5 major, 10 minor), all now fixed. **Critical:** the MEXC
  connector's `avg_price` was off by `10^qty_scale` (dividing a `price_scale` quote
  by a `qty_scale` base), mis-scaling live fills + fees and breaking parity — the
  unit/e2e tests had pinned the wrong value, masking it. **Major (all opt-in or
  replay-oracle; the conservative default golden-master path was unaffected):**
  intra-bar OCO double-execution, slippage wrongly applied to passive limit fills,
  `reduce_only` ignored, and two `diff_reports` false-mismatch mechanisms
  (cross-instrument equal-ts ordering; funding/liquidation effects in
  `realized_pnl`/`total_fees` but not `fills`). **Minor:** truncating average-entry
  price (now round-to-nearest — verified behaviour-neutral on real BTC data),
  unguarded `i64` position math, `Money::neg`/replay-lead-in overflow, a
  `read_partition` panic→`DataError`, a `mark_to_market` contract gap, and two
  deferred-futures-path bugs (f64 in tick derivation, `market+post_only`). Each fix
  shipped with a regression test; the record/replay trade oracle
  (`akadro-backtest::replay`) confirms the default path is unchanged.
- **External-benchmark audit v2 (2026-05-30, ~48 of 53 fixed).** A 110-agent
  workflow (15 web-backed comparison agents → a verifier per finding → an Opus
  reconciler) compared every dimension against Nautilus Trader, QuantConnect LEAN,
  Zipline, empyrical, CCXT, Binance/MEXC, TA-Lib and ArcticDB. 53 confirmed; ~48
  implemented across all crates (analytics limit conventions + new trade stats +
  configurable risk-free; `min_notional` scale + `round_price` + new
  order-lifecycle events + `TrailKind` + COD command; the full new `Ctx`/`Strategy`
  surface — accessors, per-event hooks, event-time timers; atomic + LZ4-compressed
  data writes with an Arrow `Timestamp` column + ordering/version validation; MEXC
  server-time sync + 429 retry + interval/limit fixes; O(1) monotonic-deque
  extremes; a live reconnect floor). See CHANGELOG. The remaining **5 (all MINOR)**
  are owner decisions / blocked on a future subsystem and are tracked in §13:
  volume-share slippage (#15), POST-body params (#47), per-`commissionAsset` fee
  scale (#37), `OrderCancelPending` (#17, needs the WS shell), and order amendment
  (#35, the D11 work).
- **External-benchmark audit (2026-05-30, fixed).** A 21-agent workflow compared
  every dimension against production systems (Nautilus Trader, QuantConnect LEAN,
  Zipline, FIX, empyrical). Confirmed issues, all now fixed: analytics used
  population (not sample, `ddof=1`) variance for Sharpe and returned `0` Calmar on
  zero-drawdown (and `volatility` is now annualized); strategies could not cancel
  orders or issue `VenueCommand`s (both now route through `Ctx`/`ExecutionClient`,
  D9); `AkadroError::LiveBackpressure` was documented but never raised (now
  enforced by the `BoundedBridge`, D7); fixed stops filled at the trigger through
  a gap (now at the worse of trigger/open); funding/liquidation overloaded `Fill`
  with magic ids (now `AccountEvent::FundingSettlement` / `Liquidation`); `Fill`
  now carries `complete`, conditional orders emit `OrderTriggered`, OCO cancels use
  `CancelReason::OcoTriggered`, the drain loop is depth-bounded, `mul_bps`
  saturates, `RunReport` records the seed, and `Price`/`Qty`/`Money` gained a
  scaled `display()`.
- **`cargo-semver-checks`** — needs a published baseline; closes from first release.
- **DEX precision** — `i64`/`i128` cannot hold `sqrtPriceX96` (2^160); DEX is
  documented future work behind an additive bignum numeric path.
- Probabilistic fill models have no live analog by construction (kept opt-in).

---

## 13. Roadmap / future work

**Shipped in v0.2** (all tested, clippy-clean, `cargo doc -D warnings` clean):
`akadro-indicators`, `akadro-analytics` (+ engine equity curve), expanded order
types (stop-limit, trailing, MIT, post-only, OCO) and realistic fill models
(slippage, latency, partial fills, funding, liquidation) in `SimulatedExchange`,
MEXC **futures** + real fees (`myTrades`) + WS control protocol (validated live),
`akadro-data` (Arrow/Feather cache + manifest), `akadro-venue-dex`, and
`akadro-live` (reconnect + paper trading). Coverage measured with `cargo-llvm-cov`.

**External-benchmark deferrals (2026-05-30 audit v2 — all MINOR; owner decisions
or blocked on a future subsystem):**

- **Volume-share slippage model (#15)** — a quadratic market-impact model
  (impact ∝ `(fill_qty / bar_volume)²`, like LEAN's `VolumeShareSlippageModel`)
  alongside the flat-bps one. Needs a `SlippageModel` enum + fixed-point quadratic
  math + threading the fill qty & bar volume into `slipped()` (today it sees only
  `(price, side)`). The flat model stays the conservative default.
- **MEXC POST params in body (#47)** — move signed `POST`/`DELETE` params from the
  URL query into an `application/x-www-form-urlencoded` body. The signed-query form
  is MEXC-compliant and keeps "signed bytes == sent bytes" trivially true; the only
  upside is keeping the signature out of proxy logs over the already-TLS URL.
- **MEXC per-`commissionAsset` fee scale (#37)** — decode the realized `myTrades`
  commission at the scale of its actual asset (quote / base / MX token), not always
  `price_scale`. Blocked on three things: wiring `myTrades` realized fees into the
  fill path; confirming MEXC's per-asset commission precision **live** (the docs do
  not pin it); and deciding how the portfolio aggregates non-quote fees (D17). The
  current `price_scale` is correct for the dominant quote-asset taker case.
- **`AccountEvent::OrderCancelPending` (#17)** — surface an in-flight cancel state;
  it only exists with push, so it needs the `akadro-live` WS shell. `ctx.cancel()`
  already documents that a `Fill` may still race a cancel live.
- **Order amendment (#35)** — `ctx.amend()` + `ExecutionClient::amend` +
  `OrderAmendAccepted`/`Rejected`; this is the D11 work. Until then a strategy
  cancels-and-resubmits (documented on `OrderAmend`).

**Remaining future work**, in rough priority:

0. **MEXC WebSocket protobuf decode** — vendor `mexcdevelop/websocket-proto`,
   compile with `prost`, add a `tokio-tungstenite` transport, decode live klines /
   private fills. (The control protocol + REST path are done; this is the one
   area the API research could not verify from docs alone.)
1. **mmap zero-copy load** for `akadro-data` (the audited-`unsafe` `memmap2` tier),
   and the optional Parquet+zstd cold/interop tier.
2. **Exact AMM math** for the DEX (constant-product `x·y=k`, bignum precision for
   `sqrtPriceX96`/18-decimal reserves).

1. **MEXC connector — spot/REST DONE** (`akadro-venue-mexc`): signing,
   `exchangeInfo`→`InstrumentSpec`, `klines`→bars, signed order submit, REST-poll
   fills; fixture-tested through the engine; `net` reqwest transport + credential-
   gated live test. **Next for MEXC:** lower-latency protobuf **WebSocket** market
   + user-data streams (needs the `akadro-live` async bridge below), MEXC
   **futures** (funding, leverage via `VenueCommand`, contract klines), and
   pulling the realised fee from fill data rather than the modelled taker rate.
   Open items flagged in `[[akadro-mexc-api]]`/the research artifact (WS protobuf
   field names, klines max, futures recv-window unit) must be confirmed live.
2. **`akadro-live` async shell** (only needed once WS push / sub-bar latency
   matters): a lossless-or-fail bounded bridge feeding the sync engine, reconnect
   reconciliation emitting synthetic `AccountEvent`s + `Event::Resync`, event-time
   `LiveClock`. A bar-cadence REST connector (MEXC v1) does not need it.
2. **Loser-tree k-way merge** + `benches/merge.rs` as a CI perf gate (D15).
3. **`akadro-data`**: Arrow/Parquet columnar storage + mmap; mirrored ring buffer.
4. **`akadro-indicators`** (incremental; SIMD behind a feature) and
   **`akadro-analytics`** (Sharpe/Sortino/maxDD/turnover, walk-forward, PBO).
5. **`#[strategy]` proc-macro** + an mdbook "I got a lifetime error" page (D14).
6. Order-type/margin/funding/liquidation realism (D11/D17); more venues; DEX.
7. CI hardening: `cargo-semver-checks`, `cargo-deny`, `cargo-hack` feature
   powerset, dhat zero-alloc gate, llvm-cov gate (99% line / 98% region; see §10
   for why literal 100% is not an honest target).

---

## 14. Changelog

See `CHANGELOG.md` (keep-a-changelog). Current: `0.1.1` (2026-06-01) — the v0.2
growth work (indicators, analytics, data layer, live shell, five venue connectors,
expanded order/fill realism) plus the final pre-release bug-hunt fix pass. `0.1.0`
(2026-05-31) was the MVP that proved the architecture (kill feature, parity,
exchange-agnostic core, deterministic engine + simulated exchange, sample
strategy, full test suite).
