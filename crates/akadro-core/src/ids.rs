// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Identifier and timestamp newtypes.
//!
//! Every id is an opaque, `Copy` newtype so the compiler stops you mixing, say,
//! an instrument id with a venue id. Ids are cheap (a single integer) and so are
//! used freely as map keys in the hot path.

use core::fmt;

/// Identifies a tradable instrument (symbol) within a run.
///
/// This is an engine-assigned dense integer (an index into the instrument
/// catalogue), *not* a venue symbol string — keeping the hot path integer-keyed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct InstrumentId(pub(crate) u32);

/// Identifies a settlement/collateral asset (e.g. USDT, BTC).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssetId(pub(crate) u32);

/// Identifies a venue (exchange) within a run.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VenueId(pub(crate) u32);

/// A client-assigned order id, unique and monotonic within a run.
///
/// The engine assigns these deterministically (a monotonic counter), so the
/// same strategy produces the same ids in backtest and live — essential for the
/// parity golden-master.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ClientOrderId(pub(crate) u64);

/// An event timestamp: nanoseconds since the Unix epoch (logical *event* time).
///
/// In both backtest and live this is the normalized event time of the data that
/// produced the current handler invocation — never wall-clock time. Two reads
/// within one handler return the same value in both modes (decision D4).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Timestamp(i64);

impl InstrumentId {
    /// Construct from a dense index.
    #[inline]
    #[must_use]
    pub const fn new(index: u32) -> Self {
        InstrumentId(index)
    }
    /// The underlying dense index.
    #[inline]
    pub const fn index(self) -> u32 {
        self.0
    }
}

impl AssetId {
    /// Construct from a dense index.
    #[inline]
    #[must_use]
    pub const fn new(index: u32) -> Self {
        AssetId(index)
    }
    /// The underlying dense index.
    #[inline]
    pub const fn index(self) -> u32 {
        self.0
    }
}

impl VenueId {
    /// Construct from a dense index.
    #[inline]
    #[must_use]
    pub const fn new(index: u32) -> Self {
        VenueId(index)
    }
    /// The underlying dense index.
    #[inline]
    pub const fn index(self) -> u32 {
        self.0
    }
}

impl ClientOrderId {
    /// Construct from a raw counter value. Normally the engine assigns these.
    #[inline]
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        ClientOrderId(raw)
    }
    /// The underlying counter value.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl Timestamp {
    /// The epoch (0 ns).
    pub const EPOCH: Timestamp = Timestamp(0);

    /// Construct from nanoseconds since the Unix epoch.
    #[inline]
    #[must_use]
    pub const fn from_nanos(nanos: i64) -> Self {
        Timestamp(nanos)
    }

    /// Nanoseconds since the Unix epoch.
    #[inline]
    pub const fn as_nanos(self) -> i64 {
        self.0
    }
}

impl fmt::Debug for InstrumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InstrumentId({})", self.0)
    }
}
impl fmt::Debug for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AssetId({})", self.0)
    }
}
impl fmt::Debug for VenueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VenueId({})", self.0)
    }
}
impl fmt::Debug for ClientOrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ClientOrderId({})", self.0)
    }
}
impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Timestamp({}ns)", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instrument_id_roundtrip() {
        let id = InstrumentId::new(7);
        assert_eq!(id.index(), 7);
        assert_eq!(format!("{id:?}"), "InstrumentId(7)");
    }

    #[test]
    fn asset_and_venue_ids() {
        assert_eq!(AssetId::new(3).index(), 3);
        assert_eq!(VenueId::new(4).index(), 4);
        assert_eq!(format!("{:?}", AssetId::new(3)), "AssetId(3)");
        assert_eq!(format!("{:?}", VenueId::new(4)), "VenueId(4)");
    }

    #[test]
    fn client_order_id() {
        let id = ClientOrderId::new(42);
        assert_eq!(id.raw(), 42);
        assert_eq!(format!("{id:?}"), "ClientOrderId(42)");
    }

    #[test]
    fn runtime_exercise_of_id_constructors_and_getters() {
        // `black_box` defeats const-folding so the const-fn bodies run at runtime.
        use std::hint::black_box;
        let u = black_box(11_u32);
        assert_eq!(black_box(InstrumentId::new(u)).index(), 11);
        assert_eq!(black_box(AssetId::new(u)).index(), 11);
        assert_eq!(black_box(VenueId::new(u)).index(), 11);
        let v = black_box(22_u64);
        assert_eq!(black_box(ClientOrderId::new(v)).raw(), 22);
        let n = black_box(33_i64);
        assert_eq!(black_box(Timestamp::from_nanos(n)).as_nanos(), 33);
    }

    #[test]
    fn timestamp_roundtrip_and_order() {
        let t = Timestamp::from_nanos(1_000);
        assert_eq!(t.as_nanos(), 1_000);
        assert!(Timestamp::EPOCH < t);
        assert_eq!(Timestamp::default(), Timestamp::EPOCH);
        assert_eq!(format!("{t:?}"), "Timestamp(1000ns)");
    }
}
