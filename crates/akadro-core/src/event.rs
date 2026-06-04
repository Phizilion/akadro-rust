// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Market and account events — the vocabulary the engine feeds to strategies.
//!
//! There are two streams, and **both are identical in backtest and live**
//! (decision D5):
//!
//! * [`Event`] — market data the engine replays/receives (e.g. a [`Bar`]).
//! * [`AccountEvent`] — the *only* way account state changes are observed. There
//!   is no synchronous "what is my balance?" query anywhere in the API; a
//!   strategy learns everything from this stream, so backtest and live are
//!   structurally the same.
//!
//! All enums are `#[non_exhaustive]`: new variants are additive, but downstream
//! `match`es must carry a `_ =>` arm.

use smallvec::SmallVec;

use crate::fixed::{Money, Price, Qty};
use crate::ids::{AssetId, ClientOrderId, InstrumentId, Timestamp};
use crate::order::Side;

/// A single OHLCV bar for one instrument over one period.
///
/// `Bar` is `Copy`: a strategy may freely keep a *past* bar by value (legitimate
/// memory of history). It can never obtain a *future* bar, because the engine
/// only ever hands it the current one.
///
/// `#[non_exhaustive]`: an optional period/timeframe field may be added later
/// (e.g. for multi-timeframe dispatch or auto-derived `periods_per_year`) without
/// a semver break. Construct via [`Bar::new`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct Bar {
    /// Instrument this bar belongs to.
    pub instrument: InstrumentId,
    /// Close-of-bar event timestamp.
    pub ts: Timestamp,
    /// Open price.
    pub open: Price,
    /// High price.
    pub high: Price,
    /// Low price.
    pub low: Price,
    /// Close price.
    pub close: Price,
    /// Traded volume over the bar.
    pub volume: Qty,
}

impl Bar {
    /// Convenience constructor.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        instrument: InstrumentId,
        ts: Timestamp,
        open: Price,
        high: Price,
        low: Price,
        close: Price,
        volume: Qty,
    ) -> Self {
        Bar {
            instrument,
            ts,
            open,
            high,
            low,
            close,
            volume,
        }
    }
}

/// A market-data event delivered to the engine.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Event {
    /// A completed OHLCV bar.
    Bar(Bar),
    /// A connection gap / resync marker (decision D8). Replayable in backtest so
    /// the strategy's gap-handling path is exercised identically.
    Resync {
        /// Instrument affected (if scoped).
        instrument: Option<InstrumentId>,
        /// The last pre-disconnect event time — a *lower bound* on when the gap
        /// began (not the reconnect instant). A connector reconciling fill history
        /// across the gap should re-fetch from `max(ts, now − lookback)`, never
        /// `ts` alone, since events may have occurred after it during the outage.
        ts: Timestamp,
    },
    /// An auxiliary **scalar signal** observation for `instrument` on `channel`
    /// at `ts` — the carrier for non-OHLCV series: open interest, long/short
    /// ratio, market-wide liquidations, funding, and arbitrary external scalars
    /// (sentiment, supply, NAV, …). The engine appends it to backward-only
    /// observed state *before* invoking the strategy, so
    /// [`Ctx::signal`](../../akadro_engine/struct.Ctx.html#method.signal) reads it
    /// look-ahead-safely, exactly like a bar. Determinism is preserved: the value
    /// is fixed-point `i64` and the engine processes events in feed order (a
    /// multi-channel feed must order equal-`ts` events deterministically).
    Signal {
        /// Instrument the signal pertains to.
        instrument: InstrumentId,
        /// Channel id distinguishing concurrent signal series for one instrument
        /// (e.g. open-interest vs long/short-ratio vs an external score).
        channel: u16,
        /// The scalar value as a fixed-point `i64` (the scale is defined per
        /// channel by the producing feed).
        value: i64,
        /// Observation event time.
        ts: Timestamp,
    },
}

impl Event {
    /// The event timestamp, regardless of variant.
    #[must_use]
    pub fn ts(&self) -> Timestamp {
        match self {
            Event::Bar(b) => b.ts,
            Event::Resync { ts, .. } | Event::Signal { ts, .. } => *ts,
        }
    }
}

/// Well-known [`Event::Signal`] channel ids for common auxiliary series, so a
/// producing feed and a consuming strategy agree on a channel without a magic
/// number. Channels outside this set are free for caller-defined use (a `u16`
/// gives 65 536 channels per instrument).
pub mod signal_channel {
    /// Open interest — outstanding contracts (crowding / cascade risk).
    pub const OPEN_INTEREST: u16 = 1;
    /// Top-trader / account long-short ratio (herding indicator).
    pub const LONG_SHORT_RATIO: u16 = 2;
    /// Market-wide liquidation volume (forced-close cascades).
    pub const LIQUIDATIONS: u16 = 3;
}

/// The kind of a trading cost (decision D10). A single fill can incur several
/// costs in different assets (e.g. a DEX swap pays an LP fee in the input token
/// *and* gas in the native token).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum CostKind {
    /// Taker fee (crossed the spread).
    Taker,
    /// Maker fee/rebate (added liquidity).
    Maker,
    /// Liquidity-provider / pool fee (AMM).
    LpFee,
    /// Gas / network fee (charged even on reverted transactions).
    Gas,
    /// Settlement fee.
    Settlement,
    /// Funding payment (perpetuals).
    Funding,
}

/// One cost component of a fill. Construct via [`Cost::new`]; `#[non_exhaustive]`
/// so cost fields (e.g. a fee tier/rate per D17) can be added without a semver break.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct Cost {
    /// Asset the cost is denominated in.
    pub asset: AssetId,
    /// Signed amount (positive = paid, negative = rebate received).
    pub amount: Money,
    /// What kind of cost this is.
    pub kind: CostKind,
}

impl Cost {
    /// Construct a cost component.
    #[must_use]
    pub const fn new(asset: AssetId, amount: Money, kind: CostKind) -> Self {
        Cost {
            asset,
            amount,
            kind,
        }
    }
}

/// A bounded list of fill costs. Inline capacity 2 covers the common (single fee) and
/// DEX (fee + gas) cases without allocating.
///
/// An **opaque newtype**, not a `SmallVec` alias, so the `smallvec` dependency stays out
/// of akadro's public API (decision D13 — a `smallvec` major bump must not be a breaking
/// change here). Build with [`Costs::new`] + [`Costs::push`]; read via the `Deref<[Cost]>`
/// (`len`/`is_empty`/`first`/indexing/`iter`) or by iterating `&Costs`.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Costs(SmallVec<[Cost; 2]>);

impl Costs {
    /// An empty cost list.
    #[must_use]
    pub fn new() -> Self {
        Costs(SmallVec::new())
    }

    /// Append a cost component.
    pub fn push(&mut self, cost: Cost) {
        self.0.push(cost);
    }
}

impl core::ops::Deref for Costs {
    type Target = [Cost];
    fn deref(&self) -> &[Cost] {
        &self.0
    }
}

impl<'a> IntoIterator for &'a Costs {
    type Item = &'a Cost;
    // `slice::Iter`, NOT `smallvec::IntoIter` — keeps the foreign type out of the API.
    type IntoIter = core::slice::Iter<'a, Cost>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Why an order was rejected. Venue-neutral, closed-meaning set; venue-specific
/// causes bucket into [`RejectReason::VenueRejected`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum RejectReason {
    /// Insufficient balance / margin.
    InsufficientFunds,
    /// Order failed instrument filters (tick/lot/min-notional).
    InvalidOrder,
    /// `reduce_only` order would have increased the position.
    WouldIncreasePosition,
    /// Venue-specific rejection (non-deterministic; see docs).
    VenueRejected,
}

/// Why an order was cancelled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum CancelReason {
    /// Cancelled at the strategy's request.
    Requested,
    /// Cancelled because a sibling in the same OCO (one-cancels-other) group filled.
    OcoTriggered,
    /// Cancelled by the venue (e.g. self-trade prevention).
    VenueCanceled,
    /// Expired per time-in-force.
    Expired,
}

/// Why a *cancel request* was rejected (the order could not be cancelled).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum CancelRejectReason {
    /// No order with that id is known (never accepted, or already cleaned up).
    UnknownOrder,
    /// The order is already in a terminal state (filled / cancelled / expired).
    AlreadyTerminal,
}

/// An observed change to account state — the single source of truth a strategy
/// reads from (decision D5).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccountEvent {
    /// An order was accepted by the venue.
    OrderAccepted {
        /// The accepted order's id.
        id: ClientOrderId,
        /// When it was accepted (event time).
        ts: Timestamp,
    },
    /// An order was rejected.
    OrderRejected {
        /// The rejected order's id.
        id: ClientOrderId,
        /// Why.
        reason: RejectReason,
        /// When (event time).
        ts: Timestamp,
    },
    /// A conditional order (stop / stop-limit / market-if-touched) was triggered:
    /// its trigger condition was met and it became active. A `Fill` or
    /// `OrderCanceled` for the same id follows.
    OrderTriggered {
        /// The triggered order's id.
        id: ClientOrderId,
        /// When (event time).
        ts: Timestamp,
    },
    /// An order (partially or fully) filled.
    Fill {
        /// Which order filled (for attribution — decision D10).
        client_order_id: ClientOrderId,
        /// Instrument.
        instrument: InstrumentId,
        /// Side of the fill.
        side: Side,
        /// Execution price.
        price: Price,
        /// Filled quantity (this fill only).
        qty: Qty,
        /// Cost components (fees, gas…).
        costs: Costs,
        /// `true` if this fill completed the order (nothing remains); `false` for
        /// a partial fill, so a strategy need not accumulate quantity to know.
        complete: bool,
        /// When (event time).
        ts: Timestamp,
    },
    /// A perpetual funding payment settled against an open position. The `cost`
    /// is signed: positive = paid, negative = received (backwardation).
    FundingSettlement {
        /// Instrument whose position was charged/credited.
        instrument: InstrumentId,
        /// The signed funding cost.
        cost: Cost,
        /// The funding rate applied, signed (positive = longs pay shorts) at
        /// [`FUNDING_RATE_SCALE`](crate::FUNDING_RATE_SCALE) — i.e. `rate · 10⁻⁸`, so
        /// `10_000` is one basis point — the same fine-grained unit the venue
        /// connectors and the engine's `mul_rate` charge use. A strategy can observe
        /// or reconstruct the effective rate without back-solving `cost`.
        rate: i64,
        /// When (event time).
        ts: Timestamp,
    },
    /// A position was force-closed by the venue (margin liquidation). Reported
    /// distinctly from a `Fill` so a strategy can react to being liquidated; the
    /// position/cash effect is applied like a closing trade.
    Liquidation {
        /// Instrument liquidated.
        instrument: InstrumentId,
        /// Side of the closing trade.
        side: Side,
        /// Execution price of the close.
        price: Price,
        /// Quantity closed.
        qty: Qty,
        /// Costs charged on the close.
        costs: Costs,
        /// When (event time).
        ts: Timestamp,
    },
    /// An order was cancelled.
    OrderCanceled {
        /// The cancelled order's id.
        id: ClientOrderId,
        /// Why.
        reason: CancelReason,
        /// When (event time).
        ts: Timestamp,
    },
    /// A cancel *request* was rejected (the order could not be cancelled, e.g. it
    /// is unknown or already terminal). Lets a strategy polling for a cancel ack
    /// stop waiting instead of blocking forever.
    OrderCancelRejected {
        /// The order the cancel targeted.
        id: ClientOrderId,
        /// Why the cancel was rejected.
        reason: CancelRejectReason,
        /// When (event time).
        ts: Timestamp,
    },
    /// An order (or its remainder) expired per time-in-force (IOC/FOK remainder,
    /// or a day order at session end). Reported distinctly from `OrderCanceled` so
    /// expiry is not conflated with an explicit/venue cancel.
    OrderExpired {
        /// The expired order's id.
        id: ClientOrderId,
        /// When (event time).
        ts: Timestamp,
    },
    /// State was re-derived from a venue snapshot after a gap (decision D8).
    Resync {
        /// Instrument the resync is scoped to, mirroring the originating
        /// [`Event::Resync`]'s `instrument`. `None` means a venue-wide resync
        /// (all instruments); `Some(id)` a single-instrument one, so a strategy
        /// reconciling per-instrument state does not over-reconcile.
        instrument: Option<InstrumentId>,
        /// When (event time).
        ts: Timestamp,
    },
}

impl AccountEvent {
    /// The event timestamp, regardless of variant.
    #[must_use]
    pub fn ts(&self) -> Timestamp {
        match self {
            AccountEvent::OrderAccepted { ts, .. }
            | AccountEvent::OrderRejected { ts, .. }
            | AccountEvent::OrderTriggered { ts, .. }
            | AccountEvent::Fill { ts, .. }
            | AccountEvent::FundingSettlement { ts, .. }
            | AccountEvent::Liquidation { ts, .. }
            | AccountEvent::OrderCanceled { ts, .. }
            | AccountEvent::OrderCancelRejected { ts, .. }
            | AccountEvent::OrderExpired { ts, .. }
            | AccountEvent::Resync { ts, .. } => *ts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bar() -> Bar {
        Bar::new(
            InstrumentId::new(0),
            Timestamp::from_nanos(10),
            Price::from_raw(100),
            Price::from_raw(110),
            Price::from_raw(90),
            Price::from_raw(105),
            Qty::from_raw(1000),
        )
    }

    #[test]
    fn bar_fields() {
        let b = sample_bar();
        assert_eq!(b.open, Price::from_raw(100));
        assert_eq!(b.high, Price::from_raw(110));
        assert_eq!(b.low, Price::from_raw(90));
        assert_eq!(b.close, Price::from_raw(105));
        assert_eq!(b.volume, Qty::from_raw(1000));
        assert_eq!(b.ts, Timestamp::from_nanos(10));
    }

    #[test]
    fn event_ts_for_each_variant() {
        assert_eq!(Event::Bar(sample_bar()).ts(), Timestamp::from_nanos(10));
        let r = Event::Resync {
            instrument: None,
            ts: Timestamp::from_nanos(5),
        };
        assert_eq!(r.ts(), Timestamp::from_nanos(5));
    }

    #[test]
    fn cost_construction() {
        let c = Cost::new(AssetId::new(0), Money::from_raw(-5), CostKind::Maker);
        assert_eq!(c.amount, Money::from_raw(-5));
        assert_eq!(c.kind, CostKind::Maker);
        assert_ne!(CostKind::Taker, CostKind::Gas);
    }

    #[test]
    fn account_event_ts_for_each_variant() {
        let ts = Timestamp::from_nanos(7);
        let id = ClientOrderId::new(1);
        assert_eq!(AccountEvent::OrderAccepted { id, ts }.ts(), ts);
        assert_eq!(
            AccountEvent::OrderRejected {
                id,
                reason: RejectReason::InvalidOrder,
                ts
            }
            .ts(),
            ts
        );
        let mut costs = Costs::new();
        costs.push(Cost::new(
            AssetId::new(0),
            Money::from_raw(1),
            CostKind::Taker,
        ));
        let fill = AccountEvent::Fill {
            client_order_id: id,
            instrument: InstrumentId::new(0),
            side: Side::Buy,
            price: Price::from_raw(100),
            qty: Qty::from_raw(1),
            costs: costs.clone(),
            complete: true,
            ts,
        };
        assert_eq!(fill.ts(), ts);
        assert_eq!(AccountEvent::OrderTriggered { id, ts }.ts(), ts);
        assert_eq!(
            AccountEvent::FundingSettlement {
                instrument: InstrumentId::new(0),
                cost: Cost::new(AssetId::new(1), Money::from_raw(-2), CostKind::Funding),
                rate: -5,
                ts,
            }
            .ts(),
            ts
        );
        assert_eq!(
            AccountEvent::OrderCancelRejected {
                id,
                reason: CancelRejectReason::UnknownOrder,
                ts,
            }
            .ts(),
            ts
        );
        assert_eq!(AccountEvent::OrderExpired { id, ts }.ts(), ts);
        assert_eq!(
            AccountEvent::Liquidation {
                instrument: InstrumentId::new(0),
                side: Side::Sell,
                price: Price::from_raw(100),
                qty: Qty::from_raw(1),
                costs,
                ts,
            }
            .ts(),
            ts
        );
        assert_eq!(
            AccountEvent::OrderCanceled {
                id,
                reason: CancelReason::Requested,
                ts
            }
            .ts(),
            ts
        );
        assert_eq!(
            AccountEvent::Resync {
                instrument: Some(InstrumentId::new(0)),
                ts
            }
            .ts(),
            ts
        );
        assert_eq!(
            AccountEvent::Resync {
                instrument: None,
                ts
            }
            .ts(),
            ts
        );
    }

    #[test]
    fn reject_and_cancel_reasons_distinct() {
        assert_ne!(RejectReason::InsufficientFunds, RejectReason::VenueRejected);
        assert_ne!(CancelReason::Requested, CancelReason::Expired);
        assert_ne!(
            RejectReason::WouldIncreasePosition,
            RejectReason::InvalidOrder
        );
        assert_eq!(CancelReason::VenueCanceled, CancelReason::VenueCanceled);
    }
}
