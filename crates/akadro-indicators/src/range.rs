// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! True-range volatility: [`Atr`].

/// Average True Range over `period` (Wilder smoothing, integer).
///
/// Unlike the single-input indicators, `Atr` needs the bar's high, low and
/// close, so it has its own [`Atr::update`] rather than implementing
/// [`Indicator`](crate::Indicator).
#[derive(Debug, Clone)]
pub struct Atr {
    period: usize,
    prev_close: Option<i64>,
    current: Option<i64>,
    /// i128 true-range accumulator (a period of large ranges overflows i64, m31).
    tr_sum: i128,
    count: usize,
}

impl Atr {
    /// Create an ATR with the given `period`.
    ///
    /// # Panics
    /// Panics if `period == 0`.
    #[must_use]
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "period must be > 0");
        Atr {
            period,
            prev_close: None,
            current: None,
            tr_sum: 0,
            count: 0,
        }
    }

    /// Feed one bar's `high`/`low`/`close` (raw scaled prices). Returns the new
    /// ATR, or `None` during warm-up.
    pub fn update(&mut self, high: i64, low: i64, close: i64) -> Option<i64> {
        // An inverted bar (high < low) would produce a negative true range and
        // corrupt the average; a real OHLC bar always has high >= low (m31).
        debug_assert!(high >= low, "ATR bar must have high >= low");
        let true_range = match self.prev_close {
            None => high - low,
            Some(pc) => (high - low).max((high - pc).abs()).max((low - pc).abs()),
        };
        self.prev_close = Some(close);

        match self.current {
            None => {
                self.tr_sum += i128::from(true_range);
                self.count += 1;
                if self.count == self.period {
                    // Clamp on the narrow back (never a bare `as i64`); true ranges and
                    // their mean are non-negative.
                    let seed = i64::try_from(self.tr_sum / self.period as i128).unwrap_or(i64::MAX);
                    self.current = Some(seed);
                    Some(seed)
                } else {
                    None
                }
            }
            Some(prev) => {
                // Wilder smoothing, widened so `prev * (p-1)` cannot overflow (m31);
                // clamp on the narrow back to match the try_from/clamp discipline.
                let p = i128::from(self.period as i64);
                let next = i64::try_from((i128::from(prev) * (p - 1) + i128::from(true_range)) / p)
                    .unwrap_or(i64::MAX);
                self.current = Some(next);
                Some(next)
            }
        }
    }

    /// The number of [`update`](Atr::update) calls before the first non-`None`
    /// output (the warm-up length): `period` bars.
    #[must_use]
    pub fn warm_up_bars(&self) -> usize {
        self.period
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atr_seeds_with_mean_true_range() {
        let mut atr = Atr::new(3);
        // First bar TR = high-low = 10.
        assert_eq!(atr.update(110, 100, 105), None);
        // TR = max(10, |110-105|, |100-105|) = 10.
        assert_eq!(atr.update(110, 100, 105), None);
        // Third bar -> seed = mean of the 3 TRs.
        assert_eq!(atr.update(110, 100, 105), Some(10));
    }

    #[test]
    fn atr_uses_gaps_in_true_range() {
        let mut atr = Atr::new(2);
        atr.update(110, 100, 108); // TR=10, prev_close=108
        // Next bar gaps up: high 130, low 120; TR = max(10, |130-108|, |120-108|) = 22.
        let v = atr.update(130, 120, 125).unwrap(); // seed = (10+22)/2 = 16
        assert_eq!(v, 16);
    }

    #[test]
    #[should_panic(expected = "period must be > 0")]
    fn rejects_zero_period() {
        let _ = Atr::new(0);
    }
}

#[cfg(test)]
mod cov_tests {
    use super::*;
    #[test]
    fn atr_wilder_continuation_branch() {
        let mut atr = Atr::new(2);
        assert_eq!(atr.update(110, 100, 105), None); // 1st
        assert!(atr.update(112, 108, 110).is_some()); // 2nd -> seed
        let v = atr.update(120, 110, 115).unwrap(); // 3rd -> Wilder smoothing
        assert!(v > 0);
    }
}
