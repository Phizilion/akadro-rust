// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cache manifest: which time ranges are already on disk, and what to fetch next.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::bars::DataError;

/// One cached `(venue, symbol, interval)` series.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Venue name (e.g. `"mexc"`).
    pub venue: String,
    /// Venue symbol (e.g. `"BTCUSDT"`).
    pub symbol: String,
    /// Bar interval (e.g. `"1m"`).
    pub interval: String,
    /// Partition files holding this series, in time order.
    pub partition_files: Vec<String>,
    /// Cached `[start_ts_nanos, end_ts_nanos]` ranges (inclusive of close times).
    pub cached_ranges: Vec<(i64, i64)>,
    /// Close timestamp (nanos) of the most recent cached bar.
    pub last_close_ts_nanos: i64,
}

/// A JSON manifest of everything in the cache directory.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    /// All cached series.
    pub entries: Vec<CacheEntry>,
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
        serde_json::from_str(&text).map_err(|e| DataError::Schema(e.to_string()))
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
    /// Returns [`DataError::Schema`] if `range` overlaps the series' existing
    /// cached history (`range.0 <= last_close_ts_nanos`) — appending an
    /// overlapping partition would duplicate bars on read.
    pub fn record(
        &mut self,
        venue: &str,
        symbol: &str,
        interval: &str,
        partition_file: &str,
        range: (i64, i64),
    ) -> Result<(), DataError> {
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
            e.last_close_ts_nanos = e.last_close_ts_nanos.max(range.1);
        } else {
            self.entries.push(CacheEntry {
                venue: venue.to_owned(),
                symbol: symbol.to_owned(),
                interval: interval.to_owned(),
                partition_files: vec![partition_file.to_owned()],
                cached_ranges: vec![range],
                last_close_ts_nanos: range.1,
            });
        }
        Ok(())
    }

    /// Validate that every series' cached ranges are sorted and non-overlapping
    /// (useful after [`Manifest::load`] of a hand-edited file).
    ///
    /// # Errors
    /// Returns [`DataError::Schema`] on the first out-of-order or overlapping pair.
    pub fn validate(&self) -> Result<(), DataError> {
        for e in &self.entries {
            for w in e.cached_ranges.windows(2) {
                if w[1].0 <= w[0].1 {
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
            entries: vec![CacheEntry {
                venue: "mexc".to_owned(),
                symbol: "ETHUSDT".to_owned(),
                interval: "1m".to_owned(),
                partition_files: vec!["a".to_owned(), "b".to_owned()],
                cached_ranges: vec![(0, 100), (50, 200)], // overlap
                last_close_ts_nanos: 200,
            }],
        };
        assert!(bad.validate().is_err());
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
    }
}
