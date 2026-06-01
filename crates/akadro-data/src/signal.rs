// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Replaying a non-OHLCV **scalar signal** series as [`Event::Signal`]s — the
//! sanctioned way to inject open interest, long/short ratio, market liquidations,
//! sentiment, supply, NAV, etc. into a backtest. Reading these through a
//! [`DataSource`] (rather than a hand-rolled side channel) keeps determinism and
//! the look-ahead guarantee intact: the engine appends each value to backward-only
//! observed state before the strategy runs, and the strategy reads it via
//! `ctx.signal(instrument, channel)`.

use akadro_core::{DataSource, Event, InstrumentId, Timestamp};

use crate::bars::DataError;

/// A [`DataSource`] that replays a scalar series for one `(instrument, channel)`
/// as [`Event::Signal`]s, in time order. Values are fixed-point `i64` at a scale
/// the channel defines.
pub struct SignalSource {
    instrument: InstrumentId,
    channel: u16,
    points: std::vec::IntoIter<(Timestamp, i64)>,
}

impl SignalSource {
    /// Build from `(timestamp, value)` points. They are sorted by time here, so the
    /// caller need not pre-sort.
    #[must_use]
    pub fn new(instrument: InstrumentId, channel: u16, mut points: Vec<(Timestamp, i64)>) -> Self {
        points.sort_by_key(|(t, _)| t.as_nanos());
        SignalSource {
            instrument,
            channel,
            points: points.into_iter(),
        }
    }

    /// Parse a simple two-column `timestamp_ns,value` CSV — one point per line,
    /// blank lines skipped, and a single leading non-numeric header row tolerated.
    /// Both columns are integers (`timestamp_ns` epoch-nanoseconds, `value` the
    /// fixed-point scalar).
    ///
    /// # Errors
    /// [`DataError::Schema`] on a line that is neither blank, the header, nor a
    /// valid `int,int` pair.
    pub fn from_csv(instrument: InstrumentId, channel: u16, csv: &str) -> Result<Self, DataError> {
        let mut points: Vec<(Timestamp, i64)> = Vec::new();
        for line in csv.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split(',');
            let (Some(a), Some(b)) = (it.next(), it.next()) else {
                return Err(DataError::Schema(format!(
                    "signal csv: expected 'ts,value', got {line:?}"
                )));
            };
            match (a.trim().parse::<i64>(), b.trim().parse::<i64>()) {
                (Ok(ts), Ok(val)) => points.push((Timestamp::from_nanos(ts), val)),
                _ if points.is_empty() => {} // tolerate one leading header row
                _ => {
                    return Err(DataError::Schema(format!(
                        "signal csv: expected two integers, got {line:?}"
                    )));
                }
            }
        }
        Ok(Self::new(instrument, channel, points))
    }
}

impl DataSource for SignalSource {
    fn next_event(&mut self) -> Option<Event> {
        self.points.next().map(|(ts, value)| Event::Signal {
            instrument: self.instrument,
            channel: self.channel,
            value,
            ts,
        })
    }
}

impl core::fmt::Debug for SignalSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SignalSource")
            .field("instrument", &self.instrument)
            .field("channel", &self.channel)
            .field("remaining", &self.points.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(mut s: SignalSource) -> Vec<(i64, i64)> {
        let mut out = Vec::new();
        while let Some(Event::Signal { value, ts, .. }) = s.next_event() {
            out.push((ts.as_nanos(), value));
        }
        out
    }

    #[test]
    fn replays_in_time_order_with_metadata() {
        // Out of order on input — sorted on construction.
        let s = SignalSource::new(
            InstrumentId::new(2),
            7,
            vec![
                (Timestamp::from_nanos(30), 300),
                (Timestamp::from_nanos(10), 100),
                (Timestamp::from_nanos(20), 200),
            ],
        );
        // The Debug impl summarises without dumping every point.
        let dbg = format!("{s:?}");
        assert!(dbg.contains("SignalSource") && dbg.contains("remaining"));
        // Inspect the first event's instrument/channel.
        let mut s2 =
            SignalSource::new(InstrumentId::new(2), 7, vec![(Timestamp::from_nanos(1), 5)]);
        match s2.next_event() {
            Some(Event::Signal {
                instrument,
                channel,
                value,
                ts,
            }) => {
                assert_eq!(instrument, InstrumentId::new(2));
                assert_eq!(channel, 7);
                assert_eq!(value, 5);
                assert_eq!(ts.as_nanos(), 1);
            }
            other => panic!("expected Signal, got {other:?}"),
        }
        assert_eq!(drain(s), vec![(10, 100), (20, 200), (30, 300)]);
    }

    #[test]
    fn from_csv_parses_with_header_and_blank_lines() {
        let csv = "ts,oi\n\n100,5\n200,7\n  300 , 9 \n";
        let s = SignalSource::from_csv(InstrumentId::new(0), 1, csv).unwrap();
        assert_eq!(drain(s), vec![(100, 5), (200, 7), (300, 9)]);
        // Empty / header-only → empty source.
        assert!(
            drain(SignalSource::from_csv(InstrumentId::new(0), 1, "ts,oi\n").unwrap()).is_empty()
        );
    }

    #[test]
    fn from_csv_rejects_malformed() {
        // A bad line AFTER valid data is an error (not a header).
        assert!(SignalSource::from_csv(InstrumentId::new(0), 1, "100,5\nnope,bad").is_err());
        // A single-column line is an error.
        assert!(SignalSource::from_csv(InstrumentId::new(0), 1, "100,5\n200").is_err());
    }

    #[test]
    fn empty_source_is_none() {
        let mut s = SignalSource::new(InstrumentId::new(0), 0, Vec::new());
        assert!(s.next_event().is_none());
    }
}
