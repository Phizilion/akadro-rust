// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Volatility bands: [`Bollinger`].

use std::collections::VecDeque;

use crate::Indicator;

/// A Bollinger reading: middle band (mean) and the upper/lower bands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Band {
    /// Middle band (simple moving average).
    pub mid: i64,
    /// Upper band (`mid + k·σ`).
    pub upper: i64,
    /// Lower band (`mid − k·σ`).
    pub lower: i64,
}

/// Bollinger Bands: a `window`-sample mean with bands `k` integer standard
/// deviations away (population σ, integer square root).
#[derive(Debug, Clone)]
pub struct Bollinger {
    window: usize,
    k: i64,
    buf: VecDeque<i64>,
    /// i128 accumulator: a window of large fixed-point prices overflows i64 (m31).
    sum: i128,
}

impl Bollinger {
    /// Create Bollinger Bands over `window` samples at `k` standard deviations.
    ///
    /// # Panics
    /// Panics if `window == 0` or `k <= 0` (a non-positive multiplier would invert
    /// the bands, leaving `upper < lower`, m31).
    #[must_use]
    pub fn new(window: usize, k: i64) -> Self {
        assert!(window > 0, "window must be > 0");
        assert!(k > 0, "k must be > 0");
        Bollinger {
            window,
            k,
            buf: VecDeque::with_capacity(window),
            sum: 0,
        }
    }
}

impl Indicator for Bollinger {
    type Output = Band;

    fn update(&mut self, input: i64) -> Option<Band> {
        self.buf.push_back(input);
        self.sum += i128::from(input);
        if self.buf.len() > self.window
            && let Some(old) = self.buf.pop_front()
        {
            self.sum -= i128::from(old);
        }
        if self.buf.len() < self.window {
            return None;
        }
        let n = i128::from(self.window as i64);
        let mid = (self.sum / n) as i64;
        // Population variance with an i128 accumulator. Widen BEFORE subtracting so
        // `x - mid` cannot wrap in i64; saturate the square and the sum so an extreme
        // i64 input (deviation² can exceed i128::MAX) can't overflow either (m31).
        let var: i128 = self
            .buf
            .iter()
            .map(|&x| {
                let d = i128::from(x) - i128::from(mid);
                d.saturating_mul(d)
            })
            .fold(0i128, i128::saturating_add)
            / n;
        let sd = (var as u128).isqrt() as i64;
        // Widen the band half-width and saturate so a huge k·σ can't wrap (m31).
        let spread = i64::try_from(i128::from(self.k) * i128::from(sd)).unwrap_or(i64::MAX);
        Some(Band {
            mid,
            upper: mid.saturating_add(spread),
            lower: mid.saturating_sub(spread),
        })
    }

    fn warm_up_bars(&self) -> usize {
        self.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_series_has_zero_width() {
        let mut bb = Bollinger::new(4, 2);
        for _ in 0..4 {
            bb.update(100);
        }
        let band = bb.update(100).unwrap();
        assert_eq!(band.mid, 100);
        assert_eq!(band.upper, 100);
        assert_eq!(band.lower, 100);
    }

    #[test]
    fn known_variance() {
        // window of {2,4,4,4,5,5,7,9}: mean 5, population sd = 2.
        let mut bb = Bollinger::new(8, 2);
        for v in [2, 4, 4, 4, 5, 5, 7, 9] {
            bb.update(v);
        }
        // last update already returned; recompute by feeding the same window:
        let mut bb2 = Bollinger::new(8, 2);
        let mut last = None;
        for v in [2, 4, 4, 4, 5, 5, 7, 9] {
            last = bb2.update(v);
        }
        let band = last.unwrap();
        assert_eq!(band.mid, 5);
        assert_eq!(band.upper, 5 + 2 * 2); // mid + k*sd, sd=2, k=2
        assert_eq!(band.lower, 5 - 2 * 2);
    }

    #[test]
    fn warmup_is_none() {
        let mut bb = Bollinger::new(3, 2);
        assert_eq!(bb.update(1), None);
        assert_eq!(bb.update(2), None);
        assert!(bb.update(3).is_some());
    }

    #[test]
    #[should_panic(expected = "window must be > 0")]
    fn rejects_zero_window() {
        let _ = Bollinger::new(0, 2);
    }
}
