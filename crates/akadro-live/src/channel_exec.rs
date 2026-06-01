// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A live [`ExecutionClient`] whose **order submission** is synchronous (delegated
//! to an inner client — typically a venue's signed REST client) but whose **fills
//! and lifecycle events arrive asynchronously** on a channel, fed by a user-data
//! WebSocket task.
//!
//! This is the execution analogue of the [`BoundedBridge`](crate::BoundedBridge):
//! the market-data bridge carries `Event`s into the engine's `DataSource` slot,
//! while [`ChannelExec`] carries `AccountEvent`s into the engine's
//! `ExecutionClient` slot. The events are drained in [`ExecutionClient::observe`],
//! which the engine calls *before* each bar's `on_bar` — so a strategy sees the
//! fills that landed since the previous bar, exactly as a backtest sees resting
//! orders fill before the next bar (the parity event-ordering contract).
//!
//! Draining is **non-blocking** (`try_recv`): it forwards whatever has arrived and
//! returns, never stalling the synchronous engine on the network.

use std::sync::mpsc::Receiver;

use akadro_core::{
    AccountEvent, ClientOrderId, Event, EventSink, ExecutionClient, OrderRequest, Timestamp,
    VenueCommand,
};

/// Wraps an inner [`ExecutionClient`] (for submit/cancel/command) and a receiver
/// of [`AccountEvent`]s (fills/acks from a user-data stream). Build the channel
/// with [`std::sync::mpsc::channel`]; hand the `Sender` to the WebSocket task and
/// the `Receiver` here.
#[derive(Debug)]
pub struct ChannelExec<X> {
    inner: X,
    fills: Receiver<AccountEvent>,
}

impl<X> ChannelExec<X> {
    /// Pair an inner execution client with a stream of asynchronously-arriving
    /// account events.
    pub fn new(inner: X, fills: Receiver<AccountEvent>) -> Self {
        ChannelExec { inner, fills }
    }
}

impl<X: ExecutionClient> ExecutionClient for ChannelExec<X> {
    fn submit(
        &mut self,
        id: ClientOrderId,
        order: OrderRequest,
        now: Timestamp,
        sink: &mut dyn EventSink,
    ) {
        self.inner.submit(id, order, now, sink);
    }

    fn observe(&mut self, event: &Event, now: Timestamp, sink: &mut dyn EventSink) {
        // Let the inner client react first (a live REST client's observe is a
        // no-op; this also keeps a simulated inner client working in tests).
        self.inner.observe(event, now, sink);
        // Drain every account event that has arrived asynchronously, in order,
        // without blocking the synchronous engine.
        while let Ok(account_event) = self.fills.try_recv() {
            sink.emit(account_event);
        }
    }

    fn cancel(&mut self, id: ClientOrderId, now: Timestamp, sink: &mut dyn EventSink) {
        self.inner.cancel(id, now, sink);
    }

    fn command(&mut self, command: VenueCommand, now: Timestamp, sink: &mut dyn EventSink) {
        self.inner.command(command, now, sink);
    }

    fn config_seed(&self) -> Option<u64> {
        self.inner.config_seed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{Bar, InstrumentId, Price, Qty};
    use std::sync::mpsc::channel;

    /// An inner client that records whether submit/observe were delegated.
    #[derive(Default)]
    struct SpyExec {
        submits: u32,
        observes: u32,
    }
    impl ExecutionClient for SpyExec {
        fn submit(
            &mut self,
            _id: ClientOrderId,
            _order: OrderRequest,
            _now: Timestamp,
            _sink: &mut dyn EventSink,
        ) {
            self.submits += 1;
        }
        fn observe(&mut self, _event: &Event, _now: Timestamp, _sink: &mut dyn EventSink) {
            self.observes += 1;
        }
    }

    fn a_bar() -> Event {
        let p = Price::from_raw(1);
        Event::Bar(Bar::new(
            InstrumentId::new(0),
            Timestamp::from_nanos(1),
            p,
            p,
            p,
            p,
            Qty::from_raw(1),
        ))
    }

    fn an_ack(id: u64) -> AccountEvent {
        AccountEvent::OrderAccepted {
            id: ClientOrderId::new(id),
            ts: Timestamp::from_nanos(1),
        }
    }

    #[test]
    fn delegates_submit_cancel_command_and_config_seed() {
        let (_tx, rx) = channel::<AccountEvent>();
        let mut exec = ChannelExec::new(SpyExec::default(), rx);
        let mut sink: Vec<AccountEvent> = Vec::new();
        exec.submit(
            ClientOrderId::new(0),
            OrderRequest::market(
                InstrumentId::new(0),
                akadro_core::Side::Buy,
                Qty::from_raw(1),
            ),
            Timestamp::from_nanos(1),
            &mut sink,
        );
        exec.cancel(ClientOrderId::new(0), Timestamp::from_nanos(1), &mut sink);
        exec.command(
            VenueCommand::ApproveAsset {
                asset: akadro_core::AssetId::new(0),
            },
            Timestamp::from_nanos(1),
            &mut sink,
        );
        // The bare SpyExec inherits the trait's None seed, delegated through.
        assert_eq!(exec.config_seed(), None);
    }

    #[test]
    fn observe_drains_async_events_into_the_sink_in_order() {
        let (tx, rx) = channel();
        let mut exec = ChannelExec::new(SpyExec::default(), rx);
        tx.send(an_ack(1)).unwrap();
        tx.send(an_ack(2)).unwrap();

        let mut sink: Vec<AccountEvent> = Vec::new();
        exec.observe(&a_bar(), Timestamp::from_nanos(1), &mut sink);

        assert_eq!(sink.len(), 2, "both queued events drained");
        assert!(matches!(
            (&sink[0], &sink[1]),
            (
                AccountEvent::OrderAccepted { id: a, .. },
                AccountEvent::OrderAccepted { id: b, .. }
            ) if *a == ClientOrderId::new(1) && *b == ClientOrderId::new(2)
        ));
        assert_eq!(exec.inner.observes, 1, "inner observe delegated");
    }

    #[test]
    fn submit_delegates_and_observe_is_noop_when_empty() {
        let (_tx, rx) = channel();
        let mut exec = ChannelExec::new(SpyExec::default(), rx);
        let mut sink: Vec<AccountEvent> = Vec::new();
        exec.submit(
            ClientOrderId::new(0),
            OrderRequest::market(
                InstrumentId::new(0),
                akadro_core::Side::Buy,
                Qty::from_raw(1),
            ),
            Timestamp::from_nanos(1),
            &mut sink,
        );
        assert_eq!(exec.inner.submits, 1);
        exec.observe(&a_bar(), Timestamp::from_nanos(1), &mut sink);
        assert!(sink.is_empty(), "no async events → nothing emitted");
    }

    #[test]
    fn dropped_sender_does_not_break_observe() {
        let (tx, rx) = channel();
        tx.send(an_ack(5)).unwrap();
        drop(tx); // producer gone, but a buffered event remains
        let mut exec = ChannelExec::new(SpyExec::default(), rx);
        let mut sink: Vec<AccountEvent> = Vec::new();
        exec.observe(&a_bar(), Timestamp::from_nanos(1), &mut sink);
        assert_eq!(
            sink.len(),
            1,
            "buffered event still drained after sender dropped"
        );
        // A subsequent observe simply finds the channel empty+closed.
        exec.observe(&a_bar(), Timestamp::from_nanos(2), &mut sink);
        assert_eq!(sink.len(), 1);
    }
}
