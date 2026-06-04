// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reading and writing OHLCV bar partitions as Arrow IPC / Feather files.
//!
//! Writes are **atomic** (temp-file + rename), **LZ4-compressed**, and reads
//! validate the cache format version, the 6-column shape, and strictly-ascending
//! timestamps. The `ts` column is an Arrow `Timestamp(Nanosecond, "UTC")` so
//! external dataframe tools (pandas / `Polars` / `DuckDB`) render it as a real
//! datetime. LZ4 decompression is ~free relative to the disk I/O it saves.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use akadro_core::{Bar, DataSource, Event, InstrumentId, Price, Qty, Timestamp};
use arrow_array::{Array, ArrayRef, Int64Array, RecordBatch, TimestampNanosecondArray};
use arrow_ipc::CompressionType;
use arrow_ipc::reader::FileReader;
use arrow_ipc::writer::{FileWriter, IpcWriteOptions};
use arrow_schema::{DataType, Field, Schema, TimeUnit};

const FORMAT_VERSION: &str = "1";

/// Errors from the on-disk cache.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DataError {
    /// Filesystem I/O error. The foreign `std::io::Error` is wrapped rather than
    /// `#[from]`-leaked (workspace error policy, i11); its [`std::io::ErrorKind`] is
    /// preserved so callers can still branch on `NotFound` / `PermissionDenied` etc.
    #[error("io error ({kind:?}): {message}")]
    Io {
        /// The OS error category.
        kind: std::io::ErrorKind,
        /// Human-readable detail.
        message: String,
    },
    /// Arrow read/write error.
    #[error("arrow error: {0}")]
    Arrow(String),
    /// The file's schema or metadata did not match expectations.
    #[error("schema/metadata error: {0}")]
    Schema(String),
    /// A connector aborted a back-fill mid-stream (a transient transport/parse
    /// error), so the downloaded window is *truncated*. Surfaced rather than
    /// silently caching a partial range; the partial progress is journaled, so a
    /// retry resumes from where it stopped. See the cache loader's error handling.
    #[error("fetch incomplete: {0}")]
    Fetch(String),
}

impl From<std::io::Error> for DataError {
    fn from(e: std::io::Error) -> Self {
        // Deliberate wrapping conversion: capture the category + message, but do not
        // store the foreign error type in the public enum (i11).
        DataError::Io {
            kind: e.kind(),
            message: e.to_string(),
        }
    }
}

pub(crate) fn schema_with_meta(
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
) -> Arc<Schema> {
    let mut meta = HashMap::new();
    meta.insert("instrument".to_owned(), instrument.index().to_string());
    meta.insert("price_scale".to_owned(), price_scale.to_string());
    meta.insert("qty_scale".to_owned(), qty_scale.to_string());
    meta.insert(
        "akadro_cache_format_version".to_owned(),
        FORMAT_VERSION.to_owned(),
    );
    Arc::new(
        Schema::new(vec![
            Field::new(
                "ts",
                DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
                false,
            ),
            Field::new("open", DataType::Int64, false),
            Field::new("high", DataType::Int64, false),
            Field::new("low", DataType::Int64, false),
            Field::new("close", DataType::Int64, false),
            Field::new("volume", DataType::Int64, false),
        ])
        .with_metadata(meta),
    )
}

/// Encode `bars` into a 6-column [`RecordBatch`] matching [`schema_with_meta`]'s
/// layout (`ts` as `Timestamp(ns, UTC)`, the rest `Int64` fixed-point raws). Shared
/// by [`write_partition`] (LZ4 file) and the incremental `.partial` stream writer so
/// both encode bars identically (DRY).
pub(crate) fn bars_to_batch(schema: &Arc<Schema>, bars: &[Bar]) -> Result<RecordBatch, DataError> {
    let col = |f: &dyn Fn(&Bar) -> i64| -> ArrayRef {
        Arc::new(Int64Array::from(bars.iter().map(f).collect::<Vec<_>>()))
    };
    let ts_col: ArrayRef = Arc::new(
        TimestampNanosecondArray::from(bars.iter().map(|b| b.ts.as_nanos()).collect::<Vec<_>>())
            .with_timezone("UTC"),
    );
    let columns: Vec<ArrayRef> = vec![
        ts_col,
        col(&|b| b.open.raw()),
        col(&|b| b.high.raw()),
        col(&|b| b.low.raw()),
        col(&|b| b.close.raw()),
        col(&|b| b.volume.raw()),
    ];
    RecordBatch::try_new(schema.clone(), columns).map_err(|e| DataError::Arrow(e.to_string()))
}

/// Decode one [`RecordBatch`] (written by [`bars_to_batch`]) back into `Bar`s for
/// `instrument`, appending to `out`. Validates the 6-column shape and the UTC `ts`
/// annotation, so a structurally-wrong batch surfaces a [`DataError`] instead of
/// panicking. Shared by [`read_partition`] and the `.partial` recovery path (DRY).
pub(crate) fn batch_to_bars(
    instrument: InstrumentId,
    batch: &RecordBatch,
    out: &mut Vec<Bar>,
) -> Result<(), DataError> {
    if batch.num_columns() != 6 {
        return Err(DataError::Schema(format!(
            "expected 6 columns, found {}",
            batch.num_columns()
        )));
    }
    if !matches!(
        batch.schema().field(0).data_type(),
        DataType::Timestamp(TimeUnit::Nanosecond, Some(tz)) if tz.as_ref() == "UTC"
    ) {
        return Err(DataError::Schema(
            "ts column must be Timestamp(Nanosecond, \"UTC\")".to_owned(),
        ));
    }
    let col = |i: usize| -> Result<&Int64Array, DataError> {
        batch
            .column(i)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| DataError::Schema(format!("column {i} is not Int64")))
    };
    let ts = batch
        .column(0)
        .as_any()
        .downcast_ref::<TimestampNanosecondArray>()
        .ok_or_else(|| DataError::Schema("ts column is not Timestamp(Nanosecond)".to_owned()))?;
    let (open, high, low, close, volume) = (col(1)?, col(2)?, col(3)?, col(4)?, col(5)?);
    for r in 0..batch.num_rows() {
        out.push(Bar::new(
            instrument,
            Timestamp::from_nanos(ts.value(r)),
            Price::from_raw(open.value(r)),
            Price::from_raw(high.value(r)),
            Price::from_raw(low.value(r)),
            Price::from_raw(close.value(r)),
            Qty::from_raw(volume.value(r)),
        ));
    }
    Ok(())
}

/// Write `bars` (all for one `instrument`, in ascending timestamp order) to `path`
/// as an LZ4-compressed Feather partition. The instrument and fixed-point scales
/// are stored in the file metadata so a reader reconstructs `Bar`s at the right
/// scale. The write is atomic: it goes to a `.feather.tmp` sibling and is renamed
/// into place on success, so a crash mid-write never leaves a truncated cache file.
///
/// # Errors
/// Returns [`DataError::Arrow`] if Arrow batch/IPC construction or encoding fails,
/// or [`DataError::Io`] if creating the temp file or renaming it into place fails.
pub fn write_partition(
    path: &Path,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
    bars: &[Bar],
) -> Result<(), DataError> {
    let schema = schema_with_meta(instrument, price_scale, qty_scale);
    let batch = bars_to_batch(&schema, bars)?;

    // LZ4-frame compression: the columnar i64 data compresses ~2-4x and decodes at
    // GB/s, so the smaller file is a net I/O win. Readers detect it from the header.
    let options = IpcWriteOptions::default()
        .try_with_compression(Some(CompressionType::LZ4_FRAME))
        .map_err(|e| DataError::Arrow(e.to_string()))?;

    // Atomic publish: write a temp file, fsync via `finish`, then rename into
    // place (atomic on POSIX). A crash mid-write leaves only the `.tmp`, never a
    // truncated cache file that `load_or_cache` would accept as a hit.
    let tmp = path.with_extension("feather.tmp");
    let write = || -> Result<(), DataError> {
        let file = File::create(&tmp)?;
        let mut writer = FileWriter::try_new_with_options(file, &schema, options.clone())
            .map_err(|e| DataError::Arrow(e.to_string()))?;
        writer
            .write(&batch)
            .map_err(|e| DataError::Arrow(e.to_string()))?;
        writer
            .finish()
            .map_err(|e| DataError::Arrow(e.to_string()))?;
        Ok(())
    };
    match write() {
        Ok(()) => {
            std::fs::rename(&tmp, path)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp); // best-effort cleanup
            Err(e)
        }
    }
}

/// Drain a [`DataSource`] and cache its bars for `instrument` to `path`.
///
/// This is the venue-agnostic downloader glue: pass any `DataSource` (e.g.
/// `akadro-venue-mexc`'s `MexcKlineFeed`, which fetches MEXC klines) to
/// fetch-then-cache. Returns the number of bars written and their
/// `(first_ts, last_ts)` nanosecond range (for recording in a [`Manifest`](crate::Manifest)).
///
/// # Errors
/// Returns a [`DataError`] if writing the partition fails.
pub fn cache_from_source<D: DataSource>(
    mut source: D,
    path: &Path,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
) -> Result<(usize, Option<(i64, i64)>), DataError> {
    let mut bars = Vec::new();
    while let Some(event) = source.next_event() {
        if let Event::Bar(bar) = event
            && bar.instrument == instrument
        {
            bars.push(bar);
        }
    }
    let range = match (bars.first(), bars.last()) {
        (Some(f), Some(l)) => Some((f.ts.as_nanos(), l.ts.as_nanos())),
        _ => None,
    };
    write_partition(path, instrument, price_scale, qty_scale, &bars)?;
    Ok((bars.len(), range))
}

/// Load bars from the cached Feather partition at `path` if it exists; otherwise
/// build the source with `make_source`, drain it into the cache, and return the
/// bars. This is the "download once, then replay offline" convenience — e.g.
/// `load_or_cache(path, id, ps, qs, || MexcKlineFeed::new(..).with_range(..))` —
/// so callers never hand-roll the download-and-cache dance. The source is only
/// built (and the network only touched) on a cache miss.
///
/// # Errors
/// Returns a [`DataError`] if reading or writing the partition fails.
pub fn load_or_cache<D, F>(
    path: &Path,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
    make_source: F,
) -> Result<Vec<Bar>, DataError>
where
    D: DataSource,
    F: FnOnce() -> D,
{
    if path.exists() {
        let loaded = read_partition(path)?;
        validate_cache_meta(&loaded, instrument, price_scale, qty_scale, path)?;
        return Ok(loaded.bars);
    }
    cache_from_source(make_source(), path, instrument, price_scale, qty_scale)?;
    Ok(read_partition(path)?.bars)
}

/// Reject a cache hit whose recorded instrument / scales differ from what the
/// caller requested. `Bar` carries no embedded scale, so silently returning
/// wrong-scaled raw integers (e.g. after a venue precision change) would be a
/// 10^Δ price error with no symptom — surface it as a `Schema` error instead so the
/// caller deletes the stale partition (M17).
fn validate_cache_meta(
    loaded: &LoadedBars,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
    path: &Path,
) -> Result<(), DataError> {
    if loaded.instrument != instrument
        || loaded.price_scale != price_scale
        || loaded.qty_scale != qty_scale
    {
        return Err(DataError::Schema(format!(
            "cache at {} holds instrument {:?} scale ({}, {}) but the request is for \
             instrument {:?} scale ({}, {}) — delete the stale partition and re-fetch",
            path.display(),
            loaded.instrument,
            loaded.price_scale,
            loaded.qty_scale,
            instrument,
            price_scale,
            qty_scale,
        )));
    }
    Ok(())
}

/// A partition loaded from disk: the instrument, its scales, and its bars.
#[derive(Debug, Clone)]
pub struct LoadedBars {
    /// Instrument the partition is for.
    pub instrument: InstrumentId,
    /// Price fixed-point scale recorded in the file.
    pub price_scale: u32,
    /// Quantity fixed-point scale recorded in the file.
    pub qty_scale: u32,
    /// The bars, in file (time) order.
    pub bars: Vec<Bar>,
}

/// Identifies one instrument's cached bar partition for [`load_or_cache_many`].
#[derive(Clone, Debug)]
pub struct BarSpec {
    /// Cache file path for this instrument's bars.
    pub path: PathBuf,
    /// Dense instrument id.
    pub instrument: InstrumentId,
    /// Price fixed-point scale.
    pub price_scale: u32,
    /// Quantity fixed-point scale.
    pub qty_scale: u32,
}

/// Load (or download-and-cache) bars for **many** instruments and merge them into
/// one timestamp-ordered `Vec<Bar>` — the cross-sectional / multi-instrument loader.
/// To drive the engine, prefer [`load_or_cache_many_feed`] (returns a ready
/// [`CachedFeed`]); this `Vec`-returning form is for analytics / inspection.
/// `make_source` builds the
/// [`DataSource`] for each [`BarSpec`] (typically a venue kline feed bound to that
/// instrument's symbol). Each instrument is cached independently via
/// [`load_or_cache`], then all bars are merged by event time with a **stable**
/// sort — equal-timestamp bars keep their per-instrument order, so the merged feed
/// is deterministic. Empty `specs` yields an empty vector.
///
/// # Errors
/// The first [`DataError`] from any instrument's load / cache / fetch.
pub fn load_or_cache_many<D, F>(specs: &[BarSpec], make_source: F) -> Result<Vec<Bar>, DataError>
where
    D: DataSource,
    F: Fn(&BarSpec) -> D,
{
    let mut all: Vec<Bar> = Vec::new();
    for spec in specs {
        let bars = load_or_cache(
            &spec.path,
            spec.instrument,
            spec.price_scale,
            spec.qty_scale,
            || make_source(spec),
        )?;
        all.extend(bars);
    }
    all.sort_by_key(|b| b.ts.as_nanos()); // stable merge by event time
    Ok(all)
}

/// [`load_or_cache`] but returns a ready-to-drive [`CachedFeed`] ([`DataSource`])
/// instead of a `Vec<Bar>` — the **blessed cache→engine path** that needs no
/// raw-`Vec` feed constructor. Fetch-if-missing (via `make_source`), cache, then
/// replay; the bars are library-owned end to end (D18).
///
/// # Errors
/// Returns a [`DataError`] if the fetch, cache write, or cache read fails.
pub fn load_or_cache_feed<D, F>(
    path: &Path,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
    make_source: F,
) -> Result<CachedFeed, DataError>
where
    D: DataSource,
    F: FnOnce() -> D,
{
    Ok(CachedFeed::from_cached(load_or_cache(
        path,
        instrument,
        price_scale,
        qty_scale,
        make_source,
    )?))
}

/// [`load_or_cache_many`] but returns a single timestamp-merged [`CachedFeed`]
/// ([`DataSource`]) over all instruments — the multi-instrument blessed cache→engine
/// path (no raw-`Vec` feed constructor needed).
///
/// # Errors
/// Returns a [`DataError`] if any instrument's fetch, cache write, or read fails.
pub fn load_or_cache_many_feed<D, F>(
    specs: &[BarSpec],
    make_source: F,
) -> Result<CachedFeed, DataError>
where
    D: DataSource,
    F: Fn(&BarSpec) -> D,
{
    Ok(CachedFeed::from_cached(load_or_cache_many(
        specs,
        make_source,
    )?))
}

/// Read a Feather partition written by [`write_partition`].
pub fn read_partition(path: &Path) -> Result<LoadedBars, DataError> {
    let file = File::open(path)?;
    let reader = FileReader::try_new(file, None).map_err(|e| DataError::Arrow(e.to_string()))?;

    let meta = reader.schema().metadata().clone();
    let get_u32 = |k: &str| -> Result<u32, DataError> {
        meta.get(k)
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or_else(|| DataError::Schema(format!("missing/invalid metadata key {k:?}")))
    };
    // Reject a partition written by an incompatible (newer/unknown) cache format.
    let version = meta.get("akadro_cache_format_version").map(String::as_str);
    if version != Some(FORMAT_VERSION) {
        return Err(DataError::Schema(format!(
            "unsupported cache format version {version:?} (this build reads {FORMAT_VERSION})"
        )));
    }
    let instrument = InstrumentId::new(get_u32("instrument")?);
    let price_scale = get_u32("price_scale")?;
    let qty_scale = get_u32("qty_scale")?;

    let mut bars = Vec::new();
    for batch in reader {
        // The shape/tz guards (a structurally-valid Arrow file with the wrong column
        // count or a non-UTC `ts`) live in `batch_to_bars`, shared with the `.partial`
        // recovery path — so a bad batch surfaces a `DataError`, never a panic.
        let batch = batch.map_err(|e| DataError::Arrow(e.to_string()))?;
        batch_to_bars(instrument, &batch, &mut bars)?;
    }
    // The engine trusts a DataSource's time ordering; a corrupt/out-of-order
    // partition would silently produce wrong look-backs and fills, so reject it.
    for w in bars.windows(2) {
        if w[0].ts.as_nanos() >= w[1].ts.as_nanos() {
            return Err(DataError::Schema(
                "bars are not in strictly ascending timestamp order".to_owned(),
            ));
        }
    }
    Ok(LoadedBars {
        instrument,
        price_scale,
        qty_scale,
        bars,
    })
}

/// A [`DataSource`] that replays a cached Feather partition into the engine.
#[derive(Debug)]
pub struct FeatherBarSource {
    bars: std::vec::IntoIter<Bar>,
}

impl FeatherBarSource {
    /// Load a partition and prepare to replay it.
    ///
    /// # Errors
    /// Returns a [`DataError`] if the file cannot be read or parsed.
    pub fn open(path: &Path) -> Result<Self, DataError> {
        Ok(FeatherBarSource {
            bars: read_partition(path)?.bars.into_iter(),
        })
    }
}

impl DataSource for FeatherBarSource {
    fn next_event(&mut self) -> Option<Event> {
        self.bars.next().map(Event::Bar)
    }
}

/// A [`DataSource`] over bars that came from the akadro **cache layer** — the
/// blessed cache→engine path. Constructed only by [`load_or_cache_feed`] /
/// [`load_or_cache_many_feed`] (there is **no** public arbitrary-`Vec<Bar>`
/// constructor): the bars are library-owned (fetched through a connector's
/// `Transport` seam and validated on cache read), so feeding them to the engine
/// never goes through a raw-`Vec` injection point (D18 — library owns the data).
#[derive(Debug)]
pub struct CachedFeed {
    bars: std::vec::IntoIter<Bar>,
}

impl CachedFeed {
    /// Crate-private: wrap library-owned cached bars. Deliberately not `pub` — the
    /// only way to obtain a `CachedFeed` is via the `load_or_cache*_feed` loaders,
    /// so user code cannot mint one over data it supplied itself.
    pub(crate) fn from_cached(bars: Vec<Bar>) -> Self {
        CachedFeed {
            bars: bars.into_iter(),
        }
    }
}

impl DataSource for CachedFeed {
    fn next_event(&mut self) -> Option<Event> {
        self.bars.next().map(Event::Bar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bars() -> Vec<Bar> {
        (1..=5)
            .map(|i| {
                let p = Price::from_raw(100 + i);
                Bar::new(
                    InstrumentId::new(3),
                    Timestamp::from_nanos(i * 60_000_000_000),
                    p,
                    Price::from_raw(110 + i),
                    Price::from_raw(90 + i),
                    p,
                    Qty::from_raw(1000 + i),
                )
            })
            .collect()
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        // PID + atomic counter so two tests sharing a `name` never collide on the same
        // on-disk file under parallel `cargo test` (matches manifest.rs/load.rs).
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "akadro_data_test_{name}_{}_{}.feather",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn round_trip_preserves_bars_and_metadata() {
        let path = tmp("roundtrip");
        let bars = sample_bars();
        write_partition(&path, InstrumentId::new(3), 2, 6, &bars).unwrap();

        let loaded = read_partition(&path).unwrap();
        assert_eq!(loaded.instrument, InstrumentId::new(3));
        assert_eq!(loaded.price_scale, 2);
        assert_eq!(loaded.qty_scale, 6);
        assert_eq!(loaded.bars, bars); // exact integer round-trip

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_to_unwritable_path_errors_via_cleanup_branch() {
        // A path under a nonexistent directory makes the temp-file create fail, so
        // write_partition takes the error-cleanup branch and returns Err.
        let path = std::path::Path::new("/nonexistent_akadro_dir_zzz/x.feather");
        let err = write_partition(path, InstrumentId::new(3), 2, 6, &sample_bars()).unwrap_err();
        assert!(matches!(err, DataError::Io { .. }));
    }

    #[test]
    fn read_rejects_out_of_order_timestamps() {
        let path = tmp("oooo");
        // write_partition does not validate order, so we can craft a bad partition.
        let p = Price::from_raw(100);
        let bad = vec![
            Bar::new(
                InstrumentId::new(3),
                Timestamp::from_nanos(20),
                p,
                p,
                p,
                p,
                Qty::from_raw(1),
            ),
            Bar::new(
                InstrumentId::new(3),
                Timestamp::from_nanos(10),
                p,
                p,
                p,
                p,
                Qty::from_raw(1),
            ),
        ];
        write_partition(&path, InstrumentId::new(3), 2, 6, &bad).unwrap();
        let err = read_partition(&path).unwrap_err();
        assert!(
            matches!(err, DataError::Schema(_)),
            "out-of-order -> schema error"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn feather_source_replays_in_order() {
        let path = tmp("source");
        let bars = sample_bars();
        write_partition(&path, InstrumentId::new(3), 2, 6, &bars).unwrap();

        let mut src = FeatherBarSource::open(&path).unwrap();
        let mut seen = Vec::new();
        while let Some(Event::Bar(b)) = src.next_event() {
            seen.push(b);
        }
        assert_eq!(seen, bars);
        assert!(src.next_event().is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_partition_round_trips() {
        let path = tmp("empty");
        write_partition(&path, InstrumentId::new(0), 2, 2, &[]).unwrap();
        let loaded = read_partition(&path).unwrap();
        assert!(loaded.bars.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_or_cache_rejects_scale_mismatch() {
        // M17: a cache hit whose recorded scale differs from the request must error,
        // not silently return wrong-scaled bars.
        struct NeverSource;
        impl DataSource for NeverSource {
            fn next_event(&mut self) -> Option<Event> {
                panic!("source must not be built on a cache hit")
            }
        }
        let path = tmp("scale_mismatch");
        write_partition(&path, InstrumentId::new(3), 2, 6, &sample_bars()).unwrap();
        // Same instrument, but price_scale 4 != the cached 2.
        let err = load_or_cache(&path, InstrumentId::new(3), 4, 6, || NeverSource).unwrap_err();
        assert!(matches!(err, DataError::Schema(_)), "{err:?}");
        // The matching scale still loads fine (and never builds the source).
        let ok = load_or_cache(&path, InstrumentId::new(3), 2, 6, || NeverSource).unwrap();
        assert_eq!(ok, sample_bars());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cache_from_source_drains_and_writes() {
        struct VecSource(std::vec::IntoIter<Event>);
        impl DataSource for VecSource {
            fn next_event(&mut self) -> Option<Event> {
                self.0.next()
            }
        }
        let path = tmp("download");
        let bars = sample_bars();
        // Interleave a non-bar event to confirm it's skipped, not a stop point.
        let mut events: Vec<Event> = bars.iter().copied().map(Event::Bar).collect();
        events.insert(
            2,
            Event::Resync {
                instrument: None,
                ts: Timestamp::from_nanos(1),
            },
        );
        let src = VecSource(events.into_iter());

        let (count, range) = cache_from_source(src, &path, InstrumentId::new(3), 2, 6).unwrap();
        assert_eq!(count, bars.len());
        assert_eq!(
            range,
            Some((bars[0].ts.as_nanos(), bars[bars.len() - 1].ts.as_nanos()))
        );
        assert_eq!(read_partition(&path).unwrap().bars, bars);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_errors() {
        let err = read_partition(Path::new("/nonexistent/akadro/none.feather")).unwrap_err();
        assert!(matches!(err, DataError::Io { .. }));
        assert!(format!("{err}").contains("io"));
    }

    #[test]
    fn cache_from_empty_source_yields_no_range() {
        struct Empty;
        impl DataSource for Empty {
            fn next_event(&mut self) -> Option<Event> {
                None
            }
        }
        let path = tmp("empty_source");
        // A source yielding nothing (or only the wrong instrument) leaves `bars`
        // empty -> the range falls through to `None`.
        let (count, range) = cache_from_source(Empty, &path, InstrumentId::new(0), 2, 6).unwrap();
        assert_eq!(count, 0);
        assert_eq!(range, None);
        // An empty partition still round-trips losslessly.
        assert!(read_partition(&path).unwrap().bars.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_or_cache_downloads_once_then_serves_from_cache() {
        struct VecSrc(std::vec::IntoIter<Event>);
        impl DataSource for VecSrc {
            fn next_event(&mut self) -> Option<Event> {
                self.0.next()
            }
        }
        let path = tmp("load_or_cache");
        let _ = std::fs::remove_file(&path);
        let bars = sample_bars();
        let builds = std::cell::Cell::new(0);
        let make = || {
            builds.set(builds.get() + 1);
            let evs: Vec<Event> = bars.iter().copied().map(Event::Bar).collect();
            VecSrc(evs.into_iter())
        };
        // Cache miss: builds the source, drains it, writes the partition.
        let got = load_or_cache(&path, InstrumentId::new(3), 2, 6, make).unwrap();
        assert_eq!(got, bars);
        assert_eq!(builds.get(), 1);
        // Cache hit: serves from the file; the source factory is NOT called.
        let got2 = load_or_cache(&path, InstrumentId::new(3), 2, 6, || {
            builds.set(builds.get() + 1);
            VecSrc(Vec::new().into_iter())
        })
        .unwrap();
        assert_eq!(got2, bars);
        assert_eq!(builds.get(), 1); // unchanged — no second download
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_or_cache_feed_replays_cached_bars_in_order() {
        // The blessed cache→engine path: a CachedFeed drives the engine directly,
        // with no raw-Vec feed constructor in sight.
        struct VecSrc(std::vec::IntoIter<Event>);
        impl DataSource for VecSrc {
            fn next_event(&mut self) -> Option<Event> {
                self.0.next()
            }
        }
        let path = tmp("load_or_cache_feed");
        let _ = std::fs::remove_file(&path);
        let bars = sample_bars();
        let make = || {
            let evs: Vec<Event> = bars.iter().copied().map(Event::Bar).collect();
            VecSrc(evs.into_iter())
        };
        let mut feed = load_or_cache_feed(&path, InstrumentId::new(3), 2, 6, make).unwrap();
        let mut drained = Vec::new();
        while let Some(Event::Bar(b)) = feed.next_event() {
            drained.push(b);
        }
        assert_eq!(
            drained, bars,
            "CachedFeed replays exactly the cached bars in order"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_or_cache_many_merges_instruments_by_timestamp() {
        struct VecSrc(std::vec::IntoIter<Event>);
        impl DataSource for VecSrc {
            fn next_event(&mut self) -> Option<Event> {
                self.0.next()
            }
        }
        let mk = |inst: u32, ts: i64| {
            Bar::new(
                InstrumentId::new(inst),
                Timestamp::from_nanos(ts),
                Price::from_raw(1),
                Price::from_raw(1),
                Price::from_raw(1),
                Price::from_raw(1),
                Qty::from_raw(1),
            )
        };
        let (p0, p1) = (tmp("many_0"), tmp("many_1"));
        let _ = std::fs::remove_file(&p0);
        let _ = std::fs::remove_file(&p1);
        let specs = vec![
            BarSpec {
                path: p0.clone(),
                instrument: InstrumentId::new(0),
                price_scale: 0,
                qty_scale: 0,
            },
            BarSpec {
                path: p1.clone(),
                instrument: InstrumentId::new(1),
                price_scale: 0,
                qty_scale: 0,
            },
        ];
        // i0: bars at ts 10, 30; i1: bars at ts 20, 40 — interleave on merge.
        let merged = load_or_cache_many(&specs, |spec| {
            let bars = if spec.instrument.index() == 0 {
                vec![mk(0, 10), mk(0, 30)]
            } else {
                vec![mk(1, 20), mk(1, 40)]
            };
            VecSrc(
                bars.into_iter()
                    .map(Event::Bar)
                    .collect::<Vec<_>>()
                    .into_iter(),
            )
        })
        .unwrap();
        let ts: Vec<i64> = merged.iter().map(|b| b.ts.as_nanos()).collect();
        assert_eq!(ts, vec![10, 20, 30, 40]); // merged in event-time order

        // Empty specs → empty.
        let empty = load_or_cache_many(&[], |_| VecSrc(Vec::new().into_iter())).unwrap();
        assert!(empty.is_empty());
        let _ = std::fs::remove_file(&p0);
        let _ = std::fs::remove_file(&p1);
    }

    #[test]
    fn load_or_cache_many_feed_merges_into_one_feed() {
        struct VecSrc(std::vec::IntoIter<Event>);
        impl DataSource for VecSrc {
            fn next_event(&mut self) -> Option<Event> {
                self.0.next()
            }
        }
        let mk_bar = |inst: u32, ts: i64| {
            Bar::new(
                InstrumentId::new(inst),
                Timestamp::from_nanos(ts),
                Price::from_raw(1),
                Price::from_raw(1),
                Price::from_raw(1),
                Price::from_raw(1),
                Qty::from_raw(1),
            )
        };
        let (p0, p1) = (tmp("manyfeed_0"), tmp("manyfeed_1"));
        let _ = std::fs::remove_file(&p0);
        let _ = std::fs::remove_file(&p1);
        let specs = vec![
            BarSpec {
                path: p0.clone(),
                instrument: InstrumentId::new(0),
                price_scale: 0,
                qty_scale: 0,
            },
            BarSpec {
                path: p1.clone(),
                instrument: InstrumentId::new(1),
                price_scale: 0,
                qty_scale: 0,
            },
        ];
        let mut feed = load_or_cache_many_feed(&specs, |s| {
            let bars = if s.instrument == InstrumentId::new(0) {
                vec![mk_bar(0, 10), mk_bar(0, 30)]
            } else {
                vec![mk_bar(1, 20), mk_bar(1, 40)]
            };
            VecSrc(
                bars.into_iter()
                    .map(Event::Bar)
                    .collect::<Vec<_>>()
                    .into_iter(),
            )
        })
        .unwrap();
        let mut ts = Vec::new();
        while let Some(Event::Bar(b)) = feed.next_event() {
            ts.push(b.ts.as_nanos());
        }
        assert_eq!(
            ts,
            vec![10, 20, 30, 40],
            "CachedFeed replays the merged stream in event-time order"
        );
        let _ = std::fs::remove_file(&p0);
        let _ = std::fs::remove_file(&p1);
    }
}
