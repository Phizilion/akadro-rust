// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Framework-owned walk-forward backtest runner — the **structural** in-sample /
//! out-of-sample boundary.
//!
//! [`WalkForwardBacktest`] is the look-ahead-safe way to run a walk-forward study:
//! the **fit** step receives only the *train* bars (the `test` slice does not exist
//! in its scope) and the **framework itself** builds and runs the out-of-sample
//! engine, so a strategy author cannot fit on the test data or hand back a
//! cherry-picked out-of-sample report. This mirrors the kill feature's first layer
//! ("the strategy never receives the dataset") raised to the walk-forward level.
//!
//! It also closes config drift *for free*: one [`FillConfig`] and one starting cash
//! are supplied once and reused verbatim on every fold, so the in-sample sweep and
//! the out-of-sample run can never diverge in fees/slippage.
//!
//! ## What this does NOT prevent (the honest residual, the D2 ceiling)
//! It is still out-of-band — and therefore impossible for any API to stop — for a
//! caller to run the whole study, read the returned per-fold out-of-sample reports,
//! pick the best, and re-run with that parameter set; or for `fit` to load its own
//! copy of the future data. Those remain the user's responsibility (mitigated by the
//! `#![forbid(unsafe_code)]` strategy template), exactly as for the in-engine kill
//! feature. The win here is precise and real: **no `fit` closure can read the
//! out-of-sample bars through this API, and no caller holds an out-of-sample report
//! at fit time.** For a fully custom feed/exchange or a non-`RunReport` result, drop
//! to [`run_walk_forward`](akadro_analytics::run_walk_forward) (the escape hatch that
//! hands both slices and so cannot offer this guarantee).

use akadro_analytics::{Window, slice_window};
use akadro_core::{AkadroError, Bar, InstrumentSpec, Money, check_bars_ordered};
use akadro_engine::{Engine, RunReport, Strategy};

use crate::exchange::{FillConfig, SimulatedExchange};
use crate::feed::HistoricalFeed;

/// The out-of-sample result of one walk-forward fold.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct OosFold {
    /// The fold's window (train/test boundaries).
    pub window: Window,
    /// Number of in-sample (train) bars the fit step saw.
    pub train_bars: usize,
    /// The out-of-sample backtest report (the engine run over the test bars).
    pub report: RunReport,
}

/// A framework-owned walk-forward backtest: fit on train (only), evaluate
/// out-of-sample with a single shared configuration. See the module docs for the
/// structural IS/OOS boundary it enforces.
#[derive(Debug)]
#[non_exhaustive]
pub struct WalkForwardBacktest<'a> {
    instruments: &'a [InstrumentSpec],
    initial_cash: Money,
    fill: FillConfig,
    bars: &'a [Bar],
    windows: &'a [Window],
}

impl<'a> WalkForwardBacktest<'a> {
    /// Create a runner. `instruments` + `initial_cash` + `fill` are the engine
    /// configuration applied **identically** to every out-of-sample fold; `bars` is
    /// the full timeline (sliced per `windows` by [`slice_window`]).
    #[must_use]
    pub fn new(
        instruments: &'a [InstrumentSpec],
        initial_cash: Money,
        fill: FillConfig,
        bars: &'a [Bar],
        windows: &'a [Window],
    ) -> Self {
        WalkForwardBacktest {
            instruments,
            initial_cash,
            fill,
            bars,
            windows,
        }
    }

    /// Run every fold. For each window: `fit` receives the **train** bars (and the
    /// threaded `state`, e.g. a warm-started model or train-derived normalization
    /// statistics) and returns opaque `Params`; `make` builds a fresh [`Strategy`]
    /// from those `Params` alone; then the framework runs a fresh [`Engine`] over a
    /// [`HistoricalFeed`] of the **test** bars with the shared [`SimulatedExchange`]
    /// configuration. Returns the per-fold [`OosFold`]s and the final threaded state.
    ///
    /// `fit` never receives the test bars and `make` never receives any bars — that
    /// is the structural boundary (see module docs).
    ///
    /// # Errors
    /// [`AkadroError::Config`] if `bars` is not a well-ordered event stream
    /// (validated once up front via [`check_bars_ordered`], since the framework — not
    /// the caller — builds each fold's feed), or whatever [`Engine::new`] returns for
    /// an invalid instrument set. On error no fold result is produced.
    pub fn run<State, Params, Fit, Make, Strat>(
        &self,
        mut state: State,
        mut fit: Fit,
        mut make: Make,
    ) -> akadro_core::Result<(Vec<OosFold>, State)>
    where
        Fit: FnMut(&[Bar], &mut State) -> Params,
        Make: FnMut(&Params) -> Strat,
        Strat: Strategy,
    {
        // The framework owns each fold's feed, so the input must be well-ordered or
        // every out-of-sample run is silently corrupt — fail fast (m: garbage-in).
        check_bars_ordered(self.bars).map_err(|e| {
            AkadroError::Config(format!(
                "walk-forward bars are not a valid ordered stream: {e:?}"
            ))
        })?;

        let mut folds = Vec::with_capacity(self.windows.len());
        for window in self.windows {
            let (train, test) = slice_window(self.bars, window);
            // fit sees ONLY train; make sees ONLY the params fit produced.
            let params = fit(&train, &mut state);
            let strategy = make(&params);
            // Fresh engine per fold (D2), with the ONE shared config (no drift). The
            // framework owns these (cache-sourced, ordering checked at run start), so
            // it uses the crate-internal unchecked ctor — no public injection door.
            let feed = HistoricalFeed::from_bars_unchecked(test);
            let exec = SimulatedExchange::with_config(self.instruments.to_vec(), self.fill);
            let report =
                Engine::new(self.instruments, self.initial_cash, feed, exec, strategy)?.run();
            folds.push(OosFold {
                window: *window,
                train_bars: train.len(),
                report,
            });
        }
        Ok((folds, state))
    }
}
