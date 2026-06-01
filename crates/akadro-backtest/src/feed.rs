// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-memory historical data feed.
//!
//! The simplest [`DataSource`]: replay a pre-built list of events in order. The
//! engine pulls one at a time; the feed owns the (already-known) future, which
//! is never exposed to a strategy.

use akadro_core::{Bar, DataSource, Event};

/// Replays a fixed list of [`Event`]s in order.
#[derive(Debug)]
pub struct HistoricalFeed {
    events: std::vec::IntoIter<Event>,
}

impl HistoricalFeed {
    /// Build a feed from a list of events (assumed already time-ordered).
    #[must_use]
    pub fn new(events: Vec<Event>) -> Self {
        HistoricalFeed {
            events: events.into_iter(),
        }
    }

    /// Build a feed from a list of bars, wrapping each in [`Event::Bar`].
    #[must_use]
    pub fn from_bars(bars: Vec<Bar>) -> Self {
        HistoricalFeed::new(bars.into_iter().map(Event::Bar).collect())
    }
}

impl DataSource for HistoricalFeed {
    fn next_event(&mut self) -> Option<Event> {
        self.events.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{InstrumentId, Price, Qty, Timestamp};

    fn bar(ts: i64) -> Bar {
        Bar::new(
            InstrumentId::new(0),
            Timestamp::from_nanos(ts),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Qty::from_raw(1),
        )
    }

    #[test]
    fn replays_in_order() {
        let mut f = HistoricalFeed::from_bars(vec![bar(1), bar(2)]);
        assert_eq!(
            f.next_event().map(|e| e.ts()),
            Some(Timestamp::from_nanos(1))
        );
        assert_eq!(
            f.next_event().map(|e| e.ts()),
            Some(Timestamp::from_nanos(2))
        );
        assert!(f.next_event().is_none());
    }

    #[test]
    fn from_events_directly() {
        let mut f = HistoricalFeed::new(vec![
            Event::Bar(bar(5)),
            Event::Resync {
                instrument: None,
                ts: Timestamp::from_nanos(6),
            },
        ]);
        assert!(matches!(f.next_event(), Some(Event::Bar(_))));
        assert!(matches!(f.next_event(), Some(Event::Resync { .. })));
        assert!(f.next_event().is_none());
        assert!(format!("{f:?}").contains("HistoricalFeed"));
    }
}
