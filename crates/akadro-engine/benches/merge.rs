// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Perf budget for the time-ordered event merge (decision D15).
//!
//! The single-backtest hot path is dominated not by per-event dispatch but by
//! merging multiple time-ordered sources. This bench compares draining a single
//! already-ordered stream against two `O(log k)` k-way merges — a `BinaryHeap` and
//! the **tournament tree** that backs `akadro_data::MergeSource` (decision D15) — at
//! `k = 12` and `k = 64`, to (a) confirm the merge is the real cost and (b) show the
//! tournament tree (one comparison per level) pulls ahead of the heap (~two) as `k`
//! grows. It is the CI perf gate guarding that the merge stays sub-linear. Run with
//! `cargo bench -p akadro-engine`.
//!
//! (Benches are not public API; the `criterion_*!` macros generate items that
//! trip `missing_docs`/style lints, so they are allowed at this crate root.)
#![allow(missing_docs, clippy::semicolon_if_nothing_returned)]

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

/// `n_sources` strictly-increasing interleaved timestamp streams.
fn make_sources(n_sources: usize, per: usize) -> Vec<Vec<u64>> {
    (0..n_sources)
        .map(|s| {
            (0..per)
                .map(|i| (i as u64) * n_sources as u64 + s as u64)
                .collect()
        })
        .collect()
}

/// Sum a single, already-ordered stream (the cheap baseline).
fn single(events: &[u64]) -> u64 {
    events.iter().fold(0u64, |acc, &t| acc.wrapping_add(t))
}

/// k-way merge via a min-heap keyed on `(timestamp, source, index)`.
fn kway(sources: &[Vec<u64>]) -> u64 {
    let mut heap: BinaryHeap<Reverse<(u64, usize, usize)>> = BinaryHeap::new();
    for (s, src) in sources.iter().enumerate() {
        if let Some(&first) = src.first() {
            heap.push(Reverse((first, s, 0)));
        }
    }
    let mut acc = 0u64;
    while let Some(Reverse((ts, s, idx))) = heap.pop() {
        acc = acc.wrapping_add(ts);
        let next = idx + 1;
        if next < sources[s].len() {
            heap.push(Reverse((sources[s][next], s, next)));
        }
    }
    acc
}

/// k-way merge via a tournament (winner) tree — the `O(log k)` structure of decision
/// D15, the microbench analog of `akadro_data::MergeSource`'s tree. Each refill
/// restores one root path (`log k` comparisons) vs the heap's sift-down (~`2 log k`).
/// Same `(ts, source)` key + accumulate-sum as `kway`, so the result is identical.
fn tournament(sources: &[Vec<u64>]) -> u64 {
    let n = sources.len();
    if n == 0 {
        return 0;
    }
    let size = n.next_power_of_two();
    let mut cur = vec![u64::MAX; size]; // current head per slot; MAX = exhausted/padding
    let mut idx = vec![0usize; size];
    for (s, src) in sources.iter().enumerate() {
        cur[s] = src.first().copied().unwrap_or(u64::MAX);
    }
    let better = |cur: &[u64], a: usize, b: usize| if (cur[a], a) <= (cur[b], b) { a } else { b };
    let mut tree = vec![0usize; 2 * size];
    for s in 0..size {
        tree[size + s] = s;
    }
    for i in (1..size).rev() {
        tree[i] = better(&cur, tree[2 * i], tree[2 * i + 1]);
    }
    let mut acc = 0u64;
    loop {
        let w = tree[1];
        if cur[w] == u64::MAX {
            break; // all real slots exhausted
        }
        acc = acc.wrapping_add(cur[w]);
        idx[w] += 1;
        cur[w] = sources[w].get(idx[w]).copied().unwrap_or(u64::MAX);
        let mut i = usize::midpoint(size, w);
        while i >= 1 {
            tree[i] = better(&cur, tree[2 * i], tree[2 * i + 1]);
            i /= 2;
        }
    }
    acc
}

fn bench_merge(c: &mut Criterion) {
    let per = 10_000;
    let flat: Vec<u64> = (0..(12 * per) as u64).collect();
    c.bench_function("single_source_drain_120k", |b| {
        b.iter(|| single(black_box(&flat)))
    });
    // Sweep k to expose the crossover: the tournament tree (log k) should pull ahead of
    // the binary heap (~2 log k) as k grows, and both beat an O(k) linear scan.
    for &n_sources in &[12usize, 64] {
        let sources = make_sources(n_sources, per);
        c.bench_function(&format!("binaryheap_kway_{n_sources}x10k"), |b| {
            b.iter(|| kway(black_box(&sources)))
        });
        c.bench_function(&format!("tournament_kway_{n_sources}x10k"), |b| {
            b.iter(|| tournament(black_box(&sources)))
        });
    }
}

criterion_group!(benches, bench_merge);
criterion_main!(benches);
