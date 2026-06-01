// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # akadro-core
//!
//! Venue-neutral domain vocabulary and the small trait surface a new exchange
//! implements. This crate has **no I/O, no async, no exchange-specific code**,
//! and (by policy) no foreign types in its public API. It is the stable
//! foundation every other akadro crate and every venue connector builds on.
//!
//! What lives here:
//!
//! * [`Price`], [`Qty`], [`Money`] — fixed-point money math (no floats, ever).
//! * [`InstrumentId`], [`AssetId`], [`VenueId`], [`ClientOrderId`],
//!   [`Timestamp`] — opaque id/time newtypes.
//! * [`Event`], [`Bar`], [`AccountEvent`] — the market and account event
//!   vocabulary (identical in backtest and live).
//! * [`OrderRequest`], [`OrderKind`], [`VenueCommand`] — what a strategy submits.
//! * [`InstrumentSpec`], [`CapSet`] — instrument metadata and venue capabilities.
//! * [`DataSource`], [`ExecutionClient`], [`InstrumentCatalog`], [`EventSink`]
//!   — the exchange-extension traits.
//!
//! What does **not** live here: `Ctx` / `MarketView` / `Series` and the
//! `Strategy` trait. Those are defined in `akadro-engine`, co-located with the
//! event loop, because the compile-time look-ahead guarantee requires the
//! context's constructor to be unreachable from any other crate (see the
//! `AGENTS.md` "kill feature" section, decision D1).

mod error;
mod event;
mod fixed;
mod ids;
mod instrument;
mod order;
mod traits;

pub use error::{AkadroError, Result};
pub use event::{
    AccountEvent, Bar, CancelReason, CancelRejectReason, Cost, CostKind, Costs, Event,
    RejectReason, signal_channel,
};
pub use fixed::{FUNDING_RATE_SCALE, Money, Price, Qty};
pub use ids::{AssetId, ClientOrderId, InstrumentId, Timestamp, VenueId};
pub use instrument::{CapSet, Capability, InstrumentKind, InstrumentSpec, RoundingRule};
pub use order::{
    ClientCommandId, MarginMode, OrderAmend, OrderKind, OrderRequest, PlacedOrder, Side,
    TimeInForce, TrailKind, TriggerBy, VenueCommand,
};
pub use traits::{DataSource, EventSink, ExecutionClient, InstrumentCatalog};

/// Common imports for downstream crates: `use akadro_core::prelude::*;`.
pub mod prelude {
    pub use crate::{
        AccountEvent, AkadroError, AssetId, Bar, CancelReason, CancelRejectReason, CapSet,
        Capability, ClientOrderId, Cost, CostKind, DataSource, Event, EventSink, ExecutionClient,
        InstrumentCatalog, InstrumentId, InstrumentKind, InstrumentSpec, Money, OrderKind,
        OrderRequest, PlacedOrder, Price, Qty, RejectReason, Side, TimeInForce, Timestamp,
        TrailKind, TriggerBy, VenueCommand, VenueId,
    };
}
