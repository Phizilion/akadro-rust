// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Order intents submitted by strategies.
//!
//! A strategy never mutates account state directly; it submits an
//! [`OrderRequest`] through the context and observes the consequences as
//! account events. The enums here are `#[non_exhaustive]` so new order kinds,
//! time-in-force policies, and venue commands can be added without a breaking
//! change (decisions D9, D11, D17).

use crate::fixed::{Price, Qty};
use crate::ids::{AssetId, ClientOrderId, InstrumentId};

/// Which side of the book an order rests on / lifts.
///
/// Deliberately **not** `#[non_exhaustive]`: a side is binary (buy or sell), the set is
/// closed, and there is no future variant to reserve for — so exhaustive `match`es on it
/// are correct and ergonomic rather than a semver hazard.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Side {
    /// Buy / long.
    Buy,
    /// Sell / short.
    Sell,
}

impl Side {
    /// The opposite side.
    #[inline]
    #[must_use]
    pub const fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    /// Signed multiplier: `+1` for buy, `-1` for sell. Useful for signed
    /// position/PnL arithmetic.
    #[inline]
    pub const fn sign(self) -> i64 {
        match self {
            Side::Buy => 1,
            Side::Sell => -1,
        }
    }
}

/// How a [`OrderKind::TrailingStop`] measures its trailing distance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum TrailKind {
    /// An absolute price offset (e.g. trail the high by `5.00`).
    Absolute(Price),
    /// A percentage callback rate in basis points of the reference price (e.g.
    /// `100` = 1%), the form every major crypto venue uses. The trigger is
    /// `ref_price * bps / 10_000` away from the reference (i128-widened, D12).
    Percent(u32),
}

/// Reference price a stop/trigger order watches (decision D17).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum TriggerBy {
    /// Last traded price.
    Last,
    /// Venue mark price.
    Mark,
    /// Index price.
    Index,
}

/// The kind of order and its price terms.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum OrderKind {
    /// Execute immediately at the best available price.
    Market,
    /// Rest at `limit` until filled or cancelled.
    Limit {
        /// Limit price.
        limit: Price,
    },
    /// Become a market order once `trigger` is touched (reference `by`). Arms on
    /// an *adverse* move (buy stop above the market, sell stop below).
    Stop {
        /// Trigger price.
        trigger: Price,
        /// Which reference price arms the trigger.
        by: TriggerBy,
    },
    /// Become a [`OrderKind::Limit`] at `limit` once `trigger` is touched.
    StopLimit {
        /// Trigger price that arms the order.
        trigger: Price,
        /// Limit price the armed order rests at.
        limit: Price,
        /// Which reference price arms the trigger.
        by: TriggerBy,
    },
    /// A stop whose trigger trails the most favourable price seen by `trail`
    /// (an absolute offset or a percentage callback rate), then fires like a
    /// [`OrderKind::Stop`].
    TrailingStop {
        /// Trailing distance — an absolute price offset or a percentage of the
        /// reference price.
        trail: TrailKind,
        /// Which reference price the trail follows.
        by: TriggerBy,
    },
    /// Become a market order once `trigger` is touched on a *favourable* move
    /// (buy MIT below the market, sell MIT above) — the mirror of a stop.
    MarketIfTouched {
        /// Trigger price.
        trigger: Price,
        /// Which reference price arms the trigger.
        by: TriggerBy,
    },
}

/// How long an order remains active.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub enum TimeInForce {
    /// Good 'til cancelled.
    #[default]
    Gtc,
    /// Immediate-or-cancel: fill what is possible now, cancel the rest.
    Ioc,
    /// Fill-or-kill: fill entirely now or cancel entirely.
    Fok,
}

/// A request to open a new order.
///
/// Built with [`OrderRequest::market`] / [`OrderRequest::limit`] and refined
/// with the builder-style setters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct OrderRequest {
    /// Instrument to trade.
    pub instrument: InstrumentId,
    /// Buy or sell.
    pub side: Side,
    /// Order size.
    pub qty: Qty,
    /// Order kind and price terms.
    pub kind: OrderKind,
    /// Time-in-force policy.
    pub tif: TimeInForce,
    /// If `true`, the order may only reduce an existing position.
    pub reduce_only: bool,
    /// If `true`, the order must add liquidity (maker-only / post-only): it is
    /// cancelled rather than crossing the spread as a taker.
    pub post_only: bool,
    /// Optional one-cancels-other group id. When any order in a group fills, the
    /// venue cancels the others in the same group (used for OCO / brackets).
    pub oco_group: Option<u32>,
}

impl OrderRequest {
    /// A market order.
    #[must_use]
    pub const fn market(instrument: InstrumentId, side: Side, qty: Qty) -> Self {
        OrderRequest {
            instrument,
            side,
            qty,
            kind: OrderKind::Market,
            tif: TimeInForce::Gtc,
            reduce_only: false,
            post_only: false,
            oco_group: None,
        }
    }

    /// A limit order at `limit`.
    #[must_use]
    pub const fn limit(instrument: InstrumentId, side: Side, qty: Qty, limit: Price) -> Self {
        OrderRequest {
            instrument,
            side,
            qty,
            kind: OrderKind::Limit { limit },
            tif: TimeInForce::Gtc,
            reduce_only: false,
            post_only: false,
            oco_group: None,
        }
    }

    /// A stop order that becomes a market order once `trigger` is touched
    /// (reference price `by`).
    #[must_use]
    pub const fn stop(
        instrument: InstrumentId,
        side: Side,
        qty: Qty,
        trigger: Price,
        by: crate::TriggerBy,
    ) -> Self {
        OrderRequest {
            instrument,
            side,
            qty,
            kind: OrderKind::Stop { trigger, by },
            tif: TimeInForce::Gtc,
            reduce_only: false,
            post_only: false,
            oco_group: None,
        }
    }

    /// Set the time-in-force.
    #[must_use]
    pub const fn with_tif(mut self, tif: TimeInForce) -> Self {
        self.tif = tif;
        self
    }

    /// A stop order that becomes a limit at `limit` once `trigger` is touched.
    #[must_use]
    pub const fn stop_limit(
        instrument: InstrumentId,
        side: Side,
        qty: Qty,
        trigger: Price,
        limit: Price,
        by: TriggerBy,
    ) -> Self {
        OrderRequest {
            instrument,
            side,
            qty,
            kind: OrderKind::StopLimit { trigger, limit, by },
            tif: TimeInForce::Gtc,
            reduce_only: false,
            post_only: false,
            oco_group: None,
        }
    }

    /// A trailing-stop order with an absolute price offset `trail`.
    #[must_use]
    pub const fn trailing_stop(
        instrument: InstrumentId,
        side: Side,
        qty: Qty,
        trail: Price,
        by: TriggerBy,
    ) -> Self {
        Self::trailing_stop_kind(instrument, side, qty, TrailKind::Absolute(trail), by)
    }

    /// A trailing-stop order with a percentage callback rate `bps` (basis points
    /// of the reference price; `100` = 1%).
    #[must_use]
    pub const fn trailing_stop_pct(
        instrument: InstrumentId,
        side: Side,
        qty: Qty,
        bps: u32,
        by: TriggerBy,
    ) -> Self {
        Self::trailing_stop_kind(instrument, side, qty, TrailKind::Percent(bps), by)
    }

    /// A trailing-stop order with an explicit [`TrailKind`].
    #[must_use]
    pub const fn trailing_stop_kind(
        instrument: InstrumentId,
        side: Side,
        qty: Qty,
        trail: TrailKind,
        by: TriggerBy,
    ) -> Self {
        OrderRequest {
            instrument,
            side,
            qty,
            kind: OrderKind::TrailingStop { trail, by },
            tif: TimeInForce::Gtc,
            reduce_only: false,
            post_only: false,
            oco_group: None,
        }
    }

    /// A market-if-touched order (fires on a favourable move to `trigger`).
    #[must_use]
    pub const fn market_if_touched(
        instrument: InstrumentId,
        side: Side,
        qty: Qty,
        trigger: Price,
        by: TriggerBy,
    ) -> Self {
        OrderRequest {
            instrument,
            side,
            qty,
            kind: OrderKind::MarketIfTouched { trigger, by },
            tif: TimeInForce::Gtc,
            reduce_only: false,
            post_only: false,
            oco_group: None,
        }
    }

    /// Mark the order reduce-only.
    #[must_use]
    pub const fn reduce_only(mut self) -> Self {
        self.reduce_only = true;
        self
    }

    /// Mark the order post-only (maker-only): cancel rather than take liquidity.
    ///
    /// Post-only is only meaningful for resting kinds ([`OrderKind::Limit`] and
    /// [`OrderKind::StopLimit`]). On a market or plain stop/MIT order it is a
    /// contradiction (those always take liquidity); the simulated exchange and
    /// venue connectors reject `post_only` on a non-limit kind as an invalid order
    /// rather than silently ignoring it.
    #[must_use]
    pub const fn post_only(mut self) -> Self {
        self.post_only = true;
        self
    }

    /// Assign the order to a one-cancels-other group.
    #[must_use]
    pub const fn oco(mut self, group: u32) -> Self {
        self.oco_group = Some(group);
        self
    }
}

/// An amendment to a working order (decision D11). Defaults leave a field
/// unchanged (`None`).
///
/// **Not yet wired:** there is no `Ctx::amend` path nor an `ExecutionClient::amend`
/// hook in this version (the type is reserved for D11). Until then, a strategy that
/// needs to modify a working order must **cancel and re-submit** it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub struct OrderAmend {
    /// New quantity, if changing.
    pub new_qty: Option<Qty>,
    /// New limit price, if changing.
    pub new_price: Option<Price>,
}

/// A venue-neutral, non-order command (decision D9).
///
/// Order placement goes through [`OrderRequest`]; everything that is *not* an
/// order (leverage, margin mode, token approvals) goes here so the surface can
/// grow without reshaping order submission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum VenueCommand {
    /// Set leverage for an instrument.
    SetLeverage {
        /// Target instrument.
        instrument: InstrumentId,
        /// Leverage multiple (e.g. 10 for 10x).
        leverage: u32,
    },
    /// Set the margin mode for an instrument.
    SetMarginMode {
        /// Target instrument.
        instrument: InstrumentId,
        /// Cross or isolated.
        mode: MarginMode,
    },
    /// Approve a spender to move an asset (DEX/ERC-20 style).
    ApproveAsset {
        /// Asset to approve.
        asset: AssetId,
    },
    /// Arm a cancel-on-disconnect / dead-man's-switch: the venue auto-cancels the
    /// session's open orders if it stops hearing from the client. The connection
    /// factory should issue this after each session opens; a connector translates
    /// it in `ExecutionClient::command`. `None` disarms.
    SetCancelOnDisconnect {
        /// Idle timeout in milliseconds before the venue cancels, or `None` to
        /// disable.
        timeout_ms: Option<u32>,
    },
}

/// Margin mode for a derivatives account.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum MarginMode {
    /// Collateral is shared across positions.
    Cross,
    /// Collateral is isolated per position.
    Isolated,
}

/// A handle to an in-flight venue command (decision D9).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ClientCommandId(pub(crate) u64);

impl ClientCommandId {
    /// Construct from a raw counter value.
    #[inline]
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        ClientCommandId(raw)
    }
    /// The underlying counter value.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Re-export for convenience: an order plus the id the engine assigned it.
///
/// `#[non_exhaustive]` (workspace growable-type policy); construct via
/// [`PlacedOrder::new`] so adding a field later is not a breaking change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct PlacedOrder {
    /// The id assigned at submission.
    pub id: ClientOrderId,
    /// The original request.
    pub request: OrderRequest,
}

impl PlacedOrder {
    /// An order paired with the id the engine assigned it at submission.
    #[must_use]
    pub const fn new(id: ClientOrderId, request: OrderRequest) -> Self {
        PlacedOrder { id, request }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INST: InstrumentId = InstrumentId::new(0);

    #[test]
    fn side_helpers() {
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.opposite(), Side::Buy);
        assert_eq!(Side::Buy.sign(), 1);
        assert_eq!(Side::Sell.sign(), -1);
    }

    #[test]
    fn market_order_defaults() {
        let o = OrderRequest::market(INST, Side::Buy, Qty::from_raw(5));
        assert_eq!(o.kind, OrderKind::Market);
        assert_eq!(o.tif, TimeInForce::Gtc);
        assert!(!o.reduce_only);
        assert_eq!(o.side, Side::Buy);
        assert_eq!(o.qty, Qty::from_raw(5));
    }

    #[test]
    fn limit_order_builder() {
        let o = OrderRequest::limit(INST, Side::Sell, Qty::from_raw(2), Price::from_raw(100))
            .with_tif(TimeInForce::Ioc)
            .reduce_only();
        assert_eq!(
            o.kind,
            OrderKind::Limit {
                limit: Price::from_raw(100)
            }
        );
        assert_eq!(o.tif, TimeInForce::Ioc);
        assert!(o.reduce_only);
    }

    #[test]
    fn tif_default_is_gtc() {
        assert_eq!(TimeInForce::default(), TimeInForce::Gtc);
    }

    #[test]
    fn amend_default_changes_nothing() {
        let a = OrderAmend::default();
        assert_eq!(a.new_qty, None);
        assert_eq!(a.new_price, None);
    }

    #[test]
    fn venue_commands_and_ids() {
        let c = VenueCommand::SetLeverage {
            instrument: INST,
            leverage: 10,
        };
        assert_eq!(
            c,
            VenueCommand::SetLeverage {
                instrument: INST,
                leverage: 10
            }
        );
        let _ = VenueCommand::SetMarginMode {
            instrument: INST,
            mode: MarginMode::Cross,
        };
        let _ = VenueCommand::ApproveAsset {
            asset: AssetId::new(1),
        };
        assert_ne!(MarginMode::Cross, MarginMode::Isolated);
        // `black_box` defeats const-folding so the const-fn bodies run at runtime.
        let raw = std::hint::black_box(9_u64);
        assert_eq!(std::hint::black_box(ClientCommandId::new(raw)).raw(), 9);
    }

    #[test]
    fn stop_and_triggerby() {
        let k = OrderKind::Stop {
            trigger: Price::from_raw(50),
            by: TriggerBy::Mark,
        };
        assert_eq!(
            k,
            OrderKind::Stop {
                trigger: Price::from_raw(50),
                by: TriggerBy::Mark
            }
        );
        assert_ne!(TriggerBy::Last, TriggerBy::Index);
    }

    #[test]
    fn stop_order_builder() {
        let o = OrderRequest::stop(
            INST,
            Side::Buy,
            Qty::from_raw(3),
            Price::from_raw(50),
            TriggerBy::Last,
        );
        assert_eq!(
            o.kind,
            OrderKind::Stop {
                trigger: Price::from_raw(50),
                by: TriggerBy::Last
            }
        );
        assert_eq!(o.tif, TimeInForce::Gtc);
        assert!(!o.reduce_only);
    }

    #[test]
    fn new_kinds_and_flags() {
        let sl = OrderRequest::stop_limit(
            INST,
            Side::Buy,
            Qty::from_raw(1),
            Price::from_raw(110),
            Price::from_raw(111),
            TriggerBy::Last,
        );
        assert_eq!(
            sl.kind,
            OrderKind::StopLimit {
                trigger: Price::from_raw(110),
                limit: Price::from_raw(111),
                by: TriggerBy::Last
            }
        );

        let ts = OrderRequest::trailing_stop(
            INST,
            Side::Sell,
            Qty::from_raw(1),
            Price::from_raw(5),
            TriggerBy::Mark,
        );
        assert_eq!(
            ts.kind,
            OrderKind::TrailingStop {
                trail: TrailKind::Absolute(Price::from_raw(5)),
                by: TriggerBy::Mark
            }
        );
        // Percentage callback rate.
        let tsp = OrderRequest::trailing_stop_pct(
            INST,
            Side::Sell,
            Qty::from_raw(1),
            100,
            TriggerBy::Mark,
        );
        assert_eq!(
            tsp.kind,
            OrderKind::TrailingStop {
                trail: TrailKind::Percent(100),
                by: TriggerBy::Mark
            }
        );

        let mit = OrderRequest::market_if_touched(
            INST,
            Side::Buy,
            Qty::from_raw(1),
            Price::from_raw(90),
            TriggerBy::Last,
        );
        assert_eq!(
            mit.kind,
            OrderKind::MarketIfTouched {
                trigger: Price::from_raw(90),
                by: TriggerBy::Last
            }
        );

        // Flags default off, set via builders.
        let o = OrderRequest::limit(INST, Side::Buy, Qty::from_raw(1), Price::from_raw(100));
        assert!(!o.post_only);
        assert_eq!(o.oco_group, None);
        let o = o.post_only().oco(7);
        assert!(o.post_only);
        assert_eq!(o.oco_group, Some(7));
    }

    #[test]
    fn placed_order_carries_id() {
        let o = OrderRequest::market(INST, Side::Buy, Qty::from_raw(1));
        let p = PlacedOrder {
            id: ClientOrderId::new(1),
            request: o,
        };
        assert_eq!(p.id, ClientOrderId::new(1));
        assert_eq!(p.request, o);
    }
}
