// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rolling extremes: [`RollingMax`] and [`RollingMin`] over a fixed window.

use std::collections::VecDeque;

use crate::Indicator;

/// Maximum value over the last `window` samples.
///
/// Uses a **monotonic deque** of `(index, value)` pairs kept decreasing by value,
/// so each `update` is O(1) amortized (each sample is pushed and popped at most
/// once) rather than an O(window) scan.
#[derive(Debug, Clone)]
pub struct RollingMax {
    window: usize,
    count: usize,
    deque: VecDeque<(usize, i64)>,
}

impl RollingMax {
    /// Create a rolling maximum over `window` samples.
    ///
    /// # Panics
    /// Panics if `window == 0`.
    #[must_use]
    pub fn new(window: usize) -> Self {
        assert!(window > 0, "window must be > 0");
        RollingMax {
            window,
            count: 0,
            deque: VecDeque::with_capacity(window),
        }
    }
}

impl Indicator for RollingMax {
    type Output = i64;

    fn update(&mut self, input: i64) -> Option<i64> {
        let i = self.count;
        self.count += 1;
        // Drop back entries that can never be the max while `input` is in window.
        while self.deque.back().is_some_and(|&(_, v)| v <= input) {
            self.deque.pop_back();
        }
        self.deque.push_back((i, input));
        // Drop the front once it has slid out of the window (index <= i - window).
        while self
            .deque
            .front()
            .is_some_and(|&(idx, _)| idx + self.window <= i)
        {
            self.deque.pop_front();
        }
        (self.count >= self.window).then(|| self.deque.front().expect("non-empty").1)
    }

    fn warm_up_bars(&self) -> usize {
        // `update` returns `None` until `count >= window`, so the first `Some`
        // arrives on the `window`-th update (matching SMA's convention). The default
        // `0` lied, panicking strategies that `is_warmed_up(ind.warm_up_bars())`.
        self.window
    }
}

/// Minimum value over the last `window` samples.
///
/// Mirror of [`RollingMax`]: a monotonic deque kept increasing by value, O(1)
/// amortized per `update`.
#[derive(Debug, Clone)]
pub struct RollingMin {
    window: usize,
    count: usize,
    deque: VecDeque<(usize, i64)>,
}

impl RollingMin {
    /// Create a rolling minimum over `window` samples.
    ///
    /// # Panics
    /// Panics if `window == 0`.
    #[must_use]
    pub fn new(window: usize) -> Self {
        assert!(window > 0, "window must be > 0");
        RollingMin {
            window,
            count: 0,
            deque: VecDeque::with_capacity(window),
        }
    }
}

impl Indicator for RollingMin {
    type Output = i64;

    fn update(&mut self, input: i64) -> Option<i64> {
        let i = self.count;
        self.count += 1;
        while self.deque.back().is_some_and(|&(_, v)| v >= input) {
            self.deque.pop_back();
        }
        self.deque.push_back((i, input));
        while self
            .deque
            .front()
            .is_some_and(|&(idx, _)| idx + self.window <= i)
        {
            self.deque.pop_front();
        }
        (self.count >= self.window).then(|| self.deque.front().expect("non-empty").1)
    }

    fn warm_up_bars(&self) -> usize {
        self.window // first `Some` on the `window`-th update; see `RollingMax`.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_extremes_warm_up_equals_first_some() {
        // warm_up_bars() must equal the number of updates until the first Some, so a
        // strategy guarding with `is_warmed_up(ind.warm_up_bars())` never unwraps None.
        for window in [1usize, 3, 4, 7] {
            let mut m = RollingMax::new(window);
            let mut first_some = None;
            for k in 1..=window + 2 {
                if m.update(k as i64).is_some() {
                    first_some = Some(k);
                    break;
                }
            }
            assert_eq!(m.warm_up_bars(), window);
            assert_eq!(first_some, Some(window), "first Some on update {window}");
            assert_eq!(RollingMin::new(window).warm_up_bars(), window);
        }
    }

    #[test]
    fn rolling_max_slides() {
        let mut m = RollingMax::new(3);
        assert_eq!(m.update(1), None);
        assert_eq!(m.update(5), None);
        assert_eq!(m.update(3), Some(5));
        assert_eq!(m.update(2), Some(5)); // window {5,3,2}
        assert_eq!(m.update(1), Some(3)); // window {3,2,1} -> 5 left
        assert_eq!(m.update(9), Some(9)); // window {2,1,9}
    }

    #[test]
    fn rolling_min_slides() {
        let mut m = RollingMin::new(3);
        m.update(5);
        m.update(1);
        assert_eq!(m.update(3), Some(1));
        assert_eq!(m.update(4), Some(1)); // {1,3,4}
        assert_eq!(m.update(9), Some(3)); // {3,4,9} -> 1 left
    }

    #[test]
    #[should_panic(expected = "window must be > 0")]
    fn max_rejects_zero() {
        let _ = RollingMax::new(0);
    }

    #[test]
    #[should_panic(expected = "window must be > 0")]
    fn min_rejects_zero() {
        let _ = RollingMin::new(0);
    }
}
