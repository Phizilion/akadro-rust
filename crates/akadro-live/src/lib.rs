// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # akadro-live
//!
//! The live-trading shell around the (synchronous, deterministic) engine.
//!
//! * [`ReconnectingFeed`] wraps a connection *factory* and transparently
//!   reconnects when a connection ends, emitting an `Event::Resync` so the
//!   strategy's gap-handling path runs (decision D8). The same code path is
//!   exercised in backtest by replaying recorded resync events.
//! * [`run_paper`] runs **paper trading**: a live market feed driving the *same*
//!   engine and strategy, but with simulated execution
//!   ([`SimulatedExchange`](akadro_backtest::SimulatedExchange)) instead of real
//!   orders — live prices, fake fills.
//!
//! ## Notes on the live contract
//!
//! Event-time is already the engine's clock in both modes
//! ([`Ctx::now`](akadro_engine::Ctx::now) returns the current event's time, never
//! wall-clock), so no separate `LiveClock` type is needed for parity (decision
//! D4). The async→sync transport for a real venue is the bounded,
//! **lossless-or-fail** [`BoundedBridge`] (decision D7): it never silently drops
//! or coalesces events — on overflow the producer gets
//! [`AkadroError::LiveBackpressure`](akadro_core::AkadroError) and the consumer
//! aborts the session. A real connector spawns that producer (e.g. a websocket
//! task) and pushes events through a [`BridgeSender`].

mod bridge;
mod channel_exec;
mod paper;
mod reconnect;

#[cfg(feature = "binance")]
pub mod binance;
#[cfg(feature = "mexc")]
pub mod mexc;

pub use bridge::{BoundedBridge, BridgeSender, bounded_bridge};
pub use channel_exec::ChannelExec;
pub use paper::{run_paper, run_paper_with};
pub use reconnect::ReconnectingFeed;
