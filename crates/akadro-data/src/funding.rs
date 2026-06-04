// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Funding-rate history cache — the perpetual-funding analog of the [bar
//! cache](crate::load_or_cache).
//!
//! Funding is a **real recurring cost** of holding a perpetual position (it can
//! dominate `PnL`), so akadro caches it as a first-class artifact with the *same*
//! discipline as bars — an atomic, version-stamped, ascending-validated Arrow
//! partition — and the same download call that caches klines also caches funding
//! (see [`load_or_cache_perp`](crate::load_or_cache_perp)). The user downloads once
//! and re-runs from cache; they never re-fetch funding per backtest.
//!
//! The partition is two `Int64` columns — `ts` (settlement time, an Arrow
//! `Timestamp(Nanosecond, "UTC")`) and `rate` (the funding rate in raw fixed-point
//! at [`akadro_core::FUNDING_RATE_SCALE`]) — matching the `(Timestamp, i64)`
//! settlements `SimulatedExchange::with_funding_schedule` consumes.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use akadro_core::{InstrumentId, Timestamp};
use arrow_array::{Array, ArrayRef, Int64Array, RecordBatch, TimestampNanosecondArray};
use arrow_ipc::CompressionType;
use arrow_ipc::reader::FileReader;
use arrow_ipc::writer::{FileWriter, IpcWriteOptions};
use arrow_schema::{DataType, Field, Schema, TimeUnit};

use crate::bars::DataError;

/// On-disk format version for funding partitions (independent of the bar cache's).
const FUNDING_FORMAT_VERSION: &str = "1";

fn funding_schema(instrument: InstrumentId) -> Arc<Schema> {
    let mut meta = HashMap::new();
    meta.insert("instrument".to_owned(), instrument.index().to_string());
    meta.insert(
        "akadro_funding_format_version".to_owned(),
        FUNDING_FORMAT_VERSION.to_owned(),
    );
    Arc::new(
        Schema::new(vec![
            Field::new(
                "ts",
                DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
                false,
            ),
            Field::new("rate", DataType::Int64, false),
        ])
        .with_metadata(meta),
    )
}

/// Write a funding-rate `schedule` (`(settlement_ts, raw_rate)` at
/// `FUNDING_RATE_SCALE`, ascending) for `instrument` to `path` as an LZ4-compressed
/// Feather partition. Atomic: a `.feather.tmp` sibling is renamed into place on
/// success, so a crash mid-write never leaves a truncated cache file.
///
/// # Errors
/// [`DataError::Arrow`] on Arrow batch/IPC failure, or [`DataError::Io`] on a
/// filesystem error.
pub fn write_funding_partition(
    path: &Path,
    instrument: InstrumentId,
    schedule: &[(Timestamp, i64)],
) -> Result<(), DataError> {
    let schema = funding_schema(instrument);
    let ts: ArrayRef = Arc::new(
        TimestampNanosecondArray::from(
            schedule
                .iter()
                .map(|(t, _)| t.as_nanos())
                .collect::<Vec<_>>(),
        )
        .with_timezone("UTC"),
    );
    let rate: ArrayRef = Arc::new(Int64Array::from(
        schedule.iter().map(|(_, r)| *r).collect::<Vec<_>>(),
    ));
    let batch = RecordBatch::try_new(schema.clone(), vec![ts, rate])
        .map_err(|e| DataError::Arrow(e.to_string()))?;
    let options = IpcWriteOptions::default()
        .try_with_compression(Some(CompressionType::LZ4_FRAME))
        .map_err(|e| DataError::Arrow(e.to_string()))?;

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
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Read a funding partition written by [`write_funding_partition`] into an ascending
/// `(settlement_ts, raw_rate)` schedule.
///
/// # Errors
/// [`DataError::Schema`] on a wrong/unknown format version, a column-shape mismatch,
/// a tz-naive `ts` column, or non-ascending timestamps; [`DataError::Arrow`]/`Io`
/// on read failure.
pub fn read_funding_partition(path: &Path) -> Result<Vec<(Timestamp, i64)>, DataError> {
    let file = File::open(path)?;
    let reader = FileReader::try_new(file, None).map_err(|e| DataError::Arrow(e.to_string()))?;
    let meta = reader.schema().metadata().clone();
    let version = meta
        .get("akadro_funding_format_version")
        .map(String::as_str);
    if version != Some(FUNDING_FORMAT_VERSION) {
        return Err(DataError::Schema(format!(
            "unsupported funding cache version {version:?} (this build reads {FUNDING_FORMAT_VERSION})"
        )));
    }
    let mut out: Vec<(Timestamp, i64)> = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| DataError::Arrow(e.to_string()))?;
        if batch.num_columns() != 2 {
            return Err(DataError::Schema(format!(
                "expected 2 funding columns, found {}",
                batch.num_columns()
            )));
        }
        if !matches!(
            batch.schema().field(0).data_type(),
            DataType::Timestamp(TimeUnit::Nanosecond, Some(tz)) if tz.as_ref() == "UTC"
        ) {
            return Err(DataError::Schema(
                "funding ts column must be Timestamp(Nanosecond, \"UTC\")".to_owned(),
            ));
        }
        let ts = batch
            .column(0)
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .ok_or_else(|| DataError::Schema("funding ts column is not Timestamp".to_owned()))?;
        let rate = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| DataError::Schema("funding rate column is not Int64".to_owned()))?;
        for r in 0..batch.num_rows() {
            out.push((Timestamp::from_nanos(ts.value(r)), rate.value(r)));
        }
    }
    // The exchange applies funding by walking settlements in order; reject a corrupt
    // partition rather than silently accrue funding at the wrong times.
    for w in out.windows(2) {
        if w[0].0.as_nanos() >= w[1].0.as_nanos() {
            return Err(DataError::Schema(
                "funding settlements are not in strictly ascending timestamp order".to_owned(),
            ));
        }
    }
    Ok(out)
}

/// Load a funding schedule from the cached partition at `path` if it exists;
/// otherwise build it with `fetch` (a library-owned connector call, e.g.
/// `okx::fetch_funding_history_paged`), cache it, and return it. The funding analog
/// of [`load_or_cache`](crate::load_or_cache): download once, then replay offline —
/// so a perpetual backtest never re-fetches funding per run.
///
/// # Errors
/// Propagates whatever `fetch` returns as a [`DataError`], or a read/write error.
pub fn load_or_cache_funding<F>(
    path: &Path,
    instrument: InstrumentId,
    fetch: F,
) -> Result<Vec<(Timestamp, i64)>, DataError>
where
    F: FnOnce() -> Result<Vec<(Timestamp, i64)>, DataError>,
{
    if path.exists() {
        return read_funding_partition(path);
    }
    let schedule = fetch()?;
    write_funding_partition(path, instrument, &schedule)?;
    Ok(schedule)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "akadro_funding_test_{name}_{}.feather",
            std::process::id()
        ))
    }
    fn pt(ts: i64, rate: i64) -> (Timestamp, i64) {
        (Timestamp::from_nanos(ts), rate)
    }

    #[test]
    fn round_trips_a_schedule() {
        let path = tmp("roundtrip");
        let _ = std::fs::remove_file(&path);
        let sched = vec![pt(1_000, 10_000), pt(2_000, -4_669), pt(3_000, 0)];
        write_funding_partition(&path, InstrumentId::new(7), &sched).unwrap();
        let back = read_funding_partition(&path).unwrap();
        assert_eq!(back, sched);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_non_ascending() {
        let path = tmp("desc");
        let _ = std::fs::remove_file(&path);
        // Written out of order — read must reject it (the exchange relies on order).
        write_funding_partition(&path, InstrumentId::new(0), &[pt(2_000, 1), pt(1_000, 1)])
            .unwrap();
        assert!(read_funding_partition(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn caches_then_serves_without_refetching() {
        let path = tmp("cache");
        let _ = std::fs::remove_file(&path);
        let sched = vec![pt(1_000, 5), pt(2_000, 6)];
        let fetches = Cell::new(0);
        // Cache miss: fetch builds + writes.
        let a = load_or_cache_funding(&path, InstrumentId::new(0), || {
            fetches.set(fetches.get() + 1);
            Ok(sched.clone())
        })
        .unwrap();
        assert_eq!(a, sched);
        assert_eq!(fetches.get(), 1);
        // Cache hit: fetch is NOT called again (the whole point — no re-download).
        let b = load_or_cache_funding(&path, InstrumentId::new(0), || {
            fetches.set(fetches.get() + 1);
            Ok(Vec::new())
        })
        .unwrap();
        assert_eq!(b, sched);
        assert_eq!(
            fetches.get(),
            1,
            "second run served from cache, fetch not called"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fetch_error_propagates_and_writes_nothing() {
        let path = tmp("fetcherr");
        let _ = std::fs::remove_file(&path);
        let r = load_or_cache_funding(&path, InstrumentId::new(0), || {
            Err(DataError::Schema("boom".to_owned()))
        });
        assert!(r.is_err());
        assert!(
            !path.exists(),
            "a failed fetch leaves no partial cache file"
        );
    }
}
