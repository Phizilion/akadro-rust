// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MEXC **futures** (contract.mexc.com) support: a distinct signer, the
//! `contract/detail` → [`InstrumentSpec`] mapping, and the order-field encoding.
//!
//! Futures signing differs from spot (decision: do not reuse the spot signer):
//! the headers are `ApiKey`, `Request-Time` (epoch-ms) and `Signature`, and the
//! signed string is `accessKey + requestTime + paramString` (the sorted query
//! string for GET, or the JSON body for POST), HMAC-SHA256 hex.

use std::collections::VecDeque;

use akadro_core::{
    Bar, CapSet, Capability, DataSource, Event, InstrumentId, InstrumentKind, InstrumentSpec,
    Money, OrderKind, OrderRequest, Price, Qty, Side, TimeInForce, Timestamp,
};
use serde::Deserialize;

use crate::convert::decimal_to_raw;
use crate::error::MexcError;
use crate::request::Method;
use crate::sign::sign;
use crate::transport::{HttpRequest, Transport};

/// Sign a MEXC futures request: `HMAC_SHA256(secret, accessKey + requestTime +
/// paramString)`, lowercase hex.
#[must_use]
pub fn sign_futures(
    access_key: &str,
    secret: &[u8],
    request_time_ms: i64,
    param_string: &str,
) -> String {
    sign(
        secret,
        &format!("{access_key}{request_time_ms}{param_string}"),
    )
}

/// MEXC futures order side/offset code: 1 open-long, 2 close-short, 3 open-short,
/// 4 close-long (derived from akadro [`Side`] + `reduce_only`).
#[must_use]
pub fn futures_side(side: Side, reduce_only: bool) -> i32 {
    match (side, reduce_only) {
        (Side::Buy, false) => 1,  // open long
        (Side::Sell, true) => 4,  // close long
        (Side::Sell, false) => 3, // open short
        (Side::Buy, true) => 2,   // close short
    }
}

/// MEXC futures order `type` code (1 limit/GTC, 2 post-only, 3 IOC, 4 FOK,
/// 5 market), or `None` if the kind is not expressible as a plain futures order.
#[must_use]
pub fn futures_type(order: &OrderRequest) -> Option<i32> {
    match (order.kind, order.tif) {
        // post-only applies only to a limit order; market + post_only is rejected
        // locally (None) rather than encoded as a post-only the venue would bounce.
        (OrderKind::Limit { .. }, _) if order.post_only => Some(2),
        (OrderKind::Market, _) => Some(5),
        (OrderKind::Limit { .. }, TimeInForce::Gtc) => Some(1),
        (OrderKind::Limit { .. }, TimeInForce::Ioc) => Some(3),
        (OrderKind::Limit { .. }, TimeInForce::Fok) => Some(4),
        _ => None, // stop/trigger orders use separate trigger fields
    }
}

#[derive(Deserialize)]
struct ContractDetailResponse {
    #[serde(default)]
    data: Vec<Contract>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Contract {
    symbol: String,
    base_coin: String,
    quote_coin: String,
    #[serde(default)]
    price_scale: u32,
    #[serde(default)]
    vol_scale: u32,
    // MEXC futures returns these as JSON NUMBERS (e.g. 0.1), unlike spot
    // exchangeInfo which uses strings — accept either.
    #[serde(default)]
    price_unit: serde_json::Value,
    #[serde(default)]
    vol_unit: serde_json::Value,
    #[serde(default)]
    state: i64,
    #[serde(default = "default_true")]
    api_allowed: bool,
}

fn default_true() -> bool {
    true
}

/// Convert a price/vol "unit" field (a JSON string like `"0.10"` or a number like
/// `0.1`) to a raw fixed-point integer at `scale`.
fn unit_to_raw(v: &serde_json::Value, scale: u32) -> i64 {
    let raw = match v {
        // Both arms go through the pure-integer `decimal_to_raw`: `Number::to_string`
        // preserves the original decimal text (e.g. "0.1"), so NO float arithmetic
        // touches tick/lot derivation (D12: no floats in the price/money path).
        serde_json::Value::String(s) => decimal_to_raw(s, scale).unwrap_or(1),
        serde_json::Value::Number(n) => decimal_to_raw(&n.to_string(), scale).unwrap_or(1),
        _ => 1,
    };
    raw.max(1)
}

/// One parsed futures contract: a perpetual [`InstrumentSpec`] plus its scales.
#[derive(Debug, Clone)]
pub struct FuturesContract {
    /// Venue symbol (e.g. `BTC_USDT`).
    pub symbol: String,
    /// Base asset name.
    pub base: String,
    /// Quote/settlement asset name.
    pub quote: String,
    /// Price fixed-point scale.
    pub price_scale: u32,
    /// Quantity fixed-point scale.
    pub qty_scale: u32,
    /// The akadro instrument spec (built once an [`InstrumentId`]/asset ids are
    /// assigned via [`FuturesContract::to_spec`]).
    pub tick_size: i64,
    /// Lot size (raw).
    pub lot_size: i64,
}

impl FuturesContract {
    /// Build an [`InstrumentSpec`] for this contract with the given dense ids.
    #[must_use]
    pub fn to_spec(
        &self,
        id: InstrumentId,
        base: akadro_core::AssetId,
        quote: akadro_core::AssetId,
    ) -> InstrumentSpec {
        InstrumentSpec::new(
            id,
            base,
            quote,
            InstrumentKind::PerpetualFuture,
            Price::from_raw(self.tick_size.max(1)),
            Qty::from_raw(self.lot_size.max(1)),
            Money::ZERO,
            CapSet::empty()
                .with(Capability::LimitOrders)
                .with(Capability::StopOrders)
                .with(Capability::PostOnly)
                .with(Capability::ReduceOnly)
                .with(Capability::Margin)
                .with(Capability::Funding)
                .with(Capability::ShortSelling),
        )
    }
}

/// Parse a futures `GET /api/v1/contract/detail` response into tradable
/// perpetual contracts (state 0 / api-allowed only).
pub fn parse_contract_detail(json: &str) -> Result<Vec<FuturesContract>, MexcError> {
    let resp: ContractDetailResponse =
        serde_json::from_str(json).map_err(|e| MexcError::Parse(e.to_string()))?;
    let mut out = Vec::new();
    for c in resp.data {
        if c.state != 0 || !c.api_allowed {
            continue;
        }
        let tick_size = unit_to_raw(&c.price_unit, c.price_scale);
        let lot_size = unit_to_raw(&c.vol_unit, c.vol_scale);
        out.push(FuturesContract {
            symbol: c.symbol,
            base: c.base_coin,
            quote: c.quote_coin,
            price_scale: c.price_scale,
            qty_scale: c.vol_scale,
            tick_size,
            lot_size,
        });
    }
    Ok(out)
}

// --- futures kline DataSource ------------------------------------------------

/// MEXC futures base URL (the contract API host, distinct from the spot host).
pub const FUTURES_BASE_URL: &str = "https://contract.mexc.com";

/// Map an akadro interval string (`"1m"`, `"4h"`, …) to the MEXC **futures**
/// kline token (`"Min1"`, `"Hour4"`, …), or `None` if unrecognized.
#[must_use]
pub fn futures_interval(interval: &str) -> Option<&'static str> {
    Some(match interval {
        "1m" => "Min1",
        "5m" => "Min5",
        "15m" => "Min15",
        "30m" => "Min30",
        "60m" | "1h" => "Min60",
        "4h" => "Hour4",
        "8h" => "Hour8",
        "1d" => "Day1",
        "1W" => "Week1",
        "1M" => "Month1",
        _ => return None,
    })
}

/// One bar length in **seconds** for an akadro interval, or `0` if unrecognized.
fn futures_interval_secs(interval: &str) -> i64 {
    match interval {
        "1m" => 60,
        "5m" => 300,
        "15m" => 900,
        "30m" => 1_800,
        "60m" | "1h" => 3_600,
        "4h" => 14_400,
        "8h" => 28_800,
        "1d" => 86_400,
        "1W" => 604_800,
        "1M" => 2_592_000,
        _ => 0,
    }
}

#[derive(Deserialize)]
struct FuturesKlineResp {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<FuturesKlineData>,
}

#[derive(Deserialize)]
struct FuturesKlineData {
    time: Vec<i64>,
    open: Vec<serde_json::Number>,
    high: Vec<serde_json::Number>,
    low: Vec<serde_json::Number>,
    close: Vec<serde_json::Number>,
    vol: Vec<serde_json::Number>,
}

/// Parse a MEXC futures `GET /api/v1/contract/kline/{symbol}` response (columnar
/// arrays) into [`Bar`]s. The contract API reports each bar's **open** time
/// (`time`, epoch-seconds); akadro — like spot REST/WS and the `HistoricalFeed` —
/// stamps bars at **close** time, so one interval is added (m20). The interval is
/// derived from consecutive opens (constant within a kline series); a single-bar
/// response cannot derive it and is left at open time. Numeric prices/volumes are
/// read via their exact decimal text, so no `f64` enters the money path.
///
/// # Errors
/// [`MexcError::Parse`] on unparseable JSON, an unsuccessful payload, mismatched
/// column lengths, a time overflow, or a value that does not fit the scale.
pub fn parse_futures_klines(
    json: &str,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
) -> Result<Vec<Bar>, MexcError> {
    let resp: FuturesKlineResp =
        serde_json::from_str(json).map_err(|e| MexcError::Parse(e.to_string()))?;
    if !resp.success {
        return Err(MexcError::Parse("contract/kline success=false".into()));
    }
    let Some(d) = resp.data else {
        return Ok(Vec::new());
    };
    let n = d.time.len();
    if [
        d.open.len(),
        d.high.len(),
        d.low.len(),
        d.close.len(),
        d.vol.len(),
    ]
    .iter()
    .any(|&l| l != n)
    {
        return Err(MexcError::Parse(
            "contract/kline column length mismatch".into(),
        ));
    }
    let px =
        |v: &serde_json::Number| decimal_to_raw(&v.to_string(), price_scale).map(Price::from_raw);
    let qt = |v: &serde_json::Number| decimal_to_raw(&v.to_string(), qty_scale).map(Qty::from_raw);
    // Interval (seconds) from consecutive opens, so each bar can be stamped at its
    // close time = open + interval (m20). 0 for a single-bar response (left at open).
    let interval_secs = if n >= 2 { d.time[1] - d.time[0] } else { 0 };
    let mut bars = Vec::with_capacity(n);
    for i in 0..n {
        let close_secs = d.time[i]
            .checked_add(interval_secs)
            .ok_or_else(|| MexcError::Parse("contract/kline time overflow".into()))?;
        let ts = Timestamp::from_nanos(
            close_secs
                .checked_mul(1_000_000_000)
                .ok_or_else(|| MexcError::Parse("contract/kline time overflow".into()))?,
        );
        bars.push(Bar::new(
            instrument,
            ts,
            px(&d.open[i])?,
            px(&d.high[i])?,
            px(&d.low[i])?,
            px(&d.close[i])?,
            qt(&d.vol[i])?,
        ));
    }
    Ok(bars)
}

/// A [`DataSource`] streaming MEXC **futures** (perpetual) klines for one symbol —
/// the futures analogue of [`MexcKlineFeed`](crate::MexcKlineFeed). It targets the
/// contract `kline` endpoint, reuses the futures interval mapping, and emits
/// standard [`Event::Bar`]s, so the engine and strategies are unchanged. Pair with
/// a `PerpetualFuture` [`InstrumentSpec`] from [`FuturesContract::to_spec`] and
/// cache through the data layer.
pub struct MexcFuturesKlineFeed<T> {
    transport: T,
    base_url: String,
    symbol: String,
    instrument: InstrumentId,
    interval: String,
    price_scale: u32,
    qty_scale: u32,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    buffer: VecDeque<Bar>,
    fetched: bool,
}

impl<T: Transport> MexcFuturesKlineFeed<T> {
    /// Create a futures feed for the contract `symbol` (e.g. `"BTC_USDT"`) at the
    /// akadro `interval` (`"1m"`…, mapped to MEXC's `Min1`/`Hour4`/… token).
    /// `price_scale`/`qty_scale` come from the futures catalogue
    /// ([`FuturesContract::to_spec`]).
    #[must_use]
    pub fn new(
        transport: T,
        base_url: impl Into<String>,
        symbol: impl Into<String>,
        instrument: InstrumentId,
        interval: impl Into<String>,
        price_scale: u32,
        qty_scale: u32,
    ) -> Self {
        MexcFuturesKlineFeed {
            transport,
            base_url: base_url.into(),
            symbol: symbol.into(),
            instrument,
            interval: interval.into(),
            price_scale,
            qty_scale,
            start_ms: None,
            end_ms: None,
            buffer: VecDeque::new(),
            fetched: false,
        }
    }

    /// Restrict to `[start_ms, end_ms)` (epoch-ms) and back-fill the window,
    /// paginating across the venue's per-call cap.
    #[must_use]
    pub fn with_range(mut self, start_ms: i64, end_ms: i64) -> Self {
        self.start_ms = Some(start_ms);
        self.end_ms = Some(end_ms);
        self
    }

    fn fetch_page(
        &mut self,
        start_s: Option<i64>,
        end_s: Option<i64>,
    ) -> Result<Vec<Bar>, MexcError> {
        use core::fmt::Write as _;
        let Some(iv) = futures_interval(&self.interval) else {
            return Err(MexcError::Parse(format!(
                "unsupported futures interval '{}'",
                self.interval
            )));
        };
        let mut url = format!(
            "{}/api/v1/contract/kline/{}?interval={}",
            self.base_url, self.symbol, iv
        );
        if let Some(s) = start_s {
            let _ = write!(url, "&start={s}");
        }
        if let Some(e) = end_s {
            let _ = write!(url, "&end={e}");
        }
        let req = HttpRequest {
            method: Method::Get,
            url,
            api_key: None,
            body: None,
        };
        let resp = self.transport.send(&req)?;
        if !resp.is_success() {
            return Err(MexcError::Transport(format!(
                "contract/kline HTTP {}",
                resp.status
            )));
        }
        parse_futures_klines(
            &resp.body,
            self.instrument,
            self.price_scale,
            self.qty_scale,
        )
    }

    fn backfill(&mut self, start_ms: i64, end_ms: i64) -> Result<Vec<Bar>, MexcError> {
        let bar_secs = futures_interval_secs(&self.interval).max(1);
        let end_secs = end_ms / 1000;
        let mut cursor_secs = start_ms / 1000;
        let mut all: Vec<Bar> = Vec::new();
        while cursor_secs < end_secs {
            let page = self.fetch_page(Some(cursor_secs), Some(end_secs))?;
            if page.is_empty() {
                break;
            }
            let last_secs = page.last().expect("non-empty").ts.as_nanos() / 1_000_000_000;
            all.extend(page);
            let next = last_secs + bar_secs;
            if next <= cursor_secs {
                break; // no forward progress
            }
            cursor_secs = next;
        }
        Ok(all)
    }

    fn fetch(&mut self) -> Result<(), MexcError> {
        let bars = match (self.start_ms, self.end_ms) {
            (Some(s), Some(e)) => self.backfill(s, e)?,
            _ => self.fetch_page(None, None)?, // most-recent window
        };
        self.buffer.extend(bars);
        Ok(())
    }
}

impl<T: Transport> DataSource for MexcFuturesKlineFeed<T> {
    fn next_event(&mut self) -> Option<Event> {
        if !self.fetched {
            self.fetched = true;
            if self.fetch().is_err() {
                return None;
            }
        }
        self.buffer.pop_front().map(Event::Bar)
    }
}

impl<T> core::fmt::Debug for MexcFuturesKlineFeed<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MexcFuturesKlineFeed")
            .field("symbol", &self.symbol)
            .field("interval", &self.interval)
            .field("buffered", &self.buffer.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::AssetId;

    #[test]
    fn futures_signer_is_concat_hmac() {
        let got = sign_futures("KEY", b"secret", 1_700_000_000_000, "symbol=BTC_USDT");
        let expected = sign(b"secret", "KEY1700000000000symbol=BTC_USDT");
        assert_eq!(got, expected);
    }

    #[test]
    fn side_offset_mapping() {
        assert_eq!(futures_side(Side::Buy, false), 1);
        assert_eq!(futures_side(Side::Sell, false), 3);
        assert_eq!(futures_side(Side::Buy, true), 2);
        assert_eq!(futures_side(Side::Sell, true), 4);
    }

    #[test]
    fn type_mapping() {
        let i = InstrumentId::new(0);
        assert_eq!(
            futures_type(&OrderRequest::market(i, Side::Buy, Qty::from_raw(1))),
            Some(5)
        );
        assert_eq!(
            futures_type(&OrderRequest::limit(
                i,
                Side::Buy,
                Qty::from_raw(1),
                Price::from_raw(1)
            )),
            Some(1)
        );
        assert_eq!(
            futures_type(
                &OrderRequest::limit(i, Side::Buy, Qty::from_raw(1), Price::from_raw(1))
                    .with_tif(TimeInForce::Ioc)
            ),
            Some(3)
        );
        assert_eq!(
            futures_type(
                &OrderRequest::limit(i, Side::Buy, Qty::from_raw(1), Price::from_raw(1))
                    .with_tif(TimeInForce::Fok)
            ),
            Some(4)
        );
        assert_eq!(
            futures_type(
                &OrderRequest::limit(i, Side::Buy, Qty::from_raw(1), Price::from_raw(1))
                    .post_only()
            ),
            Some(2)
        );
    }

    // priceUnit/volUnit are JSON NUMBERS, as the real futures API returns them.
    const DETAIL: &str = r#"{"success":true,"code":0,"data":[
      {"symbol":"BTC_USDT","baseCoin":"BTC","quoteCoin":"USDT","priceScale":2,"volScale":0,
       "priceUnit":0.10,"volUnit":1,"state":0,"apiAllowed":true,"maxLeverage":125},
      {"symbol":"DEAD_USDT","baseCoin":"DEAD","quoteCoin":"USDT","priceScale":2,"volScale":0,
       "priceUnit":0.01,"volUnit":1,"state":3,"apiAllowed":true}
    ]}"#;

    #[test]
    fn parse_contracts_filters_and_maps() {
        let contracts = parse_contract_detail(DETAIL).unwrap();
        assert_eq!(contracts.len(), 1); // DEAD (state 3) excluded
        let c = &contracts[0];
        assert_eq!(c.symbol, "BTC_USDT");
        assert_eq!(c.price_scale, 2);
        assert_eq!(c.tick_size, 10); // 0.10 at scale 2 = 10 raw
        assert_eq!(c.lot_size, 1);

        let spec = c.to_spec(InstrumentId::new(0), AssetId::new(0), AssetId::new(1));
        assert_eq!(spec.kind, InstrumentKind::PerpetualFuture);
        assert!(spec.caps.contains(Capability::Funding));
        assert!(spec.caps.contains(Capability::Margin));
        assert_eq!(spec.tick_size, Price::from_raw(10));
    }

    #[test]
    fn parse_contracts_rejects_garbage() {
        assert!(parse_contract_detail("nope").is_err());
        assert!(parse_contract_detail("{}").unwrap().is_empty());
    }
}

#[cfg(test)]
mod cov_tests {
    use super::*;
    use akadro_core::{InstrumentId, Price, Qty, Side, TriggerBy};

    #[test]
    fn futures_type_none_for_stop() {
        let o = OrderRequest::stop(
            InstrumentId::new(0),
            Side::Buy,
            Qty::from_raw(1),
            Price::from_raw(100),
            TriggerBy::Last,
        );
        assert_eq!(futures_type(&o), None);
    }

    #[test]
    fn contract_detail_string_and_null_units() {
        // string priceUnit (String branch), a contract with no priceUnit (Null -> 1),
        // and a missing apiAllowed (defaults true).
        let json = r#"{"data":[
          {"symbol":"ETH_USDT","baseCoin":"ETH","quoteCoin":"USDT","priceScale":2,"volScale":0,"priceUnit":"0.05","volUnit":"1","state":0},
          {"symbol":"SOL_USDT","baseCoin":"SOL","quoteCoin":"USDT","priceScale":3,"volScale":0,"state":0,"apiAllowed":true}
        ]}"#;
        let cs = parse_contract_detail(json).unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].tick_size, 5); // 0.05 @ scale 2
        assert_eq!(cs[1].tick_size, 1); // null priceUnit -> 1
    }
}

#[cfg(test)]
mod feed_tests {
    use super::*;
    use crate::transport::HttpResponse;

    const KLINES: &str = r#"{"success":true,"code":0,"data":{
        "time":[1700000000,1700000060],
        "open":[100.5,101.0],"high":[102,103],"low":[99,100.25],
        "close":[101,102.5],"vol":[10,20],"amount":[1,2]}}"#;

    struct Canned(Vec<String>);
    impl Transport for Canned {
        fn send(&mut self, _req: &HttpRequest) -> Result<HttpResponse, MexcError> {
            let body = if self.0.is_empty() {
                String::new()
            } else {
                self.0.remove(0)
            };
            Ok(HttpResponse { status: 200, body })
        }
    }

    #[test]
    fn interval_token_mapping() {
        assert_eq!(futures_interval("1m"), Some("Min1"));
        assert_eq!(futures_interval("1h"), Some("Min60"));
        assert_eq!(futures_interval("4h"), Some("Hour4"));
        assert_eq!(futures_interval("1M"), Some("Month1"));
        assert_eq!(futures_interval("13s"), None);
        assert_eq!(futures_interval_secs("4h"), 14_400);
        assert_eq!(futures_interval_secs("nope"), 0);
    }

    #[test]
    fn parse_columnar_klines() {
        let bars = parse_futures_klines(KLINES, InstrumentId::new(0), 2, 0).unwrap();
        assert_eq!(bars.len(), 2);
        // Stamped at CLOSE time = open + interval (60s here), matching spot/WS/feed
        // (m20): open 1700000000 + 60 → 1700000060s.
        assert_eq!(bars[0].ts.as_nanos(), 1_700_000_060_000_000_000);
        assert_eq!(bars[1].ts.as_nanos(), 1_700_000_120_000_000_000);
        assert_eq!(bars[0].open.raw(), 10050); // 100.5 @ scale 2
        assert_eq!(bars[0].high.raw(), 10200); // 102
        assert_eq!(bars[0].low.raw(), 9900); // 99
        assert_eq!(bars[0].close.raw(), 10100); // 101
        assert_eq!(bars[0].volume.raw(), 10);
        assert_eq!(bars[1].open.raw(), 10100); // 101.0
        assert_eq!(bars[1].low.raw(), 10025); // 100.25
        assert_eq!(bars[1].close.raw(), 10250); // 102.5
    }

    #[test]
    fn parse_error_paths() {
        assert!(parse_futures_klines(r#"{"success":false}"#, InstrumentId::new(0), 2, 0).is_err());
        // success but no data → empty.
        assert!(
            parse_futures_klines(r#"{"success":true}"#, InstrumentId::new(0), 2, 0)
                .unwrap()
                .is_empty()
        );
        // column-length mismatch.
        let bad = r#"{"success":true,"data":{"time":[1],"open":[1,2],"high":[1],"low":[1],"close":[1],"vol":[1]}}"#;
        assert!(parse_futures_klines(bad, InstrumentId::new(0), 2, 0).is_err());
        assert!(parse_futures_klines("not json", InstrumentId::new(0), 2, 0).is_err());
    }

    #[test]
    fn feed_streams_bars_no_range() {
        let mut feed = MexcFuturesKlineFeed::new(
            Canned(vec![KLINES.to_string()]),
            "http://x",
            "BTC_USDT",
            InstrumentId::new(0),
            "1m",
            2,
            0,
        );
        assert!(feed.next_event().is_some());
        assert!(feed.next_event().is_some());
        assert!(feed.next_event().is_none()); // buffer drained
    }

    #[test]
    fn feed_backfills_paginated_range() {
        let empty = r#"{"success":true,"data":{"time":[],"open":[],"high":[],"low":[],"close":[],"vol":[]}}"#;
        let mut feed = MexcFuturesKlineFeed::new(
            Canned(vec![KLINES.to_string(), empty.to_string()]),
            "http://x",
            "BTC_USDT",
            InstrumentId::new(0),
            "1m",
            2,
            0,
        )
        .with_range(1_700_000_000_000, 1_700_000_200_000);
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 2); // page 1 yields 2 bars; the empty page 2 terminates backfill
    }

    #[test]
    fn feed_unsupported_interval_yields_nothing() {
        let mut feed = MexcFuturesKlineFeed::new(
            Canned(vec![KLINES.to_string()]),
            "http://x",
            "BTC_USDT",
            InstrumentId::new(0),
            "13s",
            2,
            0,
        );
        assert!(feed.next_event().is_none()); // fetch errors → None
    }
}
