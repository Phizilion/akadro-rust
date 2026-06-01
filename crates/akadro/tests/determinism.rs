// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Determinism: a run is exactly reproducible, and a parameter sweep gives the
//! same per-config result whether run sequentially or across threads.

mod common;

#[test]
fn rerun_is_bit_identical() {
    let a = common::run_backtest(common::strategy());
    let b = common::run_backtest(common::strategy());
    assert_eq!(a, b);
}

#[test]
fn parallel_sweep_matches_sequential() {
    let params = [(2usize, 8usize), (3, 10), (5, 20), (4, 16)];

    // Sequential.
    let sequential: Vec<_> = params
        .iter()
        .map(|&(f, s)| (f, s, common::run_backtest(common::strategy_fs(f, s))))
        .collect();

    // Parallel: one OS thread per config; results must be identical and
    // order-independent (we re-sort by params before comparing).
    let handles: Vec<_> = params
        .iter()
        .map(|&(f, s)| {
            std::thread::spawn(move || (f, s, common::run_backtest(common::strategy_fs(f, s))))
        })
        .collect();
    let mut parallel: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("thread ok"))
        .collect();
    parallel.sort_by_key(|(f, s, _)| (*f, *s));

    let mut sequential_sorted = sequential;
    sequential_sorted.sort_by_key(|(f, s, _)| (*f, *s));

    assert_eq!(sequential_sorted, parallel);
}
