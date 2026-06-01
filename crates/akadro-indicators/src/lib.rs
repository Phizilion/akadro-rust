// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # akadro-indicators
//!
//! Incremental technical indicators for akadro strategies. Every indicator is:
//!
//! * **Incremental** — you feed it one value at a time, in order, and it keeps a
//!   small amount of running state. It only ever sees the present and the past,
//!   so it cannot leak the future.
//! * **Integer / fixed-point** — pure integer math over scaled values (feed
//!   `price.raw()`; outputs are in the same scale, except [`Rsi`] which returns a
//!   0..=10000 level meaning 0.00–100.00). No floats, so results are
//!   deterministic and identical in backtest and live.
//!
//! Note: because the recursive indicators ([`Ema`], [`Rsi`], [`Atr`], the
//! [`Bollinger`] band width) use truncating `i64` division, their values diverge
//! slightly from float reference implementations (TA-Lib / pandas-ta) — at most a
//! fraction of a tick when prices are fed in fixed-point raw units, traded for
//! exact reproducibility. See [`Ema`] for the concrete "dead-zone" example.
//!
//! Most indicators implement the [`Indicator`] trait (`update(input) -> Option`,
//! `None` during warm-up). [`Atr`] takes high/low/close so has its own `update`.
//!
//! ```
//! use akadro_indicators::{Indicator, Sma};
//! let mut sma = Sma::new(3);
//! assert_eq!(sma.update(10), None);          // warming up
//! assert_eq!(sma.update(20), None);
//! assert_eq!(sma.update(30), Some(20));       // (10+20+30)/3
//! assert_eq!(sma.update(60), Some(36));       // (20+30+60)/3 = 36
//! ```

mod average;
mod channel;
mod extremes;
mod oscillator;
mod range;

pub use average::{Ema, Macd, MacdValue, Sma};
pub use channel::{Band, Bollinger};
pub use extremes::{RollingMax, RollingMin};
pub use oscillator::Rsi;
pub use range::Atr;

/// A single-input streaming indicator.
///
/// Feed inputs in time order with [`Indicator::update`]; it returns the new value
/// once enough history has accumulated, or `None` while warming up.
pub trait Indicator {
    /// The value emitted once warmed up.
    type Output: Copy;

    /// Feed the next input value; returns the indicator's new value, or `None`
    /// during the warm-up period.
    fn update(&mut self, input: i64) -> Option<Self::Output>;

    /// The number of [`update`](Indicator::update) calls before the **first**
    /// non-`None` output — the indicator's warm-up length. Use it (rather than a
    /// hand-guessed bar count) to gate trading until the indicator is live, e.g.
    /// `if ctx.is_warmed_up(inst, ind.warm_up_bars()) { … }`. The default is `0`
    /// (always-ready); recursive/windowed indicators override it (e.g. [`Rsi`]
    /// needs `period + 1` because the first input only establishes the baseline).
    fn warm_up_bars(&self) -> usize {
        0
    }
}
