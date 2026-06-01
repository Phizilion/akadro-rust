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

/// The funding-rate decimal is normalized to **basis points** (scale 4): Binance
/// reports `"0.00010000"` for one funding period, i.e. 0.01% = 1 bp, which is the
/// granularity the engine's funding model charges (`amount = notional·rate/10000`).
pub const FUNDING_RATE_SCALE: u32 = 4;

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

/// Parse `/fapi/v1/fundingRate` history into a `(timestamp, rate_bps)` schedule
/// for `SimulatedExchange::with_funding_schedule` (in `akadro-backtest`). Rates
/// are normalized to bps ([`FUNDING_RATE_SCALE`]).
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
    fn funding_rate_normalizes_to_bps() {
        let json = r#"[
            {"symbol":"BTCUSDT","fundingTime":1700000000000,"fundingRate":"0.00010000"},
            {"symbol":"BTCUSDT","fundingTime":1700028800000,"fundingRate":"-0.00005000"}
        ]"#;
        let sched = parse_funding_rate(json).unwrap();
        assert_eq!(sched.len(), 2);
        assert_eq!(
            sched[0],
            (Timestamp::from_nanos(1_700_000_000_000_000_000), 1)
        ); // 1 bp
        assert_eq!(sched[1].1, 0); // -0.00005 truncates to 0 at bp granularity
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
}
