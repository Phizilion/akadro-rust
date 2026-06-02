// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Deterministic account state, derived solely from [`AccountEvent`]s.
//!
//! Per decision D5 there is no synchronous account query anywhere; the portfolio
//! is rebuilt from the same event stream in backtest and live, so it cannot
//! diverge between modes. All arithmetic is integer (fixed-point), so the fill
//! log and `PnL` are bit-for-bit reproducible — the basis of the parity
//! golden-master.

use akadro_core::{AccountEvent, ClientOrderId, Cost, InstrumentId, Money, Price, Qty, Side};

/// One recorded fill (the unit the parity golden-master compares). Construct via
/// [`FillRecord::new`]; `#[non_exhaustive]` so fields can be added (with
/// `#[serde(default)]`) without breaking previously-saved reports or downstream code.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct FillRecord {
    /// Order that produced this fill.
    pub id: ClientOrderId,
    /// Instrument filled.
    pub instrument: InstrumentId,
    /// Side of the fill.
    pub side: Side,
    /// Execution price.
    pub price: Price,
    /// Filled quantity.
    pub qty: Qty,
    /// Total cost (sum of all cost components) charged for this fill.
    pub fee: Money,
    /// Event time of the fill.
    pub ts: akadro_core::Timestamp,
}

impl FillRecord {
    /// Construct a fill record.
    #[must_use]
    pub fn new(
        id: ClientOrderId,
        instrument: InstrumentId,
        side: Side,
        price: Price,
        qty: Qty,
        fee: Money,
        ts: akadro_core::Timestamp,
    ) -> Self {
        FillRecord {
            id,
            instrument,
            side,
            price,
            qty,
            fee,
            ts,
        }
    }
}

/// Net position in one instrument: signed quantity and average entry price.
#[derive(Clone, Copy, Debug, Default)]
struct Position {
    /// Signed net quantity (raw): positive = long, negative = short.
    net: i64,
    /// Average entry price of the open position (`Price::ZERO` when flat).
    avg: Price,
}

impl Position {
    /// Apply a fill, returning realized `PnL` (in `Money` raw units) for any
    /// portion of the position that was closed.
    fn apply_fill(&mut self, side: Side, price: Price, qty: Qty) -> Money {
        debug_assert!(qty.raw() >= 0, "fill qty must be non-negative");
        // Saturating multiply guards the (contract-violating) `i64::MIN` qty; all
        // `.abs()` below widen to i128 FIRST so they cannot panic on `i64::MIN`.
        let signed = side.sign().saturating_mul(qty.raw()); // +buy / -sell
        if signed == 0 {
            return Money::ZERO;
        }
        let pos = self.net;
        let opening_or_increasing = pos == 0 || (pos > 0) == (signed > 0);

        if opening_or_increasing {
            // Weighted-average the entry price over the combined size, rounded to
            // nearest (not truncated) so the average is unbiased and realized PnL
            // does not systematically drift from the cash-based PnL.
            let pos_abs = i128::from(pos).abs();
            let add_abs = i128::from(signed).abs();
            let total_cost =
                pos_abs * i128::from(self.avg.raw()) + add_abs * i128::from(price.raw());
            let total_qty = pos_abs + add_abs;
            // Round half away from zero — symmetric, so the bias does not flip
            // sign for a negative `total_cost` (the truncating-toward-zero `/` plus
            // a `+ half` would round the wrong way for negatives). `total_qty > 0`
            // here (`add_abs > 0` since `signed != 0`).
            let half = total_qty / 2;
            let rounded = if total_cost >= 0 {
                (total_cost + half) / total_qty
            } else {
                (total_cost - half) / total_qty
            };
            self.avg = Price::from_raw(rounded as i64);
            self.net = pos.saturating_add(signed);
            Money::ZERO
        } else {
            // Reducing, closing, or flipping: realize `PnL` on the closed portion.
            let closing = i128::from(signed).abs().min(i128::from(pos).abs());
            let dir = i128::from(pos.signum()); // +1 if was long, -1 if short
            let realized = closing * (i128::from(price.raw()) - i128::from(self.avg.raw())) * dir;
            let new_net = pos.saturating_add(signed);
            self.net = new_net;
            if new_net == 0 {
                self.avg = Price::ZERO;
            } else if (new_net > 0) != (pos > 0) {
                // Flipped past zero: the remainder opens a fresh position here.
                self.avg = price;
            }
            Money::from_raw(realized)
        }
    }
}

/// Account state derived from the [`AccountEvent`] stream.
#[derive(Debug)]
pub(crate) struct Portfolio {
    cash: Money,
    realized: Money,
    /// Costs charged on order fills and liquidation closes (taker/maker/gas/…).
    /// Kept separate from funding so the two are never conflated (i1).
    trading_fees: Money,
    /// Signed net funding flow from `FundingSettlement` (positive = paid,
    /// negative = received). Separate from `trading_fees` so equity reconciles.
    funding_net: Money,
    /// Realized `PnL` attributable specifically to `Liquidation` events (a subset
    /// of `realized`), reported distinctly because liquidation closes are not in
    /// the order-fill log (i2). Non-zero iff a liquidation moved realized `PnL` —
    /// which is exactly the signal the replay oracle gates on (i3).
    liquidation_pnl: Money,
    positions: Vec<Position>,
    /// Instruments that currently hold a non-zero position. Maintained in `settle`
    /// (the sole position-net mutation point), so `mark_to_market` can sum equity
    /// over only the open positions instead of scanning the whole catalog every
    /// bar — O(open) rather than O(`n_instruments`), which matters a lot for a
    /// many-instrument venue catalogue (e.g. ~1200 OKX spot pairs) traded by a
    /// few-instrument strategy. A flat position contributes 0 to equity, so this is
    /// behaviour-identical; the only invariant that matters is "no open position is
    /// missing" (a stale-flat entry would merely add 0).
    held: Vec<InstrumentId>,
    fills: Vec<FillRecord>,
    /// Ids of orders currently working at the venue (accepted, not yet terminal).
    /// Derived purely from the account stream (D5).
    open: Vec<ClientOrderId>,
    /// Last settled funding rate (signed bps) per instrument, from
    /// `AccountEvent::FundingSettlement` — the backward-looking value
    /// `Ctx::current_funding_rate` reads (D5). `None` until the first settlement.
    last_funding_rate: Vec<Option<i64>>,
}

impl Portfolio {
    pub(crate) fn new(n_instruments: usize, initial_cash: Money) -> Self {
        Portfolio {
            cash: initial_cash,
            realized: Money::ZERO,
            trading_fees: Money::ZERO,
            funding_net: Money::ZERO,
            liquidation_pnl: Money::ZERO,
            positions: vec![Position::default(); n_instruments],
            held: Vec::new(),
            fills: Vec::new(),
            open: Vec::new(),
            last_funding_rate: vec![None; n_instruments],
        }
    }

    fn open_order(&mut self, id: ClientOrderId) {
        // Engine ids are unique and each order is accepted exactly once, so push
        // directly. (No `contains` dedup — that would be O(n) per accept, i.e.
        // O(n²) over a run that opens many orders, and is unnecessary given the
        // uniqueness invariant.)
        self.open.push(id);
    }

    fn close_order(&mut self, id: ClientOrderId) {
        self.open.retain(|o| *o != id);
    }

    /// Ids of orders currently working at the venue.
    pub(crate) fn open_order_ids(&self) -> &[ClientOrderId] {
        &self.open
    }

    /// Fold one account event into the running state.
    pub(crate) fn apply(&mut self, event: &AccountEvent) {
        match event {
            AccountEvent::Fill {
                client_order_id,
                instrument,
                side,
                price,
                qty,
                costs,
                complete,
                ts,
            } => {
                let fee = self.settle(*instrument, *side, *price, *qty, costs);
                self.fills.push(FillRecord {
                    id: *client_order_id,
                    instrument: *instrument,
                    side: *side,
                    price: *price,
                    qty: *qty,
                    fee,
                    ts: *ts,
                });
                if *complete {
                    self.close_order(*client_order_id);
                }
            }
            // Order-lifecycle bookkeeping for the open-order set (D5).
            AccountEvent::OrderAccepted { id, .. } => self.open_order(*id),
            AccountEvent::OrderCanceled { id, .. }
            | AccountEvent::OrderRejected { id, .. }
            | AccountEvent::OrderExpired { id, .. } => self.close_order(*id),
            AccountEvent::Liquidation {
                instrument,
                side,
                price,
                qty,
                costs,
                ..
            } => {
                // Forced close: same position/cash/fee accounting as a fill, but
                // it is a distinct event, not recorded in the order-fill log. Track
                // the realized portion separately so it can be reconciled against
                // the fill log, which omits it (i2).
                let before = self.realized;
                self.settle(*instrument, *side, *price, *qty, costs);
                self.liquidation_pnl = self
                    .liquidation_pnl
                    .saturating_add(self.realized.saturating_sub(before));
            }
            AccountEvent::FundingSettlement {
                instrument,
                cost,
                rate,
                ..
            } => {
                // Signed: a positive cost is paid, a negative one received. Funding
                // flows are tracked apart from trading fees (i1).
                self.cash = self.cash.saturating_sub(cost.amount);
                self.funding_net = self.funding_net.saturating_add(cost.amount);
                if let Some(slot) = self.last_funding_rate.get_mut(instrument.index() as usize) {
                    *slot = Some(*rate);
                }
            }
            _ => {}
        }
    }

    /// Apply a trade's position, cash and fee effect; returns the total fee.
    fn settle(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        price: Price,
        qty: Qty,
        costs: &[Cost],
    ) -> Money {
        let fee = costs
            .iter()
            .fold(Money::ZERO, |acc, c| acc.saturating_add(c.amount));
        if let Some(pos) = self.positions.get_mut(instrument.index() as usize) {
            let was_open = pos.net != 0;
            let realized = pos.apply_fill(side, price, qty);
            let now_open = pos.net != 0;
            self.realized = self.realized.saturating_add(realized);
            // Keep the open-position set in step with the net crossing 0 (the only
            // place positions change). `held` may briefly retain a flat instrument
            // between transitions only via this path, which is harmless (adds 0).
            if was_open != now_open {
                if now_open {
                    self.held.push(instrument);
                } else {
                    self.held.retain(|h| *h != instrument);
                }
            }
        }
        // Cash: pay the notional on a buy, receive it on a sell, then fees.
        let notional = price.notional(qty).raw();
        let cash_delta = Money::from_raw(-i128::from(side.sign()) * notional);
        self.cash = self.cash.saturating_add(cash_delta).saturating_sub(fee);
        self.trading_fees = self.trading_fees.saturating_add(fee);
        fee
    }

    pub(crate) fn cash(&self) -> Money {
        self.cash
    }

    pub(crate) fn realized_pnl(&self) -> Money {
        self.realized
    }

    /// Costs charged on order fills and liquidation closes (excludes funding).
    pub(crate) fn trading_fees(&self) -> Money {
        self.trading_fees
    }

    /// Signed net funding flow (positive = paid, negative = received).
    pub(crate) fn funding_net(&self) -> Money {
        self.funding_net
    }

    /// Realized `PnL` from liquidation events only (a subset of `realized_pnl`),
    /// not present in the order-fill log.
    pub(crate) fn liquidation_pnl(&self) -> Money {
        self.liquidation_pnl
    }

    /// Total costs charged over the run: trading fees plus net funding.
    pub(crate) fn net_costs(&self) -> Money {
        self.trading_fees.saturating_add(self.funding_net)
    }

    /// Instruments that currently hold a (non-zero) position — the set
    /// `mark_to_market` sums over. May transiently include a just-flattened
    /// instrument; that is harmless (it marks to 0).
    pub(crate) fn open_positions(&self) -> &[InstrumentId] {
        &self.held
    }

    pub(crate) fn net_qty(&self, instrument: InstrumentId) -> Qty {
        self.positions
            .get(instrument.index() as usize)
            .map_or(Qty::ZERO, |p| Qty::from_raw(p.net))
    }

    pub(crate) fn avg_entry(&self, instrument: InstrumentId) -> Price {
        self.positions
            .get(instrument.index() as usize)
            .map_or(Price::ZERO, |p| p.avg)
    }

    pub(crate) fn fills(&self) -> &[FillRecord] {
        &self.fills
    }

    /// Last settled funding rate (signed bps) for `instrument`, or `None` if no
    /// funding has settled (or the id is out of range).
    pub(crate) fn last_funding_rate(&self, instrument: InstrumentId) -> Option<i64> {
        self.last_funding_rate
            .get(instrument.index() as usize)
            .copied()
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{Cost, CostKind, Costs, Timestamp};

    const I: InstrumentId = InstrumentId::new(0);

    fn fill(side: Side, price: i64, qty: i64, fee: i128) -> AccountEvent {
        let mut costs = Costs::new();
        if fee != 0 {
            costs.push(Cost::new(
                akadro_core::AssetId::new(0),
                Money::from_raw(fee),
                CostKind::Taker,
            ));
        }
        AccountEvent::Fill {
            client_order_id: ClientOrderId::new(1),
            instrument: I,
            side,
            price: Price::from_raw(price),
            qty: Qty::from_raw(qty),
            costs,
            complete: true,
            ts: Timestamp::from_nanos(1),
        }
    }

    #[test]
    fn buy_then_sell_realizes_pnl() {
        let mut p = Portfolio::new(1, Money::from_raw(1_000_000));
        p.apply(&fill(Side::Buy, 100, 10, 0)); // buy 10 @ 100 -> cash -1000
        assert_eq!(p.net_qty(I), Qty::from_raw(10));
        assert_eq!(p.avg_entry(I), Price::from_raw(100));
        assert_eq!(p.cash(), Money::from_raw(1_000_000 - 1000));

        p.apply(&fill(Side::Sell, 110, 10, 0)); // sell 10 @ 110 -> +1100, realize +100
        assert_eq!(p.net_qty(I), Qty::ZERO);
        assert_eq!(p.avg_entry(I), Price::ZERO);
        assert_eq!(p.realized_pnl(), Money::from_raw((110 - 100) * 10)); // 10 units * (110-100) = 100
        assert_eq!(p.cash(), Money::from_raw(1_000_000 - 1000 + 1100));
        assert_eq!(p.fills().len(), 2);
    }

    #[test]
    fn short_then_cover() {
        let mut p = Portfolio::new(1, Money::ZERO);
        p.apply(&fill(Side::Sell, 100, 5, 0)); // short 5 @ 100
        assert_eq!(p.net_qty(I), Qty::from_raw(-5));
        assert_eq!(p.avg_entry(I), Price::from_raw(100));
        p.apply(&fill(Side::Buy, 90, 5, 0)); // cover @ 90 -> profit (100-90)*5 = 50
        assert_eq!(p.net_qty(I), Qty::ZERO);
        assert_eq!(p.realized_pnl(), Money::from_raw(50));
    }

    #[test]
    fn increasing_position_averages_entry() {
        let mut p = Portfolio::new(1, Money::ZERO);
        p.apply(&fill(Side::Buy, 100, 10, 0));
        p.apply(&fill(Side::Buy, 120, 10, 0)); // avg = (1000+1200)/20 = 110
        assert_eq!(p.net_qty(I), Qty::from_raw(20));
        assert_eq!(p.avg_entry(I), Price::from_raw(110));
        assert_eq!(p.realized_pnl(), Money::ZERO);
    }

    #[test]
    fn flip_long_to_short() {
        let mut p = Portfolio::new(1, Money::ZERO);
        p.apply(&fill(Side::Buy, 100, 5, 0)); // long 5 @ 100
        p.apply(&fill(Side::Sell, 120, 8, 0)); // sell 8: close 5 (realize +100), flip to short 3 @ 120
        assert_eq!(p.net_qty(I), Qty::from_raw(-3));
        assert_eq!(p.avg_entry(I), Price::from_raw(120));
        assert_eq!(p.realized_pnl(), Money::from_raw((120 - 100) * 5));
    }

    #[test]
    fn fees_reduce_cash_and_are_tracked() {
        let mut p = Portfolio::new(1, Money::from_raw(10_000));
        p.apply(&fill(Side::Buy, 100, 10, 5)); // notional 1000, fee 5
        assert_eq!(p.cash(), Money::from_raw(10_000 - 1000 - 5));
        assert_eq!(p.trading_fees(), Money::from_raw(5));
        assert_eq!(p.funding_net(), Money::ZERO);
    }

    #[test]
    fn zero_qty_fill_is_noop_on_position() {
        let mut pos = Position::default();
        assert_eq!(
            pos.apply_fill(Side::Buy, Price::from_raw(100), Qty::ZERO),
            Money::ZERO
        );
        assert_eq!(pos.net, 0);
    }

    #[test]
    fn non_fill_events_ignored() {
        let mut p = Portfolio::new(1, Money::from_raw(7));
        p.apply(&AccountEvent::OrderAccepted {
            id: ClientOrderId::new(1),
            ts: Timestamp::from_nanos(1),
        });
        p.apply(&AccountEvent::Resync {
            instrument: None,
            ts: Timestamp::from_nanos(1),
        });
        assert_eq!(p.cash(), Money::from_raw(7));
        assert_eq!(p.fills().len(), 0);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            // Conservation: the net position must equal the signed sum of all
            // fills, no matter the order or sign of the sequence.
            #[test]
            fn net_qty_is_signed_sum_of_fills(
                fills in proptest::collection::vec((any::<bool>(), 1i64..1_000, 1i64..100), 0..60)
            ) {
                let mut p = Portfolio::new(1, Money::from_raw(1_000_000_000));
                let mut expected: i64 = 0;
                for (is_buy, price, qty) in &fills {
                    let side = if *is_buy { Side::Buy } else { Side::Sell };
                    expected += side.sign() * qty;
                    p.apply(&fill(side, *price, *qty, 0));
                }
                prop_assert_eq!(p.net_qty(I).raw(), expected);
                prop_assert_eq!(p.fills().len(), fills.len());
            }

            // A flat round-trip (buy q @ p then sell q @ p) realizes zero PnL and
            // returns to the starting cash.
            #[test]
            fn flat_round_trip_is_pnl_neutral(price in 1i64..10_000, qty in 1i64..1_000) {
                let mut p = Portfolio::new(1, Money::ZERO);
                p.apply(&fill(Side::Buy, price, qty, 0));
                p.apply(&fill(Side::Sell, price, qty, 0));
                prop_assert_eq!(p.net_qty(I).raw(), 0);
                prop_assert_eq!(p.realized_pnl(), Money::ZERO);
                prop_assert_eq!(p.cash(), Money::ZERO);
            }
        }
    }

    #[test]
    fn fill_for_unknown_instrument_updates_cash_only() {
        // Defensive: a fill referencing an out-of-range instrument still keeps
        // cash accounting consistent without panicking.
        let mut p = Portfolio::new(1, Money::from_raw(1_000));
        let ev = AccountEvent::Fill {
            client_order_id: ClientOrderId::new(1),
            instrument: InstrumentId::new(7),
            side: Side::Buy,
            price: Price::from_raw(10),
            qty: Qty::from_raw(1),
            costs: Costs::new(),
            complete: true,
            ts: Timestamp::from_nanos(1),
        };
        p.apply(&ev);
        assert_eq!(p.cash(), Money::from_raw(990));
        assert_eq!(p.net_qty(InstrumentId::new(7)), Qty::ZERO);
    }

    #[test]
    fn funding_and_liquidation_events_update_state() {
        let mut p = Portfolio::new(1, Money::from_raw(10_000));
        // Open a long 10 @ 100 (cash -1000).
        p.apply(&fill(Side::Buy, 100, 10, 0));
        assert_eq!(p.cash(), Money::from_raw(9_000));
        // Funding: a +5 cost is paid (cash down, counted in fees); not a fill.
        p.apply(&AccountEvent::FundingSettlement {
            instrument: I,
            cost: Cost::new(
                akadro_core::AssetId::new(0),
                Money::from_raw(5),
                CostKind::Funding,
            ),
            rate: 0,
            ts: Timestamp::from_nanos(2),
        });
        assert_eq!(p.cash(), Money::from_raw(8_995));
        // Funding flows into funding_net (signed), not trading_fees (i1).
        assert_eq!(p.funding_net(), Money::from_raw(5));
        assert_eq!(p.trading_fees(), Money::ZERO);
        assert_eq!(p.fills().len(), 1); // funding is not an order fill
        // Liquidation force-closes the long at 110: realizes +100, not in the fill log.
        p.apply(&AccountEvent::Liquidation {
            instrument: I,
            side: Side::Sell,
            price: Price::from_raw(110),
            qty: Qty::from_raw(10),
            costs: Costs::new(),
            ts: Timestamp::from_nanos(3),
        });
        assert_eq!(p.net_qty(I), Qty::ZERO);
        assert_eq!(p.realized_pnl(), Money::from_raw(100));
        assert_eq!(p.fills().len(), 1); // liquidation is reported distinctly
        // The liquidation's realized PnL is tracked separately (i2/i3).
        assert_eq!(p.liquidation_pnl(), Money::from_raw(100));
    }

    #[test]
    fn negative_price_average_rounds_symmetrically() {
        // m25: with a negative price the weighted-average rounding must not flip
        // bias. Buy 3 @ -10 then 1 @ -11: total_cost = -41, total_qty = 4,
        // round-half-away-from-zero -> (-41 - 2)/4 = -43/4 = -10 (toward zero from
        // -10.75 is -10; away-from-zero rounding of -10.25 avg → -10). Verify it is
        // symmetric with the positive mirror.
        let mut neg = Position::default();
        neg.apply_fill(Side::Buy, Price::from_raw(-10), Qty::from_raw(3));
        neg.apply_fill(Side::Buy, Price::from_raw(-11), Qty::from_raw(1));
        let mut pos = Position::default();
        pos.apply_fill(Side::Buy, Price::from_raw(10), Qty::from_raw(3));
        pos.apply_fill(Side::Buy, Price::from_raw(11), Qty::from_raw(1));
        assert_eq!(neg.avg.raw(), -pos.avg.raw());
    }
}
