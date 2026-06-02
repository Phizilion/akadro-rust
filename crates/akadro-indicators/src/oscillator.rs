// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Momentum oscillators: [`Rsi`].

use crate::Indicator;

/// Wilder's Relative Strength Index over `period`.
///
/// Output is a level in `0..=10000`, i.e. RSI × 100 (so `7000` == 70.00). Uses
/// integer Wilder smoothing; a flat series (no average movement) reads `5000`.
#[derive(Debug, Clone)]
pub struct Rsi {
    period: usize,
    prev: Option<i64>,
    avg_gain: i64,
    avg_loss: i64,
    seed_gain: i128,
    seed_loss: i128,
    count: usize,
    seeded: bool,
}

impl Rsi {
    /// Create an RSI with the given `period`.
    ///
    /// # Panics
    /// Panics if `period == 0`.
    #[must_use]
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "period must be > 0");
        Rsi {
            period,
            prev: None,
            avg_gain: 0,
            avg_loss: 0,
            seed_gain: 0,
            seed_loss: 0,
            count: 0,
            seeded: false,
        }
    }

    fn level(&self) -> i64 {
        let denom = i128::from(self.avg_gain) + i128::from(self.avg_loss);
        if denom == 0 {
            5000
        } else {
            // Widen: `10_000 * avg_gain` overflows i64 for large fixed-point gains
            // (m31). The quotient is in `0..=10_000`, so the cast back is exact.
            (10_000 * i128::from(self.avg_gain) / denom) as i64
        }
    }
}

impl Indicator for Rsi {
    type Output = i64;

    fn update(&mut self, input: i64) -> Option<i64> {
        let Some(prev) = self.prev else {
            self.prev = Some(input);
            return None;
        };
        // Widen the first difference so an extreme price pair cannot overflow i64,
        // and accumulate the seed in i128 to match the Wilder path (m31).
        let delta = i128::from(input) - i128::from(prev);
        self.prev = Some(input);
        let (gain, loss) = if delta >= 0 { (delta, 0) } else { (0, -delta) };

        if !self.seeded {
            self.seed_gain += gain;
            self.seed_loss += loss;
            self.count += 1;
            if self.count == self.period {
                let p = i128::from(self.period as i64);
                self.avg_gain = i64::try_from(self.seed_gain / p).unwrap_or(i64::MAX);
                self.avg_loss = i64::try_from(self.seed_loss / p).unwrap_or(i64::MAX);
                self.seeded = true;
                return Some(self.level());
            }
            return None;
        }

        // Wilder smoothing, widened so `avg * (p-1)` cannot overflow i64 (m31).
        let p = i128::from(self.period as i64);
        self.avg_gain = ((i128::from(self.avg_gain) * (p - 1) + gain) / p) as i64;
        self.avg_loss = ((i128::from(self.avg_loss) * (p - 1) + loss) / p) as i64;
        Some(self.level())
    }

    fn warm_up_bars(&self) -> usize {
        // The first `update` only establishes the baseline `prev`; `period` deltas
        // then seed the averages, so the first output is on call `period + 1` — not
        // `period` (m31).
        self.period + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rising_series_is_high_rsi() {
        let mut rsi = Rsi::new(4);
        let mut last = None;
        for i in 1..=10 {
            last = rsi.update(i * 100);
        }
        // Monotonic up -> all gains, no losses -> RSI = 100.00.
        assert_eq!(last, Some(10_000));
    }

    #[test]
    fn falling_series_is_low_rsi() {
        let mut rsi = Rsi::new(4);
        let mut last = None;
        for i in 0..10 {
            last = rsi.update(1000 - i * 100);
        }
        // Monotonic down -> all losses -> RSI = 0.
        assert_eq!(last, Some(0));
    }

    #[test]
    fn flat_series_is_neutral() {
        let mut rsi = Rsi::new(3);
        rsi.update(500);
        rsi.update(500);
        rsi.update(500);
        // After enough flat samples, no movement -> neutral 50.00.
        assert_eq!(rsi.update(500), Some(5000));
    }

    #[test]
    fn warmup_returns_none() {
        let mut rsi = Rsi::new(3);
        assert_eq!(rsi.update(10), None); // first sample, no delta yet
        assert_eq!(rsi.update(11), None); // 1st delta
        assert_eq!(rsi.update(12), None); // 2nd delta
        assert!(rsi.update(13).is_some()); // 3rd delta -> seeded
    }

    #[test]
    #[should_panic(expected = "period must be > 0")]
    fn rejects_zero_period() {
        let _ = Rsi::new(0);
    }

    #[test]
    fn warm_up_bars_matches_first_some() {
        // m31: RSI(3) needs period+1 = 4 update calls before the first value.
        let rsi = Rsi::new(3);
        assert_eq!(rsi.warm_up_bars(), 4);
        let mut r = Rsi::new(3);
        let mut count_to_first = 0;
        for v in 1..=20 {
            count_to_first += 1;
            if r.update(v * 10).is_some() {
                break;
            }
        }
        assert_eq!(count_to_first, rsi.warm_up_bars());
    }
}
