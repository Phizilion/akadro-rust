# Installing and using akadro

akadro is a normal Cargo **library** (a workspace of crates). You consume it the
way you consume any Rust dependency: add it to your project's `Cargo.toml` and
Cargo builds it into your binary. There is nothing to "install" system-wide.

> **TL;DR**
> - Depend on the **`akadro`** umbrella crate and turn on the features you need.
> - **Now (internal):** use a [path](#a-path-dependency-same-machine) or
>   [private-git](#b-private-git-dependency-recommended-for-internal-sharing)
>   dependency.
> - **Later (published):** `cargo add akadro` from crates.io or a private
>   registry — *the consuming code does not change*, only where Cargo fetches it.
> - **"Binary + docs, no source"** is **not** how Rust libraries work — see
>   [§4](#4-can-i-ship-a-compiled-binary--docs-without-source). The production way
>   to keep source private is a private registry / git, not a binary blob.

Requirements for any consumer: **rustc ≥ 1.95** (MSRV), **edition 2024**.

---

## 1. What you depend on

| Crate | Role | How you get it |
|---|---|---|
| **`akadro`** | Umbrella — re-exports everything under tidy paths + a `prelude`. **This is what you depend on.** | `akadro = { … }` |
| `akadro-venue-mexc` | MEXC venue connector (spot, REST). **Separate dep** — not re-exported by the umbrella. | add alongside `akadro` |
| `akadro-venue-dex` | AMM/DEX connector. Separate dep. | add alongside `akadro` |

The umbrella's features (enable only what you use — defaults are minimal):

| Feature | Pulls in | Default? |
|---|---|---|
| `backtest` | `SimulatedExchange` + `HistoricalFeed` | ✅ |
| `analytics` | Sharpe/Sortino/drawdown/trade stats | — |
| `indicators` | incremental SMA/EMA/RSI/… | — |
| `data` | columnar Arrow/Feather bar cache | — |
| `live` | reconnecting feed + paper trading | — |
| `testkit` | mock-live feed for parity testing | — |
| `serde` | persist `RunReport` to/from JSON | — |

`akadro-venue-mexc` has its own `net` feature (adds the live `reqwest` transport;
without it the connector is transport-agnostic and test-mockable).

---

## 2. Use it now in another project (internal, no publishing)

### A. Path dependency (same machine)

The fastest way to develop against akadro locally. Point at the **umbrella
crate's directory** inside this workspace:

```toml
# <your-project>/Cargo.toml
[dependencies]
akadro = { path = "../akadro-rust/crates/akadro", features = ["analytics", "data"] }
# Optional: the MEXC connector (live network behind its own `net` feature)
akadro-venue-mexc = { path = "../akadro-rust/crates/akadro-venue-mexc", features = ["net"] }
```

- **Pro:** zero setup, instant edit/rebuild across both projects.
- **Con:** the consumer needs akadro's source tree on disk at that path; not
  portable to another machine or a teammate without the same layout.

### B. Private git dependency (recommended for internal sharing)

This is the **production-ready way to share akadro across projects, machines, and
teammates while keeping the source private** — only people/CI with read access to
the repo can build it. Cargo fetches and compiles it like any dependency.

```toml
[dependencies]
akadro = { git = "ssh://git@<your-host>/<org>/akadro-rust.git", tag = "v0.1.2", \
           features = ["analytics", "data"] }
akadro-venue-mexc = { git = "ssh://git@<your-host>/<org>/akadro-rust.git", tag = "v0.1.2", \
                      features = ["net"] }
```

- **Always pin** with `tag = "v0.1.2"` (or `rev = "<sha>"`) for reproducible
  builds — a bare `git = …` floats on the default branch.
- CI needs an SSH deploy key / token with read access to the repo.
- When you later publish to a registry, consumers swap the `git = …` line for a
  plain `version = …` — no other code changes.

### C. Vendoring (air-gapped / fully self-contained builds)

`cargo vendor` copies all sources (akadro + its transitive deps) into a local
directory you commit, so the consumer builds with **no network**:

```sh
cd <your-project>
cargo vendor vendor/ >> .cargo/config.toml   # prints the [source] replacement to paste
cargo build --offline
```

Use this for reproducible/offline CI or when you want the dependency frozen in
your repo.

---

## 3. Publish it later (public crates.io *or* a private registry)

When you decide where akadro lives, publishing does **not** change how consumers
write their code — they go from `git = …` / `path = …` to a plain version:

```toml
[dependencies]
akadro = "0.1"
```

What you'll do to publish (either registry):

1. **Re-add metadata** the registry requires. You removed `repository`/`homepage`
   (no host chosen yet); crates.io requires `license` (present: `MPL-2.0`) and
   strongly recommends `repository`. Add them back to `[workspace.package]` first.
2. **Publish in dependency order** — each workspace crate is published
   separately, leaves first: `akadro-core` → `akadro-engine` → `akadro-backtest`
   / `akadro-analytics` / `akadro-indicators` / `akadro-data` / `akadro-live` /
   `akadro-testkit` → venue crates → **`akadro`** last. (`akadro-compile-tests`,
   `playground`, `sma-crossover` are internal — set `publish = false` on them.)
3. The command:
   - **crates.io (public):** `cargo publish -p <crate>` for each, in order.
   - **Private registry** (Kellnr self-hosted, Cloudsmith, JFrog Artifactory, …):
     configure it once under `[registries]` in `~/.cargo/config.toml`, then
     `cargo publish -p <crate> --registry <name>`. Consumers add
     `akadro = { version = "0.1", registry = "<name>" }`.

A private registry is the cleanest "internal now, public later" path: identical
publish/consume ergonomics to crates.io, but access-controlled.

---

## 4. "Can I ship a compiled binary + docs, without source?"

**Short answer: no — not for a library like this, and not in a production-ready
way. That is a fundamental Rust limitation, not an akadro choice.**

Rust has **no stable ABI** and **no first-class precompiled-library distribution**.
Cargo always compiles dependencies **from source**. This is unlike the models you
may be picturing:

| Ecosystem | Ship binary + interface | Rust equivalent |
|---|---|---|
| C / C++ | `.so`/`.a` + headers | ❌ no supported equivalent for a rich API |
| Java | `.jar` | ❌ |
| .NET | `.dll` | ❌ |
| **Rust** | — | **source, compiled by the consumer** |

Why each Rust artifact does **not** give you "binary, no source":

- **`.rlib`** (Rust static lib) — (1) **compiler-version-locked**: an `.rlib`
  built by rustc 1.95.0 only links into a project *also* built by 1.95.0; bump the
  toolchain and it's dead. (2) It **does not hide the source** that matters:
  generic and `#[inline]` functions have their **MIR embedded** so the consumer's
  compiler can monomorphize/inline them. akadro's whole API is generic- and
  lifetime-heavy (the compile-time look-ahead guard *requires* the consumer's
  compiler to resolve lifetimes/generics), so the logic travels *with* the rlib.
  (3) Cargo has **no supported syntax** to depend on a prebuilt `.rlib` — it's a
  manual `rustc --extern` hack, not a dependency.
- **`cdylib`** (`.so`/`.dll` with a **C ABI**) — only exposes `extern "C"`
  functions. You would have to hand-write a flattened C API and **throw away the
  entire type-safe Rust surface**: no `Strategy` trait, no `Ctx`/`Series`, and
  **no compile-time look-ahead protection** (that guarantee lives in the Rust type
  system, which a C ABI erases). This defeats the point of akadro.
- **`dylib`** (Rust-ABI dynamic lib) — same toolchain-lock as `.rlib`; intended
  for use *within* a single Rust build (e.g. the compiler itself), not for
  distribution.

**So the realistic way to achieve your actual goal — "use akadro in other projects
without making its source public" — is access control, not a binary blob:**

> Distribute via a **private git dependency** ([§2B](#b-private-git-dependency-recommended-for-internal-sharing))
> or a **private registry** ([§3](#3-publish-it-later-public-cratesio-or-a-private-registry)).
> The source is compiled by the *authorized* consumer but is **never published
> publicly**. This is exactly how companies ship internal Rust crates.

If you ever genuinely must withhold source from even authorized consumers, the
*only* option is the `cdylib` + redesigned flat C API above — a separate, large
effort that sacrifices the kill feature. Not recommended.

---

## 5. API docs without the implementation source

You **can** ship browsable API documentation (signatures + your doc comments)
without the implementation bodies. rustdoc embeds a source browser by default;
delete it after building.

```sh
# Build docs for the public umbrella API, all features, no dependency docs.
cargo doc --no-deps -p akadro --all-features

# Strip the implementation-source browser (rustdoc puts it under target/doc/src).
rm -rf target/doc/src

# Result: target/doc/akadro/index.html — signatures + doc comments, no source bodies.
# Serve target/doc/ as static HTML internally, or zip it for distribution.
```

- Entry point: `target/doc/akadro/index.html`.
- Removing `target/doc/src/` drops the syntax-highlighted source browser and the
  `[source]` links; the API reference itself is unaffected (verified).
- **Caveat:** public **type signatures and doc comments are the API** — they
  cannot be hidden and still have usable docs. The strip only removes
  implementation *bodies*, not declarations.

---

## 6. Smoke-test a consumer build

A minimal strategy, to confirm the dependency resolves and the prelude works:

```rust
use akadro::prelude::*;

struct Flat;
impl Strategy for Flat {
    // Reading future bars here is a *compile error* — the look-ahead kill feature.
    fn on_bar(&mut self, _bar: Bar, _ctx: &mut Ctx<'_>) {}
}

fn main() {
    let spec = InstrumentSpec::new(
        InstrumentId::new(0), AssetId::new(0), AssetId::new(1),
        InstrumentKind::Spot, Price::from_raw(1), Qty::from_raw(1),
        Money::ZERO, CapSet::empty(),
    );
    let report = Engine::new(
        &[spec], Money::ZERO, HistoricalFeed::from_bars(Vec::new()),
        SimulatedExchange::new(Vec::new(), 0), Flat,
    ).unwrap().run();
    println!("bars processed: {}", report.bars_processed);
}
```

```sh
cargo run    # prints: bars processed: 0
```

---

## 7. Production-readiness notes

- **MSRV / edition:** consumers need **rustc ≥ 1.95**, edition 2024. State this in
  your project's CI matrix.
- **Safety:** the library is `#![forbid(unsafe_code)]` workspace-wide.
- **Semver (important at 0.x):** while akadro is `0.y`, Cargo treats a **minor**
  bump (`0.1` → `0.2`) as **allowed to break**. For internal stability, pin
  precisely — `akadro = "=0.1.2"`, or a git `tag`/`rev` — and adopt new minors
  deliberately. The public API is additively future-proofed (`#[non_exhaustive]`
  enums/structs), but 0.x makes no compatibility *promise* yet.
- **Determinism & parity:** the same strategy source runs bit-identically in
  backtest and live — see `AGENTS.md` §5. Keep a fresh engine per run.
- **Verify before relying on it:** `cargo test --workspace`, `cargo clippy
  --workspace --all-targets`, `cargo doc -D warnings` should all be green
  (`AGENTS.md` §10).
