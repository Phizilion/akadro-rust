// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Binance Vision bulk-historical connector: parity with the REST kline parser,
//! integration with the `akadro-data` Arrow cache, and (under `vision-zip`) a
//! real in-memory ZIP round-trip. The live HTTP download is the `#[ignore]`d
//! `vision-net` test at the bottom.

use akadro_core::{DataSource, Event, InstrumentId};
use akadro_data::{DataError, cache_from_source, load_or_cache, read_partition};
use akadro_venue_binance::{VisionBarSource, parse_klines, parse_vision_csv};

// Two 1-minute ms rows (12 fields, no header) — the shape of a real spot file.
const MS_CSV: &str = "1700000000000,100.00,101.00,99.00,100.50,10.00000000,1700000059999,1005.0,42,5.0,500.0,0\n\
                      1700000060000,100.50,106.00,100.00,105.00,8.00000000,1700000119999,840.0,30,4.0,420.0,0";

fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("akadro_vision_{name}.feather"))
}

#[test]
fn vision_bars_are_bit_identical_to_the_rest_kline_parser() {
    // The same OHLCV + close_time, fed as a Vision CSV row and as a REST klines
    // JSON row, must produce identical Bars — so cached Vision bars interoperate.
    let id = InstrumentId::new(0);
    let vision = parse_vision_csv(MS_CSV, id, 2, 8).unwrap();
    let json = r#"[
        [1700000000000,"100.00","101.00","99.00","100.50","10.00000000",1700000059999,"1005.0",42,"5.0","500.0","0"],
        [1700000060000,"100.50","106.00","100.00","105.00","8.00000000",1700000119999,"840.0",30,"4.0","420.0","0"]
    ]"#;
    let rest = parse_klines(json, id, 2, 8).unwrap();
    assert_eq!(
        vision, rest,
        "Vision CSV and REST JSON yield identical bars"
    );
}

#[test]
fn vision_source_caches_and_replays_through_akadro_data() {
    let id = InstrumentId::new(0);
    let path = temp_path("cache");
    let _ = std::fs::remove_file(&path);

    // Drain a VisionBarSource into the Arrow cache.
    let src = VisionBarSource::from_csv(MS_CSV, id, 2, 8).unwrap();
    let (count, range) = cache_from_source(src, &path, id, 2, 8).unwrap();
    assert_eq!(count, 2);
    assert_eq!(
        range,
        Some((1_700_000_059_999_000_000, 1_700_000_119_999_000_000))
    );

    // Read it back; bars survive round-trip (ascending-ts validation passes).
    let loaded = read_partition(&path).unwrap();
    assert_eq!(loaded.bars.len(), 2);
    assert_eq!(loaded.price_scale, 2);
    assert_eq!(loaded.qty_scale, 8);
    let direct = parse_vision_csv(MS_CSV, id, 2, 8).unwrap();
    assert_eq!(loaded.bars, direct);

    // `load_or_cache` on a hit must NOT call the factory (here it would panic).
    let replayed = load_or_cache(&path, id, 2, 8, || -> VisionBarSource {
        panic!("factory must not run on a cache hit");
    })
    .unwrap();
    assert_eq!(replayed, direct);

    // A scale mismatch on reload is rejected (M17), not silently mis-scaled.
    let mismatch = read_partition(&path).and_then(|_| {
        load_or_cache(&path, id, 2, 6, || {
            VisionBarSource::from_csv(MS_CSV, InstrumentId::new(0), 2, 6).unwrap()
        })
    });
    assert!(matches!(mismatch, Err(DataError::Schema(_))));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn vision_source_feeds_the_data_source_slot() {
    // Sanity: it's a real DataSource yielding Event::Bar in ascending order.
    let mut src = VisionBarSource::from_csv(MS_CSV, InstrumentId::new(0), 2, 8).unwrap();
    let mut count = 0;
    let mut prev = 0;
    while let Some(Event::Bar(bar)) = src.next_event() {
        assert!(bar.ts.as_nanos() > prev);
        prev = bar.ts.as_nanos();
        count += 1;
    }
    assert_eq!(count, 2);
}

#[cfg(feature = "vision-zip")]
#[test]
fn zip_round_trips_and_parses() {
    use akadro_venue_binance::unzip_single_csv;
    use std::io::Write as _;
    use zip::write::SimpleFileOptions;

    // Build a real in-memory ZIP holding one CSV entry, like a Vision archive.
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        w.start_file(
            "BTCUSDT-1m-2023-06.csv",
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        w.write_all(MS_CSV.as_bytes()).unwrap();
        w.finish().unwrap();
    }

    let csv = unzip_single_csv(&buf).unwrap();
    assert_eq!(csv, MS_CSV, "decompressed CSV matches the input");
    let bars = parse_vision_csv(&csv, InstrumentId::new(0), 2, 8).unwrap();
    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].ts.as_nanos(), 1_700_000_059_999_000_000);

    // A non-zip byte string is a clean parse error, not a panic.
    assert!(unzip_single_csv(b"not a zip").is_err());
}

#[cfg(feature = "vision-zip")]
#[test]
fn checksum_verify_accepts_match_and_rejects_mismatch() {
    use akadro_venue_binance::verify_and_unzip;
    use sha2::{Digest as _, Sha256};
    use std::io::Write as _;
    use zip::write::SimpleFileOptions;

    let mut zip_bytes = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
        w.start_file("x.csv", SimpleFileOptions::default()).unwrap();
        w.write_all(MS_CSV.as_bytes()).unwrap();
        w.finish().unwrap();
    }
    let good = hex::encode(Sha256::digest(&zip_bytes));
    // Matching GNU-format checksum → unzips fine.
    let ok_body = format!("{good}  BTCUSDT-1m-2023-06.zip");
    assert_eq!(
        verify_and_unzip(&zip_bytes, Some(&ok_body)).unwrap(),
        MS_CSV
    );
    // None → skip verification, still unzips.
    assert_eq!(verify_and_unzip(&zip_bytes, None).unwrap(), MS_CSV);
    // Wrong hash → a clean mismatch error (the previously-untested branch).
    let bad_body = format!("{}  x.zip", "0".repeat(64));
    let err = verify_and_unzip(&zip_bytes, Some(&bad_body)).unwrap_err();
    assert!(format!("{err}").contains("checksum mismatch"), "got: {err}");
}

#[cfg(feature = "vision-net")]
#[test]
#[ignore = "requires network access to data.binance.vision"]
fn live_download_a_past_month_and_parse() {
    use akadro_venue_binance::{VisionDownloader, VisionGranularity};
    let dl = VisionDownloader::new().expect("client");
    // A safely-past month (pre-2025 → millisecond timestamps), checksum-verified.
    let csv = dl
        .fetch_csv(VisionGranularity::Monthly, "BTCUSDT", "1m", "2024-01", true)
        .expect("download + verify + unzip");
    let bars = parse_vision_csv(&csv, InstrumentId::new(0), 2, 8).expect("parse");
    assert!(!bars.is_empty(), "expected a month of 1m bars");
    // Bars are ascending (the cache contract).
    assert!(
        bars.windows(2)
            .all(|w| w[1].ts.as_nanos() > w[0].ts.as_nanos())
    );
}
