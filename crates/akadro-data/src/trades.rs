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
}
