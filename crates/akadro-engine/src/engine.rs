// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The engine: a single monomorphized event loop shared by backtest and live.
//!
//! The loop is identical in both modes (goal 2, parity). Only the injected
//! [`DataSource`] and [`ExecutionClient`] differ; the strategy cannot tell which
//! it is running under. The loop is generic over the strategy, data source and
//! execution client, so it monomorphizes with no per-event dynamic dispatch.
//!
//! ## Event ordering (the parity contract)
//!
//! For each incoming event at logical time `t`:
//! 1. the execution client `observe`s the new data and emits fills for orders
//!    that were already resting (a market order submitted on bar *i* fills at
//!    bar *i+1* — never look-ahead);
//! 2. those account events update the portfolio and are delivered to
//!    `on_account`;
//! 3. the observed market state is appended (so a [`Series`](crate::Series) now
//!    includes this bar);
//! 4. `on_bar` runs; any orders it submits are routed and acknowledged.

use core::fmt;

use akadro_core::{
    AccountEvent, AkadroError, DataSource, Event, ExecutionClient, InstrumentSpec, Money, Result,
    Timestamp,
};

use crate::context::{Ctx, GateAction, OrderGate};
use crate::market::{Market, MarketView};
use crate::portfolio::{FillRecord, Portfolio};
use crate::strategy::Strategy;

/// Upper bound on account events processed in a single drain. A normal run never
/// approaches this; exceeding it means a strategy is submitting orders unboundedly
/// in response to its own acks/fills (an infinite loop), which we surface as a
/// panic rather than running out of memory.
const MAX_DRAIN_EVENTS: usize = 1_000_000;

/// One point on the mark-to-market equity curve (account value at a bar close).
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EquityPoint {
    /// Bar-close event time.
    pub ts: Timestamp,
    /// Mark-to-market account value: cash plus the value of open positions at the
    /// latest observed close.
    pub equity: Money,
}

/// The on-disk format version of a persisted [`RunReport`]. Bump on a breaking
/// schema change; [`RunReport::load`] accepts files at this version or older
/// (missing fields take their defaults) and rejects files from a newer format.
pub const REPORT_FORMAT_VERSION: u32 = 2;

/// The deterministic result of a run — comparing two of these byte-for-byte is
/// how the parity golden-master proves backtest and live behave identically.
///
/// It is serializable behind the `serde` feature: [`RunReport::save`] /
/// [`RunReport::load`] persist it as JSON. The format is **backward-compatible**
/// — a report saved by an older build loads into a newer one (fields added later
/// take their defaults via `#[serde(default)]`), and unknown fields from a newer
/// build are ignored.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct RunReport {
    /// On-disk format version (see [`REPORT_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Every fill, in occurrence order.
    pub fills: Vec<FillRecord>,
    /// Mark-to-market equity sampled at each bar close (feeds `akadro-analytics`).
    pub equity_curve: Vec<EquityPoint>,
    /// Final quote-asset cash.
    pub final_cash: Money,
    /// Total realized `PnL` (includes any `liquidation_pnl`).
    pub realized_pnl: Money,
    /// Execution costs charged on order fills and liquidation closes
    /// (taker/maker/gas/settlement) — **excludes** funding (i1). Normally
    /// non-negative; can be negative only with maker rebates.
    pub trading_fees: Money,
    /// Signed net perpetual-funding flow over the run (positive = paid to the
    /// venue, negative = received). Tracked apart from [`Self::trading_fees`] so
    /// equity reconciles and the two are never conflated (i1).
    pub funding_net: Money,
    /// Realized `PnL` attributable to `Liquidation` events (a subset of
    /// [`Self::realized_pnl`]). Liquidation closes are **not** in [`Self::fills`],
    /// so a fills-only analysis must add this back to reconcile (i2).
    pub liquidation_pnl: Money,
    /// Number of orders submitted over the run.
    pub orders_submitted: u64,
    /// Number of bars processed.
    pub bars_processed: u64,
    /// The execution client's deterministic seed, if any — enough (with the
    /// inputs) to reproduce the run from the report alone.
    pub seed: Option<u64>,
    /// Strategy-emitted equity-curve annotations `(event_time, label)`, in
    /// occurrence order (e.g. regime labels), via
    /// [`Ctx::annotate`](crate::Ctx::annotate). Empty when the strategy emits
    /// none; identical across backtest and live for the same source.
    pub annotations: Vec<(Timestamp, String)>,
}

/// The trading engine. Build with [`Engine::new`], then [`Engine::run`].
pub struct Engine<S, D, X> {
    strategy: S,
    source: D,
    exec: X,
    market: Market,
    gate: OrderGate,
    portfolio: Portfolio,
    sink: Vec<AccountEvent>,
    instruments: Vec<InstrumentSpec>,
}

impl<S, D, X> fmt::Debug for Engine<S, D, X> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("instruments", &self.instruments.len())
            .finish_non_exhaustive()
    }
}

impl<S: Strategy, D: DataSource, X: ExecutionClient> Engine<S, D, X> {
    /// Create an engine.
    ///
    /// `instruments` must be densely numbered `0..n` and listed in id order
    /// (the dense ids index the hot-path columnar storage).
    ///
    /// # Errors
    /// Returns [`AkadroError::Config`] if no instruments are given or the ids are
    /// not a dense, in-order `0..n`.
    pub fn new(
        instruments: &[InstrumentSpec],
        initial_cash: Money,
        source: D,
        exec: X,
        strategy: S,
    ) -> Result<Self> {
        if instruments.is_empty() {
            return Err(AkadroError::Config(
                "at least one instrument is required".to_owned(),
            ));
        }
        for (i, spec) in instruments.iter().enumerate() {
            if spec.id.index() as usize != i {
                let msg = format!(
                    "instrument ids must be dense 0..n in order; position {i} has id {:?}",
                    spec.id
                );
                return Err(AkadroError::Config(msg));
            }
        }
        let n = instruments.len();
        Ok(Engine {
            strategy,
            source,
            exec,
            market: Market::with_instruments(n),
            gate: OrderGate::new(),
            portfolio: Portfolio::new(n, initial_cash),
            sink: Vec::new(),
            instruments: instruments.to_vec(),
        })
    }

    /// Number of instruments the engine was configured with.
    pub fn instrument_count(&self) -> usize {
        self.instruments.len()
    }

    /// Drive the strategy to completion and return the deterministic report.
    ///
    /// # Panics
    /// Panics if a strategy submits orders unboundedly in response to its own
    /// account events (an infinite feedback loop), once a single drain exceeds
    /// `MAX_DRAIN_EVENTS` — a programmer error, surfaced rather than hung.
    pub fn run(self) -> RunReport {
        self.run_observed(&mut crate::observer::NoOpObserver)
    }

    /// As [`run`](Self::run) but driving an [`Observer`](crate::Observer) for live
    /// display / telemetry (e.g. a TUI). The observer only **reads** the run, so
    /// the returned report is identical to [`run`](Self::run) — parity and
    /// determinism are unaffected.
    ///
    /// # Panics
    /// Same as [`run`](Self::run).
    // The event loop is intentionally one function: the start/loop/stop phases
    // share borrow-split locals and read more clearly together than split apart.
    #[allow(clippy::too_many_lines)]
    pub fn run_observed<O: crate::observer::Observer>(self, obs: &mut O) -> RunReport {
        let Engine {
            mut strategy,
            mut source,
            mut exec,
            mut market,
            mut gate,
            mut portfolio,
            mut sink,
            instruments,
        } = self;

        let mut now = Timestamp::EPOCH;
        let mut bars_processed: u64 = 0;
        let mut equity_curve: Vec<EquityPoint> = Vec::new();

        {
            let mut ctx = Ctx::new(
                MarketView::new(&market),
                &mut gate,
                &portfolio,
                &instruments,
                now,
            );
            strategy.on_start(&mut ctx);
        }
        route_pending(&mut gate, &mut exec, now, &mut sink);
        drain(
            &mut sink,
            &market,
            &mut gate,
            &mut portfolio,
            &mut strategy,
            &mut exec,
            &instruments,
            now,
        );

        while let Some(event) = source.next_event() {
            now = event.ts();

            // (1) execution reacts to new data; (2) drain the resulting fills.
            exec.observe(&event, now, &mut sink);
            drain(
                &mut sink,
                &market,
                &mut gate,
                &mut portfolio,
                &mut strategy,
                &mut exec,
                &instruments,
                now,
            );

            // `if let` / `else if let` (rather than a `match` with a mandatory
            // `_ =>` arm) keeps the loop exhaustive over the variants we handle
            // without an unreachable catch-all. `Event` is `#[non_exhaustive]`;
            // unknown future variants simply fall through and are ignored.
            if let Event::Bar(bar) = &event {
                let bar = *bar;
                // A bar for an instrument outside the catalog is silently dropped by
                // `Market::push` (documented contract); surface that feed/catalog
                // mismatch in debug so a mis-wired source doesn't vanish quietly (m20).
                debug_assert!(
                    (bar.instrument.index() as usize) < instruments.len(),
                    "feed emitted a bar for instrument {} outside the {}-instrument \
                     catalog — it will be silently ignored",
                    bar.instrument.index(),
                    instruments.len()
                );
                market.push(&bar); // (3) state now includes this bar
                // Fire any event-time timers now due, before on_bar (their orders
                // are routed together with on_bar's below).
                fire_due_timers(
                    &mut strategy,
                    &market,
                    &mut gate,
                    &portfolio,
                    &instruments,
                    now,
                );
                {
                    let mut ctx = Ctx::new(
                        MarketView::new(&market),
                        &mut gate,
                        &portfolio,
                        &instruments,
                        now,
                    );
                    strategy.on_bar(bar, &mut ctx); // (4)
                }
                bars_processed += 1;
                route_pending(&mut gate, &mut exec, now, &mut sink);
                drain(
                    &mut sink,
                    &market,
                    &mut gate,
                    &mut portfolio,
                    &mut strategy,
                    &mut exec,
                    &instruments,
                    now,
                );
                // (5) record mark-to-market equity at this bar close.
                let eq = mark_to_market(&market, &portfolio);
                equity_curve.push(EquityPoint {
                    ts: now,
                    equity: eq,
                });
                // (6) notify the observer (read-only; never affects the run).
                obs.on_bar(bar, eq, portfolio.fills());
            } else if let Event::Resync { instrument, ts } = &event {
                // Forward the resync's instrument scope (m24): a per-instrument
                // resync must not be widened into a venue-wide one downstream.
                let ae = AccountEvent::Resync {
                    instrument: *instrument,
                    ts: *ts,
                };
                portfolio.apply(&ae);
                // Timers due at this resync's time fire before on_account, batched
                // with on_account's orders below (m19/m21).
                fire_due_timers(
                    &mut strategy,
                    &market,
                    &mut gate,
                    &portfolio,
                    &instruments,
                    now,
                );
                {
                    let mut ctx = Ctx::new(
                        MarketView::new(&market),
                        &mut gate,
                        &portfolio,
                        &instruments,
                        now,
                    );
                    strategy.on_account(&ae, &mut ctx);
                }
                route_pending(&mut gate, &mut exec, now, &mut sink);
                drain(
                    &mut sink,
                    &market,
                    &mut gate,
                    &mut portfolio,
                    &mut strategy,
                    &mut exec,
                    &instruments,
                    now,
                );
            } else if let Event::Signal {
                instrument,
                channel,
                value,
                ..
            } = &event
            {
                // Append the auxiliary signal to observed state. Like a bar, this
                // happens before any later strategy call, so `ctx.signal` stays
                // strictly backward-only (look-ahead-safe). Read-side only: it
                // updates observed state and is consulted at the next bar (the
                // bar-driven decision model), so it adds no strategy callback and
                // does not move the equity curve.
                market.push_signal(*instrument, *channel, *value);
                // A signal carries the event clock forward, so any timer now due must
                // still fire (m19/m21) even though a signal adds no strategy callback.
                fire_due_timers(
                    &mut strategy,
                    &market,
                    &mut gate,
                    &portfolio,
                    &instruments,
                    now,
                );
                route_pending(&mut gate, &mut exec, now, &mut sink);
                drain(
                    &mut sink,
                    &market,
                    &mut gate,
                    &mut portfolio,
                    &mut strategy,
                    &mut exec,
                    &instruments,
                    now,
                );
            }
        }

        // Final timer sweep: any timer scheduled for a time at/under the last event
        // (e.g. queued by the last handler) fires before on_stop (m21); its orders are
        // routed by the on_stop route_pending/drain below.
        fire_due_timers(
            &mut strategy,
            &market,
            &mut gate,
            &portfolio,
            &instruments,
            now,
        );
        {
            let mut ctx = Ctx::new(
                MarketView::new(&market),
                &mut gate,
                &portfolio,
                &instruments,
                now,
            );
            strategy.on_stop(&mut ctx);
        }
        route_pending(&mut gate, &mut exec, now, &mut sink);
        drain(
            &mut sink,
            &market,
            &mut gate,
            &mut portfolio,
            &mut strategy,
            &mut exec,
            &instruments,
            now,
        );

        let annotations = gate.take_annotations();
        let report = RunReport {
            format_version: REPORT_FORMAT_VERSION,
            fills: portfolio.fills().to_vec(),
            equity_curve,
            final_cash: portfolio.cash(),
            realized_pnl: portfolio.realized_pnl(),
            trading_fees: portfolio.trading_fees(),
            funding_net: portfolio.funding_net(),
            liquidation_pnl: portfolio.liquidation_pnl(),
            orders_submitted: gate.submitted_count(),
            bars_processed,
            seed: exec.config_seed(),
            annotations,
        };
        obs.on_finish(&report);
        report
    }
}

#[cfg(feature = "serde")]
impl RunReport {
    /// Serialize the report to pretty JSON.
    ///
    /// # Errors
    /// Returns any `serde_json` serialization error.
    pub fn to_json(&self) -> core::result::Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse a report from JSON, rejecting a `format_version` newer than this
    /// build supports (older or unversioned files load fine).
    ///
    /// # Errors
    /// Returns [`std::io::ErrorKind::InvalidData`] on malformed JSON or a
    /// too-new format version.
    pub fn from_json(json: &str) -> std::io::Result<Self> {
        let report: RunReport = serde_json::from_str(json).map_err(invalid_data)?;
        if report.format_version > REPORT_FORMAT_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "run report format v{} is newer than supported v{REPORT_FORMAT_VERSION}",
                    report.format_version
                ),
            ));
        }
        Ok(report)
    }

    /// Save the report to `path` as JSON.
    ///
    /// # Errors
    /// Returns any I/O or serialization error.
    pub fn save(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        std::fs::write(path, self.to_json().map_err(invalid_data)?)
    }

    /// Load a report previously written with [`RunReport::save`].
    ///
    /// # Errors
    /// Returns an I/O error, malformed JSON, or a too-new format version.
    pub fn load(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        Self::from_json(&std::fs::read_to_string(path)?)
    }
}

#[cfg(feature = "serde")]
fn invalid_data(e: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e)
}

/// Mark-to-market account value: cash plus each open position valued at its
/// latest observed close. Used to sample the equity curve at each bar.
///
/// Contract: an `ExecutionClient` must not emit a fill for an instrument before
/// that instrument's first [`Event::Bar`] has been observed. Otherwise the
/// position has no mark and would be silently dropped from equity (understating
/// the curve). A `debug_assert` surfaces the violation; all shipping clients
/// (`SimulatedExchange`, the DEX and MEXC connectors) honour it by only filling
/// against the bar they are observing.
fn mark_to_market(market: &Market, portfolio: &Portfolio) -> Money {
    let view = MarketView::new(market);
    let mut equity = portfolio.cash();
    // Sum unrealized value over only the OPEN positions — O(open), not O(catalog).
    // Scanning every instrument each bar made a few-instrument strategy on a large
    // venue catalogue (e.g. ~1200 OKX spot pairs) pay ~catalog-size cost per bar.
    // A flat position contributes 0, so this is bit-identical to the full scan.
    for &inst in portfolio.open_positions() {
        let qty = portfolio.net_qty(inst).raw();
        if qty != 0 {
            if let Some(mark) = view.closes(inst).and_then(|s| s.latest()) {
                equity = equity
                    .saturating_add(Money::from_raw(i128::from(qty) * i128::from(mark.raw())));
            } else {
                debug_assert!(
                    false,
                    "instrument {} holds a non-zero position but has no observed bar \
                     (an ExecutionClient emitted a fill before the instrument's first bar)",
                    inst.index()
                );
            }
        }
    }
    equity
}

/// Fire `on_timer` for every event-time timer now due at `now`, in event-time
/// order. Called on EVERY event that advances the clock (not just bars) and once
/// more at the loop tail, so a timer scheduled during a stretch of non-bar events
/// (`Signal`/`Resync`) — or for a time at/under the final event — still fires
/// (m19/m21). Does not route the orders it queues; the caller batches them with
/// its own `route_pending` so a timer's orders go out with the event's.
fn fire_due_timers<S: Strategy>(
    strategy: &mut S,
    market: &Market,
    gate: &mut OrderGate,
    portfolio: &Portfolio,
    instruments: &[InstrumentSpec],
    now: Timestamp,
) {
    for at in gate.drain_due_timers(now) {
        let mut ctx = Ctx::new(MarketView::new(market), gate, portfolio, instruments, now);
        strategy.on_timer(at, &mut ctx);
    }
}

/// Route every action queued since the last drain to the execution client, in
/// submission order: order placements, cancellations and venue commands.
fn route_pending<X: ExecutionClient>(
    gate: &mut OrderGate,
    exec: &mut X,
    now: Timestamp,
    sink: &mut Vec<AccountEvent>,
) {
    for action in gate.take_pending() {
        match action {
            GateAction::Submit(placed) => exec.submit(placed.id, placed.request, now, sink),
            GateAction::Cancel(id) => exec.cancel(id, now, sink),
            GateAction::Command(command) => exec.command(command, now, sink),
        }
    }
}

/// Apply every pending account event to the portfolio and deliver it to the
/// strategy, routing any orders the strategy submits in response. New events
/// appended during the loop (e.g. acks) are processed in the same pass.
#[allow(clippy::too_many_arguments)]
fn drain<S: Strategy, X: ExecutionClient>(
    sink: &mut Vec<AccountEvent>,
    market: &Market,
    gate: &mut OrderGate,
    portfolio: &mut Portfolio,
    strategy: &mut S,
    exec: &mut X,
    instruments: &[InstrumentSpec],
    now: Timestamp,
) {
    let mut i = 0;
    while i < sink.len() {
        assert!(
            i < MAX_DRAIN_EVENTS,
            "drain exceeded {MAX_DRAIN_EVENTS} account events in one batch — a strategy is \
             submitting orders unboundedly in response to its own acks/fills"
        );
        let event = sink[i].clone();
        i += 1;
        portfolio.apply(&event);
        {
            let mut ctx = Ctx::new(MarketView::new(market), gate, portfolio, instruments, now);
            // Specific callback first (Nautilus-style specific → generic), then the
            // generic catch-all. All specific hooks default to no-ops.
            match &event {
                AccountEvent::Fill { .. } => strategy.on_fill(&event, &mut ctx),
                AccountEvent::OrderRejected { .. } => strategy.on_order_rejected(&event, &mut ctx),
                AccountEvent::OrderCanceled { .. } => strategy.on_order_canceled(&event, &mut ctx),
                AccountEvent::Liquidation { .. } => strategy.on_liquidation(&event, &mut ctx),
                AccountEvent::FundingSettlement { .. } => strategy.on_funding(&event, &mut ctx),
                _ => {}
            }
            strategy.on_account(&event, &mut ctx);
        }
        route_pending(gate, exec, now, sink);
    }
    sink.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{
        AssetId, Bar, CapSet, ClientOrderId, Cost, CostKind, Costs, InstrumentId, InstrumentKind,
        OrderRequest, Price, Qty, Side,
    };

    fn spec(id: u32) -> InstrumentSpec {
        InstrumentSpec::new(
            InstrumentId::new(id),
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(1),
            Qty::from_raw(1),
            Money::ZERO,
            CapSet::empty(),
        )
    }

    fn bar(inst: u32, ts: i64, close: i64) -> Bar {
        Bar::new(
            InstrumentId::new(inst),
            Timestamp::from_nanos(ts),
            Price::from_raw(close),
            Price::from_raw(close),
            Price::from_raw(close),
            Price::from_raw(close),
            Qty::from_raw(1),
        )
    }

    #[test]
    fn signal_channel_observed_before_on_bar_and_backward_only() {
        use std::cell::RefCell;
        use std::rc::Rc;

        // Records (bar_close, signal_latest, signal_len) at each on_bar.
        type Seen = Rc<RefCell<Vec<(i64, Option<i64>, usize)>>>;
        struct Probe {
            inst: InstrumentId,
            seen: Seen,
        }
        impl Strategy for Probe {
            fn on_bar(&mut self, bar: Bar, ctx: &mut Ctx<'_>) {
                let (latest, len) = match ctx.signal(self.inst, 1) {
                    Some(s) => (s.latest(), s.len()),
                    None => (None, 0),
                };
                self.seen.borrow_mut().push((bar.close.raw(), latest, len));
            }
        }

        let inst = InstrumentId::new(0);
        let sig = |ts: i64, val: i64| Event::Signal {
            instrument: inst,
            channel: 1,
            value: val,
            ts: Timestamp::from_nanos(ts),
        };
        // Feed orders the signal BEFORE the bar at equal ts; bar @3 has no fresh
        // signal, so the latest stays put (strictly backward-only).
        let events = vec![
            sig(1, 10),
            Event::Bar(bar(0, 1, 100)),
            sig(2, 20),
            Event::Bar(bar(0, 2, 200)),
            Event::Bar(bar(0, 3, 300)),
        ];
        let seen = Rc::new(RefCell::new(Vec::new()));
        let report = Engine::new(
            &[spec(0)],
            Money::ZERO,
            VecSource(events.into_iter()),
            MockExec::default(),
            Probe {
                inst,
                seen: Rc::clone(&seen),
            },
        )
        .unwrap()
        .run();

        // @1: signal 10 observed before on_bar (len 1); @2: latest 20 (len 2);
        // @3: no new signal, still 20 (len 2).
        assert_eq!(
            *seen.borrow(),
            vec![(100, Some(10), 1), (200, Some(20), 2), (300, Some(20), 2)]
        );
        // Signals are not bars and do not add equity-curve points.
        assert_eq!(report.bars_processed, 3);
        assert_eq!(report.equity_curve.len(), 3);
    }

    #[test]
    fn run_observed_matches_run_and_drives_observer() {
        struct Flat;
        impl Strategy for Flat {
            fn on_bar(&mut self, _b: Bar, _c: &mut Ctx<'_>) {}
        }
        #[derive(Default)]
        struct Rec {
            bars: usize,
            finished: bool,
            final_bars: u64,
        }
        impl crate::observer::Observer for Rec {
            fn on_bar(&mut self, _bar: Bar, _equity: Money, _fills: &[FillRecord]) {
                self.bars += 1;
            }
            fn on_finish(&mut self, report: &RunReport) {
                self.finished = true;
                self.final_bars = report.bars_processed;
            }
        }

        let events = vec![
            Event::Bar(bar(0, 1, 100)),
            Event::Bar(bar(0, 2, 110)),
            Event::Bar(bar(0, 3, 120)),
        ];
        let make = || {
            Engine::new(
                &[spec(0)],
                Money::from_raw(1000),
                VecSource(events.clone().into_iter()),
                MockExec::default(),
                Flat,
            )
            .unwrap()
        };
        // The observer only reads, so run_observed yields a byte-identical report.
        let plain = make().run();
        let mut obs = Rec::default();
        let observed = make().run_observed(&mut obs);
        assert_eq!(plain, observed); // parity: observation does not change the run
        assert_eq!(obs.bars, 3); // on_bar fired once per bar
        assert!(obs.finished);
        assert_eq!(obs.final_bars, 3);
    }

    /// In-memory data source over a fixed list of events.
    struct VecSource(std::vec::IntoIter<Event>);
    impl DataSource for VecSource {
        fn next_event(&mut self) -> Option<Event> {
            self.0.next()
        }
    }

    /// Minimal execution client: acks on submit, fills resting market orders at
    /// the *next* observed bar's close (conservative next-bar fill), flat fee.
    #[derive(Default)]
    struct MockExec {
        resting: Vec<(ClientOrderId, OrderRequest)>,
    }
    impl ExecutionClient for MockExec {
        fn submit(
            &mut self,
            id: ClientOrderId,
            order: OrderRequest,
            now: Timestamp,
            sink: &mut dyn akadro_core::EventSink,
        ) {
            sink.emit(AccountEvent::OrderAccepted { id, ts: now });
            self.resting.push((id, order));
        }
        fn observe(
            &mut self,
            event: &Event,
            now: Timestamp,
            sink: &mut dyn akadro_core::EventSink,
        ) {
            let Event::Bar(b) = event else { return };
            let resting = std::mem::take(&mut self.resting);
            for (id, order) in resting {
                if order.instrument != b.instrument {
                    self.resting.push((id, order));
                    continue;
                }
                let mut costs = Costs::new();
                costs.push(Cost::new(
                    AssetId::new(1),
                    Money::from_raw(1),
                    CostKind::Taker,
                ));
                sink.emit(AccountEvent::Fill {
                    client_order_id: id,
                    instrument: order.instrument,
                    side: order.side,
                    price: b.close,
                    qty: order.qty,
                    costs,
                    complete: true,
                    ts: now,
                });
            }
        }
    }

    /// Buys once on the first bar it sees.
    #[derive(Default)]
    struct BuyOnce {
        done: bool,
        fills_seen: usize,
    }
    impl Strategy for BuyOnce {
        fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
            if !self.done {
                ctx.submit(OrderRequest::market(
                    InstrumentId::new(0),
                    Side::Buy,
                    Qty::from_raw(2),
                ));
                self.done = true;
            }
        }
        fn on_account(&mut self, event: &AccountEvent, _ctx: &mut Ctx<'_>) {
            if matches!(event, AccountEvent::Fill { .. }) {
                self.fills_seen += 1;
            }
        }
    }

    #[test]
    fn ctx_api_state_is_recorded() {
        // A probe whose recorded fields we read after the run (Strategy is moved
        // into the engine, so we use a shared cell).
        use std::cell::RefCell;
        use std::rc::Rc;
        #[derive(Default)]
        struct Rec {
            timer_ats: Vec<i64>,
            fills: usize,
            unrealized_end: i128,
            spec_tick: i64,
        }
        struct Probe {
            submitted: bool,
            rec: Rc<RefCell<Rec>>,
        }
        impl Strategy for Probe {
            fn on_start(&mut self, ctx: &mut Ctx<'_>) {
                ctx.schedule(Timestamp::from_nanos(15));
            }
            fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
                let inst = InstrumentId::new(0);
                if !self.submitted {
                    ctx.submit(OrderRequest::market(inst, Side::Buy, Qty::from_raw(2)));
                    self.submitted = true;
                }
                let mut r = self.rec.borrow_mut();
                r.spec_tick = ctx.instrument_spec(inst).map_or(0, |s| s.tick_size.raw());
                r.unrealized_end = ctx.unrealized_pnl(inst).raw();
            }
            fn on_fill(&mut self, _e: &AccountEvent, _ctx: &mut Ctx<'_>) {
                self.rec.borrow_mut().fills += 1;
            }
            fn on_timer(&mut self, at: Timestamp, _ctx: &mut Ctx<'_>) {
                self.rec.borrow_mut().timer_ats.push(at.as_nanos());
            }
        }
        let rec = Rc::new(RefCell::new(Rec::default()));
        let feed = VecSource(
            vec![
                Event::Bar(bar(0, 10, 100)),
                Event::Bar(bar(0, 20, 110)),
                Event::Bar(bar(0, 30, 120)),
            ]
            .into_iter(),
        );
        Engine::new(
            &[spec(0)],
            Money::from_raw(100_000),
            feed,
            MockExec::default(),
            Probe {
                submitted: false,
                rec: rec.clone(),
            },
        )
        .unwrap()
        .run();
        let r = rec.borrow();
        assert_eq!(r.timer_ats, vec![15], "timer scheduled at 15 fires once");
        assert_eq!(r.fills, 1, "on_fill hook fired for the single buy");
        assert_eq!(r.spec_tick, 1, "instrument_spec is reachable from Ctx");
        // Bought 2 @ 110, marked at 120 on the last bar: unrealized = 2*(120-110).
        assert_eq!(r.unrealized_end, 20);
    }

    #[test]
    fn timer_fires_on_non_bar_events_and_at_loop_tail() {
        // m19/m21: a timer due during a stretch of non-bar events (Signal / Resync),
        // or for a time at/under the final event, must still fire. Without the fix it
        // only fired on the next Bar — and never if no bar followed.
        use std::cell::RefCell;
        use std::rc::Rc;

        #[derive(Default)]
        struct Rec {
            timer_ats: Vec<i64>,
        }
        struct Probe {
            rec: Rc<RefCell<Rec>>,
        }
        impl Strategy for Probe {
            fn on_start(&mut self, ctx: &mut Ctx<'_>) {
                ctx.schedule(Timestamp::from_nanos(15)); // fires on the Signal @20
                ctx.schedule(Timestamp::from_nanos(25)); // fires on the Resync @30
                ctx.schedule(Timestamp::from_nanos(40)); // > last event ts: never fires
            }
            fn on_bar(&mut self, _bar: Bar, _ctx: &mut Ctx<'_>) {}
            fn on_timer(&mut self, at: Timestamp, _ctx: &mut Ctx<'_>) {
                self.rec.borrow_mut().timer_ats.push(at.as_nanos());
            }
        }

        let inst = InstrumentId::new(0);
        // One bar to seed, then ONLY non-bar events advance the clock past 15 and 25.
        let events = vec![
            Event::Bar(bar(0, 10, 100)),
            Event::Signal {
                instrument: inst,
                channel: 1,
                value: 7,
                ts: Timestamp::from_nanos(20),
            },
            Event::Resync {
                instrument: Some(inst),
                ts: Timestamp::from_nanos(30),
            },
        ];
        let rec = Rc::new(RefCell::new(Rec::default()));
        Engine::new(
            &[spec(0)],
            Money::ZERO,
            VecSource(events.into_iter()),
            MockExec::default(),
            Probe { rec: rec.clone() },
        )
        .unwrap()
        .run();
        assert_eq!(
            rec.borrow().timer_ats,
            vec![15, 25],
            "timers due fire on the Signal (15) and Resync (25); 40 stays pending (no event reaches it)"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // exhaustive: exercises every accessor + event hook
    fn ctx_accessors_and_per_event_callbacks() {
        use std::cell::RefCell;
        use std::rc::Rc;

        use akadro_core::{CancelReason, EventSink, RejectReason};

        #[derive(Default)]
        struct Rec {
            fills: usize,
            rejected: usize,
            canceled: usize,
            liquidated: usize,
            funded: usize,
            had_open: bool,
            submitted_found: bool,
        }

        // Emits one of every account-event kind on the first bar so each specific
        // callback fires; accepts orders on submit so has_open_order can be seen.
        #[derive(Default)]
        struct EmitAll {
            fired: bool,
        }
        impl ExecutionClient for EmitAll {
            fn submit(
                &mut self,
                id: ClientOrderId,
                _o: OrderRequest,
                now: Timestamp,
                sink: &mut dyn EventSink,
            ) {
                sink.emit(AccountEvent::OrderAccepted { id, ts: now });
            }
            fn observe(&mut self, event: &Event, now: Timestamp, sink: &mut dyn EventSink) {
                let Event::Bar(b) = event else { return };
                if self.fired {
                    return;
                }
                self.fired = true;
                let i = InstrumentId::new(0);
                let mut costs = Costs::new();
                costs.push(Cost::new(
                    AssetId::new(1),
                    Money::from_raw(1),
                    CostKind::Taker,
                ));
                sink.emit(AccountEvent::Fill {
                    client_order_id: ClientOrderId::new(0),
                    instrument: i,
                    side: Side::Buy,
                    price: b.close,
                    qty: Qty::from_raw(2),
                    costs,
                    complete: true,
                    ts: now,
                });
                sink.emit(AccountEvent::OrderRejected {
                    id: ClientOrderId::new(9),
                    reason: RejectReason::VenueRejected,
                    ts: now,
                });
                sink.emit(AccountEvent::OrderCanceled {
                    id: ClientOrderId::new(8),
                    reason: CancelReason::VenueCanceled,
                    ts: now,
                });
                sink.emit(AccountEvent::FundingSettlement {
                    instrument: i,
                    cost: Cost::new(AssetId::new(1), Money::from_raw(1), CostKind::Funding),
                    rate: 5,
                    ts: now,
                });
                sink.emit(AccountEvent::Liquidation {
                    instrument: i,
                    side: Side::Sell,
                    price: b.close,
                    qty: Qty::from_raw(0),
                    costs: Costs::new(),
                    ts: now,
                });
            }
        }

        struct Probe {
            submitted: bool,
            rec: Rc<RefCell<Rec>>,
        }
        impl Strategy for Probe {
            fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
                let i = InstrumentId::new(0);
                if !self.submitted {
                    let id = ctx.submit(OrderRequest::market(i, Side::Buy, Qty::from_raw(2)));
                    self.submitted = true;
                    self.rec.borrow_mut().submitted_found = ctx.submitted_order(id).is_some();
                }
                // Exercise the read-only accessors (non-panicking).
                let _ = (ctx.equity(), ctx.realized_pnl_net(), ctx.open_order_ids(i));
                let _ = ctx.is_warmed_up(i, 1);
            }
            fn on_account(&mut self, event: &AccountEvent, ctx: &mut Ctx<'_>) {
                if matches!(event, AccountEvent::OrderAccepted { .. })
                    && ctx.has_open_order(InstrumentId::new(0))
                {
                    self.rec.borrow_mut().had_open = true;
                }
            }
            fn on_fill(&mut self, _e: &AccountEvent, _c: &mut Ctx<'_>) {
                self.rec.borrow_mut().fills += 1;
            }
            fn on_order_rejected(&mut self, _e: &AccountEvent, _c: &mut Ctx<'_>) {
                self.rec.borrow_mut().rejected += 1;
            }
            fn on_order_canceled(&mut self, _e: &AccountEvent, _c: &mut Ctx<'_>) {
                self.rec.borrow_mut().canceled += 1;
            }
            fn on_liquidation(&mut self, _e: &AccountEvent, _c: &mut Ctx<'_>) {
                self.rec.borrow_mut().liquidated += 1;
            }
            fn on_funding(&mut self, _e: &AccountEvent, _c: &mut Ctx<'_>) {
                self.rec.borrow_mut().funded += 1;
            }
        }

        let rec = Rc::new(RefCell::new(Rec::default()));
        let feed =
            VecSource(vec![Event::Bar(bar(0, 10, 100)), Event::Bar(bar(0, 20, 110))].into_iter());
        Engine::new(
            &[spec(0)],
            Money::from_raw(100_000),
            feed,
            EmitAll::default(),
            Probe {
                submitted: false,
                rec: rec.clone(),
            },
        )
        .unwrap()
        .run();
        let r = rec.borrow();
        assert_eq!(
            (r.fills, r.rejected, r.canceled, r.liquidated, r.funded),
            (1, 1, 1, 1, 1),
            "each per-event callback fired once"
        );
        assert!(r.had_open, "has_open_order is true right after accept");
        assert!(
            r.submitted_found,
            "submitted_order returns the placed order"
        );
    }

    #[test]
    fn default_callbacks_and_timer_are_noops() {
        use akadro_core::{CancelReason, EventSink, RejectReason};
        // Emits every event kind so a strategy that overrides NOTHING exercises the
        // default no-op hooks (on_fill/on_order_*/on_liquidation/on_funding/on_timer).
        #[derive(Default)]
        struct E {
            fired: bool,
        }
        impl ExecutionClient for E {
            fn submit(
                &mut self,
                _i: ClientOrderId,
                _o: OrderRequest,
                _n: Timestamp,
                _s: &mut dyn EventSink,
            ) {
            }
            fn observe(&mut self, ev: &Event, now: Timestamp, sink: &mut dyn EventSink) {
                let Event::Bar(b) = ev else { return };
                if self.fired {
                    return;
                }
                self.fired = true;
                let i = InstrumentId::new(0);
                let mut c = Costs::new();
                c.push(Cost::new(
                    AssetId::new(1),
                    Money::from_raw(1),
                    CostKind::Taker,
                ));
                sink.emit(AccountEvent::Fill {
                    client_order_id: ClientOrderId::new(0),
                    instrument: i,
                    side: Side::Buy,
                    price: b.close,
                    qty: Qty::from_raw(1),
                    costs: c,
                    complete: true,
                    ts: now,
                });
                sink.emit(AccountEvent::OrderRejected {
                    id: ClientOrderId::new(1),
                    reason: RejectReason::VenueRejected,
                    ts: now,
                });
                sink.emit(AccountEvent::OrderCanceled {
                    id: ClientOrderId::new(1),
                    reason: CancelReason::VenueCanceled,
                    ts: now,
                });
                sink.emit(AccountEvent::FundingSettlement {
                    instrument: i,
                    cost: Cost::new(AssetId::new(1), Money::from_raw(1), CostKind::Funding),
                    rate: 1,
                    ts: now,
                });
                sink.emit(AccountEvent::Liquidation {
                    instrument: i,
                    side: Side::Sell,
                    price: b.close,
                    qty: Qty::from_raw(0),
                    costs: Costs::new(),
                    ts: now,
                });
            }
        }
        // A strategy that overrides nothing but on_bar (which schedules a timer so
        // the default on_timer no-op also runs).
        struct Bare {
            scheduled: bool,
        }
        impl Strategy for Bare {
            fn on_bar(&mut self, _b: Bar, ctx: &mut Ctx<'_>) {
                if !self.scheduled {
                    ctx.schedule(Timestamp::from_nanos(5));
                    self.scheduled = true;
                }
            }
        }
        let feed =
            VecSource(vec![Event::Bar(bar(0, 10, 100)), Event::Bar(bar(0, 20, 110))].into_iter());
        let report = Engine::new(
            &[spec(0)],
            Money::from_raw(100_000),
            feed,
            E::default(),
            Bare { scheduled: false },
        )
        .unwrap()
        .run();
        assert_eq!(report.fills.len(), 1);
    }

    #[test]
    fn rejects_empty_instruments() {
        let err = Engine::new(
            &[],
            Money::ZERO,
            VecSource(Vec::new().into_iter()),
            MockExec::default(),
            BuyOnce::default(),
        )
        .unwrap_err();
        assert!(matches!(err, AkadroError::Config(_)));
    }

    #[test]
    fn rejects_non_dense_ids() {
        let err = Engine::new(
            &[spec(1)], // should be id 0 at position 0
            Money::ZERO,
            VecSource(Vec::new().into_iter()),
            MockExec::default(),
            BuyOnce::default(),
        )
        .unwrap_err();
        assert!(matches!(err, AkadroError::Config(_)));
    }

    #[test]
    fn end_to_end_buy_fills_next_bar() {
        let events = vec![
            Event::Bar(bar(0, 1, 100)), // on_bar -> submit buy
            Event::Bar(bar(0, 2, 110)), // observe -> fill @ 110
            Event::Bar(bar(0, 3, 120)),
        ];
        let engine = Engine::new(
            &[spec(0)],
            Money::from_raw(1_000_000),
            VecSource(events.into_iter()),
            MockExec::default(),
            BuyOnce::default(),
        )
        .unwrap();
        assert_eq!(engine.instrument_count(), 1);
        let report = engine.run();

        assert_eq!(report.bars_processed, 3);
        assert_eq!(report.orders_submitted, 1);
        assert_eq!(report.fills.len(), 1);
        let fill = report.fills[0];
        assert_eq!(fill.price, Price::from_raw(110)); // filled at NEXT bar, not 100
        assert_eq!(fill.qty, Qty::from_raw(2));
        assert_eq!(fill.side, Side::Buy);
        // cash = 1_000_000 - 110*2 - 1 fee
        assert_eq!(report.final_cash, Money::from_raw(1_000_000 - 220 - 1));
        assert_eq!(report.trading_fees, Money::from_raw(1));
        assert_eq!(report.funding_net, Money::ZERO);
        assert_eq!(report.liquidation_pnl, Money::ZERO);
    }

    #[test]
    fn pending_order_visible_to_has_open_order_same_handler() {
        // m22: an order submitted earlier in the same handler (before its ack is
        // routed) must already count as a working order, so a strategy cannot
        // double-submit while waiting for the ack.
        use std::cell::Cell;
        use std::rc::Rc;
        struct S {
            before: Rc<Cell<bool>>,
            after: Rc<Cell<bool>>,
            done: bool,
        }
        impl Strategy for S {
            fn on_bar(&mut self, bar: Bar, ctx: &mut Ctx<'_>) {
                if self.done {
                    return;
                }
                self.done = true;
                self.before.set(ctx.has_open_order(bar.instrument));
                ctx.submit(OrderRequest::market(
                    bar.instrument,
                    Side::Buy,
                    Qty::from_raw(1),
                ));
                self.after.set(ctx.has_open_order(bar.instrument));
            }
        }
        let before = Rc::new(Cell::new(true));
        let after = Rc::new(Cell::new(false));
        let events = vec![Event::Bar(bar(0, 1, 100)), Event::Bar(bar(0, 2, 110))];
        Engine::new(
            &[spec(0)],
            Money::from_raw(1_000_000),
            VecSource(events.into_iter()),
            MockExec::default(),
            S {
                before: Rc::clone(&before),
                after: Rc::clone(&after),
                done: false,
            },
        )
        .unwrap()
        .run();
        assert!(!before.get(), "no working order before the submit");
        assert!(
            after.get(),
            "an order submitted this handler must be visible to has_open_order (m22)"
        );
    }

    #[test]
    fn resync_event_is_delivered() {
        #[derive(Default)]
        struct CountResync {
            resyncs: usize,
        }
        impl Strategy for CountResync {
            fn on_bar(&mut self, _bar: Bar, _ctx: &mut Ctx<'_>) {}
            fn on_account(&mut self, event: &AccountEvent, _ctx: &mut Ctx<'_>) {
                if matches!(event, AccountEvent::Resync { .. }) {
                    self.resyncs += 1;
                }
            }
        }
        let events = vec![
            Event::Bar(bar(0, 1, 100)),
            Event::Resync {
                instrument: None,
                ts: Timestamp::from_nanos(2),
            },
        ];
        // We can't read strategy state back after `run` consumes it, so assert
        // via the report that the run completed across the resync.
        let report = Engine::new(
            &[spec(0)],
            Money::ZERO,
            VecSource(events.into_iter()),
            MockExec::default(),
            CountResync::default(),
        )
        .unwrap()
        .run();
        assert_eq!(report.bars_processed, 1);
    }

    #[test]
    fn ctx_accessors_are_all_callable() {
        // A strategy that touches every read accessor on Ctx, so each is
        // exercised at runtime.
        #[derive(Default)]
        struct Toucher {
            cash_seen: i128,
        }
        impl Strategy for Toucher {
            fn on_start(&mut self, ctx: &mut Ctx<'_>) {
                // now() is stable within a handler.
                assert_eq!(ctx.now(), ctx.now());
            }
            fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
                let i = InstrumentId::new(0);
                let _ = ctx.now();
                let _ = ctx.opens(i);
                let _ = ctx.highs(i);
                let _ = ctx.lows(i);
                let _ = ctx.volumes(i);
                let _ = ctx.closes(i);
                let _ = ctx.bar_count(i);
                let _ = ctx.net_qty(i);
                let _ = ctx.avg_entry(i);
                let _ = ctx.realized_pnl();
                self.cash_seen = ctx.cash().raw();
                let _ = format!("{ctx:?}"); // exercise Ctx's Debug impl
                ctx.submit(OrderRequest::market(i, Side::Buy, Qty::from_raw(1)));
            }
            fn on_stop(&mut self, ctx: &mut Ctx<'_>) {
                let _ = ctx.cash();
            }
        }
        let events = vec![Event::Bar(bar(0, 1, 100)), Event::Bar(bar(0, 2, 110))];
        let report = Engine::new(
            &[spec(0)],
            Money::from_raw(5_000),
            VecSource(events.into_iter()),
            MockExec::default(),
            Toucher::default(),
        )
        .unwrap()
        .run();
        assert_eq!(report.bars_processed, 2);
        assert!(report.orders_submitted >= 1);
    }

    #[test]
    fn debug_impl() {
        let engine = Engine::new(
            &[spec(0)],
            Money::ZERO,
            VecSource(Vec::new().into_iter()),
            MockExec::default(),
            BuyOnce::default(),
        )
        .unwrap();
        assert!(format!("{engine:?}").contains("Engine"));
    }

    #[test]
    fn multi_instrument_order_only_fills_on_matching_bar() {
        // BuyOnce submits a market order on instrument 0 at the first bar. A bar
        // for instrument 1 arrives next: the execution client must skip the
        // resting order (it does not match) and only fill it once instrument 0's
        // bar appears.
        let events = vec![
            Event::Bar(bar(0, 1, 100)), // submit here; order rests
            Event::Bar(bar(1, 2, 200)), // different instrument -> skipped
            Event::Bar(bar(0, 3, 110)), // matches -> fills at 110
        ];
        let report = Engine::new(
            &[spec(0), spec(1)],
            Money::from_raw(10_000),
            VecSource(events.into_iter()),
            MockExec::default(),
            BuyOnce::default(),
        )
        .unwrap()
        .run();
        assert_eq!(report.bars_processed, 3);
        assert_eq!(report.orders_submitted, 1);
        assert_eq!(report.fills.len(), 1); // exactly one fill, on the instrument-0 bar
    }

    /// Records cancel/command calls so an engine test can assert they were routed.
    struct RecordingExec {
        resting: Vec<(ClientOrderId, OrderRequest)>,
        log: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }
    impl ExecutionClient for RecordingExec {
        fn submit(
            &mut self,
            id: ClientOrderId,
            order: OrderRequest,
            now: Timestamp,
            sink: &mut dyn akadro_core::EventSink,
        ) {
            sink.emit(AccountEvent::OrderAccepted { id, ts: now });
            self.resting.push((id, order));
        }
        fn observe(&mut self, _e: &Event, _now: Timestamp, _s: &mut dyn akadro_core::EventSink) {}
        fn cancel(
            &mut self,
            id: ClientOrderId,
            now: Timestamp,
            sink: &mut dyn akadro_core::EventSink,
        ) {
            self.log.borrow_mut().push(format!("cancel:{}", id.raw()));
            if let Some(p) = self.resting.iter().position(|(i, _)| *i == id) {
                self.resting.remove(p);
                sink.emit(AccountEvent::OrderCanceled {
                    id,
                    reason: akadro_core::CancelReason::Requested,
                    ts: now,
                });
            }
        }
        fn command(
            &mut self,
            _c: akadro_core::VenueCommand,
            _now: Timestamp,
            _s: &mut dyn akadro_core::EventSink,
        ) {
            self.log.borrow_mut().push("command".to_string());
        }
        fn config_seed(&self) -> Option<u64> {
            Some(42)
        }
    }

    #[test]
    fn ctx_cancel_and_command_route_to_execution_client() {
        // A strategy that, on its first bar, submits an order then cancels it and
        // issues a venue command — all three must reach the ExecutionClient.
        #[derive(Default)]
        struct SubmitCancelCommand {
            done: bool,
        }
        impl Strategy for SubmitCancelCommand {
            fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
                if !self.done {
                    let id = ctx.submit(OrderRequest::market(
                        InstrumentId::new(0),
                        Side::Buy,
                        Qty::from_raw(1),
                    ));
                    ctx.cancel(id);
                    ctx.command(akadro_core::VenueCommand::SetLeverage {
                        instrument: InstrumentId::new(0),
                        leverage: 3,
                    });
                    self.done = true;
                }
            }
        }
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let exec = RecordingExec {
            resting: Vec::new(),
            log: log.clone(),
        };
        let report = Engine::new(
            &[spec(0)],
            Money::ZERO,
            VecSource(vec![Event::Bar(bar(0, 1, 100))].into_iter()),
            exec,
            SubmitCancelCommand::default(),
        )
        .unwrap()
        .run();
        assert_eq!(report.orders_submitted, 1);
        assert!(report.fills.is_empty(), "canceled order must not fill");
        assert_eq!(
            *log.borrow(),
            vec!["cancel:0".to_string(), "command".to_string()]
        );
        assert_eq!(report.seed, Some(42)); // the client's seed is recorded
    }

    #[test]
    #[should_panic(expected = "drain exceeded")]
    fn runaway_account_event_loop_panics() {
        // A strategy that submits on every account event creates an unbounded
        // ack->submit->ack feedback loop; the drain guard panics rather than hangs.
        #[derive(Default)]
        struct Spinner {
            started: bool,
        }
        impl Strategy for Spinner {
            fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
                if !self.started {
                    self.started = true;
                    ctx.submit(OrderRequest::market(
                        InstrumentId::new(0),
                        Side::Buy,
                        Qty::from_raw(1),
                    ));
                }
            }
            fn on_account(&mut self, _e: &AccountEvent, ctx: &mut Ctx<'_>) {
                ctx.submit(OrderRequest::market(
                    InstrumentId::new(0),
                    Side::Buy,
                    Qty::from_raw(1),
                ));
            }
        }
        let exec = RecordingExec {
            resting: Vec::new(),
            log: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        };
        let _ = Engine::new(
            &[spec(0)],
            Money::ZERO,
            VecSource(vec![Event::Bar(bar(0, 1, 100))].into_iter()),
            exec,
            Spinner::default(),
        )
        .unwrap()
        .run();
    }

    #[cfg(feature = "serde")]
    #[test]
    fn run_report_save_load_round_trips() {
        let events = vec![Event::Bar(bar(0, 1, 100)), Event::Bar(bar(0, 2, 110))];
        let report = Engine::new(
            &[spec(0)],
            Money::from_raw(10_000),
            VecSource(events.into_iter()),
            MockExec::default(),
            BuyOnce::default(),
        )
        .unwrap()
        .run();
        let path = std::env::temp_dir().join("akadro_runreport_roundtrip.json");
        report.save(&path).unwrap();
        let loaded = RunReport::load(&path).unwrap();
        assert_eq!(report, loaded); // lossless round-trip (incl. i128 Money)
        assert_eq!(loaded.format_version, REPORT_FORMAT_VERSION);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn run_report_format_is_backward_and_forward_compatible() {
        // An older file (no `format_version`, no `seed`) still loads — both fields
        // take their defaults.
        let older = r#"{"fills":[],"equity_curve":[],"final_cash":0,
            "realized_pnl":0,"total_fees":0,"orders_submitted":0,"bars_processed":0}"#;
        let r = RunReport::from_json(older).unwrap();
        assert_eq!(r.format_version, 0);
        assert_eq!(r.seed, None);
        assert!(r.fills.is_empty());
        // A newer file with an unknown extra field is accepted (forward compat).
        let newer = r#"{"format_version":1,"fills":[],"equity_curve":[],"final_cash":5,
            "realized_pnl":0,"total_fees":0,"orders_submitted":0,"bars_processed":0,
            "seed":7,"future_field":["ignored"]}"#;
        let r = RunReport::from_json(newer).unwrap();
        assert_eq!(r.seed, Some(7));
        assert_eq!(r.final_cash, Money::from_raw(5));
        // A file from a *newer*, unknown format version is rejected.
        let future = r#"{"format_version":999,"fills":[],"equity_curve":[],"final_cash":0,
            "realized_pnl":0,"total_fees":0,"orders_submitted":0,"bars_processed":0}"#;
        assert!(RunReport::from_json(future).is_err());
        // Malformed JSON surfaces a clean error; a missing file is an I/O error.
        assert!(RunReport::from_json("definitely not json").is_err());
        assert!(RunReport::load("/nonexistent/akadro/none.json").is_err());
    }
}
