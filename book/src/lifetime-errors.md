# I got a lifetime error

You wrote a `Strategy`, the compiler rejected it, and the error mentions lifetimes,
`'bar`, "borrowed data escapes", or "cannot infer an appropriate lifetime". **This is
working as designed** — it is the look-ahead kill feature stopping you from holding on
to data you should not be able to keep. This page explains what happened and how to fix
it in one line.

> The examples below are illustrative. The *authoritative*, CI-enforced versions are the
> compile-fail proofs in `crates/akadro-compile-tests/tests/ui/*.rs` (must **not**
> compile) and the legitimate patterns in `tests/ui-pass/*.rs` (must compile). Each
> example notes the file that pins it.

## The one-sentence cause

Every strategy hook receives the context by `&mut Ctx<'_>`, where the elided `'_` is a
**fresh lifetime created for that one call** (and it is *invariant*, so it cannot be
quietly widened). Anything you borrow from the `Ctx` — a `Series`, a `LastN`, the `Ctx`
itself — lives only for that call. The moment you try to store it somewhere that
outlives the call (a field, a `Vec`, a `RefCell`, an `Rc`, a closure, another thread),
the borrow checker refuses, because keeping it would let you read it on a *later* bar:
that is exactly look-ahead.

You cannot keep the *window onto the data*. You can keep *copies of values you have
already seen*.

## The fix: copy values out, never stash the view

The rule of thumb: **read through `ctx` inside the hook, copy the scalars you need into
your own fields, and let the borrow end when the hook returns.** `Price`, `Qty`,
`Money`, `Timestamp`, `i64`, … are all `Copy`; a copy of the past is yours to keep.

### ✅ Good — remember the past by value

```rust,ignore
use akadro_engine::{Ctx, Strategy};
use akadro_core::{Bar, InstrumentId, Price};

struct Momentum { inst: InstrumentId, last_close: Price }

impl Strategy for Momentum {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        // Read through ctx, copy the SCALAR out. The borrow ends with the hook.
        if let Some(c) = ctx.closes(self.inst).and_then(|s| s.latest()) {
            self.last_close = c; // keeping a past `Price` (Copy) is fine
        }
    }
}
```

*(Pinned by `tests/ui-pass/scalar_copyout.rs` and `collect_past_copies.rs`.)*

### ❌ Bad — stashing the context (or a view) for later

```rust,ignore
use akadro_engine::{Ctx, Strategy};
use akadro_core::Bar;

struct Leaky<'a> { saved: Option<&'a mut Ctx<'a>> }

impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        self.saved = Some(ctx); // ERROR: the per-call lifetime can't escape into `self`
    }
}
```

The compiler says something like *"lifetime may not live long enough"* or *"borrowed
data escapes outside of method"* (`E0521`). It is telling you the truth: if `self`
could hold `ctx`, the *next* `on_bar` could read this bar's neighbour — the future.

*(Pinned by `tests/ui/stash_ctx_in_self.rs`. The same rejection covers a `Series` or
`LastN` stashed in a field, a `RefCell`, an `Rc<RefCell>`, an `Arc<Mutex>`, a `Cell`, a
`OnceCell`, a `Box`, a boxed closure, or moved into `thread::spawn` — see the matching
`stash_via_*.rs` cases.)*

## "Read ahead" is not even expressible

You will not find an accessor that returns the future, because none exists. `Series`
only ever looks **backward**:

```rust,ignore
let s = ctx.closes(inst).unwrap();
let now  = s.latest();  // Option<Price> — the most recent close
let prev = s.ago(1);    // Option<Price> — one bar ago (n: usize can't be negative)
let tail = s.last_n(3); // the last 3, oldest→newest

let _ = s.ahead(1);     // ERROR: no such method
let _ = s[0];           // ERROR: Series has no `Index` impl
```

There is no `ahead`, `peek`, `next_bar`, `forward`, no `Index`, and no way to borrow the
backing slice out. "Read `n` bars ahead" is a missing method or a `usize` that cannot be
negative — a *type* error, not a runtime panic.

*(Pinned by `tests/ui/series_no_forward_accessor.rs` and `series_no_index.rs`.)*

## Decoding the common messages

| The compiler said… | What it means | Fix |
|---|---|---|
| `lifetime may not live long enough` | You tried to make the per-call `'bar` outlive the call (usually by storing a borrow in `self`). | Copy the value out instead of storing the borrow. |
| `borrowed data escapes outside of method` (`E0521`) | A `&Ctx`/`&Series`/`&mut Ctx` is leaving the hook (returned, stored, captured). | End the borrow before returning; keep a `Copy` of what you need. |
| `` no method named `ahead`/`peek`/`forward` `` | You looked for a forward accessor. It does not exist by design. | Use `latest()` / `ago(n)` / `last_n(n)` (all backward). |
| `cannot index into a value of type Series` | `Series` has no `Index`. | Use `ago(n)`. |
| `` associated function `new` is private `` (`E0624`) on `Ctx::new` | You tried to construct a `Ctx` yourself. Only the engine can. | Don't — receive it as a hook argument. |

## Why it is built this way

A strategy never receives the dataset. The engine pulls one event at a time, appends it
to the observed history, and *then* calls your hook — so the `Series` you see physically
contains no future bar. The per-call invariant brand makes that structural fact a
*type-level* one: it is a compile error for safe strategy code to read future market
data through the `Ctx` API. See `AGENTS.md §4` for the full four-layer design and threat
model.

## What this does **not** catch (set your expectations)

The guarantee is precise: *no safe strategy code can read future market data through the
`Ctx` API*. It is **not** "physically impossible". It does not stop:

- **`unsafe` code** in your strategy (e.g. `mem::transmute` to launder a lifetime).
  Mitigation: the strategy-crate template sets `#![forbid(unsafe_code)]` (as
  `sma-crossover` does), and a `cargo-geiger` CI gate is recommended.
- **Out-of-band look-ahead**: loading your *own* copy of the data and indexing
  `data[t + 1]`, or stashing state in a `static`/`thread_local` across runs. No type
  system can prevent that; use a fresh engine per run and don't do it.

Inside those bounds, if it compiles, it does not peek.
