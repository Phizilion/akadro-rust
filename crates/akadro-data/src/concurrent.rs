// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Optional **concurrent multi-series** downloading, rate-limited per venue.
//!
//! [`load_many`] runs the range-aware [`load_bars`] gap-fill over many series at once
//! using `std::thread::scope` (no async runtime; `tokio` stays confined to
//! `akadro-live` — separation of concerns), bounded by [`CacheOptions::concurrency`]
//! workers and a shared
//! [`RateLimiter`] so the venue's requests-per-second ceiling is respected
//! *collectively* across workers. Results come back in **input order** and each
//! series is **fault-isolated** — one symbol's error is its own `Err` slot and never
//! aborts the others.
//!
//! Concurrency is safe because each series caches to its **own** files and its own
//! per-series manifest (see `load::manifest_path`), so parallel workers never touch
//! shared mutable cache state. With one worker (the default
//! [`CacheOptions::concurrency`] `= 1`) it degrades to a deterministic sequential run.

use std::path::Path;

use akadro_core::{Bar, DataSource, PageSink};

use crate::bars::DataError;
use crate::load::{CacheOptions, SeriesKey, load_bars};
use crate::ratelimit::RateLimiter;

/// One series to load over a `[start_ms, end_ms)` window, for [`load_many`].
#[derive(Clone, Debug)]
pub struct SeriesRequest {
    /// The series to cache/load.
    pub key: SeriesKey,
    /// Window start (epoch ms).
    pub start_ms: i64,
    /// Window end (epoch ms, exclusive).
    pub end_ms: i64,
}

impl SeriesRequest {
    /// Construct a series request.
    #[must_use]
    pub fn new(key: SeriesKey, start_ms: i64, end_ms: i64) -> Self {
        Self {
            key,
            start_ms,
            end_ms,
        }
    }
}

/// Load (gap-fill) many series, up to [`CacheOptions::concurrency`] at a time, paced
/// by `limiter`. Returns one `Result<Vec<Bar>, DataError>` per input request, in the
/// **same order** as `requests`; a failing series yields an `Err` in its slot without
/// affecting the rest.
///
/// `make_source(key, lo_ms, hi_ms, sink)` builds the connector feed for one gap of
/// one series with the incremental-journal page `sink` installed (the venue-neutral
/// seam). It is shared across workers, so it must be `Sync`.
///
/// `now_ms` is the injected wall-clock (see [`load_bars`]). Each worker calls
/// `limiter.acquire()` at the start of each series (feed-boundary pacing); the limiter
/// is shared, so the aggregate request rate stays under the configured cap.
///
/// # Panics
/// Panics if a worker thread panics, re-propagating that panic to the caller (a
/// per-series `DataError` is returned in its slot, not a panic — only a genuine bug
/// in a worker would unwind here).
pub fn load_many<D, F>(
    cache_dir: &Path,
    requests: Vec<SeriesRequest>,
    now_ms: i64,
    opts: &CacheOptions,
    limiter: &RateLimiter,
    make_source: F,
) -> Vec<Result<Vec<Bar>, DataError>>
where
    D: DataSource,
    F: Fn(&SeriesKey, i64, i64, Box<dyn PageSink>) -> D + Sync,
{
    let n = requests.len();
    if n == 0 {
        return Vec::new();
    }
    let workers = opts.concurrency.max(1).min(n);
    // Round-robin the indexed requests into per-worker buckets (mirrors `run_grid`).
    let mut buckets: Vec<Vec<(usize, SeriesRequest)>> = (0..workers).map(|_| Vec::new()).collect();
    for (i, r) in requests.into_iter().enumerate() {
        buckets[i % workers].push((i, r));
    }
    let make_source = &make_source;
    let mut indexed: Vec<(usize, Result<Vec<Bar>, DataError>)> = std::thread::scope(|s| {
        let handles: Vec<_> = buckets
            .into_iter()
            .map(|bucket| {
                let limiter = limiter.clone();
                s.spawn(move || {
                    bucket
                        .into_iter()
                        .map(|(i, r)| {
                            limiter.acquire(); // collective per-venue pacing (no-op if unbounded)
                            let res = load_bars(
                                cache_dir,
                                &r.key,
                                r.start_ms,
                                r.end_ms,
                                now_ms,
                                opts,
                                |lo, hi, sink| make_source(&r.key, lo, hi, sink),
                            );
                            (i, res)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("series-load worker panicked"))
            .collect()
    });
    indexed.sort_by_key(|(i, _)| *i);
    indexed.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{Event, InstrumentId, Price, Qty, Timestamp};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    const BAR_NS: i64 = 60_000_000_000;
    const FAR_FUTURE_MS: i64 = 1_000_000_000;

    fn unique_dir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "akadro_many_{name}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    struct FakeFeed {
        bars: std::vec::IntoIter<Bar>,
    }
    impl FakeFeed {
        fn new(bars: Vec<Bar>, mut sink: Box<dyn PageSink>) -> Self {
            sink.on_page(&bars);
            FakeFeed {
                bars: bars.into_iter(),
            }
        }
    }
    impl DataSource for FakeFeed {
        fn next_event(&mut self) -> Option<Event> {
            self.bars.next().map(Event::Bar)
        }
    }

    /// A `Sync` venue closure (shared across workers): generates contiguous bars for
    /// the requested window tagged with each series' own instrument, and counts calls.
    fn venue(
        calls: Arc<Mutex<usize>>,
    ) -> impl Fn(&SeriesKey, i64, i64, Box<dyn PageSink>) -> FakeFeed + Sync {
        move |key, lo_ms, hi_ms, sink| {
            *calls.lock().unwrap() += 1;
            let (lo_ns, hi_ns) = (lo_ms * 1_000_000, hi_ms * 1_000_000);
            let mut bars = Vec::new();
            let mut c = lo_ns;
            while c < hi_ns {
                bars.push(Bar::new(
                    key.instrument,
                    Timestamp::from_nanos(c),
                    Price::from_raw(100),
                    Price::from_raw(100),
                    Price::from_raw(100),
                    Price::from_raw(100),
                    Qty::from_raw(1),
                ));
                c += BAR_NS;
            }
            FakeFeed::new(bars, sink)
        }
    }

    fn req(symbol: &str, instrument: u32, interval: &str, end_min: i64) -> SeriesRequest {
        SeriesRequest::new(
            SeriesKey::new("v", symbol, interval, InstrumentId::new(instrument), 2, 0),
            0,
            end_min * 60_000,
        )
    }

    #[test]
    fn empty_requests_is_empty() {
        let calls = Arc::new(Mutex::new(0));
        let out = load_many(
            Path::new("/tmp/nope"),
            Vec::new(),
            FAR_FUTURE_MS,
            &CacheOptions::default(),
            &RateLimiter::unbounded(),
            venue(calls),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn concurrent_results_in_input_order_each_distinct() {
        let dir = unique_dir("order");
        let calls = Arc::new(Mutex::new(0));
        let reqs = vec![
            req("AAA", 0, "1m", 5),
            req("BBB", 1, "1m", 7),
            req("CCC", 2, "1m", 3),
        ];
        let opts = CacheOptions {
            concurrency: 4, // force the threaded path
            ..CacheOptions::default()
        };
        let out = load_many(
            &dir,
            reqs,
            FAR_FUTURE_MS,
            &opts,
            &RateLimiter::unbounded(),
            venue(calls.clone()),
        );
        assert_eq!(out.len(), 3);
        // Each result is Ok, tagged with its own instrument, in INPUT order.
        for (idx, inst) in [(0usize, 0u32), (1, 1), (2, 2)] {
            let bars = out[idx].as_ref().expect("series loaded");
            assert!(!bars.is_empty());
            assert!(bars.iter().all(|b| b.instrument == InstrumentId::new(inst)));
        }
        assert_eq!(
            *calls.lock().unwrap(),
            3,
            "each series fetched exactly once"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_matches_sequential_and_isolates_faults() {
        let dir_seq = unique_dir("seq");
        let dir_par = unique_dir("par");
        // One request has a bogus interval → load_bars errors for that slot only.
        let make_reqs = || {
            vec![
                req("AAA", 0, "1m", 5),
                req("BAD", 1, "not-an-interval", 5),
                req("CCC", 2, "1m", 5),
            ]
        };
        let seq = load_many(
            &dir_seq,
            make_reqs(),
            FAR_FUTURE_MS,
            &CacheOptions {
                concurrency: 1,
                ..CacheOptions::default()
            },
            &RateLimiter::unbounded(),
            venue(Arc::new(Mutex::new(0))),
        );
        let par = load_many(
            &dir_par,
            make_reqs(),
            FAR_FUTURE_MS,
            &CacheOptions {
                concurrency: 4,
                ..CacheOptions::default()
            },
            &RateLimiter::unbounded(),
            venue(Arc::new(Mutex::new(0))),
        );
        // Fault isolation: the middle (bad-interval) slot is Err; the others Ok.
        assert!(seq[0].is_ok() && seq[1].is_err() && seq[2].is_ok());
        assert!(par[0].is_ok() && par[1].is_err() && par[2].is_ok());
        // Concurrent output equals sequential for the successful series.
        assert_eq!(seq[0].as_ref().unwrap(), par[0].as_ref().unwrap());
        assert_eq!(seq[2].as_ref().unwrap(), par[2].as_ref().unwrap());
        let _ = std::fs::remove_dir_all(&dir_seq);
        let _ = std::fs::remove_dir_all(&dir_par);
    }
}
