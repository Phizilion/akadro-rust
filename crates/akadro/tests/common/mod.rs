// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared fixtures for the integration tests (not a test target itself).
#![allow(missing_docs, dead_code, unreachable_pub)]

use akadro_backtest::{HistoricalFeed, SimulatedExchange};
use akadro_core::{
    AssetId, Bar, CapSet, Event, InstrumentId, InstrumentKind, InstrumentSpec, Money, Price, Qty,
    Timestamp,
};
use akadro_engine::{Engine, RunReport};
use akadro_testkit::MockLiveFeed;
use sma_crossover::SmaCross;

pub const INSTRUMENT: InstrumentId = InstrumentId::new(0);
pub const FEE_BPS: i64 = 5;

pub fn initial_cash() -> Money {
    Money::from_raw(10_000_000)
}

pub fn spec() -> InstrumentSpec {
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

/// A triangle wave (100 → 138 → 100, repeated) that guarantees several SMA
/// crossovers, so the strategy actually trades.
pub fn closes() -> Vec<i64> {
    let mut v = Vec::new();
    for _ in 0..3 {
        for i in 0..20 {
            v.push(100 + i * 2);
        }
        for i in 0..20 {
            v.push(138 - i * 2);
        }
    }
    v
}

pub fn bars() -> Vec<Bar> {
    closes()
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let p = Price::from_raw(c);
            Bar::new(
                INSTRUMENT,
                Timestamp::from_nanos(i as i64 + 1),
                p,
                p,
                p,
                p,
                Qty::from_raw(1),
            )
        })
        .collect()
}

pub fn events() -> Vec<Event> {
    bars().into_iter().map(Event::Bar).collect()
}

pub fn strategy() -> SmaCross {
    strategy_fs(3, 8)
}

pub fn strategy_fs(fast: usize, slow: usize) -> SmaCross {
    SmaCross::new(INSTRUMENT, fast, slow, Qty::from_raw(1))
}

/// Run `strategy` over the fixture data through the historical backtest feed.
pub fn run_backtest(strategy: SmaCross) -> RunReport {
    Engine::new(
        &[spec()],
        initial_cash(),
        HistoricalFeed::from_bars(bars()),
        SimulatedExchange::new(vec![spec()], FEE_BPS),
        strategy,
    )
    .expect("engine builds")
    .run()
}

/// Build flat bars from an arbitrary list of close prices.
pub fn bars_from_closes(closes: &[i64]) -> Vec<Bar> {
    closes
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let p = Price::from_raw(c);
            Bar::new(
                INSTRUMENT,
                Timestamp::from_nanos(i as i64 + 1),
                p,
                p,
                p,
                p,
                Qty::from_raw(1),
            )
        })
        .collect()
}

/// Backtest a strategy over a caller-supplied bar series.
pub fn run_backtest_bars(strategy: SmaCross, bars: Vec<Bar>) -> RunReport {
    Engine::new(
        &[spec()],
        initial_cash(),
        HistoricalFeed::from_bars(bars),
        SimulatedExchange::new(vec![spec()], FEE_BPS),
        strategy,
    )
    .expect("engine builds")
    .run()
}

/// Run a strategy over a caller-supplied event stream via the mock-live feed.
pub fn run_mock_live_events(strategy: SmaCross, events: Vec<Event>, capacity: usize) -> RunReport {
    Engine::new(
        &[spec()],
        initial_cash(),
        MockLiveFeed::spawn(events, capacity),
        SimulatedExchange::new(vec![spec()], FEE_BPS),
        strategy,
    )
    .expect("engine builds")
    .run()
}

/// Run `strategy` over the SAME data delivered through the threaded mock-live
/// feed (bounded channel of `capacity`).
pub fn run_mock_live(strategy: SmaCross, capacity: usize) -> RunReport {
    Engine::new(
        &[spec()],
        initial_cash(),
        MockLiveFeed::spawn(events(), capacity),
        SimulatedExchange::new(vec![spec()], FEE_BPS),
        strategy,
    )
    .expect("engine builds")
    .run()
}
