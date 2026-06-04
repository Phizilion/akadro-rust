// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Allocation-regression gate (the dhat "zero-alloc" gate, AGENTS.md §13.7).
//!
//! Runs a fixed `BARS`-long backtest through the real engine + `SimulatedExchange`
//! under the `dhat` heap profiler and asserts the heap profile stays **bounded** —
//! specifically that the per-event hot loop does **not** allocate, so the total
//! allocation count grows with the number of *trades* (and amortized `Vec` growth),
//! never with the number of *bars*. An accidental `Box`/`Vec` in the event loop would
//! make `total_blocks` jump by ~`BARS` and trip this gate.
//!
//! This is a regression guard, not an exact pin: the ceilings carry generous headroom
//! (std/`Vec`-growth details vary), so it catches a *gross* regression without flaking.
//! Behind the off-by-default `dhat-heap` feature (it swaps the global allocator), run:
//! `cargo test -p akadro --features dhat-heap --test alloc_gate`.
#![cfg(feature = "dhat-heap")]

mod common;

use akadro_core::{Bar, InstrumentId, Price, Qty, Timestamp};

// dhat tracks every heap allocation in this test binary via the global allocator.
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const INSTRUMENT: InstrumentId = InstrumentId::new(0);
const BARS: usize = 8_000;

/// A triangle wave (period 80) that repeatedly crosses the 5/20 SMAs, so the strategy
/// trades steadily across all `BARS` — exercising the fill/portfolio path, not a flat
/// no-op run.
fn long_bars() -> Vec<Bar> {
    (0..BARS)
        .map(|i| {
            let phase = (i % 80) as i64;
            let c = 100 + if phase < 40 { phase } else { 80 - phase };
            let p = Price::from_raw(c);
            Bar::new(
                INSTRUMENT,
                Timestamp::from_nanos(i as i64 + 1),
                p,
                p,
                p,
                p,
                Qty::from_raw(1),
            )
        })
        .collect()
}

#[test]
fn backtest_heap_profile_stays_bounded() {
    // Ceilings with generous headroom (measured ~706 blocks / ~1.2 MB peak over 8000
    // bars). `total_blocks` is the load-bearing assertion: it sits *far below* `BARS`
    // precisely because the per-event loop allocates nothing — allocations come from
    // amortized `Vec` growth (fills, equity curve) and one-time setup, not per bar. A
    // hot-loop allocation regression (e.g. a per-bar `Box`) would push the count toward
    // `BARS` and trip this. The bytes ceiling guards a peak-memory blow-up. Bump these
    // *deliberately* (and say why) if a change legitimately allocates more — never to
    // paper over a hot-loop regression.
    const MAX_BLOCKS: u64 = 2_000; // « BARS (8000): proves the event loop is alloc-free
    const MAX_PEAK_BYTES: usize = 4_000_000;

    // Build the input BEFORE profiling so the fixture's allocations are excluded — we
    // gate the engine *run*, not the test's own setup.
    let bars = long_bars();
    let strategy = common::strategy_fs(5, 20);

    let _profiler = dhat::Profiler::builder().testing().build();
    let report = common::run_backtest_bars(strategy, bars);
    let stats = dhat::HeapStats::get();
    std::hint::black_box(&report);

    eprintln!(
        "dhat over {BARS}-bar backtest: total_blocks={} total_bytes={} max_bytes={} curr_blocks={}",
        stats.total_blocks, stats.total_bytes, stats.max_bytes, stats.curr_blocks
    );

    assert!(
        stats.total_blocks <= MAX_BLOCKS,
        "allocation count {} exceeded the ceiling {MAX_BLOCKS} (« BARS={BARS}) — a per-event \
         heap-allocation regression in the engine hot loop?",
        stats.total_blocks
    );
    assert!(
        stats.max_bytes <= MAX_PEAK_BYTES,
        "peak heap {} exceeded the ceiling {MAX_PEAK_BYTES} bytes — a memory blow-up?",
        stats.max_bytes
    );
}
