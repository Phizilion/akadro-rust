// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Equity-curve performance metrics.

use akadro_engine::EquityPoint;

/// Periods per year for **equity** daily bars (trading days).
pub const PERIODS_PER_YEAR_EQUITY_DAILY: f64 = 252.0;
/// Periods per year for **crypto** daily bars on a 24/7 venue such as MEXC.
pub const PERIODS_PER_YEAR_CRYPTO_DAILY: f64 = 365.0;
/// Periods per year for crypto 1-minute bars on a 24/7 venue (`365 * 1440`).
pub const PERIODS_PER_YEAR_CRYPTO_1M: f64 = 525_600.0;

/// Risk/return metrics derived from a mark-to-market equity curve.
///
/// Ratios (`sharpe`, `sortino`, `calmar`) are annualized using a caller-supplied
/// `periods_per_year` — use [`PERIODS_PER_YEAR_EQUITY_DAILY`] (252) for equity
/// daily bars, [`PERIODS_PER_YEAR_CRYPTO_DAILY`] (365) for crypto daily bars on a
/// 24/7 venue, or [`PERIODS_PER_YEAR_CRYPTO_1M`] (`525_600`) for crypto 1-minute
/// bars.
///
/// **Zero-denominator convention (all three ratios are consistent):** when the
/// denominator is zero (constant returns → zero volatility/downside, or zero
/// drawdown), the ratio is the mathematical limit: `+∞` for a positive numerator,
/// `-∞` for a negative one, and `0.0` for a flat (zero) numerator. This is chosen
/// for ranking utility (a constant-positive strategy out-ranks a flat one) and
/// deliberately differs from empyrical/pyfolio, which return `NaN` there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PerformanceReport {
    /// Number of return periods (equity points minus one).
    pub periods: usize,
    /// Total return over the whole curve: `(last - first) / first`.
    pub total_return: f64,
    /// Compound annualized return.
    pub annualized_return: f64,
    /// Mean per-period return.
    pub mean_return: f64,
    /// Annualized volatility: per-period return std × √`periods_per_year`.
    pub volatility: f64,
    /// Annualized Sharpe ratio (excess over the per-period risk-free rate). `±∞`
    /// when volatility is zero with a non-zero mean (see the type-level
    /// zero-denominator convention).
    pub sharpe: f64,
    /// Annualized Sortino ratio (downside deviation below the required return).
    /// `±∞` when downside deviation is zero with a non-zero mean.
    pub sortino: f64,
    /// Maximum drawdown as a positive fraction in `0..=1`.
    pub max_drawdown: f64,
    /// Calmar ratio: annualized return / max drawdown; `±∞` when the return is
    /// non-zero with zero drawdown (consistent with `sharpe`/`sortino`).
    pub calmar: f64,
}

impl PerformanceReport {
    /// Compute metrics from an equity curve. Returns `None` if there are fewer
    /// than two points, or if **any** equity sample is non-positive.
    ///
    /// The non-positive guard is deliberate (the closed-loop "not computable"
    /// answer): if a backtest wipes the account out — equity `≤ 0` at any point,
    /// e.g. a fee-driven blowup — the ratio/return metrics are ill-defined
    /// (per-bar returns would divide by a non-positive base, and
    /// `(1 + total_return).powf(..)` is `NaN` for `total_return ≤ -1`). Rather
    /// than fabricate `NaN`/garbage Sharpe/Calmar/volatility, we report `None`.
    #[must_use]
    pub fn from_equity(curve: &[EquityPoint], periods_per_year: f64) -> Option<Self> {
        Self::from_equity_with(curve, periods_per_year, 0.0, 0.0)
    }

    /// As [`Self::from_equity`] but with a per-period `risk_free` rate (subtracted
    /// in the Sharpe numerator) and a per-period `required_return` / MAR (the
    /// Sortino target: downside is measured below it and it is subtracted in the
    /// Sortino numerator). Both default to `0.0` in [`Self::from_equity`].
    #[must_use]
    pub fn from_equity_with(
        curve: &[EquityPoint],
        periods_per_year: f64,
        risk_free: f64,
        required_return: f64,
    ) -> Option<Self> {
        if curve.len() < 2 {
            return None;
        }
        if curve.iter().any(|p| p.equity.raw() <= 0) {
            return None;
        }
        // Annualization must be a positive, finite factor — otherwise `.sqrt()`
        // yields `NaN`/`±∞` and every ratio becomes `Some(NaN)` instead of an honest
        // `None` (m28).
        if !(periods_per_year.is_finite() && periods_per_year > 0.0) {
            return None;
        }
        // Every sample is now strictly positive, so all divisions below are safe.
        let equity: Vec<f64> = curve.iter().map(|p| p.equity.raw() as f64).collect();
        let first = equity[0];
        let last = equity[equity.len() - 1];

        // Per-period simple returns (every prior equity is > 0).
        let mut returns = Vec::with_capacity(equity.len() - 1);
        for w in equity.windows(2) {
            returns.push((w[1] - w[0]) / w[0]);
        }
        let n = returns.len() as f64;
        let mean = returns.iter().sum::<f64>() / n;
        // Sample variance (÷ n-1) for Sharpe/volatility, matching empyrical and
        // QuantStats (`ddof=1`). Undefined for a single return, so 0 there.
        let variance = if returns.len() >= 2 {
            returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0)
        } else {
            0.0
        };
        let per_period_vol = variance.sqrt();
        // Downside deviation below the required return, population denominator
        // (÷ n) as empyrical's `downside_risk` does.
        let downside = {
            let dsum: f64 = returns
                .iter()
                .map(|r| (r - required_return).min(0.0).powi(2))
                .sum();
            (dsum / n).sqrt()
        };

        let ann = periods_per_year.sqrt();
        // The reported `volatility` is ANNUALIZED, consistent with the annualized
        // ratios below (the Sharpe/Sortino math uses the per-period figures).
        let volatility = per_period_vol * ann;
        // Zero-denominator → mathematical limit (see the type-level convention):
        // +∞ for a positive numerator, -∞ for a negative one, 0.0 when flat.
        let sharpe = ratio_or_limit(mean - risk_free, per_period_vol) * ann;
        let sortino = ratio_or_limit(mean - required_return, downside) * ann;

        // Max drawdown over the (strictly positive) equity path.
        let mut peak = first;
        let mut max_dd = 0.0_f64;
        for &e in &equity {
            if e > peak {
                peak = e;
            }
            let dd = (peak - e) / peak; // peak >= first > 0
            if dd > max_dd {
                max_dd = dd;
            }
        }

        // `n >= 1` and `total_return > -1` (last > 0), so this is always finite.
        let total_return = (last - first) / first;
        let annualized_return = (1.0 + total_return).powf(periods_per_year / n) - 1.0;
        let calmar = ratio_or_limit(annualized_return, max_dd);

        Some(PerformanceReport {
            periods: returns.len(),
            total_return,
            annualized_return,
            mean_return: mean,
            volatility,
            sharpe,
            sortino,
            max_drawdown: max_dd,
            calmar,
        })
    }
}

/// A ratio `num / denom`, or its mathematical limit when `denom == 0`: `+∞` for a
/// positive numerator, `-∞` for a negative one, and `0.0` for a flat (zero) one.
fn ratio_or_limit(num: f64, denom: f64) -> f64 {
    if denom > 0.0 {
        num / denom
    } else if num > 0.0 {
        f64::INFINITY
    } else if num < 0.0 {
        f64::NEG_INFINITY
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{Money, Timestamp};

    fn pt(ts: i64, eq: i128) -> EquityPoint {
        EquityPoint {
            ts: Timestamp::from_nanos(ts),
            equity: Money::from_raw(eq),
        }
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn too_short_is_none() {
        assert!(PerformanceReport::from_equity(&[], 252.0).is_none());
        assert!(PerformanceReport::from_equity(&[pt(1, 100)], 252.0).is_none());
    }

    #[test]
    fn non_positive_start_is_none() {
        assert!(PerformanceReport::from_equity(&[pt(1, 0), pt(2, 100)], 252.0).is_none());
    }

    #[test]
    fn non_positive_or_nonfinite_periods_is_none() {
        // m28: an invalid annualization factor yields None, not Some(NaN).
        let curve = [pt(1, 100), pt(2, 110)];
        assert!(PerformanceReport::from_equity(&curve, 0.0).is_none());
        assert!(PerformanceReport::from_equity(&curve, -252.0).is_none());
        assert!(PerformanceReport::from_equity(&curve, f64::NAN).is_none());
        assert!(PerformanceReport::from_equity(&curve, f64::INFINITY).is_none());
    }

    #[test]
    fn monotonic_up_has_no_drawdown_positive_sharpe() {
        let curve = [pt(1, 100), pt(2, 110), pt(3, 121)]; // +10% each step
        let r = PerformanceReport::from_equity(&curve, 252.0).unwrap();
        assert_eq!(r.periods, 2);
        assert!(approx(r.total_return, 0.21)); // 121/100 - 1
        assert!(approx(r.max_drawdown, 0.0));
        // Constant +10% returns -> zero volatility with a POSITIVE mean -> Sharpe is
        // +∞ (the limit), NOT 0; likewise Sortino (no downside) and Calmar (no dd).
        assert!(approx(r.volatility, 0.0));
        assert!(r.sharpe.is_infinite() && r.sharpe > 0.0);
        assert!(r.sortino.is_infinite() && r.sortino > 0.0);
        assert!(r.annualized_return > 0.0);
        assert!(r.calmar.is_infinite() && r.calmar > 0.0); // positive return, no drawdown
    }

    #[test]
    fn monotonic_down_has_negative_infinite_sharpe() {
        // Constant -10% returns (all equity still positive) -> zero volatility with
        // a NEGATIVE mean -> Sharpe is -∞. (Sortino is finite-negative here: the
        // downside deviation is nonzero since every return is below zero.)
        let curve = [pt(1, 100), pt(2, 90), pt(3, 81)];
        let r = PerformanceReport::from_equity(&curve, 252.0).unwrap();
        assert!(approx(r.volatility, 0.0));
        assert!(r.sharpe.is_infinite() && r.sharpe < 0.0);
        assert!(r.sortino.is_finite() && r.sortino < 0.0);
    }

    #[test]
    fn risk_free_lowers_sharpe() {
        let curve = [pt(1, 100), pt(2, 110), pt(3, 100), pt(4, 115)];
        let base = PerformanceReport::from_equity(&curve, 252.0).unwrap();
        // A positive per-period risk-free rate reduces the Sharpe numerator.
        let rf = PerformanceReport::from_equity_with(&curve, 252.0, 0.01, 0.0).unwrap();
        assert!(rf.sharpe < base.sharpe);
        // A required return raises the Sortino downside target, lowering Sortino.
        let mar = PerformanceReport::from_equity_with(&curve, 252.0, 0.0, 0.01).unwrap();
        assert!(mar.sortino < base.sortino);
    }

    #[test]
    fn known_drawdown() {
        // 100 -> 90 -> 100: peak 100, trough 90 -> 10% drawdown.
        let curve = [pt(1, 100), pt(2, 90), pt(3, 100)];
        let r = PerformanceReport::from_equity(&curve, 252.0).unwrap();
        assert!(approx(r.max_drawdown, 0.10), "dd={}", r.max_drawdown);
        assert!(approx(r.total_return, 0.0)); // back to start
    }

    #[test]
    fn volatile_series_has_finite_sharpe() {
        let curve = [pt(1, 100), pt(2, 110), pt(3, 100), pt(4, 115)];
        let r = PerformanceReport::from_equity(&curve, 252.0).unwrap();
        assert!(r.volatility > 0.0);
        assert!(r.sharpe.is_finite());
        assert!(r.sortino.is_finite());
        assert!(r.max_drawdown > 0.0); // dipped from 110 to 100
    }

    #[test]
    fn flat_two_point_curve_is_all_zero() {
        // Two points = a single return, so sample variance is undefined -> 0
        // volatility/Sharpe; a flat path has zero return and zero drawdown -> 0
        // Calmar (not +inf, since the return is not positive).
        let r = PerformanceReport::from_equity(&[pt(1, 100), pt(2, 100)], 252.0).unwrap();
        assert_eq!(r.periods, 1);
        assert!(approx(r.volatility, 0.0));
        assert!(approx(r.sharpe, 0.0));
        assert!(approx(r.total_return, 0.0));
        assert!(approx(r.calmar, 0.0));
    }

    #[test]
    fn non_positive_equity_anywhere_is_none() {
        // A wiped-out account (equity <= 0 at any point) is not computable, so we
        // return None rather than NaN/garbage ratios (Finding 2).
        // Hits exactly zero mid-path:
        assert!(
            PerformanceReport::from_equity(&[pt(1, 100), pt(2, 0), pt(3, 50)], 252.0).is_none()
        );
        // Goes negative (the real fee-driven blowup case):
        assert!(PerformanceReport::from_equity(&[pt(1, 100), pt(2, -20)], 252.0).is_none());
        // Negative final after positive samples:
        assert!(
            PerformanceReport::from_equity(&[pt(1, 100), pt(2, 120), pt(3, -5)], 252.0).is_none()
        );
    }
}
