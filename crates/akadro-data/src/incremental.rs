// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Incremental, resumable cache writes — persisting downloaded pages **as they
//! arrive** so an interrupt (Ctrl-C, crash, power loss) preserves progress instead
//! of discarding a half-finished multi-hour back-fill.
//!
//! This module currently holds the [`FlushPolicy`] cadence knob; the streaming
//! `.partial` writer that consumes it is added alongside (see the crate's cache
//! loader).
//!
//! **On wall-clock here:** [`FlushPolicy::every`] is a duration, and the writer it
//! drives reads `Instant::now()`. That is deliberate and *sound* — this is the
//! **disk-IO layer**, not the deterministic engine. The look-ahead/parity rules
//! (no wall-clock reachable from `Ctx`; D4/D6) govern the *event loop*, where timing
//! could leak into a `Bar`/fill/`RunReport`. Flush cadence only decides *when bytes
//! hit disk*; it never changes *which* bars are cached or in what order, so it can
//! never perturb a backtest result. The bars a run sees are identical whether a
//! flush happened every page or once at the end.

use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

use akadro_core::{Bar, InstrumentId, PageSink};
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::StreamWriter;

use crate::bars::{DataError, bars_to_batch, batch_to_bars, schema_with_meta};

/// How often the incremental writer flushes buffered pages to the on-disk
/// `.partial` — the trade-off between disk-write volume and how much a crash loses.
///
/// A flush fires when **either** bound is reached: `every_pages` pages have been
/// buffered, **or** `every` wall-clock time has elapsed since the last flush
/// (whichever comes first). `None` disables that bound; with both `None` the writer
/// only flushes at finalize (maximum throughput, nothing recovered on a crash).
///
/// The [default](Self::balanced) is [`balanced`](Self::balanced): every 8 pages or
/// 5 seconds — a few writes per minute on a fast back-fill, at most a handful of
/// pages lost to an interrupt.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushPolicy {
    /// Flush after this many buffered pages, if `Some`.
    pub every_pages: Option<u32>,
    /// Flush after this much elapsed wall-clock since the last flush, if `Some`.
    pub every: Option<Duration>,
}

impl FlushPolicy {
    /// The balanced default: flush every **8 pages or 5 seconds**, whichever first.
    #[must_use]
    pub const fn balanced() -> Self {
        Self {
            every_pages: Some(8),
            every: Some(Duration::from_secs(5)),
        }
    }

    /// Flush after **every page** — maximum restorability, most disk writes. Suits a
    /// slow/expensive back-fill where losing even one page is undesirable.
    #[must_use]
    pub const fn every_page() -> Self {
        Self {
            every_pages: Some(1),
            every: None,
        }
    }

    /// Flush every `n` pages (no time bound). `n == 0` is treated as `1` (flushing
    /// "every 0 pages" is meaningless; fail safe toward more-frequent flushing).
    #[must_use]
    pub const fn every_pages(n: u32) -> Self {
        Self {
            every_pages: Some(if n == 0 { 1 } else { n }),
            every: None,
        }
    }

    /// Flush every `d` of elapsed wall-clock (no page bound).
    #[must_use]
    pub const fn every(d: Duration) -> Self {
        Self {
            every_pages: None,
            every: Some(d),
        }
    }

    /// Whether a flush is due given `pages_since_flush` buffered pages and `elapsed`
    /// since the last flush. `true` if either configured bound is met.
    #[must_use]
    pub fn is_due(&self, pages_since_flush: u32, elapsed: Duration) -> bool {
        let by_pages = self.every_pages.is_some_and(|n| pages_since_flush >= n);
        let by_time = self.every.is_some_and(|d| elapsed >= d);
        by_pages || by_time
    }
}

impl Default for FlushPolicy {
    fn default() -> Self {
        Self::balanced()
    }
}

/// An incremental, resumable writer for one downloaded chunk.
///
/// It is the [`PageSink`] the cache loader installs on a connector feed: each
/// downloaded page is appended to an **uncompressed Arrow IPC `.partial` stream**
/// (cheap, append-only — no footer rewrite) per the [`FlushPolicy`] cadence, so an
/// interrupt leaves the flushed pages recoverable on disk via [`recover_partial`].
///
/// It is a **pure crash-recovery journal** — it does *not* own the authoritative
/// finalize. The cache loader collects the bars from the feed directly (the
/// connector yields the same bars it hands here), writes the compact LZ4 `.feather`
/// via [`write_partition`], records the range, and deletes the `.partial` on success.
/// The journal exists only so a *future* process can recover after a crash that the
/// loader's finalize never ran. This keeps the bars single-copy in the loader (no
/// duplicate in-memory accumulation here).
///
/// Journal writes are **best-effort**: a flush failure (e.g. a transient disk error)
/// sets `journal_broken` and stops further `.partial` writes; it is *not* surfaced
/// through [`PageSink::on_page`] (which can't fail by contract). The download proceeds
/// in the loader's memory and any real I/O failure surfaces when the loader writes the
/// `.feather`. A broken journal only costs a resume point, never correctness.
pub(crate) struct PartialWriter {
    writer: StreamWriter<File>,
    schema: std::sync::Arc<arrow_schema::Schema>,
    /// Bars not yet flushed to the `.partial` journal.
    buffer: Vec<Bar>,
    pages_since_flush: u32,
    last_flush: Instant,
    policy: FlushPolicy,
    /// Set once a journal write fails; further `.partial` writes are skipped.
    journal_broken: bool,
}

impl PartialWriter {
    /// Create a fresh `.partial` journal at `partial_path` and write the stream
    /// header. The caller owns the path (gap-unique) and, on success, the matching
    /// `.feather` finalize + `.partial` deletion.
    ///
    /// # Errors
    /// [`DataError::Io`] if the file can't be created, or [`DataError::Arrow`] if the
    /// stream header can't be written.
    pub(crate) fn create(
        partial_path: &Path,
        instrument: InstrumentId,
        price_scale: u32,
        qty_scale: u32,
        policy: FlushPolicy,
    ) -> Result<Self, DataError> {
        let schema = schema_with_meta(instrument, price_scale, qty_scale);
        let file = File::create(partial_path)?;
        let writer =
            StreamWriter::try_new(file, &schema).map_err(|e| DataError::Arrow(e.to_string()))?;
        Ok(Self {
            writer,
            schema,
            buffer: Vec::new(),
            pages_since_flush: 0,
            last_flush: Instant::now(),
            policy,
            journal_broken: false,
        })
    }

    /// Append one page to the journal buffer, then flush it to the `.partial` if the
    /// [`FlushPolicy`] cadence is due. Flush errors are swallowed (best-effort journal
    /// — see the type docs).
    pub(crate) fn push_page(&mut self, page: &[Bar]) {
        if page.is_empty() {
            return;
        }
        self.buffer.extend_from_slice(page);
        self.pages_since_flush += 1;
        if self
            .policy
            .is_due(self.pages_since_flush, self.last_flush.elapsed())
        {
            let _ = self.flush(); // best-effort; a broken journal only loses resume
        }
    }

    /// Flush the buffered bars to the `.partial` as one stream batch and `fsync`.
    /// A no-op when the buffer is empty or the journal is already broken.
    fn flush(&mut self) -> Result<(), DataError> {
        if self.buffer.is_empty() || self.journal_broken {
            return Ok(());
        }
        let batch = bars_to_batch(&self.schema, &self.buffer)?;
        let res = (|| -> Result<(), DataError> {
            self.writer
                .write(&batch)
                .map_err(|e| DataError::Arrow(e.to_string()))?;
            self.writer
                .flush()
                .map_err(|e| DataError::Arrow(e.to_string()))?;
            // Durable: force the appended batch to disk so a crash recovers it.
            self.writer.get_ref().sync_all()?;
            Ok(())
        })();
        match res {
            Ok(()) => {
                self.buffer.clear();
                self.pages_since_flush = 0;
                self.last_flush = Instant::now();
                Ok(())
            }
            Err(e) => {
                self.journal_broken = true;
                Err(e)
            }
        }
    }
}

impl std::fmt::Debug for PartialWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hide the Arrow writer (not Debug); show the journal state a reader cares about.
        f.debug_struct("PartialWriter")
            .field("buffered", &self.buffer.len())
            .field("pages_since_flush", &self.pages_since_flush)
            .field("journal_broken", &self.journal_broken)
            .finish_non_exhaustive()
    }
}

impl PageSink for PartialWriter {
    fn on_page(&mut self, page: &[Bar]) {
        self.push_page(page);
    }
}

/// Recover the bars from an orphaned `.partial` left by a crashed/interrupted
/// download, tolerating a **truncated tail**. Reads the uncompressed stream
/// batch-by-batch and stops at the first unreadable batch (a half-written, truncated
/// final message) or a malformed one, keeping everything intact before it; a
/// corrupt/empty header yields no bars. The returned bars are sorted and deduped by
/// close-stamp, ready for [`write_partition`].
///
/// The journal is written append-then-`fsync` per flush, so the only crash mode it
/// can produce is **truncation** (a complete prefix of synced batches plus an
/// interrupted final write), which surfaces as a clean EOF/`Err` the loop drops. It
/// does not defend against arbitrary mid-file byte corruption (a length prefix of
/// garbage could make the underlying Arrow reader over-allocate) — that is not a mode
/// an append-only journal produces, the same honesty as the project's other residuals.
///
/// This is the **lazy-on-load** recovery path: the loader finalizes the recovered
/// bars to a real `.feather` and records their range, so they are never re-downloaded.
/// Recovery is best-effort and total: a missing/empty/torn journal simply yields the
/// (possibly empty) intact prefix, never an error.
pub(crate) fn recover_partial(partial_path: &Path) -> Vec<Bar> {
    let Ok(file) = File::open(partial_path) else {
        return Vec::new();
    };
    let Ok(reader) = StreamReader::try_new(file, None) else {
        return Vec::new(); // unreadable header → nothing recoverable
    };
    // Instrument id travels in the stream's schema metadata (from `schema_with_meta`).
    let Some(instrument) = reader
        .schema()
        .metadata()
        .get("instrument")
        .and_then(|v| v.parse::<u32>().ok())
        .map(InstrumentId::new)
    else {
        return Vec::new();
    };
    let mut bars = Vec::new();
    for batch in reader {
        match batch {
            Ok(b) => {
                if batch_to_bars(instrument, &b, &mut bars).is_err() {
                    break; // malformed batch → keep the intact prefix
                }
            }
            Err(_) => break, // torn tail → keep the intact prefix
        }
    }
    bars.sort_by_key(|b| b.ts.as_nanos());
    bars.dedup_by_key(|b| b.ts.as_nanos());
    bars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_balanced() {
        assert_eq!(FlushPolicy::default(), FlushPolicy::balanced());
        assert_eq!(FlushPolicy::balanced().every_pages, Some(8));
        assert_eq!(FlushPolicy::balanced().every, Some(Duration::from_secs(5)));
    }

    #[test]
    fn every_page_flushes_each_page() {
        let p = FlushPolicy::every_page();
        assert!(p.is_due(1, Duration::ZERO));
        assert!(!p.is_due(0, Duration::MAX)); // no time bound, 0 pages
    }

    #[test]
    fn every_pages_zero_is_clamped_to_one() {
        assert_eq!(FlushPolicy::every_pages(0).every_pages, Some(1));
        assert_eq!(FlushPolicy::every_pages(100).every_pages, Some(100));
    }

    #[test]
    fn is_due_on_either_bound() {
        let p = FlushPolicy::balanced();
        // Page bound only.
        assert!(p.is_due(8, Duration::ZERO));
        assert!(!p.is_due(7, Duration::from_secs(4)));
        // Time bound only.
        assert!(p.is_due(0, Duration::from_secs(5)));
        assert!(p.is_due(2, Duration::from_secs(10)));
    }

    #[test]
    fn time_only_policy_ignores_pages() {
        let p = FlushPolicy::every(Duration::from_secs(30));
        assert!(!p.is_due(1000, Duration::from_secs(29)));
        assert!(p.is_due(0, Duration::from_secs(30)));
    }

    #[test]
    fn both_none_never_flushes_until_finalize() {
        let p = FlushPolicy {
            every_pages: None,
            every: None,
        };
        assert!(!p.is_due(1_000_000, Duration::MAX));
    }

    use akadro_core::{Price, Qty, Timestamp};
    use std::path::PathBuf;

    fn unique_partial(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir()
            .join(format!(
                "akadro_partial_{name}_{}_{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ))
            .with_extension("partial")
    }

    fn bar(ts_min: i64, close: i64) -> Bar {
        Bar::new(
            InstrumentId::new(7),
            Timestamp::from_nanos(ts_min * 60_000_000_000),
            Price::from_raw(close),
            Price::from_raw(close + 5),
            Price::from_raw(close - 5),
            Price::from_raw(close),
            Qty::from_raw(1),
        )
    }

    #[test]
    fn journal_round_trips_through_recover() {
        let partial = unique_partial("roundtrip");
        {
            // every_page flushes each push; drop WITHOUT the loader's finalize →
            // an orphan journal, exactly the interrupted-download case.
            let mut w = PartialWriter::create(
                &partial,
                InstrumentId::new(7),
                2,
                0,
                FlushPolicy::every_page(),
            )
            .unwrap();
            w.push_page(&[bar(1, 100), bar(2, 101)]);
            w.push_page(&[bar(3, 102)]);
        }
        assert!(partial.exists());
        let recovered = recover_partial(&partial);
        assert_eq!(recovered.len(), 3);
        assert_eq!(recovered[0].close.raw(), 100);
        assert_eq!(recovered[2].close.raw(), 102);
        let _ = std::fs::remove_file(&partial);
    }

    #[test]
    fn recover_sorts_and_dedups() {
        let partial = unique_partial("dedup");
        {
            let mut w = PartialWriter::create(
                &partial,
                InstrumentId::new(7),
                2,
                0,
                FlushPolicy::every_page(),
            )
            .unwrap();
            // Out-of-order pages with a duplicate close-stamp across flushes.
            w.push_page(&[bar(3, 102), bar(1, 100)]);
            w.push_page(&[bar(1, 100), bar(2, 101)]); // bar(1) duplicate
        }
        let recovered = recover_partial(&partial);
        let ts: Vec<i64> = recovered.iter().map(|b| b.ts.as_nanos()).collect();
        assert_eq!(
            ts,
            vec![60_000_000_000, 2 * 60_000_000_000, 3 * 60_000_000_000]
        );
        let _ = std::fs::remove_file(&partial);
    }

    #[test]
    fn flush_policy_governs_what_is_journaled() {
        // Balanced (8 pages / 5s): 3 small pages stay buffered and are LOST on drop
        // (no flush fired), so a crash before finalize recovers nothing for them.
        let partial = unique_partial("balanced");
        {
            let mut w = PartialWriter::create(
                &partial,
                InstrumentId::new(7),
                2,
                0,
                FlushPolicy::balanced(),
            )
            .unwrap();
            w.push_page(&[bar(1, 100)]);
            w.push_page(&[bar(2, 101)]);
            w.push_page(&[bar(3, 102)]);
        }
        assert!(recover_partial(&partial).is_empty());
        let _ = std::fs::remove_file(&partial);
    }

    #[test]
    fn recover_tolerates_torn_tail() {
        let partial = unique_partial("torn");
        {
            let mut w = PartialWriter::create(
                &partial,
                InstrumentId::new(7),
                2,
                0,
                FlushPolicy::every_page(),
            )
            .unwrap();
            w.push_page(&[bar(1, 100), bar(2, 101)]); // batch 1 (2 bars)
            w.push_page(&[bar(3, 102)]); // batch 2 (1 bar)
            // drop without finalize → header + batch1 + batch2, no EOS marker.
        }
        // Simulate the realistic crash: the trailing batch's write was interrupted, so
        // the file is truncated mid-message. (An append+fsync journal only ever
        // truncates; it never writes mid-file garbage.) Cut a few bytes off the end to
        // corrupt the last batch's body.
        {
            let len = std::fs::metadata(&partial).unwrap().len();
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(&partial)
                .unwrap();
            f.set_len(len - 5).unwrap();
        }
        // The intact prefix (batch 1) is still recovered; the torn tail is dropped.
        let recovered = recover_partial(&partial);
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[1].close.raw(), 101);
        let _ = std::fs::remove_file(&partial);
    }

    #[test]
    fn recover_missing_or_garbage_is_empty() {
        // Missing file → empty.
        assert!(recover_partial(Path::new("/nonexistent/akadro/x.partial")).is_empty());
        // A garbage file with no valid stream header → empty.
        let partial = unique_partial("garbage");
        std::fs::write(&partial, b"not an arrow stream").unwrap();
        assert!(recover_partial(&partial).is_empty());
        let _ = std::fs::remove_file(&partial);
    }
}
