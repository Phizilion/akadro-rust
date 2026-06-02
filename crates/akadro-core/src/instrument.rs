// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Instrument metadata and venue capability descriptions.
//!
//! [`InstrumentSpec`] is the venue-neutral description of a tradable instrument
//! (its assets, tick/lot sizes, minimum notional). [`CapSet`] is an opaque
//! bitset describing what a venue supports, so strategies and the engine can
//! reason about capabilities without exposing a foreign `enumset` type in the
//! public API (decision D13).

use crate::fixed::{Money, Price, Qty};
use crate::ids::{AssetId, InstrumentId};

/// A single venue capability flag.
///
/// Each variant carries an **explicit, stable bit index** (0..64). The index is
/// the variant's identity in a [`CapSet`] bitset, so it must never change once
/// assigned; a new capability takes the next free index (max 63). Explicit
/// discriminants mean inserting a variant in the middle of the list can never
/// silently re-number the others (a real risk with implicit discriminants).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
#[repr(u8)]
pub enum Capability {
    /// Supports resting limit orders.
    LimitOrders = 0,
    /// Supports stop / trigger orders.
    StopOrders = 1,
    /// Supports post-only orders.
    PostOnly = 2,
    /// Supports reduce-only orders.
    ReduceOnly = 3,
    /// Supports margin / leverage.
    Margin = 4,
    /// Charges/pays funding (perpetuals).
    Funding = 5,
    /// Supports modifying (amending) a working order.
    ModifyOrder = 6,
    /// Supports short selling.
    ShortSelling = 7,
}

impl Capability {
    #[inline]
    const fn bit(self) -> u64 {
        // `self as u32` is the explicit discriminant above. The 64-slot bitset
        // requires index < 64; `checked_shl` turns an out-of-range discriminant into
        // a hard panic (a programmer error) in EVERY build, rather than a silently
        // wrapped shift in release.
        let idx = self as u32;
        match 1u64.checked_shl(idx) {
            Some(b) => b,
            None => panic!("Capability bit index must be < 64"),
        }
    }
}

/// An opaque set of [`Capability`] flags (a `u64` bitset).
///
/// Built fluently with [`CapSet::with`]:
///
/// ```
/// use akadro_core::{CapSet, Capability};
/// let caps = CapSet::empty().with(Capability::LimitOrders).with(Capability::Margin);
/// assert!(caps.contains(Capability::LimitOrders));
/// assert!(!caps.contains(Capability::PostOnly));
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct CapSet(u64);

impl CapSet {
    /// The empty capability set.
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        CapSet(0)
    }

    /// Return a copy with `cap` added.
    #[inline]
    #[must_use]
    pub const fn with(self, cap: Capability) -> Self {
        CapSet(self.0 | cap.bit())
    }

    /// `true` if `cap` is present.
    #[inline]
    pub const fn contains(self, cap: Capability) -> bool {
        self.0 & cap.bit() != 0
    }

    /// `true` if no capabilities are set.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// The economic kind of an instrument (decision D17 reserves room for more).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum InstrumentKind {
    /// Spot instrument settled in the quote asset.
    Spot,
    /// Perpetual future with funding.
    PerpetualFuture,
}

/// How to round a value to the instrument's grid.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub enum RoundingRule {
    /// Round toward zero (conservative for sizing).
    #[default]
    TowardZero,
    /// Round to the nearest grid point (ties toward zero).
    Nearest,
}

/// Venue-neutral description of a tradable instrument.
///
/// Construct with [`InstrumentSpec::new`]; the struct is `#[non_exhaustive]` so
/// fields can be added without breaking downstream code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct InstrumentSpec {
    /// Engine-assigned instrument id.
    pub id: InstrumentId,
    /// Base asset (what you hold a position in).
    pub base: AssetId,
    /// Quote/settlement asset (what `PnL` accrues in).
    pub quote: AssetId,
    /// Economic kind.
    pub kind: InstrumentKind,
    /// Minimum price increment.
    pub tick_size: Price,
    /// Minimum quantity increment.
    pub lot_size: Qty,
    /// Minimum order notional.
    pub min_notional: Money,
    /// Supported venue capabilities for this instrument.
    pub caps: CapSet,
    /// Static taxonomy labels (e.g. `&["L1", "defi"]`) for sector rotation and
    /// screeners — empty by default, set via [`InstrumentSpec::with_labels`].
    /// `&'static` keeps the spec `Copy` (compile-time tags). *Time-varying*
    /// fundamentals (circulating supply, market cap) ride the signal channel
    /// (`ctx.signal`), not this slot.
    pub labels: &'static [&'static str],
}

impl InstrumentSpec {
    /// Construct an instrument spec (with no taxonomy labels — add them with
    /// [`InstrumentSpec::with_labels`]).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        id: InstrumentId,
        base: AssetId,
        quote: AssetId,
        kind: InstrumentKind,
        tick_size: Price,
        lot_size: Qty,
        min_notional: Money,
        caps: CapSet,
    ) -> Self {
        InstrumentSpec {
            id,
            base,
            quote,
            kind,
            tick_size,
            lot_size,
            min_notional,
            caps,
            labels: &[],
        }
    }

    /// Return a copy with static taxonomy `labels` attached (sector / tags), for
    /// sector-rotation and screener strategies. `&'static` so the spec stays
    /// `Copy`; use hard-coded label literals (compile-time taxonomy).
    #[must_use]
    pub const fn with_labels(mut self, labels: &'static [&'static str]) -> Self {
        self.labels = labels;
        self
    }

    /// Whether this instrument carries the taxonomy `label` (case-sensitive).
    #[must_use]
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.contains(&label)
    }

    /// Round a quantity *down* to a whole number of lots. A `lot_size` of zero
    /// or negative is treated as "no lot grid" and returns `qty` unchanged.
    #[must_use]
    pub fn round_qty_down(&self, qty: Qty) -> Qty {
        let lot = self.lot_size.raw();
        if lot <= 0 {
            return qty;
        }
        let raw = qty.raw();
        // Truncate toward zero in lot units. Rust's `%` is the truncated
        // remainder (sign of the dividend), so `raw - raw % lot` floors toward
        // zero for both positive and negative sizes.
        Qty::from_raw(raw - raw % lot)
    }

    /// Round a price *down* to the tick grid. A `tick_size` of zero or negative is
    /// treated as "no grid" and returns `price` unchanged. Strategies should snap a
    /// computed limit/stop price to the grid before submitting — an off-grid price
    /// is silently accepted in backtest but rejected by a real venue.
    #[must_use]
    pub fn round_price_down(&self, price: Price) -> Price {
        let tick = self.tick_size.raw();
        if tick <= 0 {
            return price;
        }
        let raw = price.raw();
        // `checked_sub` cannot actually underflow here (`rem_euclid` is in
        // `[0, tick)` so the result is `<= raw`), but we avoid the bare `-` so a
        // future change near `i64::MIN` can never wrap; fall back to `price`.
        match raw.checked_sub(raw.rem_euclid(tick)) {
            Some(snapped) => Price::from_raw(snapped),
            None => price,
        }
    }

    /// Round a price *up* to the tick grid (the next tick at or above `price`).
    /// A `tick_size` of zero or negative returns `price` unchanged.
    #[must_use]
    pub fn round_price_up(&self, price: Price) -> Price {
        let tick = self.tick_size.raw();
        if tick <= 0 {
            return price;
        }
        let raw = price.raw();
        let rem = raw.rem_euclid(tick);
        if rem == 0 {
            price
        } else {
            // Snap up to the next tick, saturating rather than wrapping near
            // `i64::MAX` (an off-grid price that close to the limit is degenerate,
            // but must never silently wrap to a negative price).
            match raw.checked_add(tick - rem) {
                Some(snapped) => Price::from_raw(snapped),
                None => price,
            }
        }
    }

    /// Snap `price` to the tick grid per `rule`: [`RoundingRule::TowardZero`] rounds
    /// down (the conservative default, == [`round_price_down`](Self::round_price_down));
    /// [`RoundingRule::Nearest`] rounds to the closest tick, ties toward zero. A
    /// non-positive `tick_size` returns `price` unchanged.
    #[must_use]
    pub fn round_price(&self, price: Price, rule: RoundingRule) -> Price {
        match rule {
            RoundingRule::TowardZero => self.round_price_down(price),
            RoundingRule::Nearest => {
                let down = self.round_price_down(price);
                let up = self.round_price_up(price);
                let d_down = (i128::from(price.raw()) - i128::from(down.raw())).abs();
                let d_up = (i128::from(up.raw()) - i128::from(price.raw())).abs();
                if d_down <= d_up { down } else { up } // tie → toward zero
            }
        }
    }

    /// Snap `qty` to the lot grid per `rule`: [`RoundingRule::TowardZero`] truncates
    /// (the conservative default, == [`round_qty_down`](Self::round_qty_down));
    /// [`RoundingRule::Nearest`] rounds to the closest lot, ties toward zero. A
    /// non-positive `lot_size` returns `qty` unchanged.
    #[must_use]
    pub fn round_qty(&self, qty: Qty, rule: RoundingRule) -> Qty {
        let down = self.round_qty_down(qty);
        let lot = self.lot_size.raw();
        if matches!(rule, RoundingRule::TowardZero) || lot <= 0 || down.raw() == qty.raw() {
            return down;
        }
        // Nearest: the away-from-zero neighbour (same sign as `qty`).
        let away_raw = if qty.raw() >= 0 {
            down.raw().checked_add(lot)
        } else {
            down.raw().checked_sub(lot)
        };
        let Some(away_raw) = away_raw else {
            return down;
        };
        let d_down = (i128::from(qty.raw()) - i128::from(down.raw())).abs();
        let d_away = (i128::from(qty.raw()) - i128::from(away_raw)).abs();
        if d_down <= d_away {
            down
        } else {
            Qty::from_raw(away_raw)
        }
    }

    /// `true` if `price * qty` meets the instrument's minimum notional (by
    /// absolute value). A negative `min_notional` is treated as "no minimum"
    /// (clamped to `0`), so a malformed spec can never silently disable the guard.
    #[must_use]
    pub fn meets_min_notional(&self, price: Price, qty: Qty) -> bool {
        let notional = price.notional(qty).raw().abs();
        notional >= self.min_notional.raw().max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(lot: i64, min_notional: i128) -> InstrumentSpec {
        InstrumentSpec::new(
            InstrumentId::new(0),
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(1),
            Qty::from_raw(lot),
            Money::from_raw(min_notional),
            CapSet::empty().with(Capability::LimitOrders),
        )
    }

    #[test]
    fn labels_default_empty_and_taggable() {
        let s = spec(1, 0);
        assert!(s.labels.is_empty());
        assert!(!s.has_label("L1"));
        let tagged = s.with_labels(&["L1", "defi"]);
        assert_eq!(tagged.labels, ["L1", "defi"]);
        assert!(tagged.has_label("L1") && tagged.has_label("defi"));
        assert!(!tagged.has_label("meme"));
        // Spec stays `Copy` after adding the static-label slot: using `tagged` by
        // value twice compiles only because it is `Copy`.
        let a = tagged;
        let b = tagged;
        assert_eq!(a, b);
    }

    #[test]
    fn round_price_to_tick_grid() {
        let s = InstrumentSpec::new(
            InstrumentId::new(0),
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(5), // tick = 5
            Qty::from_raw(1),
            Money::ZERO,
            CapSet::empty(),
        );
        assert_eq!(s.round_price_down(Price::from_raw(12)), Price::from_raw(10));
        assert_eq!(s.round_price_up(Price::from_raw(12)), Price::from_raw(15));
        // Already on-grid: unchanged both ways.
        assert_eq!(s.round_price_down(Price::from_raw(10)), Price::from_raw(10));
        assert_eq!(s.round_price_up(Price::from_raw(10)), Price::from_raw(10));
        // tick <= 0 means "no grid": a no-op.
        let no_grid = InstrumentSpec::new(
            InstrumentId::new(0),
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(0),
            Qty::from_raw(1),
            Money::ZERO,
            CapSet::empty(),
        );
        assert_eq!(
            no_grid.round_price_down(Price::from_raw(12)),
            Price::from_raw(12)
        );
    }

    #[test]
    fn capset_ops() {
        let c = CapSet::empty();
        assert!(c.is_empty());
        let c = c.with(Capability::LimitOrders).with(Capability::Margin);
        assert!(c.contains(Capability::LimitOrders));
        assert!(c.contains(Capability::Margin));
        assert!(!c.contains(Capability::PostOnly));
        assert!(!c.is_empty());
        assert_eq!(CapSet::default(), CapSet::empty());
    }

    #[test]
    fn runtime_exercise_of_capset() {
        // `black_box` forces `Capability::bit` / `CapSet::with` to run at runtime.
        use std::hint::black_box;
        let cap = black_box(Capability::Margin);
        let set = black_box(CapSet::empty()).with(cap);
        assert!(black_box(set).contains(black_box(Capability::Margin)));
    }

    #[test]
    fn capability_bits_distinct() {
        // Every capability must map to a distinct bit.
        let all = [
            Capability::LimitOrders,
            Capability::StopOrders,
            Capability::PostOnly,
            Capability::ReduceOnly,
            Capability::Margin,
            Capability::Funding,
            Capability::ModifyOrder,
            Capability::ShortSelling,
        ];
        let mut acc = CapSet::empty();
        for cap in all {
            assert!(!acc.contains(cap));
            acc = acc.with(cap);
            assert!(acc.contains(cap));
        }
    }

    #[test]
    fn round_qty_down_grids() {
        let s = spec(5, 0);
        assert_eq!(s.round_qty_down(Qty::from_raw(12)), Qty::from_raw(10));
        assert_eq!(s.round_qty_down(Qty::from_raw(10)), Qty::from_raw(10));
        assert_eq!(s.round_qty_down(Qty::from_raw(4)), Qty::from_raw(0));
    }

    #[test]
    fn round_qty_down_no_grid() {
        let s = spec(0, 0);
        assert_eq!(s.round_qty_down(Qty::from_raw(7)), Qty::from_raw(7));
        let s = spec(-1, 0);
        assert_eq!(s.round_qty_down(Qty::from_raw(7)), Qty::from_raw(7));
    }

    #[test]
    fn min_notional_check() {
        let s = spec(1, 1000);
        assert!(s.meets_min_notional(Price::from_raw(100), Qty::from_raw(10))); // 1000 >= 1000
        assert!(s.meets_min_notional(Price::from_raw(100), Qty::from_raw(11))); // 1100
        assert!(!s.meets_min_notional(Price::from_raw(100), Qty::from_raw(9))); // 900 < 1000
        // Absolute value: a short (negative qty) of equal magnitude also passes.
        assert!(s.meets_min_notional(Price::from_raw(100), Qty::from_raw(-10)));
    }

    #[test]
    fn min_notional_negative_is_no_minimum() {
        // A malformed spec with a negative threshold must not silently disable the
        // guard: it clamps to 0, so any non-negative notional passes (m7).
        let s = spec(1, -1000);
        assert!(s.meets_min_notional(Price::from_raw(0), Qty::from_raw(0)));
        assert!(s.meets_min_notional(Price::from_raw(1), Qty::from_raw(1)));
    }

    #[test]
    fn round_price_saturates_near_bounds() {
        // tick-snapping must never wrap an extreme off-grid price (m6).
        let s = InstrumentSpec::new(
            InstrumentId::new(0),
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(5), // tick = 5
            Qty::from_raw(1),
            Money::ZERO,
            CapSet::empty(),
        );
        // Just below i64::MAX and off-grid: rounding up would overflow, so the
        // price is returned unchanged rather than wrapping negative.
        let near_max = Price::from_raw(i64::MAX - 1);
        assert_eq!(s.round_price_up(near_max), near_max);
        // Rounding down near i64::MIN must not underflow either.
        let near_min = Price::from_raw(i64::MIN + 1);
        let down = s.round_price_down(near_min);
        assert!(down.raw() <= near_min.raw());
    }

    #[test]
    fn rounding_rule_default() {
        assert_eq!(RoundingRule::default(), RoundingRule::TowardZero);
        assert_ne!(RoundingRule::Nearest, RoundingRule::TowardZero);
    }

    #[test]
    fn round_with_rule() {
        // tick 5, lot 10.
        let s = InstrumentSpec::new(
            InstrumentId::new(0),
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(5),
            Qty::from_raw(10),
            Money::ZERO,
            CapSet::empty().with(Capability::LimitOrders),
        );
        // Price: TowardZero == down; Nearest picks the closer tick.
        assert_eq!(
            s.round_price(Price::from_raw(13), RoundingRule::TowardZero),
            Price::from_raw(10)
        );
        assert_eq!(
            s.round_price(Price::from_raw(12), RoundingRule::Nearest),
            Price::from_raw(10) // |12-10|=2 < |15-12|=3
        );
        assert_eq!(
            s.round_price(Price::from_raw(13), RoundingRule::Nearest),
            Price::from_raw(15) // |13-10|=3 > |15-13|=2
        );
        assert_eq!(
            s.round_price(Price::from_raw(10), RoundingRule::Nearest),
            Price::from_raw(10) // on-grid unchanged
        );
        // Qty: Nearest picks the closer lot; an exact tie goes toward zero.
        assert_eq!(
            s.round_qty(Qty::from_raw(16), RoundingRule::TowardZero),
            Qty::from_raw(10)
        );
        assert_eq!(
            s.round_qty(Qty::from_raw(14), RoundingRule::Nearest),
            Qty::from_raw(10) // |14-10|=4 < |20-14|=6
        );
        assert_eq!(
            s.round_qty(Qty::from_raw(16), RoundingRule::Nearest),
            Qty::from_raw(20)
        );
        assert_eq!(
            s.round_qty(Qty::from_raw(15), RoundingRule::Nearest),
            Qty::from_raw(10) // exact tie → toward zero
        );
    }

    #[test]
    fn instrument_kind_distinct() {
        assert_ne!(InstrumentKind::Spot, InstrumentKind::PerpetualFuture);
    }
}
