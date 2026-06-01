// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! THE PARITY GOLDEN-MASTER (goal 2).
//!
//! The same strategy source, the same data, the same execution model — driven
//! once by the in-process historical feed and once by the threaded, bounded
//! mock-live feed — must produce a BIT-IDENTICAL `RunReport`. This proves the
//! strategy is blind to its feed/transport and that fixed-point money math keeps
//! results exactly reproducible across the backtest/live boundary.

mod common;

#[test]
fn backtest_equals_mock_live() {
    let backtest = common::run_backtest(common::strategy());
    // A deliberately tiny channel forces backpressure in the producer thread,
    // exercising the async-edge transport rather than a trivial drain.
    let mock_live = common::run_mock_live(common::strategy(), 4);

    assert_eq!(
        backtest, mock_live,
        "backtest and mock-live diverged — parity is broken"
    );

    // Sanity: the run was non-trivial (it actually traded).
    assert!(backtest.orders_submitted >= 1);
    assert!(!backtest.fills.is_empty());
}

#[test]
fn parity_holds_across_channel_capacities() {
    // The result must not depend on how the transport buffers events.
    let baseline = common::run_backtest(common::strategy());
    for capacity in [1usize, 2, 8, 64] {
        let live = common::run_mock_live(common::strategy(), capacity);
        assert_eq!(
            baseline, live,
            "parity broke at channel capacity {capacity}"
        );
    }
}
