// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Walk-forward window generation for out-of-sample validation.

use akadro_core::{Bar, Timestamp};

/// A single walk-forward split: train on `[train_start, train_end)`, then
/// evaluate out-of-sample on `[test_start, test_end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// In-sample (training/optimization) start.
    pub train_start: Timestamp,
    /// In-sample end (= test start).
    pub train_end: Timestamp,
    /// Out-of-sample start.
    pub test_start: Timestamp,
    /// Out-of-sample end.
    pub test_end: Timestamp,
}

/// Generate rolling walk-forward windows over `[start, end)`.
///
/// Each window trains on `train_len_ns` then tests on the following
/// `test_len_ns`; the window origin advances by `step_ns` (use `step_ns ==
/// test_len_ns` for non-overlapping, contiguous test periods). Windows whose
/// test period would run past `end` are dropped. Returns empty if any length is
/// non-positive.
#[must_use]
pub fn walk_forward(
    start: Timestamp,
    end: Timestamp,
    train_len_ns: i64,
    test_len_ns: i64,
    step_ns: i64,
) -> Vec<Window> {
    let mut out = Vec::new();
    if train_len_ns <= 0 || test_len_ns <= 0 || step_ns <= 0 {
        return out;
    }
    let end = end.as_nanos();
    let mut s = start.as_nanos();
    while let Some(train_end) = s.checked_add(train_len_ns) {
        let Some(test_end) = train_end.checked_add(test_len_ns) else {
            break;
        };
        if test_end > end {
            break;
        }
        out.push(Window {
            train_start: Timestamp::from_nanos(s),
            train_end: Timestamp::from_nanos(train_end),
            test_start: Timestamp::from_nanos(train_end),
            test_end: Timestamp::from_nanos(test_end),
        });
        match s.checked_add(step_ns) {
            Some(next) => s = next,
            None => break,
        }
    }
    out
}

/// Rolling walk-forward with **purge** and **embargo** (López de Prado), to
/// remove train/test leakage when labels span time.
///
/// Each window trains on `train_len_ns`, then — after a `purge_ns` gap — tests on
/// `test_len_ns`. The gap `[train_end, test_start)` purges training observations
/// whose labels would overlap the test set. An additional `embargo_ns` buffer is
/// reserved *after* each test (no window's test runs into the last `embargo_ns`
/// before `end`), guarding against serial-correlation leakage into any subsequent
/// training. The origin advances by `step_ns`.
///
/// Returns empty if `train_len_ns`/`test_len_ns`/`step_ns` are non-positive or
/// `purge_ns`/`embargo_ns` are negative. Set `purge_ns == 0 && embargo_ns == 0`
/// to recover [`walk_forward`].
#[must_use]
pub fn walk_forward_purged(
    start: Timestamp,
    end: Timestamp,
    train_len_ns: i64,
    test_len_ns: i64,
    step_ns: i64,
    purge_ns: i64,
    embargo_ns: i64,
) -> Vec<Window> {
    let mut out = Vec::new();
    if train_len_ns <= 0 || test_len_ns <= 0 || step_ns <= 0 || purge_ns < 0 || embargo_ns < 0 {
        return out;
    }
    let end = end.as_nanos();
    let mut s = start.as_nanos();
    while let Some(train_end) = s.checked_add(train_len_ns) {
        let Some(test_start) = train_end.checked_add(purge_ns) else {
            break;
        };
        let Some(test_end) = test_start.checked_add(test_len_ns) else {
            break;
        };
        // The embargo buffer must also fit before `end`.
        let Some(reserved_end) = test_end.checked_add(embargo_ns) else {
            break;
        };
        if reserved_end > end {
            break;
        }
        out.push(Window {
            train_start: Timestamp::from_nanos(s),
            train_end: Timestamp::from_nanos(train_end),
            test_start: Timestamp::from_nanos(test_start),
            test_end: Timestamp::from_nanos(test_end),
        });
        match s.checked_add(step_ns) {
            Some(next) => s = next,
            None => break,
        }
    }
    out
}

/// **Anchored** (expanding) walk-forward: the training window's *start is fixed*
/// at `start` and its end grows by `step_ns` each fold; the test always follows
/// the (growing) train end for `test_len_ns`.
///
/// Fold `k` trains on `[start, start + initial_train_len_ns + k·step_ns)` and
/// tests on the next `test_len_ns`. Windows whose test runs past `end` are
/// dropped. Returns empty on any non-positive length. Use this (vs the rolling
/// [`walk_forward`]) when you want the model to keep all history, not just a
/// fixed look-back.
#[must_use]
pub fn walk_forward_anchored(
    start: Timestamp,
    end: Timestamp,
    initial_train_len_ns: i64,
    test_len_ns: i64,
    step_ns: i64,
) -> Vec<Window> {
    let mut out = Vec::new();
    if initial_train_len_ns <= 0 || test_len_ns <= 0 || step_ns <= 0 {
        return out;
    }
    let start_ns = start.as_nanos();
    let end = end.as_nanos();
    let mut train_len = initial_train_len_ns;
    while let Some(train_end) = start_ns.checked_add(train_len) {
        let Some(test_end) = train_end.checked_add(test_len_ns) else {
            break;
        };
        if test_end > end {
            break;
        }
        out.push(Window {
            train_start: start, // anchored
            train_end: Timestamp::from_nanos(train_end),
            test_start: Timestamp::from_nanos(train_end),
            test_end: Timestamp::from_nanos(test_end),
        });
        match train_len.checked_add(step_ns) {
            Some(next) => train_len = next,
            None => break,
        }
    }
    out
}

/// Drive a backtest over each walk-forward `window`, threading caller `state`
/// (e.g. a fitted model / warm-start parameters) from one fold into the next.
///
/// For every window, `fold` receives the window, the **train** bars (`ts ∈
/// [train_start, train_end)`), the **test** bars (`ts ∈ [test_start, test_end)`),
/// and `&mut state`. It typically fits on the train slice, then constructs and
/// runs an [`Engine`](akadro_engine) over the test slice and returns its
/// `RunReport` (the generic `R`). Returns the per-fold results and the final
/// threaded state.
///
/// This is pure orchestration above the engine — slicing + state hand-off — so it
/// adds no look-ahead surface (each fold runs its own fresh engine). `bars` are
/// assumed globally ordered by timestamp.
pub fn run_walk_forward<S, R, F>(
    bars: &[Bar],
    windows: &[Window],
    mut state: S,
    mut fold: F,
) -> (Vec<R>, S)
where
    F: FnMut(&Window, &[Bar], &[Bar], &mut S) -> R,
{
    let mut reports = Vec::with_capacity(windows.len());
    for w in windows {
        let (tr0, tr1) = (w.train_start.as_nanos(), w.train_end.as_nanos());
        let (te0, te1) = (w.test_start.as_nanos(), w.test_end.as_nanos());
        let train: Vec<Bar> = bars
            .iter()
            .copied()
            .filter(|b| {
                let t = b.ts.as_nanos();
                t >= tr0 && t < tr1
            })
            .collect();
        let test: Vec<Bar> = bars
            .iter()
            .copied()
            .filter(|b| {
                let t = b.ts.as_nanos();
                t >= te0 && t < te1
            })
            .collect();
        reports.push(fold(w, &train, &test, &mut state));
    }
    (reports, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: i64) -> Timestamp {
        Timestamp::from_nanos(n)
    }

    #[test]
    fn rolling_windows() {
        let w = walk_forward(t(0), t(100), 40, 20, 20);
        assert_eq!(w.len(), 3);
        assert_eq!(
            w[0],
            Window {
                train_start: t(0),
                train_end: t(40),
                test_start: t(40),
                test_end: t(60)
            }
        );
        assert_eq!(w[1].train_start, t(20));
        assert_eq!(
            w[2],
            Window {
                train_start: t(40),
                train_end: t(80),
                test_start: t(80),
                test_end: t(100)
            }
        );
    }

    #[test]
    fn non_overlapping_when_step_equals_test_len() {
        // train 30, test 10, step 10.
        let w = walk_forward(t(0), t(100), 30, 10, 10);
        // first test [30,40); subsequent tests are contiguous [40,50)... but each
        // re-trains on the preceding 30, so origins advance by 10.
        assert!(!w.is_empty());
        for win in &w {
            assert_eq!(win.test_start, win.train_end);
            assert_eq!(win.test_end.as_nanos() - win.test_start.as_nanos(), 10);
        }
    }

    #[test]
    fn empty_on_bad_params() {
        assert!(walk_forward(t(0), t(100), 0, 20, 20).is_empty());
        assert!(walk_forward(t(0), t(100), 40, 0, 20).is_empty());
        assert!(walk_forward(t(0), t(100), 40, 20, 0).is_empty());
        // train+test doesn't fit in the range.
        assert!(walk_forward(t(0), t(10), 40, 20, 20).is_empty());
    }
}

#[cfg(test)]
mod cov_tests {
    use super::*;
    #[test]
    fn overflow_breaks_cleanly() {
        // train_end checked_add overflow at the while-let -> loop never enters.
        let w = walk_forward(
            Timestamp::from_nanos(i64::MAX - 5),
            Timestamp::from_nanos(i64::MAX),
            1000,
            1000,
            1000,
        );
        assert!(w.is_empty());
    }

    #[test]
    fn test_end_overflow_breaks() {
        // train_end fits (s + train), but train_end + test overflows -> inner break.
        let w = walk_forward(
            Timestamp::from_nanos(i64::MAX - 2000),
            Timestamp::from_nanos(i64::MAX),
            1000, // train_end = MAX-1000 (fits)
            2000, // train_end + 2000 overflows
            1000,
        );
        assert!(w.is_empty());
    }

    #[test]
    fn step_overflow_breaks_after_one_window() {
        // One window fits, then s + step overflows -> the step `None => break`.
        let w = walk_forward(
            Timestamp::from_nanos(1),
            Timestamp::from_nanos(i64::MAX),
            1,
            1,
            i64::MAX, // 1 + i64::MAX overflows
        );
        assert_eq!(w.len(), 1);
    }
}

#[cfg(test)]
mod ext_tests {
    use super::*;
    use akadro_core::{InstrumentId, Price, Qty};

    fn t(n: i64) -> Timestamp {
        Timestamp::from_nanos(n)
    }

    fn bar(ts: i64, px: i64) -> Bar {
        Bar::new(
            InstrumentId::new(0),
            t(ts),
            Price::from_raw(px),
            Price::from_raw(px),
            Price::from_raw(px),
            Price::from_raw(px),
            Qty::from_raw(1),
        )
    }

    #[test]
    fn purged_inserts_gap_before_test() {
        // train 30, test 10, step 30, purge 5, embargo 0.
        let w = walk_forward_purged(t(0), t(100), 30, 10, 30, 5, 0);
        assert!(!w.is_empty());
        let first = w[0];
        assert_eq!(first.train_start, t(0));
        assert_eq!(first.train_end, t(30));
        assert_eq!(first.test_start, t(35)); // train_end + purge
        assert_eq!(first.test_end, t(45));
        // The purge gap is exactly purge_ns wide.
        assert_eq!(first.test_start.as_nanos() - first.train_end.as_nanos(), 5);
    }

    #[test]
    fn purge_zero_matches_plain_walk_forward() {
        let a = walk_forward(t(0), t(100), 40, 20, 20);
        let b = walk_forward_purged(t(0), t(100), 40, 20, 20, 0, 0);
        assert_eq!(a, b);
    }

    #[test]
    fn embargo_reserves_buffer_before_end() {
        // Without embargo a test ending exactly at `end` is kept; with embargo the
        // buffer must also fit, so that final window is dropped.
        let no_emb = walk_forward_purged(t(0), t(60), 40, 20, 20, 0, 0);
        let emb = walk_forward_purged(t(0), t(60), 40, 20, 20, 0, 5);
        assert_eq!(no_emb.len(), 1); // test [40,60) fits exactly
        assert!(emb.is_empty()); // test_end 60 + embargo 5 = 65 > 60
    }

    #[test]
    fn purged_rejects_bad_params() {
        assert!(walk_forward_purged(t(0), t(100), 0, 20, 20, 5, 5).is_empty());
        assert!(walk_forward_purged(t(0), t(100), 40, 20, 20, -1, 5).is_empty());
        assert!(walk_forward_purged(t(0), t(100), 40, 20, 20, 5, -1).is_empty());
    }

    #[test]
    fn purged_overflow_paths() {
        // train_end overflow.
        assert!(walk_forward_purged(t(i64::MAX - 1), t(i64::MAX), 1000, 10, 10, 0, 0).is_empty());
        // test_start (purge) overflow: train_end fits, +purge overflows.
        assert!(
            walk_forward_purged(t(i64::MAX - 1000), t(i64::MAX), 100, 10, 10, i64::MAX, 0)
                .is_empty()
        );
        // test_end overflow: train_end+purge fit, +test_len overflows.
        assert!(
            walk_forward_purged(t(i64::MAX - 200), t(i64::MAX), 100, i64::MAX, 10, 0, 0).is_empty()
        );
        // embargo (reserved_end) overflow: test_end fits, +embargo overflows.
        assert!(
            walk_forward_purged(t(i64::MAX - 200), t(i64::MAX), 50, 50, 10, 0, i64::MAX).is_empty()
        );
        // step overflow after one window.
        let w = walk_forward_purged(t(1), t(i64::MAX), 1, 1, i64::MAX, 0, 0);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn anchored_keeps_start_and_expands() {
        // initial train 30, test 10, step 10.
        let w = walk_forward_anchored(t(0), t(100), 30, 10, 10);
        assert!(w.len() >= 2);
        // Every train starts at the anchor; train_end grows by step each fold.
        for win in &w {
            assert_eq!(win.train_start, t(0));
            assert_eq!(win.test_start, win.train_end);
        }
        assert_eq!(w[0].train_end, t(30));
        assert_eq!(w[1].train_end, t(40)); // expanded by step
        assert!(w[1].train_end.as_nanos() > w[0].train_end.as_nanos());
    }

    #[test]
    fn anchored_rejects_bad_params_and_overflow() {
        assert!(walk_forward_anchored(t(0), t(100), 0, 10, 10).is_empty());
        assert!(walk_forward_anchored(t(0), t(100), 30, 0, 10).is_empty());
        assert!(walk_forward_anchored(t(0), t(100), 30, 10, 0).is_empty());
        // train_end overflow.
        assert!(walk_forward_anchored(t(i64::MAX - 1), t(i64::MAX), 1000, 10, 10).is_empty());
        // test_end overflow: train fits, +test overflows.
        assert!(walk_forward_anchored(t(i64::MAX - 100), t(i64::MAX), 50, i64::MAX, 10).is_empty());
        // train_len step overflow after one window.
        let w = walk_forward_anchored(t(0), t(i64::MAX), 1, 1, i64::MAX);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn run_threads_state_and_slices_bars() {
        // Bars every 10ns from 0..100.
        let bars: Vec<Bar> = (0..10).map(|i| bar(i * 10, 100 + i)).collect();
        let windows = walk_forward(t(0), t(100), 40, 20, 20); // 3 folds
        assert_eq!(windows.len(), 3);

        // State = running count of folds; report = (n_train, n_test) bar counts.
        let (reports, final_count) =
            run_walk_forward(&bars, &windows, 0usize, |_w, train, test, count| {
                *count += 1;
                (train.len(), test.len())
            });

        assert_eq!(reports.len(), 3);
        assert_eq!(final_count, 3); // state threaded across all folds
        // Fold 0: train [0,40) -> ts 0,10,20,30 = 4 bars; test [40,60) -> 40,50 = 2.
        assert_eq!(reports[0], (4, 2));
        // Fold 2: train [40,80) -> 40,50,60,70 = 4; test [80,100) -> 80,90 = 2.
        assert_eq!(reports[2], (4, 2));
    }

    #[test]
    fn run_empty_windows_returns_state() {
        let bars: Vec<Bar> = vec![bar(0, 100)];
        let (reports, st): (Vec<()>, i32) = run_walk_forward(&bars, &[], 7, |_, _, _, _| ());
        assert!(reports.is_empty());
        assert_eq!(st, 7);
    }
}
