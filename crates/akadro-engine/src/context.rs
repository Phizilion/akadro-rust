// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The strategy context — the only thing a strategy is ever handed.
//!
//! [`Ctx`] is the keystone of the kill feature. It is branded with an invariant
//! per-call lifetime `'bar`, has only private fields, and is constructed *only*
//! by the engine in this crate (its constructor is `pub(crate)`, and the engine
//! loop is co-located here, so no other crate — not even a downstream user
//! crate — can forge one; decision D1). Through `Ctx` a strategy can:
//!
//! * read present and past market data ([`Ctx::closes`] etc.) — never the future;
//! * read its own account state ([`Ctx::cash`], [`Ctx::net_qty`]);
//! * submit orders ([`Ctx::submit`]);
//! * read the logical event time ([`Ctx::now`]).
//!
//! It cannot reach the dataset, the wall clock, randomness, or anything that
//! would let it look ahead or tell backtest from live.

use core::marker::PhantomData;
use std::collections::HashMap;

use akadro_core::{
    ClientOrderId, InstrumentId, InstrumentSpec, Money, OrderRequest, PlacedOrder, Price, Qty,
    Side, Timestamp, VenueCommand,
};

use crate::brand::Brand;
use crate::market::MarketView;
use crate::portfolio::{FillRecord, Portfolio};
use crate::series::Series;

/// One buffered strategy action, drained and routed to the execution client in
/// submission order.
#[derive(Debug)]
pub(crate) enum GateAction {
    /// Place a new order.
    Submit(PlacedOrder),
    /// Cancel a previously-submitted order by id.
    Cancel(ClientOrderId),
    /// A non-order venue command (leverage, margin mode, approvals — decision D9).
    Command(VenueCommand),
}

/// Buffers the actions a strategy takes during a handler and assigns each new
/// order a deterministic, monotonic [`ClientOrderId`]. The counter persists
/// across the whole run so ids are identical in backtest and live.
#[derive(Debug)]
pub(crate) struct OrderGate {
    next_id: u64,
    pending: Vec<GateAction>,
    /// Every order submitted this run, keyed by id for O(1) `Ctx::submitted_order`
    /// (fill-to-order correlation) — a map, not a scan, so `has_open_order` /
    /// `open_order_ids` stay O(open) rather than O(open × total) in a long session
    /// (m23). Only ever point-inserted and point-read, so its (nondeterministic)
    /// iteration order never reaches strategy-visible behaviour or parity.
    /// Locally generated, so it never queries the venue.
    submitted: HashMap<ClientOrderId, PlacedOrder>,
    /// Pending event-time timers (fire times), drained as the event clock passes.
    timers: Vec<Timestamp>,
    /// Strategy-emitted equity-curve annotations `(event_time, label)`, collected
    /// into `RunReport::annotations`. Observational only — never routed to the
    /// venue, so they do not affect fills or parity.
    annotations: Vec<(Timestamp, String)>,
}

impl OrderGate {
    pub(crate) fn new() -> Self {
        OrderGate {
            next_id: 0,
            pending: Vec::new(),
            submitted: HashMap::new(),
            timers: Vec::new(),
            annotations: Vec::new(),
        }
    }

    fn submit(&mut self, request: OrderRequest) -> ClientOrderId {
        let id = ClientOrderId::new(self.next_id);
        self.next_id += 1;
        self.submitted.insert(id, PlacedOrder { id, request });
        self.pending
            .push(GateAction::Submit(PlacedOrder { id, request }));
        id
    }

    /// The order submitted under `id`, if any (locally-recorded submissions only —
    /// no synchronous venue query, so this preserves parity, D5).
    fn submitted_order(&self, id: ClientOrderId) -> Option<&PlacedOrder> {
        self.submitted.get(&id)
    }

    /// `true` if a submit for an order on `instrument` is queued this handler but
    /// not yet routed/acked (it sits in `pending`, so the portfolio's open set does
    /// not know about it yet). Lets `has_open_order` see an order placed earlier in
    /// the same bar / by an earlier same-bar timer firing, preventing accidental
    /// double-submission (m22).
    fn has_pending_submit_on(&self, instrument: InstrumentId) -> bool {
        self.pending
            .iter()
            .any(|a| matches!(a, GateAction::Submit(p) if p.request.instrument == instrument))
    }

    /// Ids of orders submitted this handler but not yet routed/acked, on
    /// `instrument`, in submission order (m22).
    fn pending_submit_ids_on(
        &self,
        instrument: InstrumentId,
    ) -> impl Iterator<Item = ClientOrderId> + '_ {
        self.pending.iter().filter_map(move |a| match a {
            GateAction::Submit(p) if p.request.instrument == instrument => Some(p.id),
            _ => None,
        })
    }

    fn cancel(&mut self, id: ClientOrderId) {
        self.pending.push(GateAction::Cancel(id));
    }

    fn command(&mut self, command: VenueCommand) {
        self.pending.push(GateAction::Command(command));
    }

    fn schedule(&mut self, at: Timestamp) {
        self.timers.push(at);
    }

    fn annotate(&mut self, at: Timestamp, label: String) {
        self.annotations.push((at, label));
    }

    /// Remove and return every annotation emitted so far this run, in order.
    pub(crate) fn take_annotations(&mut self) -> Vec<(Timestamp, String)> {
        core::mem::take(&mut self.annotations)
    }

    /// Remove and return every timer due at or before `now`, in event-time order
    /// (the engine fires `on_timer` for each, before `on_bar`). Deterministic, so
    /// it behaves identically in backtest and live.
    pub(crate) fn drain_due_timers(&mut self, now: Timestamp) -> Vec<Timestamp> {
        let mut due: Vec<Timestamp> = self
            .timers
            .iter()
            .copied()
            .filter(|t| t.as_nanos() <= now.as_nanos())
            .collect();
        self.timers.retain(|t| t.as_nanos() > now.as_nanos());
        due.sort_by_key(|t| t.as_nanos());
        due
    }

    /// Remove and return every action queued since the last drain.
    pub(crate) fn take_pending(&mut self) -> Vec<GateAction> {
        core::mem::take(&mut self.pending)
    }

    /// Total number of orders submitted so far this run.
    pub(crate) fn submitted_count(&self) -> u64 {
        self.next_id
    }
}

/// The look-ahead-safe context handed to every [`Strategy`](crate::Strategy)
/// callback. See the module docs for the guarantees.
pub struct Ctx<'bar> {
    market: MarketView<'bar>,
    gate: &'bar mut OrderGate,
    portfolio: &'bar Portfolio,
    specs: &'bar [InstrumentSpec],
    now: Timestamp,
    // Load-bearing: the invariant `Brand<'bar>` is what makes `Ctx` un-stashable
    // (the kill feature, layer 3). `gate: &'bar mut` already forces invariance
    // here, but `_brand` is the *sole* invariance source on `MarketView`/`Series`,
    // and keeping it on `Ctx` too is deliberate defense-in-depth — do NOT remove it
    // in a refactor (its absence would silently weaken the look-ahead guarantee).
    _brand: Brand<'bar>,
}

impl<'bar> Ctx<'bar> {
    pub(crate) fn new(
        market: MarketView<'bar>,
        gate: &'bar mut OrderGate,
        portfolio: &'bar Portfolio,
        specs: &'bar [InstrumentSpec],
        now: Timestamp,
    ) -> Self {
        Ctx {
            market,
            gate,
            portfolio,
            specs,
            now,
            _brand: PhantomData,
        }
    }

    /// The logical event time of the current handler invocation. Identical in
    /// backtest and live; two calls within one handler return the same value.
    #[inline]
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// Close-price [`Series`] for `instrument` (present and past only), or
    /// `None` if the instrument is unknown / has no data yet.
    #[inline]
    pub fn closes(&self, instrument: InstrumentId) -> Option<Series<'bar, Price>> {
        self.market.closes(instrument)
    }

    /// Open-price series.
    #[inline]
    pub fn opens(&self, instrument: InstrumentId) -> Option<Series<'bar, Price>> {
        self.market.opens(instrument)
    }

    /// High-price series.
    #[inline]
    pub fn highs(&self, instrument: InstrumentId) -> Option<Series<'bar, Price>> {
        self.market.highs(instrument)
    }

    /// Low-price series.
    #[inline]
    pub fn lows(&self, instrument: InstrumentId) -> Option<Series<'bar, Price>> {
        self.market.lows(instrument)
    }

    /// Volume series.
    #[inline]
    pub fn volumes(&self, instrument: InstrumentId) -> Option<Series<'bar, Qty>> {
        self.market.volumes(instrument)
    }

    /// Backward-only **auxiliary scalar signal** series for `(instrument,
    /// channel)` — the non-OHLCV channel carrying open interest, long/short ratio,
    /// market-wide liquidations, funding, or arbitrary external scalars (delivered
    /// as [`Event::Signal`](akadro_core::Event)). Returns `None` until a signal on
    /// that channel has been observed. Like the price series it is look-ahead-safe:
    /// only `latest`/`ago`/`last_n` (the future is inexpressible). Values are the
    /// producing feed's fixed-point `i64`s (per-channel scale).
    #[inline]
    pub fn signal(&self, instrument: InstrumentId, channel: u16) -> Option<Series<'bar, i64>> {
        self.market.signal(instrument, channel)
    }

    /// Open-interest series for `instrument` — a [`signal`](Self::signal)
    /// convenience on the well-known
    /// [`signal_channel::OPEN_INTEREST`](akadro_core::signal_channel::OPEN_INTEREST)
    /// channel. `None` until an open-interest feed has been observed.
    #[inline]
    pub fn open_interest(&self, instrument: InstrumentId) -> Option<Series<'bar, i64>> {
        self.market
            .signal(instrument, akadro_core::signal_channel::OPEN_INTEREST)
    }

    /// Long/short-ratio series for `instrument` (the
    /// [`signal_channel::LONG_SHORT_RATIO`](akadro_core::signal_channel::LONG_SHORT_RATIO)
    /// channel). `None` until an LSR feed has been observed.
    #[inline]
    pub fn long_short_ratio(&self, instrument: InstrumentId) -> Option<Series<'bar, i64>> {
        self.market
            .signal(instrument, akadro_core::signal_channel::LONG_SHORT_RATIO)
    }

    /// Market-wide liquidation-volume series for `instrument` (the
    /// [`signal_channel::LIQUIDATIONS`](akadro_core::signal_channel::LIQUIDATIONS)
    /// channel). `None` until a liquidation feed has been observed.
    #[inline]
    pub fn market_liquidations(&self, instrument: InstrumentId) -> Option<Series<'bar, i64>> {
        self.market
            .signal(instrument, akadro_core::signal_channel::LIQUIDATIONS)
    }

    /// Number of bars observed so far for `instrument`.
    #[inline]
    pub fn bar_count(&self, instrument: InstrumentId) -> usize {
        self.market.bar_count(instrument)
    }

    /// Submit an order. Returns the engine-assigned [`ClientOrderId`]; the
    /// acknowledgement and any fills arrive later as account events.
    #[inline]
    pub fn submit(&mut self, order: OrderRequest) -> ClientOrderId {
        self.gate.submit(order)
    }

    /// Request cancellation of a previously-submitted order. The resulting
    /// [`AccountEvent::OrderCanceled`](akadro_core::AccountEvent) — or an
    /// [`AccountEvent::OrderCancelRejected`](akadro_core::AccountEvent) if it is
    /// unknown / already terminal — arrives later on the account stream.
    ///
    /// Live caveat: a `Fill` may still arrive *after* this call for a cancel that
    /// races an in-flight execution; treat the cancel as a request, not a
    /// guarantee, and reconcile against the account stream.
    #[inline]
    pub fn cancel(&mut self, id: ClientOrderId) {
        self.gate.cancel(id);
    }

    /// Replace a working order: cancel `old` and submit `new`, returning the new
    /// [`ClientOrderId`]. Both actions are buffered this handler and routed
    /// together, so from the strategy's view it is one step. The idiomatic way to
    /// run a **per-bar dynamic trailing stop**: recompute the stop from an
    /// indicator (e.g. ATR) each bar and `ctx.replace_order(prev_stop, new_stop)`.
    ///
    /// Live caveat: as with [`cancel`](Self::cancel), a `Fill` may race the cancel;
    /// reconcile against the account stream rather than assuming the old order is
    /// gone.
    #[inline]
    pub fn replace_order(&mut self, old: ClientOrderId, new: OrderRequest) -> ClientOrderId {
        self.gate.cancel(old);
        self.gate.submit(new)
    }

    /// Issue a non-order venue command (leverage, margin mode, token approval —
    /// decision D9). It is routed to the execution client like an order.
    #[inline]
    pub fn command(&mut self, command: VenueCommand) {
        self.gate.command(command);
    }

    /// Schedule a one-shot event-time timer: [`Strategy::on_timer`](crate::Strategy)
    /// fires with `at` once the event clock reaches it (at the first event whose
    /// time is `>= at`; a time already in the past fires at the next event). Uses
    /// logical event time only, so it is deterministic and behaves identically in
    /// backtest and live.
    #[inline]
    pub fn schedule(&mut self, at: Timestamp) {
        self.gate.schedule(at);
    }

    /// Annotate the equity curve at the current event time with a free-form
    /// `label` (e.g. a detected regime name). Annotations are collected, in
    /// occurrence order, into [`RunReport::annotations`](crate::RunReport) for
    /// later plotting/analysis. Purely observational: it places no order and does
    /// not affect determinism, fills, or backtest↔live parity.
    #[inline]
    pub fn annotate(&mut self, label: impl Into<String>) {
        self.gate.annotate(self.now, label.into());
    }

    /// Rebalance toward a set of target **net** positions. For each
    /// `(instrument, target)` it submits a market order for the delta
    /// `target − net_qty(instrument)` (buy if positive, sell if negative).
    /// Instruments already at their target are skipped.
    ///
    /// An order that moves a position *toward zero without flipping its sign* is
    /// marked [`reduce_only`](OrderRequest::reduce_only); a sign-flip is sent as a
    /// single plain market order for the full delta. Returns the submitted order
    /// ids (in input order, skipping no-ops). A thin convenience for
    /// cross-sectional / portfolio strategies — equivalent to computing the deltas
    /// and calling [`submit`](Self::submit) yourself. Deltas widen to `i128`
    /// before `abs` (no overflow), so it is safe at extreme position sizes.
    pub fn rebalance(&mut self, targets: &[(InstrumentId, Qty)]) -> Vec<ClientOrderId> {
        let mut ids = Vec::with_capacity(targets.len());
        for &(inst, target) in targets {
            let cur = self.net_qty(inst).raw();
            let tgt = target.raw();
            let delta = i128::from(tgt) - i128::from(cur);
            if delta == 0 {
                continue;
            }
            let side = if delta > 0 { Side::Buy } else { Side::Sell };
            let qty = Qty::from_raw(i64::try_from(delta.unsigned_abs()).unwrap_or(i64::MAX));
            // Toward zero without flipping sign → reduce_only.
            let reduces = (cur > 0 && delta < 0 && tgt >= 0) || (cur < 0 && delta > 0 && tgt <= 0);
            let mut req = OrderRequest::market(inst, side, qty);
            if reduces {
                req = req.reduce_only();
            }
            ids.push(self.submit(req));
        }
        ids
    }

    /// The most recent `n` fills for this run (oldest first, newest last) — a
    /// backward-only window over realized executions. Combine with
    /// `TradeStats::from_fills` (from `akadro-analytics`) for **rolling** trade
    /// performance — e.g. Kelly sizing off recent win-rate, with
    /// `TradeStats::from_fills(ctx.recent_fills(50))`. Returns every fill when
    /// fewer than `n` exist, and an empty slice when there are none.
    #[inline]
    #[must_use]
    pub fn recent_fills(&self, n: usize) -> &[FillRecord] {
        let f = self.portfolio.fills();
        &f[f.len().saturating_sub(n)..]
    }

    /// Signed net position in `instrument` (positive long, negative short).
    #[inline]
    pub fn net_qty(&self, instrument: InstrumentId) -> Qty {
        self.portfolio.net_qty(instrument)
    }

    /// The most recently **settled** funding rate for `instrument`, signed, at
    /// [`FUNDING_RATE_SCALE`](akadro_core::FUNDING_RATE_SCALE) (`rate · 10⁻⁸`, so
    /// `10_000` is one basis point), from the account-event stream
    /// ([`AccountEvent::FundingSettlement`](akadro_core::AccountEvent)), or `None`
    /// if none has settled yet. This is the last *settled* rate (backward-looking),
    /// so it is look-ahead-safe and identical in backtest and live (D5).
    #[inline]
    #[must_use]
    pub fn current_funding_rate(&self, instrument: InstrumentId) -> Option<i64> {
        self.portfolio.last_funding_rate(instrument)
    }

    /// Average entry price of the open position in `instrument`
    /// ([`Price::ZERO`] when flat). This is the execution-price average
    /// **excluding** fees (the crypto-exchange convention); an Interactive-Brokers
    /// style cost-basis average would add `trading_fees / net_qty`.
    #[inline]
    pub fn avg_entry(&self, instrument: InstrumentId) -> Price {
        self.portfolio.avg_entry(instrument)
    }

    /// Current quote-asset cash balance.
    #[inline]
    pub fn cash(&self) -> Money {
        self.portfolio.cash()
    }

    /// Running realized `PnL`, **gross of fees** (closed-position `PnL` only). For
    /// the after-fees figure use [`Ctx::realized_pnl_net`].
    #[inline]
    pub fn realized_pnl(&self) -> Money {
        self.portfolio.realized_pnl()
    }

    /// Running realized `PnL` **net of all costs**
    /// (`realized_pnl − trading_fees − funding_net`), matching the per-trade `PnL`
    /// convention `akadro-analytics` reports.
    #[inline]
    pub fn realized_pnl_net(&self) -> Money {
        self.portfolio
            .realized_pnl()
            .saturating_sub(self.portfolio.net_costs())
    }

    /// Unrealized (mark-to-market) `PnL` of the open position in `instrument`:
    /// `(latest_close − avg_entry) · net_qty`. `Money::ZERO` when flat, or when no
    /// bar has been observed for the instrument yet.
    #[must_use]
    pub fn unrealized_pnl(&self, instrument: InstrumentId) -> Money {
        let net = self.portfolio.net_qty(instrument).raw();
        if net == 0 {
            return Money::ZERO;
        }
        match self.market.closes(instrument).and_then(|s| s.latest()) {
            Some(mark) => {
                let avg = self.portfolio.avg_entry(instrument).raw();
                Money::from_raw(i128::from(net) * (i128::from(mark.raw()) - i128::from(avg)))
            }
            None => Money::ZERO,
        }
    }

    /// Total mark-to-market account equity: `cash + Σ (net_qty · latest_close)`
    /// over all instruments — the same value the engine samples into the equity
    /// curve. (Realized `PnL` and fees are already reflected in `cash`, so this is
    /// *not* `cash + realized + Σ unrealized`: those would double-count realized by
    /// `avg_entry · net_qty`. Use [`Ctx::cash`] + [`Ctx::unrealized_pnl`] if you
    /// need the components.)
    #[must_use]
    pub fn equity(&self) -> Money {
        let mut eq = self.portfolio.cash();
        for (i, _) in self.specs.iter().enumerate() {
            let inst = InstrumentId::new(i as u32);
            let net = self.portfolio.net_qty(inst).raw();
            if net != 0
                && let Some(mark) = self.market.closes(inst).and_then(|s| s.latest())
            {
                eq = eq.saturating_add(Money::from_raw(i128::from(net) * i128::from(mark.raw())));
            }
        }
        eq
    }

    /// The [`InstrumentSpec`] for `instrument` (tick size, lot size, caps, …), or
    /// `None` if the id is out of range. Lets a strategy snap prices to the tick
    /// grid before submitting.
    #[inline]
    #[must_use]
    pub fn instrument_spec(&self, instrument: InstrumentId) -> Option<&InstrumentSpec> {
        self.specs.get(instrument.index() as usize)
    }

    /// `true` if the strategy still has at least one working (open) order on
    /// `instrument`. Tracked from the account stream (accepted → open; complete
    /// fill / cancel / reject / expiry → closed), so no synchronous venue query. It
    /// also counts an order submitted earlier in the *current* handler that has not
    /// yet been routed/acked (e.g. by an earlier same-bar timer firing), so a
    /// strategy cannot accidentally double-submit before the ack arrives (m22).
    #[must_use]
    pub fn has_open_order(&self, instrument: InstrumentId) -> bool {
        self.portfolio
            .open_order_ids()
            .iter()
            .any(|id| self.order_on(*id, instrument))
            || self.gate.has_pending_submit_on(instrument)
    }

    /// The ids of all working (open) orders on `instrument` — acked working orders
    /// plus any submitted-but-not-yet-routed in the current handler (m22), in id
    /// order.
    #[must_use]
    pub fn open_order_ids(&self, instrument: InstrumentId) -> Vec<ClientOrderId> {
        let mut ids: Vec<ClientOrderId> = self
            .portfolio
            .open_order_ids()
            .iter()
            .copied()
            .filter(|id| self.order_on(*id, instrument))
            .collect();
        ids.extend(self.gate.pending_submit_ids_on(instrument));
        ids
    }

    fn order_on(&self, id: ClientOrderId, instrument: InstrumentId) -> bool {
        self.gate
            .submitted_order(id)
            .is_some_and(|p| p.request.instrument == instrument)
    }

    /// The order submitted under `id`, for fill-to-order correlation
    /// (`event.client_order_id` → the originating request). Locally recorded, so
    /// no venue query and no parity impact.
    #[inline]
    #[must_use]
    pub fn submitted_order(&self, id: ClientOrderId) -> Option<&PlacedOrder> {
        self.gate.submitted_order(id)
    }

    /// `true` once at least `min_bars` bars have been observed for `instrument` —
    /// the recommended indicator warm-up guard (skip trading until your longest
    /// look-back is filled).
    #[inline]
    #[must_use]
    pub fn is_warmed_up(&self, instrument: InstrumentId, min_bars: usize) -> bool {
        self.bar_count(instrument) >= min_bars
    }
}

impl core::fmt::Debug for Ctx<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ctx")
            .field("now", &self.now)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::Side;

    use akadro_core::{AccountEvent, AssetId, Cost, CostKind, Costs};

    #[test]
    fn current_funding_rate_reads_last_settlement() {
        let market = crate::market::Market::with_instruments(2);
        let mut portfolio = Portfolio::new(2, Money::from_raw(1_000_000));
        let i0 = InstrumentId::new(0);
        portfolio.apply(&AccountEvent::FundingSettlement {
            instrument: i0,
            cost: Cost::new(AssetId::new(1), Money::from_raw(5), CostKind::Funding),
            rate: 42,
            ts: Timestamp::from_nanos(1),
        });
        let mut gate = OrderGate::new();
        let specs: [InstrumentSpec; 0] = [];
        let ctx = Ctx::new(
            MarketView::new(&market),
            &mut gate,
            &portfolio,
            &specs,
            Timestamp::from_nanos(2),
        );
        assert_eq!(ctx.current_funding_rate(i0), Some(42)); // last settled rate
        assert_eq!(ctx.current_funding_rate(InstrumentId::new(1)), None); // never settled
        assert_eq!(ctx.current_funding_rate(InstrumentId::new(9)), None); // out of range
    }

    fn seed_fill(p: &mut Portfolio, inst: InstrumentId, side: Side, qty: i64) {
        p.apply(&AccountEvent::Fill {
            client_order_id: ClientOrderId::new(900),
            instrument: inst,
            side,
            price: Price::from_raw(100),
            qty: Qty::from_raw(qty),
            costs: Costs::new(),
            complete: true,
            ts: Timestamp::from_nanos(1),
        });
    }

    #[test]
    fn rebalance_deltas_sides_and_reduce_only() {
        // Five instruments, each seeded to a known net position.
        let market = crate::market::Market::with_instruments(5);
        let mut portfolio = Portfolio::new(5, Money::from_raw(10_000_000));
        let id = |i| InstrumentId::new(i);
        seed_fill(&mut portfolio, id(0), Side::Buy, 10); // long 10  → target 10  (skip)
        seed_fill(&mut portfolio, id(1), Side::Buy, 10); // long 10  → target 4   (reduce)
        seed_fill(&mut portfolio, id(2), Side::Buy, 10); // long 10  → target 15  (increase)
        seed_fill(&mut portfolio, id(3), Side::Buy, 10); // long 10  → target -5  (flip)
        seed_fill(&mut portfolio, id(4), Side::Sell, 10); // short 10 → target -4  (reduce short)

        let mut gate = OrderGate::new();
        let specs: [InstrumentSpec; 0] = [];
        let mut ctx = Ctx::new(
            MarketView::new(&market),
            &mut gate,
            &portfolio,
            &specs,
            Timestamp::from_nanos(2),
        );

        let ids = ctx.rebalance(&[
            (id(0), Qty::from_raw(10)),
            (id(1), Qty::from_raw(4)),
            (id(2), Qty::from_raw(15)),
            (id(3), Qty::from_raw(-5)),
            (id(4), Qty::from_raw(-4)),
        ]);

        // inst 0 was already on target → skipped, so 4 orders.
        assert_eq!(ids.len(), 4);
        let order = |k: usize| &ctx.submitted_order(ids[k]).unwrap().request;

        // inst 1: reduce long 10 → 4: Sell 6, reduce_only.
        assert_eq!(order(0).instrument, id(1));
        assert_eq!(order(0).side, Side::Sell);
        assert_eq!(order(0).qty, Qty::from_raw(6));
        assert!(order(0).reduce_only);
        // inst 2: increase long 10 → 15: Buy 5, NOT reduce_only.
        assert_eq!(order(1).side, Side::Buy);
        assert_eq!(order(1).qty, Qty::from_raw(5));
        assert!(!order(1).reduce_only);
        // inst 3: flip long 10 → -5: Sell 15, NOT reduce_only (crosses zero).
        assert_eq!(order(2).side, Side::Sell);
        assert_eq!(order(2).qty, Qty::from_raw(15));
        assert!(!order(2).reduce_only);
        // inst 4: reduce short -10 → -4: Buy 6, reduce_only.
        assert_eq!(order(3).side, Side::Buy);
        assert_eq!(order(3).qty, Qty::from_raw(6));
        assert!(order(3).reduce_only);
    }

    #[test]
    fn rebalance_empty_and_all_on_target_is_noop() {
        let market = crate::market::Market::with_instruments(1);
        let portfolio = Portfolio::new(1, Money::from_raw(1_000_000)); // flat
        let mut gate = OrderGate::new();
        let specs: [InstrumentSpec; 0] = [];
        let mut ctx = Ctx::new(
            MarketView::new(&market),
            &mut gate,
            &portfolio,
            &specs,
            Timestamp::from_nanos(1),
        );
        assert!(ctx.rebalance(&[]).is_empty());
        // Flat instrument, target flat → no order.
        assert!(
            ctx.rebalance(&[(InstrumentId::new(0), Qty::ZERO)])
                .is_empty()
        );
    }

    #[test]
    fn signal_convenience_accessors_use_well_known_channels() {
        use akadro_core::signal_channel;
        let mut market = crate::market::Market::with_instruments(1);
        let i = InstrumentId::new(0);
        market.push_signal(i, signal_channel::OPEN_INTEREST, 1000);
        market.push_signal(i, signal_channel::LONG_SHORT_RATIO, 150);
        market.push_signal(i, signal_channel::LIQUIDATIONS, 42);
        let portfolio = Portfolio::new(1, Money::from_raw(0));
        let mut gate = OrderGate::new();
        let specs: [InstrumentSpec; 0] = [];
        let ctx = Ctx::new(
            MarketView::new(&market),
            &mut gate,
            &portfolio,
            &specs,
            Timestamp::from_nanos(1),
        );
        assert_eq!(ctx.open_interest(i).unwrap().latest(), Some(1000));
        assert_eq!(ctx.long_short_ratio(i).unwrap().latest(), Some(150));
        assert_eq!(ctx.market_liquidations(i).unwrap().latest(), Some(42));
        // The generic accessor agrees with the convenience ones (same channel).
        assert_eq!(
            ctx.signal(i, signal_channel::OPEN_INTEREST)
                .unwrap()
                .latest(),
            Some(1000)
        );
        assert!(ctx.open_interest(InstrumentId::new(9)).is_none());
    }

    #[test]
    fn recent_fills_is_a_backward_window() {
        let market = crate::market::Market::with_instruments(1);
        let mut portfolio = Portfolio::new(1, Money::from_raw(1_000_000_000));
        let i = InstrumentId::new(0);
        for q in 1..=5 {
            seed_fill(&mut portfolio, i, Side::Buy, q);
        }
        let mut gate = OrderGate::new();
        let specs: [InstrumentSpec; 0] = [];
        let ctx = Ctx::new(
            MarketView::new(&market),
            &mut gate,
            &portfolio,
            &specs,
            Timestamp::from_nanos(1),
        );
        // Last two fills (qty 4 then 5), oldest-first.
        let last2 = ctx.recent_fills(2);
        assert_eq!(last2.len(), 2);
        assert_eq!(last2[0].qty, Qty::from_raw(4));
        assert_eq!(last2[1].qty, Qty::from_raw(5));
        // Fewer than n → all; zero → empty.
        assert_eq!(ctx.recent_fills(100).len(), 5);
        assert!(ctx.recent_fills(0).is_empty());
    }

    #[test]
    fn annotate_records_event_time_label_and_drains() {
        let market = crate::market::Market::with_instruments(1);
        let portfolio = Portfolio::new(1, Money::from_raw(1_000_000));
        let mut gate = OrderGate::new();
        let specs: [InstrumentSpec; 0] = [];
        {
            let mut ctx = Ctx::new(
                MarketView::new(&market),
                &mut gate,
                &portfolio,
                &specs,
                Timestamp::from_nanos(42),
            );
            ctx.annotate("bull"); // &str
            ctx.annotate(String::from("regime-2")); // String
        }
        let ann = gate.take_annotations();
        assert_eq!(
            ann,
            vec![
                (Timestamp::from_nanos(42), "bull".to_string()),
                (Timestamp::from_nanos(42), "regime-2".to_string()),
            ]
        );
        // A second drain is empty (take semantics).
        assert!(gate.take_annotations().is_empty());
    }

    #[test]
    fn replace_order_cancels_old_and_submits_new() {
        let market = crate::market::Market::with_instruments(1);
        let portfolio = Portfolio::new(1, Money::from_raw(1_000_000));
        let mut gate = OrderGate::new();
        let specs: [InstrumentSpec; 0] = [];
        let inst = InstrumentId::new(0);
        let (old, new) = {
            let mut ctx = Ctx::new(
                MarketView::new(&market),
                &mut gate,
                &portfolio,
                &specs,
                Timestamp::from_nanos(1),
            );
            let old = ctx.submit(OrderRequest::market(inst, Side::Buy, Qty::from_raw(1)));
            let new = ctx.replace_order(
                old,
                OrderRequest::market(inst, Side::Sell, Qty::from_raw(2)),
            );
            assert_ne!(old, new);
            // The replacement is recorded and findable by its new id.
            let o = ctx.submitted_order(new).unwrap();
            assert_eq!(o.request.side, Side::Sell);
            assert_eq!(o.request.qty, Qty::from_raw(2));
            (old, new)
        };
        // Buffered, in order: submit(old), cancel(old), submit(new).
        let actions = gate.take_pending();
        assert_eq!(actions.len(), 3);
        assert!(matches!(actions[1], GateAction::Cancel(id) if id == old));
        assert!(matches!(&actions[2], GateAction::Submit(p) if p.id == new));
    }

    #[test]
    fn order_gate_assigns_monotonic_ids() {
        let mut g = OrderGate::new();
        let i = InstrumentId::new(0);
        let id0 = g.submit(OrderRequest::market(i, Side::Buy, Qty::from_raw(1)));
        let id1 = g.submit(OrderRequest::market(i, Side::Sell, Qty::from_raw(1)));
        assert_eq!(id0, ClientOrderId::new(0));
        assert_eq!(id1, ClientOrderId::new(1));
        assert_eq!(g.submitted_count(), 2);
        let drained = g.take_pending();
        assert_eq!(drained.len(), 2);
        // Counter persists; pending is now empty.
        assert!(g.take_pending().is_empty());
        assert_eq!(g.submitted_count(), 2);
    }
}
