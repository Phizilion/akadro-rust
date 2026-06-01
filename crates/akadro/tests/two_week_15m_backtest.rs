// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end: a simple SMA-crossover strategy over **two weeks of 15-minute
//! candles** (1344 bars), driven through the real engine, simulated exchange and
//! historical feed.
//!
//! This mirrors the canonical user request — "create simple strategy and try to
//! run backtest; use 2 weeks of data, 15m candles" — and additionally pins the
//! project's core guarantees:
//!
//! * the strategy reads **only** backward-looking `Ctx`/`Series` accessors, so it
//!   cannot read future data (the framework enforces this at compile time);
//! * the price path is **deterministic** (no `thread_rng`, no `Instant`, no
//!   wall-clock) and is engineered to produce several SMA crossovers, so the
//!   strategy actually trades multiple times;
//! * running the identical setup twice yields a **bit-identical** `RunReport`
//!   (the backtest determinism / parity guarantee).

use akadro::prelude::*;

// --- Fixed market parameters ---------------------------------------------------

/// The single instrument under test (dense id 0).
const INSTRUMENT: InstrumentId = InstrumentId::new(0);

/// 15 minutes expressed in nanoseconds (`15 * 60 * 1_000_000_000`).
const FIFTEEN_MIN_NS: i64 = 900_000_000_000;

/// Exactly two weeks of 15-minute bars: `14 days * 96 bars/day`.
const BAR_COUNT: usize = 14 * 96; // = 1344

/// A fixed, realistic start epoch in nanoseconds: 2024-01-01T00:00:00Z, i.e.
/// `1_704_067_200` seconds since the Unix epoch, scaled to nanoseconds. Chosen
/// purely so timestamps look like real exchange data; nothing depends on the
/// absolute value, only on the constant 15-minute spacing.
const START_NS: i64 = 1_704_067_200 * 1_000_000_000;

/// Flat per-bar volume (every bar trades the same notional; `> 0` as required).
const VOLUME: i64 = 100;

/// Taker fee charged by the simulated exchange (5 basis points).
const FEE_BPS: i64 = 5;

/// Starting cash in quote units (scaled integer; no floats).
const INITIAL_CASH: i128 = 10_000_000;

/// Fast / slow SMA windows and order size. Tuned (together with the oscillation
/// below) so the two-week path produces well more than the asserted floor of
/// four orders.
const FAST: usize = 12; // 3 hours of 15-minute bars
const SLOW: usize = 48; // 12 hours of 15-minute bars
const ORDER_QTY: i64 = 5;

// --- Deterministic price path --------------------------------------------------

/// One full oscillation every `PERIOD` bars (48 hours) — large relative to the
/// slow SMA window (48 bars), so the fast SMA repeatedly crosses the slow SMA.
const PERIOD: f64 = 192.0;

/// Deterministic close price for bar `i`: a base level, plus a gentle **upward
/// drift**, plus a sine **oscillation** of amplitude `AMP` around the trend.
/// Period (192 bars) ≫ the slow SMA (48 bars), so the fast SMA repeatedly
/// crosses it — producing several trades over the two weeks. `f64::sin` is fine
/// for generating test data: the run is reproduced within the same process (see
/// the determinism assertion), so the prices are identical run-to-run.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
// bounded test-data generation: i < 1344, price stays near 30_000.
fn close_at(i: usize) -> i64 {
    const BASE: f64 = 30_000.0; // ~ a plausible price level
    const DRIFT_PER_BAR: f64 = 0.5; // gentle upward trend
    const AMP: f64 = 1_500.0; // oscillation amplitude in raw price units

    let phase = std::f64::consts::TAU * (i as f64) / PERIOD;
    (BASE + DRIFT_PER_BAR * (i as f64) + AMP * phase.sin()) as i64
}

/// Build the full 1344-bar OHLCV series. Each bar's OHLC is derived from its
/// own close and the previous close so that the invariants hold:
/// `low <= open <= high` and `low <= close <= high`, with `volume > 0`.
fn build_bars() -> Vec<Bar> {
    let mut bars = Vec::with_capacity(BAR_COUNT);
    for i in 0..BAR_COUNT {
        let close = close_at(i);
        // Open at the previous close (first bar opens at its own close).
        let open = if i == 0 { close } else { close_at(i - 1) };
        let high = open.max(close) + 5; // a small wick above the body
        let low = open.min(close) - 5; // a small wick below the body
        debug_assert!(low <= open && open <= high);
        debug_assert!(low <= close && close <= high);

        let ts = Timestamp::from_nanos(START_NS + (i as i64) * FIFTEEN_MIN_NS);
        bars.push(Bar::new(
            INSTRUMENT,
            ts,
            Price::from_raw(open),
            Price::from_raw(high),
            Price::from_raw(low),
            Price::from_raw(close),
            Qty::from_raw(VOLUME),
        ));
    }
    bars
}

// --- A simple inline SMA-crossover strategy ------------------------------------

/// A minimal fast/slow SMA-crossover strategy, written inline for this test.
///
/// It maintains running window sums (remembering the *past* is fine) and reads
/// the value leaving each window via `ctx.closes(..).ago(n)` — a **backward-only**
/// accessor. There is deliberately no attempt to read future bars; the framework
/// makes that a compile error, so it is simply inexpressible here.
///
/// On a golden cross (fast SMA rising above slow SMA) it goes long; on a death
/// cross (fast falling below slow) it sells to flip flat/short. Order size is
/// fixed.
struct SmaCrossover {
    instrument: InstrumentId,
    fast: usize,
    slow: usize,
    qty: Qty,
    fast_sum: i64,
    slow_sum: i64,
    count: usize,
    prev_fast_ge_slow: Option<bool>,
}

impl SmaCrossover {
    fn new(instrument: InstrumentId, fast: usize, slow: usize, qty: Qty) -> Self {
        assert!(fast > 0 && fast < slow, "need 0 < fast < slow");
        Self {
            instrument,
            fast,
            slow,
            qty,
            fast_sum: 0,
            slow_sum: 0,
            count: 0,
            prev_fast_ge_slow: None,
        }
    }
}

impl Strategy for SmaCrossover {
    fn on_bar(&mut self, bar: Bar, ctx: &mut Ctx<'_>) {
        let close = bar.close.raw();
        self.count += 1;
        self.fast_sum += close;
        self.slow_sum += close;

        // Subtract the close that has just fallen out of each rolling window.
        // `ago(n)` looks strictly backward (n bars ago); no forward read exists.
        if self.count > self.fast
            && let Some(p) = ctx.closes(self.instrument).and_then(|s| s.ago(self.fast))
        {
            self.fast_sum -= p.raw();
        }
        if self.count > self.slow
            && let Some(p) = ctx.closes(self.instrument).and_then(|s| s.ago(self.slow))
        {
            self.slow_sum -= p.raw();
        }

        // Warm-up: do nothing until the slow window is full.
        if self.count < self.slow {
            return;
        }

        let fast_sma = self.fast_sum / self.fast as i64;
        let slow_sma = self.slow_sum / self.slow as i64;
        let fast_ge_slow = fast_sma >= slow_sma;

        if let Some(prev) = self.prev_fast_ge_slow {
            let net = ctx.net_qty(self.instrument).raw();
            if fast_ge_slow && !prev && net <= 0 {
                // Golden cross: go long.
                ctx.submit(OrderRequest::market(self.instrument, Side::Buy, self.qty));
            } else if !fast_ge_slow && prev && net >= 0 {
                // Death cross: sell to flip flat/short.
                ctx.submit(OrderRequest::market(self.instrument, Side::Sell, self.qty));
            }
        }
        self.prev_fast_ge_slow = Some(fast_ge_slow);
    }
}

// --- Harness -------------------------------------------------------------------

fn strategy() -> SmaCrossover {
    SmaCrossover::new(INSTRUMENT, FAST, SLOW, Qty::from_raw(ORDER_QTY))
}

fn spec() -> InstrumentSpec {
    InstrumentSpec::new(
        INSTRUMENT,
        AssetId::new(0),
        AssetId::new(1),
        InstrumentKind::Spot,
        Price::from_raw(1),
        Qty::from_raw(1),
        Money::ZERO,
        CapSet::empty(),
    )
}

/// Build a fresh engine over a fresh copy of the data and run it to completion.
fn run(bars: Vec<Bar>) -> RunReport {
    Engine::new(
        &[spec()],
        Money::from_raw(INITIAL_CASH),
        HistoricalFeed::from_bars(bars),
        SimulatedExchange::new(vec![spec()], FEE_BPS),
        strategy(),
    )
    .expect("engine builds")
    .run()
}

// --- The test ------------------------------------------------------------------

#[test]
fn two_week_15m_backtest_trades_and_is_deterministic() {
    let bars = build_bars();
    assert_eq!(bars.len(), BAR_COUNT, "exactly two weeks of 15m bars");

    // Sanity-check the generated data: spacing, ordering and OHLCV invariants.
    for (i, b) in bars.iter().enumerate() {
        assert_eq!(b.ts.as_nanos(), START_NS + (i as i64) * FIFTEEN_MIN_NS);
        assert!(b.low.raw() <= b.open.raw() && b.open.raw() <= b.high.raw());
        assert!(b.low.raw() <= b.close.raw() && b.close.raw() <= b.high.raw());
        assert!(b.volume.raw() > 0);
    }

    let report = run(bars.clone());

    // Every bar was processed, and we marked equity at every bar close.
    assert_eq!(report.bars_processed, BAR_COUNT as u64);
    assert_eq!(report.equity_curve.len(), BAR_COUNT);

    // The oscillation forces multiple crossovers, so the strategy trades several
    // times. A floor of four orders proves it is genuinely active, not a fluke.
    assert!(
        report.orders_submitted >= 4,
        "expected several trades, got {}",
        report.orders_submitted
    );
    assert!(!report.fills.is_empty(), "expected at least one fill");

    // Fees are charged (FEE_BPS > 0) and never negative.
    assert!(!report.trading_fees.is_negative());

    // The last equity point is stamped at the last bar's logical event time.
    let last_ts = report.equity_curve.last().expect("non-empty curve").ts;
    assert_eq!(
        last_ts.as_nanos(),
        START_NS + (BAR_COUNT as i64 - 1) * FIFTEEN_MIN_NS
    );

    // Determinism / parity: a second, fully independent run over the same data
    // must produce a BIT-IDENTICAL report.
    let report2 = run(bars);
    assert_eq!(report.fills, report2.fills, "fills diverged");
    assert_eq!(
        report.equity_curve, report2.equity_curve,
        "equity curve diverged"
    );
    assert_eq!(report.final_cash, report2.final_cash, "final cash diverged");
    assert_eq!(
        report.realized_pnl, report2.realized_pnl,
        "realized pnl diverged"
    );
    assert_eq!(
        report.trading_fees, report2.trading_fees,
        "trading fees diverged"
    );
    assert_eq!(report.funding_net, report2.funding_net, "funding diverged");
    assert_eq!(
        report.orders_submitted, report2.orders_submitted,
        "orders_submitted diverged"
    );
    assert_eq!(
        report.bars_processed, report2.bars_processed,
        "bars_processed diverged"
    );
    // And the whole report compares equal (RunReport: PartialEq).
    assert_eq!(report, report2, "the two runs are not bit-identical");
}
