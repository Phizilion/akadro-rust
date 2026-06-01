// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # sma-crossover
//!
//! A worked example strategy: a simple moving-average crossover. It exists to
//! demonstrate idiomatic akadro strategy code that:
//!
//! * is written with **fully elided lifetimes** (the author never types `'bar`);
//! * keeps state as plain scalars in `self` (running window sums) — remembering
//!   the *past* is fine;
//! * reads only the present and the past via `ctx.closes(...).ago(n)`;
//! * forbids `unsafe` to make the look-ahead guarantee total within this crate
//!   (decision D3).
//!
//! The exact same source runs in backtest and live.
#![forbid(unsafe_code)]

use akadro_core::{Bar, InstrumentId, OrderRequest, Qty, Side};
use akadro_engine::{Ctx, Strategy};

/// A moving-average crossover strategy.
///
/// Goes long when the fast SMA crosses above the slow SMA, and flat/short when
/// it crosses below. Window sums are maintained incrementally; the value leaving
/// each window is read from the (past) series with `ago`.
#[derive(Debug, Clone)]
pub struct SmaCross {
    instrument: InstrumentId,
    fast: usize,
    slow: usize,
    order_qty: Qty,
    fast_sum: i64,
    slow_sum: i64,
    count: usize,
    prev_fast_ge_slow: Option<bool>,
}

impl SmaCross {
    /// Create an SMA-crossover strategy.
    ///
    /// # Panics
    /// Panics if `fast == 0` or `fast >= slow` (a crossover needs `fast < slow`).
    #[must_use]
    pub fn new(instrument: InstrumentId, fast: usize, slow: usize, order_qty: Qty) -> Self {
        assert!(fast > 0, "fast window must be > 0");
        assert!(fast < slow, "fast window must be shorter than slow window");
        SmaCross {
            instrument,
            fast,
            slow,
            order_qty,
            fast_sum: 0,
            slow_sum: 0,
            count: 0,
            prev_fast_ge_slow: None,
        }
    }
}

impl Strategy for SmaCross {
    fn on_bar(&mut self, bar: Bar, ctx: &mut Ctx<'_>) {
        let close = bar.close.raw();
        self.count += 1;
        self.fast_sum += close;
        self.slow_sum += close;

        // Drop the value that has just left each window. `ago(w)` reads a PAST
        // close (allowed); there is no way to read ahead.
        if self.count > self.fast
            && let Some(p) = ctx.closes(self.instrument).and_then(|s| s.ago(self.fast))
        {
            self.fast_sum -= p.raw();
        }
        if self.count > self.slow
            && let Some(p) = ctx.closes(self.instrument).and_then(|s| s.ago(self.slow))
        {
            self.slow_sum -= p.raw();
        }

        // Warm-up: wait until the slow window is full.
        if self.count < self.slow {
            return;
        }

        let fast_sma = self.fast_sum / self.fast as i64;
        let slow_sma = self.slow_sum / self.slow as i64;
        let fast_ge_slow = fast_sma >= slow_sma;

        if let Some(prev) = self.prev_fast_ge_slow {
            if fast_ge_slow && !prev && ctx.net_qty(self.instrument).raw() <= 0 {
                // Crossed up: go long.
                ctx.submit(OrderRequest::market(
                    self.instrument,
                    Side::Buy,
                    self.order_qty,
                ));
            } else if !fast_ge_slow && prev && ctx.net_qty(self.instrument).raw() >= 0 {
                // Crossed down: exit / go short.
                ctx.submit(OrderRequest::market(
                    self.instrument,
                    Side::Sell,
                    self.order_qty,
                ));
            }
        }
        self.prev_fast_ge_slow = Some(fast_ge_slow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "fast window must be shorter")]
    fn rejects_fast_ge_slow() {
        let _ = SmaCross::new(InstrumentId::new(0), 5, 5, Qty::from_raw(1));
    }

    #[test]
    #[should_panic(expected = "fast window must be > 0")]
    fn rejects_zero_fast() {
        let _ = SmaCross::new(InstrumentId::new(0), 0, 5, Qty::from_raw(1));
    }

    #[test]
    fn constructs() {
        let s = SmaCross::new(InstrumentId::new(0), 2, 4, Qty::from_raw(3));
        assert_eq!(s.fast, 2);
        assert_eq!(s.slow, 4);
        assert_eq!(s.order_qty, Qty::from_raw(3));
        // exercise Clone + Debug
        let s2 = s.clone();
        assert!(format!("{s2:?}").contains("SmaCross"));
    }
}
