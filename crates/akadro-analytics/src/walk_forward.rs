// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Walk-forward window generation for out-of-sample validation.

use akadro_core::{Bar, Timestamp};
#[cfg(feature = "escape-hatch")]
use akadro_core::{BarOrderError, check_bars_ordered};
use akadro_engine::{EquityPoint, RunReport};

use crate::{PerformanceReport, returns_from_equity, walk_forward_efficiency};

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
///
/// When your signal's labels span time (so a train sample's outcome overlaps the
/// test period), use [`walk_forward_purged`] instead to insert a purge gap +
/// embargo and remove train/test leakage; purge/embargo is opt-in by design (the
/// label horizon is strategy-specific, so a non-zero default would be wrong).
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

/// Split `bars` into a `window`'s `(train, test)` slices on the half-open intervals
/// `[train_start, train_end)` and `[test_start, test_end)` (by timestamp). A bar
/// exactly at `train_end` (= `test_start`) lands in **test**, not train; a bar at
/// `test_end` lands in neither. The single slicing source of truth shared by the
/// framework-owned `WalkForwardBacktest` runner and the `escape-hatch`
/// `run_walk_forward` (DRY) — and the building block for explicit custom walk-forward
/// orchestration.
#[must_use]
pub fn slice_window(bars: &[Bar], window: &Window) -> (Vec<Bar>, Vec<Bar>) {
    let (tr0, tr1) = (window.train_start.as_nanos(), window.train_end.as_nanos());
    let (te0, te1) = (window.test_start.as_nanos(), window.test_end.as_nanos());
    let in_range = |b: &Bar, lo: i64, hi: i64| {
        let t = b.ts.as_nanos();
        t >= lo && t < hi
    };
    let train = bars
        .iter()
        .copied()
        .filter(|b| in_range(b, tr0, tr1))
        .collect();
    let test = bars
        .iter()
        .copied()
        .filter(|b| in_range(b, te0, te1))
        .collect();
    (train, test)
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
/// adds no look-ahead surface (each fold runs its own fresh engine).
///
/// # Gated escape hatch — not the look-ahead-safe path (feature `escape-hatch`)
/// It hands the closure **both** `train` and `test`, so it cannot stop a caller from
/// fitting on `test` or selecting parameters by out-of-sample performance — the
/// classic walk-forward lie (the type system's look-ahead guarantee stops at one
/// `Engine::run`; this is above it). Because that is a leakage footgun, it is
/// **off by default** behind the `escape-hatch` feature. The default, safe surface
/// is `akadro_backtest::WalkForwardBacktest` (its `fit` step never receives `test`)
/// plus [`slice_window`] + [`check_bars_ordered`] if you must orchestrate by hand.
///
/// `bars` are **assumed already time-ordered**; this fn does not re-validate them.
/// Pass bars from the `akadro-data` cache (ordering enforced) or validate via
/// [`run_walk_forward_checked`] / [`check_bars_ordered`].
#[cfg(feature = "escape-hatch")]
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
        let (train, test) = slice_window(bars, w);
        reports.push(fold(w, &train, &test, &mut state));
    }
    (reports, state)
}

/// [`run_walk_forward`] but **validates that `bars` is a well-ordered event stream
/// first** ([`check_bars_ordered`]) — the blessed entry point for externally-sourced
/// bars. Unsorted / duplicate-timestamp bars silently corrupt every fold's equity
/// (the engine processes events in arrival order), so this fails fast before any
/// fold runs rather than producing fake analytics.
///
/// The closure still receives both `train` and `test` (this only guards the *input*,
/// not IS/OOS contamination — for that use `akadro_backtest::WalkForwardBacktest`,
/// whose `fit` step never receives `test`). It is therefore part of the same
/// **off-by-default `escape-hatch`** surface as [`run_walk_forward`].
///
/// # Errors
/// [`BarOrderError`] if `bars` is not non-decreasing / has a same-instrument
/// duplicate timestamp; the `fold` closure is then never invoked.
#[cfg(feature = "escape-hatch")]
pub fn run_walk_forward_checked<S, R, F>(
    bars: &[Bar],
    windows: &[Window],
    state: S,
    fold: F,
) -> Result<(Vec<R>, S), BarOrderError>
where
    F: FnMut(&Window, &[Bar], &[Bar], &mut S) -> R,
{
    check_bars_ordered(bars)?;
    Ok(run_walk_forward(bars, windows, state, fold))
}

/// Aggregate out-of-sample performance across walk-forward folds — the **only**
/// blessed OOS-aggregation path, so the level-stitch mistake is unreachable through
/// the library.
///
/// It pools the per-fold **out-of-sample return series** (each fold re-based to its
/// own start via [`returns_from_equity`]) and computes one [`PerformanceReport`] over
/// the concatenation. It deliberately does **not** stitch per-fold equity *levels*
/// into one curve: the jump from one fold's last equity to the next fold's starting
/// capital is a spurious `(next_start − prev_end)/prev_end` return that would poison
/// Sharpe/drawdown. Unmeasurable folds (a blow-up / `< 2` points, where
/// [`returns_from_equity`] is `None`) are **counted**, never dropped or zeroed (that
/// would be survivorship bias).
///
/// ```
/// use akadro_analytics::WalkForwardSummary;
/// use akadro_engine::EquityPoint;
/// use akadro_core::{Money, Timestamp};
///
/// let pt = |e: i128| EquityPoint { ts: Timestamp::EPOCH, equity: Money::from_raw(e) };
/// let fold_a = [pt(100), pt(110), pt(121)];
/// let fold_b = [pt(100), pt(0)]; // a blow-up -> unmeasurable, counted not dropped
/// let s = WalkForwardSummary::new(&[&fold_a, &fold_b], 252.0, Some(0.30));
/// assert_eq!(s.folds, 2);
/// assert_eq!(s.measured_folds, 1);
/// assert_eq!(s.unmeasurable_folds, 1);
/// assert!(s.pooled.is_some());     // pooled over the survivor's OOS returns
/// assert!(s.efficiency.is_some()); // in-sample 0.30 > 0, so WFE is defined
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct WalkForwardSummary {
    /// Total folds supplied.
    pub folds: usize,
    /// Folds that yielded a usable OOS return series.
    pub measured_folds: usize,
    /// Folds excluded as unmeasurable (blow-up / too few points).
    pub unmeasurable_folds: usize,
    /// Number of pooled OOS return periods (sum over measured folds).
    pub pooled_periods: usize,
    /// Performance over the pooled OOS returns; `None` if no fold was measurable.
    pub pooled: Option<PerformanceReport>,
    /// Walk-forward efficiency = pooled OOS total return / in-sample total return.
    /// `Some` only when an in-sample total return was supplied **and is strictly
    /// positive** and `pooled` exists — excluding the `is ≤ 0` sign-inversion / `±∞`
    /// poison of a raw [`walk_forward_efficiency`] call.
    pub efficiency: Option<f64>,
}

impl WalkForwardSummary {
    /// Build from per-fold OOS equity curves. `periods_per_year` annualizes the
    /// pooled report (see [`infer_periods_per_year`](crate::infer_periods_per_year)
    /// to derive it); `is_total_return` is the in-sample total return for the
    /// efficiency ratio, or `None` to skip it.
    #[must_use]
    pub fn new(
        oos_curves: &[&[EquityPoint]],
        periods_per_year: f64,
        is_total_return: Option<f64>,
    ) -> Self {
        let mut pooled_returns: Vec<f64> = Vec::new();
        let mut measured = 0usize;
        let mut unmeasurable = 0usize;
        for curve in oos_curves {
            match returns_from_equity(curve) {
                Some(rs) => {
                    measured += 1;
                    pooled_returns.extend(rs);
                }
                None => unmeasurable += 1,
            }
        }
        let pooled = PerformanceReport::from_returns(&pooled_returns, periods_per_year, 0.0, 0.0);
        let efficiency = match (is_total_return, pooled.as_ref()) {
            (Some(is_), Some(p)) if is_ > 0.0 => Some(walk_forward_efficiency(p.total_return, is_)),
            _ => None,
        };
        WalkForwardSummary {
            folds: oos_curves.len(),
            measured_folds: measured,
            unmeasurable_folds: unmeasurable,
            pooled_periods: pooled_returns.len(),
            pooled,
            efficiency,
        }
    }

    /// Convenience over [`new`](Self::new) for per-fold [`RunReport`]s — pools their
    /// `equity_curve`s. This is the place the level-stitch bug was easy to introduce;
    /// it does the pooling correctly.
    #[must_use]
    pub fn from_reports(
        reports: &[RunReport],
        periods_per_year: f64,
        is_total_return: Option<f64>,
    ) -> Self {
        let curves: Vec<&[EquityPoint]> =
            reports.iter().map(|r| r.equity_curve.as_slice()).collect();
        Self::new(&curves, periods_per_year, is_total_return)
    }
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

    #[cfg(feature = "escape-hatch")]
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

    #[cfg(feature = "escape-hatch")]
    #[test]
    fn run_empty_windows_returns_state() {
        let bars: Vec<Bar> = vec![bar(0, 100)];
        let (reports, st): (Vec<()>, i32) = run_walk_forward(&bars, &[], 7, |_, _, _, _| ());
        assert!(reports.is_empty());
        assert_eq!(st, 7);
    }

    #[test]
    fn slice_window_is_half_open() {
        // Bars at exactly train_end and test_end probe the boundary convention.
        let bars: Vec<Bar> = [0, 40, 60].iter().map(|&ts| bar(ts, 1)).collect();
        let w = Window {
            train_start: t(0),
            train_end: t(40),
            test_start: t(40),
            test_end: t(60),
        };
        let (train, test) = slice_window(&bars, &w);
        // ts 0 -> train; ts 40 (= train_end = test_start) -> test, NOT train;
        // ts 60 (= test_end) -> neither (half-open).
        assert_eq!(
            train.iter().map(|b| b.ts.as_nanos()).collect::<Vec<_>>(),
            vec![0]
        );
        assert_eq!(
            test.iter().map(|b| b.ts.as_nanos()).collect::<Vec<_>>(),
            vec![40]
        );
    }

    #[cfg(feature = "escape-hatch")]
    #[test]
    fn run_checked_rejects_unsorted_without_invoking_fold() {
        use std::cell::Cell;
        // Descending ts -> the engine would run backward and silently corrupt equity.
        let bars: Vec<Bar> = vec![bar(0, 100), bar(30, 100), bar(10, 100)];
        let windows = walk_forward(t(0), t(40), 20, 10, 10);
        let invoked = Cell::new(0u32);
        let result: Result<(Vec<()>, ()), _> =
            run_walk_forward_checked(&bars, &windows, (), |_w, _tr, _te, _st| {
                invoked.set(invoked.get() + 1);
            });
        assert!(matches!(
            result,
            Err(BarOrderError::NotAscending { idx: 2, .. })
        ));
        assert_eq!(invoked.get(), 0, "fold must not run when input is rejected");
    }

    #[cfg(feature = "escape-hatch")]
    #[test]
    fn run_checked_matches_raw_on_valid_bars() {
        let bars: Vec<Bar> = (0..10).map(|i| bar(i * 10, 100 + i)).collect();
        let windows = walk_forward(t(0), t(100), 40, 20, 20);
        let raw = run_walk_forward(&bars, &windows, 0usize, |_w, tr, te, c| {
            *c += 1;
            (tr.len(), te.len())
        });
        let checked = run_walk_forward_checked(&bars, &windows, 0usize, |_w, tr, te, c| {
            *c += 1;
            (tr.len(), te.len())
        })
        .expect("sorted bars accepted");
        assert_eq!(raw, checked);
    }

    // --- WalkForwardSummary (m6) ---

    fn pt(eq: i128) -> EquityPoint {
        EquityPoint {
            ts: t(0),
            equity: akadro_core::Money::from_raw(eq),
        }
    }

    #[test]
    fn summary_pools_returns_not_levels() {
        // Two folds, each +10% then +10% (curve [100,110,121]). Correct pooling =
        // four +10% returns -> total (1.1^4 - 1) ≈ 0.4641. The level-stitch mistake
        // would concatenate six points into FIVE returns, injecting a spurious
        // (100-121)/121 drop at the boundary — a different, wrong answer.
        let f0 = [pt(100), pt(110), pt(121)];
        let f1 = [pt(100), pt(110), pt(121)];
        let s = WalkForwardSummary::new(&[&f0, &f1], 252.0, None);
        assert_eq!(s.folds, 2);
        assert_eq!(s.measured_folds, 2);
        assert_eq!(s.unmeasurable_folds, 0);
        assert_eq!(
            s.pooled_periods, 4,
            "4 pooled returns, NOT 5 (no boundary stitch)"
        );
        let pooled = s.pooled.expect("measurable");
        assert!(
            (pooled.total_return - 0.4641).abs() < 1e-9,
            "tr={}",
            pooled.total_return
        );
    }

    #[test]
    fn summary_counts_unmeasurable_folds() {
        // Middle fold is a blow-up (equity hits 0) -> returns_from_equity None.
        let good0 = [pt(100), pt(110)];
        let blow = [pt(100), pt(0)];
        let good1 = [pt(100), pt(121)];
        let s = WalkForwardSummary::new(&[&good0, &blow, &good1], 252.0, None);
        assert_eq!(s.folds, 3);
        assert_eq!(s.measured_folds, 2);
        assert_eq!(s.unmeasurable_folds, 1);
        assert_eq!(s.pooled_periods, 2); // one return from each survivor
        assert!(s.pooled.is_some());
    }

    #[test]
    fn summary_all_unmeasurable_is_none() {
        let single0 = [pt(100)];
        let single1 = [pt(100)];
        let s = WalkForwardSummary::new(&[&single0, &single1], 252.0, Some(0.2));
        assert_eq!(s.measured_folds, 0);
        assert_eq!(s.unmeasurable_folds, 2);
        assert_eq!(s.pooled_periods, 0);
        assert!(s.pooled.is_none());
        assert!(s.efficiency.is_none(), "no pooled report -> no efficiency");
    }

    #[test]
    fn summary_efficiency_guards_nonpositive_is() {
        let f = [pt(100), pt(110), pt(121)];
        let pooled_tr = WalkForwardSummary::new(&[&f], 252.0, None)
            .pooled
            .unwrap()
            .total_return;
        // is <= 0 -> None (no sign inversion / division blow-up); is == 0 -> None.
        assert!(
            WalkForwardSummary::new(&[&f], 252.0, Some(0.0))
                .efficiency
                .is_none()
        );
        assert!(
            WalkForwardSummary::new(&[&f], 252.0, Some(-0.1))
                .efficiency
                .is_none()
        );
        // None in -> None out.
        assert!(
            WalkForwardSummary::new(&[&f], 252.0, None)
                .efficiency
                .is_none()
        );
        // is > 0 -> Some(pooled_oos_total / is).
        let eff = WalkForwardSummary::new(&[&f], 252.0, Some(0.10))
            .efficiency
            .unwrap();
        assert!((eff - walk_forward_efficiency(pooled_tr, 0.10)).abs() < 1e-12);
    }

    #[test]
    fn summary_from_reports_matches_new() {
        let mk = |curve: Vec<EquityPoint>| {
            // RunReport is #[non_exhaustive] (cross-crate): build via Default + assign
            // the pub field rather than a struct literal.
            let mut r = RunReport::default();
            r.equity_curve = curve;
            r
        };
        let c0 = vec![pt(100), pt(110), pt(121)];
        let c1 = vec![pt(100), pt(99)];
        let reports = [mk(c0.clone()), mk(c1.clone())];
        let from_reports = WalkForwardSummary::from_reports(&reports, 252.0, Some(0.2));
        let via_new = WalkForwardSummary::new(&[c0.as_slice(), c1.as_slice()], 252.0, Some(0.2));
        assert_eq!(from_reports, via_new);
    }

    #[test]
    fn summary_empty_input() {
        let s = WalkForwardSummary::new(&[], 252.0, None);
        assert_eq!(s.folds, 0);
        assert_eq!(s.pooled_periods, 0);
        assert!(s.pooled.is_none());
        assert!(s.efficiency.is_none());
    }
}
