// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cache manifest: which time ranges are already on disk, and what to fetch next.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::bars::DataError;

/// The current on-disk manifest schema version. A loaded file without a `version`
/// field is treated as legacy v1 and migrated to this on [`Manifest::load`].
const MANIFEST_VERSION: u32 = 2;

/// Serde default for a `version` field absent from a legacy (v1) manifest.
fn legacy_version() -> u32 {
    1
}

/// What a cached chunk's range represents: actual bars, or a span the venue was
/// asked for and **verified to hold no data** (e.g. before the venue's retention
/// horizon). An `EmptyVerified` span has no partition file but still counts as
/// *covered* for gap detection, so it is fetched **once** and never re-attempted —
/// without it, a gap the venue simply has no data for would be re-downloaded on
/// every load forever.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Coverage {
    /// The range holds actual cached bars in the parallel partition file.
    Bars,
    /// The range was fetched, returned empty, and is recorded as covered-but-empty
    /// so it is never re-fetched. No partition file backs it (empty string).
    EmptyVerified,
}

/// One cached `(venue, symbol, interval)` series.
///
/// `partition_files`, `cached_ranges`, and `coverage` are **parallel**: index `i`
/// describes one chunk — its file (`""` when `coverage[i] == EmptyVerified`), its
/// inclusive close-stamp range, and what that range holds. They are kept sorted by
/// range start by [`Manifest::record_range`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Venue name (e.g. `"mexc"`).
    pub venue: String,
    /// Venue symbol (e.g. `"BTCUSDT"`).
    pub symbol: String,
    /// Bar interval (e.g. `"1m"`).
    pub interval: String,
    /// Partition files holding this series (`""` for an `EmptyVerified` chunk).
    pub partition_files: Vec<String>,
    /// Cached `[start_ts_nanos, end_ts_nanos]` ranges (inclusive of close times).
    pub cached_ranges: Vec<(i64, i64)>,
    /// What each parallel chunk holds (bars vs verified-empty). Defaults to all
    /// [`Coverage::Bars`] when loading a legacy v1 manifest that predates the field.
    #[serde(default)]
    pub coverage: Vec<Coverage>,
    /// Close timestamp (nanos) of the most recent cached bar.
    pub last_close_ts_nanos: i64,
}

/// A JSON manifest of everything in the cache directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// On-disk schema version (currently 2). A legacy file without this
    /// field loads as `1` and is migrated up on [`Manifest::load`].
    #[serde(default = "legacy_version")]
    pub version: u32,
    /// All cached series.
    pub entries: Vec<CacheEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            entries: Vec::new(),
        }
    }
}

impl Manifest {
    /// Load a manifest from `path`, or a fresh empty one if it does not exist.
    ///
    /// # Errors
    /// Returns a [`DataError`] if the file exists but cannot be read/parsed.
    pub fn load(path: &Path) -> Result<Self, DataError> {
        if !path.exists() {
            return Ok(Manifest::default());
        }
        let text = std::fs::read_to_string(path)?;
        let mut m: Manifest =
            serde_json::from_str(&text).map_err(|e| DataError::Schema(e.to_string()))?;
        m.migrate();
        Ok(m)
    }

    /// Migrate an in-memory manifest to the current schema version: pad each entry's
    /// `coverage` to match its `cached_ranges` (a legacy v1 entry has none → every
    /// existing chunk is [`Coverage::Bars`]), then stamp the current version. Called
    /// by [`Manifest::load`]; idempotent on an already-current manifest.
    fn migrate(&mut self) {
        for e in &mut self.entries {
            if e.coverage.len() < e.cached_ranges.len() {
                e.coverage.resize(e.cached_ranges.len(), Coverage::Bars);
            }
        }
        self.version = MANIFEST_VERSION;
    }

    /// Write the manifest to `path` (pretty JSON).
    ///
    /// The write is **atomic**: the JSON is written to a sibling temp file and then
    /// renamed over `path`, so a crash mid-write can never leave a truncated,
    /// unparseable manifest that would fail every subsequent [`load`](Self::load)
    /// and lose incremental-fetch state (M16) — mirroring the bars writer.
    ///
    /// # Errors
    /// Returns a [`DataError`] on serialization or I/O failure.
    pub fn save(&self, path: &Path) -> Result<(), DataError> {
        let text =
            serde_json::to_string_pretty(self).map_err(|e| DataError::Schema(e.to_string()))?;
        // Temp file in the SAME directory so the rename is a same-filesystem atomic
        // replace (a temp in /tmp could be a cross-device rename that is not atomic).
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The cached entry for a series, if any.
    #[must_use]
    pub fn entry(&self, venue: &str, symbol: &str, interval: &str) -> Option<&CacheEntry> {
        self.entries
            .iter()
            .find(|e| e.venue == venue && e.symbol == symbol && e.interval == interval)
    }

    /// Plan the next incremental fetch window in epoch-**milliseconds**
    /// `(start_ms, end_ms)` for `now_ms`, or `None` if the cache is already up to
    /// date. With no cached entry, fetches from the beginning (`start_ms = 0`,
    /// the "venue earliest available" sentinel).
    #[must_use]
    pub fn plan_fetch(
        &self,
        venue: &str,
        symbol: &str,
        interval: &str,
        now_ms: i64,
    ) -> Option<(i64, i64)> {
        self.plan_fetch_from(venue, symbol, interval, now_ms, None)
    }

    /// As [`Manifest::plan_fetch`], but on a cold start (no cached entry) fetch
    /// from `earliest_start_ms` instead of `0`. Callers that know the venue's
    /// retention depth (e.g. MEXC keeps 5m bars ~weeks) pass that floor so a cold
    /// fetch does not request `[0, now)` and waste calls on missing history.
    #[must_use]
    pub fn plan_fetch_from(
        &self,
        venue: &str,
        symbol: &str,
        interval: &str,
        now_ms: i64,
        earliest_start_ms: Option<i64>,
    ) -> Option<(i64, i64)> {
        let start_ms = match self.entry(venue, symbol, interval) {
            None => earliest_start_ms.unwrap_or(0),
            Some(e) => e.last_close_ts_nanos / 1_000_000 + 1,
        };
        if start_ms >= now_ms {
            None
        } else {
            Some((start_ms, now_ms))
        }
    }

    /// Record (or update) a series after caching a partition.
    ///
    /// # Errors
    /// Returns [`DataError::Schema`] if `range` is reversed (`range.1 < range.0`),
    /// or if it overlaps the series' existing cached history
    /// (`range.0 <= last_close_ts_nanos`) — appending an overlapping partition would
    /// duplicate bars on read.
    pub fn record(
        &mut self,
        venue: &str,
        symbol: &str,
        interval: &str,
        partition_file: &str,
        range: (i64, i64),
    ) -> Result<(), DataError> {
        if range.1 < range.0 {
            return Err(DataError::Schema(format!(
                "partition range {range:?} is reversed (end < start)"
            )));
        }
        if let Some(e) = self
            .entries
            .iter_mut()
            .find(|e| e.venue == venue && e.symbol == symbol && e.interval == interval)
        {
            if range.0 <= e.last_close_ts_nanos {
                return Err(DataError::Schema(format!(
                    "partition range {range:?} overlaps cached history ending at {}",
                    e.last_close_ts_nanos
                )));
            }
            e.partition_files.push(partition_file.to_owned());
            e.cached_ranges.push(range);
            e.coverage.push(Coverage::Bars);
            e.last_close_ts_nanos = e.last_close_ts_nanos.max(range.1);
        } else {
            self.entries.push(CacheEntry {
                venue: venue.to_owned(),
                symbol: symbol.to_owned(),
                interval: interval.to_owned(),
                partition_files: vec![partition_file.to_owned()],
                cached_ranges: vec![range],
                coverage: vec![Coverage::Bars],
                last_close_ts_nanos: range.1,
            });
        }
        Ok(())
    }

    /// Record a cached chunk at **any** position in the series' history (sorted
    /// insert), allowing an *earlier-history* back-fill that the forward-only
    /// [`record`](Self::record) rejects. This is the range-aware cache's writer: the
    /// chunk's `(file, range, coverage)` is inserted keeping `cached_ranges` sorted
    /// by start. Adjacent / boundary-touching ranges are allowed (read-time
    /// `dedup_by_key` collapses a shared boundary bar, and [`missing_gaps`] coalesces
    /// for gap detection); only a **true interior overlap** is rejected.
    ///
    /// For an [`Coverage::EmptyVerified`] chunk (a span the venue has no data for),
    /// pass `partition_file = ""` — it records coverage without a file so the span is
    /// never re-fetched.
    ///
    /// [`missing_gaps`]: crate::missing_gaps
    ///
    /// # Errors
    /// Returns [`DataError::Schema`] if `range` is reversed (`range.1 < range.0`) or
    /// truly overlaps an existing chunk's interior.
    pub fn record_range(
        &mut self,
        venue: &str,
        symbol: &str,
        interval: &str,
        partition_file: &str,
        range: (i64, i64),
        coverage: Coverage,
    ) -> Result<(), DataError> {
        if range.1 < range.0 {
            return Err(DataError::Schema(format!(
                "chunk range {range:?} is reversed (end < start)"
            )));
        }
        let existing = self
            .entries
            .iter()
            .position(|e| e.venue == venue && e.symbol == symbol && e.interval == interval);
        let idx = if let Some(i) = existing {
            i
        } else {
            self.entries.push(CacheEntry {
                venue: venue.to_owned(),
                symbol: symbol.to_owned(),
                interval: interval.to_owned(),
                partition_files: Vec::new(),
                cached_ranges: Vec::new(),
                coverage: Vec::new(),
                last_close_ts_nanos: i64::MIN,
            });
            self.entries.len() - 1
        };
        let entry = &mut self.entries[idx];
        // Reject only a true interior overlap; a shared boundary point (`c == b`) or
        // adjacency is fine.
        for &(elo, ehi) in &entry.cached_ranges {
            if range.0.max(elo) < range.1.min(ehi) {
                return Err(DataError::Schema(format!(
                    "chunk range {range:?} interior-overlaps cached ({elo}, {ehi})"
                )));
            }
        }
        let at = entry.cached_ranges.partition_point(|&(lo, _)| lo < range.0);
        entry.cached_ranges.insert(at, range);
        entry.partition_files.insert(at, partition_file.to_owned());
        entry.coverage.insert(at, coverage);
        entry.last_close_ts_nanos = entry.last_close_ts_nanos.max(range.1);
        Ok(())
    }

    /// Validate that every series' cached ranges are sorted and non-overlapping
    /// (useful after [`Manifest::load`] of a hand-edited file).
    ///
    /// # Errors
    /// Returns [`DataError::Schema`] on the first out-of-order or overlapping pair.
    pub fn validate(&self) -> Result<(), DataError> {
        for e in &self.entries {
            // Parallel arrays must stay length-aligned (chunk i = file/range/coverage).
            if e.coverage.len() != e.cached_ranges.len()
                || e.partition_files.len() != e.cached_ranges.len()
            {
                return Err(DataError::Schema(format!(
                    "cache entry {}/{}/{} has misaligned parallel arrays: {} files, {} ranges, {} coverage",
                    e.venue,
                    e.symbol,
                    e.interval,
                    e.partition_files.len(),
                    e.cached_ranges.len(),
                    e.coverage.len()
                )));
            }
            for w in e.cached_ranges.windows(2) {
                // Strict `<`: a shared boundary point is allowed (read-time dedup
                // collapses it); only a true interior overlap or out-of-order pair is
                // rejected.
                if w[1].0 < w[0].1 {
                    return Err(DataError::Schema(format!(
                        "cached ranges for {}/{}/{} overlap or are out of order: {:?} then {:?}",
                        e.venue, e.symbol, e.interval, w[0], w[1]
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_fetch_from_empty_then_incremental() {
        let mut m = Manifest::default();
        // Nothing cached -> fetch from the beginning.
        assert_eq!(m.plan_fetch("mexc", "BTCUSDT", "1m", 1000), Some((0, 1000)));

        // Cache up to ts 600_000_000 ns (= 600 ms).
        m.record("mexc", "BTCUSDT", "1m", "2026-05.feather", (0, 600_000_000))
            .unwrap();
        // Next fetch starts at 601 ms.
        assert_eq!(
            m.plan_fetch("mexc", "BTCUSDT", "1m", 1000),
            Some((601, 1000))
        );
        // Already up to date.
        assert_eq!(m.plan_fetch("mexc", "BTCUSDT", "1m", 500), None);
        // A different symbol is independent.
        assert_eq!(m.plan_fetch("mexc", "ETHUSDT", "1m", 1000), Some((0, 1000)));
        // Cold-start floor: a fresh symbol fetches from the supplied earliest.
        assert_eq!(
            m.plan_fetch_from("mexc", "SOLUSDT", "1m", 1000, Some(300)),
            Some((300, 1000))
        );
    }

    #[test]
    fn record_appends_and_advances_last_ts() {
        let mut m = Manifest::default();
        m.record("mexc", "BTCUSDT", "1m", "a.feather", (0, 100))
            .unwrap();
        m.record("mexc", "BTCUSDT", "1m", "b.feather", (101, 200))
            .unwrap();
        let e = m.entry("mexc", "BTCUSDT", "1m").unwrap();
        assert_eq!(e.partition_files, vec!["a.feather", "b.feather"]);
        assert_eq!(e.cached_ranges.len(), 2);
        assert_eq!(e.last_close_ts_nanos, 200);
    }

    #[test]
    fn record_rejects_overlap_and_validate_catches_it() {
        let mut m = Manifest::default();
        m.record("mexc", "BTCUSDT", "1m", "a.feather", (0, 100))
            .unwrap();
        // Overlapping range (starts at/before the cached end) is rejected.
        assert!(
            m.record("mexc", "BTCUSDT", "1m", "b.feather", (100, 200))
                .is_err()
        );
        // The accepted history is still valid.
        m.validate().unwrap();

        // A hand-built entry with overlapping ranges (bypassing `record`) is
        // caught by `validate`.
        let bad = Manifest {
            version: MANIFEST_VERSION,
            entries: vec![CacheEntry {
                venue: "mexc".to_owned(),
                symbol: "ETHUSDT".to_owned(),
                interval: "1m".to_owned(),
                partition_files: vec!["a".to_owned(), "b".to_owned()],
                cached_ranges: vec![(0, 100), (50, 200)], // overlap
                coverage: vec![Coverage::Bars, Coverage::Bars],
                last_close_ts_nanos: 200,
            }],
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn record_rejects_reversed_range() {
        // m42: a range whose end precedes its start is malformed input; reject it
        // fail-fast rather than record a nonsensical span (which would also corrupt
        // last_close_ts_nanos).
        let mut m = Manifest::default();
        assert!(
            m.record("mexc", "BTCUSDT", "1m", "a.feather", (200, 100))
                .is_err(),
            "a reversed (end < start) range must be rejected"
        );
        // A degenerate single-point range (start == end) is allowed.
        m.record("mexc", "BTCUSDT", "1m", "a.feather", (100, 100))
            .unwrap();
    }

    #[test]
    fn save_load_round_trip() {
        let path = std::env::temp_dir().join("akadro_data_manifest_test.json");
        let mut m = Manifest::default();
        m.record("mexc", "BTCUSDT", "1m", "x.feather", (0, 999))
            .unwrap();
        m.save(&path).unwrap();
        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(
            loaded
                .entry("mexc", "BTCUSDT", "1m")
                .unwrap()
                .last_close_ts_nanos,
            999
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_is_empty() {
        let m = Manifest::load(Path::new("/nonexistent/akadro/manifest.json")).unwrap();
        assert!(m.entries.is_empty());
        assert_eq!(m.version, MANIFEST_VERSION); // a fresh manifest is current-version
    }

    fn unique_tmp(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "akadro_manifest_{name}_{}_{}.json",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn v1_manifest_migrates_to_v2_with_coverage() {
        // A legacy v1 file: no `version`, no `coverage` on the entry.
        let legacy = r#"{
            "entries": [{
                "venue": "mexc", "symbol": "BTCUSDT", "interval": "1m",
                "partition_files": ["a.feather", "b.feather"],
                "cached_ranges": [[0, 100], [101, 200]],
                "last_close_ts_nanos": 200
            }]
        }"#;
        let path = unique_tmp("v1");
        std::fs::write(&path, legacy).unwrap();
        let m = Manifest::load(&path).unwrap();
        assert_eq!(m.version, MANIFEST_VERSION);
        let e = m.entry("mexc", "BTCUSDT", "1m").unwrap();
        // coverage padded to match the two ranges, all Bars.
        assert_eq!(e.coverage, vec![Coverage::Bars, Coverage::Bars]);
        m.validate().unwrap();

        // Re-save then re-load is identical (now persists version + coverage).
        m.save(&path).unwrap();
        let again = Manifest::load(&path).unwrap();
        assert_eq!(again.version, MANIFEST_VERSION);
        assert_eq!(
            again.entry("mexc", "BTCUSDT", "1m").unwrap().coverage,
            vec![Coverage::Bars, Coverage::Bars]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_range_accepts_earlier_history_backfill() {
        let mut m = Manifest::default();
        // Forward record caches the recent window.
        m.record("okx", "BTC-USDT-SWAP", "1m", "recent.feather", (1000, 2000))
            .unwrap();
        // The old forward record() rejects an earlier range...
        assert!(
            m.record("okx", "BTC-USDT-SWAP", "1m", "old.feather", (0, 500))
                .is_err()
        );
        // ...but record_range accepts it and keeps cached_ranges sorted by start.
        m.record_range(
            "okx",
            "BTC-USDT-SWAP",
            "1m",
            "old.feather",
            (0, 500),
            Coverage::Bars,
        )
        .unwrap();
        let e = m.entry("okx", "BTC-USDT-SWAP", "1m").unwrap();
        assert_eq!(e.cached_ranges, vec![(0, 500), (1000, 2000)]);
        assert_eq!(e.partition_files, vec!["old.feather", "recent.feather"]);
        assert_eq!(e.coverage, vec![Coverage::Bars, Coverage::Bars]);
        m.validate().unwrap();
    }

    #[test]
    fn record_range_allows_boundary_touch_rejects_interior_overlap() {
        let mut m = Manifest::default();
        m.record_range(
            "bybit",
            "BTCUSDT",
            "5m",
            "a.feather",
            (0, 100),
            Coverage::Bars,
        )
        .unwrap();
        // Boundary-touching range (shares the endpoint 100) is allowed — read-time
        // dedup collapses the shared bar.
        m.record_range(
            "bybit",
            "BTCUSDT",
            "5m",
            "b.feather",
            (100, 200),
            Coverage::Bars,
        )
        .unwrap();
        m.validate().unwrap();
        // A true interior overlap is rejected.
        assert!(
            m.record_range(
                "bybit",
                "BTCUSDT",
                "5m",
                "c.feather",
                (50, 150),
                Coverage::Bars
            )
            .is_err()
        );
        // A reversed range is rejected.
        assert!(
            m.record_range(
                "bybit",
                "BTCUSDT",
                "5m",
                "d.feather",
                (300, 250),
                Coverage::Bars
            )
            .is_err()
        );
    }

    #[test]
    fn empty_verified_chunk_persists_and_round_trips() {
        let mut m = Manifest::default();
        // A span the venue has no data for: no file, EmptyVerified coverage.
        m.record_range(
            "kucoin",
            "BTC-USDT",
            "1m",
            "",
            (0, 1000),
            Coverage::EmptyVerified,
        )
        .unwrap();
        m.record_range(
            "kucoin",
            "BTC-USDT",
            "1m",
            "bars.feather",
            (1001, 2000),
            Coverage::Bars,
        )
        .unwrap();
        let e = m.entry("kucoin", "BTC-USDT", "1m").unwrap();
        assert_eq!(e.coverage, vec![Coverage::EmptyVerified, Coverage::Bars]);
        assert_eq!(e.partition_files, vec!["", "bars.feather"]);

        let path = unique_tmp("empty_verified");
        m.save(&path).unwrap();
        let loaded = Manifest::load(&path).unwrap();
        let le = loaded.entry("kucoin", "BTC-USDT", "1m").unwrap();
        assert_eq!(le.coverage, vec![Coverage::EmptyVerified, Coverage::Bars]);
        loaded.validate().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
