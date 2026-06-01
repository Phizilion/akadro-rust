// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end: the sample SMA strategy runs through the backtest engine and
//! actually trades.

mod common;

#[test]
fn sma_strategy_runs_and_trades() {
    let report = common::run_backtest(common::strategy());

    // Every bar was processed.
    assert_eq!(report.bars_processed as usize, common::closes().len());
    // The triangle wave forces crossovers, so the strategy must have traded.
    assert!(report.orders_submitted >= 1, "expected at least one order");
    assert!(!report.fills.is_empty(), "expected at least one fill");
    // Fees were charged (FEE_BPS > 0) and are non-negative.
    assert!(!report.trading_fees.is_negative());
}

#[test]
fn no_trades_without_warmup() {
    // With a slow window longer than the data, the strategy never leaves warm-up
    // and must submit nothing.
    let strat = common::strategy_fs(3, 10_000);
    let report = common::run_backtest(strat);
    assert_eq!(report.orders_submitted, 0);
    assert!(report.fills.is_empty());
}
