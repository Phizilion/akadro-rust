// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Aggregating **trade prints into low-timeframe OHLCV bars** — how sub-second
//! data (1s, 500ms, 100ms, …) enters akadro. Venues don't serve sub-minute
//! klines, so the connector fetches trade prints (e.g. Binance `aggTrades`) and
//! these helpers bucket them into bars at the requested interval. The engine is
//! nanosecond-resolution and interval-agnostic, so the resulting bars drive it
//! exactly like minute bars.

use akadro_core::{Bar, InstrumentId, Price, Qty, Timestamp};

/// Parse an interval string to **nanoseconds**, supporting sub-second through
/// weekly: a positive integer followed by a unit — `ms`, `s`, `m` (minute), `h`,
/// `d`, or `W`/`w`. Examples: `"100ms"`, `"500ms"`, `"1s"`, `"5s"`, `"1m"`,
/// `"15m"`, `"4h"`, `"1d"`, `"1W"`. `None` on an empty or non-positive number
/// (e.g. `"0s"`), an unknown unit, or overflow. (Note: lowercase `m` is *minute*;
/// month is not supported here.)
#[must_use]
pub fn interval_to_nanos(interval: &str) -> Option<i64> {
    let s = interval.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    if split == 0 {
        return None; // no numeric prefix
    }
    let num: i64 = s[..split].parse().ok()?;
    if num <= 0 {
        return None; // a zero/empty interval ("0s") is not a valid timeframe
    }
    let unit_ns: i64 = match &s[split..] {
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60_000_000_000,
        "h" => 3_600_000_000_000,
        "d" => 86_400_000_000_000,
        "W" | "w" => 604_800_000_000_000,
        _ => return None,
    };
    num.checked_mul(unit_ns)
}

/// Aggregate time-ordered trade prints `(ts, price, qty)` into OHLCV [`Bar`]s at
/// `interval_ns`. Trades are bucketed by `floor(ts / interval_ns)`; each non-empty
/// bucket yields one bar (open = first print, high/low = extremes, close = last
/// print, volume = summed qty) stamped at the bucket's **close** time
/// (`bucket_start + interval_ns`), matching the close-stamped spot convention.
/// Empty buckets are skipped (no synthetic bars). `interval_ns <= 0` or no trades
/// yields an empty vector. Volume sums widen to `i128` and saturate to `i64`.
#[must_use]
pub fn bars_from_trades(
    instrument: InstrumentId,
    interval_ns: i64,
    trades: &[(Timestamp, Price, Qty)],
) -> Vec<Bar> {
    if interval_ns <= 0 || trades.is_empty() {
        return Vec::new();
    }
    // Single-pass bucketing assumes time-ordered prints (the documented contract,
    // honoured by every connector that produces them). Out-of-order input would
    // silently emit duplicate/misordered buckets; catch that in debug rather than
    // pay an O(n log n) defensive sort on this aggregation hot path in release (m18).
    debug_assert!(
        trades
            .windows(2)
            .all(|w| w[0].0.as_nanos() <= w[1].0.as_nanos()),
        "bars_from_trades requires time-ordered trade prints; got an out-of-order input"
    );
    let mut bars = Vec::new();
    let mut bucket: Option<i64> = None;
    let (mut open, mut high, mut low, mut close) =
        (Price::ZERO, Price::ZERO, Price::ZERO, Price::ZERO);
    let mut vol: i128 = 0;
    let flush =
        |start: i64, o: Price, h: Price, l: Price, c: Price, v: i128, out: &mut Vec<Bar>| {
            let v64 = i64::try_from(v).unwrap_or(i64::MAX);
            out.push(Bar::new(
                instrument,
                Timestamp::from_nanos(start.saturating_add(interval_ns)),
                o,
                h,
                l,
                c,
                Qty::from_raw(v64),
            ));
        };
    for &(ts, px, qty) in trades {
        let b = ts.as_nanos().div_euclid(interval_ns) * interval_ns;
        if bucket == Some(b) {
            if px.raw() > high.raw() {
                high = px;
            }
            if px.raw() < low.raw() {
                low = px;
            }
            close = px;
            vol += i128::from(qty.raw());
        } else {
            if let Some(prev) = bucket {
                flush(prev, open, high, low, close, vol, &mut bars);
            }
            bucket = Some(b);
            open = px;
            high = px;
            low = px;
            close = px;
            vol = i128::from(qty.raw());
        }
    }
    if let Some(prev) = bucket {
        flush(prev, open, high, low, close, vol, &mut bars);
    }
    bars
}

/// Aggregate finer-timeframe [`Bar`]s up to a coarser timeframe — the cache's
/// multi-timeframe path. When a request for, say, 5m bars isn't cached but a 1m
/// series is, the loader aggregates 1m→5m for the covered span and downloads only
/// what's still missing.
///
/// `fine_ns` / `coarse_ns` are the input/output bar widths in nanoseconds. They must
/// be **divisible** (`coarse_ns % fine_ns == 0`) with `coarse_ns >= fine_ns`,
/// otherwise aggregation is meaningless and an empty vector is returned. Inputs are
/// assumed time-ordered, unique, and all one instrument (the cache's contract);
/// out-of-order input is caught by `debug_assert` rather than paying a release-mode
/// sort.
///
/// Fine bars are bucketed by their **open** time (`close − fine_ns`, since akadro
/// close-stamps); each coarse bar is `open = first.open`, `high = max`, `low = min`,
/// `close = last.close`, `volume = Σ` (widened to `i128`, saturated back to `i64`),
/// and **close-stamped** at `bucket_open + coarse_ns`.
///
/// Only **complete** buckets (exactly `coarse_ns / fine_ns` fine bars) are emitted —
/// a partially-filled bucket (a hole, or the still-forming edge) is *not* trusted as
/// a finished coarse bar. Use [`aggregate_bars_checked`] to also learn which coarse
/// stamps were incomplete so the caller can fetch them.
#[must_use]
pub fn aggregate_bars(fine: &[Bar], fine_ns: i64, coarse_ns: i64) -> Vec<Bar> {
    aggregate_bars_checked(fine, fine_ns, coarse_ns).0
}

/// As [`aggregate_bars`], but also returns the **close-stamps of incomplete buckets**
/// (ascending): coarse buckets that held fewer than `coarse_ns / fine_ns` fine bars,
/// so they're a hole (or the still-forming edge) rather than a finished coarse bar.
/// The cache loader treats those stamps as still-missing and fetches them, never
/// emitting a half-formed coarse bar from a partial bucket.
#[must_use]
pub fn aggregate_bars_checked(fine: &[Bar], fine_ns: i64, coarse_ns: i64) -> (Vec<Bar>, Vec<i64>) {
    if fine.is_empty() || fine_ns <= 0 || coarse_ns < fine_ns || coarse_ns % fine_ns != 0 {
        return (Vec::new(), Vec::new());
    }
    debug_assert!(
        fine.windows(2)
            .all(|w| w[0].ts.as_nanos() <= w[1].ts.as_nanos()),
        "aggregate_bars requires time-ordered fine bars; got an out-of-order input"
    );
    let expected = coarse_ns / fine_ns; // fine bars in a complete coarse bucket
    let instrument = fine[0].instrument;

    let mut bars = Vec::new();
    let mut incomplete = Vec::new();
    let mut bucket: Option<i64> = None; // current bucket's open time
    let (mut open, mut high, mut low, mut close) =
        (Price::ZERO, Price::ZERO, Price::ZERO, Price::ZERO);
    let (mut vol, mut count): (i128, i64) = (0, 0);

    let flush = |b_open: i64,
                 o: Price,
                 h: Price,
                 l: Price,
                 c: Price,
                 v: i128,
                 n: i64,
                 out: &mut Vec<Bar>,
                 inc: &mut Vec<i64>| {
        let close_stamp = b_open.saturating_add(coarse_ns);
        if n == expected {
            let v64 = i64::try_from(v).unwrap_or(i64::MAX);
            out.push(Bar::new(
                instrument,
                Timestamp::from_nanos(close_stamp),
                o,
                h,
                l,
                c,
                Qty::from_raw(v64),
            ));
        } else {
            inc.push(close_stamp); // partial bucket → not a finished coarse bar
        }
    };

    for b in fine {
        // Close-stamped → open = close − fine_ns; bucket the open by coarse width.
        let b_open =
            b.ts.as_nanos()
                .saturating_sub(fine_ns)
                .div_euclid(coarse_ns)
                * coarse_ns;
        if bucket == Some(b_open) {
            if b.high.raw() > high.raw() {
                high = b.high;
            }
            if b.low.raw() < low.raw() {
                low = b.low;
            }
            close = b.close;
            vol += i128::from(b.volume.raw());
            count += 1;
        } else {
            if let Some(prev) = bucket {
                flush(
                    prev,
                    open,
                    high,
                    low,
                    close,
                    vol,
                    count,
                    &mut bars,
                    &mut incomplete,
                );
            }
            bucket = Some(b_open);
            open = b.open;
            high = b.high;
            low = b.low;
            close = b.close;
            vol = i128::from(b.volume.raw());
            count = 1;
        }
    }
    if let Some(prev) = bucket {
        flush(
            prev,
            open,
            high,
            low,
            close,
            vol,
            count,
            &mut bars,
            &mut incomplete,
        );
    }
    (bars, incomplete)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_parsing() {
        assert_eq!(interval_to_nanos("100ms"), Some(100_000_000));
        assert_eq!(interval_to_nanos("500ms"), Some(500_000_000));
        assert_eq!(interval_to_nanos("1s"), Some(1_000_000_000));
        assert_eq!(interval_to_nanos("5s"), Some(5_000_000_000));
        assert_eq!(interval_to_nanos("1m"), Some(60_000_000_000));
        assert_eq!(interval_to_nanos("15m"), Some(900_000_000_000));
        assert_eq!(interval_to_nanos("4h"), Some(14_400_000_000_000));
        assert_eq!(interval_to_nanos("1d"), Some(86_400_000_000_000));
        assert_eq!(interval_to_nanos("1W"), Some(604_800_000_000_000));
        assert_eq!(interval_to_nanos(" 250ms "), Some(250_000_000)); // trimmed
        // Rejections.
        assert_eq!(interval_to_nanos("ms"), None); // no number
        assert_eq!(interval_to_nanos("5x"), None); // bad unit
        assert_eq!(interval_to_nanos(""), None);
        assert_eq!(interval_to_nanos("1y"), None);
        assert_eq!(interval_to_nanos("0s"), None); // non-positive interval (m43)
        assert_eq!(interval_to_nanos("0ms"), None);
    }

    #[test]
    fn aggregates_trades_into_sub_second_bars() {
        let i = InstrumentId::new(0);
        let p = Price::from_raw;
        let q = Qty::from_raw;
        let t = Timestamp::from_nanos;
        // 100ms buckets. Prints (ms→ns):
        // [0,100): 0ms@100/3, 50ms@110/2  -> O100 H110 L100 C110 V5, close-ts 100ms
        // [100,200): 150ms@90/1           -> single print bar, close-ts 200ms
        // [200,300): 250ms@95/4, 299ms@93/1 -> O95 H95 L93 C93 V5, close-ts 300ms
        let ms = 1_000_000;
        let trades = vec![
            (t(0), p(100), q(3)),
            (t(50 * ms), p(110), q(2)),
            (t(150 * ms), p(90), q(1)),
            (t(250 * ms), p(95), q(4)),
            (t(299 * ms), p(93), q(1)),
        ];
        let bars = bars_from_trades(i, 100 * ms, &trades);
        assert_eq!(bars.len(), 3);
        // bucket 0
        assert_eq!(bars[0].ts.as_nanos(), 100 * ms); // close-stamped
        assert_eq!(bars[0].open.raw(), 100);
        assert_eq!(bars[0].high.raw(), 110);
        assert_eq!(bars[0].low.raw(), 100);
        assert_eq!(bars[0].close.raw(), 110);
        assert_eq!(bars[0].volume.raw(), 5);
        // bucket 1 (single print)
        assert_eq!(bars[1].ts.as_nanos(), 200 * ms);
        assert_eq!(bars[1].open.raw(), 90);
        assert_eq!(bars[1].close.raw(), 90);
        assert_eq!(bars[1].volume.raw(), 1);
        // bucket 2
        assert_eq!(bars[2].open.raw(), 95);
        assert_eq!(bars[2].high.raw(), 95);
        assert_eq!(bars[2].low.raw(), 93);
        assert_eq!(bars[2].close.raw(), 93);
        assert_eq!(bars[2].volume.raw(), 5);
    }

    #[test]
    fn empty_and_degenerate() {
        let i = InstrumentId::new(0);
        assert!(bars_from_trades(i, 1000, &[]).is_empty());
        assert!(
            bars_from_trades(
                i,
                0,
                &[(
                    Timestamp::from_nanos(1),
                    Price::from_raw(1),
                    Qty::from_raw(1)
                )]
            )
            .is_empty()
        );
        // Single trade → one bar.
        let bars = bars_from_trades(
            i,
            1_000_000_000,
            &[(
                Timestamp::from_nanos(5),
                Price::from_raw(42),
                Qty::from_raw(7),
            )],
        );
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].close.raw(), 42);
        assert_eq!(bars[0].volume.raw(), 7);
    }

    const MIN: i64 = 60_000_000_000; // 1m in ns

    /// A 1m fine bar opening at `open_min` minutes (close-stamped at +1m).
    fn fb(open_min: i64, open: i64, high: i64, low: i64, close: i64, vol: i64) -> Bar {
        Bar::new(
            InstrumentId::new(0),
            Timestamp::from_nanos((open_min + 1) * MIN),
            Price::from_raw(open),
            Price::from_raw(high),
            Price::from_raw(low),
            Price::from_raw(close),
            Qty::from_raw(vol),
        )
    }

    #[test]
    fn aggregate_1m_to_5m_complete() {
        // 10 contiguous 1m bars → two complete 5m bars. OHLCV: open=first, high=max,
        // low=min, close=last, vol=Σ; coarse bars close-stamped at bucket_open + 5m.
        let fine: Vec<Bar> = (0..10)
            .map(|i| fb(i, 10 + i, 20 + i, 5 + i, 15 + i, i + 1))
            .collect();
        let (coarse, incomplete) = aggregate_bars_checked(&fine, MIN, 5 * MIN);
        assert!(incomplete.is_empty());
        assert_eq!(coarse.len(), 2);

        // Bucket 0: bars opening 0..4 min, close-stamped at 5m.
        assert_eq!(coarse[0].ts.as_nanos(), 5 * MIN);
        assert_eq!(coarse[0].open.raw(), 10); // first.open
        assert_eq!(coarse[0].high.raw(), 24); // max(20..24)
        assert_eq!(coarse[0].low.raw(), 5); // min(5..9)
        assert_eq!(coarse[0].close.raw(), 19); // last.close (15+4)
        assert_eq!(coarse[0].volume.raw(), 1 + 2 + 3 + 4 + 5);

        // Bucket 1: bars opening 5..9 min, close-stamped at 10m.
        assert_eq!(coarse[1].ts.as_nanos(), 10 * MIN);
        assert_eq!(coarse[1].open.raw(), 15);
        assert_eq!(coarse[1].high.raw(), 29);
        assert_eq!(coarse[1].low.raw(), 10);
        assert_eq!(coarse[1].close.raw(), 24);
        assert_eq!(coarse[1].volume.raw(), 6 + 7 + 8 + 9 + 10);

        // The plain entrypoint returns the same complete bars.
        assert_eq!(aggregate_bars(&fine, MIN, 5 * MIN), coarse);
    }

    #[test]
    fn incomplete_bucket_is_reported_not_emitted() {
        // 7 bars: bucket 0 complete (0..4), bucket 1 partial (5,6 only) → not emitted,
        // its close-stamp (10m) reported as still-missing so the loader fetches it.
        let fine: Vec<Bar> = (0..7).map(|i| fb(i, 100, 100, 100, 100, 1)).collect();
        let (coarse, incomplete) = aggregate_bars_checked(&fine, MIN, 5 * MIN);
        assert_eq!(coarse.len(), 1);
        assert_eq!(coarse[0].ts.as_nanos(), 5 * MIN);
        assert_eq!(incomplete, vec![10 * MIN]);
    }

    #[test]
    fn non_divisible_or_degenerate_yields_empty() {
        let fine: Vec<Bar> = (0..6).map(|i| fb(i, 1, 1, 1, 1, 1)).collect();
        // 5m is not a whole multiple of 2m.
        assert!(aggregate_bars(&fine, 2 * MIN, 5 * MIN).is_empty());
        // coarse < fine.
        assert!(aggregate_bars(&fine, 5 * MIN, MIN).is_empty());
        // Non-positive fine width, and empty input.
        assert!(aggregate_bars(&fine, 0, 5 * MIN).is_empty());
        assert!(aggregate_bars(&[], MIN, 5 * MIN).is_empty());
        // coarse == fine is the identity (divisible, >=): one bar per bucket.
        assert_eq!(aggregate_bars(&fine, MIN, MIN).len(), 6);
    }

    #[test]
    fn volume_saturates_to_i64_max() {
        // Two fine bars whose volumes sum past i64::MAX → saturate, never wrap.
        let fine = vec![fb(0, 1, 1, 1, 1, i64::MAX), fb(1, 1, 1, 1, 1, i64::MAX)];
        let coarse = aggregate_bars(&fine, MIN, 2 * MIN);
        assert_eq!(coarse.len(), 1);
        assert_eq!(coarse[0].volume.raw(), i64::MAX);
    }
}
