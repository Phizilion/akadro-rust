// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # akadro-testkit
//!
//! Utilities for testing strategies and for proving backtest↔live parity.
//!
//! [`MockLiveFeed`] delivers a fixed list of events through a **bounded channel
//! from a separate producer thread** — the same async-at-the-edge → sync-engine
//! shape a real live feed uses, minus the network. Driving the *same* strategy
//! and execution client from a [`MockLiveFeed`] and from an in-process
//! `akadro_backtest::HistoricalFeed` must produce a bit-identical
//! `akadro_engine::RunReport`; that differential test is the parity
//! golden-master.
//!
//! The channel is lossless: nothing is dropped or coalesced. (A real live bridge
//! additionally *aborts* on overflow rather than blocking — decision D7 — which
//! is not observable in a deterministic replay.)

use std::sync::mpsc::{Receiver, sync_channel};
use std::thread::JoinHandle;

use akadro_core::{DataSource, Event};

/// A [`DataSource`] that streams events from a background producer thread over a
/// bounded channel, mimicking a live feed's transport.
#[derive(Debug)]
pub struct MockLiveFeed {
    rx: Receiver<Event>,
    _producer: JoinHandle<()>,
}

impl MockLiveFeed {
    /// Spawn a producer that sends `events` over a channel of the given
    /// `capacity` (clamped to at least 1). Events are delivered in order; none
    /// are dropped.
    #[must_use]
    pub fn spawn(events: Vec<Event>, capacity: usize) -> Self {
        let (tx, rx) = sync_channel::<Event>(capacity.max(1));
        let producer = std::thread::spawn(move || {
            for event in events {
                // If the consumer has gone away, stop quietly.
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        MockLiveFeed {
            rx,
            _producer: producer,
        }
    }
}

impl DataSource for MockLiveFeed {
    fn next_event(&mut self) -> Option<Event> {
        // `recv` blocks until the next event or returns `Err` once the producer
        // has finished and the channel is drained.
        self.rx.recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{Bar, InstrumentId, Price, Qty, Timestamp};

    fn bar(ts: i64, close: i64) -> Bar {
        Bar::new(
            InstrumentId::new(0),
            Timestamp::from_nanos(ts),
            Price::from_raw(close),
            Price::from_raw(close),
            Price::from_raw(close),
            Price::from_raw(close),
            Qty::from_raw(1),
        )
    }

    #[test]
    fn delivers_all_events_in_order_small_capacity() {
        let events: Vec<Event> = (1..=20).map(|i| Event::Bar(bar(i, i))).collect();
        let mut feed = MockLiveFeed::spawn(events, 2); // tiny buffer forces backpressure
        let mut seen = Vec::new();
        while let Some(ev) = feed.next_event() {
            seen.push(ev.ts().as_nanos());
        }
        assert_eq!(seen, (1..=20).collect::<Vec<_>>());
    }

    #[test]
    fn empty_feed_yields_none() {
        let mut feed = MockLiveFeed::spawn(Vec::new(), 4);
        assert!(feed.next_event().is_none());
        assert!(format!("{feed:?}").contains("MockLiveFeed"));
    }

    // End-to-end: the same strategy + exchange driven by the mock-live feed
    // produces a sane report (full parity comparison lives in the umbrella crate).
    #[test]
    fn runs_a_strategy_through_the_mock_feed() {
        use akadro_backtest::SimulatedExchange;
        use akadro_core::{
            AssetId, CapSet, InstrumentKind, InstrumentSpec, Money, OrderRequest, Side,
        };
        use akadro_engine::{Ctx, Engine, Strategy};

        struct BuyFirst {
            done: bool,
        }
        impl Strategy for BuyFirst {
            fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
                if !self.done {
                    ctx.submit(OrderRequest::market(
                        InstrumentId::new(0),
                        Side::Buy,
                        Qty::from_raw(1),
                    ));
                    self.done = true;
                }
            }
        }

        let spec = InstrumentSpec::new(
            InstrumentId::new(0),
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(1),
            Qty::from_raw(1),
            Money::ZERO,
            CapSet::empty(),
        );
        let events: Vec<Event> = vec![Event::Bar(bar(1, 100)), Event::Bar(bar(2, 110))];
        let feed = MockLiveFeed::spawn(events, 1);
        let report = Engine::new(
            &[spec],
            Money::from_raw(1_000_000),
            feed,
            SimulatedExchange::new(vec![spec], 0),
            BuyFirst { done: false },
        )
        .unwrap()
        .run();
        assert_eq!(report.fills.len(), 1);
        assert_eq!(report.fills[0].price, Price::from_raw(110));
    }
}
