// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cross-sectional alpha-evaluation utilities: the **Information Coefficient**
//! (how well a period's predicted scores line up with realized returns) and the
//! **Information Ratio** (how consistent that IC is over time). Reporting math
//! (`f64`); the trading path stays integer-exact.

/// Cross-sectional **Information Coefficient** — the Pearson correlation between
/// `pred` (predicted scores for each instrument) and `realized` (their realized
/// returns) for one period. In `-1..=1`. `0.0` if the slices differ in length,
/// have fewer than two elements, or either side is constant (zero variance).
#[must_use]
pub fn information_coefficient(pred: &[f64], realized: &[f64]) -> f64 {
    pearson(pred, realized)
}

/// **Rank** (Spearman) Information Coefficient — Pearson correlation over the
/// average ranks of each side. Robust to outliers and monotone nonlinearity;
/// usually preferred for ranking signals. In `-1..=1`. `0.0` on degenerate input.
#[must_use]
pub fn rank_information_coefficient(pred: &[f64], realized: &[f64]) -> f64 {
    if pred.len() != realized.len() || pred.len() < 2 {
        return 0.0;
    }
    pearson(&ranks(pred), &ranks(realized))
}

/// **Information Ratio** of a series of per-period ICs: `mean(ic) / σ(ic)`
/// (sample σ, `ddof=1`) — the signal's stability/decay over time. Uses the
/// `±∞`/`0` limit when σ is zero (consistent with the crate's ratio convention).
/// `0.0` for fewer than two ICs.
#[must_use]
pub fn information_ratio(ics: &[f64]) -> f64 {
    if ics.len() < 2 {
        return 0.0;
    }
    let n = ics.len() as f64;
    let mean = ics.iter().sum::<f64>() / n;
    let sd = (ics.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
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

/// Pearson correlation, `0.0` on degenerate input (length mismatch, `< 2`, or a
/// constant side).
fn pearson(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() || a.len() < 2 {
        return 0.0;
    }
    let n = a.len() as f64;
    let ma = a.iter().sum::<f64>() / n;
    let mb = b.iter().sum::<f64>() / n;
    let (mut cov, mut va, mut vb) = (0.0, 0.0, 0.0);
    for (&x, &y) in a.iter().zip(b) {
        let (dx, dy) = (x - ma, y - mb);
        cov += dx * dy;
        va += dx * dx;
        vb += dy * dy;
    }
    let denom = (va * vb).sqrt();
    if denom > 0.0 { cov / denom } else { 0.0 }
}

/// Average ranks (`1..=n`, ties share the mean of their rank span) of `x`.
fn ranks(x: &[f64]) -> Vec<f64> {
    let n = x.len();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&i, &j| x[i].partial_cmp(&x[j]).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = vec![0.0; n];
    let mut i = 0;
    while i < n {
        // Group equal values and assign them the average rank of the span.
        let mut j = i + 1;
        while j < n && (x[idx[j]] - x[idx[i]]).abs() == 0.0 {
            j += 1;
        }
        // Ranks i..j (0-based) → average of (i+1 .. j) one-based.
        let avg = ((i + 1 + j) as f64) / 2.0;
        for &k in &idx[i..j] {
            out[k] = avg;
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn perfect_linear_correlation() {
        let pred = [1.0, 2.0, 3.0, 4.0];
        let up = [2.0, 4.0, 6.0, 8.0]; // realized = 2·pred → IC = +1
        assert!(approx(information_coefficient(&pred, &up), 1.0));
        let down = [8.0, 6.0, 4.0, 2.0]; // IC = -1
        assert!(approx(information_coefficient(&pred, &down), -1.0));
    }

    #[test]
    fn rank_ic_handles_monotone_nonlinear() {
        // realized is a monotone (cubic) but non-linear function of pred:
        let pred = [1.0, 2.0, 3.0, 4.0];
        let realized = [1.0, 8.0, 27.0, 64.0];
        // Pearson IC < 1 (nonlinear), rank IC == 1 (perfectly monotone).
        assert!(information_coefficient(&pred, &realized) < 1.0);
        assert!(approx(rank_information_coefficient(&pred, &realized), 1.0));
    }

    #[test]
    fn rank_ic_with_ties() {
        // Ties get the average rank; correlation stays well-defined.
        let pred = [1.0, 1.0, 2.0, 3.0];
        let realized = [5.0, 5.0, 6.0, 7.0];
        assert!(approx(rank_information_coefficient(&pred, &realized), 1.0));
    }

    #[test]
    fn degenerate_inputs_are_zero() {
        assert!(approx(information_coefficient(&[1.0], &[2.0]), 0.0)); // len < 2
        assert!(approx(information_coefficient(&[1.0, 2.0], &[3.0]), 0.0)); // length mismatch
        assert!(approx(
            information_coefficient(&[2.0, 2.0, 2.0], &[1.0, 2.0, 3.0]),
            0.0
        )); // constant
        assert!(approx(rank_information_coefficient(&[1.0], &[2.0]), 0.0));
    }

    #[test]
    fn information_ratio_cases() {
        // Consistent positive IC, low variance → high IR.
        assert!(information_ratio(&[0.10, 0.12, 0.11, 0.09]) > 1.0);
        // Constant IC → zero σ, positive mean → +∞ limit. (Use exactly-
        // representable 0.5 so σ is *precisely* zero, not float-noise.)
        assert!(information_ratio(&[0.5, 0.5, 0.5]).is_infinite());
        // Constant negative IC → -∞.
        assert!(
            information_ratio(&[-0.5, -0.5]).is_infinite()
                && information_ratio(&[-0.5, -0.5]) < 0.0
        );
        // Zero-mean constant → 0.0; too short → 0.0.
        assert!(approx(information_ratio(&[0.0, 0.0]), 0.0));
        assert!(approx(information_ratio(&[0.5]), 0.0));
    }
}
