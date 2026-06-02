// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `WalkForwardBacktest` — the framework-owned, fit-on-train-only OOS runner.
//! These tests pin the structural IS/OOS boundary and the parity-untouched proof.

mod common;

use akadro_analytics::{slice_window, walk_forward, walk_forward_anchored};
use akadro_backtest::{FillConfig, HistoricalFeed, SimulatedExchange, WalkForwardBacktest};
use akadro_core::{Bar, Qty, Timestamp};
use akadro_engine::Engine;
use sma_crossover::SmaCross;

use common::{INSTRUMENT, bars, initial_cash, spec};

fn t(n: i64) -> Timestamp {
    Timestamp::from_nanos(n)
}

/// Derive SMA windows from the TRAIN slice only — a stand-in for any in-sample fit /
/// normalization. `fast < slow` always holds (fast = slow/2, slow >= 2).
fn fit_params(train: &[Bar]) -> (usize, usize) {
    let slow = (train.len() / 5).clamp(2, 20);
    let fast = (slow / 2).max(1);
    (fast, slow)
}
fn make_strat(p: &(usize, usize)) -> SmaCross {
    SmaCross::new(INSTRUMENT, p.0, p.1, Qty::from_raw(1))
}

#[test]
fn fit_is_train_only_and_state_threads_per_fold() {
    let bars = bars();
    let specs = [spec()];
    let windows = walk_forward(t(0), t(121), 40, 20, 20);
    assert!(windows.len() >= 3);
    let wf = WalkForwardBacktest::new(
        &specs,
        initial_cash(),
        FillConfig::default(),
        &bars,
        &windows,
    );

    // State threads a per-fold record of the train sizes the fit step actually saw.
    let (folds, train_sizes) = wf
        .run(
            Vec::<usize>::new(),
            |train, st: &mut Vec<usize>| {
                st.push(train.len());
                fit_params(train)
            },
            make_strat,
        )
        .expect("ordered bars");

    assert_eq!(folds.len(), windows.len());
    assert_eq!(
        train_sizes.len(),
        windows.len(),
        "fit ran once per fold (state threaded)"
    );
    for (f, &sz) in folds.iter().zip(&train_sizes) {
        assert_eq!(f.train_bars, sz, "OosFold.train_bars matches what fit saw");
    }
    // The `fit` closure's signature (`Fn(&[Bar], &mut State)`) has NO `test`
    // parameter: it CANNOT read the out-of-sample bars — that absence is the
    // compile-time structural IS/OOS boundary this runner exists to provide.
}

#[test]
fn single_shared_config_reaches_every_fold() {
    let bars = bars();
    let specs = [spec()];
    // Full-period (40-bar) test windows so the triangle wave forces a crossover and
    // the SMA strategy actually trades out-of-sample (a half-period slice is monotonic).
    let windows = walk_forward(t(0), t(121), 40, 40, 40);

    // Run once with a 5bps fee and once friction-free; the SAME config must drive
    // every fold, so the fee toggle must visibly change the fills' fees.
    let run_with = |fill: FillConfig| {
        WalkForwardBacktest::new(&specs, initial_cash(), fill, &bars, &windows)
            .run((), |tr, (): &mut ()| fit_params(tr), make_strat)
            .expect("ordered")
            .0
    };
    // 100bps (1%) so the fee registers on the small synthetic notional (a 5bps fee on
    // a ~100-raw price truncates to 0 — that would test nothing).
    let fee = FillConfig {
        fee_bps: 100,
        ..Default::default()
    };
    let with_fee = run_with(fee);
    let without_fee = run_with(FillConfig::default());

    let total_fills: usize = with_fee.iter().map(|f| f.report.fills.len()).sum();
    assert!(
        total_fills > 0,
        "the SMA strategy must trade out-of-sample at least once"
    );
    // With fees, the traded folds charge a (non-negative) fee; friction-free they do
    // not — proving the one supplied FillConfig was applied on every fold, not lost.
    let fees_charged: i128 = with_fee.iter().map(|f| f.report.trading_fees.raw()).sum();
    let fees_zero: i128 = without_fee
        .iter()
        .map(|f| f.report.trading_fees.raw())
        .sum();
    assert!(
        fees_charged > 0,
        "5bps config must produce fees on the traded folds"
    );
    assert_eq!(fees_zero, 0, "friction-free config must produce zero fees");
}

#[test]
fn run_is_deterministic() {
    let bars = bars();
    let specs = [spec()];
    let windows = walk_forward(t(0), t(121), 40, 20, 20);
    let go = || {
        WalkForwardBacktest::new(
            &specs,
            initial_cash(),
            FillConfig::default(),
            &bars,
            &windows,
        )
        .run((), |tr, (): &mut ()| fit_params(tr), make_strat)
        .expect("ordered")
        .0
    };
    assert_eq!(go(), go(), "same inputs -> byte-identical per-fold reports");
}

#[test]
fn fold_matches_a_hand_built_oos_engine() {
    // Parity-untouched proof: the runner only composes Engine::new(...).run(), so a
    // hand-built OOS engine over the same test slice with the same params + config
    // must reproduce the fold's report exactly (no extra execution path is added).
    let bars = bars();
    let specs = [spec()];
    let fill = FillConfig {
        fee_bps: 5,
        ..Default::default()
    };
    let windows = walk_forward(t(0), t(121), 40, 20, 20);

    let folds = WalkForwardBacktest::new(&specs, initial_cash(), fill, &bars, &windows)
        .run((), |tr, (): &mut ()| fit_params(tr), make_strat)
        .expect("ordered")
        .0;

    // Reproduce fold 0 by hand.
    let (train0, test0) = slice_window(&bars, &windows[0]);
    let params = fit_params(&train0);
    let hand = Engine::new(
        &specs,
        initial_cash(),
        HistoricalFeed::from_bars(test0),
        SimulatedExchange::with_config(specs.to_vec(), fill),
        make_strat(&params),
    )
    .expect("engine")
    .run();
    assert_eq!(hand, folds[0].report, "orchestrator adds no execution path");
}

#[test]
fn train_derived_params_vary_with_anchored_windows() {
    // Anchored (expanding) windows grow the train slice each fold, so a train-derived
    // parameter must take different values across folds (it really depends on train).
    let bars = bars();
    let specs = [spec()];
    let windows = walk_forward_anchored(t(0), t(121), 30, 15, 15);
    assert!(windows.len() >= 3);
    let (_folds, params_seen) = WalkForwardBacktest::new(
        &specs,
        initial_cash(),
        FillConfig::default(),
        &bars,
        &windows,
    )
    .run(
        Vec::<(usize, usize)>::new(),
        |tr, st: &mut Vec<(usize, usize)>| {
            let p = fit_params(tr);
            st.push(p);
            p
        },
        make_strat,
    )
    .expect("ordered");
    assert!(
        params_seen.iter().any(|p| *p != params_seen[0]),
        "expanding train must yield differing train-fit params, got {params_seen:?}"
    );
}

#[test]
fn runner_is_window_generator_agnostic() {
    let bars = bars();
    let specs = [spec()];
    for windows in [
        walk_forward(t(0), t(121), 40, 20, 20),
        walk_forward_anchored(t(0), t(121), 30, 15, 15),
    ] {
        let folds = WalkForwardBacktest::new(
            &specs,
            initial_cash(),
            FillConfig::default(),
            &bars,
            &windows,
        )
        .run((), |tr, (): &mut ()| fit_params(tr), make_strat)
        .expect("ordered")
        .0;
        assert_eq!(folds.len(), windows.len(), "one fold per window");
    }
}

#[test]
fn unordered_bars_are_rejected_before_any_fold() {
    // The framework owns each fold's feed, so it validates the input once up front.
    let mut b = bars();
    b.swap(0, 5); // break monotonic order
    let specs = [spec()];
    let windows = walk_forward(t(0), t(121), 40, 20, 20);
    let res = WalkForwardBacktest::new(&specs, initial_cash(), FillConfig::default(), &b, &windows)
        .run((), |tr, (): &mut ()| fit_params(tr), make_strat);
    assert!(
        res.is_err(),
        "out-of-order bars must fail fast, not run a corrupt study"
    );
}
