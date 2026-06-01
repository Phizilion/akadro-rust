// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # akadro-engine
//!
//! The akadro event loop and — its defining feature — the **compile-time
//! look-ahead guarantee**. This crate defines [`Ctx`], [`Series`] and the
//! [`Strategy`] trait, co-located with the [`Engine`] loop, and a strategy can
//! only ever see the present and the past.
//!
//! ## The kill feature: four layers
//!
//! Reading future market data during a backtest is a *compile error*, not a
//! runtime check. Four independent layers enforce this (all verified on
//! rustc 1.95 during the design review; see `AGENTS.md` for the full write-up):
//!
//! 1. **Structural** — a strategy never receives the dataset. The engine owns
//!    future events behind this crate's boundary and pushes one at a time. There
//!    is no field or method on [`Ctx`] that reaches un-arrived data.
//! 2. **Backward-only series** — [`Series`] exposes `latest()`, `ago(n)` and
//!    `last_n(n)` and nothing else. "Read `n` bars ahead" is not expressible, so
//!    a forward read is a missing-method compile error, not a runtime panic.
//! 3. **Invariant brand** — [`Ctx`] and [`Series`] carry an invariant per-call
//!    lifetime `'bar`. Stashing one across calls (in `self`, a `Vec`, a
//!    `RefCell`, an `Rc`, a closure, …) fails to compile because the per-call
//!    `'bar` cannot be unified with any longer-lived lifetime.
//! 4. **Sealed construction** — [`Ctx`] has only private fields and a
//!    `pub(crate)` constructor, co-located with the loop. No other crate can
//!    forge a context over its own (future) data.
//!
//! ### Honest scope
//!
//! These layers stop a *safe* strategy from reading the future *through the
//! `Ctx` API* — the realistic bug class. They do **not** stop a determined
//! author who writes `unsafe` (e.g. `transmute`) or loads their own copy of the
//! data out-of-band. See the threat model in `AGENTS.md`.
//!
//! ## Parity
//!
//! The same [`Engine`] loop drives backtest and live; only the injected
//! [`DataSource`](akadro_core::DataSource) and
//! [`ExecutionClient`](akadro_core::ExecutionClient) differ. A strategy cannot
//! observe which mode it is in, and with fixed-point money math the resulting
//! [`RunReport`] is bit-for-bit reproducible.

mod brand;
mod context;
mod engine;
mod market;
mod observer;
mod portfolio;
mod rng;
mod series;
mod strategy;

pub use context::Ctx;
pub use engine::{Engine, EquityPoint, REPORT_FORMAT_VERSION, RunReport};
pub use observer::{NoOpObserver, Observer};
pub use portfolio::FillRecord;
pub use rng::DeterministicRng;
pub use series::{LastN, Series};
pub use strategy::Strategy;

/// Common imports for writing strategies and running engines:
/// `use akadro_engine::prelude::*;`.
pub mod prelude {
    pub use crate::{Ctx, Engine, FillRecord, RunReport, Series, Strategy};
    pub use akadro_core::prelude::*;
}
