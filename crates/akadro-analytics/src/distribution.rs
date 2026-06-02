// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Return-distribution and backtest-overfitting statistics over a strategy's
//! per-period returns: tail risk (`VaR` / `CVaR`), shape (skewness / excess
//! kurtosis), and the Bailey–López de Prado overfitting trio (Probabilistic
//! Sharpe, Deflated Sharpe, Walk-Forward Efficiency).
//!
//! All inputs are `f64` per-period **simple returns** (reporting math only — the
//! engine's execution path stays integer-exact). Use [`returns_from_equity`] to
//! derive them from a [`RunReport`](akadro_engine::RunReport)'s `equity_curve`.
//!
//! **Sharpe convention for PSR/DSR:** pass the **per-period** Sharpe (mean/σ of
//! returns), *not* the annualized one — see [`per_period_sharpe`].

use akadro_engine::EquityPoint;

/// Per-period simple returns from an equity curve. `None` if there are fewer than
/// two points or **any** equity sample is non-positive (matching
/// [`PerformanceReport::from_equity`](crate::PerformanceReport::from_equity)).
#[must_use]
pub fn returns_from_equity(curve: &[EquityPoint]) -> Option<Vec<f64>> {
    if curve.len() < 2 || curve.iter().any(|p| p.equity.raw() <= 0) {
        return None;
    }
    let eq: Vec<f64> = curve.iter().map(|p| p.equity.raw() as f64).collect();
    Some(eq.windows(2).map(|w| (w[1] - w[0]) / w[0]).collect())
}

/// Tail-observation count for confidence `alpha` over `n` samples:
/// `max(1, ceil((1 - alpha) * n))`, clamped to `n`.
fn tail_k(n: usize, alpha: f64) -> usize {
    let p = (1.0 - alpha).clamp(0.0, 1.0);
    // Subtract a tiny epsilon so a `p·n` that is integral up to float error
    // (e.g. `0.05 · 20 = 1.0000000000000009`) ceils to that integer, not the next.
    let k = (p * n as f64 - 1e-9).ceil().max(1.0) as usize;
    k.clamp(1, n)
}

/// Historical **Value-at-Risk** at confidence `alpha` (e.g. `0.95`): the loss,
/// as a **positive** fraction, that returns do not exceed with probability
/// `alpha`. Nearest-rank estimator (the `k`-th worst of `n` returns, `k =
/// ceil((1-alpha)·n)`). A negative result means even the tail return is a gain.
/// `0.0` for an empty slice.
#[must_use]
pub fn var(returns: &[f64], alpha: f64) -> f64 {
    if returns.is_empty() {
        return 0.0;
    }
    let mut s = returns.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let k = tail_k(s.len(), alpha);
    -s[k - 1]
}

/// **Conditional `VaR` / Expected Shortfall** at confidence `alpha`: the mean loss
/// (positive fraction) over the worst `k = ceil((1-alpha)·n)` returns — the
/// average severity once you are in the tail. Always `>= var` (as losses).
/// `0.0` for an empty slice.
#[must_use]
pub fn cvar(returns: &[f64], alpha: f64) -> f64 {
    if returns.is_empty() {
        return 0.0;
    }
    let mut s = returns.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let k = tail_k(s.len(), alpha);
    let mean = s[..k].iter().sum::<f64>() / k as f64;
    -mean
}

/// Central moments `(mean, m2, m3, m4)` (population, ÷ n). `None` if empty.
fn moments(returns: &[f64]) -> Option<(f64, f64, f64, f64)> {
    if returns.is_empty() {
        return None;
    }
    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let (mut m2, mut m3, mut m4) = (0.0, 0.0, 0.0);
    for &r in returns {
        let d = r - mean;
        let d2 = d * d;
        m2 += d2;
        m3 += d2 * d;
        m4 += d2 * d2;
    }
    Some((mean, m2 / n, m3 / n, m4 / n))
}

/// **Population** skewness (the biased Fisher–Pearson `g1`, dividing the central
/// moments by `n`): `m3 / m2^1.5`. `0.0` when the series is constant (`m2 == 0`) or
/// empty. NOTE: this is the *population* estimator (no `√(n(n−1))/(n−2)`
/// small-sample correction); for small `n` it understates magnitude, which biases
/// the PSR/DSR below toward optimism — prefer a long return series for those.
#[must_use]
pub fn skewness(returns: &[f64]) -> f64 {
    match moments(returns) {
        Some((_, m2, m3, _)) if m2 > 0.0 => m3 / m2.powf(1.5),
        _ => 0.0,
    }
}

/// **Population** excess kurtosis (`g2`, central moments ÷ `n`): `m4 / m2^2 - 3`
/// (normal → `0`). `0.0` when the series is constant (`m2 == 0`) or empty. Like
/// [`skewness`], this is the population (uncorrected) estimator — see its note on
/// the small-`n` PSR/DSR bias.
#[must_use]
pub fn kurtosis(returns: &[f64]) -> f64 {
    match moments(returns) {
        Some((_, m2, _, m4)) if m2 > 0.0 => m4 / (m2 * m2) - 3.0,
        _ => 0.0,
    }
}

/// Per-period Sharpe (mean / sample-σ of returns, `ddof=1`) — the input PSR/DSR
/// expect. `0.0` for a positive mean with zero σ is avoided: returns the `±∞`/`0`
/// limit consistent with [`PerformanceReport`](crate::PerformanceReport).
#[must_use]
pub fn per_period_sharpe(returns: &[f64]) -> f64 {
    if returns.len() < 2 {
        return 0.0;
    }
    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let sd = var.sqrt();
    if sd > 0.0 {
        mean / sd
    } else if mean > 0.0 {
        f64::INFINITY
    } else if mean < 0.0 {
        f64::NEG_INFINITY
    } else {
        0.0
    }
}

/// **Probabilistic Sharpe Ratio** (Bailey & López de Prado): the probability that
/// the true (per-period) Sharpe exceeds `benchmark_sr`, given the observed
/// `sr`, sample length `n`, and the return distribution's `skew` and
/// `kurtosis_excess`. Corrects the Sharpe estimate for non-normality and short
/// samples. Result in `0..=1`.
#[must_use]
pub fn probabilistic_sharpe(
    sr: f64,
    benchmark_sr: f64,
    n: usize,
    skew: f64,
    kurtosis_excess: f64,
) -> f64 {
    if n < 2 {
        return 0.0;
    }
    // Denominator: sqrt(1 - skew·SR + (γ4 - 1)/4 · SR²), γ4 = kurtosis_excess + 3,
    // so (γ4 - 1)/4 = (kurtosis_excess + 2)/4 (and = 0.5 for a normal dist).
    let denom = (1.0 - skew * sr + (kurtosis_excess + 2.0) / 4.0 * sr * sr).max(1e-12);
    let z = (sr - benchmark_sr) * ((n - 1) as f64).sqrt() / denom.sqrt();
    norm_cdf(z)
}

/// Euler–Mascheroni constant, for the expected-maximum-Sharpe benchmark.
const EULER_GAMMA: f64 = 0.577_215_664_901_532_9;

/// Expected maximum of `n_trials` i.i.d. standard-normal Sharpe estimates
/// (Bailey–López de Prado approximation). `0.0` for `n_trials < 2`.
#[must_use]
pub fn expected_max_sharpe_z(n_trials: usize) -> f64 {
    if n_trials < 2 {
        return 0.0;
    }
    let nn = n_trials as f64;
    (1.0 - EULER_GAMMA) * norm_ppf(1.0 - 1.0 / nn)
        + EULER_GAMMA * norm_ppf(1.0 - 1.0 / (nn * std::f64::consts::E))
}

/// **Deflated Sharpe Ratio**: [`probabilistic_sharpe`] against a benchmark SR*
/// set to the *expected maximum* Sharpe of `n_trials` independent backtests
/// (`= sqrt(sr_variance) · E[max]`). Deflates for selection bias under multiple
/// testing — a high in-sample Sharpe found among many trials is discounted.
/// `sr_variance` is the variance of the trial Sharpes (use a small positive
/// estimate if unknown). Result in `0..=1`.
#[must_use]
pub fn deflated_sharpe(
    sr: f64,
    sr_variance: f64,
    n_trials: usize,
    n: usize,
    skew: f64,
    kurtosis_excess: f64,
) -> f64 {
    let sr_star = sr_variance.max(0.0).sqrt() * expected_max_sharpe_z(n_trials);
    probabilistic_sharpe(sr, sr_star, n, skew, kurtosis_excess)
}

/// **Walk-Forward Efficiency**: out-of-sample performance relative to in-sample,
/// `oos / is_`. With a **positive** in-sample (the normal case), `1.0` means OOS
/// held up, `< 1` is the usual decay, and `> 1` is OOS outperformance.
///
/// CAUTION: the `< 1` / `> 1` convention only holds when `is_ > 0`. When `is_ < 0`
/// (a losing in-sample) the plain `oos / is_` ratio **inverts**: e.g.
/// `wfe(0.05, -0.10) = -0.5`, which looks poor even though OOS actually improved
/// on the IS loss. Interpret WFE only for a profitable in-sample; for `is_ <= 0`
/// it is not a meaningful efficiency. `is_ == 0` uses the `±∞`/`0` ratio limit.
#[must_use]
pub fn walk_forward_efficiency(oos: f64, is_: f64) -> f64 {
    if is_ != 0.0 {
        // The two non-zero branches were identical; the sign inversion for is_ < 0
        // is documented above rather than special-cased.
        oos / is_
    } else if oos > 0.0 {
        f64::INFINITY
    } else if oos < 0.0 {
        f64::NEG_INFINITY
    } else {
        0.0
    }
}

/// A bundle of return-distribution statistics at a single confidence level.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct DistributionStats {
    /// Number of returns.
    pub n: usize,
    /// Confidence level used for `var`/`cvar` (e.g. `0.95`).
    pub alpha: f64,
    /// Historical `VaR` at `alpha` (positive = loss). See [`var`].
    pub var: f64,
    /// Conditional `VaR` / Expected Shortfall at `alpha`. See [`cvar`].
    pub cvar: f64,
    /// Skewness (Fisher–Pearson `g1`). See [`skewness`].
    pub skewness: f64,
    /// Excess kurtosis (`g2`). See [`kurtosis`].
    pub kurtosis: f64,
}

impl DistributionStats {
    /// Compute all stats from per-period returns at confidence `alpha`. `None`
    /// for an empty slice.
    #[must_use]
    pub fn from_returns(returns: &[f64], alpha: f64) -> Option<Self> {
        if returns.is_empty() {
            return None;
        }
        Some(DistributionStats {
            n: returns.len(),
            alpha,
            var: var(returns, alpha),
            cvar: cvar(returns, alpha),
            skewness: skewness(returns),
            kurtosis: kurtosis(returns),
        })
    }

    /// Compute from an equity curve (via [`returns_from_equity`]). `None` if the
    /// curve is too short or hits non-positive equity.
    #[must_use]
    pub fn from_equity(curve: &[EquityPoint], alpha: f64) -> Option<Self> {
        let r = returns_from_equity(curve)?;
        Self::from_returns(&r, alpha)
    }
}

// --- standard-normal helpers --------------------------------------------------

/// Standard-normal CDF `Φ(x)`, via `erfc` (Abramowitz & Stegun 7.1.26,
/// `|error| < 1.5e-7`).
#[must_use]
pub fn norm_cdf(x: f64) -> f64 {
    0.5 * erfc(-x / std::f64::consts::SQRT_2)
}

/// Complementary error function `erfc(x)` (A&S 7.1.26).
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * z);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - poly * (-z * z).exp();
    if x >= 0.0 { 1.0 - erf } else { 1.0 + erf }
}

/// Standard-normal inverse CDF `Φ⁻¹(p)` (Acklam's rational approximation,
/// `|error| < 1.15e-9` in the central region). Clamps `p` to `(0, 1)`; returns
/// `±∞` at the bounds.
#[must_use]
pub fn norm_ppf(p: f64) -> f64 {
    // Coefficients (Acklam).
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    const P_LOW: f64 = 0.024_25;
    const P_HIGH: f64 = 1.0 - P_LOW;

    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    if p < P_LOW {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= P_HIGH {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{Money, Timestamp};

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn per_period_sharpe_all_branches() {
        // Too-short series -> 0.
        assert!(approx(per_period_sharpe(&[]), 0.0, 1e-12));
        assert!(approx(per_period_sharpe(&[0.1]), 0.0, 1e-12));
        // Normal: mean / sample-sigma.
        let s = per_period_sharpe(&[0.01, 0.02, 0.03]);
        assert!(s > 0.0 && s.is_finite());
        // Zero variance, positive mean -> +inf; negative -> -inf; flat zero -> 0.
        let inf = per_period_sharpe(&[0.05, 0.05]);
        assert!(inf.is_infinite() && inf > 0.0);
        let ninf = per_period_sharpe(&[-0.05, -0.05]);
        assert!(ninf.is_infinite() && ninf < 0.0);
        assert!(approx(per_period_sharpe(&[0.0, 0.0]), 0.0, 1e-12));
    }

    #[test]
    fn distribution_edge_cases() {
        // moments(empty) -> None, surfaced via skewness/kurtosis = 0.
        assert!(approx(skewness(&[]), 0.0, 1e-12));
        assert!(approx(kurtosis(&[]), 0.0, 1e-12));
        // walk_forward_efficiency with is_ == 0: sign follows oos.
        let n = walk_forward_efficiency(-1.0, 0.0);
        assert!(n.is_infinite() && n < 0.0);
        let p = walk_forward_efficiency(1.0, 0.0);
        assert!(p.is_infinite() && p > 0.0);
        assert!(approx(walk_forward_efficiency(0.0, 0.0), 0.0, 1e-12));
        // DistributionStats::from_returns on an empty slice -> None.
        assert!(DistributionStats::from_returns(&[], 0.95).is_none());
        let ds = DistributionStats::from_returns(&[-0.02, 0.01, 0.03, -0.01], 0.95).unwrap();
        assert_eq!(ds.n, 4);
        // norm_ppf: low tail (p < P_LOW) and the p >= 1.0 saturation.
        assert!(norm_ppf(0.001) < -2.0);
        let hi = norm_ppf(1.0);
        assert!(hi.is_infinite() && hi > 0.0);
        let lo = norm_ppf(0.0);
        assert!(lo.is_infinite() && lo < 0.0);
    }

    #[test]
    fn var_cvar_on_known_sample() {
        // 20 returns: nineteen +0.01 and one -0.10. At alpha=0.95, tail k=1.
        let mut r = vec![0.01_f64; 19];
        r.push(-0.10);
        assert!(approx(var(&r, 0.95), 0.10, 1e-12)); // worst return is -0.10 → VaR 0.10
        assert!(approx(cvar(&r, 0.95), 0.10, 1e-12)); // single tail obs → ES == VaR
        // alpha=0.90 → k=2: worst two are -0.10 and +0.01 → ES = -mean = 0.045
        assert!(approx(cvar(&r, 0.90), 0.045, 1e-12));
    }

    #[test]
    fn var_empty_and_all_gains() {
        assert!(approx(var(&[], 0.95), 0.0, 1e-12));
        assert!(approx(cvar(&[], 0.95), 0.0, 1e-12));
        // All positive → VaR is negative (a "loss" that is actually a gain).
        assert!(var(&[0.01, 0.02, 0.03], 0.95) < 0.0);
    }

    #[test]
    fn skew_kurtosis_symmetric() {
        // Symmetric set → ~zero skew.
        let r = [-0.02, -0.01, 0.0, 0.01, 0.02];
        assert!(approx(skewness(&r), 0.0, 1e-12));
        // Excess kurtosis of this uniform-ish set is negative (platykurtic).
        assert!(kurtosis(&r) < 0.0);
        // Constant series → 0 by convention.
        assert!(approx(skewness(&[0.01, 0.01, 0.01]), 0.0, 1e-12));
        assert!(approx(kurtosis(&[0.01, 0.01, 0.01]), 0.0, 1e-12));
    }

    #[test]
    fn skew_sign() {
        // Right tail (one big positive) → positive skew.
        let r = [-0.01, -0.01, -0.01, 0.10];
        assert!(skewness(&r) > 0.0);
    }

    #[test]
    fn norm_cdf_known_points() {
        assert!(approx(norm_cdf(0.0), 0.5, 1e-9));
        assert!(approx(norm_cdf(1.0), 0.841_344_746, 1e-6));
        assert!(approx(norm_cdf(-1.0), 0.158_655_254, 1e-6));
        assert!(norm_cdf(5.0) > 0.999_999);
    }

    #[test]
    fn norm_ppf_inverts_cdf() {
        assert!(approx(norm_ppf(0.975), 1.959_963_985, 1e-4));
        assert!(approx(norm_ppf(0.5), 0.0, 1e-9));
        assert!(approx(norm_ppf(0.025), -1.959_963_985, 1e-4));
        assert!(norm_ppf(0.0).is_infinite() && norm_ppf(0.0) < 0.0);
        assert!(norm_ppf(1.0).is_infinite() && norm_ppf(1.0) > 0.0);
        // Round-trip Φ(Φ⁻¹(p)) ≈ p.
        for &p in &[0.1, 0.3, 0.6, 0.9] {
            assert!(approx(norm_cdf(norm_ppf(p)), p, 1e-4));
        }
    }

    #[test]
    fn psr_monotone_in_sr() {
        // Higher observed SR → higher probability of beating the benchmark.
        let lo = probabilistic_sharpe(0.10, 0.0, 100, 0.0, 0.0);
        let hi = probabilistic_sharpe(0.30, 0.0, 100, 0.0, 0.0);
        assert!(hi > lo);
        // SR == benchmark with symmetric normal returns → ~0.5.
        assert!(approx(
            probabilistic_sharpe(0.2, 0.2, 100, 0.0, 0.0),
            0.5,
            1e-9
        ));
        // Out of range guard.
        assert!(approx(
            probabilistic_sharpe(0.2, 0.0, 1, 0.0, 0.0),
            0.0,
            1e-12
        ));
    }

    #[test]
    fn dsr_deflates_with_more_trials() {
        // More trials → higher SR* benchmark → lower DSR for the same observed SR.
        let few = deflated_sharpe(0.5, 0.01, 5, 250, 0.0, 0.0);
        let many = deflated_sharpe(0.5, 0.01, 500, 250, 0.0, 0.0);
        assert!(many < few, "few={few} many={many}");
        assert!((0.0..=1.0).contains(&few) && (0.0..=1.0).contains(&many));
        // n_trials < 2 → SR* = 0 → DSR == PSR vs 0.
        assert!(approx(
            deflated_sharpe(0.3, 0.01, 1, 250, 0.0, 0.0),
            probabilistic_sharpe(0.3, 0.0, 250, 0.0, 0.0),
            1e-12
        ));
    }

    #[test]
    fn expected_max_grows_with_trials() {
        assert!(approx(expected_max_sharpe_z(1), 0.0, 1e-12));
        assert!(expected_max_sharpe_z(1000) > expected_max_sharpe_z(10));
    }

    #[test]
    fn wfe_cases() {
        assert!(approx(walk_forward_efficiency(0.08, 0.10), 0.8, 1e-12));
        assert!(walk_forward_efficiency(0.05, 0.0).is_infinite());
        assert!(approx(walk_forward_efficiency(0.0, 0.0), 0.0, 1e-12));
    }

    #[test]
    fn distribution_stats_from_equity() {
        let curve = [
            EquityPoint {
                ts: Timestamp::from_nanos(1),
                equity: Money::from_raw(100),
            },
            EquityPoint {
                ts: Timestamp::from_nanos(2),
                equity: Money::from_raw(110),
            },
            EquityPoint {
                ts: Timestamp::from_nanos(3),
                equity: Money::from_raw(99),
            },
            EquityPoint {
                ts: Timestamp::from_nanos(4),
                equity: Money::from_raw(115),
            },
        ];
        let s = DistributionStats::from_equity(&curve, 0.95).unwrap();
        assert_eq!(s.n, 3);
        assert!(approx(s.alpha, 0.95, 1e-12));
        // Non-positive equity anywhere → None (consistent with PerformanceReport).
        let bad = [
            EquityPoint {
                ts: Timestamp::from_nanos(1),
                equity: Money::from_raw(100),
            },
            EquityPoint {
                ts: Timestamp::from_nanos(2),
                equity: Money::from_raw(0),
            },
        ];
        assert!(DistributionStats::from_equity(&bad, 0.95).is_none());
        assert!(returns_from_equity(&curve[..1]).is_none());
    }
}
