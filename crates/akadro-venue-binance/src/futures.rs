// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Binance **USDⓈ-M futures** (`fapi.binance.com`) extras on top of the shared
//! REST machinery: the `PERPETUAL` catalogue
//! ([`BinanceCatalog::from_futures_exchange_info`](crate::BinanceCatalog::from_futures_exchange_info)),
//! the funding-rate history ([`parse_funding_rate`], for
//! `SimulatedExchange::with_funding_schedule`), and the auxiliary **signal**
//! producer feeds — open interest and the global long/short account ratio —
//! which normalize Binance's futures-data JSON into [`Event::Signal`]s on the
//! [`signal_channel`] channels, ready to merge with a price feed
//! (`akadro_data::MergeSource`) and read via `ctx.signal`.
//!
//! Klines and order submission reuse the spot types with their futures switch:
//! `BinanceKlineFeed::new(.., FUTURES_BASE_URL, ..).for_futures()` and
//! `BinanceSpotExec::new(..).for_futures()`.
//!
//! **Liquidations** ([`signal_channel::LIQUIDATIONS`]) have no public REST history
//! endpoint on Binance (the old `allForceOrders` route was removed), so that
//! channel is fed live from the `!forceOrder@arr` WebSocket stream in the
//! `akadro-live` shell — there is deliberately no REST liquidation feed here.

use std::collections::VecDeque;

use akadro_core::{DataSource, Event, InstrumentId, Timestamp, signal_channel};
use serde::Deserialize;

use crate::{BinanceError, HttpRequest, Method, Transport, decimal_to_raw};

/// Base URL for the USDⓈ-M futures API.
pub const FUTURES_BASE_URL: &str = "https://fapi.binance.com";

/// Funding-rate fixed-point scale — re-exported from `akadro-core` so the connector
/// and the engine's `mul_rate` charge cannot drift. Binance reports `fundingRate`
/// with exactly 8 decimals (`"0.00010000"` = 0.01% = 1 bp), so the `1e-8` scale is
/// **lossless** here: `"0.00010000"` → `10_000`, and a sub-bp `"0.00000466"` →
/// `466` rather than rounding to `0` as a basis-point scale would.
pub use akadro_core::FUNDING_RATE_SCALE;

/// The long/short account ratio is normalized to `ratio × 10_000` (scale 4):
/// Binance's `"1.8105"` → `18_105`. Strategies divide by `10_000` to recover it.
pub const LONG_SHORT_RATIO_SCALE: u32 = 4;

/// Open interest is normalized to integer base-asset contracts (scale 0): the
/// fractional part of `sumOpenInterest` is dropped (OI is millions of contracts;
/// sub-unit precision is immaterial and keeps it in `i64`).
pub const OPEN_INTEREST_SCALE: u32 = 0;

/// A futures-data JSON body parser: produces `(timestamp, value)` points.
type SignalParser = fn(&str) -> Result<Vec<(Timestamp, i64)>, BinanceError>;

fn ms_to_ns(ms: i64) -> Result<i64, BinanceError> {
    ms.checked_mul(1_000_000)
        .ok_or_else(|| BinanceError::Parse("timestamp overflow".into()))
}

#[derive(Deserialize)]
struct FundingRow {
    #[serde(rename = "fundingTime")]
    funding_time: i64,
    #[serde(rename = "fundingRate")]
    funding_rate: String,
}

/// Parse `/fapi/v1/fundingRate` history into a `(timestamp, rate)` schedule for
/// `SimulatedExchange::with_funding_schedule` (in `akadro-backtest`). Rates are
/// normalized to [`FUNDING_RATE_SCALE`] (`1e-8`), which is lossless for Binance's
/// 8-decimal `fundingRate` and preserves sub-bp rates instead of rounding them away.
///
/// # Errors
/// [`BinanceError::Parse`] on bad JSON, a bad rate, or a timestamp overflow.
pub fn parse_funding_rate(json: &str) -> Result<Vec<(Timestamp, i64)>, BinanceError> {
    let rows: Vec<FundingRow> =
        serde_json::from_str(json).map_err(|e| BinanceError::Parse(e.to_string()))?;
    rows.into_iter()
        .map(|r| {
            Ok((
                Timestamp::from_nanos(ms_to_ns(r.funding_time)?),
                decimal_to_raw(&r.funding_rate, FUNDING_RATE_SCALE)?,
            ))
        })
        .collect()
}

/// Fetch `/fapi/v1/fundingRate` for `symbol` over `transport` (pass
/// [`FUTURES_BASE_URL`]) and parse it into a `with_funding_schedule` schedule (rates
/// at [`FUNDING_RATE_SCALE`]). `limit` caps the settlements returned (Binance's cap is
/// 1000). A public endpoint — no signing. The reusable connector entry point —
/// callers never build the URL or touch the body.
///
/// # Errors
/// [`BinanceError::Transport`] on a transport failure or a non-2xx status;
/// [`BinanceError::Parse`] on malformed JSON.
pub fn fetch_funding_history<T: Transport>(
    transport: &mut T,
    base_url: &str,
    symbol: &str,
    limit: u32,
) -> Result<Vec<(Timestamp, i64)>, BinanceError> {
    let resp = transport.send(&HttpRequest {
        method: Method::Get,
        url: format!(
            "{base_url}/fapi/v1/fundingRate?symbol={symbol}&limit={}",
            limit.min(1000)
        ),
        api_key: None,
    })?;
    if !resp.is_success() {
        return Err(BinanceError::Transport(format!(
            "fundingRate HTTP {}",
            resp.status
        )));
    }
    parse_funding_rate(&resp.body)
}

/// Page `/fapi/v1/fundingRate` **forward** over `[start_ms, end_ms]` into one
/// ascending [`FUNDING_RATE_SCALE`] schedule. Binance returns settlements ascending
/// from `startTime` (1000/page ≈ 333 days at the 8h cadence), so each page advances
/// `startTime` past the newest settlement seen; a short page (< 1000) or one reaching
/// `end_ms` ends it. De-duplicates by time. The reusable connector entry point.
///
/// # Errors
/// [`BinanceError::Transport`] on a transport failure or a non-2xx status;
/// [`BinanceError::Parse`] on malformed JSON.
pub fn fetch_funding_history_paged<T: Transport>(
    transport: &mut T,
    base_url: &str,
    symbol: &str,
    start_ms: i64,
    end_ms: i64,
    max_pages: u32,
) -> Result<Vec<(Timestamp, i64)>, BinanceError> {
    let mut all: Vec<(Timestamp, i64)> = Vec::new();
    let mut cursor = start_ms;
    for _ in 0..max_pages.max(1) {
        let resp = transport.send(&HttpRequest {
            method: Method::Get,
            url: format!(
                "{base_url}/fapi/v1/fundingRate?symbol={symbol}&startTime={cursor}&endTime={end_ms}&limit=1000"
            ),
            api_key: None,
        })?;
        if !resp.is_success() {
            return Err(BinanceError::Transport(format!(
                "fundingRate HTTP {}",
                resp.status
            )));
        }
        let page = parse_funding_rate(&resp.body)?; // ascending
        let n = page.len();
        let newest_ms = page.last().map(|(t, _)| t.as_nanos() / 1_000_000);
        all.extend(page);
        let Some(newest_ms) = newest_ms else { break };
        if n < 1000 || newest_ms >= end_ms {
            break; // short page (or reached the window end) → done
        }
        let next = newest_ms + 1;
        if next <= cursor {
            break; // no forward progress
        }
        cursor = next;
    }
    all.sort_by_key(|(t, _)| t.as_nanos());
    all.dedup_by_key(|(t, _)| t.as_nanos());
    all.retain(|(t, _)| {
        let ms = t.as_nanos() / 1_000_000;
        ms >= start_ms && ms <= end_ms
    });
    Ok(all)
}

#[derive(Deserialize)]
struct OpenInterestRow {
    #[serde(rename = "sumOpenInterest")]
    sum_open_interest: String,
    timestamp: i64,
}

/// Parse `/futures/data/openInterestHist` into `(timestamp, open_interest)` points
/// (integer contracts, [`OPEN_INTEREST_SCALE`]).
///
/// # Errors
/// [`BinanceError::Parse`] on bad JSON, a bad number, or a timestamp overflow.
pub fn parse_open_interest(json: &str) -> Result<Vec<(Timestamp, i64)>, BinanceError> {
    let rows: Vec<OpenInterestRow> =
        serde_json::from_str(json).map_err(|e| BinanceError::Parse(e.to_string()))?;
    rows.into_iter()
        .map(|r| {
            Ok((
                Timestamp::from_nanos(ms_to_ns(r.timestamp)?),
                decimal_to_raw(&r.sum_open_interest, OPEN_INTEREST_SCALE)?,
            ))
        })
        .collect()
}

#[derive(Deserialize)]
struct LongShortRow {
    #[serde(rename = "longShortRatio")]
    long_short_ratio: String,
    timestamp: i64,
}

/// Parse `/futures/data/globalLongShortAccountRatio` into `(timestamp, ratio)`
/// points (ratio × `10_000`, [`LONG_SHORT_RATIO_SCALE`]).
///
/// # Errors
/// [`BinanceError::Parse`] on bad JSON, a bad number, or a timestamp overflow.
pub fn parse_long_short_ratio(json: &str) -> Result<Vec<(Timestamp, i64)>, BinanceError> {
    let rows: Vec<LongShortRow> =
        serde_json::from_str(json).map_err(|e| BinanceError::Parse(e.to_string()))?;
    rows.into_iter()
        .map(|r| {
            Ok((
                Timestamp::from_nanos(ms_to_ns(r.timestamp)?),
                decimal_to_raw(&r.long_short_ratio, LONG_SHORT_RATIO_SCALE)?,
            ))
        })
        .collect()
}

/// A [`DataSource`] that fetches a Binance futures-data series once and replays it
/// as [`Event::Signal`]s on a [`signal_channel`] channel. Build with
/// [`BinanceSignalFeed::open_interest`] / [`BinanceSignalFeed::long_short_ratio`],
/// then merge it with a price feed via `akadro_data::MergeSource`.
pub struct BinanceSignalFeed<T> {
    transport: T,
    url: String,
    instrument: InstrumentId,
    channel: u16,
    parser: SignalParser,
    buffer: VecDeque<(Timestamp, i64)>,
    fetched: bool,
}

impl<T: Transport> BinanceSignalFeed<T> {
    fn new(
        transport: T,
        url: String,
        instrument: InstrumentId,
        channel: u16,
        parser: SignalParser,
    ) -> Self {
        BinanceSignalFeed {
            transport,
            url,
            instrument,
            channel,
            parser,
            buffer: VecDeque::new(),
            fetched: false,
        }
    }

    /// Open-interest history on the [`signal_channel::OPEN_INTEREST`] channel.
    /// `period` is a Binance window (`"5m"`, `"1h"`, …); `limit` ≤ 500.
    #[must_use]
    pub fn open_interest(
        transport: T,
        base_url: &str,
        symbol: &str,
        instrument: InstrumentId,
        period: &str,
        limit: u32,
    ) -> Self {
        let url = format!(
            "{base_url}/futures/data/openInterestHist?symbol={symbol}&period={period}&limit={limit}"
        );
        Self::new(
            transport,
            url,
            instrument,
            signal_channel::OPEN_INTEREST,
            parse_open_interest,
        )
    }

    /// Global long/short account ratio on the [`signal_channel::LONG_SHORT_RATIO`]
    /// channel. `period` is a Binance window (`"5m"`, `"1h"`, …); `limit` ≤ 500.
    #[must_use]
    pub fn long_short_ratio(
        transport: T,
        base_url: &str,
        symbol: &str,
        instrument: InstrumentId,
        period: &str,
        limit: u32,
    ) -> Self {
        let url = format!(
            "{base_url}/futures/data/globalLongShortAccountRatio?symbol={symbol}&period={period}&limit={limit}"
        );
        Self::new(
            transport,
            url,
            instrument,
            signal_channel::LONG_SHORT_RATIO,
            parse_long_short_ratio,
        )
    }

    fn fetch(&mut self) -> Result<(), BinanceError> {
        let resp = self.transport.send(&HttpRequest {
            method: Method::Get,
            url: self.url.clone(),
            api_key: None,
        })?;
        if !resp.is_success() {
            return Err(BinanceError::Transport(format!(
                "futures-data HTTP {}",
                resp.status
            )));
        }
        let mut points = (self.parser)(&resp.body)?;
        points.sort_by_key(|(t, _)| t.as_nanos());
        self.buffer = points.into();
        Ok(())
    }
}

impl<T: Transport> DataSource for BinanceSignalFeed<T> {
    fn next_event(&mut self) -> Option<Event> {
        if !self.fetched {
            self.fetched = true;
            if self.fetch().is_err() {
                return None;
            }
        }
        self.buffer.pop_front().map(|(ts, value)| Event::Signal {
            instrument: self.instrument,
            channel: self.channel,
            value,
            ts,
        })
    }
}

impl<T> core::fmt::Debug for BinanceSignalFeed<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BinanceSignalFeed")
            .field("channel", &self.channel)
            .field("instrument", &self.instrument)
            .field("buffered", &self.buffer.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BinanceCatalog, HttpResponse, MockTransport};
    use akadro_core::{InstrumentCatalog, InstrumentKind};

    const FUTURES_INFO: &str = r#"{"symbols":[
        {"symbol":"BTCUSDT","baseAsset":"BTC","quoteAsset":"USDT","status":"TRADING",
         "contractType":"PERPETUAL","filters":[
            {"filterType":"PRICE_FILTER","tickSize":"0.10"},
            {"filterType":"LOT_SIZE","stepSize":"0.001"},
            {"filterType":"MIN_NOTIONAL","notional":"5"}]},
        {"symbol":"BTCUSDT_240329","baseAsset":"BTC","quoteAsset":"USDT","status":"TRADING",
         "contractType":"CURRENT_QUARTER","filters":[
            {"filterType":"PRICE_FILTER","tickSize":"0.10"},
            {"filterType":"LOT_SIZE","stepSize":"0.001"}]}
    ]}"#;

    #[test]
    fn futures_catalog_keeps_only_tradable_perpetuals() {
        let cat = BinanceCatalog::from_futures_exchange_info(FUTURES_INFO).unwrap();
        assert_eq!(cat.specs().len(), 1, "dated quarterly skipped");
        let id = cat.id_of("BTCUSDT").unwrap();
        let spec = cat.spec(id).unwrap();
        assert_eq!(spec.kind, InstrumentKind::PerpetualFuture);
        assert_eq!(cat.scales(id), Some((1, 3))); // tick 0.10 → 1, step 0.001 → 3
        // MIN_NOTIONAL "5" @ scale (1+3)=4 → 5 * 1e4.
        assert_eq!(spec.min_notional, akadro_core::Money::from_raw(50_000));
    }

    #[test]
    fn funding_rate_normalizes_to_fine_scale() {
        let json = r#"[
            {"symbol":"BTCUSDT","fundingTime":1700000000000,"fundingRate":"0.00010000"},
            {"symbol":"BTCUSDT","fundingTime":1700028800000,"fundingRate":"-0.00005000"}
        ]"#;
        let sched = parse_funding_rate(json).unwrap();
        assert_eq!(sched.len(), 2);
        // At FUNDING_RATE_SCALE (1e-8), Binance's 8-decimal rate is lossless:
        // 0.00010000 → 10_000 (= 1 bp); the sub-bp -0.00005000 → -5_000 (was 0 at bps).
        assert_eq!(
            sched[0],
            (Timestamp::from_nanos(1_700_000_000_000_000_000), 10_000)
        );
        assert_eq!(sched[1].1, -5_000);
    }

    #[test]
    fn open_interest_and_ratio_parse_and_scale() {
        let oi = r#"[{"symbol":"BTCUSDT","sumOpenInterest":"20403.637","sumOpenInterestValue":"1","timestamp":1700000000000}]"#;
        let pts = parse_open_interest(oi).unwrap();
        assert_eq!(pts[0].1, 20_403); // integer contracts (scale 0)

        let lsr = r#"[{"symbol":"BTCUSDT","longShortRatio":"1.8105","longAccount":"0.64","shortAccount":"0.36","timestamp":1700000000000}]"#;
        let pts = parse_long_short_ratio(lsr).unwrap();
        assert_eq!(pts[0].1, 18_105); // ratio × 10_000
    }

    #[test]
    fn signal_feed_streams_open_interest_in_time_order() {
        let body = r#"[
            {"symbol":"BTCUSDT","sumOpenInterest":"30","sumOpenInterestValue":"1","timestamp":1700000120000},
            {"symbol":"BTCUSDT","sumOpenInterest":"10","sumOpenInterestValue":"1","timestamp":1700000000000}
        ]"#;
        let mut feed = BinanceSignalFeed::open_interest(
            MockTransport::new(vec![HttpResponse {
                status: 200,
                body: body.to_string(),
            }]),
            FUTURES_BASE_URL,
            "BTCUSDT",
            InstrumentId::new(0),
            "5m",
            500,
        );
        // Sorted ascending by ts despite the out-of-order response.
        let Some(Event::Signal {
            value, channel, ts, ..
        }) = feed.next_event()
        else {
            panic!("expected signal");
        };
        assert_eq!(channel, signal_channel::OPEN_INTEREST);
        assert_eq!(value, 10);
        assert_eq!(ts.as_nanos(), 1_700_000_000_000_000_000);
        assert_eq!(
            feed.next_event().map(|e| e.ts().as_nanos()),
            Some(1_700_000_120_000_000_000)
        );
        assert!(feed.next_event().is_none());
    }

    #[test]
    fn signal_feed_returns_none_on_transport_error() {
        let mut feed = BinanceSignalFeed::long_short_ratio(
            MockTransport::new(vec![]), // no canned response → transport error
            FUTURES_BASE_URL,
            "BTCUSDT",
            InstrumentId::new(0),
            "5m",
            500,
        );
        assert!(feed.next_event().is_none());
        assert!(format!("{feed:?}").contains("BinanceSignalFeed"));
    }

    #[test]
    fn long_short_ratio_feed_streams_and_http_errors_are_none() {
        let body = r#"[{"symbol":"BTCUSDT","longShortRatio":"1.5","longAccount":"0.6","shortAccount":"0.4","timestamp":1700000000000}]"#;
        let mut feed = BinanceSignalFeed::long_short_ratio(
            MockTransport::new(vec![HttpResponse {
                status: 200,
                body: body.to_string(),
            }]),
            FUTURES_BASE_URL,
            "BTCUSDT",
            InstrumentId::new(0),
            "5m",
            500,
        );
        let Some(Event::Signal { channel, value, .. }) = feed.next_event() else {
            panic!("expected a signal");
        };
        assert_eq!(channel, signal_channel::LONG_SHORT_RATIO);
        assert_eq!(value, 15_000); // 1.5 × 10_000
        assert!(feed.next_event().is_none());

        // A non-2xx response yields no events.
        let mut err = BinanceSignalFeed::open_interest(
            MockTransport::new(vec![HttpResponse {
                status: 500,
                body: "x".into(),
            }]),
            FUTURES_BASE_URL,
            "BTCUSDT",
            InstrumentId::new(0),
            "5m",
            500,
        );
        assert!(err.next_event().is_none());
    }

    #[test]
    fn parse_helpers_reject_bad_json() {
        assert!(parse_funding_rate("not json").is_err());
        assert!(parse_open_interest("not json").is_err());
        assert!(parse_long_short_ratio("not json").is_err());
    }

    #[test]
    fn fetch_funding_history_builds_url() {
        let body =
            r#"[{"symbol":"BTCUSDT","fundingTime":1700000000000,"fundingRate":"0.00010000"}]"#;
        let mut t = MockTransport::new(vec![HttpResponse {
            status: 200,
            body: body.into(),
        }]);
        let sched = fetch_funding_history(&mut t, FUTURES_BASE_URL, "BTCUSDT", 1000).unwrap();
        assert_eq!(
            sched,
            vec![(Timestamp::from_nanos(1_700_000_000_000_000_000), 10_000)]
        );
        assert!(
            t.sent[0]
                .url
                .contains("/fapi/v1/fundingRate?symbol=BTCUSDT")
        );
        assert!(t.sent[0].api_key.is_none()); // public — unsigned
        let mut bad = MockTransport::new(vec![HttpResponse {
            status: 500,
            body: String::new(),
        }]);
        assert!(fetch_funding_history(&mut bad, FUTURES_BASE_URL, "BTCUSDT", 1000).is_err());
    }

    #[test]
    fn funding_history_paged_walks_forward() {
        // A full 1000-row first page forces a second (forward) request; the short
        // second page ends it. Verifies startTime advances past the newest seen.
        let period = 28_800_000i64; // 8h
        let start = 1_700_000_000_000i64;
        let rows: String = (0..1000)
            .map(|i| {
                format!(
                    r#"{{"symbol":"BTCUSDT","fundingTime":{},"fundingRate":"0.00010000"}}"#,
                    start + i * period
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let page1 = format!("[{rows}]");
        let newest1 = start + 999 * period;
        let page2 = format!(
            r#"[{{"symbol":"BTCUSDT","fundingTime":{},"fundingRate":"0.00020000"}}]"#,
            newest1 + period
        );
        let end = newest1 + 2 * period;
        let mut t = MockTransport::new(vec![
            HttpResponse {
                status: 200,
                body: page1,
            },
            HttpResponse {
                status: 200,
                body: page2,
            },
        ]);
        let sched =
            fetch_funding_history_paged(&mut t, FUTURES_BASE_URL, "BTCUSDT", start, end, 10)
                .unwrap();
        assert_eq!(sched.len(), 1001); // both pages, de-duplicated
        assert!(t.sent[0].url.contains(&format!("startTime={start}")));
        assert!(
            t.sent[1]
                .url
                .contains(&format!("startTime={}", newest1 + 1))
        ); // advanced
        let mut bad = MockTransport::new(vec![HttpResponse {
            status: 500,
            body: String::new(),
        }]);
        assert!(fetch_funding_history_paged(&mut bad, FUTURES_BASE_URL, "X", 0, 1, 10).is_err());
    }
}
