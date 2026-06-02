// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Validation that a `&[Bar]` slice is a well-ordered event stream.
//!
//! The engine appends bars in arrival order and a [`Series`](crate) is indexed by
//! insertion, so the whole look-ahead and parity machinery assumes the input stream
//! is **non-decreasing in time**. Out-of-order bars silently drive the engine
//! backward and corrupt fills/equity — a classic "garbage in" footgun, especially
//! for hand-built / CSV-concatenated `Vec<Bar>` that skipped the ordering the
//! `akadro-data` cache enforces on read.
//!
//! This check lives in `akadro-core` (where [`Bar`] is defined) so every layer that
//! accepts externally-sourced bars — the walk-forward entry points in
//! `akadro-analytics` and the checked feed constructor in `akadro-backtest` —
//! validates *identically* against one predicate (DRY), without those crates having
//! to depend on each other for it.

use crate::Bar;

/// Why a `&[Bar]` slice is not a valid event stream. Carries the offending index
/// and timestamps for diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BarOrderError {
    /// The bar at `idx` has an earlier timestamp than its predecessor (time went
    /// backward) — the corrupting case.
    NotAscending {
        /// Index of the offending bar.
        idx: usize,
        /// Its (earlier) timestamp, in nanoseconds.
        ts: i64,
        /// The predecessor's (later) timestamp, in nanoseconds.
        prev_ts: i64,
    },
    /// The bar at `idx` repeats the previous bar's timestamp for the **same
    /// instrument** (a true duplicate). Equal timestamps on *different* instruments
    /// are allowed (a legitimately interleaved multi-instrument timeline) and never
    /// reported here.
    DuplicateTimestamp {
        /// Index of the duplicate bar.
        idx: usize,
        /// The repeated timestamp, in nanoseconds.
        ts: i64,
    },
}

/// Check that `bars` is a valid event stream: **non-decreasing** in timestamp, with
/// no two *consecutive* same-instrument bars sharing a timestamp. Equal timestamps
/// across **different** instruments are permitted (an interleaved multi-instrument
/// feed). A slice of length `< 2` is trivially `Ok`.
///
/// Validation is single-pass over consecutive pairs, which catches the realistic
/// corruptions (a descending timestamp, an adjacent same-instrument duplicate); it
/// deliberately does not deduplicate across a whole equal-timestamp run.
///
/// # Errors
/// [`BarOrderError`] describing the first offending index.
pub fn check_bars_ordered(bars: &[Bar]) -> Result<(), BarOrderError> {
    for (i, pair) in bars.windows(2).enumerate() {
        let (prev, cur) = (&pair[0], &pair[1]);
        let (prev_ts, ts) = (prev.ts.as_nanos(), cur.ts.as_nanos());
        if ts < prev_ts {
            return Err(BarOrderError::NotAscending {
                idx: i + 1,
                ts,
                prev_ts,
            });
        }
        if ts == prev_ts && cur.instrument == prev.instrument {
            return Err(BarOrderError::DuplicateTimestamp { idx: i + 1, ts });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InstrumentId, Price, Qty, Timestamp};

    fn bar_on(ts: i64, inst: u32) -> Bar {
        let p = Price::from_raw(1);
        Bar::new(
            InstrumentId::new(inst),
            Timestamp::from_nanos(ts),
            p,
            p,
            p,
            p,
            Qty::from_raw(1),
        )
    }
    fn bar(ts: i64) -> Bar {
        bar_on(ts, 0)
    }

    #[test]
    fn ascending_unique_is_ok() {
        assert_eq!(check_bars_ordered(&[]), Ok(()));
        assert_eq!(check_bars_ordered(&[bar(5)]), Ok(()));
        assert_eq!(check_bars_ordered(&[bar(0), bar(10), bar(20)]), Ok(()));
    }

    #[test]
    fn descending_is_not_ascending() {
        assert_eq!(
            check_bars_ordered(&[bar(0), bar(20), bar(10)]),
            Err(BarOrderError::NotAscending {
                idx: 2,
                ts: 10,
                prev_ts: 20
            })
        );
    }

    #[test]
    fn same_instrument_duplicate_ts_is_rejected() {
        assert_eq!(
            check_bars_ordered(&[bar(0), bar(10), bar(10)]),
            Err(BarOrderError::DuplicateTimestamp { idx: 2, ts: 10 })
        );
    }

    #[test]
    fn equal_ts_different_instrument_is_allowed() {
        // An interleaved two-instrument timeline: same ts, different instruments.
        assert_eq!(
            check_bars_ordered(&[bar_on(10, 0), bar_on(10, 1), bar_on(20, 0)]),
            Ok(())
        );
    }
}
