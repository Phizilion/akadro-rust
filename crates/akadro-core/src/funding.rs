// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Funding-schedule helpers shared by perpetual-venue connectors.

use crate::Timestamp;

/// Default perpetual funding cadence (8 hours, in ms) — the most common across
/// venues (Binance / OKX / MEXC / Bybit), used as the fallback when a schedule is
/// too short to infer the period from.
pub const DEFAULT_FUNDING_PERIOD_MS: i64 = 8 * 3_600_000;

/// Infer the settlement period (milliseconds) of a perpetual funding schedule from
/// the **smallest positive gap** between consecutive settlement timestamps. Robust
/// to an out-of-order or duplicated point (those yield non-positive gaps, which are
/// filtered out). Falls back to [`DEFAULT_FUNDING_PERIOD_MS`] when fewer than two
/// usable points are present.
///
/// This is venue-neutral — every perp connector that returns a `(timestamp, rate)`
/// funding history infers its cadence the same way — so it lives here once (DRY)
/// rather than being re-implemented per connector.
///
/// ```
/// use akadro_core::{infer_funding_period_ms, Timestamp};
/// let h = 3_600_000_000_000; // one hour in ns
/// let sched = [
///     (Timestamp::from_nanos(0), 10),
///     (Timestamp::from_nanos(8 * h), 12),
///     (Timestamp::from_nanos(16 * h), 9),
/// ];
/// assert_eq!(infer_funding_period_ms(&sched), 8 * 3_600_000); // 8h
/// assert_eq!(infer_funding_period_ms(&sched[..1]), 8 * 3_600_000); // fallback
/// ```
#[must_use]
pub fn infer_funding_period_ms(schedule: &[(Timestamp, i64)]) -> i64 {
    schedule
        .windows(2)
        .map(|w| w[1].0.as_nanos() / 1_000_000 - w[0].0.as_nanos() / 1_000_000)
        .filter(|d| *d > 0)
        .min()
        .unwrap_or(DEFAULT_FUNDING_PERIOD_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_smallest_positive_gap() {
        let ms = 1_000_000; // 1 ms in ns
        let sched = [
            (Timestamp::from_nanos(0), 1),
            (Timestamp::from_nanos(4 * 3_600_000 * ms), 1), // +4h
            (Timestamp::from_nanos(12 * 3_600_000 * ms), 1), // +8h
        ];
        assert_eq!(infer_funding_period_ms(&sched), 4 * 3_600_000); // smallest gap = 4h
    }

    #[test]
    fn falls_back_on_too_few_points() {
        assert_eq!(infer_funding_period_ms(&[]), DEFAULT_FUNDING_PERIOD_MS);
        assert_eq!(
            infer_funding_period_ms(&[(Timestamp::from_nanos(0), 1)]),
            DEFAULT_FUNDING_PERIOD_MS
        );
    }

    #[test]
    fn ignores_non_positive_gaps() {
        let h = 3_600_000_000_000i64;
        // A duplicate/out-of-order point yields a non-positive gap, filtered out.
        let sched = [
            (Timestamp::from_nanos(0), 1),
            (Timestamp::from_nanos(0), 1),     // dup → gap 0
            (Timestamp::from_nanos(8 * h), 1), // +8h
        ];
        assert_eq!(infer_funding_period_ms(&sched), 8 * 3_600_000);
    }
}
