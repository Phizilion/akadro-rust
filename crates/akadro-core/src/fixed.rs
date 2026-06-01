// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fixed-point money math.
//!
//! akadro never uses floating point for prices, quantities, or balances. Floats
//! make backtest↔live parity impossible to guarantee (rounding differs by
//! evaluation order and target), so all monetary values are scaled integers.
//!
//! * [`Price`] and [`Qty`] are `i64` scaled integers ("raw" units). The decimal
//!   scale is a property of the *instrument* (see `InstrumentSpec`), not the
//!   number, so the arithmetic here is pure integer arithmetic.
//! * [`Money`] is an `i128` accumulator for notionals, balances and `PnL`.
//!
//! ## The widen-before-multiply rule (decision D12)
//!
//! `Price * Qty` overflows `i64` for realistic venues (e.g. a price scaled by
//! 10^8 times a size scaled by 10^8). Every multiplication therefore widens
//! both operands to `i128` *before* multiplying. This is enforced structurally:
//! there is **no** `Mul` impl that returns `i64`; the only multiply is
//! [`Price::notional`], which returns [`Money`] (`i128`). Benchmarks during
//! design showed `i128` notional math (~7.4 ns) is actually *faster* than the
//! equivalent `f64` path, so this costs nothing.

use core::fmt;

/// A price as a fixed-point scaled integer.
///
/// The integer is in the instrument's smallest price increment ("raw" units).
/// Construct with [`Price::from_raw`] and read the integer with [`Price::raw`].
///
/// ```
/// use akadro_core::Price;
/// let p = Price::from_raw(5_000_000); // e.g. $50,000.00 at scale 2
/// assert_eq!(p.raw(), 5_000_000);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Price(i64);

/// A quantity (order size, position size) as a fixed-point scaled integer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Qty(i64);

/// A monetary amount (notional, balance, `PnL`) as a 128-bit scaled integer.
///
/// `Money` is the accumulator type: notionals and running `PnL` are summed here so
/// that long backtests never overflow and remain bit-for-bit reproducible.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Money(i128);

/// Fixed-point scale (decimal places) for a **funding rate**: a rate of `r` raw
/// units means the fraction `r · 10⁻⁸`. So `1` basis point (`0.0001`) is `10_000`,
/// and a sub-basis-point rate like `0.0000466` is `4_669` — representable, where a
/// basis-point (`10⁻⁴`) scale would round it to `0`. This is exactly Binance's
/// `fundingRate` precision (8 decimals), captures OKX's sub-bp rates, and is the
/// unit [`Money::mul_rate`] divides by. Venue connectors normalize funding rates to
/// this scale; the engine charges them with `mul_rate`, so the two cannot drift.
pub const FUNDING_RATE_SCALE: u32 = 8;

impl Price {
    /// The zero price.
    pub const ZERO: Price = Price(0);

    /// Wrap a raw scaled integer as a [`Price`].
    #[inline]
    #[must_use]
    pub const fn from_raw(raw: i64) -> Self {
        Price(raw)
    }

    /// The raw scaled integer.
    #[inline]
    pub const fn raw(self) -> i64 {
        self.0
    }

    /// Checked addition; `None` on overflow.
    #[inline]
    pub const fn checked_add(self, rhs: Price) -> Option<Price> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Price(v)),
            None => None,
        }
    }

    /// Checked subtraction; `None` on overflow.
    #[inline]
    pub const fn checked_sub(self, rhs: Price) -> Option<Price> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Price(v)),
            None => None,
        }
    }

    /// Signed price difference `self - rhs`, widened to [`Money`] units.
    #[inline]
    pub const fn diff(self, rhs: Price) -> Money {
        Money(self.0 as i128 - rhs.0 as i128)
    }

    /// Notional value `price * qty`, widened to `i128` before multiplying
    /// (decision D12). This is the *only* multiplication involving a price, so
    /// the lossy `i64 * i64` path is unrepresentable.
    ///
    /// ```
    /// use akadro_core::{Price, Qty};
    /// let n = Price::from_raw(5_000_000).notional(Qty::from_raw(3));
    /// assert_eq!(n.raw(), 15_000_000);
    /// ```
    #[inline]
    pub const fn notional(self, qty: Qty) -> Money {
        Money(self.0 as i128 * qty.0 as i128)
    }

    /// A human-readable view at a decimal `scale` (the instrument's price scale).
    ///
    /// ```
    /// use akadro_core::Price;
    /// assert_eq!(Price::from_raw(5_000_000).display(2).to_string(), "50000.00");
    /// ```
    #[must_use]
    pub fn display(self, scale: u32) -> impl fmt::Display {
        Scaled {
            raw: i128::from(self.0),
            scale,
        }
    }
}

impl Qty {
    /// The zero quantity.
    pub const ZERO: Qty = Qty(0);

    /// Wrap a raw scaled integer as a [`Qty`].
    #[inline]
    #[must_use]
    pub const fn from_raw(raw: i64) -> Self {
        Qty(raw)
    }

    /// The raw scaled integer.
    #[inline]
    pub const fn raw(self) -> i64 {
        self.0
    }

    /// `true` if the quantity is exactly zero.
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Checked addition; `None` on overflow.
    #[inline]
    pub const fn checked_add(self, rhs: Qty) -> Option<Qty> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Qty(v)),
            None => None,
        }
    }

    /// Checked subtraction; `None` on overflow.
    #[inline]
    pub const fn checked_sub(self, rhs: Qty) -> Option<Qty> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Qty(v)),
            None => None,
        }
    }

    /// The smaller of two quantities.
    #[inline]
    #[must_use]
    pub const fn min(self, rhs: Qty) -> Qty {
        if self.0 <= rhs.0 { self } else { rhs }
    }

    /// A human-readable view at a decimal `scale` (the instrument's qty scale).
    #[must_use]
    pub fn display(self, scale: u32) -> impl fmt::Display {
        Scaled {
            raw: i128::from(self.0),
            scale,
        }
    }
}

impl Money {
    /// Zero money.
    pub const ZERO: Money = Money(0);

    /// Wrap a raw scaled 128-bit integer as [`Money`].
    #[inline]
    #[must_use]
    pub const fn from_raw(raw: i128) -> Self {
        Money(raw)
    }

    /// The raw scaled 128-bit integer.
    #[inline]
    pub const fn raw(self) -> i128 {
        self.0
    }

    /// Saturating addition (balances should never silently wrap).
    #[inline]
    #[must_use]
    pub const fn saturating_add(self, rhs: Money) -> Money {
        Money(self.0.saturating_add(rhs.0))
    }

    /// Saturating subtraction.
    #[inline]
    #[must_use]
    pub const fn saturating_sub(self, rhs: Money) -> Money {
        Money(self.0.saturating_sub(rhs.0))
    }

    /// Negate the amount (saturating, like the other `Money` operations: negating
    /// `i128::MIN` yields `i128::MAX` rather than overflowing).
    #[inline]
    #[must_use]
    pub const fn neg(self) -> Money {
        Money(self.0.saturating_neg())
    }

    /// Multiply by a basis-point rate, truncating toward zero
    /// (`self * bps / 10_000`). Used for fees. Truncation is deterministic and
    /// identical in backtest and live. The intermediate product **saturates** on
    /// overflow (consistent with the other `Money` arithmetic) rather than
    /// panicking in debug / wrapping in release — relevant only at the extreme
    /// `i128` range, far beyond any realistic notional.
    ///
    /// Note: because it truncates rather than rounding up, a modelled fee is a
    /// **conservative underestimate** versus a venue that rounds its fee up to the
    /// quote tick — at most one raw unit per fill, negligible at realistic scales.
    ///
    /// ```
    /// use akadro_core::Money;
    /// // 10 bps fee on a notional of 1_000_000 == 1000.
    /// assert_eq!(Money::from_raw(1_000_000).mul_bps(10).raw(), 1000);
    /// ```
    #[inline]
    #[must_use]
    pub const fn mul_bps(self, bps: i64) -> Money {
        Money(self.0.saturating_mul(bps as i128) / 10_000)
    }

    /// Multiply by a funding rate at [`FUNDING_RATE_SCALE`] (`self * rate / 10⁸`),
    /// truncating toward zero. This is the funding analogue of
    /// [`mul_bps`](Self::mul_bps) but `10_000×` finer, so **sub-basis-point** funding
    /// rates — routine on major perps (e.g. `0.0000466` = `0.47 bp`) — accrue a
    /// non-zero amount instead of rounding away as they do at basis-point scale. The
    /// divisor is derived from `FUNDING_RATE_SCALE`, so the connector's rate scale and
    /// the engine's charge can never disagree. Saturates on overflow like `mul_bps`.
    ///
    /// ```
    /// use akadro_core::Money;
    /// // 1 bp == 10_000 at this scale: the same charge mul_bps(1) would give.
    /// assert_eq!(Money::from_raw(1_000_000).mul_rate(10_000).raw(), 100);
    /// assert_eq!(Money::from_raw(1_000_000).mul_bps(1).raw(), 100);
    /// // A 0.47 bp rate (== 4_669) on a 1e9 notional accrues — mul_bps would see 0 bp.
    /// assert_eq!(Money::from_raw(1_000_000_000).mul_rate(4_669).raw(), 46_690);
    /// ```
    #[inline]
    #[must_use]
    pub const fn mul_rate(self, rate: i64) -> Money {
        Money(self.0.saturating_mul(rate as i128) / 10i128.pow(FUNDING_RATE_SCALE))
    }

    /// `true` if strictly positive.
    #[inline]
    pub const fn is_positive(self) -> bool {
        self.0 > 0
    }

    /// `true` if strictly negative.
    #[inline]
    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// A human-readable view at a decimal `scale` (e.g. `price_scale + qty_scale`
    /// for a notional, or the quote scale for a balance).
    ///
    /// ```
    /// use akadro_core::Money;
    /// assert_eq!(Money::from_raw(-12_345).display(2).to_string(), "-123.45");
    /// ```
    #[must_use]
    pub fn display(self, scale: u32) -> impl fmt::Display {
        Scaled { raw: self.0, scale }
    }
}

// ---- Debug / Display ---------------------------------------------------------
// Debug prints the raw integer with a type tag so logs stay unambiguous about
// scale; we deliberately do not guess a decimal point here (scale is per
// instrument). For human-readable output, the opt-in `display(scale)` helpers
// format the value at a caller-supplied decimal scale.

/// A scaled fixed-point value formatted at `scale` decimal places. Returned by
/// the `display(scale)` helpers; not constructed directly.
struct Scaled {
    raw: i128,
    scale: u32,
}

impl fmt::Display for Scaled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scale = self.scale.min(38); // 10^38 < u128::MAX; avoids pow overflow
        let div = 10u128.pow(scale);
        let abs = self.raw.unsigned_abs();
        if self.raw < 0 {
            write!(f, "-")?;
        }
        if scale == 0 {
            write!(f, "{abs}")
        } else {
            write!(
                f,
                "{}.{:0width$}",
                abs / div,
                abs % div,
                width = scale as usize
            )
        }
    }
}

impl fmt::Debug for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Price({})", self.0)
    }
}

impl fmt::Debug for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Qty({})", self.0)
    }
}

impl fmt::Debug for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Money({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_construct_and_read() {
        let p = Price::from_raw(123);
        assert_eq!(p.raw(), 123);
        assert_eq!(Price::ZERO.raw(), 0);
        assert_eq!(Price::default(), Price::ZERO);
    }

    #[test]
    fn price_checked_arithmetic() {
        assert_eq!(
            Price::from_raw(2).checked_add(Price::from_raw(3)),
            Some(Price::from_raw(5))
        );
        assert_eq!(
            Price::from_raw(5).checked_sub(Price::from_raw(3)),
            Some(Price::from_raw(2))
        );
        assert_eq!(
            Price::from_raw(i64::MAX).checked_add(Price::from_raw(1)),
            None
        );
        assert_eq!(
            Price::from_raw(i64::MIN).checked_sub(Price::from_raw(1)),
            None
        );
    }

    #[test]
    fn price_diff_widens() {
        assert_eq!(
            Price::from_raw(10).diff(Price::from_raw(4)),
            Money::from_raw(6)
        );
        // Difference that would overflow i64 is fine in i128.
        let d = Price::from_raw(i64::MAX).diff(Price::from_raw(i64::MIN));
        assert_eq!(d.raw(), i64::MAX as i128 - i64::MIN as i128);
    }

    #[test]
    fn notional_widens_before_multiply() {
        // i64 would overflow here; i128 does not.
        let big = Price::from_raw(1_000_000_000);
        let n = big.notional(Qty::from_raw(1_000_000_000));
        assert_eq!(n.raw(), 1_000_000_000i128 * 1_000_000_000i128);
        assert_eq!(
            Price::from_raw(5_000_000).notional(Qty::from_raw(3)).raw(),
            15_000_000
        );
    }

    #[test]
    fn qty_helpers() {
        assert!(Qty::ZERO.is_zero());
        assert!(!Qty::from_raw(1).is_zero());
        assert_eq!(
            Qty::from_raw(2).checked_add(Qty::from_raw(3)),
            Some(Qty::from_raw(5))
        );
        assert_eq!(
            Qty::from_raw(2).checked_sub(Qty::from_raw(3)),
            Some(Qty::from_raw(-1))
        );
        assert_eq!(Qty::from_raw(i64::MAX).checked_add(Qty::from_raw(1)), None);
        assert_eq!(Qty::from_raw(i64::MIN).checked_sub(Qty::from_raw(1)), None);
        assert_eq!(Qty::from_raw(2).min(Qty::from_raw(5)), Qty::from_raw(2));
        assert_eq!(Qty::from_raw(5).min(Qty::from_raw(2)), Qty::from_raw(2));
    }

    #[test]
    fn money_helpers() {
        assert_eq!(
            Money::from_raw(2).saturating_add(Money::from_raw(3)),
            Money::from_raw(5)
        );
        assert_eq!(
            Money::from_raw(2).saturating_sub(Money::from_raw(3)),
            Money::from_raw(-1)
        );
        assert_eq!(
            Money::from_raw(i128::MAX).saturating_add(Money::from_raw(1)),
            Money::from_raw(i128::MAX)
        );
        assert_eq!(
            Money::from_raw(i128::MIN).saturating_sub(Money::from_raw(1)),
            Money::from_raw(i128::MIN)
        );
        assert_eq!(Money::from_raw(5).neg(), Money::from_raw(-5));
        assert_eq!(
            Money::from_raw(1_000_000).mul_bps(10),
            Money::from_raw(1000)
        );
        assert_eq!(Money::from_raw(1_000_000).mul_bps(0), Money::ZERO);
        assert!(Money::from_raw(1).is_positive());
        assert!(!Money::from_raw(0).is_positive());
        assert!(Money::from_raw(-1).is_negative());
        assert!(!Money::from_raw(0).is_negative());
    }

    #[test]
    fn runtime_exercise_of_constructors_and_getters() {
        // With const arguments the compiler const-folds these calls away, so we
        // route the inputs through `black_box` to force a real runtime call
        // (and exercise the bodies for coverage).
        use std::hint::black_box;
        let a = black_box(123_i64);
        assert_eq!(black_box(Price::from_raw(a)).raw(), 123);
        assert_eq!(black_box(Qty::from_raw(a)).raw(), 123);
        let b = black_box(456_i128);
        assert_eq!(black_box(Money::from_raw(b)).raw(), 456);
    }

    #[test]
    fn debug_formatting() {
        assert_eq!(format!("{:?}", Price::from_raw(7)), "Price(7)");
        assert_eq!(format!("{:?}", Qty::from_raw(7)), "Qty(7)");
        assert_eq!(format!("{:?}", Money::from_raw(7)), "Money(7)");
    }

    #[test]
    fn mul_bps_saturates_instead_of_overflowing() {
        // Old `self.0 * bps` would overflow-panic in debug; saturating keeps it finite.
        assert_eq!(
            Money::from_raw(i128::MAX).mul_bps(2),
            Money::from_raw(i128::MAX / 10_000)
        );
        assert_eq!(
            Money::from_raw(i128::MIN).mul_bps(2),
            Money::from_raw(i128::MIN / 10_000)
        );
    }

    #[test]
    fn mul_rate_captures_sub_bp_and_agrees_with_mul_bps() {
        // The divisor is 10^FUNDING_RATE_SCALE — the connector scale and the engine
        // charge are wired to the same constant.
        assert_eq!(10i128.pow(FUNDING_RATE_SCALE), 100_000_000);
        // 1 bp == 10_000 at this scale → identical charge to mul_bps(1).
        let notional = Money::from_raw(1_000_000);
        assert_eq!(notional.mul_rate(10_000), notional.mul_bps(1));
        // THE FIX: a 0.47 bp rate (4_669) accrues, where the old bp-granular path
        // (mul_bps of the rounded `0` bp) would have charged nothing.
        let big = Money::from_raw(1_000_000_000);
        assert_eq!(big.mul_rate(4_669).raw(), 46_690);
        assert_eq!(big.mul_bps(0).raw(), 0); // what the old path did
        // Signed: backwardation credits the long (negative cost).
        assert_eq!(notional.mul_rate(-10_000), Money::from_raw(-100));
        // Saturates like mul_bps rather than overflow-panicking.
        assert_eq!(
            Money::from_raw(i128::MAX).mul_rate(2),
            Money::from_raw(i128::MAX / 100_000_000)
        );
    }

    #[test]
    fn scaled_display() {
        assert_eq!(
            Price::from_raw(5_000_000).display(2).to_string(),
            "50000.00"
        );
        assert_eq!(Qty::from_raw(123).display(0).to_string(), "123");
        assert_eq!(Money::from_raw(-12_345).display(2).to_string(), "-123.45");
        assert_eq!(Money::from_raw(5).display(6).to_string(), "0.000005");
        assert_eq!(Money::from_raw(0).display(2).to_string(), "0.00");
        // Extreme scale clamps to 38 rather than panicking on pow overflow.
        let _ = Money::from_raw(1).display(99).to_string();
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn price_qty_money_roundtrip(x in any::<i64>(), m in any::<i128>()) {
                prop_assert_eq!(Price::from_raw(x).raw(), x);
                prop_assert_eq!(Qty::from_raw(x).raw(), x);
                prop_assert_eq!(Money::from_raw(m).raw(), m);
            }

            #[test]
            fn notional_equals_widened_product(p in any::<i64>(), q in any::<i64>()) {
                // The product of two i64 always fits in i128, so widening is exact.
                let expected = i128::from(p) * i128::from(q);
                prop_assert_eq!(Price::from_raw(p).notional(Qty::from_raw(q)).raw(), expected);
            }

            #[test]
            fn mul_bps_matches_formula(
                m in (i128::from(i64::MIN))..=(i128::from(i64::MAX)),
                bps in 0i64..=10_000,
            ) {
                let expected = m * i128::from(bps) / 10_000;
                prop_assert_eq!(Money::from_raw(m).mul_bps(bps).raw(), expected);
            }

            #[test]
            fn diff_is_widened_difference(a in any::<i64>(), b in any::<i64>()) {
                prop_assert_eq!(Price::from_raw(a).diff(Price::from_raw(b)).raw(),
                                i128::from(a) - i128::from(b));
            }
        }
    }

    #[test]
    fn ordering_and_hash() {
        use std::collections::HashSet;
        assert!(Price::from_raw(1) < Price::from_raw(2));
        assert!(Qty::from_raw(2) > Qty::from_raw(1));
        assert!(Money::from_raw(1) < Money::from_raw(2));
        let mut set = HashSet::new();
        set.insert(Price::from_raw(1));
        assert!(set.contains(&Price::from_raw(1)));
    }
}
