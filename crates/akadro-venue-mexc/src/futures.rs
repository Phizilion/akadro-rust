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
    futures_interval_entry(interval).map(|(token, _)| token)
}

/// The single source of truth for the MEXC futures interval mapping:
/// `(kline token, bar seconds)` for an akadro interval, or `None` if unrecognized.
/// `futures_interval` and `futures_interval_secs` both derive from it (DRY) so the
/// token and the duration can never disagree.
fn futures_interval_entry(interval: &str) -> Option<(&'static str, i64)> {
    Some(match interval {
        "1m" => ("Min1", 60),
        "5m" => ("Min5", 300),
        "15m" => ("Min15", 900),
        "30m" => ("Min30", 1_800),
        "60m" | "1h" => ("Min60", 3_600),
        "4h" => ("Hour4", 14_400),
        "8h" => ("Hour8", 28_800),
        "1d" => ("Day1", 86_400),
        "1W" => ("Week1", 604_800),
        "1M" => ("Month1", 2_592_000),
        _ => return None,
    })
}

/// One bar length in **seconds** for an akadro interval, or `0` if unrecognized.
fn futures_interval_secs(interval: &str) -> i64 {
    futures_interval_entry(interval).map_or(0, |(_, secs)| secs)
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
/// stamps bars at **close** time, so one interval is added (m20). `interval_secs` is
/// the authoritative bar length (from the requested interval); it is used as the
/// close offset so even a **single-bar** response is correctly close-stamped (m32) —
/// not left at open time as deriving the interval from consecutive opens would force.
/// As a fallback (`interval_secs <= 0`, an unrecognized interval) the interval is
/// derived from consecutive opens, and a lone bar is left at open. Numeric
/// prices/volumes are read via their exact decimal text, so no `f64` enters the
/// money path.
///
/// # Errors
/// [`MexcError::Parse`] on unparseable JSON, an unsuccessful payload, mismatched
/// column lengths, a time overflow, or a value that does not fit the scale.
pub fn parse_futures_klines(
    json: &str,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
    interval_secs: i64,
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
    // Close offset (seconds): the caller's authoritative interval, so each bar —
    // including a lone single-bar page — is stamped at close = open + interval (m20,
    // m32). Fall back to deriving it from consecutive opens only when the caller
    // didn't supply one (unrecognized interval); a single bar then stays at open.
    let interval_secs = if interval_secs > 0 {
        interval_secs
    } else if n >= 2 {
        d.time[1] - d.time[0]
    } else {
        0
    };
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

// --- futures funding-rate history --------------------------------------------

/// Funding-rate fixed-point scale — re-exported from `akadro-core` so the connector
/// and the engine's `mul_rate` charge cannot drift. Rates normalize to `1e-8`
/// fractions: MEXC's `0.0001` (1 bp) → `10_000`, and a sub-bp `0.0000466` → `4_660`
/// instead of rounding to `0`. See `SimulatedExchange::with_funding_schedule`.
pub use akadro_core::FUNDING_RATE_SCALE;

#[derive(Deserialize)]
struct FuturesFundingResp {
    #[serde(default)]
    success: bool,
    data: Option<FuturesFundingData>,
}
#[derive(Deserialize)]
struct FuturesFundingData {
    #[serde(rename = "resultList", default)]
    result_list: Vec<FuturesFundingRow>,
}
#[derive(Deserialize)]
struct FuturesFundingRow {
    #[serde(rename = "fundingRate")]
    funding_rate: serde_json::Value,
    #[serde(rename = "settleTime")]
    settle_time: i64,
}

/// Convert a JSON funding rate (the contract API returns a number, but a string is
/// accepted too) to a raw fixed-point value at [`FUNDING_RATE_SCALE`]. A JSON number
/// is rendered to a fixed (non-exponential) decimal first, so a tiny sub-bp rate is
/// not emitted as `1e-7` and no `f64` reaches the money path beyond this single
/// venue-data boundary parse.
fn funding_rate_to_raw(v: &serde_json::Value) -> Result<i64, MexcError> {
    match v {
        serde_json::Value::String(s) => decimal_to_raw(s, FUNDING_RATE_SCALE),
        serde_json::Value::Number(n) => {
            let f = n
                .as_f64()
                .ok_or_else(|| MexcError::Parse("funding rate not numeric".into()))?;
            decimal_to_raw(&format!("{f:.18}"), FUNDING_RATE_SCALE)
        }
        other => Err(MexcError::Parse(format!("bad fundingRate {other}"))),
    }
}

/// Parse a MEXC futures `GET /api/v1/contract/funding_rate/history` body into an
/// ascending `(timestamp, rate)` schedule for `SimulatedExchange::with_funding_schedule`.
/// Rates are at [`FUNDING_RATE_SCALE`] (`1e-8`), so MEXC's sub-bp rates are preserved
/// rather than rounded to `0`. `settleTime` is epoch-ms; the page is re-sorted
/// ascending (MEXC returns it newest-first).
///
/// # Errors
/// [`MexcError::Parse`] on bad JSON, an unsuccessful payload, a bad rate, or a
/// timestamp overflow.
pub fn parse_funding_rate(json: &str) -> Result<Vec<(Timestamp, i64)>, MexcError> {
    let resp: FuturesFundingResp =
        serde_json::from_str(json).map_err(|e| MexcError::Parse(e.to_string()))?;
    if !resp.success {
        return Err(MexcError::Parse(
            "contract/funding_rate success=false".into(),
        ));
    }
    let Some(d) = resp.data else {
        return Ok(Vec::new());
    };
    let mut out: Vec<(Timestamp, i64)> = d
        .result_list
        .iter()
        .map(|r| {
            let ns = r
                .settle_time
                .checked_mul(1_000_000)
                .ok_or_else(|| MexcError::Parse("settleTime overflow".into()))?;
            Ok((
                Timestamp::from_nanos(ns),
                funding_rate_to_raw(&r.funding_rate)?,
            ))
        })
        .collect::<Result<_, MexcError>>()?;
    out.sort_by_key(|(t, _)| t.as_nanos());
    Ok(out)
}

/// Fetch `/api/v1/contract/funding_rate/history` for the contract `symbol` (e.g.
/// `"BTC_USDT"`) over `transport` (use [`FUTURES_BASE_URL`]) and parse it into a
/// `with_funding_schedule` schedule (rates at [`FUNDING_RATE_SCALE`]). `limit` caps
/// the page size (MEXC's cap is 100). A public endpoint — no signing. The reusable
/// connector entry point — callers never build the URL or touch the body.
///
/// # Errors
/// [`MexcError::Transport`] on a transport failure or a non-2xx status;
/// [`MexcError::Parse`] on malformed JSON.
pub fn fetch_funding_history<T: Transport>(
    transport: &mut T,
    base_url: &str,
    symbol: &str,
    limit: u32,
) -> Result<Vec<(Timestamp, i64)>, MexcError> {
    let resp = transport.send(&HttpRequest {
        method: Method::Get,
        url: format!(
            "{base_url}/api/v1/contract/funding_rate/history?symbol={symbol}&page_num=1&page_size={}",
            limit.min(100)
        ),
        api_key: None,
        body: None,
    })?;
    if !resp.is_success() {
        return Err(MexcError::Transport(format!(
            "contract/funding_rate HTTP {}",
            resp.status
        )));
    }
    parse_funding_rate(&resp.body)
}

/// Page `/api/v1/contract/funding_rate/history` for `symbol` (100 settlements/page,
/// newest-first) up to `max_pages` and concatenate into one ascending
/// [`FUNDING_RATE_SCALE`] schedule — one page is ≈33 days at the 8h cadence, so a
/// multi-page fetch covers a long backtest window. Stops early on an empty page;
/// de-duplicates by settlement time. A public endpoint — no signing. The reusable
/// connector entry point — callers never build the URL or touch the body.
///
/// # Errors
/// [`MexcError::Transport`] on a transport failure or a non-2xx status;
/// [`MexcError::Parse`] on malformed JSON.
pub fn fetch_funding_history_paged<T: Transport>(
    transport: &mut T,
    base_url: &str,
    symbol: &str,
    max_pages: u32,
) -> Result<Vec<(Timestamp, i64)>, MexcError> {
    let mut all: Vec<(Timestamp, i64)> = Vec::new();
    for page_num in 1..=max_pages.max(1) {
        let resp = transport.send(&HttpRequest {
            method: Method::Get,
            url: format!(
                "{base_url}/api/v1/contract/funding_rate/history?symbol={symbol}&page_num={page_num}&page_size=100"
            ),
            api_key: None,
            body: None,
        })?;
        if !resp.is_success() {
            return Err(MexcError::Transport(format!(
                "contract/funding_rate HTTP {}",
                resp.status
            )));
        }
        let page = parse_funding_rate(&resp.body)?;
        if page.is_empty() {
            break;
        }
        all.extend(page);
    }
    all.sort_by_key(|(t, _)| t.as_nanos());
    all.dedup_by_key(|(t, _)| t.as_nanos());
    Ok(all)
}

/// Fetch `/api/v1/contract/detail` (public) over `transport` and parse it into the
/// tradable perpetual contracts ([`parse_contract_detail`]). The reusable connector
/// entry point — callers never build the URL or touch the body.
///
/// # Errors
/// [`MexcError::Transport`] on a transport failure or a non-2xx status;
/// [`MexcError::Parse`] on malformed JSON.
pub fn fetch_contracts<T: Transport>(
    transport: &mut T,
    base_url: &str,
) -> Result<Vec<FuturesContract>, MexcError> {
    let resp = transport.send(&HttpRequest {
        method: Method::Get,
        url: format!("{base_url}/api/v1/contract/detail"),
        api_key: None,
        body: None,
    })?;
    if !resp.is_success() {
        return Err(MexcError::Transport(format!(
            "contract/detail HTTP {}",
            resp.status
        )));
    }
    parse_contract_detail(&resp.body)
}

/// A [`DataSource`] streaming MEXC **futures** (perpetual) klines for one symbol —
/// the futures analogue of [`MexcKlineFeed`](crate::MexcKlineFeed). It targets the
/// contract `kline` endpoint, reuses the futures interval mapping, and emits
/// standard [`Event::Bar`]s, so the engine and strategies are unchanged. Pair with
/// a `PerpetualFuture` [`InstrumentSpec`] from [`FuturesContract::to_spec`] and
/// cache through the data layer.
///
/// **Unclosed last bar (no-range mode).** Without [`with_range`](Self::with_range)
/// the feed fetches the most-recent window, whose last bar is the **in-progress**
/// (still-forming) candle — its OHLCV mutates and its close time is in the future.
/// A bounded [`with_range`](Self::with_range) back-fill ending before the present
/// yields only closed bars; prefer it for reproducible runs, or drop the final bar
/// of the no-range pull. (The contract API carries no per-bar "closed" flag here.)
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
    /// Courtesy delay between back-fill pages (rate-limit politeness) and the unit of
    /// the bounded 429 back-off. `0` in tests; matters for finer intervals over long
    /// windows, which take many pages.
    page_delay: std::time::Duration,
    /// Bounded retries on a `429` (rate-limited) page before giving up.
    max_retries: u32,
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
            page_delay: std::time::Duration::from_millis(120),
            max_retries: 8,
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

    /// Override the inter-page courtesy delay (default 120 ms), which also sets the
    /// 429 back-off unit. Set to [`Duration::ZERO`](std::time::Duration::ZERO) in
    /// tests to page without sleeping. Lower it / raise it to trade download speed
    /// against the venue's rate limit on long, fine-interval back-fills.
    #[must_use]
    pub fn with_page_delay(mut self, delay: std::time::Duration) -> Self {
        self.page_delay = delay;
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
        // Bounded 429 back-off (a zero `page_delay`, i.e. tests, means zero back-off
        // and an immediate retry); other non-2xx statuses surface as an error.
        let backoff = if self.page_delay.is_zero() {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_secs(2)
        };
        let mut retries = 0u32;
        loop {
            let req = HttpRequest {
                method: Method::Get,
                url: url.clone(),
                api_key: None,
                body: None,
            };
            let resp = self.transport.send(&req)?;
            if resp.status == 429 && retries < self.max_retries {
                retries += 1;
                if !backoff.is_zero() {
                    std::thread::sleep(backoff);
                }
                continue;
            }
            if !resp.is_success() {
                return Err(MexcError::Transport(format!(
                    "contract/kline HTTP {}",
                    resp.status
                )));
            }
            return parse_futures_klines(
                &resp.body,
                self.instrument,
                self.price_scale,
                self.qty_scale,
                futures_interval_secs(&self.interval),
            );
        }
    }

    /// Back-fill `[start_ms, end_ms)` by paging the contract-kline endpoint
    /// **newest→oldest**. MEXC caps each response at ~2000 bars anchored on `end` and
    /// **ignores `start`** once the range exceeds that cap, so a forward cursor never
    /// advances; instead we move the `end` cursor back to just before the oldest bar
    /// received and repeat until a page reaches `start`. Bars are close-stamped; the
    /// result is sorted ascending, de-duplicated, and clipped to `[start, end)` on
    /// open time.
    fn backfill(&mut self, start_ms: i64, end_ms: i64) -> Result<Vec<Bar>, MexcError> {
        let bar_secs = futures_interval_secs(&self.interval).max(1);
        let start_secs = start_ms / 1000;
        let mut cursor_end = end_ms / 1000;
        let mut all: Vec<Bar> = Vec::new();
        loop {
            let page = self.fetch_page(Some(start_secs), Some(cursor_end))?;
            if page.is_empty() {
                break;
            }
            // `ts` is close time; open = close - interval. The oldest open seen drives
            // the next (earlier) `end` cursor.
            let oldest_close_secs = page
                .iter()
                .map(|b| b.ts.as_nanos() / 1_000_000_000)
                .min()
                .expect("non-empty");
            let oldest_open_secs = oldest_close_secs - bar_secs;
            all.extend(page);
            if oldest_open_secs <= start_secs {
                break;
            }
            let next_end = oldest_open_secs - bar_secs;
            if next_end >= cursor_end {
                break; // no backward progress (guards a venue that ignores the cursor)
            }
            cursor_end = next_end;
            if !self.page_delay.is_zero() {
                std::thread::sleep(self.page_delay); // pace long, fine-interval back-fills
            }
        }
        all.sort_by_key(|b| b.ts.as_nanos());
        all.dedup_by_key(|b| b.ts.as_nanos());
        let bar_ms = bar_secs * 1000;
        all.retain(|b| {
            let open_ms = b.ts.as_nanos() / 1_000_000 - bar_ms;
            open_ms >= start_ms && open_ms < end_ms
        });
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

    /// Build a columnar contract-kline body (flat OHLC=100, vol=1) for the given
    /// **open** seconds — a test page for the backward-paging back-fill.
    fn fut_cols(opens: &[i64]) -> String {
        let times = opens
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let ones = vec!["100"; opens.len()].join(",");
        let vols = vec!["1"; opens.len()].join(",");
        format!(
            r#"{{"success":true,"data":{{"time":[{times}],"open":[{ones}],"high":[{ones}],"low":[{ones}],"close":[{ones}],"vol":[{vols}]}}}}"#
        )
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
        let bars = parse_futures_klines(KLINES, InstrumentId::new(0), 2, 0, 60).unwrap();
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
        assert!(
            parse_futures_klines(r#"{"success":false}"#, InstrumentId::new(0), 2, 0, 60).is_err()
        );
        // success but no data → empty.
        assert!(
            parse_futures_klines(r#"{"success":true}"#, InstrumentId::new(0), 2, 0, 60)
                .unwrap()
                .is_empty()
        );
        // column-length mismatch.
        let bad = r#"{"success":true,"data":{"time":[1],"open":[1,2],"high":[1],"low":[1],"close":[1],"vol":[1]}}"#;
        assert!(parse_futures_klines(bad, InstrumentId::new(0), 2, 0, 60).is_err());
        assert!(parse_futures_klines("not json", InstrumentId::new(0), 2, 0, 60).is_err());
    }

    #[test]
    fn single_bar_is_close_stamped_from_passed_interval() {
        // m32: a single-bar response can't derive the interval from consecutive opens.
        // The passed interval (60s) must still close-stamp it (open 1700000000 + 60),
        // not leave it at open time — otherwise a lone page stitches inconsistently
        // with multi-bar pages.
        let one = r#"{"success":true,"data":{"time":[1700000000],"open":[100],"high":[100],"low":[100],"close":[100],"vol":[1]}}"#;
        let bars = parse_futures_klines(one, InstrumentId::new(0), 2, 0, 60).unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(
            bars[0].ts.as_nanos(),
            1_700_000_060_000_000_000,
            "single bar stamped at close = open + interval"
        );
        // With no interval supplied (0) and a single bar, it falls back to open time.
        let fallback = parse_futures_klines(one, InstrumentId::new(0), 2, 0, 0).unwrap();
        assert_eq!(fallback[0].ts.as_nanos(), 1_700_000_000_000_000_000);
    }

    #[test]
    fn funding_rate_parses_numbers_to_fine_scale_sorted() {
        // The contract API returns fundingRate as a JSON NUMBER, settleTime as ms.
        // Newest-first; re-sorted ascending.
        let json = r#"{"success":true,"data":{"resultList":[
            {"symbol":"BTC_USDT","fundingRate":0.0001,"settleTime":1700028800000,"collectCycle":8},
            {"symbol":"BTC_USDT","fundingRate":-0.0000466,"settleTime":1700000000000,"collectCycle":8}
        ]}}"#;
        let sched = parse_funding_rate(json).unwrap();
        assert_eq!(sched.len(), 2);
        // At FUNDING_RATE_SCALE (1e-8): 0.0001 → 10_000 (= 1 bp); the sub-bp number
        // -0.0000466 → -4_660, not rounded to 0 (and not mangled into 1e-N text).
        assert_eq!(
            sched[0],
            (Timestamp::from_nanos(1_700_000_000_000_000_000), -4_660)
        );
        assert_eq!(
            sched[1],
            (Timestamp::from_nanos(1_700_028_800_000_000_000), 10_000)
        );
        // A string rate is also accepted; success=false errors; no data → empty.
        let str_rate = r#"{"success":true,"data":{"resultList":[
            {"symbol":"BTC_USDT","fundingRate":"0.0001","settleTime":1700000000000}]}}"#;
        assert_eq!(parse_funding_rate(str_rate).unwrap()[0].1, 10_000);
        assert!(parse_funding_rate(r#"{"success":false}"#).is_err());
        assert!(
            parse_funding_rate(r#"{"success":true}"#)
                .unwrap()
                .is_empty()
        );
        assert!(parse_funding_rate("not json").is_err());
    }

    #[test]
    fn fetch_funding_history_builds_public_contract_url() {
        use crate::transport::MockTransport;
        let body = r#"{"success":true,"data":{"resultList":[
            {"symbol":"BTC_USDT","fundingRate":0.0001,"settleTime":1700000000000}]}}"#;
        let mut t = MockTransport::new(vec![HttpResponse::ok(body)]);
        let sched = fetch_funding_history(&mut t, FUTURES_BASE_URL, "BTC_USDT", 100).unwrap();
        assert_eq!(sched.len(), 1);
        assert!(
            t.sent[0]
                .url
                .contains("/api/v1/contract/funding_rate/history?symbol=BTC_USDT")
        );
        assert!(t.sent[0].api_key.is_none()); // public endpoint — unsigned
        let mut bad = MockTransport::new(vec![HttpResponse::error(500, "")]);
        assert!(fetch_funding_history(&mut bad, FUTURES_BASE_URL, "BTC_USDT", 100).is_err());
    }

    #[test]
    fn fetch_funding_history_paged_concatenates_until_empty() {
        use crate::transport::MockTransport;
        let page1 = r#"{"success":true,"data":{"resultList":[
            {"symbol":"BTC_USDT","fundingRate":0.0001,"settleTime":1700028800000},
            {"symbol":"BTC_USDT","fundingRate":-0.0000466,"settleTime":1700000000000}]}}"#;
        let empty = r#"{"success":true,"data":{"resultList":[]}}"#;
        let mut t = MockTransport::new(vec![HttpResponse::ok(page1), HttpResponse::ok(empty)]);
        let sched = fetch_funding_history_paged(&mut t, FUTURES_BASE_URL, "BTC_USDT", 5).unwrap();
        assert_eq!(sched.len(), 2); // page 1's two rows; empty page 2 stops paging
        assert_eq!(t.sent.len(), 2);
        assert!(t.sent[0].url.contains("page_num=1&page_size=100"));
        assert!(t.sent[1].url.contains("page_num=2"));
        // Concatenated + ascending: -0.0000466 → -4_660 (older), 0.0001 → 10_000 (newer).
        assert_eq!(
            sched[0],
            (Timestamp::from_nanos(1_700_000_000_000_000_000), -4_660)
        );
        assert_eq!(
            sched[1],
            (Timestamp::from_nanos(1_700_028_800_000_000_000), 10_000)
        );
        let mut bad = MockTransport::new(vec![HttpResponse::error(500, "")]);
        assert!(fetch_funding_history_paged(&mut bad, FUTURES_BASE_URL, "X", 5).is_err());
    }

    #[test]
    fn fetch_contracts_builds_public_detail_url() {
        use crate::transport::MockTransport;
        let body = r#"{"success":true,"data":[
            {"symbol":"BTC_USDT","baseCoin":"BTC","quoteCoin":"USDT","priceScale":2,"volScale":0,
             "priceUnit":0.10,"volUnit":1,"state":0,"apiAllowed":true}]}"#;
        let mut t = MockTransport::new(vec![HttpResponse::ok(body)]);
        let contracts = fetch_contracts(&mut t, FUTURES_BASE_URL).unwrap();
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].symbol, "BTC_USDT");
        assert!(t.sent[0].url.ends_with("/api/v1/contract/detail"));
        assert!(t.sent[0].api_key.is_none());
        let mut bad = MockTransport::new(vec![HttpResponse::error(500, "")]);
        assert!(fetch_contracts(&mut bad, FUTURES_BASE_URL).is_err());
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
        .with_range(1_700_000_000_000, 1_700_000_200_000)
        .with_page_delay(std::time::Duration::ZERO);
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 2); // page 1 reaches `start` (oldest open == start) → backfill stops
    }

    /// Regression: MEXC caps a contract-kline response at ~2000 bars anchored on
    /// `end` and ignores `start` for long ranges, so the back-fill must page
    /// newest→oldest by moving the `end` cursor back — not by advancing `start`
    /// (which re-fetched the same recent window forever, the bug that capped a
    /// year-long request at one page).
    #[test]
    fn feed_pages_backward_by_end_cursor() {
        use crate::transport::MockTransport;
        // Two 5-bar pages of 1m bars spanning [1700000000, 1700000540] (opens).
        let recent = fut_cols(&[
            1_700_000_300,
            1_700_000_360,
            1_700_000_420,
            1_700_000_480,
            1_700_000_540,
        ]);
        let older = fut_cols(&[
            1_700_000_000,
            1_700_000_060,
            1_700_000_120,
            1_700_000_180,
            1_700_000_240,
        ]);
        let mut feed = MexcFuturesKlineFeed::new(
            MockTransport::new(vec![HttpResponse::ok(recent), HttpResponse::ok(older)]),
            "http://x",
            "BTC_USDT",
            InstrumentId::new(0),
            "1m",
            0,
            0,
        )
        .with_range(1_700_000_000_000, 1_700_000_600_000)
        .with_page_delay(std::time::Duration::ZERO);
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 10); // BOTH pages collected, not just the recent 5
        // The `end` cursor moved BACKWARD between requests (newest→oldest paging).
        assert!(feed.transport.sent[0].url.contains("end=1700000600"));
        assert!(feed.transport.sent[1].url.contains("end=1700000240"));
    }

    #[test]
    fn feed_retries_on_429_then_yields() {
        use crate::transport::MockTransport;
        // A leading 429 must be retried (zero back-off at page_delay 0), not fatal.
        let mut feed = MexcFuturesKlineFeed::new(
            MockTransport::new(vec![
                HttpResponse::error(429, ""),
                HttpResponse::ok(fut_cols(&[1_700_000_000])),
            ]),
            "http://x",
            "BTC_USDT",
            InstrumentId::new(0),
            "1m",
            0,
            0,
        )
        .with_page_delay(std::time::Duration::ZERO);
        // No range → single recent fetch; the 429 is retried, then the bar is yielded.
        assert!(feed.next_event().is_some());
        assert!(feed.next_event().is_none());
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

    fn mexc_range_feed(
        bodies: Vec<crate::transport::HttpResponse>,
        start_ms: i64,
        end_ms: i64,
    ) -> MexcFuturesKlineFeed<crate::transport::MockTransport> {
        MexcFuturesKlineFeed::new(
            crate::transport::MockTransport::new(bodies),
            "http://x",
            "BTC_USDT",
            InstrumentId::new(0),
            "1m",
            0,
            0,
        )
        .with_range(start_ms, end_ms)
        .with_page_delay(std::time::Duration::ZERO)
    }

    #[test]
    fn feed_guards_against_no_progress() {
        // Same page twice: the `end` cursor can't move back → terminate, not hang.
        let p = || HttpResponse::ok(fut_cols(&[1_700_000_300, 1_700_000_360]));
        let mut feed = mexc_range_feed(vec![p(), p()], 1_700_000_000_000, 1_700_000_600_000);
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 2); // de-duplicated; guard stopped the stall
    }

    #[test]
    fn feed_clips_window_and_dedups() {
        // open seconds; start = 1_700_000_060 s. The 1_700_000_000 bar is clipped;
        // the 1_700_000_060 duplicate collapses.
        let page = fut_cols(&[1_700_000_000, 1_700_000_060, 1_700_000_060, 1_700_000_120]);
        let mut feed = mexc_range_feed(
            vec![HttpResponse::ok(page)],
            1_700_000_060_000,
            1_700_000_600_000,
        );
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 2); // 60 & 120; 0 clipped, dup collapsed
    }

    #[test]
    fn feed_empty_page_terminates_backfill() {
        // Page 1 has oldest open > start (would page again); the empty page 2 stops it.
        let empty = r#"{"success":true,"data":{"time":[],"open":[],"high":[],"low":[],"close":[],"vol":[]}}"#;
        let mut feed = mexc_range_feed(
            vec![
                HttpResponse::ok(fut_cols(&[1_700_000_300, 1_700_000_360])),
                HttpResponse::ok(empty.to_string()),
            ],
            1_700_000_000_000,
            1_700_000_600_000,
        );
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 2); // page 1 kept; empty page 2 ended the backfill
    }

    #[test]
    fn feed_429_exhausted_yields_none() {
        let mut feed = mexc_range_feed(
            (0..10).map(|_| HttpResponse::error(429, "")).collect(),
            1_700_000_000_000,
            1_700_000_060_000,
        );
        assert!(feed.next_event().is_none());
    }
}
