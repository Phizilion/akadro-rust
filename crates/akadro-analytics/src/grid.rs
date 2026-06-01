// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Parallel **grid / batch** backtest execution and the **CSCV** combinatorial
//! split generator — orchestration for optimizers and overfitting tests (PBO),
//! built purely on `std::thread` (no new dependency) above the engine.

/// Run many independent backtest *cells* in parallel and collect their results
/// **in input order**.
///
/// Each cell is a closure that builds and runs one backtest (typically
/// `|| Engine::new(specs, cash, feed, exchange, strategy).run()`, returning its
/// [`RunReport`](akadro_engine::RunReport) — `R`). Cells run across
/// `available_parallelism()` scoped threads, so a cell may borrow shared inputs
/// (e.g. the bar slice) without `'static`. Each cell is an independent fresh
/// engine — the documented "fresh engine per run" contract (D2) — so determinism
/// is preserved and the result is identical to running the cells sequentially
/// (a parallel sweep is itself a tested determinism guarantee).
///
/// # Panics
/// Propagates a panic from any cell (after the other threads finish).
#[must_use]
pub fn run_grid<R, F>(cells: Vec<F>) -> Vec<R>
where
    F: FnOnce() -> R + Send,
    R: Send,
{
    let n = cells.len();
    if n == 0 {
        return Vec::new();
    }
    let nthreads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(n);
    // Round-robin the indexed cells into per-thread buckets.
    let mut buckets: Vec<Vec<(usize, F)>> = (0..nthreads).map(|_| Vec::new()).collect();
    for (i, cell) in cells.into_iter().enumerate() {
        buckets[i % nthreads].push((i, cell));
    }
    let mut indexed: Vec<(usize, R)> = std::thread::scope(|s| {
        let handles: Vec<_> = buckets
            .into_iter()
            .map(|bucket| {
                s.spawn(move || {
                    bucket
                        .into_iter()
                        .map(|(i, c)| (i, c()))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("grid cell panicked"))
            .collect()
    });
    indexed.sort_by_key(|(i, _)| *i);
    indexed.into_iter().map(|(_, r)| r).collect()
}

/// Generate the **Combinatorial Symmetric Cross-Validation** (CSCV) splits used to
/// estimate the Probability of Backtest Overfitting: partition the data into
/// `n_groups` equal blocks, then enumerate every way to choose `k` of them as the
/// **test** set. Returns `(train_groups, test_groups)` block-index pairs, one per
/// `C(n_groups, k)` combination, in lexicographic order. `train_groups` is the
/// complement. Empty if `k == 0` or `k > n_groups`.
///
/// The caller maps block indices to bar ranges and runs each split via
/// [`run_grid`]. Pure index math — no engine or data dependency.
#[must_use]
pub fn combinatorial_splits(n_groups: usize, k: usize) -> Vec<(Vec<usize>, Vec<usize>)> {
    let mut out = Vec::new();
    if k == 0 || k > n_groups {
        return out;
    }
    let mut idx: Vec<usize> = (0..k).collect(); // current k-combination
    loop {
        let test = idx.clone();
        let train: Vec<usize> = (0..n_groups).filter(|g| !test.contains(g)).collect();
        out.push((train, test));
        // Advance to the next lexicographic k-combination of 0..n_groups.
        let mut i = k;
        loop {
            if i == 0 {
                return out;
            }
            i -= 1;
            if idx[i] != i + n_groups - k {
                idx[i] += 1;
                for j in (i + 1)..k {
                    idx[j] = idx[j - 1] + 1;
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_grid_preserves_order_and_runs_all() {
        // Cells return their own index; result must be 0..100 in order despite
        // parallel execution.
        let cells: Vec<_> = (0..100usize).map(|i| move || i * 2).collect();
        let got = run_grid(cells);
        assert_eq!(got, (0..100).map(|i| i * 2).collect::<Vec<_>>());
    }

    #[test]
    fn run_grid_empty_is_empty() {
        let cells: Vec<fn() -> u8> = Vec::new();
        assert!(run_grid(cells).is_empty());
    }

    #[test]
    fn run_grid_cells_can_borrow_shared_state() {
        // Scoped threads let cells borrow `data` without `'static`.
        let data = vec![10, 20, 30];
        let cells: Vec<_> = (0..data.len())
            .map(|i| {
                let d = &data;
                move || d[i] + 1
            })
            .collect();
        assert_eq!(run_grid(cells), vec![11, 21, 31]);
    }

    #[test]
    fn cscv_enumerates_all_combinations() {
        // C(4,2) = 6 splits; test ∪ train = 0..4, disjoint, |test| = 2.
        let splits = combinatorial_splits(4, 2);
        assert_eq!(splits.len(), 6);
        for (train, test) in &splits {
            assert_eq!(test.len(), 2);
            assert_eq!(train.len(), 2);
            let mut all: Vec<usize> = train.iter().chain(test).copied().collect();
            all.sort_unstable();
            assert_eq!(all, vec![0, 1, 2, 3]); // partition of all groups
        }
        // First and last combinations (lexicographic).
        assert_eq!(splits[0].1, vec![0, 1]);
        assert_eq!(splits[5].1, vec![2, 3]);
        assert_eq!(splits[0].0, vec![2, 3]); // complement
    }

    #[test]
    fn cscv_edge_cases() {
        assert!(combinatorial_splits(4, 0).is_empty());
        assert!(combinatorial_splits(3, 4).is_empty()); // k > n
        assert_eq!(combinatorial_splits(3, 3).len(), 1); // single split, test = all
        assert_eq!(combinatorial_splits(5, 1).len(), 5); // leave-one-out
    }
}
