// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure gap algebra for the range-aware bar cache.
//!
//! The cache is keyed by *(venue, symbol, interval)* — **not** by the requested
//! window — so the same series accumulates across requests and a request for
//! `[start, end]` only downloads the timestamps it doesn't already hold. This module
//! is the heart of that: given the ranges already cached for a series and a requested
//! window, it returns the **missing sub-ranges** to fetch. A window shifted by even
//! one bar against a fully-cached series yields **no** gaps (zero network), which is
//! the whole point — the previous filename-keyed cache re-downloaded everything on
//! any shift.
//!
//! All timestamps are **close-stamp nanoseconds** (akadro stamps bars at close), and
//! all ranges are **inclusive** `[lo, hi]` of the first/last close in a cached chunk
//! — matching the `(first_ts, last_ts)` the cache writer records in the
//! [`Manifest`](crate::Manifest). `bar_ns` is the interval (one bar's width); it
//! defines what "adjacent" means: two chunks one bar apart are *contiguous*, not
//! gapped.

/// Compute the missing close-stamp sub-ranges of `[req_lo, req_hi]` (inclusive ns)
/// given the already-`cached` ranges (each inclusive `[lo, hi]`, in any order) for a
/// series whose bar width is `bar_ns`.
///
/// Returns a minimal, ascending, non-overlapping list of `(lo, hi)` holes to fetch.
/// Empty when the window is already fully covered (including a window shifted by a
/// fraction of a bar against a covered series). Sub-bar slivers are dropped.
///
/// Algorithm: coalesce cached ranges (two are contiguous when `next.lo <= cur.hi +
/// bar_ns`, i.e. ≤ one bar apart), clip to the request, then walk left→right emitting
/// the holes between coverage.
#[must_use]
pub fn missing_gaps(
    cached: &[(i64, i64)],
    req_lo: i64,
    req_hi: i64,
    bar_ns: i64,
) -> Vec<(i64, i64)> {
    if req_hi < req_lo {
        return Vec::new();
    }
    let bar_ns = bar_ns.max(1);

    // Coalesce cached ranges that are contiguous within one bar of slack. Drop
    // malformed (reversed) ranges defensively.
    let mut ranges: Vec<(i64, i64)> = cached.iter().copied().filter(|&(l, h)| h >= l).collect();
    ranges.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        match merged.last_mut() {
            // `lo <= cur.hi + bar_ns` → adjacent or overlapping → one bar apart counts
            // as contiguous (no gap between consecutive bars).
            Some(cur) if lo <= cur.1.saturating_add(bar_ns) => cur.1 = cur.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }

    // Walk the request window, emitting the holes between merged coverage.
    let mut gaps = Vec::new();
    let mut cursor = req_lo; // next still-uncovered close-stamp
    for (c_lo, c_hi) in merged {
        if c_hi < cursor {
            continue; // coverage entirely before the cursor
        }
        if c_lo > req_hi {
            break; // coverage starts past the request
        }
        if c_lo > cursor {
            // Hole from cursor up to one bar before this coverage begins.
            let hole_hi = (c_lo - bar_ns).min(req_hi);
            if hole_hi >= cursor {
                gaps.push((cursor, hole_hi));
            }
        }
        // Advance the cursor to one bar past this coverage.
        cursor = cursor.max(c_hi.saturating_add(bar_ns));
        if cursor > req_hi {
            break;
        }
    }
    if cursor <= req_hi {
        gaps.push((cursor, req_hi));
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAR: i64 = 60_000_000_000; // 1m in ns

    #[test]
    fn empty_cache_is_full_window() {
        assert_eq!(missing_gaps(&[], 0, 100 * BAR, BAR), vec![(0, 100 * BAR)]);
    }

    #[test]
    fn fully_cached_yields_nothing_even_when_shifted() {
        let cached = [(0, 100 * BAR)];
        // Exact window, and a window shifted inside the covered span → no gaps.
        assert_eq!(missing_gaps(&cached, 0, 100 * BAR, BAR), vec![]);
        assert_eq!(missing_gaps(&cached, BAR, 99 * BAR, BAR), vec![]);
        // Shifted by a fraction of a bar (the "1 second" case) → still no gaps.
        assert_eq!(missing_gaps(&cached, 1, 100 * BAR - 1, BAR), vec![]);
    }

    #[test]
    fn partial_left_fetches_only_the_front() {
        // Cached [50,100]; request [0,100] → fetch [0, 49 bars] (up to one bar before 50).
        let cached = [(50 * BAR, 100 * BAR)];
        assert_eq!(
            missing_gaps(&cached, 0, 100 * BAR, BAR),
            vec![(0, 49 * BAR)]
        );
    }

    #[test]
    fn partial_right_fetches_only_the_tail() {
        let cached = [(0, 50 * BAR)];
        assert_eq!(
            missing_gaps(&cached, 0, 100 * BAR, BAR),
            vec![(51 * BAR, 100 * BAR)]
        );
    }

    #[test]
    fn interior_gap() {
        // Cached [0,30] and [70,100]; request [0,100] → the hole [31,69 bars].
        let cached = [(0, 30 * BAR), (70 * BAR, 100 * BAR)];
        assert_eq!(
            missing_gaps(&cached, 0, 100 * BAR, BAR),
            vec![(31 * BAR, 69 * BAR)]
        );
    }

    #[test]
    fn adjacent_chunks_coalesce_no_refetch() {
        // [0,50] and [51bar,100] are one bar apart → contiguous → fully covered.
        let cached = [(0, 50 * BAR), (51 * BAR, 100 * BAR)];
        assert_eq!(missing_gaps(&cached, 0, 100 * BAR, BAR), vec![]);
    }

    #[test]
    fn unordered_cached_input_is_handled() {
        let cached = [(70 * BAR, 100 * BAR), (0, 30 * BAR)];
        assert_eq!(
            missing_gaps(&cached, 0, 100 * BAR, BAR),
            vec![(31 * BAR, 69 * BAR)]
        );
    }

    #[test]
    fn two_interior_gaps() {
        let cached = [(0, 10 * BAR), (40 * BAR, 50 * BAR), (90 * BAR, 100 * BAR)];
        assert_eq!(
            missing_gaps(&cached, 0, 100 * BAR, BAR),
            vec![(11 * BAR, 39 * BAR), (51 * BAR, 89 * BAR)]
        );
    }

    #[test]
    fn reversed_request_is_empty() {
        assert_eq!(missing_gaps(&[], 100, 0, BAR), vec![]);
    }

    #[test]
    fn property_gaps_within_request_and_disjoint_from_cache() {
        // A returned gap is always within the request and never overlaps a cached range.
        let cached = [(20 * BAR, 40 * BAR), (60 * BAR, 80 * BAR)];
        let gaps = missing_gaps(&cached, 0, 100 * BAR, BAR);
        for &(g_lo, g_hi) in &gaps {
            assert!(g_lo >= 0 && g_hi <= 100 * BAR && g_lo <= g_hi);
            for &(c_lo, c_hi) in &cached {
                assert!(
                    g_hi < c_lo || g_lo > c_hi,
                    "gap {g_lo}..{g_hi} overlaps cache {c_lo}..{c_hi}"
                );
            }
        }
    }

    proptest::proptest! {
        /// The cache-gap invariant over arbitrary cached-range sets: the returned holes
        /// are well-formed (ascending, non-overlapping, within the window), and adding
        /// them to the cached set **completes the coverage** — a second pass finds
        /// nothing more to fetch. This is the loader's resume guarantee (fetch the
        /// gaps, and the window is covered).
        #[test]
        fn gaps_complete_the_coverage(
            raw in proptest::collection::vec((0i64..1000, 0i64..1000), 0..6),
            lo in 0i64..1000,
            span in 0i64..1000,
            bar in 1i64..50,
        ) {
            let hi = lo + span;
            // Normalize to well-formed (min, max) ranges for a clean union.
            let cached: Vec<(i64, i64)> =
                raw.iter().map(|&(a, b)| (a.min(b), a.max(b))).collect();
            let gaps = missing_gaps(&cached, lo, hi, bar);

            let mut prev_hi = i64::MIN;
            for &(g_lo, g_hi) in &gaps {
                proptest::prop_assert!(g_lo <= g_hi, "gap is well-formed");
                proptest::prop_assert!(g_lo >= lo && g_hi <= hi, "gap within the window");
                proptest::prop_assert!(g_lo > prev_hi, "gaps ascending + non-overlapping");
                prev_hi = g_hi;
            }

            // Fetching the gaps completes the coverage: a second pass is empty.
            let mut union = cached.clone();
            union.extend(gaps);
            proptest::prop_assert!(
                missing_gaps(&union, lo, hi, bar).is_empty(),
                "gaps + cached must complete the coverage (second pass finds nothing)"
            );
        }
    }
}
