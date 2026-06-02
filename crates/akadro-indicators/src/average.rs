// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Moving averages: simple ([`Sma`]), exponential ([`Ema`]), and [`Macd`].

use std::collections::VecDeque;

use crate::Indicator;

/// Simple moving average over a fixed `window` (integer mean).
#[derive(Debug, Clone)]
pub struct Sma {
    window: usize,
    buf: VecDeque<i64>,
    /// i128 accumulator: a window of large fixed-point prices would overflow i64
    /// (m31).
    sum: i128,
}

impl Sma {
    /// Create an SMA over `window` samples.
    ///
    /// # Panics
    /// Panics if `window == 0`.
    #[must_use]
    pub fn new(window: usize) -> Self {
        assert!(window > 0, "window must be > 0");
        Sma {
            window,
            buf: VecDeque::with_capacity(window),
            sum: 0,
        }
    }
}

impl Indicator for Sma {
    type Output = i64;

    fn update(&mut self, input: i64) -> Option<i64> {
        self.buf.push_back(input);
        self.sum += i128::from(input);
        if self.buf.len() > self.window
            && let Some(old) = self.buf.pop_front()
        {
            self.sum -= i128::from(old);
        }
        (self.buf.len() == self.window).then(|| (self.sum / self.window as i128) as i64)
    }

    fn warm_up_bars(&self) -> usize {
        self.window
    }
}

/// Exponential moving average with period `n` (smoothing factor `2/(n+1)`).
///
/// **Seeding (convention):** seeded with the simple mean of the first `n` samples
/// (so it warms up for `n` bars and emits nothing before then), matching TA-Lib's
/// default. This differs from Tulip Indicators / `TradingView`, which seed from the
/// **first** value and emit immediately; expect a small transient difference vs
/// those references over the first ~`n` bars.
///
/// **Integer arithmetic (divergence):** the recurrence uses `i64` truncating
/// division, so it is deterministic and reproducible but diverges from a float
/// EMA — notably a "dead zone" where `(input − prev) * 2 / (n+1)` truncates to `0`
/// (the EMA stops moving for sub-tick deltas at large `n`). Feed prices in their
/// fixed-point raw units (small ticks) to keep the truncation error negligible.
#[derive(Debug, Clone)]
pub struct Ema {
    period: usize,
    /// i128 accumulator for the seed mean (a window of large prices overflows i64).
    seed_sum: i128,
    count: usize,
    current: Option<i64>,
}

impl Ema {
    /// Create an EMA with the given `period`.
    ///
    /// # Panics
    /// Panics if `period == 0`.
    #[must_use]
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "period must be > 0");
        Ema {
            period,
            seed_sum: 0,
            count: 0,
            current: None,
        }
    }
}

impl Indicator for Ema {
    type Output = i64;

    fn update(&mut self, input: i64) -> Option<i64> {
        match self.current {
            None => {
                self.seed_sum += i128::from(input);
                self.count += 1;
                if self.count == self.period {
                    let seed = (self.seed_sum / self.period as i128) as i64;
                    self.current = Some(seed);
                    Some(seed)
                } else {
                    None
                }
            }
            Some(prev) => {
                // ema += (input - prev) * 2 / (period + 1); widen so a large price
                // delta can't overflow the `* 2` (m31). Clamp the i128 result into
                // i64 — a bare `step as i64` would truncate before the saturate.
                let step = (i128::from(input) - i128::from(prev)) * 2 / (self.period as i128 + 1);
                let next = i64::try_from(i128::from(prev) + step).unwrap_or(if step > 0 {
                    i64::MAX
                } else {
                    i64::MIN
                });
                self.current = Some(next);
                Some(next)
            }
        }
    }

    fn warm_up_bars(&self) -> usize {
        self.period
    }
}

/// The three lines of a [`Macd`] reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacdValue {
    /// Fast EMA minus slow EMA.
    pub macd: i64,
    /// Signal line: EMA of the MACD line.
    pub signal: i64,
    /// MACD minus signal.
    pub histogram: i64,
}

/// Moving-average convergence/divergence: `EMA(fast) - EMA(slow)`, with a signal
/// EMA of that difference. Classic parameters are `(12, 26, 9)`.
#[derive(Debug, Clone)]
pub struct Macd {
    fast: Ema,
    slow: Ema,
    signal: Ema,
}

impl Macd {
    /// Create a MACD from fast/slow/signal periods.
    ///
    /// # Panics
    /// Panics if `fast >= slow` or any period is zero.
    #[must_use]
    pub fn new(fast: usize, slow: usize, signal: usize) -> Self {
        assert!(fast < slow, "fast period must be shorter than slow period");
        Macd {
            fast: Ema::new(fast),
            slow: Ema::new(slow),
            signal: Ema::new(signal),
        }
    }
}

impl Indicator for Macd {
    type Output = MacdValue;

    fn update(&mut self, input: i64) -> Option<MacdValue> {
        let fast = self.fast.update(input);
        let slow = self.slow.update(input);
        let (Some(fast), Some(slow)) = (fast, slow) else {
            return None;
        };
        let macd = fast - slow;
        let signal = self.signal.update(macd)?;
        Some(MacdValue {
            macd,
            signal,
            histogram: macd - signal,
        })
    }

    fn warm_up_bars(&self) -> usize {
        // The MACD line first emits when the slow EMA is seeded (`slow` bars); the
        // signal EMA then needs `signal` MACD values, so the full reading first
        // appears at `slow + signal - 1` (m31) — not `slow`.
        self.slow.period + self.signal.period - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sma_basic_and_sliding() {
        let mut sma = Sma::new(3);
        assert_eq!(sma.update(10), None);
        assert_eq!(sma.update(20), None);
        assert_eq!(sma.update(30), Some(20));
        assert_eq!(sma.update(60), Some(36)); // (20+30+60)/3 = 36
        assert_eq!(sma.update(60), Some(50)); // (30+60+60)/3 = 50
    }

    #[test]
    #[should_panic(expected = "window must be > 0")]
    fn sma_rejects_zero_window() {
        let _ = Sma::new(0);
    }

    #[test]
    fn ema_seeds_with_mean_then_tracks() {
        let mut ema = Ema::new(3);
        assert_eq!(ema.update(10), None);
        assert_eq!(ema.update(20), None);
        assert_eq!(ema.update(30), Some(20)); // seed = (10+20+30)/3
        // next = 20 + (40-20)*2/4 = 30
        assert_eq!(ema.update(40), Some(30));
        // constant input converges toward the input
        let mut flat = Ema::new(2);
        flat.update(100);
        let v = flat.update(100).unwrap();
        assert_eq!(v, 100);
        assert_eq!(flat.update(100), Some(100));
    }

    #[test]
    #[should_panic(expected = "period must be > 0")]
    fn ema_rejects_zero_period() {
        let _ = Ema::new(0);
    }

    #[test]
    fn macd_warms_up_then_emits() {
        let mut macd = Macd::new(2, 4, 2);
        // Feed a rising series; once warmed up, MACD should be defined and the
        // histogram = macd - signal.
        let mut last = None;
        for i in 1..=20 {
            last = macd.update(i * 10);
        }
        let v = last.expect("warmed up");
        assert_eq!(v.histogram, v.macd - v.signal);
        // Rising series -> fast EMA above slow EMA -> macd line positive.
        assert!(
            v.macd > 0,
            "rising series should give positive macd, got {}",
            v.macd
        );
    }

    #[test]
    #[should_panic(expected = "fast period must be shorter")]
    fn macd_rejects_bad_periods() {
        let _ = Macd::new(26, 12, 9);
    }

    #[test]
    fn warm_up_bars_match_first_some() {
        // m31: SMA emits at `window`; MACD's full reading first appears at
        // slow + signal - 1, not `slow`.
        assert_eq!(Sma::new(5).warm_up_bars(), 5);
        assert_eq!(Ema::new(5).warm_up_bars(), 5);
        let macd = Macd::new(2, 4, 3);
        assert_eq!(macd.warm_up_bars(), 4 + 3 - 1);
        let mut m = Macd::new(2, 4, 3);
        let mut n = 0;
        for v in 1..=50 {
            n += 1;
            if m.update(v * 10).is_some() {
                break;
            }
        }
        assert_eq!(n, macd.warm_up_bars());
    }
}
