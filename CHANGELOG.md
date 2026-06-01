# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) from its first
release.

## [Unreleased]

## [0.1.1] - 2026-06-01

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
