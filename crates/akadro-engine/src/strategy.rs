// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `Strategy` trait — the code a user writes, unchanged for backtest and live.
//!
//! A strategy reacts to events through a [`Ctx`]. The method signatures use
//! *elided* lifetimes on `Ctx<'_>`, which makes each call's brand a fresh,
//! late-bound lifetime — so you never type `'bar` yourself, yet stashing the
//! context (or anything borrowed from it) across calls is a compile error. This
//! is the look-ahead kill feature in action; see `AGENTS.md`.
//!
//! ```
//! use akadro_engine::{Ctx, Strategy};
//! use akadro_core::{Bar, Side, OrderRequest, Qty, InstrumentId};
//!
//! struct BuyEveryBar { inst: InstrumentId }
//!
//! impl Strategy for BuyEveryBar {
//!     fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
//!         ctx.submit(OrderRequest::market(self.inst, Side::Buy, Qty::from_raw(1)));
//!     }
//! }
//! ```

use akadro_core::{AccountEvent, Bar};

use crate::context::Ctx;

/// A trading strategy: a stateful reactor over market and account events.
///
/// Only [`Strategy::on_bar`] is required; the lifecycle hooks default to no-ops.
/// New hooks added in future versions will also ship with default bodies, so
/// adding them is not a breaking change.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not an akadro `Strategy`",
    label = "implement `Strategy` (at minimum `on_bar`) for this type",
    note = "a `Strategy` reacts to events via a `Ctx`; it can read the present and past but never the future"
)]
pub trait Strategy {
    /// Called once before the first event.
    fn on_start(&mut self, _ctx: &mut Ctx<'_>) {}

    /// Called for each completed [`Bar`]. The bar is passed by value (it is
    /// `Copy`); you may keep a *past* bar, but you can never obtain a future one.
    fn on_bar(&mut self, bar: Bar, ctx: &mut Ctx<'_>);

    /// Called for each [`AccountEvent`] (acks, fills, cancels, resyncs) in
    /// occurrence order — the only way account changes are observed. The
    /// type-specific hooks below fire *before* this generic catch-all, so override
    /// whichever granularity you need (all default to no-ops).
    fn on_account(&mut self, _event: &AccountEvent, _ctx: &mut Ctx<'_>) {}

    /// Called for an [`AccountEvent::Fill`] (before [`Strategy::on_account`]).
    fn on_fill(&mut self, _event: &AccountEvent, _ctx: &mut Ctx<'_>) {}

    /// Called for an [`AccountEvent::OrderRejected`].
    fn on_order_rejected(&mut self, _event: &AccountEvent, _ctx: &mut Ctx<'_>) {}

    /// Called for an [`AccountEvent::OrderCanceled`].
    fn on_order_canceled(&mut self, _event: &AccountEvent, _ctx: &mut Ctx<'_>) {}

    /// Called for an [`AccountEvent::Liquidation`].
    fn on_liquidation(&mut self, _event: &AccountEvent, _ctx: &mut Ctx<'_>) {}

    /// Called for an [`AccountEvent::FundingSettlement`].
    fn on_funding(&mut self, _event: &AccountEvent, _ctx: &mut Ctx<'_>) {}

    /// Called when an event-time timer scheduled via
    /// [`Ctx::schedule`](crate::Ctx::schedule) comes due, with the timer's
    /// scheduled time. Fires before [`Strategy::on_bar`] for the bar that reached
    /// the time. Event-time only, so it is deterministic across backtest and live.
    fn on_timer(&mut self, _at: akadro_core::Timestamp, _ctx: &mut Ctx<'_>) {}

    /// Called once after the last event.
    fn on_stop(&mut self, _ctx: &mut Ctx<'_>) {}
}
