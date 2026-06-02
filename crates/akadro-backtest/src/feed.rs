// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-memory historical data feed.
//!
//! The simplest [`DataSource`]: replay a pre-built list of events in order. The
//! engine pulls one at a time; the feed owns the (already-known) future, which
//! is never exposed to a strategy.

use akadro_core::{Bar, DataSource, Event};
#[cfg(feature = "import-bars")]
use akadro_core::{BarOrderError, check_bars_ordered};

/// Replays a fixed list of [`Event`]s in order.
#[derive(Debug)]
pub struct HistoricalFeed {
    events: std::vec::IntoIter<Event>,
}

impl HistoricalFeed {
    /// Crate-private, always-available constructor. The public `new`/`from_bars`/
    /// `try_from_bars` (gated behind `import-bars`) delegate here, and internal
    /// callers that own their bars (`WalkForwardBacktest`, `replay`, tests) use it
    /// directly — so the crate builds with the gate off while no *user-facing* raw
    /// injection point exists by default.
    pub(crate) fn new_unchecked(events: Vec<Event>) -> Self {
        HistoricalFeed {
            events: events.into_iter(),
        }
    }

    /// Crate-private: wrap bars into an unchecked feed (see [`new_unchecked`]).
    ///
    /// [`new_unchecked`]: Self::new_unchecked
    pub(crate) fn from_bars_unchecked(bars: Vec<Bar>) -> Self {
        Self::new_unchecked(bars.into_iter().map(Event::Bar).collect())
    }

    /// Build a feed from a list of events (assumed already time-ordered).
    ///
    /// **Gated behind the off-by-default `import-bars` feature** (D18 — library owns
    /// the data). Raw event/bar injection lets a user feed arbitrary self-sourced
    /// data into the engine, which the library cannot validate for look-ahead /
    /// survivorship bias. The blessed path is `akadro_data::load_or_cache_feed`
    /// (cache → `DataSource`) or a venue connector feed; to supply your own data,
    /// implement [`DataSource`] deliberately rather than reach for this constructor.
    #[cfg(feature = "import-bars")]
    #[must_use]
    pub fn new(events: Vec<Event>) -> Self {
        Self::new_unchecked(events)
    }

    /// Build a feed from a list of bars, wrapping each in [`Event::Bar`].
    ///
    /// **Gated behind `import-bars`** (see [`new`](Self::new)). The bars are
    /// **assumed already time-ordered** — this does not validate them; use
    /// [`try_from_bars`](Self::try_from_bars) for the validated form, or the blessed
    /// `akadro_data::load_or_cache_feed` cache path.
    #[cfg(feature = "import-bars")]
    #[must_use]
    pub fn from_bars(bars: Vec<Bar>) -> Self {
        Self::from_bars_unchecked(bars)
    }

    /// Like [`from_bars`](Self::from_bars) but **validates** that `bars` is a
    /// well-ordered event stream first ([`check_bars_ordered`]) — fail-fast on the
    /// "garbage in" footgun (an unsorted / duplicate-timestamp `Vec<Bar>`, e.g. a
    /// hand-concatenated CSV) instead of producing a corrupt backtest. Also gated
    /// behind `import-bars`.
    ///
    /// # Errors
    /// [`BarOrderError`] if `bars` is not non-decreasing in time, or has a
    /// same-instrument duplicate timestamp (equal timestamps on *different*
    /// instruments are allowed). On error the feed is not built.
    #[cfg(feature = "import-bars")]
    pub fn try_from_bars(bars: Vec<Bar>) -> Result<Self, BarOrderError> {
        check_bars_ordered(&bars)?;
        Ok(Self::from_bars_unchecked(bars))
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

    fn bar_on(ts: i64, inst: u32) -> Bar {
        Bar::new(
            InstrumentId::new(inst),
            Timestamp::from_nanos(ts),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Qty::from_raw(1),
        )
    }
    fn bar(ts: i64) -> Bar {
        bar_on(ts, 0)
    }

    // Uses the always-available `*_unchecked` ctors so it runs with `import-bars`
    // OFF (default) — proving the internal construction seam is intact.
    #[test]
    fn replays_in_order() {
        let mut f = HistoricalFeed::from_bars_unchecked(vec![bar(1), bar(2)]);
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
        let mut f = HistoricalFeed::new_unchecked(vec![
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

    // The PUBLIC gated ctor + its validation only exist with `import-bars` on.
    #[cfg(feature = "import-bars")]
    #[test]
    fn try_from_bars_validates_ordering() {
        // The public gated wrappers delegate to the unchecked ctors (smoke).
        assert!(
            HistoricalFeed::from_bars(vec![bar(1)])
                .next_event()
                .is_some()
        );
        assert!(
            HistoricalFeed::new(vec![Event::Bar(bar(1))])
                .next_event()
                .is_some()
        );
        // Valid ascending -> Ok and behaves exactly like from_bars.
        let ok = HistoricalFeed::try_from_bars(vec![bar(1), bar(2), bar(3)]);
        assert!(ok.is_ok());
        // Descending -> rejected before the corrupt feed is built.
        assert!(matches!(
            HistoricalFeed::try_from_bars(vec![bar(3), bar(1)]),
            Err(BarOrderError::NotAscending { .. })
        ));
        // Same-instrument duplicate timestamp -> rejected.
        assert!(matches!(
            HistoricalFeed::try_from_bars(vec![bar(1), bar(1)]),
            Err(BarOrderError::DuplicateTimestamp { .. })
        ));
        // Equal timestamp on DIFFERENT instruments (interleaved multi-instrument
        // feed) is allowed — not an over-constraint.
        assert!(
            HistoricalFeed::try_from_bars(vec![bar_on(1, 0), bar_on(1, 1), bar_on(2, 0)]).is_ok()
        );
    }
}
