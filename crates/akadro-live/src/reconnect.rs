// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A [`DataSource`] that auto-reconnects and emits a resync marker on each gap.

use core::fmt;
use std::time::Duration;

use akadro_core::{DataSource, Event, Timestamp};

/// A boxed connection: any [`DataSource`] representing one live connection.
type Connection = Box<dyn DataSource>;

/// Wraps a connection *factory* and reconnects transparently.
///
/// Each time the current connection ends (`next_event` returns `None`), the
/// factory is asked for a new connection. If it yields one, a single
/// `Event::Resync` is emitted (so the strategy reconciles state after the gap,
/// decision D8) before events from the new connection flow. When the factory
/// returns `None`, the feed is truly finished.
///
/// In a real deployment the factory (re)opens a websocket and snapshots venue
/// state; in tests it can hand back a sequence of in-memory connections to
/// simulate disconnects deterministically.
pub struct ReconnectingFeed {
    factory: Box<dyn FnMut() -> Option<Connection>>,
    current: Option<Connection>,
    started: bool,
    last_ts: Timestamp,
    reconnect_delay: Option<Duration>,
    connect_failed: bool,
}

impl ReconnectingFeed {
    /// Build a reconnecting feed from a connection factory.
    ///
    /// A default **reconnect floor** of 500 ms is applied between connection
    /// attempts so a factory that reopens instantly cannot busy-loop and hammer
    /// the venue. The factory should still implement its own exponential backoff;
    /// override or disable the floor with [`Self::with_reconnect_delay`].
    #[must_use]
    pub fn new(factory: impl FnMut() -> Option<Connection> + 'static) -> Self {
        ReconnectingFeed {
            factory: Box::new(factory),
            current: None,
            started: false,
            last_ts: Timestamp::EPOCH,
            reconnect_delay: Some(Duration::from_millis(500)),
            connect_failed: false,
        }
    }

    /// Set the minimum delay slept between connection attempts (`None` disables
    /// it — use that in deterministic tests).
    #[must_use]
    pub fn with_reconnect_delay(mut self, delay: Option<Duration>) -> Self {
        self.reconnect_delay = delay;
        self
    }

    /// `true` if the feed finished because the factory never produced an **initial**
    /// connection — a hard connect failure, distinct from a clean end-of-stream after
    /// data flowed. Lets a live shell tell "the venue never came up" apart from "an
    /// empty feed", which both otherwise surface as `next_event` returning `None`.
    #[must_use]
    pub fn connect_failed(&self) -> bool {
        self.connect_failed
    }
}

impl fmt::Debug for ReconnectingFeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReconnectingFeed")
            .field("connected", &self.current.is_some())
            .field("started", &self.started)
            .field("connect_failed", &self.connect_failed)
            .finish_non_exhaustive()
    }
}

impl DataSource for ReconnectingFeed {
    fn next_event(&mut self) -> Option<Event> {
        loop {
            if self.current.is_none() {
                // On a genuine reconnect, sleep the floor first so an
                // instant-reopen factory cannot busy-loop the venue.
                if self.started
                    && let Some(d) = self.reconnect_delay
                {
                    std::thread::sleep(d);
                }
                self.current = (self.factory)();
                if self.current.is_none() {
                    // Factory yielded no connection. If we never connected at all,
                    // that's a hard connect failure (not a clean end-of-stream); flag
                    // it so the caller can distinguish the two (m26).
                    if !self.started {
                        self.connect_failed = true;
                    }
                    return None;
                }
                if self.started && self.last_ts != Timestamp::EPOCH {
                    // A genuine reconnect *after data flowed*: surface a resync before
                    // resuming. Note `ts` is the LAST pre-disconnect event time — a
                    // *lower bound* on the gap, not the reconnect instant. A connector
                    // reconciling fill history across the gap must re-fetch from
                    // `max(last_ts, now − lookback)`, never `last_ts` alone.
                    return Some(Event::Resync {
                        instrument: None,
                        ts: self.last_ts,
                    });
                }
                // A reconnect before any event ever arrived (last_ts still EPOCH) has
                // no gap to reconcile, so emitting an EPOCH-stamped resync would be
                // meaningless (and back-dated); suppress it and just resume (m45).
                self.started = true;
            }
            // `current` is Some here (just set above).
            let conn = self.current.as_mut()?;
            match conn.next_event() {
                Some(event) => {
                    self.last_ts = event.ts();
                    return Some(event);
                }
                None => self.current = None, // disconnect; loop to reconnect
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{Bar, InstrumentId, Price, Qty};

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

    struct VecConn(std::vec::IntoIter<Event>);
    impl DataSource for VecConn {
        fn next_event(&mut self) -> Option<Event> {
            self.0.next()
        }
    }

    #[test]
    fn reconnects_and_emits_resync_between_connections() {
        // Two connections, then exhausted.
        let conns: Vec<Vec<Event>> = vec![vec![bar(1), bar(2)], vec![bar(3)]];
        let mut it = conns.into_iter();
        let feed = ReconnectingFeed::new(move || {
            it.next()
                .map(|evs| Box::new(VecConn(evs.into_iter())) as Box<dyn DataSource>)
        })
        .with_reconnect_delay(None); // deterministic test: no real-time sleep

        let mut f = feed;
        let mut got = Vec::new();
        while let Some(e) = f.next_event() {
            got.push(e);
        }
        // bar1, bar2, [reconnect -> Resync], bar3, then done.
        assert_eq!(got.len(), 4);
        assert!(matches!(got[0], Event::Bar(_)));
        assert!(matches!(got[1], Event::Bar(_)));
        assert!(matches!(got[2], Event::Resync { .. }));
        // The resync carries the last seen timestamp.
        assert_eq!(got[2].ts(), Timestamp::from_nanos(2));
        assert!(matches!(got[3], Event::Bar(_)));
        assert_eq!(got[3].ts(), Timestamp::from_nanos(3));
    }

    #[test]
    fn no_resync_before_first_connection() {
        let mut once = Some(vec![bar(1)]);
        let mut f = ReconnectingFeed::new(move || {
            once.take()
                .map(|evs| Box::new(VecConn(evs.into_iter())) as Box<dyn DataSource>)
        })
        .with_reconnect_delay(None);
        // First event is the bar, not a spurious resync.
        assert!(matches!(f.next_event(), Some(Event::Bar(_))));
        assert!(f.next_event().is_none());
    }

    #[test]
    fn empty_factory_is_immediately_done() {
        let mut f = ReconnectingFeed::new(|| None);
        assert!(f.next_event().is_none());
    }

    #[test]
    fn no_resync_when_first_connection_yielded_no_events() {
        // m45: the first connection opens but disconnects before any event arrives
        // (last_ts still EPOCH), then a second connection brings data. There is no gap
        // to reconcile, so no (back-dated, EPOCH-stamped) resync should be emitted.
        let conns: Vec<Vec<Event>> = vec![vec![], vec![bar(3)]];
        let mut it = conns.into_iter();
        let mut f = ReconnectingFeed::new(move || {
            it.next()
                .map(|evs| Box::new(VecConn(evs.into_iter())) as Box<dyn DataSource>)
        })
        .with_reconnect_delay(None);
        let mut got = Vec::new();
        while let Some(e) = f.next_event() {
            got.push(e);
        }
        assert_eq!(got.len(), 1, "only the real bar, no spurious EPOCH resync");
        assert!(matches!(got[0], Event::Bar(_)));
        assert_eq!(got[0].ts(), Timestamp::from_nanos(3));
    }

    #[test]
    fn connect_failed_flags_initial_connection_failure() {
        // m26: factory never produces a connection -> hard connect failure.
        let mut f = ReconnectingFeed::new(|| None);
        assert!(f.next_event().is_none());
        assert!(f.connect_failed(), "never connected -> connect_failed");

        // A feed that connected and ended cleanly is NOT a connect failure.
        let mut once = Some(vec![bar(1)]);
        let mut g = ReconnectingFeed::new(move || {
            once.take()
                .map(|evs| Box::new(VecConn(evs.into_iter())) as Box<dyn DataSource>)
        })
        .with_reconnect_delay(None);
        while g.next_event().is_some() {}
        assert!(
            !g.connect_failed(),
            "clean end-of-stream is not a connect failure"
        );
    }
}

#[cfg(test)]
mod cov_tests {
    use super::*;
    #[test]
    fn debug_impl_renders() {
        let f = ReconnectingFeed::new(|| None);
        assert!(format!("{f:?}").contains("ReconnectingFeed"));
    }
}
