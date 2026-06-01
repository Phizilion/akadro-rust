// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The async→sync **lossless-or-fail** bridge (decision D7).
//!
//! A real venue's market/user-data arrives asynchronously (e.g. a websocket
//! task); the engine is synchronous. This bridge connects the two over a
//! *bounded* queue. Its defining property is that it never silently drops or
//! coalesces events — losing an event would put backtest and live out of sync.
//! On overflow it instead **fails loudly**: the producer is told via
//! [`AkadroError::LiveBackpressure`] and the consuming [`DataSource`] aborts the
//! session rather than continue past a gap.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use akadro_core::{AkadroError, DataSource, Event};

/// The producer handle: a real connector (e.g. a websocket task) pushes events
/// in through this. Cloneable for fan-in from multiple sources.
#[derive(Debug, Clone)]
pub struct BridgeSender {
    tx: SyncSender<Event>,
    capacity: usize,
    overflow: Arc<AtomicBool>,
}

impl BridgeSender {
    /// Push one event toward the engine. **Lossless-or-fail** (decision D7): if
    /// the bounded queue is full this neither blocks nor drops — it flags the
    /// session as overflowed and returns the error, so the consumer aborts rather
    /// than skipping events. A disconnected consumer reports `Ok` (the session is
    /// ending regardless).
    ///
    /// # Errors
    /// Returns [`AkadroError::LiveBackpressure`] when the queue is at capacity.
    pub fn send(&self, event: Event) -> Result<(), AkadroError> {
        match self.tx.try_send(event) {
            Err(TrySendError::Full(_)) => {
                self.overflow.store(true, Ordering::SeqCst);
                Err(AkadroError::LiveBackpressure {
                    capacity: self.capacity,
                })
            }
            // Delivered, or the consumer has hung up (the session is ending) —
            // both are fine; only a *full* queue is the lossless-or-fail failure.
            Ok(()) | Err(TrySendError::Disconnected(_)) => Ok(()),
        }
    }
}

/// The consuming side: a [`DataSource`] the engine pulls from. `next_event`
/// blocks for the next event and returns `None` once all producers are gone — or
/// **panics** (aborts the session) if the producer overflowed the queue.
#[derive(Debug)]
pub struct BoundedBridge {
    rx: Receiver<Event>,
    capacity: usize,
    overflow: Arc<AtomicBool>,
}

impl BoundedBridge {
    /// `true` if the producer ever overflowed the bounded queue.
    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.overflow.load(Ordering::SeqCst)
    }
}

impl DataSource for BoundedBridge {
    fn next_event(&mut self) -> Option<Event> {
        // Lossless-or-fail: abort the session if the producer overflowed, rather
        // than continue past events that were never delivered (decision D7).
        assert!(
            !self.overflow.load(Ordering::SeqCst),
            "live bridge overflowed its bounded queue (capacity {}): \
             AkadroError::LiveBackpressure",
            self.capacity
        );
        self.rx.recv().ok()
    }
}

/// Create a bounded lossless-or-fail bridge buffering up to `capacity` events
/// (clamped to at least 1). Returns the producer and consumer halves.
#[must_use]
pub fn bounded_bridge(capacity: usize) -> (BridgeSender, BoundedBridge) {
    let capacity = capacity.max(1);
    let (tx, rx) = sync_channel::<Event>(capacity);
    let overflow = Arc::new(AtomicBool::new(false));
    (
        BridgeSender {
            tx,
            capacity,
            overflow: Arc::clone(&overflow),
        },
        BoundedBridge {
            rx,
            capacity,
            overflow,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{Bar, InstrumentId, Price, Qty, Timestamp};

    fn bar(ts: i64) -> Event {
        let p = Price::from_raw(ts);
        Event::Bar(Bar::new(
            InstrumentId::new(0),
            Timestamp::from_nanos(ts),
            p,
            p,
            p,
            p,
            Qty::from_raw(1),
        ))
    }

    #[test]
    fn delivers_in_order_then_finishes() {
        let (tx, mut rx) = bounded_bridge(4);
        tx.send(bar(1)).unwrap();
        tx.send(bar(2)).unwrap();
        drop(tx); // disconnect: recv drains the buffer then returns None
        assert_eq!(rx.next_event().map(|e| e.ts().as_nanos()), Some(1));
        assert_eq!(rx.next_event().map(|e| e.ts().as_nanos()), Some(2));
        assert!(rx.next_event().is_none());
        assert!(!rx.overflowed());
    }

    #[test]
    fn overflow_returns_backpressure_and_is_lossless() {
        let (tx, rx) = bounded_bridge(1);
        assert!(tx.send(bar(1)).is_ok()); // buffered (1/1)
        let err = tx.send(bar(2)).unwrap_err(); // full -> backpressure, not dropped silently
        assert!(matches!(err, AkadroError::LiveBackpressure { capacity: 1 }));
        assert!(rx.overflowed());
    }

    #[test]
    #[should_panic(expected = "LiveBackpressure")]
    fn consumer_aborts_after_overflow() {
        let (tx, mut rx) = bounded_bridge(1);
        let _ = tx.send(bar(1));
        let _ = tx.send(bar(2)); // overflow
        let _ = rx.next_event(); // aborts the session
    }

    #[test]
    fn disconnected_send_is_ok() {
        let (tx, rx) = bounded_bridge(2);
        drop(rx);
        assert!(tx.send(bar(1)).is_ok()); // consumer gone -> not an error
    }
}
