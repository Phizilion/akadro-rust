// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The exchange-extension traits — the *entire* surface a new venue implements.
//!
//! This is the heart of the exchange-agnostic goal (goal 1): to support a new
//! venue you implement [`DataSource`], [`ExecutionClient`] and
//! [`InstrumentCatalog`] in a new crate that depends only on `akadro-core`. No
//! change to the engine, the strategy API, or any other venue is required.
//!
//! These traits are deliberately small and venue-neutral. Anything a venue does
//! that is *not* placing an order (leverage, margin mode, approvals) flows
//! through [`crate::VenueCommand`], not through bespoke trait methods.

use crate::event::{AccountEvent, Event};
use crate::ids::{ClientOrderId, InstrumentId, Timestamp};
use crate::instrument::InstrumentSpec;
use crate::order::{OrderRequest, VenueCommand};

/// A sink the engine provides for execution clients to emit [`AccountEvent`]s.
///
/// Abstracting the sink (rather than handing out a `Vec`) lets the engine pick
/// its own backing buffer without that choice leaking into the public API
/// (decision D13). You *call* this from an [`ExecutionClient`]; you do not
/// normally implement it (the engine does).
pub trait EventSink {
    /// Emit one account event into the engine's stream, in occurrence order.
    fn emit(&mut self, event: AccountEvent);
}

/// A normalized, time-ordered source of market [`Event`]s.
///
/// The same trait serves backtest (replaying stored data) and live (a venue
/// socket normalized to `Event`s). The engine only ever pulls one event at a
/// time and *owns* nothing beyond the current event — this structural boundary
/// is the first layer of look-ahead protection (the strategy can never reach
/// the underlying dataset because it never sees this type).
pub trait DataSource {
    /// Return the next event in non-decreasing timestamp order, or `None` once
    /// the stream is exhausted.
    fn next_event(&mut self) -> Option<Event>;
}

/// A venue that accepts orders and reports account changes.
///
/// The engine assigns each order a [`ClientOrderId`] deterministically and
/// passes it to [`ExecutionClient::submit`]. Acknowledgements, fills, and
/// cancels are reported by emitting [`AccountEvent`]s into the supplied
/// [`EventSink`] — there is no synchronous "order status" query, so backtest and
/// live are structurally identical (decision D5).
pub trait ExecutionClient {
    /// Submit a new order under the engine-assigned `id`.
    ///
    /// **Contract:** an implementation MUST emit *exactly one* terminal-or-accept
    /// outcome for `id` before returning — an [`AccountEvent::OrderAccepted`] if
    /// the venue took the order, or an [`AccountEvent::OrderRejected`] otherwise.
    /// This holds **even on a transport error**: if the order could not be placed
    /// (network failure, signing error, venue 4xx/5xx), emit `OrderRejected`
    /// (typically [`RejectReason::VenueRejected`]) rather than returning silently —
    /// otherwise `id` is left in limbo and a strategy waiting on the ack hangs
    /// forever. Subsequent fills/cancels for `id` arrive as later events (live: the
    /// venue stream; backtest: [`ExecutionClient::observe`]).
    ///
    /// [`AccountEvent::OrderAccepted`]: crate::AccountEvent::OrderAccepted
    /// [`AccountEvent::OrderRejected`]: crate::AccountEvent::OrderRejected
    /// [`RejectReason::VenueRejected`]: crate::RejectReason::VenueRejected
    fn submit(
        &mut self,
        id: ClientOrderId,
        order: OrderRequest,
        now: Timestamp,
        sink: &mut dyn EventSink,
    );

    /// React to new market data. A simulated venue matches resting/pending
    /// orders against `event` and emits fills; a live venue typically ignores
    /// this (its fills arrive over its own stream). Must not look ahead: only
    /// information in `event` and prior state may influence fills.
    fn observe(&mut self, event: &Event, now: Timestamp, sink: &mut dyn EventSink);

    /// Cancel a previously-submitted order.
    ///
    /// **Contract:** a venue that supports cancellation MUST emit *exactly one*
    /// outcome for `id` — an [`AccountEvent::OrderCanceled`] on success, or an
    /// [`AccountEvent::OrderCancelRejected`] if the order is unknown or already
    /// terminal — so a strategy polling for the cancel ack stops waiting instead of
    /// blocking forever. (A `Fill` may still race a cancel live: the venue can fill
    /// before the cancel lands.) The default is a no-op, intended only for venues
    /// that do not support cancellation at all; such a venue should advertise the
    /// absence via its [`InstrumentCatalog`] capabilities.
    ///
    /// [`AccountEvent::OrderCanceled`]: crate::AccountEvent::OrderCanceled
    /// [`AccountEvent::OrderCancelRejected`]: crate::AccountEvent::OrderCancelRejected
    fn cancel(&mut self, id: ClientOrderId, now: Timestamp, sink: &mut dyn EventSink) {
        let _ = (id, now, sink);
    }

    /// Apply a non-order venue command (leverage, margin mode, approvals —
    /// decision D9). The default is a no-op; venues that support these override it.
    fn command(&mut self, command: VenueCommand, now: Timestamp, sink: &mut dyn EventSink) {
        let _ = (command, now, sink);
    }

    /// The deterministic seed this client used, if any (e.g. a simulated venue's
    /// probabilistic-fill seed). Recorded in the run report so a run can be
    /// reproduced from the report alone. `None` for clients with no seeded model.
    fn config_seed(&self) -> Option<u64> {
        None
    }
}

/// Lookup of [`InstrumentSpec`] by id.
pub trait InstrumentCatalog {
    /// The spec for `id`, if known.
    fn spec(&self, id: InstrumentId) -> Option<&InstrumentSpec>;
}

/// A `Vec<InstrumentSpec>` indexed densely by [`InstrumentId`] is a catalogue.
impl InstrumentCatalog for Vec<InstrumentSpec> {
    fn spec(&self, id: InstrumentId) -> Option<&InstrumentSpec> {
        self.get(id.index() as usize)
    }
}

/// A `Vec<AccountEvent>` is the simplest possible [`EventSink`] (handy in tests
/// and as the engine's default drain buffer).
impl EventSink for Vec<AccountEvent> {
    fn emit(&mut self, event: AccountEvent) {
        self.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Bar;
    use crate::fixed::{Price, Qty};
    use crate::instrument::{CapSet, InstrumentKind};
    use crate::{AssetId, Money};

    #[test]
    fn vec_is_event_sink() {
        let mut sink: Vec<AccountEvent> = Vec::new();
        sink.emit(AccountEvent::OrderAccepted {
            id: ClientOrderId::new(1),
            ts: Timestamp::from_nanos(1),
        });
        assert_eq!(sink.len(), 1);
    }

    #[test]
    fn vec_is_instrument_catalog() {
        let specs = vec![InstrumentSpec::new(
            InstrumentId::new(0),
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(1),
            Qty::from_raw(1),
            Money::ZERO,
            CapSet::empty(),
        )];
        assert!(specs.spec(InstrumentId::new(0)).is_some());
        assert!(specs.spec(InstrumentId::new(1)).is_none());
    }

    #[test]
    fn execution_client_defaults_are_noops() {
        // A client implementing only the required methods inherits no-op cancel /
        // command and a `None` seed.
        struct Bare;
        impl ExecutionClient for Bare {
            fn submit(
                &mut self,
                _: ClientOrderId,
                _: OrderRequest,
                _: Timestamp,
                _: &mut dyn EventSink,
            ) {
            }
            fn observe(&mut self, _: &Event, _: Timestamp, _: &mut dyn EventSink) {}
        }
        let mut c = Bare;
        let mut events: Vec<AccountEvent> = Vec::new();
        // Drive the required methods too (a bare client's submit/observe are no-ops).
        c.submit(
            ClientOrderId::new(0),
            OrderRequest::market(InstrumentId::new(0), crate::Side::Buy, Qty::from_raw(1)),
            Timestamp::EPOCH,
            &mut events,
        );
        c.observe(
            &Event::Bar(Bar::new(
                InstrumentId::new(0),
                Timestamp::EPOCH,
                Price::from_raw(1),
                Price::from_raw(1),
                Price::from_raw(1),
                Price::from_raw(1),
                Qty::from_raw(1),
            )),
            Timestamp::EPOCH,
            &mut events,
        );
        c.cancel(ClientOrderId::new(0), Timestamp::EPOCH, &mut events);
        c.command(
            VenueCommand::ApproveAsset {
                asset: AssetId::new(0),
            },
            Timestamp::EPOCH,
            &mut events,
        );
        assert!(events.is_empty());
        assert_eq!(c.config_seed(), None);
    }

    // A tiny in-memory DataSource exercising the trait object-safely.
    struct VecSource(std::vec::IntoIter<Event>);
    impl DataSource for VecSource {
        fn next_event(&mut self) -> Option<Event> {
            self.0.next()
        }
    }

    #[test]
    fn datasource_drains_in_order() {
        let b = Bar::new(
            InstrumentId::new(0),
            Timestamp::from_nanos(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Qty::from_raw(1),
        );
        let mut src = VecSource(vec![Event::Bar(b)].into_iter());
        assert!(src.next_event().is_some());
        assert!(src.next_event().is_none());
    }
}
