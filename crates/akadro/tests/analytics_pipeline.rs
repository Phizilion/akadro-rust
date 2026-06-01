// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Full pipeline: backtest -> equity curve + fills -> analytics metrics.

mod common;

use akadro_analytics::{PerformanceReport, TradeStats};

#[test]
fn analytics_from_a_backtest_run() {
    let report = common::run_backtest(common::strategy());

    // The engine recorded one equity point per bar.
    assert_eq!(report.equity_curve.len(), common::closes().len());

    // Performance metrics are computable and finite.
    let perf = PerformanceReport::from_equity(&report.equity_curve, 525_600.0)
        .expect("enough equity points");
    assert_eq!(perf.periods, report.equity_curve.len() - 1);
    assert!(perf.sharpe.is_finite());
    assert!(perf.sortino.is_finite());
    assert!((0.0..=1.0).contains(&perf.max_drawdown));

    // Trade stats reconstruct round trips from the fills.
    let trades = TradeStats::from_fills(&report.fills);
    assert!(trades.win_rate >= 0.0 && trades.win_rate <= 1.0);
    if trades.num_trades > 0 {
        // wins + losses never exceeds the number of completed trades.
        assert!(trades.wins + trades.losses <= trades.num_trades);
    }
}

#[test]
fn metrics_are_deterministic() {
    let a = common::run_backtest(common::strategy());
    let b = common::run_backtest(common::strategy());
    let pa = PerformanceReport::from_equity(&a.equity_curve, 252.0).unwrap();
    let pb = PerformanceReport::from_equity(&b.equity_curve, 252.0).unwrap();
    // Same input -> identical metrics (PerformanceReport is PartialEq).
    assert_eq!(pa, pb);
}
