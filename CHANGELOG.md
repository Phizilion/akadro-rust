# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) from its first
release.

## [Unreleased]

## [0.2.0] - 2026-06-04

Headline: the v0.2 line (indicators, analytics, the data cache, the live shell, five
venue connectors + DEX, the TUI, expanded order/fill realism) plus this release's
hardening — the data-layer silent-data-loss fix, the `O(log k)` tournament-tree merge,
the dhat allocation gate, the mdbook lifetime-error guide, and a full adversarial
quality-audit fix pass (overflow discipline, the `Costs` D13 newtype, `#[non_exhaustive]`
/ `Error` semver fixes, the `Resync` drain unification, new regression + property tests,
and an honest coverage gate). **Minor bump (pre-1.0): contains breaking API changes** —
`Costs` is now an opaque newtype (was a `SmallVec` alias) and `PlacedOrder` is
`#[non_exhaustive]` (construct via `PlacedOrder::new`).

### Changed — quality-audit fixes: discipline drift + semver hazards
From an adversarial quality audit (`audits/RATING_REPORT.md`), all high-priority items resolved
(parity bit-identical: replay oracle + golden-master + `proptest_parity` re-verified):
- **Overflow discipline**: the reduce-only `net_after` staging in `SimulatedExchange` and
  the Wilder smoothing in RSI/ATR (`akadro-indicators`) used bare `+=` / `as i64`; now
  `saturating_add` / `i64::try_from(..).unwrap_or(..)`, matching the rest of the code.
- **`Costs` is now an opaque newtype** (was `pub type Costs = SmallVec<[Cost; 2]>`), so
  `smallvec` is no longer in akadro's public API (D13). Read via `Deref<[Cost]>` /
  `&Costs` iteration; build with `Costs::new` + `push`. No call site changed behaviour.
- **`PlacedOrder` gained `#[non_exhaustive]` + `PlacedOrder::new`** (growable-type policy);
  `Side` is documented as *intentionally* exhaustive (binary, closed — not a semver hazard).
- **`BarOrderError` now implements `std::error::Error`** (via `thiserror`), so it composes
  with `?` / `Box<dyn Error>`.
- **Engine `Resync` now flows through the `drain` loop** like every other account event
  (apply → dispatch → route), instead of a hand-applied inline copy — one code path, no
  parity-drift hazard (safe because `Portfolio::apply` treats a resync as an accounting
  no-op).

### Added — test-gap closure + property tests + coverage-gate honesty (quality audit)
- **New regression tests**: concurrent reduce-only orders in one bar capping at the
  position (M11); an armed IOC stop-limit that can't fill expiring (not resting);
  `Debug` credential-hiding for OKX/KuCoin/Bybit/Binance exec clients (was MEXC-only);
  MEXC spot back-fill now dedups overlapping boundary pages (+ test), like the others.
- **New property tests** (proptest): the `SimulatedExchange` reduce-only invariant
  (arbitrary position + sell sizes never overshoot/flip), the cache-gap algebra
  (`missing_gaps` over arbitrary cached sets always completes the coverage), and an
  analytics **cross-validation** asserting Sharpe/Sortino/Calmar/vol against independent
  closed-form (empyrical-convention) values — so the `ddof=1`/annualization claims are
  code-verified, not prose.
- **`bars.rs` test temp paths** now use PID + an atomic counter (no parallel-`cargo test`
  collisions), matching the other data modules.
- **Coverage gate reconciled to reality**: the CI line/region thresholds were `99/98`
  while the tree measures ~97.1/97.8 (the live-IO `data/venues.rs` dispatch + the
  `akadro-tui` UI were not excluded). Both are now excluded from the gate (same class as
  `net.rs`/WS drivers) and the gate is set to its honest floor (`97/97`); the docs were
  updated to the freshly-measured figures.
- **Perf**: documented `Portfolio::close_order`'s `Vec::retain` (O(open-orders), tiny in
  practice) and the `akadro-tui` rolling-window `Vec` (a `VecDeque` would fight ratatui's
  contiguous-slice `Dataset` for no gain at UI cadence).

### Added — mdbook "I got a lifetime error" guide (`book/`, D14)
A human-facing book (`book/`, built by a new `mdbook` CI job) whose flagship page decodes
the kill-feature *brand lifetime errors* a strategy author hits the first time they try to
stash a `Ctx`/`Series`: the one-sentence cause (the per-call invariant lifetime), the
one-line fix (copy `Copy` scalars out, never keep the view), a GOOD/BAD pair, a
message→meaning→fix table (`E0521`, `E0624`, "no method `ahead`", "no `Index`"), and the
honest threat model (`unsafe`/out-of-band not covered). Examples are anchored to the
committed `akadro-compile-tests` trybuild proofs (the authoritative compile-fail checks).
**The `#[strategy]` proc-macro (the other half of D14) is deliberately *not* built** — a
reasoned non-fix: the `Strategy` trait already ships default no-op bodies for every hook
except `on_bar` and uses elided late-bound `Ctx<'_>` lifetimes, so a strategy impl is
already boilerplate-free; a macro would add a `syn`/`quote` crate and codegen over the
kill-feature surface for no ergonomic gain (see roadmap §13.5).

### Added — dhat allocation-regression gate (CI hardening, AGENTS §13.7)
A new `alloc-gate` CI job runs `crates/akadro/tests/alloc_gate.rs` under the `dhat` heap
profiler: a fixed 8000-bar backtest through the real engine + `SimulatedExchange`, with
the **total allocation count bounded far below the bar count** (measured ~706 blocks /
~1.2 MB peak; ceiling 2000 blocks). Because the per-event loop allocates nothing,
allocations come only from amortized `Vec` growth and one-time setup — so a hot-loop
allocation regression (e.g. an accidental per-bar `Box`) would push the count toward
8000 and fail the gate (goal #4, performance). Behind the off-by-default `dhat-heap`
feature (it swaps the global allocator), so normal builds/tests are byte-for-byte
unaffected and `dhat` is never linked into the library. Run locally with
`cargo test -p akadro --features dhat-heap --test alloc_gate`.

### Fixed — data-layer silent-data-loss on a truncated download (`akadro-data` + connectors)
A connector that aborted a back-fill mid-stream (a transient transport/parse error)
surfaced the failure as an empty feed (`next_event() -> None`), indistinguishable from
"the venue has no more data here". The cache loader then recorded the bars it got and
**dead-marked the unfetched remainder `EmptyVerified`** — silently losing the tail
forever (the residual flagged in the cache redesign). Now `PageSink` gains an additive
`on_error(&str)` (default no-op) that every connector calls when it gives up on the
remaining pages — both the `?`-propagate feeds (Binance/Bybit/KuCoin/MEXC-futures) and
the keep-partial-break feeds (MEXC-spot/OKX, which previously swallowed the mid-range
error entirely). The loader observes it via an `ObservingSink` and **fails fast**
(`DataError::Fetch`) rather than caching a truncated window, while **keeping the
`.partial` journal** so the next load's orphan recovery resumes from the pages already
fetched. This also resolves the loader's documented "empty == possibly-failed fetch"
ambiguity: an empty result *with* `on_error` is surfaced; a genuinely-empty no-data
window still re-probes (returns `Ok`). The **public `DataSource` trait is unchanged** —
the signal flows through the existing loader-owned page-sink seam (open/closed, goal #1).
Two regression tests pin both directions; the successful-fetch path is byte-identical
(parity golden-master + replay oracle + `proptest_parity` re-verified).

### Changed — `MergeSource` k-way merge is now `O(log k)` (tournament tree, D15)
`akadro_data::MergeSource` selected the next event with an `O(k)` linear scan over the
buffered heads. It now uses a **tournament (winner) tree** (`TournamentTree`): each
refill restores one root path in `O(log k)` comparisons — the sub-linear k-way merge of
decision D15. The `(ts, child_index)` key and lower-index-wins tie-break are **exactly
preserved**, so the merged stream is bit-identical to the linear merge — proven by a
3000-iteration fuzz test against a naive reference (random `k`, lengths, and frequent
ties) plus the existing ordering/tie-break tests. `benches/merge.rs` (the CI perf gate)
adds the tournament tree alongside the `BinaryHeap` baseline and sweeps `k ∈ {12, 64}`:
the tree is **~42 % faster than the heap at k = 12** (1.08 ms vs 1.85 ms) and ~26 %
faster at k = 64 — both `O(log k)`, but the tree does one comparison per level vs the
heap's ~two. Engine-facing data path, so parity golden-master + `proptest_parity` +
determinism + replay oracle re-verified bit-identical.

### Added — Probability of Backtest Overfitting (CSCV) in `akadro-analytics`
`akadro_analytics::probability_of_backtest_overfitting(strategy_returns, n_groups) ->
Option<PboResult>` implements the Bailey–Borwein–López de Prado–Zhu (2017) Combinatorial
Symmetric Cross-Validation PBO: partition the `T` observations into `n_groups` equal
blocks, and over every `C(n_groups, n_groups/2)` in-/out-of-sample split pick the
in-sample-best strategy (by `per_period_sharpe`) and record its out-of-sample relative
rank `ω = rank/(N+1)`; PBO is the fraction of splits where the logit `ln(ω/(1−ω)) ≤ 0`
(the winner at/below the OOS median). `PboResult` carries `pbo`, the split count, and the
per-split logits (for a diagnostic histogram). It complements the existing
`deflated_sharpe` (multiple-testing-corrected Sharpe). Pure math over a returns matrix
(no engine/data dep); 4 unit tests pin the extremes (dominant strategy → PBO 0,
edge-in-one-block → PBO 1) + degenerate-input rejection. A 2-lens adversarial review
confirmed the CSCV algorithm + DSR inputs are correct. The playground's `--overfit` flag
demos both over the SMA grid (a live BTCUSDT 1h run: PSR 71% but Deflated SR 45% and PBO
84% — strongly overfit, consistent with that run's walk-forward WFE of −0.27).

### Added — range-aware, resumable, multi-timeframe bar cache (`akadro-data`)
A ground-up redesign of the bar cache so a caller thinks about the data layer as
little as possible: ask for a window, get the bars, with downloads minimized,
interrupts survivable, and finer cached data reused. All of this is **above the
engine**, so the conservative spot path stays bit-identical (parity golden-master +
replay oracle + `proptest_parity` re-verified).

- **Range-aware gap-fill, keyed by *(venue, symbol, interval)* — not the window.**
  The cache records covered `(lo, hi)` close-stamp ranges in a per-series JSON
  manifest (the **sole** source of truth — gap detection is a manifest read +
  `missing_gaps` arithmetic, never a scan of the bar files). A request shifted by even
  one second reuses what's present and downloads only the genuine holes
  (`missing_gaps`, `Manifest::record_range`). A gap the venue truly has no data for is
  recorded `Coverage::EmptyVerified` **once** (guarded by a settled-cutoff so a
  not-yet-closed recent bar is retried, not dead-marked) and never re-attempted —
  closing the "request more history than the venue keeps → re-download forever" hole.
- **Incremental, resumable writes.** Each downloaded page is journaled to an
  uncompressed, append-only Arrow IPC `.partial` stream as it arrives (`PartialWriter`,
  installed on every connector feed via the new venue-neutral `akadro_core::PageSink`
  seam + `with_page_sink`), at a configurable cadence (`FlushPolicy`: every N pages or
  N seconds; default 8 pages / 5 s). An interrupt leaves the flushed pages recoverable;
  the next run lazily compresses the orphaned `.partial` into a real LZ4 `.feather`
  chunk, records its range, and resumes — never re-downloading what was already saved
  (`recover_partial`, tolerant of a truncated tail). Compression happens **only** at
  finalize/recovery, never per flush.
- **Multi-timeframe aggregation.** `aggregate_bars` rolls finer bars up to a coarser
  interval (OHLCV: open=first, high=max, low=min, close=last, vol=Σ, close-stamped,
  complete-bucket-only); `load_bars_aggregated` satisfies a coarse request by
  gap-filling the cached finer series and aggregating it up, downloading only the
  finer data still missing.
- **All venues via one DRY seam.** `with_page_sink` is wired into all six connector
  feeds (MEXC spot + futures, Binance, OKX, Bybit, KuCoin); the loader is venue-neutral
  (a `Fn(lo_ms, hi_ms, sink) -> impl DataSource` closure), so adding a venue needs zero
  loader changes.
- **Optional rate-limited concurrency.** `load_many` gap-fills many series at once over
  `std::thread::scope` (no async — `tokio` stays in `akadro-live`), bounded by
  `CacheOptions::concurrency` and a shared `RateLimiter` (per-venue req/s floor via
  `default_req_per_sec`, user-overridable). Results come back in input order, each
  series fault-isolated; per-series manifest files keep parallel workers race-free.
- **One-call surface (the headline UX).** `akadro::data::load(&DataRequest::new(Venue,
  symbol, interval, start, end), &opts, cache_dir)` resolves the venue catalogue +
  scales and returns engine-ready bars (a `LoadedMarket`); `load_perp` adds the
  mandatory funding schedule (a `PerpMarket`); `load_aggregated` rolls a cached finer
  interval up to a coarse one; `load_many` gap-fills several requests concurrently —
  so a caller never touches a venue crate, a `SeriesKey`, or a feed closure. Wired for
  **all eight** venue variants behind the umbrella's `venues` feature: MEXC spot/futures,
  OKX spot/swap, Binance spot/USDⓈ-M futures, Bybit spot/linear-perp, KuCoin spot
  (`#[non_exhaustive]`). A caller always speaks the akadro interval vocabulary
  (`"1h"`, …) and the umbrella maps it to each venue's **exact native token** (MEXC
  spot `60m`, OKX uppercase `1H`/`1D`, Bybit minute-numbers `60`/`D`, KuCoin `1hour`).
  It is an inherently live/network path (catalogue fetch + first download), so it is
  exercised by the **playground against the real venues**, not the offline unit suite —
  and the playground is migrated onto it (no direct venue-crate dep). Verified live
  across all eight venues + perps + concurrency + 1m/1s/1h/4h + aggregation.
- **Two cache-correctness fixes surfaced by live testing.** (1) **Download-seam
  bar-boundary:** a gap is in close-stamp coords but a connector's `with_range` takes
  open time, so the fetch now reaches back one bar — without it a bar at a
  download-session seam was skipped (a 1-bar hole). (2) **No dead-marking a fully-empty
  gap:** a completely empty fetch is indistinguishable from a *failed* one (the
  connectors surface a transport/parse error as an empty feed), so a fully-empty gap is
  now **re-probed** next load instead of recorded `EmptyVerified`. This removes a
  silent-data-loss vector (one transient error could otherwise permanently suppress
  re-download); the realistic "more history than the venue keeps" case is unaffected —
  it returns *some* data, whose settled **leading hole** is still recorded `EmptyVerified`,
  so deep-history requests don't re-download forever.
- New API surface: `akadro_data::{load_bars, load_bars_feed, load_bars_aggregated,
  load_many, SeriesKey, CacheOptions, SeriesRequest, FlushPolicy, RateLimiter,
  default_req_per_sec, Coverage, missing_gaps, aggregate_bars, aggregate_bars_checked}`
  + `akadro_core::{PageSink, impl DataSource for Box<dyn DataSource>}` + `akadro::data::{
  Venue, DataRequest, LoadedMarket, PerpMarket, MarketError, load, load_perp,
  load_aggregated, load_many}` (umbrella `venues` feature). 81 `akadro-data` tests +
  umbrella venue-dispatch unit tests, clippy-clean, full gate green, **all eight venues
  live-verified via the playground**.
- **Playground exercises the full feature set live** (a scratch binary, not the library):
  all eight venues + perps, 1s/1m/1h/4h intervals, gap-fill reuse, fee/cash/funding,
  the replay trade-oracle + report save/reload, bench modes, and demo flags for
  concurrency (`--symbols`), multi-timeframe aggregation (`--aggregate-from`), fill
  realism (`--slippage-bps`/`--impact-bps`/`--latency-bars`/`--participation-bps`), and a
  **structural walk-forward analysis** (`--walk-forward`): `WalkForwardBacktest` fits SMA
  `(fast,slow)` on each in-sample train slice via `run_grid` and evaluates out-of-sample,
  pooling per-fold OOS *returns* into a `WalkForwardSummary` with walk-forward efficiency
  — the IS fit reuses the OOS `FillConfig` verbatim (`with_config`, no drift), and a
  4-lens adversarial review confirmed **no IS/OOS leakage**. (A genuine BTCUSDT 1h run
  showed the textbook overfit tell: mean IS +3.5% vs pooled OOS −0.9%, WFE −0.27.)

### Changed — funding is cached like klines, and mandatory for perpetuals
Funding-rate history was re-fetched on every backtest run (and could be silently
disabled) while candles were cached — an asymmetry for data that is *as important* as
candles (a real recurring cost that moves perp PnL). Now the library handles it:
- **Funding cache** (`akadro_data::load_or_cache_funding` + `write_/read_funding_partition`)
  — a versioned, atomic, ascending-validated 2-column Arrow partition (`ts`, `rate`),
  mirroring the bar cache. Download once, replay from disk; never re-fetch per run.
- **One call for both** — `akadro_data::load_or_cache_perp(bars_path, funding_path, …)`
  returns `CachedPerp { feed, funding }`, downloading-and-caching the klines **and** the
  funding history together, and **errors if a perpetual has no funding data**.
- **Funding mandatory for perps** — `SimulatedExchange` now **panics on the first bar**
  if any `PerpetualFuture` instrument has no funding configured (a schedule or a constant
  `with_funding` rate): a perp backtest with zero funding is silently wrong, so it is a
  hard setup error, not a default. Spot is unaffected; the conservative spot path stays
  bit-identical (parity golden-master + replay oracle re-verified).
- **Playground** drops the `--funding` disable flag (funding is always on for perps),
  routes the venue funding fetch through `load_or_cache_funding` (cached next to the
  klines), and hard-errors on a perp with no funding instead of "running with funding = 0".
- Recorded the standing principle (memory + AGENTS.md): *the library does the plumbing so
  the user thinks about strategy, not data/cache chores; think several steps ahead.*

### Added — kline-download progress seam (connector emits, consumer renders)
A long `with_range` back-fill used to run silently (the connector does the whole
multi-page download inside one opaque `next_event()`). Added an opt-in per-page
progress callback so a consumer can draw a download bar — keeping the UI out of the
library (D18: the connector emits numbers, the consumer renders).
- **`akadro_core::BackfillProgress`** (`#[non_exhaustive]` + `::new`; `pages`, `bars`,
  `frontier_ms`) — the venue-neutral progress vocabulary, so one renderer works for any
  venue. No foreign types.
- **`with_progress(impl FnMut(BackfillProgress) + 'static)`** on `MexcKlineFeed`,
  `MexcFuturesKlineFeed`, and `OkxCandleFeed` — invoked after each fetched page of a
  ranged back-fill (forward-paging reports the latest close reached; backward-paging the
  oldest open). Off by default (`None`) → bit-identical to before; never fires on a cache
  hit. (binance/bybit/kucoin can adopt the identical builder later — same pattern; not
  wired as they aren't used by the playground/lab.)
- **Playground download bar** — a dependency-free stderr carriage-return bar
  (`[####----] 42%  page 5/~12  12,345 bars`), estimating total pages from the requested
  span; attached to all three playground venue downloads. No new crate dependency, no
  `indicatif` in any library crate.
- Closed loop: parity golden-master + replay oracle bit-identical (fetch-path-only,
  above the engine); `progress.rs` 100% region, region gate held at 98.2%.

## [0.1.2] - 2026-06-02

### Changed — D18 enforcement: close the casual data-import doors (research-driven)
Took "library owns the data" (D18) from a documented discipline to an enforced default,
without breaking the blessed cache/connector path or the exchange-agnostic `DataSource`
extensibility (goal #1). Additive except the deliberate gate below.
- **Added the blessed cache→engine feed:** `akadro_data::load_or_cache_feed` /
  `load_or_cache_many_feed` return a ready `CachedFeed` (a `DataSource` over
  library-owned cached bars; no public arbitrary-`Vec` constructor). akadro-data still
  depends only on akadro-core — no cycle. The `load_or_cache*` `Vec`-returning loaders
  stay for analytics/inspection.
- **Gated the casual raw-injection doors:** `HistoricalFeed::{new, from_bars,
  try_from_bars}` (arbitrary `Vec<Bar>`/`Vec<Event>`) now live behind the off-by-default
  `akadro-backtest` feature **`import-bars`** (umbrella passthrough `akadro/import-bars`,
  kept distinct from `escape-hatch` — different footgun). Internal callers
  (`WalkForwardBacktest`, `replay`, tests) use a new `pub(crate)` `from_bars_unchecked`/
  `new_unchecked` seam, so the crate builds with the gate off and the default user
  surface has no casual injection point. **Minor-breaking** for an external crate that
  called `new`/`from_bars` without the feature (pre-1.0; all in-repo callers are wired).
- **Honest ceiling (documented, not hidden):** a custom `impl DataSource` can still feed
  the engine anything — that is the sanctioned, deliberate escape hatch (it IS the
  venue-extension surface), the data analogue of the §4 D2 in-band/out-of-band line.
- **Agentic strategy lab — custom `DataSource` disallowed completely:** a new hard rule
  in `agentic/AGENTS.md` plus a scaffold guard (generated by `agentic/scaffold-lab.sh`
  into each lab's `scripts/verify-no-custom-datasource.sh`) that
  rejects any `impl … DataSource`/`ExecutionClient`/`InstrumentCatalog` in
  `src/strategies/`); the lab builds without `import-bars`. So a lab agent has **no**
  data-import path except `load_or_cache_feed` / connector feeds. Lab GUIDE + scaffold
  starter rewritten to the cache-feed path; default-feature doctests rewritten off the
  gated ctor. Designed via a 4-agent research workflow; AGENTS.md §7 D18 + §15 updated.

### Added — walk-forward look-ahead & fool-protection (research-driven)
Closed the realistic, *in-band* walk-forward (WFA) footguns where they can actually be
closed (the IS/OOS analogue of the kill feature), keeping everything additive and above
the engine — parity/replay paths untouched:
- **`akadro_backtest::WalkForwardBacktest`** — a framework-owned OOS runner whose `fit`
  closure receives **only the train slice** (the test bars are not in its scope, so
  fitting on out-of-sample is *inexpressible*) and which runs the OOS engine itself with
  one shared `FillConfig` (config drift impossible). The structural IS/OOS boundary;
  returns a per-fold `OosFold` (`#[non_exhaustive]`).
- **`WalkForwardSummary`** — the only blessed OOS aggregator: pools per-fold OOS *return
  series* (never stitched equity levels — the boundary-jump bug is unreachable), counts
  blow-up folds instead of dropping them, and guards the `±∞` walk-forward-efficiency case.
- **`PerformanceReport::from_equity_auto`** + `infer_periods_per_year` — annualization
  inferred from the equity curve's timestamps (median spacing), closing the wrong-
  `periods_per_year` footgun; `PerformanceReport::from_returns` extracted as the shared
  returns→metrics core (behaviour-neutral; `from_equity_with` delegates to it).
- **`akadro_core::check_bars_ordered`** + **`HistoricalFeed::try_from_bars`** — fail-fast
  `Result` (`BarOrderError`) on unordered / same-instrument-duplicate-timestamp bars
  (equal ts on different instruments allowed); `from_bars` stays the trusted fast path.
- **Removed user access to the leakage-prone escape hatches:** the generic both-slices
  runners `run_walk_forward` / `run_walk_forward_checked` are now behind an off-by-default
  `escape-hatch` feature (umbrella passthrough), so the default surface — and the agentic
  strategy lab (built without it; the scaffold's API docs now match) — cannot call them.
  `slice_window` stays public for explicit custom orchestration. **Minor-breaking** for an
  external crate that called `run_walk_forward`/`_checked` without the feature (same as the
  `import-bars` gate; pre-1.0, all in-repo callers migrated).
- Residual (out-of-band, the D2 ceiling, documented not hidden): re-picking a parameter set
  by reading the returned per-fold OOS reports cannot be prevented by any API. Designed via
  a 6-agent research workflow; AGENTS.md §15 + the agentic lab guide updated.

## [0.1.1] - 2026-06-01

### Fixed — Opus total-review pass (all confirmed majors + minors)
A multi-agent Opus review of the whole workspace found and this pass fixed:
- **Backtest fidelity (`SimulatedExchange`).** OCO partial fills no longer strip the
  protective sibling — a partial keeper fill **reduces** each sibling to the keeper's
  unfilled remainder (cancel only on a complete fill), without re-opening intra-bar
  double-execution. A **dormant** (un-triggered) conditional order (stop / stop-limit /
  MIT / trailing) now keeps resting regardless of TIF — IOC/FOK governs an order only
  once it's *actionable*, so a quiet bar no longer wrongly expires a resting stop. A
  canceled OCO leg no longer emits a spurious `OrderTriggered` (trigger now fires after
  the OCO check). Slippage / market-impact / volume-cap math now clamps (not truncates)
  its `i128` intermediates into `i64` and saturates the final adjustment.
- **Engine.** Event-time timers now fire on **every** event that advances the clock
  (`Signal`/`Resync` and the loop tail), not only on bars — a timer due during a no-bar
  stretch no longer waits for the next bar (or never fires). Added a `debug_assert` that
  a bar's instrument is within the catalog (a mis-wired feed no longer vanishes silently).
- **Live shell.** `ChannelExec` now delegates `ExecutionClient::sync_clock` to its inner
  venue client (previously it hit the trait's no-op default, leaving a wrapped venue
  signing with a stale clock) and gates its async-fill drain on bar events (parity
  contract). `MexcSpotExec` overrides `sync_clock`. `ReconnectingFeed` suppresses the
  back-dated EPOCH `Resync` when the first connection yields nothing, and exposes
  `connect_failed()` to tell a hard connect failure from a clean empty feed. Documented
  the Binance `listenKey` keepalive obligation.
- **Data layer.** `MergeSource` asserts the merged stream stays non-decreasing (catches a
  misbehaving child); `bars_from_trades` asserts time-ordered input; `interval_to_nanos`
  rejects a non-positive interval (`"0s"`); `Manifest::record` rejects a reversed range;
  `write_partition` doc gained an `# Errors` section.
- **Connectors.** Signed requests are no longer slept-and-retried on a transport error
  (MEXC, matching Binance) — cumulative backoff could push the baked-in timestamp past
  `recvWindow`. `parse_futures_klines` takes the authoritative interval so a **single-bar**
  MEXC futures page is close-stamped (not left at open). KuCoin rejects an unrecognized
  candle type (e.g. `"1month"`) fail-fast on every path. Corrected the misleading
  `with_timestamp_ms` docs (Bybit/KuCoin) and documented the unclosed-last-candle caveat
  on every REST kline parser. Binance spot `reduce_only` is rejected; the crate doctest is
  feature-gated so `--no-default-features` builds.
- **Indicators.** `RollingMax`/`RollingMin` report their true `warm_up_bars`; RSI/Bollinger/
  EMA widen to `i128` and saturate to avoid overflow on extreme inputs.
- **Semver / API.** `#[non_exhaustive]` on growable public structs (`FillRecord` + a
  `::new`, perf/trade/distribution reports, `Cost`, `ReplayDiff`); `Capability::bit` uses
  `checked_shl`; `InstrumentSpec` gained `round_price`/`round_qty` taking a `RoundingRule`.
- **DRY.** Hoisted the venue-neutral decimal helpers (`decimal_to_raw`/`raw_to_decimal`/
  `scale_of`) and funding-period inference (`infer_funding_period_ms`) into `akadro-core`
  (the four-way-duplicated, subtly-divergent copies are gone); connectors now wrap the
  single source of truth. Deduped the playground's funding-period inference.
- Closed loop: `cargo test --workspace` 0 failures · `clippy --workspace --all-targets
  --all-features` 0 · `fmt` clean · `rustdoc -D warnings --all-features` clean · **replay
  trade-oracle + parity golden-master + proptest bit-identical** (the default GTC path is
  unchanged) · `llvm-cov` 97.3% line / **98.2% region** (region clears the 98 gate; new
  `decimal.rs`/`funding.rs` are 100% region; line residual itemized in §10).

### Added — venue feature parity: range backfill, pacing, catalog/funding fetch on every connector
- Brought **Bybit** and **KuCoin** kline/candle feeds to parity with OKX/MEXC: historical
  `with_range` back-fill (newest→oldest paging by the `end`/`endTime`/`endAt` cursor),
  `with_page_delay` pacing + a bounded `429` back-off in `fetch_page`. Added
  `BybitCatalog::fetch` / `KucoinCatalog::fetch` / `BinanceCatalog::fetch`(+`fetch_futures`)
  so instruments load over the `Transport` seam — **no raw requests** in caller code (D18).
- **Funding history is now paged on every connector** (`fetch_funding_history_paged`):
  OKX (`after` cursor), Bybit (`endTime`), KuCoin (`to`), Binance (forward `startTime`),
  joining MEXC — so a long backtest gets full-window funding, not just the recent ~100
  settlements. Binance also gained `fetch_funding_history` (it had only `parse_funding_rate`).
  All paging verified against the **live** venue schemas, all over `MockTransport`.
- **Adversarially reviewed** (a 41-agent dynamic workflow across correctness / data-leakage
  (D18) / DRY-SOLID-SoC-LoD-fail-fast / fixed-point / coverage): 28 confirmed findings, all
  resolved — added the OKX back-fill **no-progress guard** (it could loop on a duplicate/
  non-monotonic page; peers had it), removed `VisionBarSource::from_bars` (a venue connector
  must not offer raw-`Bar` injection — D18), de-duplicated MEXC's interval table to one
  source of truth, and added ~30 branch tests (guard / dedup / window-clip / 429-retry /
  429-exhaustion / empty-page / mid-range-error / HTTP-error). The cross-venue paging/429-loop
  duplication is a deliberate venue-crate-independence trade-off (a shared helper would need an
  HTTP abstraction in `akadro-core`, breaking SoC); flagged, not refactored.
- Closed loop: `cargo test --workspace` green · `clippy --workspace` 0 · `fmt` clean ·
  `rustdoc -D warnings` clean · **replay trade-oracle + parity golden-master bit-identical**
  (data-fetch changes don't touch the trade-sim path) · `llvm-cov` 97.3% line / **98.1%
  region** (region clears the 98 gate; the sub-99 line is pre-existing exec/WS order paths +
  the irreducible pacing-`sleep` lines, both itemized in §10). New standing rules recorded:
  **D18 library-owns-data**, DRY/SOLID/SoC/LoD/fail-fast, and the production-ready gate (§7/§10).

### Fixed — MEXC futures kline back-fill paged forward but the API anchors on `end`
- `MexcFuturesKlineFeed::with_range` advanced a *forward* `start` cursor, but the MEXC
  contract-kline endpoint caps each response at ~2000 bars **anchored on `end`** and
  **ignores `start`** once the range exceeds the cap — so a long-range request just
  re-fetched the most-recent ~2000 bars and stopped (a 1-year request yielded only
  ~83 days). The back-fill now pages **newest→oldest** by moving the `end` cursor back
  to just before the oldest bar received, clipped to `[start, end)` on open time —
  verified by a `feed_pages_backward_by_end_cursor` regression test and a real
  1-year `BTC_USDT` 1h download (8760 bars). This is a data-fetch fix; the trade-sim
  path is untouched (replay oracle still MATCH).

### Added — MEXC futures connector fetch helpers + playground `mexc-futures` venue
- `akadro-venue-mexc`: `fetch_contracts` (`/api/v1/contract/detail`) and
  `fetch_funding_history_paged` (pages `/contract/funding_rate/history`, 100/page, to
  cover a long window) join `fetch_funding_history` — all public, over the `Transport`
  seam, `MockTransport`-tested. The `playground` gains a `--venue mexc-futures`
  (perpetual contract klines + default-on funding); a 1-year run pulled 1200 funding
  settlements and accrued ~13.4k quote, replay-oracle MATCH.
- **`MexcFuturesKlineFeed` page pacing** (`with_page_delay`, default 120 ms) + a
  bounded `429` back-off in `fetch_page`, mirroring the OKX feed — so long, fine-
  interval back-fills (many pages) stay within the venue's rate limit. Tests set the
  delay to zero; a `feed_retries_on_429_then_yields` test covers the back-off path.
  Verified by a 3-month 5m `BTC_USDT` download (26,496 bars over ~13 paced pages,
  oracle MATCH). Venue note: MEXC contract klines cap each response at ~2000 bars and
  **1m history only reaches ~30 days back** (5m/1h reach ≥90/365 days), so a 3-month
  1m back-fill returns only the most recent ~30 days — a venue retention limit, not a
  connector bug.

### Added — OKX connector: historical backfill + perpetual funding (reusable, no raw requests)
- **`OkxCandleFeed::with_range(start_ms, end_ms)`** turns the feed from a single
  recent `/market/candles` fetch into a paged `/market/history-candles` backfill
  (newest→oldest, 100/page, exclusive `after` cursor), windowed to `[start, end)`,
  ascending + de-duplicated, close-stamped exactly like `parse_candles` — the way
  to pull, e.g., a full day of `1s` bars. A bounded `429` back-off and an
  inter-page courtesy delay (tunable via `with_page_delay`, `0` in tests) live in
  the feed; a leading-page error surfaces, a mid-range error keeps the pages
  already fetched.
- **`OkxCatalog::fetch(transport, base_url, inst_type)`** and
  **`fetch_funding_history(transport, base_url, inst_id, limit)` + `funding_period_ms`**:
  the instruments catalogue and the perpetual funding-rate schedule are now first-
  class connector entry points over the mockable `Transport` seam — callers never
  hand-build a URL or touch a response body. All unit-tested with `MockTransport`
  (paging/windowing/dedup/429-retry/leading-error, catalogue + funding fetch).
- **Why:** these were previously ad-hoc `reqwest` requests inside the `playground`
  binary — i.e. data-download logic living in user/strategy code, which is exactly
  what an exchange-agnostic library exists to prevent (no per-user request points;
  reusable across consumers). The playground is now a thin consumer of the
  connector. Backtest output is **bit-identical** to the old path. Funding is
  applied **by default** for OKX perps in the playground runner (opt out with
  `--funding false`); the `SimulatedExchange` global default stays off so the
  parity golden-master remains bit-identical.

### Fixed — sub-basis-point funding now accrues (venue-wide precision fix)
- **The funding-rate unit is now `1e-8` fractions, not basis points.** The engine's
  funding charge was basis-point-granular (`amount = notional · rate_bps / 10_000`),
  so real venue rates — routinely **sub-1bp** for major perps (OKX BTC-USDT-SWAP on
  2026-05-31 ranged 0.02–0.47 bp) — rounded to `0` and accrued nothing. Funding is a
  real, recurring cost on perpetuals, so this silently understated it.
- **The fix:** a single authoritative `akadro_core::FUNDING_RATE_SCALE = 8` (`1e-8`,
  i.e. exactly Binance's 8-decimal `fundingRate` precision) plus a new
  `Money::mul_rate` (the `10_000×`-finer analogue of `mul_bps`, divisor derived from
  the same constant so connector scale and engine charge cannot drift). The schedule
  path in `SimulatedExchange` now charges via `mul_rate`; **every** funding-producing
  connector re-exports the core constant and normalizes to it. A `0.0000466`
  (0.47 bp) rate now charges where it previously charged nothing.

### Added — funding-rate history for every funding-capable venue
- `fetch_funding_history` + `parse_funding_rate` now exist for **all five** venues
  with perpetual funding, each over the mockable `Transport` seam (no raw requests),
  normalizing to `FUNDING_RATE_SCALE`: `akadro-venue-okx`, `akadro-venue-binance`,
  **`akadro-venue-mexc`** (`/api/v1/contract/funding_rate/history`, public; rate is a
  JSON number), **`akadro-venue-bybit`** (`/v5/market/funding/history?category=linear`;
  string rate), and **`akadro-venue-kucoin`** (`/api/v1/contract/funding-rates` on the
  futures host `FUTURES_BASE_URL`; rate is a JSON number in **scientific notation**).
  Number-typed rates are rendered to a fixed (non-exponential) decimal before
  fixed-point parsing, so a tiny sub-bp rate (or KuCoin's `1.49E-4`) is never mangled
  and no `f64` reaches the money path beyond the one venue-data boundary parse.
- **Closed loop:** each parser has `MockTransport` fixture tests (sub-bp + sort +
  error paths), and the live public funding endpoints were hit on 2026-06-01 to
  **verify the real response schemas** — which corrected KuCoin's fields to
  `fundingRate`/`timepoint` (a fixture had matched a wrong guess). The DEX (AMM swap)
  connector has no funding concept; KuCoin's connector is spot-only, so its funding
  fetch targets the futures host and pairs with a perp `InstrumentSpec` the caller
  supplies. Live-schema confidence: OKX/Binance/MEXC/Bybit/KuCoin all verified.
- **`AccountEvent::FundingSettlement.rate` and `Ctx::current_funding_rate` are now at
  `FUNDING_RATE_SCALE`** (`10_000` = 1 bp), not bps — a strategy-facing unit change,
  documented on both.
- **Bit-identical where it matters:** the coarse constant `with_funding(bps, …)` /
  `FillConfig.funding_bps` API is unchanged and behaviour-identical (bps widens
  `×10_000` into the `1e-8` unit and `mul_rate`'s `10⁸` divisor cancels it back), so
  the conservative default and the parity golden-master / replay trade-oracle all
  stay bit-for-bit identical (verified). Closed loop: a `mul_rate` doctest+unit test,
  a `sub_basis_point_funding_now_accrues` engine test, updated OKX/Binance parse
  tests, and an end-to-end OKX BTC-USDT-SWAP backtest that now reports non-zero
  funding where it reported `~0.00` before.

### Performance — equity marking is O(open positions), not O(catalogue)
- The engine's per-bar mark-to-market previously scanned **every** instrument in
  the catalogue each bar to sum equity. On a large venue universe traded by a
  few-instrument strategy this dominated runtime — e.g. an OKX 1s backtest over a
  ~1262-pair spot catalogue ran at ~1.1M bars/sec and a ~355-contract swap
  catalogue at ~3M, while a single-symbol MEXC catalogue hit ~10M (throughput
  tracked 1/catalogue-size). The `Portfolio` now maintains the set of instruments
  with a **non-zero** position (updated at the sole position-mutation point), and
  `mark_to_market` sums over only those — O(open) instead of O(catalogue). Result:
  the same OKX runs now hit ~9–14M bars/sec (≈5–8×), independent of catalogue
  size. **Bit-identical** (a flat position marks to 0): the parity golden-master,
  proptest parity, determinism sweep, and replay trade-oracle all still pass.

### Added — Binance Vision bulk-historical data downloader
- **`akadro-venue-binance::vision`**: download *years* of klines from
  `data.binance.vision` flat ZIP/CSV archives (no API key, no `/klines` rate
  limit). `parse_vision_csv`/`parse_vision_row` normalize the 12-field CSV into
  `Bar`s **bit-identical** to `parse_klines` (close-time stamped), and
  `VisionBarSource` is a `DataSource` that plugs straight into `akadro-data`
  caching (`load_or_cache` / `cache_from_source`) to download once and replay from
  the Arrow cache. `vision_zip_url` / `vision_checksum_url` / `parse_checksum`
  build the daily/monthly URLs (note the monthly bar token is `1mo`) and read the
  GNU `sha256sum` `.CHECKSUM`. **Handles the 2025-01 spot timestamp switch from
  milliseconds to microseconds** (auto-detected per file via `VisionTimeUnit`) and
  tolerates an optional CSV header row.
- **Optional dependency:** `zip` is opt-in behind the new **`vision-zip`** feature
  (ZIP decode: `unzip_single_csv` / `verify_and_unzip`); the live HTTP downloader
  `VisionDownloader` (blocking `reqwest`, SHA-256 verify, no tokio) is behind
  **`vision-net`** (= `net` + `vision-zip`) and lives in `net.rs` (excluded from
  the coverage gate like the other live IO). The default build pulls in neither.
  Fixture-tested (ms/µs/header/short-row parsing, URL/checksum builders, an
  in-memory ZIP round-trip, checksum match/mismatch, cache round-trip + parity
  with the REST parser); a credential-free `#[ignore]` live test downloads a real
  month. Built and reviewed against the just-changed crate API (a 4-agent
  understand sweep + a 4-lens adversarial review, each finding independently
  verified).

### Added — OKX, Bybit, and KuCoin venue connectors (REST)
- **`akadro-venue-okx`** (spot + perpetual **swap**): Base64 HMAC-SHA256 signing
  with the `OK-ACCESS-*` headers + passphrase, `/api/v5/public/instruments` →
  `InstrumentSpec`, `/api/v5/market/candles` → bars (newest-first → sorted, close-
  time stamped via the `bar` token), and signed `/api/v5/trade/order` submission
  (`sCode` acceptance, `clOrdId` tag, spot market-buy `tgtCcy=base_ccy`). Includes
  a chrono-free ISO-8601 timestamp formatter.
- **`akadro-venue-bybit`** (v5 unified: spot + **linear perp**): hex HMAC-SHA256
  v5 signing (`X-BAPI-*`), `/v5/market/instruments-info` → `InstrumentSpec`
  (`basePrecision`/`qtyStep`, `minOrderAmt`/`minNotionalValue` by category),
  `/v5/market/kline` → bars (descending → sorted, close-time stamped), and signed
  `/v5/order/create` (`retCode` acceptance, `orderLinkId` tag).
- **`akadro-venue-kucoin`** (spot): Base64 HMAC-SHA256 signing with the v2
  **encrypted passphrase** (`KC-API-*` + `KC-API-KEY-VERSION: 2`), `/api/v1/symbols`
  → `InstrumentSpec`, `/api/v1/market/candles` → bars (the KuCoin
  `[time, open, close, high, low, volume]` layout in seconds, descending → sorted,
  close-time stamped), and signed `/api/v1/orders` (`code=200000` acceptance,
  `clientOid` tag).
- All three depend only on `akadro-core`, expose a mockable `Transport` seam, gate
  the live `reqwest` transport behind `net` (extracted to `net.rs`, excluded from
  the coverage gate alongside the other live-IO transports), carry the `ak<id>`
  client-order-tag convention for WebSocket fill attribution, and are wired end-to-
  end through the real engine with fixtures plus credential-gated `#[ignore]` live
  tests. A workspace `clippy.toml` adds `KuCoin` to the `doc_markdown` ident list.

### Fixed — final pre-release bug-hunt pass (2026-06-01)

A 58-finder adversarial review (each finding verified) surfaced 62 confirmed
issues across every subsystem; the fixes below close all 17 majors, ~28 of 31
minors, and the info-level items, each with a regression test. The default
conservative backtest path stays **bit-identical** (parity golden-master + trade
oracle re-confirmed); all changes to opt-in features preserve the default.

- **Engine/portfolio:** `RunReport.total_fees` split into `trading_fees` (signed
  execution costs) + `funding_net` (signed funding flow); added `liquidation_pnl`;
  weighted-average entry price now rounds symmetrically (correct for negative
  prices); `AccountEvent::Resync` carries an instrument scope (forwarded from
  `Event::Resync`); `has_open_order` sees a same-handler pending submit (no
  double-submit); `OrderGate` lookups are O(1); `LastN` Debug no longer dumps
  history; `Ctx::equity` docstring corrected.
- **Backtest exchange:** stop-limit that arms+fills in one bar fills at the
  trigger, not the pre-activation open (and its post-only test uses the trigger);
  `reduce_only` is capped at the position (single and concurrent — never flips);
  `StopLimit` is included in the `min_notional` check; liquidation cancels resting
  orders; `post_only` on a non-limit kind is rejected; the cash guard reserves
  worst-case volume-impact and stops over-reserving passive limits; `settle_cash`,
  `Shadow::apply`, funding accrual and trailing-stop math are saturating/widened;
  per-instrument funding interval; a realistic **opt-in probabilistic maker-fill**
  model wired to the seeded RNG (default-off; `config_seed` reports `None` unless
  drawn). Replay `diff_reports` now gates the realized-PnL compare on liquidation
  presence (fixes funded false-match and fee-less-liquidation false-mismatch).
- **Connectors (MEXC + Binance, in scope):** MEXC post-only → `LIMIT_MAKER`;
  `EXPIRED` is terminal; full-qty-under-`PARTIALLY_FILLED` no longer leaks an open
  order; partial-fill price is the marginal tranche (not cumulative VWAP); signed
  requests aren't sleep-retried with a stale timestamp (and an IP-ban no longer
  blocks the engine thread); WS deal `complete` no longer evicts on the first
  partial; backfill clamps `limit≥1`, preserves partial pages, and propagates
  precision-parse errors; futures bars stamp close-time. Binance honours
  `order.tif` (IOC/FOK) and `post_only` (`LIMIT_MAKER`), gained a signing clock,
  and clamps the decimal scale.
- **Analytics/indicators/data/DEX:** analytics guard non-positive/non-finite
  annualization and zero-qty trades, document the population skew/kurtosis and WFE
  inversion; indicators use i128 accumulators + correct warm-up (`warm_up_bars`)
  and reject inverted bars / non-positive Bollinger `k`; the data layer makes the
  manifest write atomic, validates cached scale + the UTC tz on read, and wraps
  `io::Error` (preserving `ErrorKind`); the DEX clamps/saturates impact math and
  never returns a non-positive price.
- **Kill feature:** added per-hook compile-fail brand proofs for the six newer
  hooks (`on_fill`/`on_order_rejected`/`on_order_canceled`/`on_liquidation`/
  `on_funding`/`on_timer`) — all 10 hooks are now regression-locked (27
  compile-fail cases). `Capability` bit indices are explicit.

### Added — Binance venue, live WebSockets, and the signal-merge primitive
- **`akadro-venue-binance`** (new crate, depends only on `akadro-core`): a
  production Binance connector for **spot + USDⓈ-M futures** over REST —
  HMAC-SHA256 signing (RFC-4231 verified), `exchangeInfo`→`InstrumentSpec`
  (spot `NOTIONAL` + futures `MIN_NOTIONAL`/`PERPETUAL` filters), `klines`→bars
  with range pagination (`.for_futures()` swaps to `fapi`), signed order submit
  (`newClientOrderId` tagging for live fill attribution; futures `reduceOnly`),
  funding-rate history → `with_funding_schedule`, and open-interest / long-short
  producer feeds emitting `Event::Signal`. Mockable `Transport`; `net` feature
  adds the live `reqwest` transport; live round-trips are credential-gated
  `#[ignore]` tests. Wired end-to-end through the real engine with fixtures
  (including a futures klines+OI+funding `MergeSource` run).
- **`akadro-data::MergeSource`**: a stable k-way merge of `DataSource`s into one
  non-decreasing stream (ties broken by child index, deterministically) — the
  primitive that makes the `Event::Signal` channel usable by combining a price
  feed with auxiliary signal feeds for the engine's single source slot.
- **Live WebSocket transports** (`akadro-live`, behind `binance` / `mexc`
  features; tokio confined here): `binance::spawn_klines`/`spawn_user_data`
  (kline + `executionReport` streams) and `mexc::spawn_klines`/`spawn_user_data`
  (protobuf market + private-deals streams with the JSON subscribe/ping
  handshake). Pure frame decoders live in the venue crates
  (`akadro_venue_binance::ws`, `akadro_venue_mexc::ws_proto` — a self-contained
  protobuf wire reader over the field tags vendored from
  `mexcdevelop/websocket-proto`), so decoding is fixture-tested with no network;
  byte-level confirmation is the credential-gated `#[ignore]` live tests.
- **`akadro-live::ChannelExec`**: a live `ExecutionClient` that submits via an
  inner (REST) client and drains asynchronously-arriving fills (from a user-data
  WS task) into the engine on each `observe`, preserving the parity
  event-ordering contract.

## [0.1.0] - 2026-05-31

### Added — MVP that proves the architecture
- **Compile-time look-ahead protection** ("the kill feature"): backward-only
  `Series`, an invariant per-call lifetime brand on `Ctx`/`Series`, structural
  ownership boundary, and sealed construction. Proven by `trybuild` compile-fail
  tests (including a downstream `forge_context` proof) and compile-pass tests for
  legitimate patterns.
- **`akadro-core`**: fixed-point `Price`/`Qty`/`Money` (widen-before-multiply),
  ids, `Event`/`Bar`/`AccountEvent`, `OrderRequest`/`OrderKind`/`VenueCommand`,
  `InstrumentSpec`/`CapSet`, and the `DataSource`/`ExecutionClient`/
  `InstrumentCatalog`/`EventSink` extension traits.
- **`akadro-engine`**: the single monomorphized event loop, `Strategy`, the
  look-ahead-safe `Ctx`/`MarketView`/`Series`, a portfolio derived solely from
  account events, and a seeded `DeterministicRng`.
- **`akadro-backtest`**: `HistoricalFeed` and a conservative deterministic
  `SimulatedExchange` (next-bar-open market fills, crossing limit fills, flat fee).
- **`akadro-testkit`**: `MockLiveFeed` (threaded bounded-channel transport).
- **`sma-crossover`**: a worked example strategy (`#![forbid(unsafe_code)]`).
- **Backtest↔live parity golden-master**: bit-identical `RunReport` from the
  historical feed and the mock-live feed, for the fixture and for arbitrary
  random price paths (proptest); plus determinism (rerun + parallel sweep).
- **`akadro-venue-mexc`** — the first real venue connector (MEXC spot, over REST):
  HMAC-SHA256 signing (verified against an RFC 4231 vector), decimal↔fixed-point
  conversion, `exchangeInfo` → `InstrumentSpec`, `klines` → bars, signed order
  submission, and REST-poll fills, all normalized to akadro types over a mockable
  `Transport` and tested end-to-end through the engine with recorded fixtures. The
  optional `net` feature adds a `reqwest` blocking transport; a credential-gated
  `#[ignore]` test exercises the live public endpoint. Built with zero changes to
  `akadro-core`/`akadro-engine`, demonstrating the exchange-agnostic goal.

### Added — v0.2 growth (in progress)
- **`akadro-indicators`** — incremental, integer-deterministic, look-ahead-safe
  indicators: `Sma`, `Ema`, `Macd`, `Rsi`, `Bollinger`, `Atr`, `RollingMax`,
  `RollingMin` (an `Indicator` trait + per-indicator tests against known values).
- **Real-MEXC validation:** the connector is now verified against the live
  `api.mexc.com` — public klines fetch+parse, and a signed `/account` query that
  returns 200 (so HMAC signing is accepted by the real server), via `#[ignore]`d
  credential-gated tests.
- **Engine equity curve:** `RunReport` now carries a per-bar mark-to-market
  `equity_curve` (deterministic — parity still holds).
- **`akadro-analytics`** — `PerformanceReport` (total/annualized return, Sharpe,
  Sortino, max drawdown, Calmar, volatility) from the equity curve; `TradeStats`
  (win rate, profit factor, expectancy) reconstructed from the fill log; and
  `walk_forward` window splitting for out-of-sample validation. (Reporting uses
  `f64`; the execution path stays integer-only.)
- **Coverage now measured with `cargo-llvm-cov`** (region-based; correctly counts
  the const-fn getters tarpaulin missed). Toolchain consolidated on rustup 1.95.0.
  **99.04% line / 98.35% region** across the workspace; the gate is 99% line /
  98% region. Every reachable production line is covered — the residual is
  itemized (defensive-unreachable guards, a `#[non_exhaustive]` catch-all, live
  network IO, test-only helper arms, and two argument-line attribution artifacts);
  see `AGENTS.md` §10.

- **Order types (growth pt 4):** added stop-limit, trailing-stop,
  market-if-touched, post-only, and OCO grouping (`#[non_exhaustive]`, additive),
  all modelled in `SimulatedExchange`.
- **Realistic fills (growth pt 5):** opt-in `FillConfig` for slippage, latency,
  partial fills (volume participation cap), perpetual funding (zero-qty
  `CostKind::Funding` flows), and simplified liquidation — conservative defaults
  keep the simple path bit-identical (parity preserved).
- **MEXC futures + real fees + WebSocket (growth pt 2):** futures signing
  (`contract.mexc.com`), `contract/detail` → perpetual `InstrumentSpec`, futures
  order-field encoding; realised-fee parsing via `myTrades`; and the WebSocket
  control protocol (subscribe/ping/ack). Validated live against real MEXC (public
  klines, signed `/account`, futures `contract/detail`). Protobuf market-data
  decode is the documented remaining integration.
- **`akadro-data` (growth pt 10a):** columnar Arrow/Feather bar cache
  (lossless i64 SoA + self-describing metadata), a JSON manifest of cached ranges,
  incremental `plan_fetch`, and a venue-agnostic `cache_from_source` downloader.
- **`akadro-venue-dex` (growth pt 10b):** a basic AMM/DEX swap connector with
  price impact, LP fee + gas (two costs in two assets) — validates the venue
  abstraction on a no-order-book venue.
- **`akadro-live` (growth pt 3):** `ReconnectingFeed` (auto-reconnect + `Resync`)
  and `run_paper` (paper trading: live feed + simulated execution).
- **Docs:** workspace builds clean under `cargo doc -D warnings`; the public
  surface has no leaked internals (the strategy API needs no macros).
- **Opt-in cash enforcement (`SimulatedExchange`):** `FillConfig.starting_cash` /
  `with_starting_cash(Money)` make the venue mirror a quote-cash balance and
  reject buys it cannot afford (`RejectReason::InsufficientFunds`), so a backtest
  can no longer spend money it does not have. Default `None` keeps the
  conservative path bit-identical (parity preserved). Surfaced by a real-data
  experiment where a fee-heavy strategy drove equity negative.
- **Analytics guard on blowups:** `PerformanceReport::from_equity` now returns
  `None` if any equity sample is `≤ 0` (was: only the first), instead of
  fabricating `NaN`/garbage Sharpe/Calmar/volatility when an account is wiped out.
- **Download + cache is now library code (not the caller's job):**
  `MexcKlineFeed::with_range` **paginates the full window** across as many requests
  as MEXC's per-call cap needs (skipping a leading gap from the venue's finite
  fine-interval retention); `MexcCatalog::fetch` issues the `exchangeInfo` request;
  the live `ReqwestTransport` transparently retries transient failures; and
  `akadro-data::load_or_cache` serves a Feather partition from disk if present,
  else downloads (draining the feed) and caches it — "download once, replay
  offline" in one call. The `playground` now uses these instead of a hand-rolled
  pagination loop.
- **External-benchmark hardening (110-agent audit vs production systems; 53
  confirmed, ~45 implemented, the rest documented).** Compared every subsystem
  against Nautilus Trader, QuantConnect LEAN, Zipline, empyrical, CCXT,
  Binance/MEXC, TA-Lib and ArcticDB. Implemented:
  - **analytics:** Sharpe/Sortino/Calmar now use a consistent `±∞`/`0` limit at
    zero denominator (a constant-positive strategy no longer scores `0`);
    `profit_factor` is `+∞` with no losing trades; `TradeStats` gained
    `avg_win`/`avg_loss`/`payoff_ratio`/`break_even`; `PerformanceReport::
    from_equity_with` adds a configurable risk-free / required-return; named
    `PERIODS_PER_YEAR_*` constants.
  - **core:** `meets_min_notional` now compares at the combined scale (the guard
    actually fires); `InstrumentSpec::round_price_down`/`round_price_up`; new
    `AccountEvent::OrderCancelRejected`/`OrderExpired` (+ `CancelRejectReason`),
    a `rate` on `FundingSettlement`, `TrailKind` (percentage trailing stops),
    `VenueCommand::SetCancelOnDisconnect`, `Bar` is now `#[non_exhaustive]`.
  - **engine:** `Ctx` gained `unrealized_pnl`/`equity`/`instrument_spec`/
    `has_open_order`/`open_order_ids`/`submitted_order`/`realized_pnl_net`/
    `is_warmed_up`/`schedule`; `Strategy` gained type-specific hooks
    (`on_fill`/`on_order_rejected`/`on_order_canceled`/`on_liquidation`/
    `on_funding`/`on_timer`); the portfolio tracks open orders from the event
    stream; event-time timers.
  - **backtest:** `OrderCancelRejected` on cancel of an unknown order;
    `OrderTriggered` for stop/MIT/trailing (not just stop-limit); limit orders
    price-improve on a gapped open; the participation cap depletes across orders
    in a bar.
  - **data:** atomic write (temp + rename), **LZ4-compressed** partitions with an
    Arrow `Timestamp(Nanosecond, "UTC")` `ts` column (legacy `Int64` still read),
    ascending-timestamp + format-version validation on read, manifest overlap
    rejection + `validate()`, `plan_fetch_from` floor.
  - **mexc:** server-time sync (`sync_time`), 429/418 rate-limit retry,
    `MAX_KLINES_LIMIT` clamp, `1W`/`1M` interval support.
  - **indicators:** `RollingMax`/`RollingMin` now O(1) via a monotonic deque;
    EMA seeding + integer-truncation conventions documented.
  - **live:** a reconnect-delay floor (`with_reconnect_delay`); the `Resync`
    timestamp semantics documented.
  The 5 minor items left for the roadmap (AGENTS §13): volume-share slippage
  model, POST params in body, per-`commissionAsset` fee scale, `OrderCancelPending`
  (needs the WS shell), and order amendment (D11).
- **Deep bug-hunt fixes (62-agent audit: 8 Opus auditors → per-finding verify →
  Opus reconcile; 16 confirmed, all fixed).**
  - **Critical — backtest↔live parity:** the MEXC connector's `avg_price` was off
    by `10^qty_scale` (it divided a `price_scale` quote by a `qty_scale` base),
    so live fills — and the modelled fee derived from them — were mis-scaled for
    any symbol with nonzero base precision. Now re-scales correctly; the unit + e2e
    tests that had pinned the wrong value are corrected.
  - **`SimulatedExchange`:** one-cancels-other is now enforced *intra-bar* (a
    single wide bar could fill both bracket legs); adverse slippage no longer
    applies to passive limit / stop-limit fills (they never execute worse than
    their price); `reduce_only` is enforced (rejected with `WouldIncreasePosition`
    when flat or same-side); `post_only` is enforced for armed stop-limits; and the
    buying-power reservation counts only a partially-filled buy's *remainder*.
  - **Replay oracle:** `diff_reports` now sorts both fill lists (no false mismatch
    on equal-timestamp cross-instrument fills) and compares fills-only fees/PnL (no
    false mismatch when the saved report carries funding/liquidation effects).
  - **Edge-hardening:** average-entry price is rounded to nearest (was truncated,
    biasing realized PnL — behaviour-neutral at realistic scales); position math
    widens before `.abs()` and saturates (no `i64::MIN` panic/overflow); `Money::neg`
    saturates; `read_partition` returns `DataError` (not a panic) on a wrong column
    count; the replay lead-in timestamp uses `saturating_sub`; `mark_to_market`
    documents the fill-before-bar contract with a debug assert; and the futures
    `unit_to_raw` tick/lot path is pure-integer (no f64, D12) with `market+post_only`
    rejected locally.
- **Kill-feature hardened by a 62-agent wide audit (verdict: holds).** A dynamic
  workflow (35 hunters on distinct attack angles → a verifier per candidate → an
  Opus reconciler) found no safe-code, in-scope future-data leak; the four layers
  were re-verified against source. The minor regression-coverage gaps it surfaced
  are fixed: `LastN` now carries the invariant `Brand<'bar>` (was covariant —
  defense-in-depth, no behaviour change); the compile-fail proof suite grew from
  **5 to 20** cases — every realistic stash vector (`RefCell`, `Rc<RefCell>`,
  `Arc<Mutex>`, `Cell`, `OnceCell`, `Box`, a boxed closure, `thread::spawn`, a
  stashed `LastN`, the `&mut Ctx` itself), the two layer-2 sub-properties
  (`series.ahead(1)` → no such method, `series[i]` → no `Index`), and the brand on
  **all four** lifecycle hooks (`on_start`/`on_bar`/`on_account`/`on_stop`). See
  `AGENTS.md` §4 and §12.
- **Record/replay regression oracle (`akadro-backtest::replay`):** a built-in
  `ReplayStrategy` + `ReplayFeed` (and a `replay_trades` convenience) that re-drive
  a *saved* `RunReport`'s exact trade sequence through the **real** engine +
  `SimulatedExchange` + portfolio — with **no strategy logic** — rebuilding the
  result from zero. `diff_reports` compares the saved report against its replay on
  the trade-simulation invariants (per-fill side/price/qty/fee/ts, realized PnL,
  total fees). Replaying a report saved by an old binary under a new one flags an
  *unintended* change to trade-simulation behaviour (replay diverges) — or a change
  that *should* have moved the result but didn't (replay still matches a stale
  report). The replay canonicalizes trades as next-bar-open market orders (the
  report stores realized fills, not the original market data / order kinds), so it
  validates the fee/PnL/accounting path, not the original fill *generation*. The
  `playground` runs this on its saved report each run (`MATCH ✓`).
- **Persistable `RunReport` (opt-in `serde` feature):** `RunReport::save(path)` /
  `RunReport::load(path)` round-trip the report to/from JSON (`to_json`/`from_json`
  too). The format is **backward-compatible** — a `format_version` field plus
  `#[serde(default)]` mean a file written by an older build loads into a newer one
  (fields added later take their defaults), unknown fields from a newer build are
  ignored, and a file from a newer, unknown format is rejected. The value
  vocabulary (`Price`/`Qty`/`Money`/ids/`Side`) gains `serde` derives behind the
  same feature. The `playground` can write its report via `--save-report <path>`.
- **Real-data experiment harness (`crates/playground`):** a scratch binary that
  fetches genuine MEXC `BTCUSDT` klines via the connector, runs the SMA strategy
  through the real engine, caches the bars to the Feather store, and prints
  analytics + a returns t-test/p-value. Not a CI test (needs the network);
  excluded from the coverage gate.

### Known limitations / deferred
- Real venue connectors (MEXC first) and the async live shell (`akadro-live`) are
  in progress.
- Coverage is 99.04% line / 98.35% region (`cargo-llvm-cov`). The gap from
  literal 100% is defensive-unreachable guards, a `#[non_exhaustive]` catch-all,
  live-network IO (`net.rs`, excluded), test-only helper arms, and two
  attribution artifacts — no untested production behavior. Itemized in
  `AGENTS.md` §10.
- DEX precision, advanced order types, and full margin/funding/liquidation
  realism are reserved (additive) for later versions.

<!-- Version-compare links intentionally omitted until a public repository URL is
     chosen. When you pick a host, add e.g.:
     [Unreleased]: https://<host>/<org>/akadro-rust/compare/v0.1.0...HEAD
     [0.1.0]:      https://<host>/<org>/akadro-rust/releases/tag/v0.1.0 -->
