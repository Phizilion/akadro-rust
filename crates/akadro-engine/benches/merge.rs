// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Perf budget for the time-ordered event merge (decision D15).
//!
//! The single-backtest hot path is dominated not by per-event dispatch but by
//! merging multiple time-ordered sources. This bench compares draining a single
//! already-ordered stream against a `BinaryHeap` k-way merge of 12 sources, to
//! (a) confirm the merge is the real >10% cost and (b) establish the baseline a
//! future loser-tree implementation must beat. Run with `cargo bench -p
//! akadro-engine`.
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

fn bench_merge(c: &mut Criterion) {
    let per = 10_000;
    let n_sources = 12;
    let sources = make_sources(n_sources, per);
    let flat: Vec<u64> = (0..(n_sources * per) as u64).collect();

    c.bench_function("single_source_drain_120k", |b| {
        b.iter(|| single(black_box(&flat)))
    });
    c.bench_function("binaryheap_kway_merge_12x10k", |b| {
        b.iter(|| kway(black_box(&sources)))
    });
}

criterion_group!(benches, bench_merge);
criterion_main!(benches);
