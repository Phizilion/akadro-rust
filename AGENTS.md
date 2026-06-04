# AGENTS.md — akadro-rust

> Engineering guide and architecture decision record for **akadro**, an
> exchange-agnostic backtesting + live trading framework for Rust whose
> defining features are a **compile-time guarantee against look-ahead** and
> **strict backtest↔live parity**.
>
> `CLAUDE.md` is a symlink to this file. Read this first before changing anything.
>
> **→ [§15 Agent playbook](#15-agent-playbook--pitfalls-failure-modes--hard-won-lessons) is the
> distilled pitfalls-and-lessons manual. Skim its Golden Rules every session; read the matching
> subsection before touching that subsystem. Nearly every rule traces to a bug that shipped despite
> passing tests.**

---

## 1. What this project is (and the five goals)

akadro lets users write a trading strategy **once** and run it unchanged in
backtest and in live trading. It is built around five non-negotiable goals, in
the priority the project owner set them:

1. **Exchange-agnostic.** Add any venue (CEX spot, CEX perp/futures, on-chain
   perp, AMM/DEX) by implementing a *small* trait set in a new crate, with
   **zero engine/strategy changes** (open/closed). Exchange specifics never leak
   into the core domain types; *shared venue-neutral logic* (decimal↔fixed-point,
   funding-period inference, the page-sink/progress vocabulary) does live in
   `akadro-core` so connectors reuse it rather than re-implement it (DRY) — that is
   additive, not a per-venue core edit.
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

**Shared venue plumbing lives in `akadro-core`, not copied per connector (DRY).**
Decimal-string↔fixed-point conversion (`decimal_to_raw`/`raw_to_decimal`/`scale_of`,
`decimal.rs`) and perpetual funding-period inference (`infer_funding_period_ms`,
`funding.rs`) are venue-neutral, so they live once in core; a connector calls them
(wrapping `Option`→its own `…Error` where it needs a `Result`). Add a venue by
reusing these, not re-implementing them — the four-way-duplicated decimal helpers
that preceded this were the source of subtly-divergent edge-case handling.

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
- **Library owns the data — no leakage points (D18).** All market/venue data enters
  through *library-owned, well-defined functions*: the `Transport`-seam connectors
  (`*Catalog::fetch`, `*Feed::with_range`, `fetch_funding_history`/`…_paged`, …) and
  the `akadro-data` cache layer (`load_or_cache` / `load_or_cache_feed`). **User/
  strategy/example code must never hand-roll an HTTP request or hand-inject
  downloaded data into library structures** — every fetch+parse lives in the
  connector as reusable code, tested over `MockTransport`. The **casual raw-injection
  doors are now gated off by default**: `HistoricalFeed::{new, from_bars,
  try_from_bars}` (arbitrary `Vec<Bar>`/`Vec<Event>`) live behind the off-by-default
  `import-bars` feature (internal callers use a `pub(crate)` `*_unchecked` seam); the
  blessed cache→engine path is `akadro_data::load_or_cache_feed` /
  `load_or_cache_many_feed`, which return a ready `CachedFeed` (`DataSource`) so no
  raw-`Vec` constructor is touched. A user *can* still supply a custom `DataSource` —
  that is the **sanctioned** escape hatch (it IS goal #1, the venue-extension
  surface) and the honest ceiling, the data analogue of the §4 D2 in-band/out-of-band
  line: *no casual library API imports arbitrary user data, but a deliberate
  `DataSource` impl can.* The **agentic strategy lab forbids even that** — a hard
  rule ("No custom `DataSource`") backed by `scripts/verify-no-custom-datasource.sh`,
  and it builds without `import-bars`, so a lab agent has **no** data-import path
  except the library fetchers/cache. This is leakage-prevention discipline, of a piece
  with the look-ahead (§4) and parity (§5) goals: the fewer places raw data enters,
  the fewer places a look-ahead/parity bug can. The `playground` is the reference
  *consumer* — a thin orchestrator, zero raw requests.
- **The library does the plumbing; the user does strategy ([[akadro-library-does-the-work]]).**
  If data is worth fetching it's worth caching as a first-class artifact — e.g. **funding-rate
  history is cached exactly like klines** (`akadro_data::load_or_cache_funding`, a versioned/
  atomic 2-column Arrow partition mirroring the bar cache), and `load_or_cache_perp` downloads+
  caches **both** bars and funding in one call. Never ship an immature-library chore (re-fetching
  funding every backtest run). And **funding is mandatory for perps**: a `PerpetualFuture` with no
  funding is a hard error (`load_or_cache_perp` errors; `SimulatedExchange` panics on the first
  bar) — funding is a real recurring cost, so a zero-funding perp run is *silently wrong*, not an
  acceptable default. Always think several steps ahead: make the correct thing automatic and the
  incomplete thing an error.
- **Engineering principles.** Code is held to **DRY** (one source of truth — e.g.
  `FUNDING_RATE_SCALE` lives once in `akadro-core` and connectors re-export it),
  **SOLID** (the venue trait set is the open/closed extension surface; model traits
  are sealed, D16), **SoC** (core vocabulary ≠ engine loop ≠ venue I/O ≠ data layer),
  **LoD** (talk to immediate collaborators via the trait seams, not reach through
  them), and **fail-fast** (validate at construction, return `Result`/`Option` early,
  `debug_assert` invariants — never silently coerce bad input).
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
│   ├── akadro-core/                fixed.rs ids.rs instrument.rs order.rs event.rs traits.rs error.rs decimal.rs(shared venue decimal↔fixed-point) funding.rs(shared funding-period inference)
│   ├── akadro-engine/              brand.rs series.rs market.rs portfolio.rs rng.rs context.rs strategy.rs observer.rs engine.rs
│   ├── akadro-backtest/            feed.rs exchange.rs replay.rs (record/replay trade oracle)
│   ├── akadro-testkit/             lib.rs (MockLiveFeed)
│   ├── akadro/                     umbrella + prelude + features; data/venues.rs (one-call `data::load`/`load_perp` venue dispatch, `venues` feature); tests/{backtest_e2e,parity_golden_master,determinism,proptest_parity,analytics_pipeline,two_week_15m_backtest}.rs
│   ├── sma-crossover/              example strategy (forbid unsafe)
│   ├── akadro-indicators/          Indicator trait + average.rs channel.rs extremes.rs oscillator.rs range.rs (SMA/EMA/MACD/Bollinger/RSI/ATR, warm_up_bars)
│   ├── akadro-analytics/           equity.rs trades.rs distribution.rs cross_section.rs grid.rs walk_forward.rs (Sharpe/Sortino/maxDD, PSR/DSR, walk-forward)
│   ├── akadro-data/                bars.rs (Arrow/Feather cache, atomic write) manifest.rs(v2: Coverage{Bars,EmptyVerified}+record_range, per-series files) gap.rs(missing_gaps) incremental.rs(FlushPolicy+PartialWriter journal+recover_partial) ratelimit.rs load.rs(range-aware gap-fill loader: load_bars/_feed/_aggregated) concurrent.rs(load_many) merge.rs signal.rs trades.rs(aggregate_bars)
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
├── book/                           mdbook user guide (src/lifetime-errors.md — the D14 "I got a lifetime error" page)
└── audits/                         adversarial audit reports: RATING_REPORT.md, AUDIT_OBK.md, BUGHUNT_REPORT.md, SIM_ACCURACY.md
```

(The k-way-merge perf bench lives at `crates/akadro-engine/benches/merge.rs` — a real
CI perf gate now, not a stub: the tournament-tree merge vs the `BinaryHeap` baseline.)

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

**Production-ready gate — every change runs the closed loop before it is called
done** (no exceptions, no "should pass"): `cargo test --workspace` green ·
`cargo clippy --workspace --all-targets` **0 warnings** · `cargo fmt --all --check`
clean · `cargo doc -D warnings` (with features) clean · **maximize coverage** —
write a real asserting test for every branch you can exercise (don't game the
metric) · and **run the replay trade-oracle + parity golden-master on any
trade-sim/engine change** to prove behaviour is bit-identical (§5). A guessed
external schema is verified against the live API, not just a fixture. State results
honestly: if a step was skipped or a loop can't close, say so.

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

**Coverage = 97.1% line / 97.8% region (measured 2026-06-04, `cargo-llvm-cov`, workspace
`--all-features`, excluding `akadro-compile-tests`, the scratch `playground`, and the
live-IO/UI modules: connector `net.rs` + WS drivers, the umbrella `data/venues.rs` live
`ReqwestTransport` dispatch, and the `akadro-tui` terminal dashboard).** **Region** — the
meaningful metric for this const-fn- and connector-heavy code (line attribution
double-counts multi-line expressions) — and **line** both clear the **97% gate**. They
trail a literal 99% because v0.2's growth (five venue connectors + WS drivers + the TUI +
the cache layer) added integration code whose error/IO-adjacent and pacing-sleep branches
are exercised end-to-end through recorded fixtures but are not all cheaply unit-coverable;
this is tracked growth, not a regression. (Earlier revisions claimed 97.3%/98.2% and a 99/98
gate, but that predated the cache-layer growth and was not freshly re-measured — the figures
here are.)

**Philosophy:** the number is a *proxy*, not the goal — the goal is reliability.
So we write a real, asserting test for **every branch we can actually exercise**
(error paths, fill-realism branches, the new accessors/events, edge cases) and
skip only the **genuinely-untestable**: paths that need infrastructure we can't
cheaply build (a write that fails mid-`rename`, a malformed Arrow file with the
wrong column count, a real-time reconnect sleep, a thread race) or a defensive
guard no realistic input reaches. We do **not** game the metric (no assertion-free
"coverage" tests, no lowering the gate below the honest floor). The enforced gate is
`--fail-under-lines 97 --fail-under-regions 97` (passing). `akadro-compile-tests` and the
scratch `playground` are excluded from instrumentation; `net.rs`/WS drivers, the
`data/venues.rs` live dispatch, and `akadro-tui` from the gate. The residual
lines, itemized so the open loop is explicit (closed-loop principle):

| Line(s) | Why uncovered (genuinely-untestable) |
|---|---|
| `backtest/exchange.rs` defensive arms (`_ => Outcome::Rest`, `_ => 0` in the `TrailKind` offset) | `#[non_exhaustive]` catch-alls; every current variant is handled explicitly above them. |
| `data/bars.rs` + `data/funding.rs` format-version, column-count, and tz/Int64 guards | Need a **malformed Arrow file** (wrong cache version / wrong column count / tz-naive ts) — the writer never produces one, so they require hand-crafting an Arrow IPC file we don't build in-tree. The round-trip, ascending-ts guard, cache-hit, and fetch-error paths *are* tested (funding mirrors the bar cache). |
| `engine/context.rs` `unrealized_pnl` no-mark branch | A position with no observed bar — forbidden by the `ExecutionClient` contract (a fill before the instrument's first bar) and already guarded by a `debug_assert` in `mark_to_market`. |
| `core/decimal.rs` + `venue-*/parse.rs` arg-line attribution artifacts | Multi-line `decimal_to_raw(...)` argument/closure lines whose decode path *is* exercised (`core/decimal.rs` is **100% region**); llvm maps them to a region that doesn't independently register on the line metric. |
| `testkit/lib.rs` 41 | **Non-deterministic** thread race (`break` on consumer-dropped); covering it would need a `sleep`-based race that contradicts the suite's determinism. |
| test-only `_ => None` arms in `filter_map` helpers (`exchange.rs`, `venue-dex`, `client.rs`) | Test scaffolding, not library behaviour. |
| venue back-fill **pacing/back-off** lines (`venue-*` `with_page_delay` feeds + funding paging): the `else { Duration::from_secs(2) }` back-off branch and the two `std::thread::sleep(…)` calls | Only execute when `page_delay > 0` (a real download). Deterministic tests set `page_delay = ZERO` (no sleep, instant 429 retry), so the *pacing-on* arm is unreachable without a real wall-clock sleep — same class as the `testkit` sleep race above. The paging **logic** (cursor/guard/dedup/window-clip/429-retry/exhaustion/error) is all tested at delay 0. |

To reproduce: `cargo llvm-cov ... --show-missing-lines` (command above). Note the
playground is excluded from the gate (`--exclude playground`, scratch binary, §8); the
WS/exec/connector order paths are the main residual below a literal 99% line and are
tracked as live-IO/growth work, not regressions.

---

## 11. Hardened decisions D1–D18 (index)

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
| D18 | Library owns the data — every fetch+parse is reusable connector code over the `Transport` seam + the `akadro-data` cache; user code never hand-rolls requests or injects raw data (leakage-prevention). §7 |

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
- **Coverage metric = literal 100%** — measured 97.1% line / 97.8% region with
  `cargo-llvm-cov` (workspace `--all-features`, excluding compile-tests, the scratch
  `playground`, and live-IO/UI: `net.rs`/WS drivers, the `data/venues.rs` live dispatch,
  and `akadro-tui`). Both line and region clear the **97% gate**; they trail a literal
  99% because of v0.2's venue/WS/TUI/cache integration code (error/IO-adjacent +
  pacing-sleep branches exercised via fixtures, not all unit-coverable), plus the usual
  `#[non_exhaustive]` catch-alls, test-only helper arms, and multi-line
  argument-attribution artifacts. No untested production *logic*. Itemized in §10. This is
  the honest post-v0.2 ceiling without weakening code or racy tests.
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
2. **k-way merge — DONE (D15).** `MergeSource` now selects via an `O(log k)` tournament
   tree (was an `O(k)` linear scan), bit-identical to the linear merge (3000-iteration
   fuzz vs a naive reference + parity/replay re-verified). `benches/merge.rs`
   (in `akadro-engine`) is the CI perf gate: the tree beats the `BinaryHeap` baseline
   (~42% at k=12, sweeps k∈{12,64}).
3. **`akadro-data`**: Arrow/Parquet columnar storage + mmap; mirrored ring buffer.
4. **`akadro-indicators`** (incremental; SIMD behind a feature) and
   **`akadro-analytics`** (Sharpe/Sortino/maxDD/turnover, walk-forward, PBO).
5. **mdbook "I got a lifetime error" page — SHIPPED** (`book/`): the human-facing
   companion that decodes the brand lifetime errors, anchored to the trybuild proofs.
   The **`#[strategy]` proc-macro is deliberately deferred (D14)** — a reasoned
   non-fix: the `Strategy` trait already ships default no-op bodies for every hook
   except `on_bar` and uses elided late-bound `Ctx<'_>` lifetimes, so a strategy impl is
   *already* boilerplate-free (see `sma-crossover`). A macro would add a `syn`/`quote`
   proc-macro crate and codegen over the kill-feature surface for **no ergonomic gain**;
   revisit only if a concrete repeated boilerplate pattern actually emerges.
6. Order-type/margin/funding/liquidation realism (D11/D17); more venues; DEX.
7. CI hardening: `cargo-semver-checks` (still blocked on a published baseline),
   `cargo-deny` (live), `cargo-hack` feature powerset (live), **dhat zero-alloc gate —
   DONE** (the `alloc-gate` CI job bounds total allocations « bar count over a fixed
   backtest; see §13.7's `alloc_gate` test), llvm-cov gate (line/region at the honest
   97% floor; see §10).
8. **`akadro-venue-common` DRY extraction — deferred (reasoned).** A quality audit
   flagged the duplicated `Method`/`HttpRequest`/`HttpResponse`/`Transport`/`MockTransport`
   (4 connectors) + HMAC signing (5) and suggested a sibling crate. The catch the
   one-liner missed: each connector's `Transport::send` returns its **own** error type
   (`BinanceError`/`MexcError`/…), and the `HttpRequest` shapes already **differ** (mexc
   carries a `body`, okx `headers`, binance neither). So a shared `Transport` needs either
   a *unified* error (changing five connectors' public signatures — breaking) or a generic
   error parameter (intrusive). The genuinely-uniform sliver (the `HMAC-SHA256` primitive)
   is ~3 lines per venue. Net: a real cross-connector API/design change that warrants its
   own focused pass + the connector adversarial review, not a drive-by refactor — so it is
   deferred with this reasoning rather than forced (the §9 "defer disproportionate work"
   discipline). The duplication is cosmetic, not a correctness risk.

---

## 14. Changelog

See `CHANGELOG.md` (keep-a-changelog). Current: `0.1.2` (2026-06-02) — **walk-forward
look-ahead & fool-protection** (the structural `WalkForwardBacktest` whose `fit` step
never sees the OOS slice; `WalkForwardSummary` correct OOS pooling; `from_equity_auto`
annualization; the IS/OOS-leaking generic runners gated behind `escape-hatch`) and
**D18 data-import enforcement** (the casual `HistoricalFeed::{new,from_bars,
try_from_bars}` Vec-injectors gated behind off-by-default `import-bars`; the blessed
`akadro_data::load_or_cache_feed`→`CachedFeed` cache path; the agentic lab forbids a
custom `DataSource` outright via `verify-no-custom-datasource.sh`). Both designed via
research workflows; parity + replay bit-identical (the new code is above the engine).
`0.1.1` (2026-06-01) — the v0.2
growth work (indicators, analytics, data layer, live shell, five venue connectors,
expanded order/fill realism) and the pre-release bug-hunt fix pass + the Opus
total-review fix pass, plus the final
pre-release hardening: the **Binance Vision** bulk-historical downloader; a
venue-wide **sub-basis-point funding** precision fix (`FUNDING_RATE_SCALE = 1e-8` +
`Money::mul_rate`, bit-identical on the conservative default); **O(open positions)**
equity marking; and **full venue feature parity** — historical `with_range`
back-fill, `with_page_delay` pacing + 429 back-off, `*Catalog::fetch`, and paged
`fetch_funding_history` on every connector (OKX/Binance/MEXC/Bybit/KuCoin), each
verified against the live venue schema and hardened by a 41-agent adversarial review
(D18 library-owns-data, DRY/SOLID/SoC/LoD/fail-fast). `0.1.0` (2026-05-31) was the
MVP that proved the architecture (kill feature, parity, exchange-agnostic core,
deterministic engine + simulated exchange, sample strategy, full test suite).

---

## 15. Agent playbook — pitfalls, failure modes & hard-won lessons

> **This section is the always-in-context operating manual distilled from every review, bug-hunt, and
> external-benchmark audit run on akadro (the 14-agent architecture review, the 62-agent kill-feature
> wide audit, the 62-agent deep bug-hunt, the 110- and 21-agent external-benchmark audits, and the
> Opus total-review pass). It is co-located in `AGENTS.md` (= `CLAUDE.md`) precisely so it survives
> context reduction. Skim the Golden Rules every session; read the matching numbered subsection in full
> before touching that subsystem. Nearly every rule traces to a bug that shipped *despite* passing
> tests — "looks right" is never sufficient; run the closed loop and observe the output.**

### Executive summary

This playbook is the permanent operating manual for **akadro**, an exchange-agnostic Rust framework for backtesting and live trading whose two defining features are a **compile-time guarantee against look-ahead** (it is a compile error for safe strategy code to read future market data through `Ctx` during a backtest) and **strict backtest↔live parity** (the same strategy source runs unchanged in both modes and yields a bit-identical `RunReport`). It distills the architecture, the hardened decisions (D1–D18), the shipped-and-fixed bug classes, and the build/CI/review discipline into one self-sufficient reference. The five project goals, **in the owner's priority order, are: (1) exchange-agnostic** (add any venue in a new `akadro-venue-*` crate with zero core/engine/strategy edits); **(2) backtest↔live parity** (the make-or-break goal); **(3) compile-time look-ahead protection** (the kill feature); **(4) performance** (optimize only what saves >10% wall-clock); **(5) quality** (idiomatic, semver-stable, documented, edition 2024 / MSRV 1.95).

How to use this doc: **skim the Golden Rules every session** — they are the single most important artifact and are ordered by blast radius. **Before touching any subsystem, read its matching numbered section in full** (the "How to read this playbook" TOC maps each subsystem to a trigger). Treat every rule as a review gate, not advice: nearly all of them trace to a specific bug that shipped *despite* passing tests, so "looks right" is never sufficient — run the closed loop and observe the output.

### The non-negotiable golden rules

1. Never read future data through `Ctx`; the four layers (append-before-call, backward-only `Series`, invariant brand, sealed ctor) are all load-bearing — adding a forward accessor, an `Index` impl, an `as_slice -> &'bar [T]`, or a longer-lived lifetime silently breaks the guarantee. [#1]
2. Keep `Ctx::new`/`Series::new`/`MarketView::new` `pub(crate)` and co-located with the loop in `akadro-engine`; never split branded types into another crate (D1 — a `pub`/`doc(hidden)` ctor lets any downstream crate forge a context over its own data). [#1]
3. Keep the brand invariant (`PhantomData<fn(&'bar ()) -> &'bar ()>`); a covariant marker can be shrunk and gives false safety. Carry the invariant `Brand<'bar>` on every new branded view type and give it a `Debug` that hides the borrowed slice. [#1]
4. Every `Strategy` hook takes `&mut Ctx<'_>` (elided); every new hook needs its own `stash_view_in_<hook>.rs` compile-fail case, or that hook can leak while the other nine stay safe. [#1]
5. Never overclaim the kill feature: the defensible claim is "no safe strategy code can read future data through the `Ctx` API" — `unsafe` and out-of-band look-ahead are not preventable by types (D2; mitigate with the `#![forbid(unsafe_code)]` template + cargo-geiger). [#1]
6. Treat `LastN` as the first place to re-audit if a by-reference accessor is ever added; never add `as_slice -> &'bar [T]` without re-proving the guarantee and adding a compile-fail case. [#1]
7. Never fork the event loop per mode: no `mode` field, no `cfg!(backtest)`, no second loop, no `tokio` types in the engine — the strategy must not be able to tell which mode it is in. [#2]
8. Preserve the 4-step per-event ordering (observe-resting-fills → drain account events → push bar → run `on_bar`); reordering append-before-fill is execution look-ahead and a parity break. [#2]
9. Keep `Ctx::now()` as logical event-time only; no `Instant::now`/`SystemTime`/wall-clock reachable from `Ctx` (D4 — it would be a mode-detection oracle). [#2]
10. Derive all account state from the `AccountEvent` stream via the read-only `Portfolio`; never add a synchronous "what's my balance/position?" venue query anywhere (D5, also LoD). [#2]
11. Use no floats and no ambient nondeterminism in the money path (core/engine/backtest); randomness comes only from the engine-owned seeded `ChaCha8` (D6), injected, never `thread_rng`. [#3]
12. Widen `Price*Qty` to `i128` before multiplying via `Price::notional`; there is deliberately no i64 multiply (D12) — and fixed-point money is the basis of the bit-identical golden-master. [#3]
13. When converting between two scaled quantities, write out the scale algebra and confirm the `10^k` factors cancel or are reintroduced (the critical MEXC `avg_price` bug divided a price-scale quote by a qty-scale base). [#3][#5]
14. Carry funding at `FUNDING_RATE_SCALE = 1e-8` and charge via `Money::mul_rate`, never a bps scale (sub-1bp rates round to zero); reuse the one shared constant so connector and engine can't drift. [#3]
15. Round weighted-average entry price half-away-from-zero (never truncate); both the engine `Portfolio` and the exchange `Shadow` must agree. [#3]
16. Clamp every `i128→i64` narrowing (`try_from(..).unwrap_or(i64::MAX)` or `.clamp(..)`) and saturate the final accumulation; never a bare `as i64` (it wraps) and never a bare `+` (it overflows). Use `i128` accumulators + saturating reductions in indicators. [#3]
17. Keep `FillConfig::default()` fully friction-free and gate every realism knob behind a `with_*` builder defaulting to the no-op value; prove the default path is bit-identical. [#4]
18. Claim an OCO group on its FIRST (even partial) fill so one wide bar can't fill both legs; on a partial keeper fill, shrink the sibling to the remainder rather than cancelling it. [#4]
19. Apply TIF only once an order is actionable (a dormant un-armed conditional keeps resting under IOC/FOK); emit `OrderTriggered` only after the maker-touch and OCO-claimed checks pass. [#4]
20. Apply slippage/impact to liquidity-takers only, never to passive limits (a limit can never fill worse than its price); fill fixed stops at the worse of trigger/open (gap honesty). [#4]
21. Cap reduce-only orders at the currently-reducible remainder across all orders in a bar (staged `net_after`) so they never increase or flip a position. [#4]
22. Keep `diff_reports` canonicalizing both fill lists by total key before comparing, and gate the `realized_pnl` compare on liquidation presence, not fee totals — both were false-mismatch/false-match bug sources. [#2][#4]
23. Re-run the replay trade-oracle + parity golden-master + proptest_parity on ANY trade-sim/engine/fixed-point change; a MATCH on a change that should have moved trading (or a MISMATCH you didn't intend) is a red flag. [#2][#4][#7]
24. Add a venue only as a new `akadro-venue-*` crate depending solely on `akadro-core`, implementing `DataSource`/`ExecutionClient`/`InstrumentCatalog` with zero upstream edits; route non-order actions through `VenueCommand` (D9), never bespoke trait methods. [#5][#6]
25. Have `submit` emit exactly one accept-or-reject for the id before returning, even on transport error; map venue error codes to a venue-neutral `RejectReason` and reject locally what the venue/simulator would reject. [#5]
26. Put every fetch+parse behind the `Transport` seam as reusable connector code (D18); confine live IO to a feature-gated `net.rs`; user/playground/strategy code never hand-rolls HTTP or injects raw `Bar`s. [#5][#6][#9]
27. Verify any guessed venue schema (field names, units s-vs-ms, number-vs-string, paging anchor) against the LIVE API before calling it done; a passing fixture can encode a wrong guess. [#5][#7][#9]
28. Never sleep-and-retry a SIGNED request (the baked timestamp falls outside recvWindow); only unsigned public requests retry with bounded backoff; sync server time through the `sync_clock` seam. [#5]
29. Per-venue back-fill must page from the correct anchor and always guard against no-backward-progress, dedup overlaps, clip to the window, and clamp the page limit to ≥1. [#5]
30. Keep shared venue logic (decimal↔fixed-point, funding-period inference, `FUNDING_RATE_SCALE`) in `akadro-core` and wrap it per-connector (DRY); never re-implement or copy-paste it. [#5][#6]
31. Confine `tokio`/async/runtime types to `akadro-live` — never in another crate's public signature (SoC); the sync engine talks to live IO only through the lossless-or-fail `BoundedBridge` (D7), which aborts on overflow rather than dropping events. [#2][#6]
32. Validate at construction and fail fast: `Result`/`Option` for externally-sourced input, `debug_assert` for upstream-contract invariants; never silently coerce bad input. [#6]
33. Give every growable public type both `#[non_exhaustive]` and a `::new` constructor; wrap foreign errors as owned `String` (never `#[from]`-leak); keep no foreign types in public signatures (D13: `CapSet`, `EventSink`, no `Index`). [#6]
34. Treat the production-ready gate as mandatory and observed: `test` green, `clippy --all-targets --all-features` zero warnings, `fmt --check`, `doc --all-features -D warnings`, coverage, trade-oracle, live-schema — never "should pass". [#7][#8]
35. Invoke cargo as `PATH=/home/admin/.cargo/bin:$PATH cargo +1.95.0 ...` (absolute paths, prepend PATH every call); the system `/usr/sbin/cargo` ignores `+toolchain` and the `.stderr` snapshots are pinned to 1.95. [#8]
36. A "hung" cargo is almost always an orphaned runaway test binary at ~100% CPU (suspect an accidental O(n²)/infinite loop in the code under test); diagnose with `ps` and `kill -9` the PID — `pkill cargo` won't reap it. [#8]
37. Region (98%) is the coverage gate, line (99%) is aspirational; never game the metric (no assertion-free tests, no lowering the gate, no deleting guards) — cover every exercisable branch and itemize the genuinely-untestable in §10. Update stale coverage numbers instead of letting them rot. [#7][#9]
38. Run reviews adversarially and at scale: split the attack by goal, run a per-finding verifier that defaults to "refuted," benchmark conventions against reference implementations, and write down why a disproportionate fix is deferred rather than half-implementing it. [#9]
39. Derive expected money/fill values independently (by hand from documented scales, cross-checked on real data); never let the code under test produce its own pinned expected value. [#3][#5][#9]
40. Run walk-forward through the **structural** `akadro_backtest::WalkForwardBacktest` (its `fit` step never receives `test` — fitting on OOS is inexpressible — and it owns the OOS engine run with one shared `FillConfig`). The generic both-slices runners (`run_walk_forward`/`_checked`) are an IS/OOS-leakage footgun, now gated behind the off-by-default `escape-hatch` feature; the residual (re-picking by OOS report) is out-of-band, the D2 ceiling. [#10]
41. Use the blessed analytics helpers that fail honestly: `PerformanceReport::from_equity_auto` (infers `periods_per_year` from the curve's timestamps — no `252`-for-hourly slip) and `WalkForwardSummary` (pools per-fold OOS *returns* not stitched equity, counts blow-up folds, guards `∞` WFE). Handle the `None` from `from_equity*` (blow-up / <2 points) — never `unwrap`/zero it. [#10]
42. Data enters ONLY through library fetchers + the cache (D18): the raw `HistoricalFeed::{new,from_bars,try_from_bars}` `Vec`-injectors are gated behind the off-by-default `import-bars` feature (internal callers use the `pub(crate)` `*_unchecked` seam); the blessed cache→engine path is `akadro_data::load_or_cache_feed`→`CachedFeed`. A custom `DataSource` is the sanctioned deliberate escape hatch (goal #1) — but the **agentic lab forbids it outright** (hard rule + `verify-no-custom-datasource.sh`) and builds without `import-bars`, so a lab agent has no data-import path but the fetchers/cache. [#5][#6]
43. The library does the plumbing; the user does strategy — cache every fetched series as a first-class artifact, never ship a re-download-every-run chore. **Funding is cached like klines** (`load_or_cache_funding`; `load_or_cache_perp` caches bars+funding in one call) and is **mandatory for perps**: a `PerpetualFuture` with no funding is a hard error (`load_or_cache_perp` errors, `SimulatedExchange` panics on first bar), never a silently-zero-funding run. Think several steps ahead: make the right thing automatic, the incomplete thing an error. [#6]

### How to read this playbook

| Section | Consult before you... |
|---|---|
| ### 1. The kill feature — compile-time look-ahead protection | touch `Ctx`/`Series`/`MarketView`/the brand, add a `Strategy` hook or accessor, or change the trybuild suite. |
| ### 2. Backtest↔live parity (the make-or-break goal) | change the engine loop, event ordering, clock, account-state flow, the live bridge, or anything compared by the golden-master. |
| ### 3. Fixed-point money & numeric-overflow discipline | write any money/price/qty/funding math, an `i128→i64` narrowing, or an indicator accumulator. |
| ### 4. Fill-simulation realism & the order-lifecycle bug class | edit `SimulatedExchange` fills/OCO/TIF/triggers/slippage/reduce-only or the `replay.rs` diff oracle. |
| ### 5. Venue connectors — the exchange-agnostic extension surface | add or change a venue connector: signing, schema parsing, paging, fills, scaling, rejections. |
| ### 6. Engineering principles (DRY, SOLID, SoC, LoD, fail-fast, semver) | make any structural/API change — crate boundaries, traits, errors, public types, lints. |
| ### 7. Testing, coverage & the closed-loop principle | declare a change done, write tests, measure coverage, or run the production-ready gate. |
| ### 8. Build, toolchain, CI & operational gotchas | run cargo/clippy/doc/fmt/coverage, diagnose a "hang," or push past a CI gate (fmt/clippy/doc/trybuild). |
| ### 9. Process & meta-lessons (how to run reviews, what actually worked) | plan a review, audit a money/parity path, defer a fix, or update a stale claim. |
| ### 10. Walk-forward & analytics integrity (the orchestration layer types can't protect) | run WFA, optimize parameters, build the OOS feed, or compute/aggregate analytics (Sharpe / WFE / PBO). |

### 1. The kill feature -- compile-time look-ahead protection

The kill feature is goal #3 and the project's reason to exist: it is a **compile error** for safe strategy code to read future market data through `Ctx` during a backtest. It is not one trick but **four independent layers**, all verified on rustc 1.95 / edition 2024. Never weaken any layer to make a refactor compile — touching any of them silently degrades the headline guarantee, and the only thing that catches that is the trybuild suite (see below). Every claim here is enforced by an automated, observed check.

#### The four layers (and the exact symbols that implement them)

1. **Structural append-before-call.** The strategy never receives the dataset; the engine pulls one event at a time and appends it to observed state *before* invoking the strategy. The per-event ordering is documented and implemented in `akadro-engine/src/engine.rs`: `exec.observe(...)` fills resting orders (line 238) → account events reach `on_account` → `market.push(&bar)` (line 266, "state now includes this bar") → `strategy.on_bar(bar, &mut ctx)` (line 285). Because the append happens at (3) and the call at (4), a `Series` handed to `on_bar` *physically* contains no future. `Market`/`MarketView` (`market.rs`) only ever expose what has been pushed.
2. **Backward-only `Series`** (`series.rs`). The entire read API is `latest()`, `ago(n: usize)`, `last_n(n)`, `len`, `is_empty` — and nothing else. `ago` takes a `usize` (no negative/forward offset is *expressible*) and returns `Option` (out-of-range is `None`, never a panic, never the future — `series.rs:72-79`). There is **no** forward accessor, **no** `Index` impl, and **no** way to borrow the backing slice out.
3. **Invariant generative brand** (`brand.rs:24`): `pub(crate) type Brand<'bar> = PhantomData<fn(&'bar ()) -> &'bar ()>`. It is carried by `Ctx<'bar>` (`context.rs:177`), `MarketView<'bar>` (`market.rs:104`), `Series<'bar, T>` (`series.rs:34`) and `LastN<'bar, T>` (`series.rs:118`). Each handler call gets a *fresh, late-bound* `'bar` via elided lifetimes (`fn on_bar(&mut self, bar: Bar, ctx: &mut Ctx<'_>)`, `strategy.rs:46`), so stashing a `Ctx`/`Series` anywhere longer-lived fails to unify `'bar`.
4. **Sealed construction.** `Ctx::new` is `pub(crate)` (`context.rs:181`) with all-private fields; `Series::new`/`MarketView::new` are `pub(crate)`. Only the engine, co-located in the same crate, can construct any of these.

#### D1 — why `Ctx`/`Series`/brand/loop MUST live in `akadro-engine`, not `akadro-core`

The instinct to put domain types in `core` is **unsound** and was proven so on the real toolchain. Rust has **no "pub to one specific external crate" visibility**, so:

- A truly-private `Ctx::new` in `core` is **uncallable** from a separate `engine` crate → `error[E0624]`.
- A `pub` / `#[doc(hidden)] pub` constructor lets *any* downstream crate forge `Ctx::__new(&my_own_future_data)` — verified to compile and run, defeating every other layer.

The only sound resolution: define the branded types, the `pub(crate)` constructor, **and the event loop** in one crate (`akadro-engine`). `Strategy` also lives there because its methods take `&mut Ctx<'_>` (putting it in `core` would cycle). `akadro-core` holds only vocabulary + extension-trait signatures. The permanent proof is the `forge_context` trybuild case — a *genuinely downstream* crate (`akadro-compile-tests` depends on `akadro-engine`):

```rust
// tests/ui/forge_context.rs  — MUST NOT COMPILE
use akadro_engine::Ctx;
fn main() { let _forged = Ctx::new(); }   // error[E0624]: associated function `new` is private
```

**Rule:** never make `Ctx::new` / `Series::new` / `MarketView::new` more public than `pub(crate)`, and never split the branded types into a different crate from the loop. If you must, the `forge_context.stderr` snapshot will change and CI will fail — that failure is the system working.

#### Invariance is load-bearing — never make the brand covariant

The brand shape `fn(&'bar ()) -> &'bar ()` is invariant because `'bar` appears in **both** argument and return position. A covariant marker (`PhantomData<&'bar ()>`) could be silently *shrunk* by the compiler and would give **false safety** (`brand.rs:13-17`).

```rust
// BAD — covariant marker: 'bar can be shrunk, brand gives false safety
type Brand<'bar> = PhantomData<&'bar ()>;
// GOOD — invariant in 'bar (appears in arg AND return), no coercion possible
type Brand<'bar> = PhantomData<fn(&'bar ()) -> &'bar ()>;
```

Note in `context.rs:172-177`: `gate: &'bar mut OrderGate` *also* forces `Ctx` invariant, but `_brand` is kept anyway as defense-in-depth and is the **sole** invariance source on `MarketView`/`Series`. The comment is explicit: "do NOT remove it in a refactor (its absence would silently weaken the look-ahead guarantee)."

#### The `LastN` covariance non-exploit — and the one place to re-audit

`LastN` (`series.rs:114-151`) is currently a non-exploit even though `Iterator::next` yields by value: covariance could only *shrink* `'bar`, and `next()` returns `Copy` scalars (a copy of the past, which you are free to keep). It was nonetheless given the invariant `Brand<'bar>` (2026-05-30 audit, `series.rs:118`) as defense-in-depth. **The documented warning (`series.rs:108-113`): `LastN` is the FIRST place to re-audit if a by-reference accessor like `as_slice(&self) -> &'bar [T]` is ever added** — a borrowed slice over `'bar` would resurrect the covariance-shrink hazard. Do not add such an accessor without re-proving the guarantee and adding a compile-fail case.

#### What is NOT protected (D2 threat model — state it honestly, never overclaim)

The four layers stop a **safe** strategy from reading the future **through the `Ctx` API** — the realistic bug class. They do **not** stop:

- `unsafe` in the user's strategy crate (e.g. `mem::transmute` to launder `'bar` to `'static`). Mitigation (D3): the strategy-crate template sets `#![forbid(unsafe_code)]` (as `sma-crossover` does); recommend a `cargo-geiger` CI gate.
- **Out-of-band** look-ahead: a strategy loading its *own* copy of the data and indexing `data[t+1]`, or stashing state in a `static`/`thread_local` across passes. No type system prevents this. Mitigation: a fresh engine per run + the documented contract.

The defensible claim is exactly *"no safe strategy code can read future market data through the `Ctx` API"* — never "physically impossible."

#### Trybuild discipline — the closed-loop proof (27 cases)

`crates/akadro-compile-tests/tests/trybuild.rs` runs two buckets:

- `tests/ui-pass/*.rs` — legitimate patterns that **MUST compile** (`scalar_copyout.rs`, `collect_past_copies.rs`: copying a `Copy` scalar/past values into `self` is remembering the past, not look-ahead).
- `tests/ui/*.rs` — **27** attacks that **MUST NOT compile**, each with a committed `.stderr` snapshot pinning the exact diagnostic.

The `.stderr` files are **toolchain-pinned to rustc 1.95** (the MSRV). Regenerate them only with `TRYBUILD=overwrite cargo test -p akadro-compile-tests`, and only after deliberately verifying each new diagnostic is still a *rejection for the right reason* — a wording change OR an accidental API loosening both surface as a diff here.

The suite covers every realistic stash vector so that no single escape hatch reopens the leak: `stash_via_refcell`, `stash_via_rc_refcell`, `stash_via_arc_mutex`, `stash_via_cell`, `stash_via_oncecell`, `stash_via_box`, `stash_via_closure`, `stash_via_thread_spawn`, `stash_lastn_across_bars`, `stash_signal_across_bars`, `stash_ctx_in_self` (the `&mut Ctx` itself), plus the two layer-2 sub-properties (`series_no_forward_accessor`, `series_no_index`) and the `coerce_brand_to_static` launder attempt. Critically, there is a **per-hook brand proof on all ten lifecycle hooks** — nine `stash_view_in_on_<hook>.rs` files (`on_start`/`on_account`/`on_stop`/`on_fill`/`on_order_rejected`/`on_order_canceled`/`on_liquidation`/`on_funding`/`on_timer`) plus `stash_view_across_bars.rs` for `on_bar` — so a per-hook signature slip (e.g. one hook accidentally typed with a named/outliving lifetime) **cannot silently weaken the brand** on just that hook.

#### Three GOOD-vs-BAD contrasts (drawn from the real suite)

**(a) Stashing the context — borrow-check rejects the per-call `'bar`:**
```rust
// BAD — tests/ui/stash_ctx_in_self.rs (MUST NOT COMPILE)
struct Leaky<'a> { saved: Option<&'a mut Ctx<'a>> }
impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) { self.saved = Some(ctx); }
}
// GOOD — tests/ui-pass/scalar_copyout.rs (MUST COMPILE): copy the scalar OUT
struct Memory { last_close: Price }
impl Strategy for Memory {
    fn on_bar(&mut self, bar: Bar, ctx: &mut Ctx<'_>) {
        if let Some(c) = ctx.closes(InstrumentId::new(0)).and_then(|s| s.latest()) {
            self.last_close = c;   // remembering the PAST is fine
        }
    }
}
```

**(b) No forward accessor / no `Index` — "read ahead" is inexpressible:**
```rust
// BAD — tests/ui/series_no_forward_accessor.rs: no such method (E0599)
let _ = series.ahead(1);
// BAD — tests/ui/series_no_index.rs: Series implements no Index
let _ = series[0usize];
// GOOD — the only reads go backward in time:
let now  = series.latest();   // Option<T>
let prev = series.ago(1);     // Option<T>, n: usize can't be negative
```
**Rule:** never add `ahead`/`peek`/`next_bar`/`forward`, an `Index` impl, or an `as_slice -> &'bar [T]` to `Series`. Any of these makes look-ahead expressible; the suite above is the regression net.

**(c) Forging a context downstream (the D1 proof):** see `forge_context.rs` above — `Ctx::new()` is `error[E0624]: associated function 'new' is private`. **Rule:** keep the constructor `pub(crate)` and co-located with the loop; a `pub`/`doc(hidden)` constructor would compile here and that is the exact unsound state D1 forbids.

#### Points of failure to watch in any future change

- **A new lifecycle hook** added to `Strategy` (`strategy.rs`) **must** take `&mut Ctx<'_>` (elided), and **must** get its own `stash_view_in_<hook>.rs` compile-fail case — otherwise that hook can leak while the other nine stay safe.
- **A new accessor on `Ctx`/`MarketView`** must return either a `Copy` scalar or a backward-only `Series<'bar, _>` — never a raw `&'bar [T]`, never anything carrying a forward index. (Cf. `signal`/`open_interest`/`unrealized_pnl` in `context.rs`, all backward-only and brand-respecting.)
- **Adding any new branded view type** → carry the invariant `Brand<'bar>`, make its constructor `pub(crate)`, and give it a `Debug` that hides the borrowed slice (cf. `Series`/`LastN` manual `Debug`, `series.rs:94-99,124-129` — a derived `Debug` would dump observed history into output).
- **No synchronous venue/wall-clock query** may be reachable from `Ctx` (parity D4/D5): `Ctx::now()` is logical event-time (`context.rs:201`); account state comes only from `AccountEvent`s via the read-only `Portfolio`. Adding a "what's my live balance?" or `Instant::now()` accessor would both break parity and create a mode-detection oracle.

### 2. Backtest<->live parity (the make-or-break goal)

Parity is the goal most easily broken by an innocent-looking change, because it is an emergent property of four mechanisms (one loop, event-time-only clock, events-only state, fixed-point math), not a single guarded function. The standing discipline: parity is proven by *bit-identical `RunReport` comparison*, never by reasoning. Every claim below is tied to a check you must re-run.

#### The one shared loop — never fork it per mode

There is exactly **one** event loop: `Engine::run_observed` (`crates/akadro-engine/src/engine.rs:196`), generic over `<S: Strategy, D: DataSource, X: ExecutionClient>`. Backtest and live differ **only** in the injected `D` (`HistoricalFeed` vs `MockLiveFeed`/`BoundedBridge`) and `X`. The strategy cannot tell which it is in. The `run()` path delegates to `run_observed(&mut NoOpObserver)` (`engine.rs:182-184`), and the observer is read-only by contract (`engine.rs:740` asserts `plain == observed`). 

- **Never** add a `mode: Mode` field, a `cfg!(backtest)` branch, or a second loop. The moment the strategy or loop can observe the mode, the golden-master is meaningless and parity is structurally lost.
- **Never** let `tokio`/async types escape into the engine. The async edge lives only in `akadro-live`; the engine stays sync and monomorphized.

#### The 4-step per-event ordering contract (memorize it)

For each incoming event at logical time `t` (`engine.rs:234-304`):

1. **`exec.observe(&event, now, &mut sink)`** — the execution client fills orders *already resting*. A market order submitted on bar *i* fills at bar *i+1*'s price; this is the no-execution-look-ahead guarantee (`engine.rs:237-238`, proven by `end_to_end_buy_fills_next_bar` at `engine.rs:1228` — submit sees close 100, fill price is 110, **not** 100).
2. **`drain(...)`** — the resulting `AccountEvent`s update the `Portfolio` and reach `on_account`/`on_fill`/etc. (`engine.rs:239-248`, `584-597`).
3. **`market.push(&bar)`** — observed market state is appended, so a `Series` *now* includes this bar (`engine.rs:266`, comment "(3) state now includes this bar").
4. **`strategy.on_bar(bar, &mut ctx)`** — runs last; orders it submits are routed (`route_pending`) and acknowledged (`engine.rs:285-298`).

The ordering of steps 1→4 is load-bearing for both parity *and* the kill feature. If you ever reorder "append bar" before "fill resting orders against the prior state," a market order would fill on its own submission bar — that is execution look-ahead and a parity break. The same drain/route discipline is repeated for `Resync` (`engine.rs:307`) and `Signal` (`engine.rs:346`) — signals are appended to observed state *before* the next bar (test `signal_channel_observed_before_on_bar_and_backward_only`, `engine.rs:638`), so they stay strictly backward-only too.

#### D4 — event-time clock ONLY; a wall-clock read is a parity-breaking oracle

`Ctx::now()` returns the stored logical event time and nothing else:

```rust
// GOOD — crates/akadro-engine/src/context.rs:201
pub fn now(&self) -> Timestamp { self.now }   // identical in both modes; two reads in one handler are equal
```

```rust
// BAD (would re-introduce a mode-detection oracle, never do this)
pub fn now(&self) -> Timestamp {
    Timestamp::from_nanos(std::time::SystemTime::now()... )  // differs backtest vs live → parity dead
}
```

`now` is threaded in from `event.ts()` (`engine.rs:235`) and the same value is handed to every `Ctx::new` for that event. Test `ctx_accessors_are_all_callable` asserts `ctx.now() == ctx.now()` within a handler (`engine.rs:1355`). **Rule:** no `Instant::now`, no `SystemTime`, no wall-clock is reachable from `Ctx` — if a strategy could read wall time it would behave differently live, and that difference would be a backtest-undetectable bug.

#### D5 — all account state via `AccountEvent`s; zero synchronous venue queries

The read-only `Portfolio` derives *all* state (positions, cash, realized PnL, fees) from the `AccountEvent` stream via `portfolio.apply(&event)` (`engine.rs:584`). `Ctx`'s account accessors (`cash`, `net_qty`, `realized_pnl`, `unrealized_pnl`, `avg_entry`) read that portfolio — there is **no** `exchange.get_balance()` / `get_position()` anywhere.

- **Never** add a synchronous "what's my balance/position?" call to any `ExecutionClient` or venue connector. Live, such a call would race the event stream and diverge from backtest, which has no such call to make. State must flow one way: venue → `AccountEvent` → `Portfolio` → `Ctx`.

#### D6 — deterministic seeded ChaCha8 RNG; strategies get NONE

Strategies receive no randomness. The only RNG is the engine-owned `DeterministicRng` (`ChaCha8Rng::seed_from_u64`, `crates/akadro-engine/src/rng.rs:31`). Probabilistic fill models (partials, latency jitter) are **backtest-only** and have no live analog — they are opt-in and off by default. The seed is recorded into `RunReport.seed` (`engine.rs:429`) so a run is reproducible from the report alone.

- **Never** call `thread_rng()`/`rand::random()` in library or strategy code (the module doc at `rng.rs:8` forbids exactly this). Anything nondeterministic must be injected and seeded.

#### D12 — fixed-point money is the *basis* of the golden-master

`Price`/`Qty` are `i64`, `Money` is `i128`; every `Price*Qty` widens to `i128` before multiplying (e.g. `mark_to_market` at `engine.rs:510`: `i128::from(qty) * i128::from(mark.raw())`). No floats touch money. This is what makes the fill/PnL log **bit-for-bit** reproducible across the backtest↔live boundary — `RunReport` derives `PartialEq + Eq` (`engine.rs:69`) precisely so two runs can be compared with `assert_eq!`. A single `f64` in the PnL path would make `==` flaky and silently destroy the golden-master.

#### The conservative default path MUST stay bit-identical when you add realism

`FillConfig::default()` is fully conservative: `fee_bps=0`, no slippage, no latency, no participation cap, no funding/liquidation, no cash enforcement (`crates/akadro-backtest/src/exchange.rs:39-95`). Every realism knob is opt-in and documented to keep "the conservative path bit-identical (parity preserved)" — e.g. `with_starting_cash` (`exchange.rs:354`), `with_slippage_bps` (`exchange.rs:265`), `impact_bps` ("`0` (default) keeps the conservative path bit-identical").

This is a hard contract. The cash-enforcement feature shipped this way after a real-data bug:

```rust
// BAD (the bug that shipped): spot exchange enforced NO cash balance, so a
// fee-heavy strategy could drive equity negative with no rejection (AGENTS §12).
// Always filling regardless of affordability.

// GOOD (the fix): opt-in, default OFF so the golden-master stays bit-identical.
// exchange.rs:88-95 — `starting_cash: Option<i128>`; `None` (default) disables
// the check. with_starting_cash(...) rejects unaffordable buys with InsufficientFunds.
```

**Rule:** when you add any fill realism, gate it behind a `with_*` knob whose default is the no-op value, and prove the default path is unchanged with the trade oracle below. If your "harmless refactor" moves the default-path bytes, it is not harmless.

#### THE STANDING RULE — re-run the oracle + golden-master on ANY trade-sim/engine change

Per `akadro-trade-oracle-rule`: any change to the trade-simulation pipeline (`akadro-engine` loop/portfolio, `akadro-backtest` `SimulatedExchange` fills/fees, fixed-point math, order handling) **must** re-run all three and prove the result is intentional:

```sh
# 1. the replay trade-oracle (record/replay): MATCH = trade-behaviour-neutral
cargo test -p akadro-backtest replay
# 2. the parity golden-master (backtest == threaded mock-live, across capacities)
cargo test -p akadro parity_golden_master
# 3. parity + determinism for ARBITRARY random price paths
cargo test -p akadro proptest_parity
```

The oracle (`crates/akadro-backtest/src/replay.rs`): `replay_trades(&saved, fee_bps)` re-drives a saved `RunReport`'s realized fills as canonical next-bar-open market orders through the *live* fee/PnL code, then `diff_reports(&saved, &rebuilt)` compares per-fill `(instrument, side, price, qty, fee, ts)`, fills-only `trading_fees`, and `realized_pnl` (`replay.rs:252-328`).

- **MATCH** = the change was trade-neutral (safe refactor).
- **MISMATCH** = trade behaviour moved → it had **better** be intentional. A change you *intended* that still MATCHES is *also* a red flag — it didn't take effect (`akadro-trade-oracle-rule`).
- Always pass the **same `fee_bps` the baseline used** so a mismatch isolates a *code* change, not a config change (`replay_detects_a_fee_change`, `replay.rs:414`, proves the oracle catches a real fee-code change).

#### `diff_reports` is a *fills-only* oracle — historical false-positive/negative bugs to respect

`diff_reports` has two subtleties that were the source of shipped false-mismatch/false-match bugs (AGENTS §12, "i3"); do not "simplify" them away:

```rust
// BAD (the bug that shipped): positional zip of two fill lists.
// Two equal-ts fills on different instruments delivered in feed order vs
// instrument-index order produced a FALSE MISMATCH.
for (a, b) in original.fills.iter().zip(&replay.fills) { ... }

// GOOD (the fix, replay.rs:255-268): canonicalise BOTH lists by a total key
// (ts, instrument, side, price, qty) BEFORE the positional compare.
let key = |f: &FillRecord| (f.ts.as_nanos(), f.instrument.index(), f.side.sign(), f.price.raw(), f.qty.raw());
a_fills.sort_by_key(key); b_fills.sort_by_key(key);
```

Second subtlety (`replay.rs:310-322`): `realized_pnl` is compared **only** when the saved run was *not* liquidated (`liquidation_pnl == Money::ZERO`), gated on **liquidation presence, not on fee totals**. Funding lands in `funding_net` (not `fills`, not `realized_pnl`); liquidation folds non-fill PnL into `realized_pnl` that the fills-only replay can't reproduce. The earlier fee-total gating produced both a false-match (a *funded* run wrongly skipped) and a false-mismatch (a *fee-less* liquidation wrongly compared). Tests `diff_still_compares_realized_pnl_when_only_funding_present` (`replay.rs:544`) and `diff_skips_realized_pnl_on_feeless_liquidation` (`replay.rs:562`) pin both directions. The separation of `trading_fees` vs `funding_net` vs `liquidation_pnl` on `RunReport` (`engine.rs:85-93`) exists precisely so these never conflate.

#### The golden-master itself — what it actually asserts

`crates/akadro/tests/parity_golden_master.rs`: `backtest_equals_mock_live` runs the **same `SmaCross` source** over the **same bars** through `HistoricalFeed` and through `MockLiveFeed` (a real producer thread over a bounded `sync_channel`, `crates/akadro-testkit/src/lib.rs:39-53`) and asserts `assert_eq!(backtest, mock_live)`. Two non-obvious robustness requirements:

- It uses a **deliberately tiny channel (capacity 4, then `[1,2,8,64]`)** to *force backpressure* in the producer thread (`parity_golden_master.rs:20,36`). Parity must not depend on how the transport buffers. Keep these small-capacity cases.
- `proptest_parity.rs` extends this to **arbitrary random close paths** (`50i64..200`, 12..80 bars, 64 cases) and also asserts a backtest is deterministic on rerun. A parity bug that only triggers on certain price geometry is caught here, not by the single fixture.
- `determinism.rs` proves a parallel parameter sweep (one OS thread per config) matches the sequential sweep (`parallel_sweep_matches_sequential`, `determinism.rs:18`) — no hidden global/`thread_local` state leaking across runs.

#### D7 — the live bridge is lossless-or-fail (overflow aborts, never coalesces)

The future live transport never silently drops or coalesces events — that would make live diverge from a backtest that saw every event. `BoundedBridge`/`bounded_bridge` (`crates/akadro-live/src/bridge.rs`) sets an `overflow` flag and the consuming `DataSource` **aborts** with `AkadroError::LiveBackpressure { capacity }` (`error.rs:24-28`, `bridge.rs:42-43,74-79`) rather than blocking or losing an event. `MockLiveFeed` is lossless (`testkit/lib.rs:18-19`); the abort-on-overflow behaviour is the one live-only addition and is not observable in deterministic replay.

- **Never** "fix" a slow consumer by dropping/down-sampling events on the bridge. The contract is lossless **or** loud failure.

#### The honest open loop (state it, don't paper over it)

Real-venue **latency and fill fidelity cannot be empirically tested** without live credentials/network (`akadro-closed-loop`, item 1; AGENTS §12). The substitute — and the *only* parity loop that closes here — is **structural** parity: the same engine + same strategy + same execution code, driven by the threaded `MockLiveFeed` across channel capacities and arbitrary random price paths, yielding a bit-identical `RunReport`. Real-venue latency/fill fidelity is the documented residual accuracy ceiling. When you touch this area, say explicitly that the live-fidelity loop stays open; do not imply the mock-live equality proves real-venue parity.

### 3. Fixed-point money & numeric-overflow discipline

This is the highest-severity bug class in akadro: a scale or overflow mistake silently mis-prices fills, breaks bit-identical backtest↔live parity (goal 2), and corrupts the golden-master. The rules below are not stylistic — every one traces to a shipped bug or a hard invariant. Source of truth: `crates/akadro-core/src/fixed.rs` (`Price`/`Qty` = `i64`, `Money` = `i128`, `FUNDING_RATE_SCALE = 8`).

#### Never use floats in the money path — and never reach for ambient nondeterminism

`fixed.rs:7-9` states it: floats make parity *impossible* to guarantee because rounding differs by evaluation order and target. The whole money path is scaled integers. The check is enforced: a grep of `f64|f32|as f|Instant::now|thread_rng|SystemTime` across `akadro-engine`, `akadro-backtest`, and `akadro-core` non-test code returns nothing.

- **Never** introduce `f64` into `core`/`engine`/`backtest`/`SimulatedExchange`. Floats are confined to two places by design: analytics *reporting* (`akadro-analytics`, never fed back into execution) and the *single* venue-data boundary parse where a venue sends a JSON number (e.g. KuCoin's `1.49E-4` funding rate is rendered to a fixed decimal string *before* fixed-point parsing — CHANGELOG 0.1.1, "no `f64` reaches the money path beyond the one venue-data boundary parse").
- **Never** read wall-clock or `thread_rng` in library logic (D4/D6). `Ctx::now()` is logical event-time; the only randomness is the engine-owned seeded `ChaCha8` (`SimulatedExchange` holds `rng: Option<DeterministicRng>`, `exchange.rs:183`, explicitly commented "Never `thread_rng` (D6)"). Inject all nondeterminism.

#### Widen `Price*Qty` to i128 BEFORE multiplying — there is structurally no i64 multiply (D12)

`fixed.rs:105-117`: `Price::notional` is the *only* multiplication involving a price, and it widens both operands before multiplying:

```rust
pub const fn notional(self, qty: Qty) -> Money {
    Money(self.0 as i128 * qty.0 as i128)   // widen, THEN multiply
}
```

There is deliberately **no** `Mul` impl returning `i64`. A price scaled by 1e8 times a size scaled by 1e8 overflows `i64`; the lossy path is made *unrepresentable*, not merely discouraged (`fixed.rs:18-23`). The property test `notional_equals_widened_product` (`fixed.rs:537-542`) pins it.

**BAD** (overflows — the thing the type system forbids you from writing):
```rust
let notional: i64 = price.raw() * qty.raw();        // i64*i64 wraps at ~9.2e18
```
**GOOD:**
```rust
let notional: Money = price.notional(qty);          // widens to i128 first
```
When you need a price-times-rate product yourself, follow the same order: widen to `i128`, multiply, divide, *then* clamp back (see the slippage/impact examples below). Same for `diff` (`fixed.rs:99-103`): `Price::diff` widens both to `i128` before subtracting.

#### Scale discipline is the #1 severity bug — derive expected money values independently, never copy the code's output

The CRITICAL finding of the deep bug-hunt (CHANGELOG 0.1.0, "Deep bug-hunt fixes"): the MEXC connector's `avg_price` divided a `price_scale` quote by a `qty_scale` base, mis-scaling every live fill (and the fee derived from it) by `10^qty_scale` for any symbol with nonzero base precision — i.e. essentially all of them. It broke parity. **The unit and e2e tests had pinned the *wrong* expected value, so the suite was green and the bug shipped.**

The corrected derivation is now spelled out in `crates/akadro-venue-mexc/src/parse.rs:139-160`:

**BAD (the bug that shipped):**
```rust
// quote is at price_scale, base (executed_qty) is at qty_scale.
// Dividing them directly drops the 10^qty_scale factor:
let price = quote_raw / executed_qty_raw;   // under-reports by 10^qty_scale
```
**GOOD (the fix, parse.rs:151-159):**
```rust
// price_raw = quote_raw * 10^qty_scale / base_raw
//   (the 10^price_scale cancels; the 10^qty_scale must be REINTRODUCED).
let scaled = self.cummulative_quote.raw()
    .saturating_mul(10i128.saturating_pow(self.qty_scale));
Some(Price::from_raw(
    i64::try_from(scaled / i128::from(q)).unwrap_or(i64::MAX),
))
```
Lessons, imperative:
- **When converting between two scaled quantities, write out the scale algebra in a comment and confirm the `10^k` factors cancel or are reintroduced.** A quote-over-base price needs `10^qty_scale` reintroduced.
- **Never copy the code's computed output into the assertion.** Derive the expected `Money`/`Price` value by hand — ideally on real venue data (the fix was verified behaviour-neutral on real BTC data) — so a wrong-scaled implementation fails the test instead of pinning itself.
- Venue connectors must reuse the one shared `akadro_core::decimal_to_raw`/`raw_to_decimal` (`decimal.rs`); the four-way-duplicated copies were "subtly-divergent" (CLAUDE.md §6, CHANGELOG 0.1.1 DRY). `convert.rs:31-32` now just wraps the core helper.

#### Sub-basis-point rates: use `FUNDING_RATE_SCALE = 1e-8` + `Money::mul_rate`, never a bps scale

A bps scale (`1e-4`) rounds any rate below 1 bp to zero. Real perp funding is routinely sub-1 bp (OKX BTC-USDT-SWAP ranged 0.02–0.47 bp), so the old bps-granular charge `notional · rate_bps / 10_000` silently accrued *nothing* and understated a real recurring cost (CHANGELOG 0.1.1, "sub-basis-point funding now accrues").

`fixed.rs:62` defines `FUNDING_RATE_SCALE = 8` (exactly Binance's 8-decimal `fundingRate` precision); `Money::mul_rate` (`fixed.rs:269-273`) divides by `10^FUNDING_RATE_SCALE`, derived from the *same constant* so connector scale and engine charge cannot drift.

**BAD (rounds a 0.47 bp rate to 0):**
```rust
let funding = notional.mul_bps(0);   // 0.47 bp truncated to 0 bps -> charges nothing
```
**GOOD (`exchange.rs:783-791`, `fixed.rs:266-267`):**
```rust
// rate carried at 1e-8: 0.47 bp == 4_669
assert_eq!(Money::from_raw(1_000_000_000).mul_rate(4_669).raw(), 46_690);
// in the exchange: widen via notional, then mul_rate (saturating)
let amount = bar.close.notional(Qty::from_raw(net)).mul_rate(rate);
```
Critically this is **bit-identical on the conservative default**: the coarse `funding_bps` constant is widened `×10_000` into the 1e-8 unit (`exchange.rs:785`) and `mul_rate`'s `10^8` divisor cancels it back to exactly what `mul_bps` gave — verified by `mul_rate_captures_sub_bp_and_agrees_with_mul_bps` (`fixed.rs:489-509`, `notional.mul_rate(10_000) == notional.mul_bps(1)`). Always normalize a connector's funding rate to `FUNDING_RATE_SCALE` and charge via `mul_rate`; never carry funding in bps.

#### Average-entry price must round-to-nearest (symmetric), never truncate

Truncating the weighted-average entry biases realized PnL (CHANGELOG 0.1.0, "average-entry price is rounded to nearest (was truncated)"). It must round *half away from zero* so the bias does not flip sign for negative prices. Both the engine `Portfolio` and the exchange's `Shadow` must agree — `exchange.rs:130-138`:

```rust
// Round half away from zero — symmetric (so the bias does not flip for
// negative prices), matching the engine portfolio's average.
let half = total_qty / 2;
let rounded = if total >= 0 { (total + half) / total_qty }
              else          { (total - half) / total_qty };
self.avg = rounded as i64;
```
**BAD:** `self.avg = (total / total_qty) as i64;` — truncates toward zero, biases PnL, and diverges between the two position trackers.

#### Clamp i128→i64 with `try_from`/`clamp`, then SATURATE the final add — never a bare `as i64`

A bare `as i64` on an out-of-range `i128` *wraps* (e.g. to a negative price). Every `i128→i64` narrowing in the fill model was hardened to clamp, and every final accumulation saturates (CHANGELOG 0.1.1, "clamps (not truncates) its `i128` intermediates ... and saturates the final adjustment"). Two correct idioms:

- **Positive-only narrowing** → `i64::try_from(x).unwrap_or(i64::MAX)` (slippage `exchange.rs:525-527`; impact `:546-551`; trailing-percent offset `:673-675`; RSI seed `oscillator.rs:77-78`; Bollinger spread `channel.rs:82`).
- **Signed narrowing** → `x.clamp(i64::MIN as i128, i64::MAX as i128) as i64` (the participation cap, `exchange.rs:981-982`).
- **EMA step** picks the bound by sign: `unwrap_or(if step > 0 { i64::MAX } else { i64::MIN })` (`average.rs:116-120`).

**BAD (slippage that wraps to a negative price on an extreme input):**
```rust
let adj = (base.raw() as i128 * slippage_bps as i128 / 10_000) as i64;  // wraps
Price::from_raw(base.raw() + side.sign() * adj)                          // can overflow
```
**GOOD (`exchange.rs:524-528`):**
```rust
let adj = i64::try_from(
    i128::from(base.raw()) * i128::from(self.config.slippage_bps) / 10_000
).unwrap_or(i64::MAX);
Price::from_raw(base.raw().saturating_add(side.sign().saturating_mul(adj)))
```
The same pattern guards `Shadow::apply` position math (widen before `.abs()`, `saturating_mul`/`saturating_add`, `exchange.rs:122-140`) and `settle_cash` (`exchange.rs:384-390`) — an `i64::MIN` `.abs()` would otherwise panic in debug. `mul_bps` itself saturates its intermediate product (`fixed.rs:249-251`, `self.0.saturating_mul(bps as i128) / 10_000`) — see `mul_bps_saturates_instead_of_overflowing` (`fixed.rs:476-487`); the comment notes the old `self.0 * bps` overflow-panicked in debug.

#### `Money` arithmetic is saturating by contract — use it, don't hand-roll

Balances must never silently wrap. `Money::saturating_add`/`saturating_sub`/`neg` (the last via `saturating_neg`, `fixed.rs:227`, so negating `i128::MIN` doesn't overflow — a fixed minor bug) are the only blessed ops. The engine `Portfolio` uses `saturating_add`/`saturating_sub` at every accumulation point (`portfolio.rs:242,252-253,273,278,293-294,324`). `Price::checked_add`/`checked_sub`/`Qty::checked_add` return `Option` for genuine overflow at the `i64` tier (`fixed.rs:81-97,157-173`) — propagate that, never wrap.

#### Indicators need i128 accumulators and saturating reductions

A `window`-sized sum of large fixed-point prices overflows `i64`; every accumulator is `i128` (CHANGELOG 0.1.1 "RSI/Bollinger/EMA widen to i128 and saturate"; marked `m31` throughout):

- **SMA** `sum: i128`, subtract the evicted sample in `i128` (`average.rs:18,42,46`).
- **RSI** `seed_gain`/`seed_loss` are `i128`; the first difference is widened (`i128::from(input) - i128::from(prev)`, `oscillator.rs:67`); Wilder smoothing widens so `avg*(p-1)` can't overflow (`:87-88`); `level()` widens `10_000 * avg_gain` (`:46-52`, comment: "`10_000 * avg_gain` overflows i64").
- **Bollinger** variance widens *before* subtracting the mean (`d = i128::from(x) - i128::from(mid)`), uses a **saturating square** then a **`fold(0i128, i128::saturating_add)`** so `deviation²` summed can't overflow even i128 (`channel.rs:71-82`).
- **EMA** widens the `*2` step and clamps the result back (`average.rs:115-120`).

**BAD:** `let mid = (self.buf.iter().sum::<i64>()) / self.window as i64;` — the `i64` sum overflows for a window of realistic fixed-point prices.
**GOOD:** accumulate in `sum: i128`, then `let mid = (self.sum / n) as i64;` where the quotient is provably in range (`channel.rs:66-67`).

Also fail-fast on inputs that would invert the math: `Bollinger::new` panics on `window == 0` *or* `k <= 0` (a non-positive `k` would leave `upper < lower`, `channel.rs:40-42`); `Sma`/`Ema`/`Rsi::new` panic on a zero period; `Macd::new` panics on `fast >= slow`. And report `warm_up_bars()` as the true first-`Some` call (RSI is `period + 1`, MACD is `slow + signal - 1`, not `slow`) so a strategy doesn't act on an unwarmed indicator (`oscillator.rs:92-97`, `average.rs:186-191`).

#### Keep the conservative default bit-identical, and re-run the trade oracle

Every realism knob (slippage, impact, funding scale, maker-fill) is opt-in and `0`/`None`-default precisely so the parity golden-master stays bit-for-bit identical (`exchange.rs:538` returns `slipped` unchanged when `impact_bps == 0`; the funding widen/divide cancels). When you touch any of this fixed-point/fill math, the production gate requires re-running the **replay trade-oracle + parity golden-master** to prove the default path is unchanged (CLAUDE.md §10; every CHANGELOG entry closes with "replay trade-oracle + parity golden-master bit-identical"). A money-math change that *should* move the result but leaves the oracle matching a stale report is itself a red flag.

### 4. Fill-simulation realism & the order-lifecycle bug class

This section governs `crates/akadro-backtest/src/exchange.rs` (the `SimulatedExchange` matching engine) and `crates/akadro-backtest/src/replay.rs` (the regression oracle). Fill simulation is where parity (goal 2) is won or lost: a fill that the model gets wrong is a backtest that lies. Every lesson here is grounded in a shipped-and-fixed bug.

#### The cardinal rule: conservative default, opt-in realism

`FillConfig::default()` (`exchange.rs:42-96`) is **fully friction-free** — `fee_bps`, `slippage_bps`, `impact_bps`, `latency_bars=0`, `max_participation_bps=None`, `funding_bps=0`, `liquidation_bps=None`, `maker_fill_prob_bps=None`, `starting_cash=None`. The ONLY timing conservatism is structural: a market order submitted on bar *i* fills at bar *i+1*'s open; a limit fills only when a later bar reaches it (`exchange.rs:5-11`).

- **Always** add realism behind a `with_*` builder that defaults to the zero/`None` value, and **always** prove the default path is bit-identical (parity golden-master + replay oracle). Every CHANGELOG realism addition repeats this clause: *"conservative defaults keep results bit-identical (parity preserved)"*.
- **Why it's load-bearing:** the parity golden-master and `proptest_parity` compare a *historical* feed to a *mock-live* feed for bit-identical `RunReport`s. Any realism that fires on the default path silently breaks that contract for every user who didn't opt in.
- The bit-identity must survive even internal refactors. When funding moved from bps to `FUNDING_RATE_SCALE` (1e-8), the constant path was preserved by widening `funding_bps * 10_000` (`exchange.rs:785`) so `mul_rate`'s `10⁸` divisor cancels back to `mul_bps` exactly (`exchange.rs:776-786`). The CHANGELOG calls this out explicitly (line 174-179): "the 10_000× widen and `mul_rate`'s 10⁸ divisor cancel back to `mul_bps`."

#### OCO claim-on-first-fill: the intra-bar double-execution bug class

A bracket (take-profit limit + stop-loss stop) in one `oco_group` must never have BOTH legs execute on a single wide bar. This is enforced by **claiming the group on its first (even partial) fill** (`exchange.rs:988-995`, `1074-1085`, `1176-1180`).

GOOD (the claim is on the FIRST fill, regardless of completeness) — `exchange.rs:1176-1180`:
```rust
// Claim the OCO group on this (first) fill, recording the keeper's
// cumulative filled qty and whether it completed.
if let Some(g) = r.order.oco_group
    && !claimed_oco.iter().any(|(cg, ..)| *cg == g)
{
    claimed_oco.push((g, r.id, r.filled, complete));
}
```
And the guard that cancels a sibling whose group is already claimed *this bar* (`exchange.rs:1074-1085`):
```rust
if let Some(g) = r.order.oco_group
    && claimed_oco.iter().any(|(cg, cid, _, _)| *cg == g && *cid != r.id)
{
    sink.emit(AccountEvent::OrderCanceled { id: r.id, reason: CancelReason::OcoTriggered, ts: now });
    continue;
}
```

BAD (the original bug, per CHANGELOG line 463-465): one-cancels-other was applied only *after* the whole bar, so "a single wide bar could fill both bracket legs." Equivalent broken logic would claim only on a *complete* fill:
```rust
// BAD: claim gated on `&& complete` re-opens the intra-bar double-exec hole.
if let Some(g) = r.order.oco_group && complete && !already_claimed(g) {
    claimed_oco.push((g, r.id));   // a partial keeper fill leaves the group UNclaimed,
}                                  // so the sibling also fills on this same wide bar.
```
Regression test: `oco_both_legs_fillable_on_one_wide_bar_fills_only_one` (`exchange.rs:1572-1617`).

#### The OCO *partial*-fill subtlety: reduce the sibling, don't cancel it

The naive partial-fill fix is wrong in the other direction. If a partial take-profit cancels the protective stop, the still-open balance is left unprotected (CHANGELOG line 14-16). The correct behaviour (`reduce_or_cancel_oco_siblings`, `exchange.rs:716-745`):

- **Complete** keeper fill → cancel every sibling (`OcoTriggered`).
- **Partial** keeper fill → shrink each sibling to the keeper's unfilled remainder (`r.filled = r.filled.max(keeper_filled)`, `exchange.rs:740`) and keep it resting.
- A sibling fully covered by the keeper (`qty - keeper_filled <= 0`) → cancel (`exchange.rs:728`).

GOOD (`exchange.rs:726-741`):
```rust
if keeper_complete || r.order.qty.raw().saturating_sub(keeper_filled) <= 0 {
    sink.emit(AccountEvent::OrderCanceled { id: r.id, reason: CancelReason::OcoTriggered, ts: now });
    continue;
}
r.filled = r.filled.max(keeper_filled);  // shrink to the keeper's remainder; keep resting
```
BAD:
```rust
// BAD: a partial take-profit fill strips the stop for the rest of the position.
for sibling in &group { sink.emit(OrderCanceled { id: sibling.id, .. }); }
```
Regression test: `oco_partial_keeper_fill_reduces_sibling_not_cancels_it` (`exchange.rs:1619-1695`) — TP fills 5 of 10 (50% cap), the stop must stay resting reduced to 5, then fill exactly 5 on the next bar.

#### TIF governs an order only once it is ACTIONABLE

A dormant (un-triggered) conditional — `Stop`, `StopLimit`, `MarketIfTouched`, `TrailingStop` — must keep resting on a quiet bar **regardless of IOC/FOK**. TIF only applies once the order can actually act. A bare `tif == Gtc ? rest : expire` wrongly expires a resting stop on its first quiet bar (CHANGELOG line 16-18).

GOOD (`exchange.rs:1043-1055`):
```rust
let dormant_conditional = !r.armed
    && matches!(r.order.kind,
        OrderKind::Stop { .. } | OrderKind::StopLimit { .. }
        | OrderKind::MarketIfTouched { .. } | OrderKind::TrailingStop { .. });
if r.order.tif == TimeInForce::Gtc || dormant_conditional {
    self.resting.push(r);
} else {
    sink.emit(AccountEvent::OrderExpired { id: r.id, ts: now });
}
```
BAD:
```rust
// BAD: expires a not-yet-triggered IOC/FOK stop on a quiet bar — it never gets to fire.
if r.order.tif == TimeInForce::Gtc { self.resting.push(r); }
else { sink.emit(OrderExpired { .. }); }
```
The `!r.armed` gate is essential: an *already-armed* conditional that couldn't act, or a plain `Limit` (always actionable — it could have crossed), correctly expires under IOC/FOK. Regression test: `ioc_dormant_conditional_keeps_resting_then_fires` (`exchange.rs:1697-1743`).

#### `OrderTriggered` must fire AFTER the OCO-claimed check

A stop/MIT/trailing leg triggers on its first fill. Emit the `OrderTriggered` **only after** the maker-touch and OCO-claimed checks pass — otherwise a leg about to be OCO-cancelled emits a spurious trigger (CHANGELOG line 19-21). Contrast the two trigger sites:

- A `StopLimit` *arms* inside `evaluate` (`exchange.rs:626-629`); its trigger surfaces at `exchange.rs:1024-1028` (the `was_armed → r.armed` edge).
- A `Stop`/`MIT`/`TrailingStop` triggers on first fill, emitted at `exchange.rs:1090-1100`, which sits *after* the maker-touch `continue` (`1063`) and the OCO-cancel `continue` (`1079`).

GOOD (`exchange.rs:1086-1100`) — trigger is downstream of the OCO cancel branch:
```rust
// ... after the maker-touch and OCO-claimed `continue`s above ...
if !r.armed && matches!(r.order.kind, OrderKind::Stop { .. } | ..) {
    r.armed = true;
    sink.emit(AccountEvent::OrderTriggered { id: r.id, ts: now });
}
```
BAD: emitting the trigger up in `evaluate`/before the OCO check makes a sibling that gets `OcoTriggered`-cancelled this bar *also* emit `OrderTriggered` — a phantom lifecycle event the strategy's `on_timer`/event hooks will react to. Regression test: `oco_canceled_leg_emits_no_spurious_trigger` (`exchange.rs:1745-1794`).

#### Slippage applies to liquidity-takers ONLY, never to passive limits

A passive `Limit`/`StopLimit` is price-guaranteed: it can never execute worse than its limit. Adverse slippage and volume-impact apply only to liquidity-taking kinds (market / stop / trailing / MIT). The original bug applied slippage to passive limits (CHANGELOG line 465-466).

GOOD (`exchange.rs:1147-1151`):
```rust
let price = match r.order.kind {
    OrderKind::Limit { .. } | OrderKind::StopLimit { .. } => base,  // no slippage/impact
    _ => self.fill_price(base, r.order.side, want, bar.volume.raw()),
};
```
BAD:
```rust
// BAD: slipped a passive limit to 101 when its limit was 100 — fills better than the limit price.
let price = self.slipped(base, r.order.side);
```
Regression test: `limit_fill_is_never_worse_than_limit_under_slippage` (`exchange.rs:1546-1570`). The same discipline appears in the cash-guard reservation (`buy_cost_qty`, `exchange.rs:489-498`): a passive limit reserves at its limit (`taker=false`), a taker reserves slipped + **worst-case 100%** impact so the reservation never under-counts.

#### Fixed stops fill at the WORSE of trigger/open (gap honesty)

A fixed stop's trigger is known *before* the bar, so a gap straight through it cannot fill optimistically at the trigger — it fills at the worse of trigger/open (CHANGELOG audit, "fixed stops filled at the trigger through a gap; now at the worse of trigger/open"). `stop_fill` (`exchange.rs:897-902`):
```rust
fn stop_fill(side: Side, trigger: i64, open: i64) -> Price {
    Price::from_raw(match side {
        Side::Buy  => trigger.max(open),  // a buy stop's worse price is higher
        Side::Sell => trigger.min(open),  // a sell stop's worse price is lower
    })
}
```
**Distinction to preserve:** a `TrailingStop` derives its trigger from the *same* bar's high/low intra-bar (`exchange.rs:680-695`), so the trigger isn't known at the open — it fills *at* the computed trigger, and `stop_fill` deliberately does not apply (`exchange.rs:893-896`). A `MarketIfTouched` is a *favourable* touch and fills at the trigger by design (`exchange.rs:653-664`; tests `market_if_touched_gap_down_buy`, `..._gap_up_sell`, `exchange.rs:2056-2102`).

#### `reduce_only` must never increase/flip, even across concurrent orders in one bar

A reduce-only order is rejected at submit when flat or same-side (`rejection`, `exchange.rs:435-443`, `RejectReason::WouldIncreasePosition`). But the deeper bug class is *intra-bar*: several reduce-only orders in the same bar must not collectively overshoot zero. The fill loop tracks a staged `net_after` (`exchange.rs:1000-1003`, advanced at `1167`) so each reduce-only fill caps at the *currently reducible* remainder (`exchange.rs:1109-1121`):
```rust
let reducible = if net_after != 0 && net_after.signum() != r.order.side.sign() {
    net_after.abs()
} else { 0 };
if want >= reducible { want = reducible; reduce_exhausted = true; }
```
An oversized reduce-only completes when the position flattens even if its nominal qty was larger (`complete = ... || reduce_exhausted`, `exchange.rs:1155`); a reduce-only with nothing left to reduce is cancelled, never left resting doing nothing (`exchange.rs:1127-1135`). Tests: `reduce_only_caps_at_position_and_does_not_flip` (M10) and `reduce_only_rejects_unless_it_reduces` (`exchange.rs:1351`, `1867`).

#### Fixed-point overflow: clamp `i128` intermediates into `i64`, saturate the final add

Every slippage/impact/trail/cap computation widens to `i128` to multiply, then must **clamp** (not `as i64` truncate) back, and **saturate** the final price adjustment (CHANGELOG line 21-22; per-site notes m8-m11, m40). Examples: `slipped` (`exchange.rs:519-529`), `fill_price` impact (`exchange.rs:536-557`), `TrailKind::Percent` (`exchange.rs:673-675`), the participation `cap` (`exchange.rs:977-983`), `Shadow::apply` (`exchange.rs:117-151`, widen before `.abs()` to dodge the `i64::MIN` panic), funding via `Money::mul_rate` (`exchange.rs:791`), `settle_cash` saturating (`exchange.rs:384-390`).

GOOD (`exchange.rs:525-528`):
```rust
let adj = i64::try_from(i128::from(base.raw()) * i128::from(self.config.slippage_bps) / 10_000)
    .unwrap_or(i64::MAX);
Price::from_raw(base.raw().saturating_add(side.sign().saturating_mul(adj)))
```
BAD:
```rust
// BAD: `as i64` truncates (wraps) a large product; bare + can overflow at i64::MAX.
let adj = (base.raw() as i128 * bps as i128 / 10_000) as i64;
Price::from_raw(base.raw() + side.sign() * adj)
```

#### The `diff_reports` false-mismatch / false-match traps

`replay.rs::diff_reports` is the trade oracle's comparator. Two structural pitfalls, both shipped-and-fixed:

1. **Equal-ts cross-instrument ordering** (CHANGELOG line 469-470). A positional `zip` of two fill lists falsely mismatches when equal-timestamp fills from different instruments arrive in feed order vs canonical order. Fix: sort **both** lists by a total key `(ts, instrument, side, price, qty)` before the positional compare (`replay.rs:255-268`). Test `diff_is_order_independent_for_equal_ts_cross_instrument_fills` (`replay.rs:496-521`).
2. **Funding/liquidation land in scalars, not in `fills`** (CHANGELOG line 470-471). Funding flows into `funding_net` and liquidation folds non-fill PnL into `realized_pnl`, but the replay re-drives only `fills` and cannot reproduce them. So: compare the **fills-only** fee total (`replay.rs:294-308`), and gate the `realized_pnl` compare on **liquidation presence** (`liquidation_pnl == 0`), NOT on fee totals (`replay.rs:309-322`). Gating on fees produced both a funded false-match and a fee-less-liquidation false-mismatch (`i3`). Tests `diff_does_not_false_mismatch_on_non_fill_fees`, `diff_still_compares_realized_pnl_when_only_funding_present`, `diff_skips_realized_pnl_on_feeless_liquidation` (`replay.rs:523-578`).

#### The replay oracle is the regression net — run it on EVERY change here

`replay.rs` re-drives a saved `RunReport`'s exact `(instrument, side, price, qty, ts)` trade sequence as canonical next-bar-open market orders through the **real** engine + `SimulatedExchange` + portfolio, with no strategy logic (`replay.rs:5-42`, `196-218`). It validates the fee/PnL/accounting path, *not* the original fill generation (it doesn't know which limit/stop level crossed). The closed-loop rule (CLAUDE.md §10, every CHANGELOG "Closed loop" line): **any** trade-sim/engine change must re-run the replay trade-oracle **and** the parity golden-master to prove the default path is bit-identical. A `MATCH` after your change means you did not silently move the trading result; a `MISMATCH` you didn't intend is a red flag.

#### Other attention points

- **Liquidation cancels all working orders on the instrument** (`check_liquidation`, `exchange.rs:845-861`, M15) — otherwise a resting limit/stop silently re-opens a position the strategy never re-requested. Test `liquidation_cancels_resting_orders` (`exchange.rs:1410`).
- **Per-instrument funding interval** (`funding_interval` map, `exchange.rs:336-339`, `750-754`, m12) — a per-instrument `with_funding_schedule` must NOT clobber the global cadence for every other instrument.
- **Probabilistic maker-fill** (`exchange.rs:559-591`, `1058-1070`) is backtest-only by construction (a live venue reports its own fills, D6), draws from the engine-owned seeded `ChaCha8` in deterministic feed order, and on a *miss* the leg rests/expires WITHOUT claiming its OCO group. `config_seed` reports `None` until the RNG is actually drawn (`exchange.rs:960-964`, i4) — never confuse "no randomness" with "seed 0".
- **`post_only` on a non-limit kind is rejected** (`exchange.rs:413-420`, m1), not silently filled — it always takes liquidity, a contradiction. A just-armed post-only stop-limit measures marketability at the *trigger*, not the pre-activation `bar.open` (`exchange.rs:637-647`, M8/M9).
- **The participation cap is one shared running counter per bar/instrument** (`exchange.rs:977-983`, depleted at `1170-1172`) so N orders can't each take the full bar volume.

### 5. Venue connectors -- the exchange-agnostic extension surface

The open/closed promise (goal 1) is concrete: a venue is a **new `akadro-venue-*` crate depending only on `akadro-core`** that implements three traits — `DataSource`, `ExecutionClient`, `InstrumentCatalog` — with **zero edits** to core/engine/strategy. Every existing connector (`mexc`, `binance`, `okx`, `bybit`, `kucoin`, `dex`) is a worked instance. This section captures the points of failure that the live API actually punished.

#### The three-trait contract (and the no-op defaults that make it small)

The entire surface is in `crates/akadro-core/src/traits.rs`:
- `DataSource::next_event(&mut self) -> Option<Event>` (line 42) — normalize venue market data to time-ordered `Event`s, `None` at exhaustion.
- `ExecutionClient::submit` (line 68) + `observe` (line 80) emit `AccountEvent`s through `&mut dyn EventSink`. There is **no synchronous "what's my balance/position?" query** — that is D5/parity, not an accident; do not add one.
- `InstrumentCatalog::spec(id) -> Option<&InstrumentSpec>` (line 128).

`cancel` (95), `command` (101), `sync_clock` (113), `config_seed` (120) all default to no-ops. **Never invent bespoke trait methods for leverage/margin/approvals** — route them through the venue-neutral `VenueCommand` enum (D9, `order.rs:359`: `SetLeverage`/`SetMarginMode`/`ApproveAsset`/`SetCancelOnDisconnect`) inside `ExecutionClient::command`. That is what keeps the surface additive.

**`submit` MUST emit exactly one accept-or-reject for `id` before returning, even on transport error** (traits.rs:55-63). MEXC obeys this at `client.rs:489-509`: a transport `Err(_)` still emits `OrderRejected{ VenueRejected }`. Returning silently leaves `id` in limbo and hangs a strategy waiting on the ack.

```rust
// GOOD (mexc client.rs): transport failure is still a terminal outcome
match self.transport.send(&http) {
    Ok(resp) => match parse_order_ack(&resp.body) { /* Accepted | reject(code) */ },
    Err(_) => reject(sink, RejectReason::VenueRejected),  // never return silently
}
```

#### The Transport seam is THE test boundary; `net.rs` is the ONLY live-IO file

Every connector defines a `Transport` trait + `MockTransport` returning canned responses and recording `sent` requests (`mexc/transport.rs:66-110`). All signing/parsing/normalization runs over `MockTransport` in unit tests with zero network. The real blocking `reqwest` client lives **only** in a feature-gated `net.rs` (`#[cfg(feature = "net")] mod net;`), excluded from the coverage gate and exercised only by a credential-gated `#[ignore]` live test. The CI command (AGENTS §10) excludes `venue-*/src/net.rs` by regex precisely because it is the one untestable-without-a-venue file.

**Always put a new fetch+parse behind the seam as a reusable connector entry point** (D18). Catalog/feed/funding fetchers build the URL and parse the body themselves — `MexcCatalog::fetch` (client.rs:240), `*Feed::with_range`, `fetch_funding_history`/`_paged` (futures.rs:419/453). The MEMORY rule is blunt: *"WHY raw request in PLAYGROUND??? WHOLE point of library that user does not make raw request ... so no leakage points."* User/strategy/playground code must **never** hand-roll an HTTP call or hand-inject downloaded `Bar`s into a feed.

**Download progress = connector emits, consumer renders (no UI dep in the libs).** A long `with_range` back-fill runs the whole multi-page download inside one opaque `next_event()`, so the only way to surface progress is a callback *inside* the connector's `backfill()` loop. The seam: `with_progress(impl FnMut(akadro_core::BackfillProgress) + 'static)` on the paged feeds (`MexcKlineFeed`/`MexcFuturesKlineFeed`/`OkxCandleFeed`; binance/bybit/kucoin can adopt the identical builder), emitting a venue-neutral `BackfillProgress {pages, bars, frontier_ms}` per page. Off by default (`None`) → bit-identical, never fires on a cache hit. The terminal bar lives in the *consumer* (the playground's dependency-free `download_bar`); never put `indicatif`/terminal I/O in a library crate (SoC/D13). Total pages isn't known up front (venue caps, short pages, retention gaps) — the consumer estimates it from the requested span (`~`), or shows a live count.

#### HARDEST LESSON: never ship a guessed venue schema — curl the live API

The most expensive class of bug: a fixture written to match a *guessed* schema passes CI while being wrong against the real venue.

```rust
// BAD (the KuCoin funding schema that "passed" against a wrong-guess fixture):
struct FundingRow { value: f64, timePoint: i64 }   // guessed field names + type

// GOOD (kucoin/lib.rs:429-437, verified live 2026-06-01 by curling the endpoint):
struct FundingRow {
    timepoint: i64,                         // lowercase 'p' — not `timePoint`
    #[serde(rename = "fundingRate")]
    funding_rate: serde_json::Value,        // a JSON NUMBER in SCIENTIFIC notation: 1.49E-4
}
```

The fix forced two disciplines now baked into every funding parser: (1) confirm **field names, units (s vs ms), and number-vs-string** against the live response, and (2) accept `serde_json::Value` for the rate and render it through a fixed (non-exponential) decimal before `decimal_to_raw`, so `1.49E-4` is not mangled and a sub-bp rate is not rounded to 0 (`kucoin/lib.rs:444-455`, mirrored in `futures.rs:359-370`). Other live-verified facts you must not re-guess: MEXC spot has **no `timeInForce` param** (TIF is encoded into `type`: `LIMIT`/`IMMEDIATE_OR_CANCEL`/`FILL_OR_KILL`/`LIMIT_MAKER`, client.rs:403-426), MEXC REST klines have **no 8h interval** (WS-only), and `exchangeInfo` tradability gates on `status=='1'` not `isSpotTradingAllowed`.

#### Signing: a SIGNED request must never be slept-and-retried

A signed request bakes a `timestamp` into the HMAC. Any cumulative backoff pushes that timestamp past `recvWindow` and earns MEXC error `700003`. `net.rs:86-108` encodes the rule exactly:

```rust
let signed = request.api_key.is_some();
// On a transport error:
if signed { return Err(e); }            // never sleep-and-retry a baked signature (m33)
// On a 429:
if resp.status == 429 && !signed && attempt < 3 { sleep(...); continue; }  // ONLY unsigned retries
return Ok(resp);                        // a signed 429/418 returns immediately
```

Return the 429/transport error so the **bar-cadence poller re-signs next bar**. Only **unsigned public** requests (klines/exchangeInfo) retry with bounded backoff; an 418 IP-ban returns immediately (sleeping minutes on the synchronous engine thread is unacceptable).

Two clock pitfalls go with this. **Server-time sync flows through the `ExecutionClient::sync_clock` seam** (client.rs:610-616) so it is reachable generically even when wrapped by `ChannelExec`; `sync_time` (client.rs:346) layers a `serverTime − clock_ms` offset on top to survive host-clock drift > `recvWindow`. And **never pin a fixed signing timestamp in live** — `with_timestamp_ms`/`with_timestamp` (bybit/kucoin/okx) are test-only; the doc warns that a reused timestamp soon falls outside the window so signed orders start being rejected (m36). Unset, they sign with the handler's event time, which under a live event-time clock tracks wall-clock.

#### Backward-paging anchors differ per venue — guard, dedup, clip

The single most subtle market-data bug: assuming all venues page the same way. **MEXC contract-klines anchor on `end` and IGNORE `start`** once the range exceeds the cap, so a forward `start` cursor re-fetches the same recent window forever (the bug that capped a year-long request at one page). The fix pages the **`end` cursor backward** (futures.rs:664-703); Binance, by contrast, pages forward by `startTime`. Every range back-fill must carry the same three safeguards regardless of anchor:

```rust
// GOOD (mexc futures.rs backfill): no-backward-progress guard + dedup + window-clip
let next_end = oldest_open_secs - bar_secs;
if next_end >= cursor_end { break; }    // guards a venue that ignores the cursor (no infinite loop)
cursor_end = next_end;
// ...after the loop:
all.sort_by_key(|b| b.ts.as_nanos());
all.dedup_by_key(|b| b.ts.as_nanos());          // overlapping pages
all.retain(|b| { let open = b.ts.as_nanos()/1_000_000 - bar_ms; open >= start_ms && open < end_ms });
```

Same pattern in `okx` (`after` cursor, lib.rs:721), `bybit` (`endTime`, lib.rs:743), `kucoin` (`to`, lib.rs:691). Also **clamp the page limit** (`with_limit` clamps to `[1, MAX]` at client.rs:101 / bybit `clamp(1,1000)`) — a `0` limit makes the cursor advance by zero and spins the loop forever (m17/bug class 8). A mid-range page error should **keep the pages already fetched** (client.rs:174-179) rather than discard a long back-fill over one hiccup.

#### Unclosed last candle + close-time stamping

Bars are stamped at **CLOSE time in nanoseconds**, never open time — venues report open time, so add one interval. OKX has a per-row `confirm` flag (index 8) so it can **skip the still-forming bar** (`okx/lib.rs:414-420`); **most REST klines have no closed flag** (MEXC/Bybit/KuCoin), so the parser doc must warn the caller and the safe play is a bounded `with_range` ending before `now`. The interval must be the **authoritative** one passed in, not derived from consecutive opens, or a **single-bar page** gets left at open time and stitches inconsistently with multi-bar pages (m32):

```rust
// GOOD (mexc futures.rs:297-303): caller's interval close-stamps even a lone bar
let interval_secs = if interval_secs > 0 { interval_secs }
                    else if n >= 2 { d.time[1] - d.time[0] } else { 0 };
```

KuCoin fails *fast* on an unrecognized candle type on every path (lib.rs:377-381) rather than silently stamping at open with a zero offset (m31). Note KuCoin's surprising row order `[time, open, **close, high, low**, volume, turnover]` (lib.rs:397-406) — read the columns, don't assume OHLC order.

#### DRY: wrap the shared core helpers, never re-implement them

Decimal↔fixed-point (`decimal_to_raw`/`raw_to_decimal`/`scale_of`) and `infer_funding_period_ms` are **venue-neutral and live in `akadro-core`** — the four-way-duplicated decimal helpers that preceded this were the source of divergent edge-case handling. A connector wraps them, mapping `None → its own ...Error`:

```rust
// GOOD (mexc convert.rs:31, okx/bybit/kucoin all identical-shaped):
pub fn decimal_to_raw(s: &str, scale: u32) -> Result<i64, MexcError> {
    akadro_core::decimal_to_raw(s, scale)
        .ok_or_else(|| MexcError::Parse(format!("bad decimal {s:?} at scale {scale}")))
}
pub use akadro_core::{raw_to_decimal, scale_of};      // okx/bybit/kucoin
pub use akadro_core::FUNDING_RATE_SCALE;              // 1e-8, so connector + engine mul_rate can't drift
```

(Exception: MEXC keeps its own `raw_to_decimal` because order params must **pad to exactly `scale` places** — the precise byte form the signature is computed over, convert.rs:37-63. Document any such intentional divergence.) **No `f64` in the money path** — `unit_to_raw` (futures.rs:104-114) routes a JSON `Number` through `n.to_string()` then `decimal_to_raw`, so tick/lot derivation stays integer (D12).

#### Costs are multi-asset by design (D10), and economics are default-on

`Fill` carries `costs: SmallVec<[Cost; 2]>`. The DEX connector is the proof that one swap charges two costs in **two different assets** — an LP fee in the quote asset *and* gas in the chain's native asset (`dex/lib.rs:147-168`). When modelling costs, use `> 0` not `!= 0`: a negative fee/impact/gas must be treated as none, never credited as a rebate (m9). Funding is applied **by default** for perps (connector-owns-economics memory); the connector supplies the schedule via `fetch_funding_history` at `FUNDING_RATE_SCALE`.

#### The critical scaling bug — parity's worst enemy lived in a connector

The single critical finding of the 62-agent bug hunt was in the MEXC connector, and the unit/e2e tests had **pinned the wrong value**, masking it:

```rust
// BAD (the avg_price that shipped): divides a price_scale quote by a qty_scale base
let price = quote_raw / qty_raw;              // off by 10^qty_scale for any nonzero base precision

// GOOD (client.rs:556-561): re-scale the quote by 10^qty_scale before dividing,
// and use the MARGINAL tranche (delta), not the cumulative VWAP (M6)
let delta_quote = cum_quote - p.prev_quote;
let scaled = delta_quote.saturating_mul(10i128.saturating_pow(p.qty_scale));
let price = Price::from_raw(i64::try_from(scaled / i128::from(delta_qty)).unwrap_or(i64::MAX));
```

Lessons: a connector mis-scaling fills silently breaks backtest↔live parity (the fills *and* the fees derived from them); and a green test is worthless if it asserts a number you computed the same wrong way. Two related fill-tracking rules from the same file: mark `complete` **by quantity** (`filled >= order_qty`), not by the status string, so a full quantity reported under `PARTIALLY_FILLED` still closes the order (M5, client.rs:574); and price each fill as the **marginal** tranche, not the cumulative VWAP, or every fill after the first is mispriced when tranches differ (M6).

#### Local rejection parity — reject what the venue/simulator would reject

Translate akadro order semantics to the venue and **reject locally** what cannot be expressed, matching the simulator (bug classes 1/2):
- `post_only` is maker-only → only valid on a limit kind; on market it is a contradiction, reject (mexc client.rs:410-415; okx/bybit/kucoin mirror this).
- `reduce_only` doesn't exist on spot → reject on spot, inject `reduceOnly:true` only on the derivatives kind (bybit lib.rs:969-982 rejects spot, injects linear; okx lib.rs:932/990).
- stop/trigger kinds unsupported on a venue's plain order endpoint → reject (mexc client.rs:424, futures.rs:66).

Map venue error codes to a **venue-neutral `RejectReason`** with a conservative default (`mexc/error.rs:37-48`: balance→`InsufficientFunds`, filter/precision→`InvalidOrder`, signature/clock/unknown→`VenueRejected`). Never leak a venue-specific code into the engine.

#### Hygiene the reviewers enforce

- **Never print the API secret.** Every exec client hand-writes `Debug` with `finish_non_exhaustive()` and asserts `!dbg.contains("secret")` (mexc client.rs:619-627, test at 884-891).
- Connector errors are `#[non_exhaustive] thiserror` enums with foreign errors wrapped in a `String`, never `#[from]`-leaked.
- Caps come from the catalogue: a perp spec advertises `Margin`/`Funding`/`ShortSelling`/`ReduceOnly` (futures.rs:153-160), spot advertises only `LimitOrders`.
- The live round-trip is the irreducible open loop: read-only keys verify signing against the real server (`~/.config/akadro/mexc-test.env`), but order-submit E2E needs a trade-enabled testnet key. State this honestly rather than claiming full live coverage.

### 6. Engineering principles (DRY, SOLID, SoC, LoD, fail-fast, semver)

These are not aspirational adjectives — they are concrete review gates. The owner stated them explicitly (`AGENTS.md` §7; memory `akadro-engineering-principles`). Every change is judged on all six. Below, each principle is grounded in the actual code, with the historical bug that motivated the rule where one exists.

#### DRY — one source of truth, especially for venue plumbing

The canonical lesson: decimal-string↔fixed-point conversion was **copy-pasted into four venue connectors** (binance/okx/bybit/kucoin) and the copies drifted — e.g. empty-string handling differed (`"" → 0` in one, error in another), so the same venue payload parsed differently per venue. The fix hoisted it into `akadro-core::decimal_to_raw`/`raw_to_decimal`/`scale_of` (`crates/akadro-core/src/decimal.rs`) and `infer_funding_period_ms` (`crates/akadro-core/src/funding.rs`). See `CHANGELOG.md:51-54`.

**Rule: shared venue logic lives once in `akadro-core`; connectors wrap it with a thin adapter that maps `None`→their own error.** Never re-implement parse/scale/funding-period math per venue.

BAD (the divergence that shipped — four near-copies, subtly different):
```rust
// in each akadro-venue-*/src/parse.rs, hand-rolled, edge cases drifted:
fn decimal_to_raw(s: &str, scale: u32) -> i64 {
    if s.is_empty() { return 0; }      // <-- one venue did this
    // ...another returned Err, another truncated differently...
}
```
GOOD (single source + per-venue wrapper, `kucoin/src/lib.rs:168-179`):
```rust
pub use akadro_core::{raw_to_decimal, scale_of};

pub fn decimal_to_raw(s: &str, scale: u32) -> Result<i64, KucoinError> {
    akadro_core::decimal_to_raw(s, scale)         // the ONE implementation
        .ok_or_else(|| KucoinError::Parse(format!("bad decimal {s:?} at scale {scale}")))
}
```
The core fn rejects empty/non-numeric/overflow uniformly by returning `None` (`decimal.rs:43-50`, `decimal.rs:79`); empty-string handling is now defined in exactly one place. Likewise `FUNDING_RATE_SCALE` is defined once (`fixed.rs:62`) and connectors re-export it; the engine's `Money::mul_rate` divides by `10^FUNDING_RATE_SCALE` (`fixed.rs:272`) so "the connector's rate scale and the engine's charge can never disagree."

#### SOLID — venue traits are the OPEN extension surface; model traits are SEALED (D16)

The open/closed boundary is exactly three traits in `crates/akadro-core/src/traits.rs`: `DataSource` (`:39`), `ExecutionClient` (`:52`), `InstrumentCatalog` (`:126`). "to support a new venue you implement [these] in a new crate that depends only on `akadro-core`. No change to the engine, the strategy API, or any other venue is required" (`traits.rs:5-10`).

**Rule: adding a venue must touch ZERO upstream code.** If a venue needs a new capability, add it through the venue-neutral `VenueCommand` envelope (D9) via `ExecutionClient::command` (`traits.rs:101`), never a bespoke trait method. Anything not-an-order (leverage, margin, approvals) goes through `VenueCommand`. Conversely, **model traits will be sealed (D16)** so downstream users cannot implement them and break engine invariants — the venue traits are the *only* surface users extend.

#### SoC — crate boundaries are concern boundaries; tokio NEVER escapes `akadro-live`

Dependencies flow strictly downward, no cycles (`AGENTS.md` §3): core vocabulary ≠ engine loop ≠ venue I/O ≠ data layer. The single hardest constraint: **`tokio` is confined to `akadro-live` and must never appear in another crate's public signature** (`AGENTS.md` §6/§9). The async types are declared in the workspace only for the `akadro-live` `binance`/`mexc` features (`Cargo.toml:75-78`).

**Rule: no foreign async/runtime types leak across a crate seam.** The sync engine talks to live I/O through the lossless-or-fail `BoundedBridge` (D7), not by importing tokio. This is *why* D1 forced `Ctx` into `akadro-engine`: putting domain + I/O concerns in one crate would have destroyed both the kill feature and SoC.

#### LoD — talk to immediate collaborators through the trait seams

`AGENTS.md` §7: "talk to immediate collaborators via the trait seams, not reach through them." The engine emits account events through `EventSink::emit` (`traits.rs:27-30`); it does **not** hand an `ExecutionClient` a `Vec` to push into (D13). Account state is reconstructed only from `AccountEvent`s by the read-only portfolio (D5, `portfolio.rs:5-11`) — there is **no synchronous "what's my balance/position?" venue query anywhere**, which is also what makes backtest and live structurally identical.

**Rule: never add a synchronous venue query.** If a strategy needs account state, derive it from the event stream; reaching through the `ExecutionClient` for state is both an LoD violation and a parity break.

#### Fail-fast — validate at construction, return `Result`/`Option` early, `debug_assert` invariants, NEVER silently coerce

`AGENTS.md` §7: "validate at construction, return `Result`/`Option` early, `debug_assert` invariants — never silently coerce bad input." Recent guards added under this rule (all `CHANGELOG.md:34-42`):

- **`MergeSource` monotonicity** (`data/merge.rs:80-85`): a misbehaving child that breaks non-decreasing time order is caught in debug rather than silently feeding an out-of-order event to a strategy (a look-ahead hazard):
  ```rust
  debug_assert!(chosen_ts >= self.last_ts,
      "MergeSource child {chosen} produced an out-of-order event ...");
  ```
- **`bars_from_trades` ordering** (`data/trades.rs:63-68`): asserts time-ordered prints in debug rather than paying an O(n log n) defensive sort on the release hot path.
- **`interval_to_nanos` rejects non-positive** (`data/trades.rs:28-30`): `"0s"` → `None`, not a zero/garbage interval.
- **`Manifest::record` rejects a reversed range** (end < start) (`data/manifest.rs:257-266`).
- **KuCoin unknown candle type** fails fast on *every* path.

BAD (silent coercion — the class the rule forbids):
```rust
let interval = interval_to_nanos(s).unwrap_or(0); // "0s" silently → a 0-width bar bucket
```
GOOD (`data/trades.rs:21-41`): returns `None` for empty/non-positive/unknown-unit/overflow, forcing the caller to handle the bad input.

Note the deliberate split: **`debug_assert!` for invariants an upstream contract should already guarantee** (a misbehaving `DataSource` child — a programmer error, checked cheaply in dev where `overflow-checks`/`debug-assertions` are on, `Cargo.toml:136-139`), but **`Result`/`Option` for any externally-sourced/parsed input** (decimal strings, intervals, ranges). Library paths return errors; `panic!`/`assert!` is only for genuine programmer errors and must carry a `# Panics` doc.

#### Semver — `#[non_exhaustive]` + a `::new` constructor on every growable public type

**Rule: a growable public struct needs BOTH `#[non_exhaustive]` AND a `::new` constructor.** `#[non_exhaustive]` alone forbids cross-crate struct *literals* but not field reads; without a constructor, downstream code cannot build the value at all. With both, adding a field later (with `#[serde(default)]`) is non-breaking. `FillRecord` gained both in this pass (`portfolio.rs:18-60`; `CHANGELOG.md:48-50`), and a cross-crate struct literal in the analytics tests broke until migrated to `::new`.

BAD (cross-crate struct literal — breaks the moment a field is added, and won't even compile against a `#[non_exhaustive]` type):
```rust
// in akadro-analytics tests:
let fill = FillRecord { id, instrument, side, price, qty, fee, ts }; // E0639 cross-crate
```
GOOD (`analytics/src/trades.rs:202-210`):
```rust
FillRecord::new(ClientOrderId::new(0), InstrumentId::new(0), side,
    Price::from_raw(price), Qty::from_raw(qty), Money::from_raw(fee),
    Timestamp::from_nanos(1))
```
(Note the in-crate engine code may still use the literal — `portfolio.rs:208`, `observer.rs:58` — because `#[non_exhaustive]` only restricts *cross-crate* construction; never copy that pattern into another crate's tests.)

This is applied broadly: every growable public enum/struct is `#[non_exhaustive]` — all of `event.rs`, `order.rs`, `error.rs:14`, `instrument.rs`, the perf/trade/distribution reports, `Cost`, `ReplayDiff`. `Event` being `#[non_exhaustive]` is also why the engine's match has a documented reason for no catch-all (`engine.rs:252`).

#### Errors — `thiserror` enums; foreign errors WRAPPED, never `#[from]`-leaked (D13)

`AkadroError` (`error.rs:13-32`) is one `#[non_exhaustive]` `thiserror` enum. **Rule: a foreign error type never enters a public error enum via `#[from]`** — that would leak the foreign type into our public API and couple our semver to theirs. Capture its *kind + message* as an owned `String` instead (see every connector's `…Error::Transport(String)`/`Parse(String)`, e.g. `kucoin/src/lib.rs:71`, `:251`, `:267`).

BAD (leaks `reqwest::Error` / `serde_json::Error` into the public surface):
```rust
#[derive(thiserror::Error, Debug)]
pub enum KucoinError {
    #[error(transparent)]
    Http(#[from] reqwest::Error),   // foreign type now in our public enum
}
```
GOOD (wrap kind+message; the foreign type stays internal):
```rust
serde_json::from_str(json).map_err(|e| KucoinError::Parse(e.to_string()))?;
```

#### D13 — no foreign types in public signatures

Three concrete realizations to copy:
- **`CapSet`** wraps a `u64` bitset instead of exposing `enumset` (`instrument.rs:60-96`): opaque `CapSet`/`Capability`, built fluently `CapSet::empty().with(Capability::LimitOrders)`. `Capability::bit` uses `checked_shl` (`instrument.rs:53`) — fail-fast on a >64 discriminant.
- **`EventSink`** is a trait the engine implements, rather than handing out a `Vec<AccountEvent>` (`traits.rs:21-30`) — "lets the engine pick its own backing buffer without that choice leaking into the public API (decision D13)."
- **No `Index` on `Series`** — both a kill-feature requirement and a D13 instance (no `core::ops` foreign-trait surface that could be sliced).

#### Lints — the enforced quality floor (`Cargo.toml:99-123`)

Workspace-wide, members opt in via `[lints] workspace = true` (every member's `Cargo.toml`, e.g. `core/Cargo.toml:25`). The floor:
- `unsafe_code = "forbid"` **everywhere** (`Cargo.toml:103`). Only the future `akadro-data` mmap tier may override locally, behind audited `// SAFETY:` modules.
- `missing_docs`, `missing_debug_implementations`, `unreachable_pub` = warn (`:104-106`).
- `clippy::pedantic` = warn group (`:109`), with a *short, justified* allow-list (`:112-123`): e.g. `cast_lossless` is allowed because `i64 as i128` widening lives in `const fn` money math where `i128::from` is unavailable (`From` is not `const`) and is always lossless.

**Rule: do not add a blanket `#[allow]` to silence pedantic.** Allows are per-line and carry a justification comment (`instrument.rs:157` `too_many_arguments` on the `::new` constructor; `engine.rs:195`/`955` `too_many_lines` on the exhaustive event loop). A new pedantic warning is a hard signal to fix the code, not to widen the workspace allow-list.

### 7. Testing, coverage & the closed-loop principle

#### The production-ready gate — run it, observe it, never "should pass"
A change is **not done** until every step below has been *run and the output observed*. "Looks right" / "should pass" is forbidden (`akadro-production-ready.md`). State results honestly: if a step was skipped or a loop can't close, say so (§10/§12).

```sh
cargo test --workspace                       # 1. green: unit + integration + doctests + trybuild
cargo clippy --workspace --all-targets       # 2. 0 warnings (pedantic) — not "few", zero
cargo fmt --all -- --check                   # 3. clean
cargo doc --all-features -D warnings         # 4. no broken intra-doc links
# 5. coverage: a real asserting test per exercisable branch (below)
# 6. replay trade-oracle + parity golden-master on ANY trade-sim/engine change (below)
# 7. any GUESSED venue schema verified against the LIVE API, not just a fixture
```

Toolchain is pinned: `cargo +1.95.0` with `PATH=/home/admin/.cargo/bin:$PATH` (the `rustup`/`cargo` shims are **not** on the default shell PATH; `/usr/sbin/cargo` is Arch's and does not understand `+toolchain`). The 1.95 pin matters because the trybuild `.stderr` snapshots are toolchain-pinned to it.

**Rule for step 7 (live-schema):** a fixture is *not* proof a schema is right — a KuCoin fixture once matched a *wrong* hand-guessed schema and passed (`akadro-connector-owns-io`). When you guess an external venue schema, verify against the live API before calling it done.

#### Coverage: REGION is the gate, LINE is aspirational — and the numbers were stale
The const-fn- and connector-heavy code makes the *line* metric lie: line attribution double-counts multi-line expressions (e.g. multi-line `decimal_to_raw(...)` argument lines that don't independently register). **Region is the meaningful metric.**

- **Enforced gate** (`.github/workflows/ci.yml`): `--fail-under-lines 97 --fail-under-regions 97`. Both **pass**; they trail a literal 99/98 because of v0.2 growth (see below), so the gate is set to the honest measured floor, not an aspirational target.
- **Current measured (2026-06-04): 97.1% line / 97.8% region.** v0.2 added five venue connectors + WS drivers + a TUI + the cache layer — integration code whose error/IO-adjacent and pacing-`sleep` branches are exercised end-to-end via recorded fixtures but aren't all cheaply unit-coverable. The live-IO/UI files (`net.rs`, WS drivers, `data/venues.rs` live dispatch, `akadro-tui`) are excluded from the gate. This is **tracked growth, not a regression**.
- **STALE-NUMBER WARNING:** older AGENTS.md revisions claimed `99.04%/98.35%` (and memory once said `99.45%`) — those were **MVP-era figures**. When you re-measure, **update the stale numbers in AGENTS.md §10 and the memory; do not let them rot.** Always re-measure on *current* code before acting: llvm-cov line numbers in a `--show-missing-lines` list go stale the moment `cargo fmt` reformats.

The exact command, with its non-negotiable exclusions (CI uses `report --fail-under-*` after a build-time `--exclude`):

```sh
cargo +1.95.0 llvm-cov --workspace --all-features \
  --exclude akadro-compile-tests --exclude playground \
  --ignore-filename-regex '(venue-(mexc|binance|okx|bybit|kucoin)/src/net|akadro-live/src/(binance|mexc))\.rs' \
  --show-missing-lines
```

Why each exclusion: `akadro-compile-tests` is a trybuild crate (its programs must *not* compile, so it can't be instrumented); `playground` is a hand-run scratch binary that does live network IO (§8); `net.rs` + the `binance.rs`/`mexc.rs` WS drivers are live-network IO exercised only by credential-gated `#[ignore]` tests.

#### Coverage is a PROXY — cover every branch you CAN exercise, itemize what you can't
The goal is reliability, not a round number (`akadro-coverage-philosophy.md`). Write a real, asserting test for **every branch a real bug could touch**: error paths, fill-realism branches, new accessors/events, edge cases. **Skip only the genuinely-untestable, and itemize each one in §10 so the open loop is explicit.** The committed residual (AGENTS.md §10 table) is the template for what "genuinely-untestable" means:

| Residual (real, in §10) | Why it's legitimately uncovered |
|---|---|
| `backtest/exchange.rs` `_ => Outcome::Rest`, `_ => 0` | `#[non_exhaustive]` catch-alls; every live variant is handled explicitly above them |
| `data/bars.rs` format-version / column-count guards | Need a malformed Arrow file the writer never produces (≠6 columns / wrong cache version) |
| `engine/context.rs` `unrealized_pnl` no-mark branch | Forbidden by the `ExecutionClient` contract; already `debug_assert`-guarded in `mark_to_market` |
| `testkit/lib.rs:41` `break`-on-consumer-dropped | A **non-deterministic thread race**; covering it needs a `sleep`-race that contradicts the suite's determinism |
| `venue-*` `with_page_delay` `std::thread::sleep` + `else { Duration::from_secs(2) }` back-off | Only fire when `page_delay > 0` (a real download); deterministic tests use `page_delay = ZERO`. The paging *logic* (cursor/dedup/window-clip/429-retry/exhaustion) **is** all tested at delay 0 |
| multi-line `decimal_to_raw(...)` arg lines | Attribution artifact — the decode path *is* exercised (`core/decimal.rs` is **100% region**) |

#### NEVER game the metric — three GOOD-vs-BAD examples

**(1) Gaming the gate vs. testing the branch.** The line gate sits below 99. The temptation is to make it green the cheap way.

```rust
// BAD: assertion-free "coverage" test — drives the % up, catches no regression
#[test] fn covers_load_or_cache() { let _ = load_or_cache(&src, &path); } // no assert!

// BAD: lowering the gate to make CI green
//   --fail-under-regions 95   // <- never. The honest ceiling is 98, and it passes.

// GOOD: assert the actual behaviour on a branch a real bug could hit
#[test]
fn load_or_cache_returns_cached_on_second_call() {
    let first  = load_or_cache(&src, &path).unwrap();
    let second = load_or_cache(&src, &path).unwrap(); // must hit cache, not re-fetch
    assert_eq!(first, second);
    assert_eq!(src.fetch_calls(), 1);
}
```
Do not lower the gate below its honest floor (`--fail-under-lines 97 --fail-under-regions 97`), do not delete a defensive guard to bump the %, and do not write tests with no assertion. If a branch is genuinely-untestable, add a row to the §10 table instead.

**(2) Semantic verification beats line coverage.** A line-coverage number can be 100% while the *guarantee* is broken. The kill feature and parity are verified **semantically**, and those tests are the real safety net:

- **trybuild compile-fail/pass** (`akadro-compile-tests/tests/trybuild.rs`): `tests/ui-pass/*.rs` MUST compile, `tests/ui/*.rs` (look-ahead/forge attempts) MUST NOT — 27 cases. The committed `.stderr` snapshots pin the exact diagnostic, so an *accidental API loosening* or a rustc wording change fails CI. **Pin `.stderr` to the toolchain (1.95); regenerate with `TRYBUILD=overwrite cargo test`** — never edit them by hand.
- **Differential parity golden-master** (`crates/akadro/tests/parity_golden_master.rs` + `proptest_parity.rs:19` `parity_holds_for_arbitrary_prices`): backtest feed and threaded mock-live feed must yield a **bit-identical** `RunReport`, for the fixture *and* arbitrary random price paths.
- **Determinism** (`crates/akadro/tests/determinism.rs`): `rerun_is_bit_identical` (rerun → identical) **and** `parallel_sweep_matches_sequential` (one OS thread per config → identical to sequential).
- **proptest invariants**: fixed-point round-trip / `notional` / `mul_bps` / `diff` (`akadro-core/src/fixed.rs`); position conservation and `flat_round_trip_is_pnl_neutral` (`akadro-engine/src/portfolio.rs:494` — buy q@p then sell q@p realizes zero PnL).
- **Doctests** on public items (e.g. the `Price::notional` / `mul_bps` examples in `fixed.rs`).

**(3) The trade-oracle is mandatory on trade-sim/engine changes.** Any change to the fill simulation, portfolio, or engine loop can silently shift behaviour. Closing that loop is step 6 of the gate, not optional (`akadro-trade-oracle-rule`):

```rust
// On ANY trade-sim/engine change, re-drive the saved trades with NO strategy logic
// and diff the invariants — proves the default path is bit-identical.
use akadro_backtest::replay::{replay_trades, diff_reports};
let rebuilt = replay_trades(saved, fee_bps)?;   // akadro-backtest/src/replay.rs
let check   = diff_reports(saved, &rebuilt);     // a divergence = you changed trading behaviour
```
A change that *should not* affect trading but makes the replay diverge — or one that *should* but doesn't — is the bug this catches. The critical MEXC `avg_price` off-by-`10^qty_scale` bug (§12) shipped precisely because a *unit/e2e test pinned the wrong value*; the replay oracle + golden-master are the defense against that class.

#### Flaky tests: unique temp paths, no shared mutable fixtures
Two `akadro-data` tests once raced on the same on-disk file because `tmp("empty")` returned a **fixed** path (`bars.rs:438` builds `akadro_data_test_{name}.feather` in the shared temp dir). Tests run concurrently, so two cases touching the same `name` race on the same file.

```rust
// BAD: two tests sharing a name collide on one file when run in parallel
fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("akadro_data_test_{name}.feather"))
}
// ... test_a uses tmp("empty"); test_b also uses tmp("empty")  -> data race on disk

// GOOD: make each path unique (unique `name` per test, or fold in pid/atomic)
let path = tmp("empty_source");  // distinct from every other test's name
// (or: format!("akadro_data_test_{name}_{}_{}.feather", std::process::id(), COUNTER.fetch_add(1, ..)))
```
**Rule:** every test that writes a file uses a path no other test uses — give each test a distinct `name`, and prefer folding in `std::process::id()` + an atomic counter so even re-runs and same-name slips can't collide. Never share a mutable on-disk fixture across tests, and never rely on `Instant::now`/`thread_rng`/wall-clock in test logic (it makes the suite non-deterministic — the same reason `testkit/lib.rs:41`'s race is left uncovered rather than tested with a `sleep`).

### 8. Build, toolchain, CI & operational gotchas

This repo has **no `rust-toolchain.toml`** (confirmed: `ls rust-toolchain*` → no matches), so the toolchain is *never* implied — you must name it on every invocation. The dev host's PATH and `cargo` shim are both traps. Internalize the canonical invocation below before running anything.

#### Always invoke cargo via the rustup shim with an explicit toolchain

On this host `which -a cargo` resolves to `/usr/sbin/cargo` (Arch's **system** cargo) *before* `~/.cargo/bin`. The rustup shim is at `/home/admin/.cargo/bin/cargo` (a symlink to `rustup`), and `~/.cargo/bin` is **not** on the default shell PATH.

- **BAD** — picks up `/usr/sbin/cargo`, which does not understand `+toolchain` syntax and errors (or silently runs the wrong rustc):
  ```sh
  cargo +1.95.0 test --workspace          # error: no such subcommand / +toolchain not understood
  ```
- **GOOD** — prepend the rustup bin dir, name the MSRV explicitly:
  ```sh
  PATH=/home/admin/.cargo/bin:$PATH cargo +1.95.0 test --workspace
  ```
  Use `+1.95.0` because that is the MSRV (`rust-version = "1.95"`, root `Cargo.toml:32`) **and** the toolchain the trybuild `.stderr` snapshots are pinned to (CI `test` matrix is `["1.95.0", "stable"]`, `ci.yml:41`). Agent threads reset cwd between bash calls — always use absolute paths and prepend the PATH in *every* command (shell state does not persist).

#### Cap parallelism at ≤4 cores — it is host config, not repo config

`~/.cargo/config.toml` on this host sets `[build] jobs = 4` (confirmed). This is **host-specific and NOT in the repo**, so it will not travel with a clone and a future agent on another host has no such cap. When you add a parallel command (e.g. `cargo test -j`, `nextest`, a build script that spawns), do not exceed 4 cores here. Do **not** "fix" the missing cap by committing a `config.toml` to the repo — it is deliberately host-local.

#### A "hung" cargo is almost always an orphaned runaway test binary — kill by PID, never just retry

Cargo on this host is fast (`cargo build -p akadro-core` ≈ 0.43s). A `cargo test` "stuck for 20+ minutes" is essentially never a slow compile — it is a `<crate>-<hash>` **test binary infinite-looping / running O(n²) code at ~100% CPU**. Those binaries are orphaned children that **survive `kill cargo` / `TaskStop` / `pkill cargo`**, accumulate across retries, and starve the CPU so every *later* cargo command also crawls.

- **BAD:** Ctrl-C the cargo invocation and re-run `cargo test` again. The orphan is still pegging a core; you now have two.
- **GOOD:** diagnose, then kill the actual binary PIDs:
  ```sh
  ps -eo pid,etimes,pcpu,comm --sort=-pcpu | grep <crate>   # a <crate>-<hash> at ~100% for minutes = runaway
  kill -9 <pid>                                             # cargo/TaskStop/pkill cargo will NOT reap it
  ps | grep <crate>_                                        # confirm empty BEFORE re-running
  ```
  Real precedent: `Portfolio::open_order` did a `Vec::contains` per `OrderAccepted` → O(n²); a 1M-order test spun 31 min. Removing the dedup → 0.6s. **When a test "hangs," first suspect an accidental O(n²) / infinite loop in the code under test, not the toolchain.**

#### Run `cargo doc` with `--all-features` or latent doc-link errors hide until CI

CI's `docs` job sets `RUSTDOCFLAGS: -D warnings` and runs `cargo doc --workspace --no-deps --all-features` (`ci.yml:56-65`). The WS-driver modules are **feature-gated** — `akadro-live/src/lib.rs:35-38` (`pub mod binance` behind `binance`, `pub mod mexc` behind `mexc`) and `akadro-venue-mexc/src/lib.rs:39` (`#[cfg(feature = "net")]`). A default-feature `cargo doc` never compiles those modules, so a broken `[intra-doc-link]` or redundant-explicit-link inside them is invisible locally but fails the `-D warnings` gate in CI.

- **BAD** (passes locally, fails CI): `PATH=/home/admin/.cargo/bin:$PATH cargo +1.95.0 doc --workspace --no-deps`
- **GOOD** (matches CI): `RUSTDOCFLAGS="-D warnings" PATH=/home/admin/.cargo/bin:$PATH cargo +1.95.0 doc --workspace --no-deps --all-features`

#### Struct-doc method links need the `Self::` qualifier

A bare `` [`method`] `` in a struct's doc comment does **not** resolve and trips `-D warnings`. The repo convention is to qualify with `Self::` (confirmed throughout: `akadro-analytics/src/equity.rs:72`, `akadro-backtest/src/exchange.rs:84,184,477`, `akadro-engine/src/engine.rs:87,91`, `akadro-venue-binance/src/lib.rs:490,670`).

- **BAD:** `/// As [`from_equity`] but with a per-period risk_free rate`
- **GOOD:** `/// As [`Self::from_equity`] but with a per-period risk_free rate` (`equity.rs:72`)

#### Feature-gate doctest *bodies* so `--no-default-features` compiles an empty body

Doctests run by default; a doctest using feature-only types fails to resolve them under `--no-default-features`. Wrap the body in a hidden `# #[cfg(...)] { ... # }` so it degrades to an empty (still-compiling) body. Live example, `akadro/src/lib.rs:24-39` (the comment even cites the finding `m39`):
```rust
//! ```
//! # #[cfg(feature = "backtest")] {
//! use akadro::prelude::*;
//! // ... uses HistoricalFeed + SimulatedExchange (backtest-only) ...
//! # }
//! ```
```
The `# ` prefix hides the line from rendered docs; the `{ }` scopes the cfg. Without it, the `feature-powerset` job (`cargo hack --feature-powerset --no-dev-deps check`, `ci.yml:113-121`) and a `--no-default-features` doc build break. Note `backtest` is the `default` feature (`akadro/Cargo.toml:30`), so this only bites the powerset/no-default builds — exactly the ones easy to skip locally.

#### Always `cargo fmt --all` before the gate — manual i128 / try_from edits drift

`rustfmt.toml` pins `edition = "2024"` + `max_width = 100`. CI's first job is `cargo fmt --all --check` (`ci.yml:13-21`), a hard fail. Hand-written multi-line money math (`i128` widening, `try_from`, chained `notional`) drifts from rustfmt's preferred wrapping, so a change that "looks formatted" still fails `--check`.

- **GOOD:** `PATH=/home/admin/.cargo/bin:$PATH cargo +1.95.0 fmt --all` (mutate) before `cargo fmt --all --check` (verify). Never hand-format; let rustfmt own line wrapping.

#### trybuild `.stderr` is pinned to rustc 1.95 — a toolchain bump means TRYBUILD=overwrite

The 27 compile-fail `.stderr` snapshots in `akadro-compile-tests` capture exact diagnostic wording, which changes between rustc versions. CI only asserts them on the 1.95.0 matrix leg and **excludes** them on stable (`ci.yml:48-54`: `--exclude akadro-compile-tests` on non-1.95). So:
- Run/regenerate snapshots **only** under `+1.95.0`. Regenerate after an intentional toolchain bump or API loosening: `TRYBUILD=overwrite PATH=/home/admin/.cargo/bin:$PATH cargo +1.95.0 test -p akadro-compile-tests`, then **diff the new `.stderr` by eye** — an unexpected change there can mean the kill-feature guarantee silently weakened, not just reworded diagnostics.

#### Build-profile gotchas (root `Cargo.toml:130-139`)

- `dev` sets `overflow-checks = true` + `debug-assertions = true` — fixed-point money math that wraps will **panic in dev/test** (intended fail-fast) but silently wrap in `release`. Run tests in the default (dev) profile so overflow/`debug_assert` invariants actually fire; don't validate money math under `--release`.
- `release` uses `panic = "abort"` + `codegen-units = 1` + `lto = "thin"` for deterministic, fully-inlined backtests. `panic = "abort"` means tests relying on `#[should_panic]` / unwinding won't behave under `--release`.

#### Lint gate is `clippy::pedantic` workspace-wide; `unsafe_code = "forbid"`

`clippy` CI runs `--workspace --all-targets --all-features` with `RUSTFLAGS: -D warnings` (`ci.yml:23-32`, env `:8-10`) — pedantic warnings are **hard errors**. A specific set is allowed *with justification* in the root manifest (`Cargo.toml:108-123`): `module_name_repetitions`, `must_use_candidate`, `missing_errors_doc`, the four `cast_*` lints, and `cast_lossless` (the last because `i64 as i128` widening lives in `const fn`s where `i128::from` is unavailable). **Do not add new blanket `#[allow]`s** — fix the lint or, if it genuinely fights the newtype/finance style, add it to the central list *with a comment*, never sprinkle local allows. `unsafe_code = "forbid"` is workspace-wide (`Cargo.toml:103`); only the future `akadro-data` will ever override it locally behind audited `// SAFETY:` modules.

#### `cargo-deny` config and what CI actually enforces vs. defers

`deny.toml` enforces: `yanked = "deny"`, an explicit license allowlist (`MPL-2.0` + permissive: MIT/Apache/BSD/ISC/Unicode-3.0/Zlib), `wildcards = "deny"`, `unknown-registry`/`unknown-git = "deny"`. **A new dependency with a license outside that allowlist, or a `*` version, fails the `deny` job** (`ci.yml:97-102`) — pin caret versions in `[workspace.dependencies]` and check the license first. `multiple-versions = "warn"` (not deny), so duplicate transitive versions are tolerated but visible.

#### Coverage gate is exact and tuned — know what's excluded before "improving" it

The `coverage` job runs `cargo llvm-cov --workspace --all-features` then `report --fail-under-lines 97 --fail-under-regions 97` (`ci.yml`). Excluded from instrumentation: `akadro-compile-tests` and `playground` (`--exclude`). Excluded from the gate via `--ignore-filename-regex`: the live-IO `venue-(mexc|binance|okx|bybit|kucoin)/src/net.rs` + the `akadro-live/src/(binance|mexc).rs` WS drivers + the `akadro/src/data/venues.rs` live dispatch + the `akadro-tui` terminal UI. **Region is the meaningful metric** for this const-fn/connector-heavy code; both clear 97%. Do not chase 100% with assertion-free tests or by lowering the gate — write a real asserting test for every *exercisable* branch; the genuinely-untestable residual is itemized in AGENTS.md §10.

#### CI jobs that are stubbed / deferred — don't assume they're green

- `semver` job is hard-disabled (`if: false`, `ci.yml:104-108`) — **no published baseline before 0.1.0**; it activates from the first release. Do not assume `cargo-semver-checks` is catching breaking changes yet.
- Roadmap CI hardening still pending (AGENTS §13.7): `cargo-semver-checks` activation, a `dhat` zero-alloc gate, `cargo-hack` is *present* (`feature-powerset` job) but verify new feature combos build under it locally before pushing.

#### Run the full local gate before declaring done (closed-loop)

```sh
P=/home/admin/.cargo/bin; TC=+1.95.0
PATH=$P:$PATH cargo $TC test --workspace
PATH=$P:$PATH cargo $TC clippy --workspace --all-targets --all-features    # 0 warnings
PATH=$P:$PATH cargo $TC fmt --all --check
RUSTDOCFLAGS="-D warnings" PATH=$P:$PATH cargo $TC doc --workspace --no-deps --all-features
```
On any trade-sim/engine change also run the replay trade-oracle + parity golden-master. The four CI gates that bite hardest in this order — **fmt drift → clippy pedantic → feature-gated doc-link/doctest under `--all-features` → trybuild on 1.95** — are exactly the ones invisible to a casual `cargo test` on default features.

### 9. Process & meta-lessons (how to run reviews, what actually worked)

This project's correctness was not produced by careful reasoning alone — it was produced by **adversarial, closed-loop review at scale**, and every documented bug below shipped *despite* passing tests at the time. Treat the patterns here as the operating procedure, not history.

#### The review pattern that works (reuse it)

The pipeline that produced the hardened architecture and found every major class of bug (AGENTS §2, line 61–64) is:

```
research (web-backed, N agents)
  → synthesis (1 agent, reconcile sources)
  → ADVERSARIAL VERIFIERS, one per goal/dimension, each told to BREAK a different thing:
      leak-hunter · parity-auditor · exchange-integrator · performance-skeptic
      · ergonomics/maintainability · completeness
  → reconciliation (1 agent: accept / fix / document-as-residual)
```

Then a second stage that is just as important: **a verifier PER candidate finding, prompted to default-to-refuted.** The 62-agent kill-feature audit (§12, line 470) was "35 hunters → a verifier per candidate → one reconciler" and **dismissed 22 of 26 candidates** as non-exploits (`LastN` covariance only *shrinks* `'bar`; the clock oracle conveys no OHLCV; a forged `Series` is a dead end because no public API consumes one). The deep bug-hunt (§12, line 511) was "8 Opus auditors → a Sonnet verifier per finding → an Opus reconciler" — 16 confirmed of many more raised.

- **Always split the attack by goal.** One generalist reviewer finds shallow bugs; six specialists each obsessed with *one* of the five goals (§1) find the deep ones. The single most important finding in the project's history — D1 — came from the *leak-hunter* specifically attacking the look-ahead guarantee.
- **Always run a per-finding verifier that defaults to "refuted."** Plausible-but-wrong findings cost a fix (and risk a regression) if they reach reconciliation unchallenged. The kill-feature audit's value was as much in the 22 it *killed* as the 4 it confirmed.
- **Scale review effort to the ask.** A typo fix does not need 110 agents. But lean hard toward adversarial verification + live-schema confirmation + the closed loop for **anything touching money, fills, or parity** — that is where every critical bug landed.

#### Verify soundness against the REAL toolchain, not on paper

D1 (AGENTS §3, line ~100) is the canonical lesson. The natural design — put `Ctx` in `akadro-core`, the loop in `akadro-engine` — is **unsound**, and it was proven so *by compiling it*, not by arguing about it. Rust has no "pub to one specific external crate" visibility:

- A truly-private `Ctx::new` in `core` is uncallable from `engine` (`error[E0624]`) — verified on rustc 1.95.
- A `#[doc(hidden)] pub` constructor compiles **and runs** from any downstream crate: `Ctx::__new(&my_own_future_data)` forges a context over attacker data.

The resolution (co-locate `Ctx`/`Series`/brand/loop in `akadro-engine` with a `pub(crate)` ctor) is now permanently regression-locked by the `forge_context` trybuild case in the genuinely-downstream `akadro-compile-tests` crate. **Lesson: a soundness claim about visibility, lifetimes, or variance is not proven until rustc rejects the attack.** That is why the kill feature has 27 compile-fail `tests/ui/*.rs` with pinned `.stderr` snapshots — a wording change *or an API loosening* fails CI.

#### CRITICAL: a test can pin the WRONG value and mask a real bug

The most dangerous failure mode in this codebase. The MEXC connector's `avg_price` was off by `10^qty_scale` — it divided a `price_scale` quote by a `qty_scale` base, mis-scaling **every live fill and the fee derived from it** for any symbol with nonzero base precision (§12 line 514; CHANGELOG line 459). It survived because **the unit and e2e tests had pinned the wrong expected value.** Green tests asserted the bug was correct.

**GOOD vs BAD — deriving expected values:**

```rust
// BAD: expected value computed the same (buggy) way as the code, or eyeballed
// from the code's own output. Test is a tautology; it locks in whatever shipped.
assert_eq!(fill.avg_price.raw(), 4_523_710_000); // "whatever the code printed once"

// GOOD: derive the expected value INDEPENDENTLY (by hand from the venue's
// documented scales) AND cross-check against real-data behaviour.
// quote notional / base qty, each at its OWN scale -> price at price_scale.
// Confirmed by a real BTC backtest reporting a sane $-level avg entry.
```

Reinforcing precedents:
- A KuCoin funding fixture **matched a wrong schema guess** and passed; only hitting the live endpoint on 2026-06-01 corrected the fields to `fundingRate`/`timepoint` (CHANGELOG line 167–168; production-ready gate step 7).
- Sub-bp funding silently accrued **zero** because the rate unit was basis points, not `1e-8` — real OKX rates (0.02–0.47 bp) rounded to 0 (CHANGELOG line 140). A fixture with a 1bp rate would never have caught it.

**Rules:** Never let the code under test produce its own expected value. Derive expected values by hand from an independent source (the venue's documented precision, a closed-form formula). For money/fill/funding paths, **cross-check against real data** and against the replay trade-oracle. A guessed external schema is verified against the **live API**, not just a fixture (production-ready step 7).

#### External-benchmark comparison surfaces convention bugs you cannot self-find

You will not notice that *your* Sharpe is wrong if you only compare it to *your* Sharpe. The 110-agent audit v2 (§12 line 528) and the 21-agent audit (§12 line 543) compared every dimension against Nautilus Trader, QuantConnect LEAN, Zipline, empyrical, CCXT, Binance/MEXC, TA-Lib, and ArcticDB — and found real convention bugs:

```text
BAD (shipped):                          GOOD (fix, vs empyrical/LEAN convention):
Sharpe uses population variance         sample variance, ddof = 1
volatility unannualized                 annualized (× √periods_per_year)
Calmar = 0 on zero drawdown             ±∞ / 0 limit; constant-positive ≠ score 0
min_notional compared at price_scale    compared at COMBINED price+qty scale (guard fires)
```

**Lesson:** for any computation with an industry-standard definition (risk metrics, fee tiers, order semantics, funding), benchmark the *numbers and conventions* against the reference implementations. Self-consistency is not correctness.

#### Keep parity-breaking changes opt-in or in the runner — never global-default

The parity golden-master (`crates/akadro/tests/parity_golden_master.rs`) asserts a **bit-identical** `RunReport` between the historical and mock-live feeds. Several improvements would have silently broken it if defaulted on. The discipline is: the conservative default path stays bit-identical; realism is opt-in.

**GOOD vs BAD — funding/cash:**

```rust
// BAD: make SimulatedExchange charge funding (or enforce cash) for everyone.
// Breaks the golden-master and changes every existing backtest's numbers.

// GOOD (exchange.rs:88, :354): opt-in, default None/off.
pub starting_cash: Option<i128>,          // None => no cash guard => bit-identical
pub fn with_starting_cash(mut self, cash: Money) -> Self { ... }
// Funding default stays OFF globally; the playground RUNNER turns it on per-venue
// (--funding, default-on for OKX perps), NOT the library default (CHANGELOG:137).
```

And the funding *unit* fix was engineered to be provably bit-identical on the default path: `with_funding(bps, …)` widens `×10_000` into the `1e-8` unit and `Money::mul_rate`'s `10⁸` divisor cancels it back (`fixed.rs:271`, CHANGELOG line 175). **Rule:** when you change fill/funding/PnL math, prove bit-identicality with the replay trade-oracle + parity golden-master + proptest (production-ready step 6), or gate the change behind opt-in config. Every CHANGELOG fix pass re-confirms "default conservative path stays bit-identical."

#### Defer disproportionate fixes explicitly — do not force them

Forcing a fix that breaks separation-of-concerns is worse than the duplication it removes. The cross-venue paging/429-retry logic is duplicated across five connectors. The 41-agent review (CHANGELOG line 72) **deliberately did not refactor it**: a shared helper would need an HTTP abstraction in `akadro-core`, breaking SoC and the venue-crate independence that is the exchange-agnostic goal (§1). It was **flagged, not refactored** (CHANGELOG line 78–80).

Likewise, five MINOR audit-v2 findings (volume-share slippage #15, POST-body params #47, per-`commissionAsset` fee scale #37, `OrderCancelPending` #17, order amendment #35) were **deferred with written reasons** to AGENTS §13 — several blocked on a future subsystem (the WS shell, D11). **Rule:** when a fix is disproportionate or would violate DRY/SOLID/SoC/LoD, write down *why* you are deferring (what it would cost, what it is blocked on) rather than half-implementing it. A documented residual is an engineering decision; a forced bad fix is a regression.

#### Closed-loop honesty: say when a loop can't close; update stale numbers

The closed-loop principle (memory `akadro-closed-loop.md`): never declare done by assertion — act → run an automated check → observe → correct — and **explicitly tell the user where a loop cannot close, with the reason.** Loops that genuinely cannot close here are stated, not hidden:

- **Real-venue live parity** — no credentials/network; substitute is structural parity vs the mock-live feed; real latency/fill fidelity is the irreducible ceiling (§12 line ~465; memory item 1).
- **Out-of-band look-ahead & `unsafe`** — not preventable by types; mitigated by the `#![forbid(unsafe_code)]` template + cargo-geiger recipe (D2/D3; memory item 2).
- **`cargo-semver-checks`** — needs a published baseline; closes from first release.

**Update stale doc numbers rather than leaving a false claim.** The coverage figure was `99.04% line / 98.35% region` (CHANGELOG line 371) but v0.2's five venue connectors + WS drivers + TUI added integration code; the honest current number is **97.1% line / 97.8% region** (freshly measured 2026-06-04, with the live-IO/UI files excluded from the gate) and AGENTS §10 *explicitly notes the old figures predated the v0.2/cache growth*. The line residual is itemized in a table (§10) so the open loop is visible per-line. **Rule:** a stale "99%" left in the docs is a lie the next agent will trust; when reality moves, edit the claim and explain the delta. The metric is a **proxy** — cover every branch you can actually exercise, never game it with assertion-free tests, and itemize the genuinely-untestable.

#### Anti-patterns this project corrected (do not reintroduce)

- **Guessing venue schemas** — every guessed external schema must be verified live (the wrong-KuCoin-fixture incident; production-ready step 7). Open MEXC items (WS protobuf field names, klines max, futures recv-window unit) are flagged as "confirm live," not assumed.
- **Raw HTTP in playground/user code** — D18: all data enters through library-owned fetchers over the `Transport` seam (`*Catalog::fetch`, `*Feed::with_range`, `fetch_funding_history_paged`) + the `akadro-data` cache. The OKX backfill moved ad-hoc `reqwest` out of the playground into the connector (CHANGELOG line 131); `VisionBarSource::from_bars` was *removed* because a venue connector must not offer raw-`Bar` injection (CHANGELOG line 76). The playground is a thin consumer, never a data-entry point.
- **Premature micro-optimization** — goal 4 (§1): optimize only what saves >10% wall-clock. The real, measured win was O(open positions) marking instead of O(catalogue) — a 5–8× speedup proven *and shown bit-identical* (CHANGELOG line 184), not a guessed hot loop.
- **Trusting green tests on money paths** — see the avg_price tautology above. Green is necessary, not sufficient; for fills/PnL/parity, the bar is the replay trade-oracle + golden-master + an independently-derived expected value.

### 10. Walk-forward & analytics integrity (the orchestration layer types can't protect)

The kill feature (#1) is a **per-backtest** guarantee: inside one `Engine::run`, safe strategy code cannot read a future bar through `Ctx`. **Walk-forward analysis (WFA) and the `akadro-analytics` metrics run ABOVE the engine, so none of that protection applies** — IS/OOS separation, parameter selection, input ordering, and metric aggregation are all the caller's responsibility. This is precisely the D2 "out-of-band look-ahead" class the type system cannot catch (§4). The WFA tooling (`akadro-analytics`: `walk_forward`/`walk_forward_anchored`/`walk_forward_purged`, `run_walk_forward`, `run_grid`, `combinatorial_splits`, `PerformanceReport`, `walk_forward_efficiency`, `deflated_sharpe`) is real, tested, and fail-fast on its OWN inputs — but it is a **framework, not a turnkey optimizer**, and it is easy to produce green, plausible, but fake results. Tell users this plainly; never imply the look-ahead guarantee extends to WFA.

#### The hardened safe API (added 2026-06-02 — prefer these; they close most footguns below)
The WFA surface was hardened so the safe path is the default and the leakage-prone helper is opt-in:
- **`akadro_backtest::WalkForwardBacktest`** — the **structural IS/OOS boundary**. Its `fit(&[Bar], &mut State) -> Params` closure receives ONLY the train slice (the test slice is not in scope, so fitting on OOS is *inexpressible* — the WFA analog of kill-feature layer 1 "the strategy never receives the dataset"); `make(&Params) -> impl Strategy` sees no bars; the framework owns the OOS `Engine::run` with **one shared `FillConfig`** (config drift impossible) and validates bar ordering once up front. This is the default WFA runner.
- **`WalkForwardSummary`** (`new` / `from_reports`) — the **only** blessed OOS aggregator: pools per-fold OOS *return series* (never stitched equity levels — the boundary-jump bug is unreachable), **counts** (never drops/zeros) unmeasurable blow-up folds, and reports `efficiency: Option` that is `Some` only for a strictly-positive in-sample (no `±∞` WFE).
- **`PerformanceReport::from_equity_auto`** + `infer_periods_per_year` — annualization inferred from the curve's own timestamps (median spacing → 365.25-day year), closing the wrong-`periods_per_year` footgun; `from_returns` is the shared returns→metrics core both it and `from_equity_with` delegate to.
- **`HistoricalFeed::try_from_bars`** + `akadro_core::check_bars_ordered` — fail-fast `Result` (`BarOrderError`) on unordered / same-instrument-dup-ts bars (the garbage-in footgun). `from_bars` stays the "I promise it's ordered" fast path (real callers; sanctioned data path).
- **The generic both-slices runners `run_walk_forward` / `run_walk_forward_checked` are gated behind the off-by-default `escape-hatch` feature** (they hand the closure the OOS slice = leakage), so they are NOT on the default user surface and the agentic lab (built without that feature) cannot call them. `slice_window` stays public for deliberate, explicit custom orchestration.

#### What the library DOES protect (fail-fast / honest — do not re-guard)
- `walk_forward` / `walk_forward_anchored` / `walk_forward_purged` return an **empty `Vec`** on non-positive lengths or overflow (`walk_forward.rs`) — never a silent fake window.
- `WalkForwardBacktest` (and the gated `run_walk_forward`) runs **a fresh `Engine` per fold** (the D2 "fresh engine per run" contract), so engine state never leaks across folds, determinism holds, and the kill feature still binds *within* each fold.
- `PerformanceReport::from_equity` / `from_equity_with` / `from_equity_auto` return **`None`** (never `NaN`) for `< 2` points, any non-positive equity sample (a blown-up account), or a non-finite/non-positive `periods_per_year` (the m28 guard in `equity.rs`). Sharpe/vol use sample variance `ddof=1` (empyrical/QuantStats convention).

#### What it TRUSTS the caller to get right (the footguns)
1. **IS/OOS contamination (silent, the dangerous class).** `run_walk_forward`'s `fold(&Window, train, test, &mut state)` receives BOTH slices; nothing forces separation. Fitting on `test` (or the full `bars`), **selecting parameters by OOS performance**, or threading test-derived `state` into the next fold's training all yield optimistic OOS that is really IS. Types cannot catch this — same class as `data[t+1]`.
2. **Label leakage.** Use `walk_forward_purged` (purge + embargo, López de Prado) when labels span time; plain `walk_forward` leaks train↔test.
3. **Unsorted / duplicate-ts bars.** `HistoricalFeed::from_bars` is documented "assumed already time-ordered" and does **not** sort or assert (`feed.rs`). Out-of-order bars drive the engine backward → corrupt fills/equity → fake report. The `akadro-data` cache enforces ascending ts on read; a hand-built `Vec<Bar>` does not.
4. **Wrong annualization.** `periods_per_year` is caller-supplied; pass `252` for hourly bars and Sharpe/vol are annualized wrong but plausible. The library cannot know your interval.
5. **Config drift.** Use the SAME `FillConfig` (fees/slippage/cash) for the IS sweep and the OOS run, or OOS is unrealistic.
6. **Aggregation / interpretation.** Don't compute the headline Sharpe over concatenated per-fold equity *levels* — the inter-fold jump is a spurious return; pool the per-fold OOS *return series*. `walk_forward_efficiency(oos, is_)` returns `∞` when `is_ == 0` (and inverts for `is_ < 0`) — don't average it raw. Feed `deflated_sharpe` / `combinatorial_splits` the correct trial count or the PBO estimate is wrong. Don't treat a `None` from `from_equity` as zero.

#### GOOD vs BAD

**BAD** — optimizes on the test slice (the classic WFA sin; compiles, runs, looks great):
```rust
let (reports, _) = run_walk_forward(&bars, &windows, (), |_w, _train, test, _| {
    let params = best_params_by_return(test);   // selected on OOS == look-ahead
    run_engine(test, params)
});
```
**GOOD** — params chosen on `train` only; `test` used solely to build the OOS feed, same config:
```rust
let (reports, _) = run_walk_forward(&bars, &windows, (), |_w, train, test, _| {
    let params = best_params_by_return(train);  // IS optimization (e.g. run_grid over `train`)
    run_engine(test, params)                     // OOS evaluation, identical FillConfig
});
```

**BAD** — unsorted hand-built bars + wrong annualization + panic on a blow-up:
```rust
let feed = HistoricalFeed::from_bars(bars_unordered);     // no sort/assert -> engine runs backward
let sharpe = PerformanceReport::from_equity(&curve, 252.0) // 252 but these are hourly bars
    .unwrap().sharpe;                                      // unwrap() panics on a blow-up's None
```
**GOOD**:
```rust
bars.sort_by_key(|b| b.ts);                                // or load via the akadro-data cache (enforces order)
match PerformanceReport::from_equity(&curve, 24.0 * 365.0) // hourly -> periods/year
    { Some(r) => use_it(r.sharpe), None => report_unmeasurable() }
```

#### Rule
Treat every WFA result as guilty until the IS/OOS wall is proven: params from `train` only, `test` only to feed the OOS engine, time-sorted bars, interval-correct `periods_per_year`, identical IS/OOS `FillConfig`, `None`/`∞` handled, OOS *returns* pooled. The framework gives you the splits, the parallel sweep, CSCV/PBO, and the metrics — **the integrity of the split is yours.**
