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
- **Data only through akadro — never download it yourself.** Obtain ALL market
  data via the akadro data API:
  - `akadro::data::load_or_cache(...)` — download-once / replay-from-cache,
  - `akadro_venue_mexc::MexcKlineFeed` — fetch klines through the connector,
  - `HistoricalFeed::from_bars(...)` — drive an existing `Vec<Bar>` from akadro.

  Do NOT write your own downloader (`curl`/`wget`/HTTP/a custom fetcher), do NOT
  read data files you obtained outside akadro, and do NOT hand-build `Bar`s from an
  outside source. Feed data through the `Engine`;
  in a strategy read it ONLY via `ctx`/`Series` — never keep your own copy and
  index past `now` (out-of-band look-ahead, the one thing the type system can't
  catch). See `api/examples/full_pipeline.rs` for the sanctioned fetch→cache→run flow.
- **Don't read akadro's source** — its directory is denied by
  `../.claude/settings.json`, and you don't need it; everything is in `api/`.

## To change a locked file (add a dependency, etc.)
Ask a human. Dependencies and the build config are intentionally outside your
control.

## Build / run
```sh
cargo run        # runs strategies::run() (prints "lab OK ...")
cargo test
cargo clippy --all-targets
```
