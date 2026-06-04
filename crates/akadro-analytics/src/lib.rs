// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # akadro-analytics
//!
//! Performance and risk analytics computed from a deterministic backtest
//! [`RunReport`](akadro_engine::RunReport): a [`PerformanceReport`] from the
//! equity curve (return, Sharpe, Sortino, max drawdown, Calmar, volatility),
//! [`TradeStats`] from the fill log (win rate, profit factor, expectancy), and
//! [`walk_forward`] window splitting for out-of-sample validation.
//!
//! Unlike the engine, analytics is *reporting* (run after the backtest, never in
//! the trading loop), so it uses `f64` for ratio metrics that inherently need
//! it — this does not affect the parity-critical, integer-only execution path.

mod cross_section;
mod distribution;
mod equity;
mod grid;
mod trades;
mod walk_forward;

pub use akadro_core::{BarOrderError, check_bars_ordered};
pub use cross_section::{information_coefficient, information_ratio, rank_information_coefficient};
pub use distribution::{
    DistributionStats, PboResult, cvar, deflated_sharpe, expected_max_sharpe_z, kurtosis, norm_cdf,
    norm_ppf, per_period_sharpe, probabilistic_sharpe, probability_of_backtest_overfitting,
    returns_from_equity, skewness, var, walk_forward_efficiency,
};
pub use equity::{
    PERIODS_PER_YEAR_CRYPTO_1M, PERIODS_PER_YEAR_CRYPTO_DAILY, PERIODS_PER_YEAR_EQUITY_DAILY,
    PerformanceReport, infer_periods_per_year,
};
pub use grid::{combinatorial_splits, run_grid};
pub use trades::TradeStats;
pub use walk_forward::{
    WalkForwardSummary, Window, slice_window, walk_forward, walk_forward_anchored,
    walk_forward_purged,
};
// The generic fold runners hand a closure both train AND test — an IS/OOS leakage
// footgun — so they live behind the off-by-default `escape-hatch` feature, not the
// default user surface (which offers only the structurally-safe WalkForwardBacktest).
#[cfg(feature = "escape-hatch")]
pub use walk_forward::{run_walk_forward, run_walk_forward_checked};
