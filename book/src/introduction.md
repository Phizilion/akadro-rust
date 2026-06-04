# akadro

**akadro** is an exchange-agnostic framework for backtesting and live trading in Rust.
You write a strategy **once** and run it unchanged in backtest and live, with two
defining guarantees:

1. **Compile-time look-ahead protection.** It is a *compile error* for safe strategy
   code to read future market data during a backtest. Not a runtime check, not a lint —
   the future is simply not expressible through the `Ctx` API.
2. **Backtest↔live parity.** The same strategy source produces a bit-identical
   `RunReport` whether it is driven by a historical feed or a live one.

The look-ahead guarantee is enforced with a *branded lifetime*: the `Ctx` your strategy
receives is stamped with a fresh, per-call lifetime, so you can read the present and the
past but you can never stash the context (or anything borrowed from it) to peek at a
later bar. That is great for correctness — but the first time you try to hold onto
something the compiler hands you, you will get a lifetime error that can look cryptic.

If that is why you are here, jump straight to **[I got a lifetime error](./lifetime-errors.md)**.

> This book is the human-facing companion to the engineering guide in
> [`AGENTS.md`](https://github.com/Phizilion/akadro-rust/blob/main/AGENTS.md) (`§4` covers
> the kill feature in full). The compile-fail proofs that back every claim here live in
> the `akadro-compile-tests` crate.
