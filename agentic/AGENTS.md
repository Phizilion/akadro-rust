# akadro strategy lab — agent instructions

<!-- Template: copied verbatim into each scaffolded lab as AGENTS.md, with CLAUDE.md
     symlinked to it. The relative paths below (api/, ../scripts, ../.claude) resolve
     inside a lab. -->

You write and run trading strategies in this crate, built on the **akadro**
framework. Work to its **public API**, documented locally.

## Start here
**Read [`GUIDE.md`](GUIDE.md) first** — the mental model, the event/fill-timing
contract, how to write a `Strategy`, the fixed-point money model, the look-ahead
rules, getting data, running a backtest, analytics, and a troubleshooting table.
This file is just the rules + where things live.

## Where you write code
- **`src/strategies/`** — the ONLY place you edit. Add files here, declare them in
  `src/strategies/mod.rs` (`mod my_strategy;`), and implement `run()` to construct
  and run a backtest.
- Everything else is **locked / read-only** to you: `src/main.rs`, `Cargo.toml`,
  `rust-toolchain.toml`, `.cargo/`, `api/`, this file, `../.claude/`,
  `../scripts/` (enforced by `../.claude/settings.json` deny rules).

## Read these for the API
- **[`README.md`](README.md)** — what akadro is + a 15-line strategy (orientation).
- **[`GUIDE.md`](GUIDE.md)** — the how-to guide (concepts, recipes, cheatsheet).
- **`api/doc/akadro/index.html`** — full public API reference (human-browsable HTML).
- **`api/json/*.json`** — the same API as machine-readable rustdoc JSON, one file
  per crate (signatures + docs, no bodies). Best for programmatic lookup; see
  `api/json/README.md` for which crate defines what (the umbrella re-exports, so
  e.g. `Strategy`/`Engine` are in `akadro_engine.json`, `Price`/`Bar` in
  `akadro_core.json`).
- **`api/examples/`** — worked usage: `sma_crossover.rs`, `full_pipeline.rs`,
  `test_*.rs` (integration tests driving the public API), `legal_*.rs`
  (look-ahead-safe patterns that legitimately compile).

## Hard rules (enforced, not just requested)
- **No `unsafe`.** `#![forbid(unsafe_code)]` binds the whole crate including
  `src/strategies/`; `unsafe` (or `#[allow(unsafe_code)]`) is a compile error. A
  maintainer also runs `../scripts/verify-no-unsafe.sh`, which forces the lint even
  if the attribute were removed. Don't try — it can't pass.
- **No future data.** `Series` only goes backward (`latest()`, `ago(n)`,
  `last_n(n)`); a `Ctx`/`Series` can't be stored to peek the next bar. Reading
  ahead does not compile.
- **Data only through akadro — never load your own.** Obtain the engine's
  [`DataSource`] ONLY from the akadro data API:
  - `akadro::data::load_or_cache_feed(...)` — download-once / replay-from-cache as a
    ready feed (the usual path); `load_or_cache_many_feed(...)` for multi-instrument,
  - `akadro_venue_mexc::MexcKlineFeed` (and other `akadro_venue_*` feeds) — fetch
    klines through the connector, driven straight into the `Engine`.

  Do NOT write your own downloader (`curl`/`wget`/HTTP/a custom fetcher), do NOT read
  data files you obtained outside akadro, and do NOT hand-build `Bar`s. The raw
  `HistoricalFeed::from_bars`/`new` Vec-injection constructors are **not available**
  here (gated off). In a strategy read data ONLY via `ctx`/`Series` — never keep your
  own copy and index past `now` (out-of-band look-ahead, the one thing the type system
  can't catch). See `api/examples/full_pipeline.rs` for the sanctioned fetch→cache→run flow.
- **No custom `DataSource` — disallowed completely.** Implementing `DataSource` (or
  any other feed/exchange trait) to feed the engine is **forbidden in this lab**: it
  is the one remaining way to smuggle in your own data, and the leakage risk
  (look-ahead / survivorship bias the library can't validate) is exactly what we
  prevent. Get the feed only from the data API above. A maintainer runs
  `../scripts/verify-no-custom-datasource.sh`, which fails the build if `src/strategies/`
  contains an `impl DataSource` (or `ExecutionClient`/`InstrumentCatalog`). Don't — it
  won't pass review.
- **Don't read akadro's source** — its directory is denied by
  `../.claude/settings.json`, and you don't need it; everything is in `api/`.

## Walk-forward & analytics — use the safe, structural API
The "No future data" protection above covers a **single** `Engine::run`; walk-forward runs
*above* the engine. akadro gives you a **structurally look-ahead-safe** runner — use it and the
in-sample/out-of-sample separation is enforced for you:
- **Run walk-forward with `WalkForwardBacktest`** (`akadro::backtest`). Its `fit` step receives
  **only the train bars** — the test slice does not exist in its scope, so you *cannot* fit on
  out-of-sample data — and the framework runs the OOS engine itself with one shared `FillConfig`
  (no config drift). Build params from train in `fit`, build your strategy from those params in
  `make`. The low-level generic runner that hands you both slices is intentionally **not
  available** (behind the off-by-default `escape-hatch` feature) — it invites the walk-forward lie.
- **Aggregate with `WalkForwardSummary::from_reports(&[RunReport], ppy, is_total_return)`** — the
  only correct pooling: it pools per-fold OOS *returns* (never stitched equity levels) and counts
  blow-up folds instead of hiding them.
- **Annualize with `PerformanceReport::from_equity_auto`** (infers periods/year from the curve's
  timestamps) so you can't mis-pass `252` for hourly bars. `from_equity(curve, ppy)` still takes
  an explicit factor for a trading-day calendar.
- **Bars come only from the data API** (`load_or_cache_feed` / connector feeds) and are already
  time-ordered — you cannot (and must not) hand-build or import a `Vec<Bar>` (see the Hard rules).
- **Handle the honest `None`.** `from_equity`/`from_equity_auto` return `None` (not `NaN`) on a
  blown-up account or `< 2` points — don't `unwrap()` it into a panic or treat it as zero.
- The one thing no API can stop (out-of-band, the same ceiling as loading your own future data):
  running the whole study, reading the per-fold OOS reports, and re-picking the winner. Don't.
- Use `walk_forward_purged` (purge + embargo) windows when your signal's labels span time. See
  `GUIDE.md` (analytics section) and `api/examples/full_pipeline.rs`.

## To change a locked file (add a dependency, etc.)
Ask a human. Dependencies and the build config are intentionally outside your
control.

## Build / run
```sh
cargo run        # runs strategies::run() (prints "lab OK ...")
cargo test
cargo clippy --all-targets
```
