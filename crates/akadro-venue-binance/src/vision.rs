// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Binance **Vision** bulk-historical data (`data.binance.vision`) — flat ZIP/CSV
//! archives of klines, the right way to get *years* of bars without paginating
//! the rate-limited `/api/v3/klines` REST endpoint (and with no API key).
//!
//! The pure parsing surface — [`parse_vision_csv`] / [`parse_vision_row`], the
//! [`VisionBarSource`] [`DataSource`], and the URL/checksum builders — is always
//! compiled and fixture-tested. ZIP decoding (`unzip_single_csv`) is behind the
//! optional **`vision-zip`** feature; the live HTTP download (`VisionDownloader`,
//! in `net.rs`) behind **`vision-net`** (= `net` + `vision-zip`), using
//! `reqwest::blocking` (no tokio) and excluded from the coverage gate like the
//! other live IO.
//!
//! Bars are stamped at **close time** in nanoseconds — bit-identical to
//! [`parse_klines`](crate::parse_klines) — so cached Vision bars and REST bars
//! interoperate. Pair [`VisionBarSource`] with `akadro-data`'s `load_or_cache` /
//! `cache_from_source` to download once and replay from the Arrow cache.
//!
//! ## Format facts this module encodes (verified against real archives)
//! * URL: `…/data/spot/{daily|monthly}/klines/{SYMBOL}/{INTERVAL}/{SYMBOL}-{INTERVAL}-{DATE}.zip`
//!   (`DATE` = `YYYY-MM-DD` daily, `YYYY-MM` monthly). The **monthly bar interval
//!   token is `1mo`**, not the REST `1M`.
//! * Each ZIP holds one same-named CSV: 12 comma-separated fields, no quoting,
//!   `open_time, open, high, low, close, volume, close_time, …` (fields 7–11,
//!   incl. the garbage `ignore`, are discarded). Usually **no header row**, but a
//!   header is tolerated (sniffed by whether field 0 is an integer).
//! * **Timestamp unit:** spot data from 2025-01-01 is in **microseconds**; earlier
//!   is **milliseconds**. The unit is detected per file from field 0's magnitude
//!   ([`VisionTimeUnit::detect`]) and applied to the close time — a hard-coded
//!   `×1_000_000` would be 1000× wrong on recent data.

use std::collections::VecDeque;

use akadro_core::{Bar, DataSource, Event, InstrumentId, Price, Qty, Timestamp};

use crate::{BinanceError, decimal_to_raw};

/// Default Binance Vision host.
pub const VISION_BASE: &str = "https://data.binance.vision";

/// Archive granularity: one file per day or per month.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisionGranularity {
    /// One file per UTC day (`YYYY-MM-DD`), available the next day.
    Daily,
    /// One file per month (`YYYY-MM`), available early the following month.
    Monthly,
}

impl VisionGranularity {
    fn as_str(self) -> &'static str {
        match self {
            VisionGranularity::Daily => "daily",
            VisionGranularity::Monthly => "monthly",
        }
    }
}

/// The timestamp unit of a Vision file's `open_time`/`close_time` columns. Spot
/// data from 2025-01-01 onward is [`Micros`](VisionTimeUnit::Micros); earlier data
/// is [`Millis`](VisionTimeUnit::Millis).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisionTimeUnit {
    /// 13-digit millisecond epoch (pre-2025 spot).
    Millis,
    /// 16-digit microsecond epoch (2025+ spot).
    Micros,
}

impl VisionTimeUnit {
    /// Detect the unit from a raw `open_time` integer by magnitude: `>= 1e15`
    /// (≥16 digits) is microseconds, otherwise milliseconds. Decided once per file
    /// from its first data row — a millisecond epoch stays 13 digits until ~year
    /// 2286, so the threshold is unambiguous for any plausible date.
    #[must_use]
    pub fn detect(open_time_raw: i64) -> Self {
        if open_time_raw >= 1_000_000_000_000_000 {
            VisionTimeUnit::Micros
        } else {
            VisionTimeUnit::Millis
        }
    }

    /// Nanoseconds per unit: `1_000_000` for ms, `1_000` for µs.
    #[must_use]
    pub fn nanos_per(self) -> i64 {
        match self {
            VisionTimeUnit::Millis => 1_000_000,
            VisionTimeUnit::Micros => 1_000,
        }
    }
}

// --- URL + checksum builders -------------------------------------------------

/// Build the spot-klines ZIP URL. `date_token` is `YYYY-MM-DD` (daily) or
/// `YYYY-MM` (monthly). `interval` is a Binance kline token — note the **monthly
/// bar** is `"1mo"` here (the REST API spells it `"1M"`).
#[must_use]
pub fn vision_zip_url(
    base: &str,
    granularity: VisionGranularity,
    symbol: &str,
    interval: &str,
    date_token: &str,
) -> String {
    format!(
        "{base}/data/spot/{}/klines/{symbol}/{interval}/{symbol}-{interval}-{date_token}.zip",
        granularity.as_str()
    )
}

/// The companion checksum URL for a ZIP URL (`<zip_url>.CHECKSUM`).
#[must_use]
pub fn vision_checksum_url(zip_url: &str) -> String {
    format!("{zip_url}.CHECKSUM")
}

/// Extract the lowercase hex SHA-256 from a `.CHECKSUM` body (GNU `sha256sum`
/// format: `<64-hex><whitespace><filename>`).
///
/// # Errors
/// [`BinanceError::Parse`] if the first token is not 64 hex characters.
pub fn parse_checksum(body: &str) -> Result<String, BinanceError> {
    let hash = body
        .split_whitespace()
        .next()
        .ok_or_else(|| BinanceError::Parse("empty checksum body".into()))?;
    if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(hash.to_ascii_lowercase())
    } else {
        Err(BinanceError::Parse(format!("bad checksum hash {hash:?}")))
    }
}

// --- CSV parsing -------------------------------------------------------------

/// Parse one Vision kline CSV row into a [`Bar`], stamped at **close time** in ns
/// (using `unit`), exactly like [`parse_klines`](crate::parse_klines). Returns
/// `Ok(None)` for a header row (field 0 is not an integer) so the caller skips it.
///
/// `unit` must be the unit detected for the whole file (see [`parse_vision_csv`]).
///
/// # Errors
/// [`BinanceError::Parse`] on too few fields, a bad number, or a timestamp overflow.
pub fn parse_vision_row(
    line: &str,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
    unit: VisionTimeUnit,
) -> Result<Option<Bar>, BinanceError> {
    let fields: Vec<&str> = line.split(',').collect();
    if fields.len() < 7 {
        return Err(BinanceError::Parse(format!(
            "vision row has {} fields",
            fields.len()
        )));
    }
    // Header sniff: a data row's open_time (field 0) is always an integer.
    if fields[0].trim().parse::<i64>().is_err() {
        return Ok(None);
    }
    let close_raw: i64 = fields[6]
        .trim()
        .parse()
        .map_err(|_| BinanceError::Parse(format!("vision close_time not int: {:?}", fields[6])))?;
    let ts_ns = close_raw
        .checked_mul(unit.nanos_per())
        .ok_or_else(|| BinanceError::Parse("vision close_time overflow".into()))?;
    Ok(Some(Bar::new(
        instrument,
        Timestamp::from_nanos(ts_ns),
        Price::from_raw(decimal_to_raw(fields[1], price_scale)?),
        Price::from_raw(decimal_to_raw(fields[2], price_scale)?),
        Price::from_raw(decimal_to_raw(fields[3], price_scale)?),
        Price::from_raw(decimal_to_raw(fields[4], price_scale)?),
        Qty::from_raw(decimal_to_raw(fields[5], qty_scale)?),
    )))
}

/// Parse a whole decompressed Vision kline CSV body into [`Bar`]s. The timestamp
/// unit is detected once from the first data row and applied to every row; an
/// optional header row is skipped; blank lines are ignored.
///
/// # Errors
/// [`BinanceError::Parse`] on a malformed data row.
pub fn parse_vision_csv(
    csv: &str,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
) -> Result<Vec<Bar>, BinanceError> {
    // Detect the unit from the first line whose first field is an integer.
    let unit = csv
        .lines()
        .filter_map(|l| l.split(',').next())
        .find_map(|f| f.trim().parse::<i64>().ok())
        .map_or(VisionTimeUnit::Millis, VisionTimeUnit::detect);
    let mut bars = Vec::new();
    for line in csv.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(bar) = parse_vision_row(line, instrument, price_scale, qty_scale, unit)? {
            bars.push(bar);
        }
    }
    Ok(bars)
}

/// A [`DataSource`] that replays bars parsed from already-decompressed Vision CSV
/// bodies. No I/O — build it from downloaded/cached CSV text and feed it to the
/// engine or to `akadro-data`'s caching layer (`cache_from_source` / `load_or_cache`).
///
/// Bars are yielded in the order parsed; pass CSV bodies in **chronological order**
/// (each file's rows are already ascending) so the stream stays ascending — the
/// Arrow cache's `read_partition` rejects out-of-order partitions.
pub struct VisionBarSource {
    bars: VecDeque<Bar>,
}

impl VisionBarSource {
    /// Build from one CSV body.
    ///
    /// # Errors
    /// [`BinanceError::Parse`] on a malformed row.
    pub fn from_csv(
        csv: &str,
        instrument: InstrumentId,
        price_scale: u32,
        qty_scale: u32,
    ) -> Result<Self, BinanceError> {
        Ok(VisionBarSource {
            bars: parse_vision_csv(csv, instrument, price_scale, qty_scale)?.into(),
        })
    }

    /// Build from several CSV bodies (e.g. a span of daily/monthly files), parsed
    /// in the given order and concatenated. Each body's timestamp unit is detected
    /// independently, so a span crossing the 2025 ms→µs boundary is handled.
    ///
    /// # Errors
    /// [`BinanceError::Parse`] on a malformed row in any body.
    pub fn from_csvs(
        csvs: &[&str],
        instrument: InstrumentId,
        price_scale: u32,
        qty_scale: u32,
    ) -> Result<Self, BinanceError> {
        let mut bars = VecDeque::new();
        for csv in csvs {
            bars.extend(parse_vision_csv(csv, instrument, price_scale, qty_scale)?);
        }
        Ok(VisionBarSource { bars })
    }

    // No raw-`Bar` hand-injection constructor: a venue connector must not be a data
    // leakage point — data enters only through the parse path (`from_csv`/`from_csvs`),
    // per D18 (library-owns-data). To replay an arbitrary `Vec<Bar>`, use
    // `akadro_backtest::HistoricalFeed::from_bars` (the blessed replay primitive for
    // cache-/connector-sourced bars).
}

impl DataSource for VisionBarSource {
    fn next_event(&mut self) -> Option<Event> {
        self.bars.pop_front().map(Event::Bar)
    }
}

impl core::fmt::Debug for VisionBarSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VisionBarSource")
            .field("buffered", &self.bars.len())
            .finish()
    }
}

// --- ZIP decode (the `vision-zip` feature) -----------------------------------

/// Decompress a Vision archive's single CSV entry to a `String`.
///
/// # Errors
/// [`BinanceError::Parse`] if the archive is empty, malformed, or the entry is not
/// valid UTF-8.
#[cfg(feature = "vision-zip")]
pub fn unzip_single_csv(zip_bytes: &[u8]) -> Result<String, BinanceError> {
    use std::io::Read as _;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))
        .map_err(|e| BinanceError::Parse(format!("vision zip open: {e}")))?;
    if archive.is_empty() {
        return Err(BinanceError::Parse("vision zip is empty".into()));
    }
    let mut entry = archive
        .by_index(0)
        .map_err(|e| BinanceError::Parse(format!("vision zip entry: {e}")))?;
    let mut csv = String::new();
    entry
        .read_to_string(&mut csv)
        .map_err(|e| BinanceError::Parse(format!("vision zip read: {e}")))?;
    Ok(csv)
}

/// Optionally verify a downloaded ZIP against its `.CHECKSUM` body (GNU
/// `sha256sum` format), then [`unzip_single_csv`] it. Pass `None` to skip
/// verification. This is the pure (network-free) half of the download path, so
/// the checksum-mismatch branch is unit-testable.
///
/// # Errors
/// [`BinanceError::Parse`] on a malformed checksum body, a SHA-256 mismatch, or a
/// bad archive.
#[cfg(feature = "vision-zip")]
pub fn verify_and_unzip(
    zip_bytes: &[u8],
    checksum_body: Option<&str>,
) -> Result<String, BinanceError> {
    if let Some(body) = checksum_body {
        use sha2::{Digest as _, Sha256};
        let want = parse_checksum(body)?;
        let got = hex::encode(Sha256::digest(zip_bytes));
        if got != want {
            return Err(BinanceError::Parse(format!(
                "vision checksum mismatch: {got} != {want}"
            )));
        }
    }
    unzip_single_csv(zip_bytes)
}

// The live HTTP downloader, [`VisionDownloader`] (the `vision-net` feature =
// `net` + `vision-zip`), lives in `net.rs` alongside the other `reqwest::blocking`
// transports — live IO, excluded from the coverage gate like the rest.

#[cfg(test)]
mod tests {
    use super::*;

    // A real-shape ms row (2023): open=…000, close=…059999 (13-digit timestamps).
    const MS_CSV: &str = "1700000000000,100.00,101.00,99.00,100.50,10.00000000,1700000059999,1005.0,42,5.0,500.0,0\n\
                          1700000060000,100.50,106.00,100.00,105.00,8.00000000,1700000119999,840.0,30,4.0,420.0,0";

    // A real-shape µs row (2025+): 16-digit timestamps.
    const US_CSV: &str = "1735689600000000,42000.0,42100.0,41900.0,42050.0,3.50000000,1735689659999999,147175.0,99,1.5,63075.0,0";

    #[test]
    fn time_unit_detection_boundary() {
        assert_eq!(
            VisionTimeUnit::detect(1_700_000_000_000),
            VisionTimeUnit::Millis
        ); // 13 digits
        assert_eq!(
            VisionTimeUnit::detect(999_999_999_999_999),
            VisionTimeUnit::Millis
        ); // 1e15-1
        assert_eq!(
            VisionTimeUnit::detect(1_000_000_000_000_000),
            VisionTimeUnit::Micros
        ); // 1e15
        assert_eq!(
            VisionTimeUnit::detect(1_735_689_600_000_000),
            VisionTimeUnit::Micros
        ); // 16 digits
        assert_eq!(VisionTimeUnit::Millis.nanos_per(), 1_000_000);
        assert_eq!(VisionTimeUnit::Micros.nanos_per(), 1_000);
    }

    #[test]
    fn ms_csv_parses_and_matches_close_time_stamping() {
        let bars = parse_vision_csv(MS_CSV, InstrumentId::new(0), 2, 8).unwrap();
        assert_eq!(bars.len(), 2);
        // Stamped at close_time (1700000059999 ms) → ns, identical to parse_klines.
        assert_eq!(bars[0].ts.as_nanos(), 1_700_000_059_999_000_000);
        assert_eq!(bars[0].open, Price::from_raw(10_000)); // 100.00 @ 2
        assert_eq!(bars[0].high, Price::from_raw(10_100));
        assert_eq!(bars[0].low, Price::from_raw(9_900));
        assert_eq!(bars[0].close, Price::from_raw(10_050));
        assert_eq!(bars[0].volume, Qty::from_raw(1_000_000_000)); // 10.0 @ 8
        assert_eq!(bars[1].ts.as_nanos(), 1_700_000_119_999_000_000);
    }

    #[test]
    fn us_csv_uses_microsecond_unit_not_millisecond() {
        let bars = parse_vision_csv(US_CSV, InstrumentId::new(0), 1, 8).unwrap();
        assert_eq!(bars.len(), 1);
        // close_time 1735689659999999 µs × 1_000 ns/µs.
        assert_eq!(bars[0].ts.as_nanos(), 1_735_689_659_999_999_000);
        // The kill-bug guard: a ms misparse (×1_000_000) would be exactly 1000×
        // larger — ~1.7e21, which doesn't even fit in i64 (computed in i128 here).
        assert_ne!(
            i128::from(bars[0].ts.as_nanos()),
            1_735_689_659_999_999_i128 * 1_000_000
        );
        assert_eq!(bars[0].open, Price::from_raw(420_000)); // 42000.0 @ 1
    }

    #[test]
    fn header_row_is_tolerated_without_dropping_data() {
        let with_header = format!(
            "open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_base,taker_quote,ignore\n{MS_CSV}"
        );
        let with = parse_vision_csv(&with_header, InstrumentId::new(0), 2, 8).unwrap();
        let without = parse_vision_csv(MS_CSV, InstrumentId::new(0), 2, 8).unwrap();
        assert_eq!(with, without); // header skipped, no off-by-one
        assert_eq!(with.len(), 2);
    }

    #[test]
    fn header_only_and_blank_csv_yield_no_bars() {
        // No numeric row → the unit-detection fallback (Millis) is harmless and the
        // result is simply empty (not an error).
        let header_only =
            "open_time,open,high,low,close,volume,close_time,quote_volume,count,tb,tq,ignore";
        assert!(
            parse_vision_csv(header_only, InstrumentId::new(0), 2, 8)
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_vision_csv("", InstrumentId::new(0), 2, 8)
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_vision_csv("   \n\n  ", InstrumentId::new(0), 2, 8)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn errors_on_short_row_and_bad_number() {
        assert!(parse_vision_csv("1,2,3", InstrumentId::new(0), 2, 8).is_err()); // < 7 fields
        let bad_num = "1700000000000,x.y,1,1,1,1,1700000059999,0,0,0,0,0";
        assert!(parse_vision_csv(bad_num, InstrumentId::new(0), 2, 8).is_err());
        let bad_close = "1700000000000,1,1,1,1,1,notanint,0,0,0,0,0";
        assert!(parse_vision_csv(bad_close, InstrumentId::new(0), 2, 8).is_err());
    }

    #[test]
    fn vision_bar_source_streams_in_order_and_from_csvs_concatenates() {
        use akadro_core::Event;
        let mut src = VisionBarSource::from_csv(MS_CSV, InstrumentId::new(0), 2, 8).unwrap();
        assert!(format!("{src:?}").contains("VisionBarSource"));
        let mut prev = 0;
        let mut n = 0;
        while let Some(Event::Bar(bar)) = src.next_event() {
            assert!(bar.ts.as_nanos() > prev, "ascending");
            prev = bar.ts.as_nanos();
            n += 1;
        }
        assert_eq!(n, 2);
        // from_csvs concatenates two bodies (here ms then µs — each unit detected separately).
        let both =
            VisionBarSource::from_csvs(&[MS_CSV, US_CSV], InstrumentId::new(0), 2, 8).unwrap();
        assert_eq!(both.bars.len(), 3);
        // An empty body parses to an empty source (no rows).
        assert_eq!(
            VisionBarSource::from_csv("", InstrumentId::new(0), 2, 8)
                .unwrap()
                .bars
                .len(),
            0
        );
    }

    #[test]
    fn url_builders_match_the_real_layout() {
        assert_eq!(
            vision_zip_url(
                VISION_BASE,
                VisionGranularity::Daily,
                "BTCUSDT",
                "1m",
                "2025-05-30"
            ),
            "https://data.binance.vision/data/spot/daily/klines/BTCUSDT/1m/BTCUSDT-1m-2025-05-30.zip"
        );
        assert_eq!(
            vision_zip_url(
                VISION_BASE,
                VisionGranularity::Monthly,
                "BTCUSDT",
                "1mo",
                "2025-01"
            ),
            "https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1mo/BTCUSDT-1mo-2025-01.zip"
        );
        assert_eq!(
            vision_checksum_url("https://x/BTCUSDT-1m-2025-01.zip"),
            "https://x/BTCUSDT-1m-2025-01.zip.CHECKSUM"
        );
    }

    #[test]
    fn checksum_parse_handles_gnu_format_and_rejects_garbage() {
        let body = "8d028b2f91aad57d6b693d44449c93f9c4b7044f55f298c8a3ac40ab676dafac  BTCUSDT-1m-2025-01.zip\n";
        assert_eq!(
            parse_checksum(body).unwrap(),
            "8d028b2f91aad57d6b693d44449c93f9c4b7044f55f298c8a3ac40ab676dafac"
        );
        assert!(parse_checksum("").is_err());
        assert!(parse_checksum("nothex  file.zip").is_err());
        assert!(parse_checksum("abc  file.zip").is_err()); // too short
    }
}
