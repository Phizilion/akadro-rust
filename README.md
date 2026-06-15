# akadro

**Exchange-agnostic backtesting + live trading for Rust — with a compile-time
guarantee against look-ahead, and strict backtest↔live parity.**

[![license](https://img.shields.io/badge/license-MPL--2.0-blue)](#license)
[![rust](https://img.shields.io/badge/rust-1.95%2B-orange)](#)

## Why akadro

- **You can't accidentally read the future.** During a backtest, reading
  tomorrow's price is a **compile error**, not a subtle bug that flatters your
  results. The market data API only goes backward in time, and the data you're
  handed each step cannot be stashed and peeked at later.
- **Backtest and live run the same code.** A strategy is written once. The engine
  loop is identical in both modes; only the data feed and the exchange connector
  differ, and the strategy can't tell which it's running under. With integer
  (fixed-point) money math, results are bit-for-bit reproducible.
- **Add any exchange without touching the core.** A new venue is a small,
  self-contained crate implementing three traits.

## A strategy in 15 lines

```rust
use akadro::prelude::*;

struct Smaish { inst: InstrumentId, last: Price }

impl Strategy for Smaish {
    fn on_bar(&mut self, bar: Bar, ctx: &mut Ctx<'_>) {
        // You can read the present and the past...
        if let Some(prev) = ctx.closes(self.inst).and_then(|s| s.ago(1)) {
            if bar.close > prev && ctx.net_qty(self.inst).raw() <= 0 {
                ctx.submit(OrderRequest::market(self.inst, Side::Buy, Qty::from_raw(1)));
            }
        }
        self.last = bar.close;
        // ...but `ctx.closes(self.inst)` can NOT be stored in `self` to peek
        // next bar — that would not compile. There is no "next bar" method at all.
    }
}
```

See [`crates/sma-crossover`](crates/sma-crossover) for the full worked example,
and run the backtest↔live parity proof with `cargo test -p akadro`.

## Documentation

- **[`GUIDE.md`](GUIDE.md)** — the strategy author's guide: mental model, the
  event/fill-timing contract, the `Strategy`/`Ctx`/`Series` API, fixed-point money,
  the look-ahead rules, analytics, and a troubleshooting table. For humans and agents.
- **[`INSTALL.md`](INSTALL.md)** — adding akadro to a project (path / private-git /
  registry) and generating API docs without source.
- **[`AGENTS.md`](AGENTS.md)** — the internal engineering design & decision record.
- **[`agentic/`](agentic/)** — scaffold a sandboxed *strategy lab* where an agent
  writes strategies against akadro's public API (HTML + JSON docs, examples, and
  `GUIDE.md` bundled) but **cannot read akadro's source** and **cannot use
  `unsafe`**. One command: `agentic/scaffold-lab.sh <target-dir>`.

## Installation

akadro is a Cargo library — depend on the **`akadro`** umbrella crate and enable
the features you need (requires **rustc ≥ 1.95**, edition 2024):

```toml
[dependencies]
# Internal release: a path dep for local dev, or a pinned private-git dep to
# share across projects without publishing the source. (Swap for `akadro = "0.1"`
# once published to a registry — no code changes.)
akadro = { path = "../akadro-rust/crates/akadro", features = ["analytics", "data"] }
```

Full guide — local/path, private-git, vendoring, publishing to a public or
private registry, generating API docs without source, and why Rust has no
"binary-only" library distribution — in **[`INSTALL.md`](INSTALL.md)**.

## Workspace layout

| crate | purpose |
|-------|---------|
| `akadro-core` | domain vocabulary + the exchange-extension traits (no I/O) |
| `akadro-engine` | the event loop + look-ahead-safe `Ctx`/`Series`/`Strategy` (the kill feature) |
| `akadro-backtest` | historical feed + conservative simulated exchange |
| `akadro-testkit` | mock-live feed + test helpers |
| `akadro-indicators` | incremental, integer-deterministic indicators (SMA, EMA, MACD, RSI, Bollinger, ATR, …) |
| `akadro-analytics` | performance/risk metrics (Sharpe, drawdown, trade stats) + walk-forward |
| `akadro-data` | columnar on-disk cache (Arrow/Feather) + incremental download planning |
| `akadro-venue-mexc` | MEXC connector — spot + futures, REST signing, WS protocol |
| `akadro-venue-okx` | OKX connector — spot + perpetual swap (REST), Base64-HMAC + passphrase |
| `akadro-venue-binance` | Binance connector — spot + USDⓈ-M futures (REST) + WS + the Vision bulk-history downloader |
| `akadro-venue-bybit` | Bybit connector — v5 unified spot + linear perpetual (REST) |
| `akadro-venue-kucoin` | KuCoin connector — spot (REST), v2 encrypted-passphrase signing |
| `akadro-venue-dex` | basic AMM/DEX (Uniswap-style) swap connector — validates the venue abstraction |
| `akadro-live` | live shell: auto-reconnect/resync feeds + paper trading |
| `akadro` | umbrella crate + prelude (feature-gated re-exports) |
| `sma-crossover` | example strategy |
| `akadro-compile-tests` | `trybuild` proofs that look-ahead does not compile |

## Building & testing

```sh
cargo test --workspace                  # unit + integration + doctests + trybuild proofs
cargo clippy --workspace --all-targets  # pedantic, must be clean
```

## License

Licensed under the [Mozilla Public License 2.0](LICENSE) (MPL-2.0).

MPL-2.0 is *weak, file-level copyleft*: you may use akadro in proprietary
software (including statically linked, with no relinking obligation), but
modifications to akadro's own source files must be published under the MPL.
Strategies and venue connectors written in your *own* files are unaffected.
