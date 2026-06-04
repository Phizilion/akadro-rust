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
#[non_exhaustive]
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

    /// [`from_equity`] with the annualization factor **inferred from the curve's own
    /// timestamps** ([`infer_periods_per_year`]) instead of supplied — removing the
    /// silent "wrong `periods_per_year`" footgun (e.g. passing `252` for hourly bars)
    /// for the common regular-cadence case.
    ///
    /// The inference is a *calendar*-frequency estimate (periods per 365.25-day year)
    /// and matches akadro's 24/7-venue convention. It **cannot** recover an equity
    /// 252-**trading-day** calendar from timestamps — for that, pass
    /// [`PERIODS_PER_YEAR_EQUITY_DAILY`] to [`from_equity`] explicitly. Returns `None`
    /// when the period can't be inferred (`< 2` points, non-increasing timestamps) or
    /// the metrics aren't computable (see [`from_equity`]).
    ///
    /// [`from_equity`]: Self::from_equity
    #[must_use]
    pub fn from_equity_auto(curve: &[EquityPoint]) -> Option<Self> {
        let periods_per_year = infer_periods_per_year(curve)?;
        Self::from_equity(curve, periods_per_year)
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
        // Per-period simple returns from the (now strictly-positive) equity levels;
        // every metric is then computed by the single source of truth, `from_returns`.
        let returns: Vec<f64> = curve
            .windows(2)
            .map(|w| {
                let prev = w[0].equity.raw() as f64;
                (w[1].equity.raw() as f64 - prev) / prev
            })
            .collect();
        Self::from_returns(&returns, periods_per_year, risk_free, required_return)
    }

    /// Compute metrics directly from a per-period **simple-return** series — the
    /// single source of truth for the return→metrics math that [`from_equity_with`]
    /// delegates to, and the seam a pooled out-of-sample return series (from
    /// `WalkForwardSummary`) feeds without round-tripping through a synthetic curve.
    ///
    /// `total_return` is the compounded `∏(1 + rᵢ) − 1`; `max_drawdown` is taken over
    /// a normalized equity path reconstructed from the returns (drawdown is
    /// scale-invariant, so this equals a level-based computation). Sample variance
    /// (`ddof = 1`) for Sharpe/volatility, population-denominator downside for
    /// Sortino, and the same zero-denominator-limit convention as the rest of the
    /// type — all identical to the level path.
    ///
    /// [`from_equity_with`]: Self::from_equity_with
    ///
    /// Returns `None` for an empty series, a non-finite / non-positive
    /// `periods_per_year` (m28), or any return `≤ −100%` (a wipeout — reconstructed
    /// equity would go non-positive, so the ratios are ill-defined; the honest
    /// closed-loop "not computable" answer rather than a fabricated `NaN`).
    #[must_use]
    pub fn from_returns(
        returns: &[f64],
        periods_per_year: f64,
        risk_free: f64,
        required_return: f64,
    ) -> Option<Self> {
        if returns.is_empty() {
            return None;
        }
        // Annualization must be a positive, finite factor — otherwise `.sqrt()`
        // yields `NaN`/`±∞` and every ratio becomes `Some(NaN)` instead of `None`.
        if !(periods_per_year.is_finite() && periods_per_year > 0.0) {
            return None;
        }
        // A return ≤ −100% drives the reconstructed equity ≤ 0 (a wipeout) — the same
        // ill-defined case `from_equity` rejects via its non-positive-sample guard.
        if returns.iter().any(|&r| 1.0 + r <= 0.0) {
            return None;
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

        // Reconstruct a normalized equity path (base 1.0) for the total return and
        // max drawdown; drawdown is scale-invariant, so this matches a level path.
        let mut equity = 1.0_f64;
        let mut peak = 1.0_f64;
        let mut max_dd = 0.0_f64;
        for &r in returns {
            equity *= 1.0 + r; // 1 + r > 0, guarded above
            if equity > peak {
                peak = equity;
            }
            let dd = (peak - equity) / peak; // peak >= 1.0 > 0
            if dd > max_dd {
                max_dd = dd;
            }
        }
        let total_return = equity - 1.0; // ∏(1 + rᵢ) − 1
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

/// Nanoseconds in a 365.25-day calendar year (the annualization base).
const NANOS_PER_YEAR: f64 = 365.25 * 24.0 * 60.0 * 60.0 * 1e9;

/// Infer the periods-per-year annualization factor from an equity curve's own
/// timestamps: the **median** consecutive timestamp spacing → periods per 365.25-day
/// calendar year. Using the median makes it robust to weekend/holiday gaps and the
/// occasional missing bar (a few large gaps don't move the estimate).
///
/// Pair with [`PerformanceReport::from_equity_auto`] so Sharpe/volatility can't be
/// silently mis-annualized by a hand-passed factor. Returns `None` for `< 2` points,
/// a non-increasing pair (median step `≤ 0`), or a non-finite/non-positive result.
///
/// **Caveat:** this is a *calendar*-frequency estimate (a 24/7 venue convention). An
/// equity 252-**trading-day** calendar is not recoverable from timestamps — such
/// callers should pass [`PERIODS_PER_YEAR_EQUITY_DAILY`] to
/// [`PerformanceReport::from_equity`] explicitly.
#[must_use]
pub fn infer_periods_per_year(curve: &[EquityPoint]) -> Option<f64> {
    if curve.len() < 2 {
        return None;
    }
    let mut steps: Vec<i64> = curve
        .windows(2)
        .map(|w| w[1].ts.as_nanos() - w[0].ts.as_nanos())
        .collect();
    steps.sort_unstable();
    let median = steps[steps.len() / 2];
    if median <= 0 {
        return None;
    }
    let ppy = NANOS_PER_YEAR / median as f64;
    (ppy.is_finite() && ppy > 0.0).then_some(ppy)
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

    // --- from_returns extraction (m5) ---

    #[test]
    fn from_equity_delegates_to_from_returns() {
        // from_equity_with now computes returns then delegates to from_returns, so the
        // two paths must agree field-for-field on the same curve (behaviour-neutral).
        use crate::returns_from_equity;
        for curve in [
            vec![pt(1, 100), pt(2, 110), pt(3, 121)],
            vec![pt(1, 100), pt(2, 90), pt(3, 100), pt(4, 115)],
            vec![pt(1, 100), pt(2, 120), pt(3, 90), pt(4, 130)],
        ] {
            let via_equity = PerformanceReport::from_equity(&curve, 252.0).unwrap();
            let returns = returns_from_equity(&curve).unwrap();
            let via_returns = PerformanceReport::from_returns(&returns, 252.0, 0.0, 0.0).unwrap();
            assert_eq!(
                via_equity, via_returns,
                "delegation must be field-identical"
            );
        }
    }

    #[test]
    fn from_returns_hand_computed_and_guards() {
        // Two +10% periods: total = 1.1*1.1-1 = 0.21, no drawdown, +inf Sharpe (vol 0).
        let r = PerformanceReport::from_returns(&[0.1, 0.1], 252.0, 0.0, 0.0).unwrap();
        assert!(approx(r.total_return, 0.21));
        assert!(approx(r.max_drawdown, 0.0));
        assert_eq!(r.periods, 2);
        // Guards: empty, bad ppy, and a wipeout (<= -100%) are all None.
        assert!(PerformanceReport::from_returns(&[], 252.0, 0.0, 0.0).is_none());
        assert!(PerformanceReport::from_returns(&[0.1], f64::NAN, 0.0, 0.0).is_none());
        assert!(PerformanceReport::from_returns(&[0.1], -1.0, 0.0, 0.0).is_none());
        assert!(
            PerformanceReport::from_returns(&[0.2, -1.0], 252.0, 0.0, 0.0).is_none(),
            "a -100% return wipes the (reconstructed) account out -> None"
        );
    }

    // --- periods-per-year inference (m4) ---

    #[test]
    fn infer_periods_per_year_common_cadences() {
        let day = 86_400_000_000_000i64; // 1 day in ns
        let daily = [pt(0, 100), pt(day, 101), pt(2 * day, 102)];
        assert!((infer_periods_per_year(&daily).unwrap() - 365.25).abs() < 1.0);
        let min = 60_000_000_000i64; // 1 minute in ns
        let m1 = [pt(0, 100), pt(min, 101), pt(2 * min, 102)];
        assert!((infer_periods_per_year(&m1).unwrap() - 525_960.0).abs() < 60.0);
    }

    #[test]
    fn infer_robust_to_a_gap() {
        // Nine 1-minute steps and one 10-minute gap: the median ignores the gap.
        let min = 60_000_000_000i64;
        let mut ts = 0i64;
        let mut curve = vec![pt(0, 100)];
        for i in 1..=10 {
            ts += if i == 5 { 10 * min } else { min };
            curve.push(pt(ts, 100 + i));
        }
        assert!((infer_periods_per_year(&curve).unwrap() - 525_960.0).abs() < 60.0);
    }

    #[test]
    fn infer_none_on_too_short_or_nonincreasing() {
        assert!(infer_periods_per_year(&[]).is_none());
        assert!(infer_periods_per_year(&[pt(1, 100)]).is_none());
        assert!(infer_periods_per_year(&[pt(10, 100), pt(10, 101)]).is_none()); // dup ts -> step 0
        assert!(infer_periods_per_year(&[pt(20, 100), pt(10, 101)]).is_none()); // descending
    }

    #[test]
    fn from_equity_auto_matches_manual_inferred() {
        let day = 86_400_000_000_000i64;
        let curve = [pt(0, 100), pt(day, 110), pt(2 * day, 121)];
        let ppy = infer_periods_per_year(&curve).unwrap();
        assert_eq!(
            PerformanceReport::from_equity_auto(&curve),
            PerformanceReport::from_equity(&curve, ppy)
        );
        // Unsorted -> can't infer -> None (does not fabricate a factor).
        assert!(PerformanceReport::from_equity_auto(&[pt(20, 100), pt(10, 110)]).is_none());
    }

    #[test]
    fn metrics_match_independent_closed_form() {
        // Cross-validate against values derived BY HAND from the documented conventions
        // (empyrical/QuantStats): Sharpe/volatility use sample variance (ddof=1);
        // Sortino's downside deviation uses the population denominator (÷n); volatility
        // and the ratios are annualized by √periods_per_year. Returns chosen so every
        // figure is hand-computable.
        let returns = [0.10, -0.05, 0.10, -0.05];
        let ppy = 4.0; // annualization factor √4 = 2
        let r = PerformanceReport::from_returns(&returns, ppy, 0.0, 0.0).unwrap();

        // mean = 0.025; sample variance = 0.0225/3 = 0.0075; per-period σ = 0.08660254.
        // volatility (annualized) = 0.08660254 × 2.
        assert!(
            (r.volatility - 0.173_205_08).abs() < 1e-7,
            "vol {}",
            r.volatility
        );
        // Sharpe = (mean/σ)×√ppy = (0.025/0.08660254)×2 = 0.57735027.
        assert!(
            (r.sharpe - 0.577_350_27).abs() < 1e-7,
            "sharpe {}",
            r.sharpe
        );
        // Downside dev (÷n, below 0) = √((0.0025+0.0025)/4) = 0.03535534.
        // Sortino = (0.025/0.03535534)×2 = √2 (i.e. 2/√2).
        assert!(
            (r.sortino - std::f64::consts::SQRT_2).abs() < 1e-7,
            "sortino {}",
            r.sortino
        );
        // total return = (1.10·0.95)² − 1 = 0.092025; ann return = same (n = ppy).
        assert!(
            (r.total_return - 0.092_025).abs() < 1e-9,
            "total {}",
            r.total_return
        );
        assert!(
            (r.annualized_return - 0.092_025).abs() < 1e-9,
            "ann {}",
            r.annualized_return
        );
        // max drawdown = 0.05 (each −5% step off the running peak).
        assert!(
            (r.max_drawdown - 0.05).abs() < 1e-9,
            "maxdd {}",
            r.max_drawdown
        );
        // Calmar = ann_return / maxDD = 0.092025 / 0.05 = 1.8405.
        assert!((r.calmar - 1.8405).abs() < 1e-9, "calmar {}", r.calmar);

        // The ddof=1 distinction is observable: the population-variance Sharpe would be
        // (mean / σ_pop)×2 where σ_pop = √(0.0225/4) = 0.075 → 0.6666…, NOT 0.57735.
        let sharpe_population = (0.025 / 0.075) * 2.0;
        assert!(
            (r.sharpe - sharpe_population).abs() > 0.08,
            "Sharpe must use sample (ddof=1) variance, not population"
        );
    }
}
