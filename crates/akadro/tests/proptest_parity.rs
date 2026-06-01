// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Property test: backtest↔mock-live parity must hold for ARBITRARY price
//! paths, not just the hand-picked fixture. For any random sequence of closes,
//! the backtest feed and the threaded mock-live feed must produce a bit-identical
//! `RunReport`.

mod common;

use akadro_core::Event;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn parity_holds_for_arbitrary_prices(closes in proptest::collection::vec(50i64..200, 12..80)) {
        let bars = common::bars_from_closes(&closes);
        let backtest = common::run_backtest_bars(common::strategy(), bars.clone());
        let events: Vec<Event> = bars.into_iter().map(Event::Bar).collect();
        let live = common::run_mock_live_events(common::strategy(), events, 4);
        prop_assert_eq!(backtest, live);
    }

    // Determinism for arbitrary inputs: rerunning the same backtest is identical.
    #[test]
    fn backtest_is_deterministic_for_arbitrary_prices(
        closes in proptest::collection::vec(50i64..200, 12..80)
    ) {
        let bars = common::bars_from_closes(&closes);
        let a = common::run_backtest_bars(common::strategy(), bars.clone());
        let b = common::run_backtest_bars(common::strategy(), bars);
        prop_assert_eq!(a, b);
    }
}
