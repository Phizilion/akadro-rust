// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Record / replay: rebuild a backtest result from a *saved* [`RunReport`] with
//! **no strategy logic**, as a regression oracle for the trade-simulation pipeline.
//!
//! # What it is for
//!
//! Save a [`RunReport`] (the fills + realized `PnL` + fees a strategy produced),
//! then later replay just its *trades* through the real [`Engine`] +
//! [`SimulatedExchange`] + portfolio — without recompiling or re-running the
//! original strategy. Comparing the replay to the saved report tells you whether a
//! code change altered the trading result:
//!
//! * a change that **should not** affect trading but did → the replay diverges
//!   from the saved report (a red flag), and
//! * a change that **should** affect trading but did not → the replay still
//!   matches a stale saved report (also a red flag).
//!
//! # How it works (and its honest scope)
//!
//! The report stores the realized fills, **not** the original market data or order
//! kinds. So the replay does not reconstruct the original fill *generation* (which
//! limit/stop level crossed, slippage, partials). Instead it re-drives the exact
//! realized `(instrument, side, price, qty, ts)` sequence as **canonical
//! next-bar-open market orders**: [`ReplayFeed`] synthesizes the minimal bar grid
//! so the conservative exchange fills each recorded trade at its recorded price and
//! time, and [`ReplayStrategy`] re-submits one market order per trade. The fee and
//! `PnL` are then recomputed by the **live** fee/accounting code. A mismatch
//! therefore pinpoints a change in the fee / `PnL` / portfolio path — which is
//! exactly the "did the simulation result move?" question.
//!
//! ```no_run
//! # use akadro_engine::RunReport;
//! # use akadro_backtest::replay::{replay_trades, diff_reports};
//! # fn demo(saved: &RunReport, fee_bps: i64) -> Result<(), Box<dyn std::error::Error>> {
//! let rebuilt = replay_trades(saved, fee_bps)?;   // re-drive the trades from zero
//! let check = diff_reports(saved, &rebuilt);      // compare the invariants
//! assert!(check.matches, "simulation result changed: {:?}", check.issues);
//! # Ok(()) }
//! ```

use akadro_core::{
    AkadroError, AssetId, Bar, CapSet, DataSource, Event, InstrumentId, InstrumentKind,
    InstrumentSpec, Money, OrderRequest, Price, Qty, Side, Timestamp,
};
use akadro_engine::{Ctx, Engine, FillRecord, RunReport, Strategy};

use crate::SimulatedExchange;

/// One recorded trade lifted from a [`FillRecord`]: what to re-submit, and the
/// price/time at which it must fill.
#[derive(Clone, Copy, Debug)]
struct ReplayTrade {
    instrument: InstrumentId,
    side: Side,
    price: Price,
    qty: Qty,
    ts: Timestamp,
}

impl From<&FillRecord> for ReplayTrade {
    fn from(f: &FillRecord) -> Self {
        ReplayTrade {
            instrument: f.instrument,
            side: f.side,
            price: f.price,
            qty: f.qty,
            ts: f.ts,
        }
    }
}

/// The recorded fills as trades, in fill order (ts then instrument, stable). Both
/// [`ReplayFeed`] and [`ReplayStrategy`] derive from this identical ordering so the
/// k-th synthesized bar lines up with the k-th re-submitted order.
fn sorted_trades(report: &RunReport) -> Vec<ReplayTrade> {
    let mut trades: Vec<ReplayTrade> = report.fills.iter().map(ReplayTrade::from).collect();
    // Stable sort by event time (then instrument) — the report is already emitted
    // in ts order, so this is a no-op for well-formed reports but defends against
    // a hand-edited file.
    trades.sort_by_key(|t| (t.ts.as_nanos(), t.instrument.index()));
    trades
}

/// A degenerate (zero-range) bar at `price` for `instrument` at `ts`.
fn flat_bar(instrument: InstrumentId, ts: Timestamp, price: Price, volume: Qty) -> Bar {
    Bar::new(instrument, ts, price, price, price, price, volume)
}

/// A permissive spot spec (unit tick/lot, no min-notional) so market replays fill
/// without rounding or rejection; `quote` is set so the taker fee is emitted.
fn replay_spec(id: u32) -> InstrumentSpec {
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

/// A [`DataSource`] that emits the minimal bar grid to reproduce a recorded fill
/// sequence through the conservative [`SimulatedExchange`] (market orders fill at
/// the *next* bar's open).
///
/// For `n` trades it yields `n + 1` flat bars: a lead-in bar (so the first
/// re-submitted order has a next bar to fill on) followed by one bar per trade
/// whose `open`/`ts` equal that trade's recorded price/time.
#[derive(Debug)]
pub struct ReplayFeed {
    bars: Vec<Bar>,
    idx: usize,
}

impl ReplayFeed {
    /// Build a replay feed from a saved report's fills.
    #[must_use]
    pub fn from_report(report: &RunReport) -> Self {
        Self::from_trades(&sorted_trades(report))
    }

    fn from_trades(trades: &[ReplayTrade]) -> Self {
        let mut bars = Vec::with_capacity(trades.len() + 1);
        if let Some(first) = trades.first() {
            // Lead-in: one tick before the first fill, at the first price. Nothing
            // rests yet, so it produces no fill — it only gives bar 1 a predecessor.
            // `saturating_sub` guards a crafted/corrupt report whose first ts is
            // `i64::MIN` (otherwise the subtraction would overflow); sharing the ts
            // is harmless for the lead-in.
            bars.push(flat_bar(
                first.instrument,
                Timestamp::from_nanos(first.ts.as_nanos().saturating_sub(1)),
                first.price,
                first.qty,
            ));
            for t in trades {
                bars.push(flat_bar(t.instrument, t.ts, t.price, t.qty));
            }
        }
        ReplayFeed { bars, idx: 0 }
    }
}

impl DataSource for ReplayFeed {
    fn next_event(&mut self) -> Option<Event> {
        let bar = self.bars.get(self.idx)?;
        self.idx += 1;
        Some(Event::Bar(*bar))
    }
}

/// A built-in [`Strategy`] that replays a recorded trade sequence: on the k-th bar
/// it submits a market order matching the k-th recorded trade. It reads **no**
/// market data (the schedule is the recorded list), so it behaves identically in
/// any build — only the simulation pipeline it drives can change the result.
#[derive(Debug)]
pub struct ReplayStrategy {
    orders: Vec<(InstrumentId, Side, Qty)>,
    next: usize,
}

impl ReplayStrategy {
    /// Build a replay strategy from a saved report's fills.
    #[must_use]
    pub fn from_report(report: &RunReport) -> Self {
        let orders = sorted_trades(report)
            .iter()
            .map(|t| (t.instrument, t.side, t.qty))
            .collect();
        ReplayStrategy { orders, next: 0 }
    }
}

impl Strategy for ReplayStrategy {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        if let Some(&(instrument, side, qty)) = self.orders.get(self.next) {
            ctx.submit(OrderRequest::market(instrument, side, qty));
            self.next += 1;
        }
    }
}

/// Replay a saved report's trades through the real engine + simulated exchange,
/// rebuilding the result from **zero** starting cash. `fee_bps` is the taker fee
/// to recompute with — pass the same fee the original ran with so a fee-code change
/// (not a config change) is what a mismatch reveals.
///
/// # Errors
/// Returns [`AkadroError::Config`] only if engine construction rejects the
/// synthesized instrument set (it does not in practice — the specs are dense).
pub fn replay_trades(report: &RunReport, fee_bps: i64) -> Result<RunReport, AkadroError> {
    let trades = sorted_trades(report);
    // Dense specs 0..=max referenced instrument (≥ one, since the engine requires
    // at least one instrument even for an empty report).
    let max_id = trades
        .iter()
        .map(|t| t.instrument.index())
        .max()
        .unwrap_or(0);
    let specs: Vec<InstrumentSpec> = (0..=max_id).map(replay_spec).collect();

    let feed = ReplayFeed::from_trades(&trades);
    let strategy = ReplayStrategy {
        orders: trades
            .iter()
            .map(|t| (t.instrument, t.side, t.qty))
            .collect(),
        next: 0,
    };
    let exchange = SimulatedExchange::new(specs.clone(), fee_bps);
    let engine = Engine::new(&specs, Money::ZERO, feed, exchange, strategy)?;
    Ok(engine.run())
}

/// The outcome of comparing a saved report against its replay.
#[derive(Debug, Default, Clone)]
pub struct ReplayDiff {
    /// `true` when every compared invariant (fills, realized `PnL`, fees) matched.
    pub matches: bool,
    /// Human-readable description of each divergence (empty when `matches`).
    pub issues: Vec<String>,
}

/// Compare a saved report against its replay on the trade-simulation invariants:
/// the per-fill `(instrument, side, price, qty, fee, ts)`, a **fills-only** fee
/// total, and the realized `PnL`. Starting-cash-dependent fields (`final_cash`)
/// and the market-marked `equity_curve` are **not** compared — the replay rebuilds
/// from zero over a synthetic grid, so those legitimately differ.
///
/// Two subtleties make this a *fills-only* oracle:
/// * Both fill lists are sorted by `(ts, instrument, side, price, qty)` before the
///   positional compare, so a multi-instrument report whose fills arrive in
///   feed order (not instrument-index order) does not produce a false mismatch.
/// * `funding`/`liquidation` account effects are **not** in `fills`, and the
///   replay (which re-drives only `fills`) cannot reproduce them. Since the report
///   now separates `trading_fees` (fill/liquidation execution costs) from
///   `funding_net`, the fill-fee compare uses the replay's `trading_fees` directly
///   (it has no funding/liquidation, so that equals its fill-fee sum). For the
///   `realized_pnl` scalar, only **liquidation** folds non-fill `PnL` into it
///   (funding does not), so it is compared exactly when the saved report recorded
///   no liquidation (`liquidation_pnl == 0`) — gating on liquidation presence, not
///   on fee totals. This removes the previous false-match (a *funded* but
///   un-liquidated run was wrongly skipped) and false-mismatch (a *fee-less*
///   liquidation was wrongly compared) cases (i3).
#[must_use]
pub fn diff_reports(original: &RunReport, replay: &RunReport) -> ReplayDiff {
    let mut issues = Vec::new();

    // Order-independent compare: canonicalise both fill lists by a total key.
    let key = |f: &FillRecord| {
        (
            f.ts.as_nanos(),
            f.instrument.index(),
            f.side.sign(),
            f.price.raw(),
            f.qty.raw(),
        )
    };
    let mut a_fills = original.fills.clone();
    let mut b_fills = replay.fills.clone();
    a_fills.sort_by_key(key);
    b_fills.sort_by_key(key);

    if a_fills.len() == b_fills.len() {
        for (i, (a, b)) in a_fills.iter().zip(&b_fills).enumerate() {
            if a.instrument != b.instrument
                || a.side != b.side
                || a.price != b.price
                || a.qty != b.qty
                || a.fee != b.fee
                || a.ts != b.ts
            {
                issues.push(format!(
                    "fill[{i}]: saved (side {:?}, px {}, qty {}, fee {}) vs replay (side {:?}, px {}, qty {}, fee {})",
                    a.side, a.price.raw(), a.qty.raw(), a.fee.raw(),
                    b.side, b.price.raw(), b.qty.raw(), b.fee.raw(),
                ));
            }
        }
    } else {
        issues.push(format!(
            "fill count: saved {} vs replay {}",
            a_fills.len(),
            b_fills.len()
        ));
    }

    // Fills-only fee total: the replay has no funding/liquidation, so its
    // `trading_fees` is exactly its fill-fee sum; compare against the saved report's
    // fill-fee sum. (`funding_net` is excluded by construction now — i1/i3.)
    let fill_fee_sum = |fs: &[FillRecord]| {
        fs.iter()
            .fold(Money::ZERO, |acc, f| acc.saturating_add(f.fee))
    };
    let saved_fill_fees = fill_fee_sum(&original.fills);
    if replay.trading_fees != saved_fill_fees {
        issues.push(format!(
            "fill fees: saved {} vs replay {}",
            saved_fill_fees.raw(),
            replay.trading_fees.raw()
        ));
    }

    // `realized_pnl` is comparable unless the saved run was liquidated — funding
    // leaves realized `PnL` untouched, but liquidation folds non-fill `PnL` into it
    // which the replay (fills-only) cannot reproduce. Gate on liquidation presence
    // (`liquidation_pnl == 0`), NOT on fee totals — that fixes both the funded
    // false-match and the fee-less-liquidation false-mismatch (i3).
    let saved_is_fills_only = original.liquidation_pnl == Money::ZERO;
    if saved_is_fills_only && original.realized_pnl != replay.realized_pnl {
        issues.push(format!(
            "realized_pnl: saved {} vs replay {}",
            original.realized_pnl.raw(),
            replay.realized_pnl.raw()
        ));
    }

    ReplayDiff {
        matches: issues.is_empty(),
        issues,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HistoricalFeed;

    /// A throwaway strategy that buys once then sells once — enough to produce a
    /// real two-fill report to replay.
    struct BuyThenSell {
        instrument: InstrumentId,
        qty: Qty,
        step: u8,
    }

    impl Strategy for BuyThenSell {
        fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
            match self.step {
                0 => ctx.submit(OrderRequest::market(self.instrument, Side::Buy, self.qty)),
                1 => ctx.submit(OrderRequest::market(self.instrument, Side::Sell, self.qty)),
                _ => return,
            };
            self.step += 1;
        }
    }

    fn bar(inst: u32, ts: i64, open: i64, close: i64) -> Bar {
        Bar::new(
            InstrumentId::new(inst),
            Timestamp::from_nanos(ts),
            Price::from_raw(open),
            Price::from_raw(open.max(close)),
            Price::from_raw(open.min(close)),
            Price::from_raw(close),
            Qty::from_raw(1_000_000),
        )
    }

    fn run_original(fee_bps: i64) -> RunReport {
        let specs = vec![replay_spec(0)];
        let bars = vec![
            bar(0, 10, 100, 101),
            bar(0, 20, 110, 111), // buy (submitted bar0) fills here @110
            bar(0, 30, 120, 121), // sell (submitted bar1) fills here @120
            bar(0, 40, 130, 131),
        ];
        let strat = BuyThenSell {
            instrument: InstrumentId::new(0),
            qty: Qty::from_raw(5),
            step: 0,
        };
        let exchange = SimulatedExchange::new(specs.clone(), fee_bps);
        Engine::new(
            &specs,
            Money::from_raw(1_000_000),
            HistoricalFeed::from_bars(bars),
            exchange,
            strat,
        )
        .unwrap()
        .run()
    }

    #[test]
    fn replay_reproduces_a_real_runs_fills_and_pnl() {
        let original = run_original(10);
        assert_eq!(original.fills.len(), 2, "buy + sell");

        let rebuilt = replay_trades(&original, 10).unwrap();
        let diff = diff_reports(&original, &rebuilt);
        assert!(diff.matches, "replay should match: {:?}", diff.issues);

        // The rebuilt fills are bit-identical on the compared invariants...
        assert_eq!(rebuilt.fills.len(), 2);
        for (a, b) in original.fills.iter().zip(&rebuilt.fills) {
            assert_eq!(
                (a.side, a.price, a.qty, a.fee, a.ts),
                (b.side, b.price, b.qty, b.fee, b.ts)
            );
        }
        // ...and PnL was rebuilt from zero through the live accounting.
        assert_eq!(original.realized_pnl, rebuilt.realized_pnl);
        assert_eq!(original.trading_fees, rebuilt.trading_fees);
    }

    #[test]
    fn replay_detects_a_fee_change() {
        // Saved with 10 bps; replaying under a *different* fee recomputes different
        // fees, so the oracle flags it — the "code that should affect trading" case.
        let original = run_original(10);
        let rebuilt = replay_trades(&original, 25).unwrap();
        let diff = diff_reports(&original, &rebuilt);
        assert!(!diff.matches, "a fee change must be detected");
        assert!(
            diff.issues
                .iter()
                .any(|m| m.contains("fee") || m.contains("total_fees")),
            "the divergence should mention fees: {:?}",
            diff.issues
        );
    }

    #[test]
    fn replay_is_deterministic() {
        let original = run_original(7);
        let a = replay_trades(&original, 7).unwrap();
        let b = replay_trades(&original, 7).unwrap();
        assert!(diff_reports(&a, &b).matches);
    }

    #[test]
    fn empty_report_replays_empty() {
        let empty = RunReport::default();
        let rebuilt = replay_trades(&empty, 5).unwrap();
        assert_eq!(rebuilt.fills.len(), 0);
        assert!(diff_reports(&empty, &rebuilt).matches);
    }

    #[test]
    fn zero_fee_replay_matches() {
        let original = run_original(0);
        let rebuilt = replay_trades(&original, 0).unwrap();
        let diff = diff_reports(&original, &rebuilt);
        assert!(diff.matches, "{:?}", diff.issues);
        assert_eq!(rebuilt.trading_fees, Money::ZERO);
    }

    #[test]
    fn public_from_report_constructors_drive_a_run() {
        // The advertised building blocks (`ReplayFeed`/`ReplayStrategy::from_report`)
        // wire up by hand to the same result as `replay_trades`.
        let original = run_original(10);
        let specs = vec![replay_spec(0)];
        let exchange = SimulatedExchange::new(specs.clone(), 10);
        let rebuilt = Engine::new(
            &specs,
            Money::ZERO,
            ReplayFeed::from_report(&original),
            exchange,
            ReplayStrategy::from_report(&original),
        )
        .unwrap()
        .run();
        assert!(diff_reports(&original, &rebuilt).matches);
    }

    #[test]
    fn diff_flags_count_field_and_pnl_divergence() {
        // Mutate clones of a real report to exercise each divergence message.
        let base = run_original(10); // two fills
        assert_eq!(base.fills.len(), 2);

        let mut fewer = base.clone();
        fewer.fills.pop();
        let d = diff_reports(&base, &fewer);
        assert!(!d.matches && d.issues.iter().any(|m| m.contains("fill count")));

        let mut altered = base.clone();
        altered.fills[1].price = Price::from_raw(altered.fills[1].price.raw() + 1);
        let d = diff_reports(&base, &altered);
        assert!(!d.matches && d.issues.iter().any(|m| m.contains("fill[1]")));

        let mut pnl = base.clone();
        pnl.realized_pnl = Money::from_raw(base.realized_pnl.raw() + 1);
        let d = diff_reports(&base, &pnl);
        assert!(!d.matches && d.issues.iter().any(|m| m.contains("realized_pnl")));
    }

    #[test]
    fn diff_is_order_independent_for_equal_ts_cross_instrument_fills() {
        use akadro_core::ClientOrderId;
        let f = |inst: u32, ts: i64| FillRecord {
            id: ClientOrderId::new(0),
            instrument: InstrumentId::new(inst),
            side: Side::Buy,
            price: Price::from_raw(100 + i64::from(inst)),
            qty: Qty::from_raw(5),
            fee: Money::ZERO,
            ts: Timestamp::from_nanos(ts),
        };
        // Same trade set at the same timestamp, delivered in feed order (inst 1
        // before inst 0) vs the replay's canonical (ts, instrument) order.
        let mut feed_order = RunReport::default();
        feed_order.fills = vec![f(1, 10), f(0, 10)];
        let mut sorted_order = RunReport::default();
        sorted_order.fills = vec![f(0, 10), f(1, 10)];
        // Pre-fix this produced a false MISMATCH from the positional zip.
        assert!(
            diff_reports(&feed_order, &sorted_order).matches,
            "equal-ts cross-instrument fills must match regardless of arrival order"
        );
    }

    #[test]
    fn diff_does_not_false_mismatch_on_non_fill_fees() {
        // Funding lands in `funding_net` (not in `fills`); the replay cannot
        // reproduce it. The oracle compares the *fills-only* `trading_fees` and must
        // not flag a mismatch on funding.
        let saved = run_original(10);
        let rebuilt = replay_trades(&saved, 10).unwrap();
        assert!(diff_reports(&saved, &rebuilt).matches);

        // Simulate funding paid: bump funding_net (realized_pnl is untouched).
        let mut funded = saved.clone();
        funded.funding_net = funded.funding_net.saturating_add(Money::from_raw(9_999));
        let d = diff_reports(&funded, &rebuilt);
        assert!(
            d.matches,
            "non-fill (funding) flow must not produce a false mismatch: {:?}",
            d.issues
        );
    }

    #[test]
    fn diff_still_compares_realized_pnl_when_only_funding_present() {
        // i3 false-MATCH fix: a *funded* (not liquidated) run must still have its
        // realized_pnl compared — funding does not touch realized_pnl, so a genuine
        // realized_pnl divergence must NOT be masked by funding presence.
        let saved = run_original(10);
        let rebuilt = replay_trades(&saved, 10).unwrap();
        let mut funded = saved.clone();
        funded.funding_net = Money::from_raw(9_999); // funding present, no liquidation
        funded.realized_pnl = funded.realized_pnl.saturating_add(Money::from_raw(1)); // real diff
        let d = diff_reports(&funded, &rebuilt);
        assert!(
            !d.matches && d.issues.iter().any(|m| m.contains("realized_pnl")),
            "a real realized_pnl diff under funding-only must be detected: {:?}",
            d.issues
        );
    }

    #[test]
    fn diff_skips_realized_pnl_on_feeless_liquidation() {
        // i3 false-MISMATCH fix: a liquidation folds non-fill PnL into realized_pnl
        // that the replay can't reproduce. Even a *fee-less* liquidation (no extra
        // fee) must skip the realized_pnl compare — gated on liquidation_pnl, not
        // fees.
        let saved = run_original(10);
        let rebuilt = replay_trades(&saved, 10).unwrap();
        let mut liq = saved.clone();
        liq.liquidation_pnl = Money::from_raw(500); // liquidation occurred (fee-less)
        liq.realized_pnl = liq.realized_pnl.saturating_add(Money::from_raw(500));
        let d = diff_reports(&liq, &rebuilt);
        assert!(
            d.matches,
            "a fee-less liquidation must not produce a realized_pnl false mismatch: {:?}",
            d.issues
        );
    }
}
