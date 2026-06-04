// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The **range-aware, resumable** bar-cache loader — the one entry point a caller
//! needs: hand it a series key and a `[start, end)` window and it downloads only the
//! timestamps it doesn't already hold, persists pages incrementally so an interrupt
//! is resumable, and returns the assembled bars.
//!
//! The cache is keyed by *(venue, symbol, interval)* — **never** by the requested
//! window — so a request shifted by even one bar reuses what's present instead of
//! invalidating it ([`missing_gaps`](crate::missing_gaps)). Coverage lives in the
//! JSON [`Manifest`](crate::Manifest) as recorded `(lo, hi)` close-stamp ranges (the
//! **sole** source of truth — gap detection never scans the bar files), so a gap the
//! venue genuinely has no data for is recorded [`EmptyVerified`](crate::Coverage)
//! once and never re-attempted.
//!
//! Flow of [`load_bars`]:
//! 1. Load the manifest; **recover** any orphaned `.partial` journals a previous run
//!    left (lazy-compress them to real `.feather` chunks and record their ranges).
//! 2. Compute the missing sub-ranges of the request from the recorded coverage.
//! 3. For each gap: build the connector feed with a [`PartialWriter`] page-sink
//!    installed (incremental journaling), drain it, write the authoritative
//!    LZ4 `.feather` chunk, record `Bars` for the data actually returned and
//!    `EmptyVerified` for the settled holes the venue had no data for, delete the
//!    `.partial`, and atomically save the manifest.
//! 4. Read back every chunk intersecting the window, merge/dedup, and clip to it.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use akadro_core::{Bar, DataSource, Event, InstrumentId, PageSink};

use crate::bars::{CachedFeed, DataError, read_partition, write_partition};
use crate::gap::missing_gaps;
use crate::incremental::{PartialWriter, recover_partial};
use crate::manifest::{Coverage, Manifest};
use crate::trades::aggregate_bars;
use crate::trades::interval_to_nanos;

/// Identifies one cached `(venue, symbol, interval)` series and the fixed-point
/// scales its bars are stored at.
#[derive(Clone, Debug)]
pub struct SeriesKey {
    /// Venue name, e.g. `"mexc"` (also used for the per-venue rate-limit default).
    pub venue: String,
    /// Venue symbol, e.g. `"BTCUSDT"`.
    pub symbol: String,
    /// Bar interval, e.g. `"1m"` (parsed to the bar width via [`interval_to_nanos`]).
    pub interval: String,
    /// Dense engine instrument id the bars are tagged with.
    pub instrument: InstrumentId,
    /// Price fixed-point scale.
    pub price_scale: u32,
    /// Quantity fixed-point scale.
    pub qty_scale: u32,
}

impl SeriesKey {
    /// Construct a series key.
    #[must_use]
    pub fn new(
        venue: impl Into<String>,
        symbol: impl Into<String>,
        interval: impl Into<String>,
        instrument: InstrumentId,
        price_scale: u32,
        qty_scale: u32,
    ) -> Self {
        Self {
            venue: venue.into(),
            symbol: symbol.into(),
            interval: interval.into(),
            instrument,
            price_scale,
            qty_scale,
        }
    }
}

/// Tunables for the cache loader. All default to sensible, conservative values, so
/// `CacheOptions::default()` is the "just do the right thing" choice.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct CacheOptions {
    /// How often the incremental journal is flushed to disk (resumability vs write
    /// volume). See [`FlushPolicy`](crate::FlushPolicy).
    pub flush: crate::FlushPolicy,
    /// Whether to satisfy a missing interval by aggregating a cached finer interval
    /// before downloading. Wired by the multi-timeframe path (handled by the umbrella
    /// loader, which knows the venue's finest interval).
    pub aggregate: bool,
    /// Requests-per-second cap for downloads, or `None` to use the venue default.
    /// Consumed by the concurrent multi-symbol path.
    pub rate_limit_per_sec: Option<f64>,
    /// Max concurrent symbol downloads for the multi-symbol path (`0`/`1` = sequential).
    pub concurrency: usize,
}

impl Default for CacheOptions {
    fn default() -> Self {
        Self {
            flush: crate::FlushPolicy::balanced(),
            aggregate: true,
            rate_limit_per_sec: None,
            concurrency: 1,
        }
    }
}

/// Filename-safe rendering of a key part (non-alphanumerics → `_`).
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// The shared filename prefix for every chunk/journal of a series.
fn series_base(key: &SeriesKey) -> String {
    format!(
        "{}_{}_{}",
        sanitize(&key.venue),
        sanitize(&key.symbol),
        sanitize(&key.interval)
    )
}

fn chunk_filename(key: &SeriesKey, lo: i64, hi: i64) -> String {
    format!("{}_{lo}_{hi}.feather", series_base(key))
}

/// The manifest file is **per-series** (`{series_base}.manifest.json`), not one
/// shared file, so concurrent multi-series loads never read-modify-write the same
/// manifest (which would lose updates). Each series owns its own coverage record.
fn manifest_path(cache_dir: &Path, key: &SeriesKey) -> PathBuf {
    cache_dir.join(format!("{}.manifest.json", series_base(key)))
}

/// Recover any orphaned `.partial` journals for this series that a previous run left
/// behind (crash/interrupt before finalize): read each back to its intact bars,
/// write a real `.feather` chunk, record the range, and delete the `.partial`. The
/// recovered bars are never re-downloaded; any residual gap is planned afterward.
fn recover_orphans(
    cache_dir: &Path,
    key: &SeriesKey,
    manifest: &mut Manifest,
) -> Result<(), DataError> {
    let prefix = series_base(key);
    let Ok(dir) = std::fs::read_dir(cache_dir) else {
        return Ok(()); // no cache dir yet → nothing to recover
    };
    let mut partials: Vec<PathBuf> = Vec::new();
    for entry in dir.flatten() {
        let path = entry.path();
        let is_ours = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".partial"));
        if is_ours {
            partials.push(path);
        }
    }
    partials.sort(); // deterministic recovery order
    for partial in partials {
        let bars = recover_partial(&partial);
        if bars.is_empty() {
            let _ = std::fs::remove_file(&partial); // nothing recoverable → discard
            continue;
        }
        let lo = bars.first().expect("non-empty").ts.as_nanos();
        let hi = bars.last().expect("non-empty").ts.as_nanos();
        let fname = chunk_filename(key, lo, hi);
        write_partition(
            &cache_dir.join(&fname),
            key.instrument,
            key.price_scale,
            key.qty_scale,
            &bars,
        )?;
        // A recovered range may already overlap recorded coverage if a prior run
        // recorded it before crashing on cleanup; ignore that benign overlap.
        let _ = manifest.record_range(
            &key.venue,
            &key.symbol,
            &key.interval,
            &fname,
            (lo, hi),
            Coverage::Bars,
        );
        let _ = std::fs::remove_file(&partial);
    }
    manifest.save(&manifest_path(cache_dir, key))?;
    Ok(())
}

/// Record coverage for one fetched gap `(g_lo, g_hi)` (close-stamp ns): write the
/// `Bars` chunk for what the venue actually returned, and mark the **settled** holes
/// *around that data* (a retention prefix / an interior data-gap) as `EmptyVerified`
/// so they are never re-fetched.
///
/// `now_ns` is the injected wall-clock (IO layer, not the engine): a hole is only
/// recorded `EmptyVerified` when it lies before `now_ns - bar_ns` (settled), so a
/// not-yet-closed recent bar is left to be fetched on a later load.
///
/// **A *fully*-empty gap is deliberately NOT recorded** as verified-empty. A
/// completely empty result is indistinguishable from a failed fetch — the connectors
/// surface a transport/parse error as `None` (an empty feed), so dead-marking a fully
/// empty gap would let one transient error (or a mis-formed request) permanently
/// suppress re-download of data the venue actually has (silent data loss). The
/// realistic "more history than the venue keeps" case still returns *some* data, whose
/// settled **leading hole** is recorded `EmptyVerified` below — so requesting deep
/// history does not re-download forever. The only cost of not dead-marking a fully
/// empty gap is re-probing a window the venue has *no* data for at all (rare, and one
/// empty page per load).
fn record_fetched_gap(
    cache_dir: &Path,
    key: &SeriesKey,
    manifest: &mut Manifest,
    gap: (i64, i64),
    bars: &[Bar],
    now_ns: i64,
    bar_ns: i64,
) -> Result<(), DataError> {
    let (g_lo, g_hi) = gap;
    let cutoff = now_ns.saturating_sub(bar_ns); // close-stamps below this are settled

    if bars.is_empty() {
        return Ok(()); // empty == possibly-failed fetch → re-probe next load, never dead-mark
    }

    let a_lo = bars.first().expect("non-empty").ts.as_nanos();
    let a_hi = bars.last().expect("non-empty").ts.as_nanos();
    let fname = chunk_filename(key, a_lo, a_hi);
    write_partition(
        &cache_dir.join(&fname),
        key.instrument,
        key.price_scale,
        key.qty_scale,
        bars,
    )?;
    manifest.record_range(
        &key.venue,
        &key.symbol,
        &key.interval,
        &fname,
        (a_lo, a_hi),
        Coverage::Bars,
    )?;

    // Leading hole [g_lo, a_lo - bar]: the venue skipped it (e.g. retention horizon).
    // It is entirely before a real bar, hence settled → verified-empty (this is what
    // stops a request for more history than the venue keeps from re-downloading the
    // missing prefix forever).
    if a_lo.saturating_sub(bar_ns) >= g_lo {
        manifest.record_range(
            &key.venue,
            &key.symbol,
            &key.interval,
            "",
            (g_lo, a_lo - bar_ns),
            Coverage::EmptyVerified,
        )?;
    }

    // Trailing hole [a_hi + bar, min(g_hi, cutoff)]: only the *settled* part — the
    // rest of the gap toward `now` is left unrecorded so a later load picks up newly
    // closed bars instead of falsely marking them dead.
    let trail_lo = a_hi.saturating_add(bar_ns);
    let trail_hi = g_hi.min(cutoff);
    if trail_hi >= trail_lo {
        manifest.record_range(
            &key.venue,
            &key.symbol,
            &key.interval,
            "",
            (trail_lo, trail_hi),
            Coverage::EmptyVerified,
        )?;
    }
    Ok(())
}

/// Read back every cached `Bars` chunk for the series whose range intersects
/// `[req_lo, req_hi]` (close-stamp ns), merge them, dedup equal close-stamps, and clip
/// to the window. `EmptyVerified` chunks (no file) are skipped.
fn assemble(
    cache_dir: &Path,
    key: &SeriesKey,
    manifest: &Manifest,
    req_lo: i64,
    req_hi: i64,
) -> Result<Vec<Bar>, DataError> {
    let mut out: Vec<Bar> = Vec::new();
    if let Some(entry) = manifest.entry(&key.venue, &key.symbol, &key.interval) {
        for ((file, &(lo, hi)), &cov) in entry
            .partition_files
            .iter()
            .zip(&entry.cached_ranges)
            .zip(&entry.coverage)
        {
            if cov != Coverage::Bars || hi < req_lo || lo > req_hi {
                continue; // empty-verified, or disjoint from the request
            }
            let loaded = read_partition(&cache_dir.join(file))?;
            if loaded.instrument != key.instrument
                || loaded.price_scale != key.price_scale
                || loaded.qty_scale != key.qty_scale
            {
                return Err(DataError::Schema(format!(
                    "cached chunk {file} holds instrument {:?} scale ({}, {}) but the request is \
                     for {:?} scale ({}, {}) — delete the stale cache and re-fetch",
                    loaded.instrument,
                    loaded.price_scale,
                    loaded.qty_scale,
                    key.instrument,
                    key.price_scale,
                    key.qty_scale,
                )));
            }
            out.extend(loaded.bars);
        }
    }
    out.sort_by_key(|b| b.ts.as_nanos());
    out.dedup_by_key(|b| b.ts.as_nanos());
    out.retain(|b| (req_lo..=req_hi).contains(&b.ts.as_nanos()));
    Ok(out)
}

/// Wraps the incremental [`PartialWriter`] page-sink so the loader can also observe a
/// connector's [`PageSink::on_error`] *truncation* signal. This is what lets the loader
/// tell a fetch that aborted mid-stream apart from a genuinely-complete (or
/// genuinely-empty) download — the distinction the `record_fetched_gap` doc warned was
/// previously unresolvable, and the one that prevents dead-marking an error-truncated
/// remainder as verified-empty (silent data loss).
struct ObservingSink {
    inner: Box<dyn PageSink>,
    error: Rc<RefCell<Option<String>>>,
}

impl PageSink for ObservingSink {
    fn on_page(&mut self, page: &[Bar]) {
        self.inner.on_page(page);
    }
    fn on_error(&mut self, error: &str) {
        // The connector signals once when it gives up; keep the first error.
        self.error
            .borrow_mut()
            .get_or_insert_with(|| error.to_string());
        self.inner.on_error(error);
    }
}

/// Load bars for `key` over `[start_ms, end_ms)` (epoch ms), downloading only the
/// gaps not already cached and persisting pages incrementally so an interrupt is
/// resumable.
///
/// `now_ms` is the current wall-clock (epoch ms), injected so the function stays
/// deterministic and testable; it governs only whether a no-data gap is "settled"
/// enough to record as verified-empty. `make_source(lo_ms, hi_ms, sink)` builds the
/// connector feed for one gap with the incremental-journal page `sink` installed
/// (the venue-neutral seam — the umbrella's per-venue dispatch supplies it).
///
/// # Errors
/// [`DataError`] if the interval is unparseable or any cache read/write fails;
/// [`DataError::Fetch`] if a connector aborts a download mid-stream (signalled via
/// [`PageSink::on_error`]). On a truncation the bars already fetched are journaled to a
/// `.partial`, so a retry resumes from them rather than re-downloading — and crucially
/// the unfetched remainder is left as a real gap, never dead-marked verified-empty.
pub fn load_bars<D, F>(
    cache_dir: &Path,
    key: &SeriesKey,
    start_ms: i64,
    end_ms: i64,
    now_ms: i64,
    opts: &CacheOptions,
    make_source: F,
) -> Result<Vec<Bar>, DataError>
where
    D: DataSource,
    F: Fn(i64, i64, Box<dyn PageSink>) -> D,
{
    let bar_ns = interval_to_nanos(&key.interval)
        .ok_or_else(|| DataError::Schema(format!("unrecognized interval {:?}", key.interval)))?;
    std::fs::create_dir_all(cache_dir)?;
    let mpath = manifest_path(cache_dir, key);
    let mut manifest = Manifest::load(&mpath)?;

    // (1) Recover any orphaned journals before planning, so resumed bars aren't
    // re-downloaded.
    recover_orphans(cache_dir, key, &mut manifest)?;

    // (2) Plan the missing sub-ranges from recorded coverage (close-stamp ns).
    let req_lo = start_ms.saturating_mul(1_000_000);
    let req_hi = end_ms.saturating_mul(1_000_000);
    let wall_now_ns = now_ms.saturating_mul(1_000_000);
    let cached: Vec<(i64, i64)> = manifest
        .entry(&key.venue, &key.symbol, &key.interval)
        .map(|e| e.cached_ranges.clone())
        .unwrap_or_default();
    let gaps = missing_gaps(&cached, req_lo, req_hi, bar_ns);

    // (3) Fetch each gap, journaling incrementally, then record coverage.
    for (g_lo, g_hi) in gaps {
        let partial = cache_dir.join(format!("{}_{g_lo}_{g_hi}.partial", series_base(key)));
        let writer = PartialWriter::create(
            &partial,
            key.instrument,
            key.price_scale,
            key.qty_scale,
            opts.flush,
        )?;
        // Observe the connector's truncation signal: a fetch that aborts mid-stream
        // must not have its unfetched remainder dead-marked verified-empty.
        let fetch_error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let sink = ObservingSink {
            inner: Box::new(writer),
            error: Rc::clone(&fetch_error),
        };
        // Fetch [g_lo, g_hi] in epoch-ms (end-exclusive). Gaps are in **close-stamp**
        // ns but a connector's `with_range` takes **open** time, so the bar that
        // *closes* at `g_lo` *opens* at `g_lo - bar_ns`: start the fetch there, or the
        // boundary bar at a download seam is skipped (a 1-bar hole). The loader clips
        // the collected bars back to `[g_lo, g_hi]` close-stamps, so the extra reach is
        // harmless. The connector clips/sorts too.
        let g_lo_ms = (g_lo - bar_ns).max(0) / 1_000_000;
        let g_hi_ms = g_hi / 1_000_000 + 1;
        let mut feed = make_source(g_lo_ms, g_hi_ms, Box::new(sink));
        let mut collected: Vec<Bar> = Vec::new();
        while let Some(event) = feed.next_event() {
            if let Event::Bar(bar) = event
                && bar.instrument == key.instrument
            {
                collected.push(bar);
            }
        }
        drop(feed); // close the journal file (flush) before we inspect/keep/delete it

        // A truncated download (the connector signalled `on_error`) is surfaced rather
        // than silently cached: the bars already journaled stay in the `.partial` (NOT
        // deleted), so the next load's orphan recovery resumes from them, and the
        // unfetched remainder is left as a real gap to retry instead of being
        // dead-marked verified-empty. Fail-fast: the caller learns the window is
        // incomplete rather than backtesting over a hole.
        if let Some(msg) = fetch_error.borrow().clone() {
            manifest.save(&mpath)?; // persist coverage recorded for any earlier gaps
            return Err(DataError::Fetch(format!(
                "{}/{}/{} gap [{g_lo}, {g_hi}] download aborted mid-stream ({msg}); \
                 partial progress journaled for resume — retry to continue",
                key.venue, key.symbol, key.interval
            )));
        }

        collected.sort_by_key(|b| b.ts.as_nanos());
        collected.dedup_by_key(|b| b.ts.as_nanos());
        // Keep only bars within the gap (defensive: connectors already clip).
        collected.retain(|b| (g_lo..=g_hi).contains(&b.ts.as_nanos()));

        record_fetched_gap(
            cache_dir,
            key,
            &mut manifest,
            (g_lo, g_hi),
            &collected,
            wall_now_ns,
            bar_ns,
        )?;
        let _ = std::fs::remove_file(&partial); // success → journal no longer needed
        manifest.save(&mpath)?; // persist after each gap so progress survives an interrupt
    }

    // (4) Assemble the window from the cached chunks.
    assemble(cache_dir, key, &manifest, req_lo, req_hi)
}

/// As [`load_bars`], but returns a ready-to-drive [`CachedFeed`] ([`DataSource`]) —
/// the blessed cache→engine path.
///
/// # Errors
/// Returns a [`DataError`] if the load fails (see [`load_bars`]).
pub fn load_bars_feed<D, F>(
    cache_dir: &Path,
    key: &SeriesKey,
    start_ms: i64,
    end_ms: i64,
    now_ms: i64,
    opts: &CacheOptions,
    make_source: F,
) -> Result<CachedFeed, DataError>
where
    D: DataSource,
    F: Fn(i64, i64, Box<dyn PageSink>) -> D,
{
    Ok(CachedFeed::from_cached(load_bars(
        cache_dir,
        key,
        start_ms,
        end_ms,
        now_ms,
        opts,
        make_source,
    )?))
}

/// Load bars at a **coarser** interval by reusing a cached **finer** series and
/// aggregating it up — the multi-timeframe path (requirement #5). The finer series
/// (`finer_key`, e.g. 1m) is gap-filled over the window via [`load_bars`] (downloading
/// only what's missing, incrementally + resumably), then rolled up to `coarse_ns`
/// (e.g. 5m) with [`aggregate_bars`]. Only **complete** coarse buckets are returned,
/// so a partial bucket at an edge or hole is dropped rather than reported as a
/// finished bar.
///
/// This honours "download as little as possible": a finer series already on disk is
/// reused for free, and only its missing sub-ranges hit the network. Caching at the
/// finer granularity also makes the data reusable for every coarser interval.
///
/// `coarse_ns` must be a whole multiple of the finer interval's width
/// (`coarse_ns % finer_ns == 0`, `coarse_ns >= finer_ns`).
///
/// # Errors
/// [`DataError::Schema`] if `finer_key`'s interval is unparseable or `coarse_ns` is
/// not an aggregable multiple of it; otherwise any error from the finer [`load_bars`].
// One more arg than `load_bars` (the coarse width); all are meaningful and a builder
// would be noisier for a single call site (matches the venue-feed `new` convention).
#[allow(clippy::too_many_arguments)]
pub fn load_bars_aggregated<D, F>(
    cache_dir: &Path,
    finer_key: &SeriesKey,
    coarse_ns: i64,
    start_ms: i64,
    end_ms: i64,
    now_ms: i64,
    opts: &CacheOptions,
    make_finer_source: F,
) -> Result<Vec<Bar>, DataError>
where
    D: DataSource,
    F: Fn(i64, i64, Box<dyn PageSink>) -> D,
{
    let finer_ns = interval_to_nanos(&finer_key.interval).ok_or_else(|| {
        DataError::Schema(format!(
            "unrecognized finer interval {:?}",
            finer_key.interval
        ))
    })?;
    if coarse_ns < finer_ns || coarse_ns % finer_ns != 0 {
        return Err(DataError::Schema(format!(
            "coarse width {coarse_ns}ns is not a whole multiple of finer width {finer_ns}ns \
             (cannot aggregate {:?} up to it)",
            finer_key.interval
        )));
    }
    let finer = load_bars(
        cache_dir,
        finer_key,
        start_ms,
        end_ms,
        now_ms,
        opts,
        make_finer_source,
    )?;
    Ok(aggregate_bars(&finer, finer_ns, coarse_ns))
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{Price, Qty, Timestamp};
    use std::cell::RefCell;
    use std::rc::Rc;

    const BAR_NS: i64 = 60_000_000_000; // 1m
    const INSTR: InstrumentId = InstrumentId::new(3);
    const FAR_FUTURE_MS: i64 = 1_000_000_000; // now ≫ any test close-stamp → settled

    fn key() -> SeriesKey {
        SeriesKey::new("testvenue", "BTC/USDT", "1m", INSTR, 2, 0)
    }

    fn unique_dir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "akadro_load_{name}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn bar_at(close_ns: i64) -> Bar {
        Bar::new(
            INSTR,
            Timestamp::from_nanos(close_ns),
            Price::from_raw(100),
            Price::from_raw(105),
            Price::from_raw(95),
            Price::from_raw(100),
            Qty::from_raw(1),
        )
    }

    /// A fake connector feed: on construction it journals every bar to the page-sink
    /// (simulating the connector's single internal back-fill), then replays them.
    struct FakeFeed {
        bars: std::vec::IntoIter<Bar>,
    }
    impl FakeFeed {
        fn new(bars: Vec<Bar>, mut sink: Box<dyn PageSink>) -> Self {
            sink.on_page(&bars); // the connector hands each page to the journal
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

    /// A feed that journals bars up to `up_to_ns` (the pages it managed to fetch) and
    /// then signals a *truncation* via [`PageSink::on_error`] instead of completing —
    /// simulating a mid-download transport failure. With `up_to_ns` below the window
    /// start it journals nothing (a first-page failure that returns an empty result).
    struct TruncatingFeed {
        bars: std::vec::IntoIter<Bar>,
    }
    impl TruncatingFeed {
        fn new(partial: Vec<Bar>, mut sink: Box<dyn PageSink>) -> Self {
            sink.on_page(&partial); // journal whatever pages were fetched
            sink.on_error("simulated mid-download transport error"); // then truncate
            TruncatingFeed {
                bars: partial.into_iter(),
            }
        }
    }
    impl DataSource for TruncatingFeed {
        fn next_event(&mut self) -> Option<Event> {
            self.bars.next().map(Event::Bar)
        }
    }
    fn truncating_venue(
        up_to_ns: i64,
        calls: Rc<RefCell<Vec<(i64, i64)>>>,
    ) -> impl Fn(i64, i64, Box<dyn PageSink>) -> TruncatingFeed {
        move |lo_ms, hi_ms, sink| {
            calls.borrow_mut().push((lo_ms, hi_ms));
            let (lo_ns, hi_ns) = (lo_ms * 1_000_000, hi_ms * 1_000_000);
            let mut bars = Vec::new();
            let mut c = lo_ns;
            while c < hi_ns && c <= up_to_ns {
                bars.push(bar_at(c));
                c += BAR_NS;
            }
            TruncatingFeed::new(bars, sink)
        }
    }

    /// Build a `make_source` closure whose "venue" only has data with close-stamps in
    /// `available`; it records each `(lo_ms, hi_ms)` it is asked for into `calls`.
    fn venue(
        available: (i64, i64),
        calls: Rc<RefCell<Vec<(i64, i64)>>>,
    ) -> impl Fn(i64, i64, Box<dyn PageSink>) -> FakeFeed {
        move |lo_ms, hi_ms, sink| {
            calls.borrow_mut().push((lo_ms, hi_ms));
            let lo_ns = lo_ms * 1_000_000;
            let hi_ns = hi_ms * 1_000_000;
            let mut bars = Vec::new();
            let mut c = lo_ns;
            while c < hi_ns {
                if c >= available.0 && c <= available.1 {
                    bars.push(bar_at(c));
                }
                c += BAR_NS;
            }
            FakeFeed::new(bars, sink)
        }
    }

    fn load(
        dir: &Path,
        start_ms: i64,
        end_ms: i64,
        now_ms: i64,
        avail: (i64, i64),
        calls: &Rc<RefCell<Vec<(i64, i64)>>>,
    ) -> Vec<Bar> {
        load_bars(
            dir,
            &key(),
            start_ms,
            end_ms,
            now_ms,
            &CacheOptions::default(),
            venue(avail, calls.clone()),
        )
        .unwrap()
    }

    const ALL: (i64, i64) = (i64::MIN, i64::MAX);
    const NONE_AVAIL: (i64, i64) = (1, 0); // empty interval → venue returns nothing

    #[test]
    fn cold_load_fetches_then_warm_load_reuses_cache() {
        let dir = unique_dir("warm");
        let calls = Rc::new(RefCell::new(Vec::new()));
        // Cold: [0, 5min) → fetch one gap.
        let first = load(&dir, 0, 5 * 60_000, FAR_FUTURE_MS, ALL, &calls);
        assert_eq!(calls.borrow().len(), 1, "cold load fetches once");
        assert!(!first.is_empty());
        // Warm: identical request → no fetch, same bars.
        let second = load(&dir, 0, 5 * 60_000, FAR_FUTURE_MS, ALL, &calls);
        assert_eq!(calls.borrow().len(), 1, "warm load fetches nothing more");
        assert_eq!(first, second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shifted_window_reuses_cache_no_refetch() {
        let dir = unique_dir("shift");
        let calls = Rc::new(RefCell::new(Vec::new()));
        load(&dir, 0, 10 * 60_000, FAR_FUTURE_MS, ALL, &calls);
        assert_eq!(calls.borrow().len(), 1);
        // A window shifted INSIDE the covered span (the "1-second shift" case) → no
        // new fetch, and the returned subset stays within the request.
        let sub = load(&dir, 60_000, 9 * 60_000, FAR_FUTURE_MS, ALL, &calls);
        assert_eq!(
            calls.borrow().len(),
            1,
            "shifted window must not re-download"
        );
        assert!(
            sub.iter()
                .all(|b| (60_000 * 1_000_000..=9 * 60_000 * 1_000_000).contains(&b.ts.as_nanos()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn interior_gap_fetches_only_the_hole() {
        let dir = unique_dir("interior");
        let calls = Rc::new(RefCell::new(Vec::new()));
        // Prime two disjoint spans, leaving a hole between them.
        load(&dir, 0, 3 * 60_000, FAR_FUTURE_MS, ALL, &calls);
        load(&dir, 6 * 60_000, 9 * 60_000, FAR_FUTURE_MS, ALL, &calls);
        assert_eq!(calls.borrow().len(), 2);
        // Full request: only the middle hole is fetched (one more call).
        load(&dir, 0, 9 * 60_000, FAR_FUTURE_MS, ALL, &calls);
        assert_eq!(calls.borrow().len(), 3, "only the interior hole is fetched");
        // The third fetch's window is the hole, not the whole range.
        let (lo_ms, _hi_ms) = calls.borrow()[2];
        assert!(
            lo_ms >= 3 * 60_000,
            "the refetch starts after the first span"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fully_empty_gap_is_reprobed_not_dead_marked() {
        let dir = unique_dir("empty_settled");
        let calls = Rc::new(RefCell::new(Vec::new()));
        // The venue returns nothing for the whole window (settled: now ≫ end). A fully
        // empty result is indistinguishable from a failed fetch, so it is RE-PROBED on
        // the next load rather than dead-marked EmptyVerified (which would risk silent
        // data loss if the emptiness was actually a transient error).
        let first = load(&dir, 0, 5 * 60_000, FAR_FUTURE_MS, NONE_AVAIL, &calls);
        assert!(first.is_empty());
        assert_eq!(calls.borrow().len(), 1);
        let second = load(&dir, 0, 5 * 60_000, FAR_FUTURE_MS, NONE_AVAIL, &calls);
        assert!(second.is_empty());
        assert_eq!(
            calls.borrow().len(),
            2,
            "a fully-empty gap is re-probed (could have been a transient fetch error)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mid_download_error_is_surfaced_and_resumable() {
        let dir = unique_dir("truncate");
        let calls = Rc::new(RefCell::new(Vec::new()));
        // First load: the venue returns the first ~4 minutes, then a transport error
        // truncates the rest (it signals `on_error`). The OLD behaviour recorded the
        // returned bars + dead-marked the trailing hole verified-empty → the tail was
        // silently lost forever. The fix surfaces the truncation instead.
        let res = load_bars(
            &dir,
            &key(),
            0,
            10 * 60_000,
            FAR_FUTURE_MS,
            &CacheOptions::default(),
            truncating_venue(4 * BAR_NS, calls.clone()),
        );
        assert!(
            matches!(res, Err(DataError::Fetch(_))),
            "a truncated download is surfaced (fail-fast), not silently cached as partial"
        );
        // The pages already fetched are journaled (the `.partial` is KEPT for resume,
        // not deleted) rather than discarded over one hiccup.
        let has_partial = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".partial"));
        assert!(
            has_partial,
            "partial progress is journaled (kept) for resume"
        );
        // Second load with a healthy venue: the error-truncated tail is re-fetched (it
        // was NOT dead-marked verified-empty), so the full window comes back — proving
        // no silent data loss.
        let bars = load(&dir, 0, 10 * 60_000, FAR_FUTURE_MS, ALL, &calls);
        assert!(
            bars.iter().any(|b| b.ts.as_nanos() > 4 * BAR_NS),
            "the error-truncated tail is recovered on retry, not silently dropped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_result_with_error_is_surfaced_unlike_genuine_empty() {
        let dir = unique_dir("empty_err");
        let calls = Rc::new(RefCell::new(Vec::new()));
        // An empty result that arrived WITH a fetch error is surfaced as Err — distinct
        // from a genuinely-empty no-data window, which re-probes and returns Ok (see
        // `fully_empty_gap_is_reprobed_not_dead_marked`). The `on_error` signal is what
        // resolves the "empty == possibly-failed fetch" ambiguity the loader doc noted.
        let res = load_bars(
            &dir,
            &key(),
            0,
            5 * 60_000,
            FAR_FUTURE_MS,
            &CacheOptions::default(),
            truncating_venue(-1, calls.clone()), // journals nothing, then errors
        );
        assert!(
            matches!(res, Err(DataError::Fetch(_))),
            "an empty result caused by a fetch error is surfaced, not silently re-probed as no-data"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsettled_no_data_gap_is_retried_not_marked_empty() {
        let dir = unique_dir("empty_recent");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let end_ms = 5 * 60_000;
        // now == end of the request → the gap's recent end is NOT settled.
        load(&dir, 0, end_ms, end_ms, NONE_AVAIL, &calls);
        assert_eq!(calls.borrow().len(), 1);
        // Because nothing was marked verified-empty, a second load retries (the bars
        // may simply not have closed yet).
        load(&dir, 0, end_ms, end_ms, NONE_AVAIL, &calls);
        assert_eq!(
            calls.borrow().len(),
            2,
            "an unsettled (recent) empty gap must be retried, not dead-marked"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_leading_gap_marked_empty_once() {
        let dir = unique_dir("retention");
        let calls = Rc::new(RefCell::new(Vec::new()));
        // The venue only has data from 3min onward (older history aged out).
        let avail = (3 * BAR_NS, i64::MAX);
        let first = load(&dir, 0, 10 * 60_000, FAR_FUTURE_MS, avail, &calls);
        assert_eq!(calls.borrow().len(), 1);
        // Returned bars start at the venue's earliest available close-stamp.
        assert_eq!(first.first().unwrap().ts.as_nanos(), 3 * BAR_NS);
        // Second load: the un-retained prefix was marked EmptyVerified → no refetch.
        let second = load(&dir, 0, 10 * 60_000, FAR_FUTURE_MS, avail, &calls);
        assert_eq!(
            calls.borrow().len(),
            1,
            "the venue's missing-history prefix must not be re-downloaded forever"
        );
        assert_eq!(first, second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aggregation_rolls_finer_cache_up_and_reuses_it() {
        let dir = unique_dir("agg");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let finer = key(); // 1m series
        let coarse_ns = 5 * BAR_NS; // request 5m
        // First call: fetches the 1m series once, aggregates to 5m.
        let first = load_bars_aggregated(
            &dir,
            &finer,
            coarse_ns,
            0,
            30 * 60_000, // 30 minutes
            FAR_FUTURE_MS,
            &CacheOptions::default(),
            venue(ALL, calls.clone()),
        )
        .unwrap();
        assert_eq!(calls.borrow().len(), 1, "fetches the finer series once");
        assert!(!first.is_empty());
        // The result is genuinely coarse: 5m close-stamps are multiples of 5m.
        assert!(first.iter().all(|b| b.ts.as_nanos() % coarse_ns == 0));

        // Second call: the 1m cache is reused — NO new download — same coarse bars.
        let second = load_bars_aggregated(
            &dir,
            &finer,
            coarse_ns,
            0,
            30 * 60_000,
            FAR_FUTURE_MS,
            &CacheOptions::default(),
            venue(ALL, calls.clone()),
        )
        .unwrap();
        assert_eq!(
            calls.borrow().len(),
            1,
            "aggregation reuses the cached finer series, no re-download"
        );
        assert_eq!(first, second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aggregation_rejects_non_divisible_coarse() {
        let dir = unique_dir("agg_bad");
        let calls = Rc::new(RefCell::new(Vec::new()));
        // 1m finer, ask for a 7m coarse (not a multiple of 1m? 7m IS a multiple of 1m;
        // use a coarse smaller than finer to trigger the guard).
        let err = load_bars_aggregated(
            &dir,
            &key(),
            BAR_NS / 2, // coarse < finer → invalid
            0,
            10 * 60_000,
            FAR_FUTURE_MS,
            &CacheOptions::default(),
            venue(ALL, calls.clone()),
        );
        assert!(
            err.is_err(),
            "a coarse width below the finer width is rejected"
        );
        assert_eq!(calls.borrow().len(), 0, "no download on a config error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphan_partial_is_recovered_on_load() {
        use crate::FlushPolicy;
        let dir = unique_dir("orphan");
        std::fs::create_dir_all(&dir).unwrap();
        // Simulate an interrupted previous run: a flushed-but-unfinalized .partial.
        let partial = dir.join(format!("{}_0_99.partial", series_base(&key())));
        {
            let mut w =
                PartialWriter::create(&partial, INSTR, 2, 0, FlushPolicy::every_page()).unwrap();
            w.push_page(&[bar_at(BAR_NS), bar_at(2 * BAR_NS), bar_at(3 * BAR_NS)]);
        }
        let calls = Rc::new(RefCell::new(Vec::new()));
        // Loading the recovered window returns the recovered bars; the venue is only
        // consulted for whatever is still missing around them — never for the recovered
        // span itself.
        let bars = load(&dir, 1, 4 * 60_000, FAR_FUTURE_MS, NONE_AVAIL, &calls);
        assert!(
            bars.iter().any(|b| b.ts.as_nanos() == 2 * BAR_NS),
            "recovered bars are present after load"
        );
        assert!(!partial.exists(), "the orphan .partial is finalized away");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
