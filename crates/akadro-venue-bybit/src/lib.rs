// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bybit venue connector (v5 unified: spot + **linear perpetual**, REST) for
//! akadro — depends only on `akadro-core`, so it adds a venue with **zero**
//! changes to the engine or strategies (the open/closed goal).
//!
//! It provides a [`BybitCatalog`] (`/v5/market/instruments-info` → [`InstrumentSpec`]),
//! a [`BybitKlineFeed`] ([`DataSource`] over `/v5/market/kline`), and a
//! [`BybitExec`] ([`ExecutionClient`] that signs and submits `/v5/order/create`).
//! Logic talks to Bybit through the mockable [`Transport`] seam, so signing,
//! parsing and normalization are fixture-tested with no network; the live
//! `reqwest` transport is behind the `net` feature. Live **fills** arrive on the
//! `execution` WebSocket topic (the `akadro-live` shell), so [`BybitExec::observe`]
//! is a no-op on bars.
//!
//! Bybit v5 signs `hex(HMAC-SHA256(secret, timestamp + api_key + recv_window +
//! payload))` (payload = the query string for `GET`, the raw JSON body for `POST`)
//! and authenticates with the `X-BAPI-API-KEY/SIGN/TIMESTAMP/RECV-WINDOW` headers.

use akadro_core::{
    AccountEvent, AssetId, Bar, CapSet, Capability, ClientOrderId, DataSource, Event, EventSink,
    ExecutionClient, InstrumentCatalog, InstrumentId, InstrumentKind, InstrumentSpec, Money,
    OrderKind, OrderRequest, PageSink, Price, Qty, RejectReason, Side, TimeInForce, Timestamp,
};
use serde::Deserialize;

mod sign {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;

    /// `hex(HMAC-SHA256(secret, prehash))` where `prehash = timestamp + api_key +
    /// recv_window + payload` — the Bybit v5 `X-BAPI-SIGN`.
    ///
    /// # Panics
    /// Never in practice — HMAC-SHA256 accepts a key of any length.
    #[must_use]
    pub fn sign(secret: &[u8], prehash: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC key any length");
        mac.update(prehash.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}
pub use sign::sign;

/// Connector errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BybitError {
    /// Transport / network failure.
    #[error("transport error: {0}")]
    Transport(String),
    /// Unparseable or unexpected response.
    #[error("parse error: {0}")]
    Parse(String),
}

// --- transport seam ----------------------------------------------------------

/// HTTP method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
}

/// An outbound HTTP request: a method, full URL, optional JSON body, and header
/// `(name, value)` pairs (the signed auth headers, when present).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// Method.
    pub method: Method,
    /// Full URL (base + path + query).
    pub url: String,
    /// JSON body for `POST`s.
    pub body: Option<String>,
    /// Header `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
}

/// An HTTP response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Body.
    pub body: String,
}

impl HttpResponse {
    /// `true` for 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Sends an [`HttpRequest`] and returns an [`HttpResponse`].
pub trait Transport {
    /// Send one request.
    ///
    /// # Errors
    /// [`BybitError::Transport`] on a network/transport failure.
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, BybitError>;
}

/// A test transport returning canned responses in order, recording requests.
#[derive(Debug, Default)]
pub struct MockTransport {
    responses: std::collections::VecDeque<HttpResponse>,
    /// Every request sent, for assertions.
    pub sent: Vec<HttpRequest>,
}

impl MockTransport {
    /// Build from a list of responses (returned in order).
    #[must_use]
    pub fn new(responses: Vec<HttpResponse>) -> Self {
        MockTransport {
            responses: responses.into(),
            sent: Vec::new(),
        }
    }
}

impl Transport for MockTransport {
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, BybitError> {
        self.sent.push(request.clone());
        self.responses
            .pop_front()
            .ok_or_else(|| BybitError::Transport("no canned response".into()))
    }
}

// --- fixed-point decimal helpers ---------------------------------------------

// `scale_of` and `raw_to_decimal` are venue-neutral and live in `akadro-core` (DRY),
// re-exported here for this connector's public surface.
pub use akadro_core::{raw_to_decimal, scale_of};

/// Parse a decimal string to a raw fixed-point `i64` at `scale` (truncating excess
/// precision). Thin adapter over [`akadro_core::decimal_to_raw`] mapping a rejected
/// value to this venue's error.
///
/// # Errors
/// [`BybitError::Parse`] if `s` is empty, non-numeric, or overflows `i64`.
pub fn decimal_to_raw(s: &str, scale: u32) -> Result<i64, BybitError> {
    akadro_core::decimal_to_raw(s, scale)
        .ok_or_else(|| BybitError::Parse(format!("bad decimal {s:?} at scale {scale}")))
}

/// Milliseconds in a Bybit kline `interval` token: minutes as a number (`"1"`,
/// `"60"`, `"240"`) or `"D"`/`"W"`/`"M"`; `0` if unrecognized.
#[must_use]
pub fn interval_millis(interval: &str) -> i64 {
    match interval {
        "D" => 86_400_000,
        "W" => 604_800_000,
        "M" => 2_592_000_000, // 30 days, nominal
        other => other.parse::<i64>().unwrap_or(0) * 60_000,
    }
}

// --- category ----------------------------------------------------------------

/// The Bybit product category this connector is configured for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    /// Spot.
    Spot,
    /// USDT/USDC linear perpetual.
    Linear,
}

impl Category {
    /// The `category` query/body value (`"spot"` / `"linear"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Spot => "spot",
            Category::Linear => "linear",
        }
    }
    fn kind(self) -> InstrumentKind {
        match self {
            Category::Spot => InstrumentKind::Spot,
            Category::Linear => InstrumentKind::PerpetualFuture,
        }
    }
}

// --- catalogue ---------------------------------------------------------------

#[derive(Deserialize)]
struct InstrumentsResp {
    result: InstrumentsResult,
}
#[derive(Deserialize)]
struct InstrumentsResult {
    #[serde(default)]
    list: Vec<InstrumentRow>,
}
#[derive(Deserialize)]
struct InstrumentRow {
    symbol: String,
    #[serde(rename = "baseCoin")]
    base_coin: String,
    #[serde(rename = "quoteCoin")]
    quote_coin: String,
    #[serde(default)]
    status: String,
    #[serde(rename = "priceFilter")]
    price_filter: PriceFilter,
    #[serde(rename = "lotSizeFilter")]
    lot_size_filter: LotSizeFilter,
}
#[derive(Deserialize)]
struct PriceFilter {
    #[serde(rename = "tickSize")]
    tick_size: String,
}
#[derive(Deserialize, Default)]
struct LotSizeFilter {
    #[serde(rename = "basePrecision", default)]
    base_precision: String,
    #[serde(rename = "qtyStep", default)]
    qty_step: String,
    #[serde(rename = "minOrderAmt", default)]
    min_order_amt: String,
    #[serde(rename = "minNotionalValue", default)]
    min_notional_value: String,
}

/// Maps Bybit symbols to dense [`InstrumentSpec`]s and asset ids.
#[derive(Debug, Clone)]
pub struct BybitCatalog {
    specs: Vec<InstrumentSpec>,
    symbols: Vec<String>,
    scales: Vec<(u32, u32)>,
}

impl BybitCatalog {
    /// Fetch `/v5/market/instruments-info?category={spot|linear}` over `transport` and
    /// parse it into a catalogue (see [`from_instruments`](Self::from_instruments)).
    /// The reusable connector entry point — callers never build the URL or touch the
    /// response body.
    ///
    /// # Errors
    /// [`BybitError::Transport`] on a transport failure or a non-2xx status;
    /// [`BybitError::Parse`] on malformed JSON.
    pub fn fetch<T: Transport>(
        transport: &mut T,
        base_url: &str,
        category: Category,
    ) -> Result<Self, BybitError> {
        let resp = transport.send(&HttpRequest {
            method: Method::Get,
            url: format!(
                "{base_url}/v5/market/instruments-info?category={}",
                category.as_str()
            ),
            body: None,
            headers: Vec::new(),
        })?;
        if !resp.is_success() {
            return Err(BybitError::Transport(format!(
                "instruments-info HTTP {}",
                resp.status
            )));
        }
        Self::from_instruments(&resp.body, category)
    }

    /// Parse `/v5/market/instruments-info?category={spot|linear}` into a catalogue.
    /// Only `Trading` symbols are kept; the lot step is `basePrecision` (spot) or
    /// `qtyStep` (linear), and the minimum notional is `minOrderAmt` (spot) or
    /// `minNotionalValue` (linear). Each gets a dense [`InstrumentId`].
    ///
    /// # Errors
    /// [`BybitError::Parse`] on unparseable JSON or a bad filter value.
    pub fn from_instruments(json: &str, category: Category) -> Result<Self, BybitError> {
        let resp: InstrumentsResp =
            serde_json::from_str(json).map_err(|e| BybitError::Parse(e.to_string()))?;
        let mut specs = Vec::new();
        let mut symbols = Vec::new();
        let mut scales = Vec::new();
        let mut assets: Vec<String> = Vec::new();
        let asset_id = |name: &str, assets: &mut Vec<String>| -> AssetId {
            if let Some(i) = assets.iter().position(|a| a == name) {
                AssetId::new(i as u32)
            } else {
                assets.push(name.to_string());
                AssetId::new((assets.len() - 1) as u32)
            }
        };
        let caps = if category == Category::Linear {
            CapSet::empty()
                .with(Capability::LimitOrders)
                .with(Capability::Margin)
                .with(Capability::Funding)
                .with(Capability::ShortSelling)
        } else {
            CapSet::empty().with(Capability::LimitOrders)
        };
        for row in resp.result.list {
            if !row.status.is_empty() && row.status != "Trading" {
                continue;
            }
            let step = match category {
                Category::Spot => &row.lot_size_filter.base_precision,
                Category::Linear => &row.lot_size_filter.qty_step,
            };
            if row.price_filter.tick_size.is_empty() || step.is_empty() {
                continue;
            }
            let price_scale = scale_of(&row.price_filter.tick_size);
            let qty_scale = scale_of(step);
            let min_notional = match category {
                Category::Spot => &row.lot_size_filter.min_order_amt,
                Category::Linear => &row.lot_size_filter.min_notional_value,
            };
            let min_notional_raw = if min_notional.is_empty() {
                0
            } else {
                decimal_to_raw(min_notional, price_scale + qty_scale)?
            };
            let id = InstrumentId::new(specs.len() as u32);
            let base = asset_id(&row.base_coin, &mut assets);
            let quote = asset_id(&row.quote_coin, &mut assets);
            specs.push(InstrumentSpec::new(
                id,
                base,
                quote,
                category.kind(),
                Price::from_raw(decimal_to_raw(&row.price_filter.tick_size, price_scale)?),
                Qty::from_raw(decimal_to_raw(step, qty_scale)?),
                Money::from_raw(i128::from(min_notional_raw)),
                caps,
            ));
            symbols.push(row.symbol);
            scales.push((price_scale, qty_scale));
        }
        Ok(BybitCatalog {
            specs,
            symbols,
            scales,
        })
    }

    /// All specs (dense, in id order) — pass to `Engine::new`.
    #[must_use]
    pub fn specs(&self) -> &[InstrumentSpec] {
        &self.specs
    }

    /// The [`InstrumentId`] for `symbol` (e.g. `"BTCUSDT"`), if present.
    #[must_use]
    pub fn id_of(&self, symbol: &str) -> Option<InstrumentId> {
        self.symbols
            .iter()
            .position(|s| s == symbol)
            .map(|i| InstrumentId::new(i as u32))
    }

    /// `(price_scale, qty_scale)` for an instrument.
    #[must_use]
    pub fn scales(&self, id: InstrumentId) -> Option<(u32, u32)> {
        self.scales.get(id.index() as usize).copied()
    }
}

impl InstrumentCatalog for BybitCatalog {
    fn spec(&self, id: InstrumentId) -> Option<&InstrumentSpec> {
        self.specs.get(id.index() as usize)
    }
}

// --- klines (DataSource) -----------------------------------------------------

#[derive(Deserialize)]
struct KlineResp {
    result: KlineResult,
}
#[derive(Deserialize)]
struct KlineResult {
    #[serde(default)]
    list: Vec<Vec<String>>,
}

/// Parse a Bybit `/v5/market/kline` body into [`Bar`]s. Rows are
/// `[startTime, o, h, l, c, volume, turnover]` with `startTime` the window
/// **open** time (ms); Bybit returns them newest-first, so they are sorted
/// ascending and stamped at close time (`startTime + interval_millis`).
///
/// **Unclosed last candle.** The Bybit v5 REST kline carries no per-row "closed"
/// flag (only the WS kline has `confirm`). When the requested range reaches the
/// present, the first row Bybit returns (newest, sorted last here) is the still-
/// forming candle, whose OHLCV keeps changing. This parser returns it as-is; for
/// reproducible runs back-fill a window ending before the present, or drop a
/// trailing bar whose `startTime + interval` exceeds server time.
///
/// # Errors
/// [`BybitError::Parse`] on bad JSON, a short row, or a bad number.
pub fn parse_klines(
    json: &str,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
    interval: &str,
) -> Result<Vec<Bar>, BybitError> {
    let resp: KlineResp =
        serde_json::from_str(json).map_err(|e| BybitError::Parse(e.to_string()))?;
    let interval_ms = interval_millis(interval);
    let mut bars = Vec::with_capacity(resp.result.list.len());
    for row in resp.result.list {
        if row.len() < 6 {
            return Err(BybitError::Parse(format!(
                "kline row has {} fields",
                row.len()
            )));
        }
        let open_ms: i64 = row[0]
            .parse()
            .map_err(|_| BybitError::Parse(format!("bad ts {:?}", row[0])))?;
        let close_ms = open_ms
            .checked_add(interval_ms)
            .ok_or_else(|| BybitError::Parse("ts overflow".into()))?;
        bars.push(Bar::new(
            instrument,
            Timestamp::from_nanos(
                close_ms
                    .checked_mul(1_000_000)
                    .ok_or_else(|| BybitError::Parse("ts overflow".into()))?,
            ),
            Price::from_raw(decimal_to_raw(&row[1], price_scale)?),
            Price::from_raw(decimal_to_raw(&row[2], price_scale)?),
            Price::from_raw(decimal_to_raw(&row[3], price_scale)?),
            Price::from_raw(decimal_to_raw(&row[4], price_scale)?),
            Qty::from_raw(decimal_to_raw(&row[5], qty_scale)?),
        ));
    }
    bars.sort_by_key(|b| b.ts.as_nanos()); // Bybit returns newest-first
    Ok(bars)
}

// --- funding-rate history ----------------------------------------------------

/// Funding-rate fixed-point scale — re-exported from `akadro-core` so the connector
/// and the engine's `mul_rate` charge cannot drift. Rates normalize to `1e-8`
/// fractions: Bybit's `"0.0001"` (1 bp) → `10_000`, and a sub-bp `"0.0000466"` →
/// `4_660` instead of rounding to `0`. See `SimulatedExchange::with_funding_schedule`.
pub use akadro_core::FUNDING_RATE_SCALE;

#[derive(Deserialize)]
struct FundingResp {
    result: FundingResult,
}
#[derive(Deserialize, Default)]
struct FundingResult {
    #[serde(default)]
    list: Vec<FundingRow>,
}
#[derive(Deserialize)]
struct FundingRow {
    #[serde(rename = "fundingRate")]
    funding_rate: String,
    #[serde(rename = "fundingRateTimestamp")]
    funding_rate_timestamp: String, // Bybit returns the epoch-ms as a string
}

/// Parse a Bybit `/v5/market/funding/history` (linear) body into an ascending
/// `(timestamp, rate)` schedule for `SimulatedExchange::with_funding_schedule`.
/// `fundingRate` is a decimal-fraction string normalized to [`FUNDING_RATE_SCALE`]
/// (`1e-8`), so Bybit's sub-bp rates are preserved rather than rounded to `0`. Bybit
/// returns newest-first, so the result is re-sorted ascending.
///
/// # Errors
/// [`BybitError::Parse`] on bad JSON, a bad rate, or a bad/overflowing timestamp.
pub fn parse_funding_rate(json: &str) -> Result<Vec<(Timestamp, i64)>, BybitError> {
    let resp: FundingResp =
        serde_json::from_str(json).map_err(|e| BybitError::Parse(e.to_string()))?;
    let mut out: Vec<(Timestamp, i64)> = resp
        .result
        .list
        .into_iter()
        .map(|r| {
            let ms: i64 = r.funding_rate_timestamp.trim().parse().map_err(|_| {
                BybitError::Parse(format!(
                    "bad fundingRateTimestamp {:?}",
                    r.funding_rate_timestamp
                ))
            })?;
            let ns = ms
                .checked_mul(1_000_000)
                .ok_or_else(|| BybitError::Parse("fundingRateTimestamp overflow".into()))?;
            Ok((
                Timestamp::from_nanos(ns),
                decimal_to_raw(&r.funding_rate, FUNDING_RATE_SCALE)?,
            ))
        })
        .collect::<Result<_, BybitError>>()?;
    out.sort_by_key(|(t, _)| t.as_nanos());
    Ok(out)
}

/// Fetch `/v5/market/funding/history` (category `linear`) for `symbol` over
/// `transport` and parse it into a `with_funding_schedule` schedule (rates at
/// [`FUNDING_RATE_SCALE`]). `limit` caps the settlements returned (Bybit's cap is
/// 200). The reusable connector entry point — callers never build the URL or touch
/// the body.
///
/// # Errors
/// [`BybitError::Transport`] on a transport failure or a non-2xx status;
/// [`BybitError::Parse`] on malformed JSON.
pub fn fetch_funding_history<T: Transport>(
    transport: &mut T,
    base_url: &str,
    symbol: &str,
    limit: u32,
) -> Result<Vec<(Timestamp, i64)>, BybitError> {
    let resp = transport.send(&HttpRequest {
        method: Method::Get,
        url: format!(
            "{base_url}/v5/market/funding/history?category=linear&symbol={symbol}&limit={}",
            limit.min(200)
        ),
        body: None,
        headers: Vec::new(),
    })?;
    if !resp.is_success() {
        return Err(BybitError::Transport(format!(
            "funding/history HTTP {}",
            resp.status
        )));
    }
    parse_funding_rate(&resp.body)
}

/// Page `/v5/market/funding/history` back over `max_pages` (200/page) into one
/// ascending [`FUNDING_RATE_SCALE`] schedule covering a long backtest window. Bybit
/// pages by `endTime` (returns the ≤200 settlements at or before it, newest-first);
/// each page moves `endTime` to just before the oldest settlement seen. Stops early
/// on an empty page; de-duplicates by time. The reusable connector entry point.
///
/// # Errors
/// [`BybitError::Transport`] on a transport failure or a non-2xx status;
/// [`BybitError::Parse`] on malformed JSON.
pub fn fetch_funding_history_paged<T: Transport>(
    transport: &mut T,
    base_url: &str,
    symbol: &str,
    start_ms: i64,
    end_ms: i64,
    max_pages: u32,
) -> Result<Vec<(Timestamp, i64)>, BybitError> {
    let mut all: Vec<(Timestamp, i64)> = Vec::new();
    let mut cursor_end = end_ms;
    for _ in 0..max_pages.max(1) {
        let resp = transport.send(&HttpRequest {
            method: Method::Get,
            url: format!(
                "{base_url}/v5/market/funding/history?category=linear&symbol={symbol}&startTime={start_ms}&endTime={cursor_end}&limit=200"
            ),
            body: None,
            headers: Vec::new(),
        })?;
        if !resp.is_success() {
            return Err(BybitError::Transport(format!(
                "funding/history HTTP {}",
                resp.status
            )));
        }
        let page = parse_funding_rate(&resp.body)?; // ascending
        let Some((oldest, _)) = page.first().copied() else {
            break;
        };
        let oldest_ms = oldest.as_nanos() / 1_000_000;
        all.extend(page);
        if oldest_ms <= start_ms {
            break;
        }
        let next_end = oldest_ms - 1;
        if next_end >= cursor_end {
            break; // no backward progress
        }
        cursor_end = next_end;
    }
    all.sort_by_key(|(t, _)| t.as_nanos());
    all.dedup_by_key(|(t, _)| t.as_nanos());
    all.retain(|(t, _)| {
        let ms = t.as_nanos() / 1_000_000;
        ms >= start_ms && ms <= end_ms
    });
    Ok(all)
}

/// A [`DataSource`] streaming Bybit klines for one instrument (a single recent fetch,
/// or — with [`with_range`](BybitKlineFeed::with_range) — a paged historical window).
pub struct BybitKlineFeed<T> {
    transport: T,
    base_url: String,
    category: Category,
    symbol: String,
    instrument: InstrumentId,
    interval: String,
    price_scale: u32,
    qty_scale: u32,
    limit: u32,
    /// `Some((start_ms, end_ms))` → page back over `[start, end)`; `None` → one
    /// recent fetch.
    range: Option<(i64, i64)>,
    /// Inter-page courtesy delay + the 429 back-off unit; `0` in tests.
    page_delay: std::time::Duration,
    /// Bounded retries on a `429` page.
    max_retries: u32,
    /// Opt-in per-page sink for incremental cache journaling (resumable back-fill).
    page_sink: Option<Box<dyn PageSink>>,
    buffer: std::collections::VecDeque<Bar>,
    fetched: bool,
}

impl<T: Transport> BybitKlineFeed<T> {
    /// Create a feed for `symbol` (mapped to `instrument`) at `interval`.
    #[must_use]
    #[allow(clippy::too_many_arguments)] // venue id + market params; a builder would be noisier
    pub fn new(
        transport: T,
        base_url: impl Into<String>,
        category: Category,
        symbol: impl Into<String>,
        instrument: InstrumentId,
        interval: impl Into<String>,
        price_scale: u32,
        qty_scale: u32,
    ) -> Self {
        BybitKlineFeed {
            transport,
            base_url: base_url.into(),
            category,
            symbol: symbol.into(),
            instrument,
            interval: interval.into(),
            price_scale,
            qty_scale,
            limit: 200,
            range: None,
            page_delay: std::time::Duration::from_millis(120),
            max_retries: 8,
            page_sink: None,
            buffer: std::collections::VecDeque::new(),
            fetched: false,
        }
    }

    /// Install a per-page [`PageSink`] (the cache's incremental journal), handed each
    /// page of a [`with_range`](Self::with_range) back-fill before it is buffered so
    /// the download is resumable. The cache layer supplies this; user code does not.
    #[must_use]
    pub fn with_page_sink(mut self, sink: Box<dyn PageSink>) -> Self {
        self.page_sink = Some(sink);
        self
    }

    /// Set the page size (Bybit caps `/v5/market/kline` at 1000).
    #[must_use]
    pub fn with_limit(mut self, limit: u32) -> Self {
        // Clamp to [1, 1000]: a 0 limit sends `limit=0` and silently empties the
        // feed (bug class 8).
        self.limit = limit.clamp(1, 1000);
        self
    }

    /// Back-fill an arbitrary historical window `[start_ms, end_ms)` (epoch-ms on the
    /// candle **open** time) by paging `/v5/market/kline` newest→oldest (Bybit anchors
    /// each ≤1000-bar page on `end` and returns it newest-first), instead of the
    /// single recent fetch. Uses the 1000-bar cap for the fewest pages; bars come back
    /// ascending, de-duplicated, close-stamped exactly like [`parse_klines`].
    #[must_use]
    pub fn with_range(mut self, start_ms: i64, end_ms: i64) -> Self {
        self.range = Some((start_ms, end_ms));
        self.limit = 1000; // range back-fill wants the largest pages
        self
    }

    /// Override the inter-page courtesy delay (default 120 ms; also the 429 back-off
    /// unit). Set to [`Duration::ZERO`](std::time::Duration::ZERO) in tests.
    #[must_use]
    pub fn with_page_delay(mut self, delay: std::time::Duration) -> Self {
        self.page_delay = delay;
        self
    }

    /// One `/v5/market/kline` page bounded by an optional `[start, end]` (ms), with a
    /// bounded 429 back-off (zero at `page_delay` 0). Bars are parsed + close-stamped.
    fn fetch_page(
        &mut self,
        start_ms: Option<i64>,
        end_ms: Option<i64>,
    ) -> Result<Vec<Bar>, BybitError> {
        use core::fmt::Write as _;
        let mut url = format!(
            "{}/v5/market/kline?category={}&symbol={}&interval={}&limit={}",
            self.base_url,
            self.category.as_str(),
            self.symbol,
            self.interval,
            self.limit
        );
        if let Some(s) = start_ms {
            let _ = write!(url, "&start={s}");
        }
        if let Some(e) = end_ms {
            let _ = write!(url, "&end={e}");
        }
        let backoff = if self.page_delay.is_zero() {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_secs(2)
        };
        let mut retries = 0u32;
        loop {
            let resp = self.transport.send(&HttpRequest {
                method: Method::Get,
                url: url.clone(),
                body: None,
                headers: Vec::new(),
            })?;
            if resp.status == 429 && retries < self.max_retries {
                retries += 1;
                if !backoff.is_zero() {
                    std::thread::sleep(backoff);
                }
                continue;
            }
            if !resp.is_success() {
                return Err(BybitError::Transport(format!("kline HTTP {}", resp.status)));
            }
            return parse_klines(
                &resp.body,
                self.instrument,
                self.price_scale,
                self.qty_scale,
                &self.interval,
            );
        }
    }

    /// Page `[start_ms, end_ms)` newest→oldest by moving the `end` cursor back to just
    /// before the oldest bar received, until a page reaches `start`. Bars are
    /// close-stamped; the result is ascending, de-duplicated, clipped to the window.
    fn backfill(&mut self, start_ms: i64, end_ms: i64) -> Result<Vec<Bar>, BybitError> {
        let interval_ms = interval_millis(&self.interval);
        if interval_ms == 0 {
            return Err(BybitError::Parse(format!(
                "unsupported interval {:?}",
                self.interval
            )));
        }
        let mut cursor_end = end_ms;
        let mut all: Vec<Bar> = Vec::new();
        loop {
            let page = self.fetch_page(Some(start_ms), Some(cursor_end))?;
            if page.is_empty() {
                break;
            }
            // `ts` is close time; open = close - interval. Oldest open drives the next
            // (earlier) `end` cursor.
            let oldest_open_ms = page
                .iter()
                .map(|b| b.ts.as_nanos() / 1_000_000 - interval_ms)
                .min()
                .expect("non-empty");
            if let Some(sink) = self.page_sink.as_mut() {
                sink.on_page(&page); // incremental journal before buffering (resumable)
            }
            all.extend(page);
            if oldest_open_ms <= start_ms {
                break;
            }
            let next_end = oldest_open_ms - 1; // strictly older than the oldest we have
            if next_end >= cursor_end {
                break; // no backward progress
            }
            cursor_end = next_end;
            if !self.page_delay.is_zero() {
                std::thread::sleep(self.page_delay);
            }
        }
        all.sort_by_key(|b| b.ts.as_nanos());
        all.dedup_by_key(|b| b.ts.as_nanos());
        all.retain(|b| {
            let open_ms = b.ts.as_nanos() / 1_000_000 - interval_ms;
            open_ms >= start_ms && open_ms < end_ms
        });
        Ok(all)
    }

    fn fetch(&mut self) -> Result<(), BybitError> {
        let bars = match self.range {
            Some((s, e)) => self.backfill(s, e)?,
            None => self.fetch_page(None, None)?,
        };
        self.buffer.extend(bars);
        Ok(())
    }
}

impl<T: Transport> DataSource for BybitKlineFeed<T> {
    fn next_event(&mut self) -> Option<Event> {
        if !self.fetched {
            self.fetched = true;
            if let Err(e) = self.fetch() {
                // Truncated download: tell the page-sink so the cache loader does
                // not record the unfetched remainder as verified-empty (silent loss).
                if let Some(sink) = self.page_sink.as_mut() {
                    sink.on_error(&e.to_string());
                }
                return None;
            }
        }
        self.buffer.pop_front().map(Event::Bar)
    }
}

impl<T> core::fmt::Debug for BybitKlineFeed<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BybitKlineFeed")
            .field("symbol", &self.symbol)
            .field("interval", &self.interval)
            .field("buffered", &self.buffer.len())
            .finish_non_exhaustive()
    }
}

// --- execution client --------------------------------------------------------

/// One instrument's symbol + scales + category, for [`BybitExec`] order encoding.
#[derive(Debug, Clone)]
pub struct InstrumentMeta {
    /// Bybit symbol (e.g. `"BTCUSDT"`).
    pub symbol: String,
    /// Price fixed-point scale.
    pub price_scale: u32,
    /// Quantity fixed-point scale.
    pub qty_scale: u32,
    /// Spot or linear (selects the order `category`).
    pub category: Category,
}

/// The akadro `client_order_id` tag set as Bybit's `orderLinkId` for live fill
/// attribution: `client_order_tag(ClientOrderId::new(7))` → `"ak7"`.
#[must_use]
pub fn client_order_tag(id: ClientOrderId) -> String {
    format!("ak{}", id.raw())
}

/// Inverse of [`client_order_tag`].
#[must_use]
pub fn parse_client_order_tag(tag: &str) -> Option<ClientOrderId> {
    tag.strip_prefix("ak")
        .and_then(|n| n.parse::<u64>().ok())
        .map(ClientOrderId::new)
}

/// An [`ExecutionClient`] that signs and submits Bybit v5 orders over REST.
/// `submit` posts a signed `/v5/order/create` and emits
/// [`AccountEvent::OrderAccepted`] when `retCode == 0`, else
/// [`AccountEvent::OrderRejected`]. `observe` is a no-op: live fills arrive on the
/// `execution` WebSocket topic.
pub struct BybitExec<T> {
    transport: T,
    base_url: String,
    api_key: String,
    secret: Vec<u8>,
    recv_window: String,
    meta: Vec<InstrumentMeta>,
    timestamp_ms: String,
}

impl<T: Transport> BybitExec<T> {
    /// Create with credentials and per-instrument [`InstrumentMeta`] (dense, in id
    /// order — typically from a [`BybitCatalog`]).
    #[must_use]
    pub fn new(
        transport: T,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        secret: impl Into<Vec<u8>>,
        meta: Vec<InstrumentMeta>,
    ) -> Self {
        BybitExec {
            transport,
            base_url: base_url.into(),
            api_key: api_key.into(),
            secret: secret.into(),
            recv_window: "5000".into(),
            meta,
            timestamp_ms: String::new(),
        }
    }

    /// Pin a **fixed** millisecond timestamp for request signing — intended for
    /// deterministic tests. When left **unset** (the default), each request signs
    /// with the handler's event time (`now`), which under a live event-time clock
    /// tracks wall-clock; that is the correct live behaviour. Do **not** call this in
    /// live: a pinned timestamp is reused verbatim for every request and soon falls
    /// outside Bybit's `recv_window`, so signed orders start being rejected (m36).
    #[must_use]
    pub fn with_timestamp_ms(mut self, ms: i64) -> Self {
        self.timestamp_ms = ms.to_string();
        self
    }
}

impl<T: Transport> ExecutionClient for BybitExec<T> {
    fn submit(
        &mut self,
        id: ClientOrderId,
        order: OrderRequest,
        now: Timestamp,
        sink: &mut dyn EventSink,
    ) {
        let Some(m) = self.meta.get(order.instrument.index() as usize) else {
            sink.emit(AccountEvent::OrderRejected {
                id,
                reason: RejectReason::InvalidOrder,
                ts: now,
            });
            return;
        };
        // post-only is maker-only (limit kinds only); reject it on any other kind
        // rather than silently sending a taker order (bug class 1).
        if order.post_only && !matches!(order.kind, OrderKind::Limit { .. }) {
            sink.emit(AccountEvent::OrderRejected {
                id,
                reason: RejectReason::InvalidOrder,
                ts: now,
            });
            return;
        }
        let side = match order.side {
            Side::Buy => "Buy",
            Side::Sell => "Sell",
        };
        let qty = raw_to_decimal(order.qty.raw(), m.qty_scale);
        let tag = client_order_tag(id);
        let mut body = match order.kind {
            OrderKind::Market => format!(
                "{{\"category\":\"{}\",\"symbol\":\"{}\",\"side\":\"{side}\",\"orderType\":\"Market\",\"qty\":\"{qty}\",\"orderLinkId\":\"{tag}\"}}",
                m.category.as_str(),
                m.symbol
            ),
            OrderKind::Limit { limit } => {
                // Encode the time-in-force (bug class 1): post_only -> Bybit's
                // maker-only `PostOnly`, else IOC/FOK from the order's TIF, else GTC.
                // Bybit v5 defaults to GTC when the field is absent.
                let tif = if order.post_only {
                    "PostOnly"
                } else {
                    match order.tif {
                        TimeInForce::Ioc => "IOC",
                        TimeInForce::Fok => "FOK",
                        _ => "GTC",
                    }
                };
                format!(
                    "{{\"category\":\"{}\",\"symbol\":\"{}\",\"side\":\"{side}\",\"orderType\":\"Limit\",\"qty\":\"{qty}\",\"price\":\"{}\",\"timeInForce\":\"{tif}\",\"orderLinkId\":\"{tag}\"}}",
                    m.category.as_str(),
                    m.symbol,
                    raw_to_decimal(limit.raw(), m.price_scale)
                )
            }
            _ => {
                sink.emit(AccountEvent::OrderRejected {
                    id,
                    reason: RejectReason::InvalidOrder,
                    ts: now,
                });
                return;
            }
        };
        // reduce-only is a derivatives concept: inject it on linear-perp orders
        // (bug class 2). On spot it is silently dropped (Bybit spot has no
        // reduce-only), matching the field's no-op there.
        if order.reduce_only {
            if m.category == Category::Linear {
                body.insert_str(body.len() - 1, ",\"reduceOnly\":true");
            } else {
                // Spot has no reduce-only; reject rather than silently send a
                // position-increasing order — parity with OKX/KuCoin + the simulator.
                sink.emit(AccountEvent::OrderRejected {
                    id,
                    reason: RejectReason::InvalidOrder,
                    ts: now,
                });
                return;
            }
        }
        // Signing timestamp: the test/driver-set value, or the handler's event time
        // when unset — never sign with an empty string (which Bybit rejects), and the
        // signed bytes match the header (bug class 7).
        let timestamp_ms = if self.timestamp_ms.is_empty() {
            (now.as_nanos() / 1_000_000).to_string()
        } else {
            self.timestamp_ms.clone()
        };
        // POST signs over timestamp + api_key + recv_window + rawBody.
        let prehash = format!(
            "{}{}{}{}",
            timestamp_ms, self.api_key, self.recv_window, body
        );
        let signature = sign(&self.secret, &prehash);
        let req = HttpRequest {
            method: Method::Post,
            url: format!("{}/v5/order/create", self.base_url),
            body: Some(body),
            headers: vec![
                ("X-BAPI-API-KEY".into(), self.api_key.clone()),
                ("X-BAPI-SIGN".into(), signature),
                ("X-BAPI-TIMESTAMP".into(), timestamp_ms),
                ("X-BAPI-RECV-WINDOW".into(), self.recv_window.clone()),
                ("Content-Type".into(), "application/json".into()),
            ],
        };
        match self.transport.send(&req) {
            Ok(resp) if resp.is_success() && ret_code_ok(&resp.body) => {
                sink.emit(AccountEvent::OrderAccepted { id, ts: now });
            }
            Ok(_) | Err(_) => sink.emit(AccountEvent::OrderRejected {
                id,
                reason: RejectReason::VenueRejected,
                ts: now,
            }),
        }
    }

    fn observe(&mut self, _event: &Event, _now: Timestamp, _sink: &mut dyn EventSink) {
        // Live fills arrive on the Bybit `execution` WebSocket topic (akadro-live).
    }
}

/// `true` if a response carries `retCode:0` (request-level success).
fn ret_code_ok(body: &str) -> bool {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(rename = "retCode", default = "non_zero")]
        ret_code: i64,
    }
    fn non_zero() -> i64 {
        -1
    }
    serde_json::from_str::<Resp>(body).is_ok_and(|r| r.ret_code == 0)
}

impl<T> core::fmt::Debug for BybitExec<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BybitExec")
            .field("base_url", &self.base_url)
            .field("instruments", &self.meta.len())
            .finish_non_exhaustive()
    }
}

// --- live network transport (the `net` feature) ------------------------------

// The blocking `reqwest` transport is live IO (exercised only by the `#[ignore]`d
// live tests), so it lives in its own module to keep it out of the coverage gate.
#[cfg(feature = "net")]
mod net;
#[cfg(feature = "net")]
pub use net::{ReqwestTransport, unix_millis};

#[cfg(test)]
mod tests {
    use super::*;

    const INSTRUMENTS: &str = r#"{"retCode":0,"result":{"list":[
        {"symbol":"BTCUSDT","baseCoin":"BTC","quoteCoin":"USDT","status":"Trading",
         "priceFilter":{"tickSize":"0.01"},
         "lotSizeFilter":{"basePrecision":"0.000001","minOrderQty":"0.000048","minOrderAmt":"1"}},
        {"symbol":"DEADUSDT","baseCoin":"DEAD","quoteCoin":"USDT","status":"Closed",
         "priceFilter":{"tickSize":"0.1"},"lotSizeFilter":{"basePrecision":"0.1"}}
    ]}}"#;

    #[test]
    fn sign_matches_hex_hmac() {
        // RFC 4231 §4.3 HMAC-SHA256 (hex).
        assert_eq!(
            sign(b"Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn decimal_and_interval_helpers() {
        assert_eq!(scale_of("0.01"), 2);
        assert_eq!(decimal_to_raw("0.01", 2).unwrap(), 1);
        assert_eq!(raw_to_decimal(12345, 2), "123.45");
        assert!(decimal_to_raw("x", 2).is_err());
        assert_eq!(interval_millis("1"), 60_000);
        assert_eq!(interval_millis("240"), 14_400_000);
        assert_eq!(interval_millis("D"), 86_400_000);
        assert_eq!(interval_millis("?"), 0);
    }

    #[test]
    fn catalog_keeps_trading_symbols() {
        let cat = BybitCatalog::from_instruments(INSTRUMENTS, Category::Spot).unwrap();
        assert_eq!(cat.specs().len(), 1); // Closed skipped
        let id = cat.id_of("BTCUSDT").unwrap();
        assert_eq!(cat.scales(id), Some((2, 6)));
        let spec = cat.spec(id).unwrap();
        assert_eq!(spec.tick_size, Price::from_raw(1)); // 0.01 @ 2
        assert_eq!(spec.min_notional, Money::from_raw(100_000_000)); // minOrderAmt 1 @ (2+6)
        assert!(cat.id_of("NONE").is_none());
    }

    #[test]
    fn linear_catalog_uses_qty_step_and_is_perpetual() {
        let json = r#"{"retCode":0,"result":{"list":[
            {"symbol":"BTCUSDT","baseCoin":"BTC","quoteCoin":"USDT","status":"Trading",
             "priceFilter":{"tickSize":"0.1"},
             "lotSizeFilter":{"qtyStep":"0.001","minNotionalValue":"5"}}]}}"#;
        let cat = BybitCatalog::from_instruments(json, Category::Linear).unwrap();
        let id = cat.id_of("BTCUSDT").unwrap();
        assert_eq!(cat.scales(id), Some((1, 3)));
        assert_eq!(cat.spec(id).unwrap().kind, InstrumentKind::PerpetualFuture);
    }

    #[test]
    fn klines_parse_sorted_and_close_stamped() {
        let json = r#"{"retCode":0,"result":{"list":[
            ["1700000060000","105","106","104","105.5","8","x"],
            ["1700000000000","100","101","99","100.5","10","x"]
        ]}}"#;
        let bars = parse_klines(json, InstrumentId::new(0), 2, 0, "1").unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].ts.as_nanos(), 1_700_000_060_000_000_000); // open + 60s
        assert_eq!(bars[0].open, Price::from_raw(10_000));
        assert_eq!(bars[1].ts.as_nanos(), 1_700_000_120_000_000_000);
        assert!(
            parse_klines(
                r#"{"result":{"list":[["1","2"]]}}"#,
                InstrumentId::new(0),
                2,
                0,
                "1"
            )
            .is_err()
        );
    }

    #[test]
    fn funding_rate_parses_to_fine_scale_sorted() {
        // Bybit returns newest-first; both fields are strings.
        let json = r#"{"retCode":0,"result":{"list":[
            {"symbol":"BTCUSDT","fundingRate":"0.0001","fundingRateTimestamp":"1700028800000"},
            {"symbol":"BTCUSDT","fundingRate":"-0.0000466","fundingRateTimestamp":"1700000000000"}
        ]}}"#;
        let sched = parse_funding_rate(json).unwrap();
        assert_eq!(sched.len(), 2);
        // Sorted ascending; at FUNDING_RATE_SCALE (1e-8): 0.0001 → 10_000 (= 1 bp);
        // the sub-bp -0.0000466 → -4_660 (preserved, not rounded to 0).
        assert_eq!(
            sched[0],
            (Timestamp::from_nanos(1_700_000_000_000_000_000), -4_660)
        );
        assert_eq!(
            sched[1],
            (Timestamp::from_nanos(1_700_028_800_000_000_000), 10_000)
        );
        assert!(parse_funding_rate("nope").is_err());
        assert!(
            parse_funding_rate(
                r#"{"result":{"list":[{"fundingRate":"0.1","fundingRateTimestamp":"x"}]}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn fetch_funding_history_builds_linear_url() {
        let mut t = MockTransport::new(vec![HttpResponse {
            status: 200,
            body: r#"{"retCode":0,"result":{"list":[{"symbol":"BTCUSDT","fundingRate":"0.0001","fundingRateTimestamp":"1700000000000"}]}}"#.into(),
        }]);
        let sched = fetch_funding_history(&mut t, "http://x", "BTCUSDT", 200).unwrap();
        assert_eq!(sched.len(), 1);
        assert!(
            t.sent[0]
                .url
                .contains("/v5/market/funding/history?category=linear&symbol=BTCUSDT")
        );
        let mut bad = MockTransport::new(vec![HttpResponse {
            status: 500,
            body: String::new(),
        }]);
        assert!(fetch_funding_history(&mut bad, "http://x", "BTCUSDT", 200).is_err());
    }

    #[test]
    fn catalog_fetch_via_transport() {
        let mut t = MockTransport::new(vec![HttpResponse {
            status: 200,
            body: INSTRUMENTS.into(),
        }]);
        let cat = BybitCatalog::fetch(&mut t, "http://x", Category::Spot).unwrap();
        assert!(cat.id_of("BTCUSDT").is_some());
        assert!(
            t.sent[0]
                .url
                .contains("/v5/market/instruments-info?category=spot")
        );
        let mut bad = MockTransport::new(vec![HttpResponse {
            status: 503,
            body: String::new(),
        }]);
        assert!(BybitCatalog::fetch(&mut bad, "http://x", Category::Linear).is_err());
    }

    /// A `/v5/market/kline` page (flat OHLC=100) for the given **open** ms.
    fn kline_page(opens_ms: &[i64]) -> String {
        let rows = opens_ms
            .iter()
            .map(|o| format!(r#"["{o}","100","100","100","100","1","1"]"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"retCode":0,"result":{{"list":[{rows}]}}}}"#)
    }

    #[test]
    fn range_feed_pages_backward_by_end() {
        // Two 5-bar 1m pages spanning opens [0..540k] within [0, 600k); newest→oldest.
        let recent = kline_page(&[
            1_700_000_300_000,
            1_700_000_360_000,
            1_700_000_420_000,
            1_700_000_480_000,
            1_700_000_540_000,
        ]);
        let older = kline_page(&[
            1_700_000_000_000,
            1_700_000_060_000,
            1_700_000_120_000,
            1_700_000_180_000,
            1_700_000_240_000,
        ]);
        let mut feed = BybitKlineFeed::new(
            MockTransport::new(vec![
                HttpResponse {
                    status: 200,
                    body: recent,
                },
                HttpResponse {
                    status: 200,
                    body: older,
                },
            ]),
            "http://x",
            Category::Linear,
            "BTCUSDT",
            InstrumentId::new(0),
            "1",
            2,
            0,
        )
        .with_range(1_700_000_000_000, 1_700_000_600_000)
        .with_page_delay(std::time::Duration::ZERO);
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 10); // both pages, windowed
    }

    fn bybit_range_feed(
        bodies: Vec<HttpResponse>,
        start: i64,
        end: i64,
    ) -> BybitKlineFeed<MockTransport> {
        BybitKlineFeed::new(
            MockTransport::new(bodies),
            "http://x",
            Category::Linear,
            "BTCUSDT",
            InstrumentId::new(0),
            "1",
            2,
            0,
        )
        .with_range(start, end)
        .with_page_delay(std::time::Duration::ZERO)
    }

    fn ok(body: String) -> HttpResponse {
        HttpResponse { status: 200, body }
    }

    #[test]
    fn range_feed_guards_against_no_progress() {
        // The same page twice must not loop forever: the `end` cursor can't advance.
        let p = || ok(kline_page(&[1_700_000_300_000, 1_700_000_360_000]));
        let mut feed = bybit_range_feed(vec![p(), p()], 1_700_000_000_000, 1_700_000_600_000);
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 2); // de-duplicated; guard stopped the stall
    }

    #[test]
    fn range_feed_clips_window_and_dedups() {
        // One page with a duplicate and a bar OLDER than `start`: the dup collapses
        // and the out-of-window bar is clipped. Oldest open == start → stops.
        let page = kline_page(&[
            1_700_000_000_000, // < start → clipped
            1_700_000_060_000, // in window
            1_700_000_060_000, // duplicate
            1_700_000_120_000, // in window
        ]);
        let mut feed = bybit_range_feed(vec![ok(page)], 1_700_000_060_000, 1_700_000_600_000);
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 2); // 60k & 120k; 0 clipped, dup collapsed
    }

    #[test]
    fn range_feed_429_exhausted_yields_none() {
        let mut feed = bybit_range_feed(
            (0..10)
                .map(|_| HttpResponse {
                    status: 429,
                    body: String::new(),
                })
                .collect(),
            1_700_000_000_000,
            1_700_000_060_000,
        );
        assert!(feed.next_event().is_none());
    }

    #[test]
    fn range_feed_retries_on_429() {
        let mut feed = BybitKlineFeed::new(
            MockTransport::new(vec![
                HttpResponse {
                    status: 429,
                    body: String::new(),
                },
                HttpResponse {
                    status: 200,
                    body: kline_page(&[1_700_000_000_000]),
                },
            ]),
            "http://x",
            Category::Linear,
            "BTCUSDT",
            InstrumentId::new(0),
            "1",
            2,
            0,
        )
        .with_range(1_700_000_000_000, 1_700_000_060_000)
        .with_page_delay(std::time::Duration::ZERO);
        assert!(feed.next_event().is_some()); // survived the 429
    }

    #[test]
    fn funding_history_paged_walks_back() {
        let p1 = r#"{"retCode":0,"result":{"list":[
            {"symbol":"BTCUSDT","fundingRate":"0.0001","fundingRateTimestamp":"1700000120000"},
            {"symbol":"BTCUSDT","fundingRate":"0.0002","fundingRateTimestamp":"1700000060000"}]}}"#;
        let p2 = r#"{"retCode":0,"result":{"list":[
            {"symbol":"BTCUSDT","fundingRate":"-0.0001","fundingRateTimestamp":"1700000000000"}]}}"#;
        let mut t = MockTransport::new(vec![
            HttpResponse {
                status: 200,
                body: p1.into(),
            },
            HttpResponse {
                status: 200,
                body: p2.into(),
            },
        ]);
        let sched = fetch_funding_history_paged(
            &mut t,
            "http://x",
            "BTCUSDT",
            1_700_000_000_000,
            1_700_000_120_000,
            10,
        )
        .unwrap();
        assert_eq!(sched.len(), 3);
        assert!(t.sent[0].url.contains("endTime=1700000120000"));
        assert!(t.sent[1].url.contains("endTime=1700000059999")); // oldest(60000)-1
    }

    #[test]
    fn funding_history_paged_guards_no_progress() {
        // Same page twice (oldest above `start`): the `endTime` cursor stalls → stop.
        let page = r#"{"retCode":0,"result":{"list":[
            {"symbol":"BTCUSDT","fundingRate":"0.0001","fundingRateTimestamp":"1700000120000"},
            {"symbol":"BTCUSDT","fundingRate":"0.0002","fundingRateTimestamp":"1700000060000"}]}}"#;
        let mut t = MockTransport::new(vec![
            HttpResponse {
                status: 200,
                body: page.into(),
            },
            HttpResponse {
                status: 200,
                body: page.into(),
            },
        ]);
        let sched =
            fetch_funding_history_paged(&mut t, "http://x", "BTCUSDT", 0, 1_700_000_120_000, 10)
                .unwrap();
        assert_eq!(sched.len(), 2); // de-duplicated; guard stopped the stall
        assert_eq!(t.sent.len(), 2);
        let mut bad = MockTransport::new(vec![HttpResponse {
            status: 500,
            body: String::new(),
        }]);
        assert!(fetch_funding_history_paged(&mut bad, "http://x", "B", 0, 1, 10).is_err());
    }

    #[test]
    fn kline_feed_streams_and_http_errors() {
        let body = r#"{"retCode":0,"result":{"list":[["1700000000000","100","100","100","100","1","x"]]}}"#;
        let mut feed = BybitKlineFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 200,
                body: body.into(),
            }]),
            "http://x",
            Category::Spot,
            "BTCUSDT",
            InstrumentId::new(0),
            "1",
            2,
            0,
        )
        .with_limit(5000);
        assert!(feed.next_event().is_some());
        assert!(feed.next_event().is_none());
        assert!(format!("{feed:?}").contains("BybitKlineFeed"));

        let mut err = BybitKlineFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 500,
                body: "x".into(),
            }]),
            "http://x",
            Category::Spot,
            "BTCUSDT",
            InstrumentId::new(0),
            "1",
            2,
            0,
        );
        assert!(err.next_event().is_none());
    }

    struct SinkVec(Vec<AccountEvent>);
    impl EventSink for SinkVec {
        fn emit(&mut self, e: AccountEvent) {
            self.0.push(e);
        }
    }

    fn exec_with(responses: Vec<HttpResponse>, category: Category) -> BybitExec<MockTransport> {
        BybitExec::new(
            MockTransport::new(responses),
            "http://x",
            "KEY",
            b"secret".to_vec(),
            vec![InstrumentMeta {
                symbol: "BTCUSDT".into(),
                price_scale: 2,
                qty_scale: 4,
                category,
            }],
        )
        .with_timestamp_ms(1_700_000_000_000)
    }

    #[test]
    fn debug_hides_secret() {
        // Credential-hiding regression lock (secret = b"secret" via exec_with).
        let dbg = format!("{:?}", exec_with(vec![], Category::Spot));
        assert!(
            dbg.contains("BybitExec"),
            "Debug should still name the type"
        );
        assert!(
            !dbg.contains("secret"),
            "Debug must not leak the API secret"
        );
    }

    #[test]
    fn submit_signs_and_accepts_on_ret_code_zero() {
        let mut x = exec_with(
            vec![HttpResponse {
                status: 200,
                body: r#"{"retCode":0,"retMsg":"OK","result":{"orderId":"1","orderLinkId":"ak0"}}"#
                    .into(),
            }],
            Category::Spot,
        );
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(InstrumentId::new(0), Side::Buy, Qty::from_raw(100)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(s.0[0], AccountEvent::OrderAccepted { .. }));
        let sent = &x.transport.sent[0];
        let body = sent.body.as_deref().unwrap();
        assert!(body.contains("\"orderType\":\"Market\""));
        assert!(body.contains("\"qty\":\"0.01\"")); // 100 @ 4
        assert!(body.contains("\"orderLinkId\":\"ak0\""));
        let names: Vec<&str> = sent.headers.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"X-BAPI-SIGN"));
    }

    #[test]
    fn submit_limit_linear_and_debug() {
        let mut x = exec_with(
            vec![HttpResponse {
                status: 200,
                body: r#"{"retCode":0}"#.into(),
            }],
            Category::Linear,
        );
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::limit(
                InstrumentId::new(0),
                Side::Sell,
                Qty::from_raw(100),
                Price::from_raw(5000),
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(s.0[0], AccountEvent::OrderAccepted { .. }));
        let body = x.transport.sent[0].body.as_deref().unwrap();
        assert!(body.contains("\"category\":\"linear\""));
        assert!(body.contains("\"price\":\"50\"")); // 5000 @ 2
        assert!(format!("{x:?}").contains("BybitExec"));
    }

    #[test]
    fn submit_encodes_tif_post_only_reduce_only() {
        let ok = || HttpResponse {
            status: 200,
            body: r#"{"retCode":0}"#.into(),
        };
        // IOC limit -> timeInForce IOC (Bybit defaults to GTC when absent).
        let mut x = exec_with(vec![ok()], Category::Spot);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(
                InstrumentId::new(0),
                Side::Buy,
                Qty::from_raw(100),
                Price::from_raw(1000),
            )
            .with_tif(TimeInForce::Ioc),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(
            x.transport.sent[0]
                .body
                .as_deref()
                .unwrap()
                .contains("\"timeInForce\":\"IOC\"")
        );

        // post_only limit -> PostOnly (maker-only, not a taker GTC limit).
        let mut x = exec_with(vec![ok()], Category::Spot);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::limit(
                InstrumentId::new(0),
                Side::Sell,
                Qty::from_raw(100),
                Price::from_raw(1000),
            )
            .post_only(),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(
            x.transport.sent[0]
                .body
                .as_deref()
                .unwrap()
                .contains("\"timeInForce\":\"PostOnly\"")
        );

        // linear-perp reduce_only -> reduceOnly:true.
        let mut x = exec_with(vec![ok()], Category::Linear);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(2),
            OrderRequest::market(InstrumentId::new(0), Side::Sell, Qty::from_raw(100))
                .reduce_only(),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(
            x.transport.sent[0]
                .body
                .as_deref()
                .unwrap()
                .contains("\"reduceOnly\":true")
        );

        // post_only on a market order -> rejected, no request sent.
        let mut x = exec_with(vec![], Category::Spot);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(3),
            OrderRequest::market(InstrumentId::new(0), Side::Buy, Qty::from_raw(100)).post_only(),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(s.0[0], AccountEvent::OrderRejected { .. }));
        assert!(x.transport.sent.is_empty());
    }

    #[test]
    fn submit_rejections_and_noop_observe() {
        // retCode != 0 → rejected.
        let mut x = exec_with(
            vec![HttpResponse {
                status: 200,
                body: r#"{"retCode":110007,"retMsg":"insufficient"}"#.into(),
            }],
            Category::Spot,
        );
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(InstrumentId::new(0), Side::Buy, Qty::from_raw(1)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(
            s.0[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::VenueRejected,
                ..
            }
        ));
        // Unknown instrument → InvalidOrder, no call.
        let mut x2 = exec_with(vec![], Category::Spot);
        let mut s2 = SinkVec(Vec::new());
        x2.submit(
            ClientOrderId::new(1),
            OrderRequest::market(InstrumentId::new(9), Side::Buy, Qty::from_raw(1)),
            Timestamp::from_nanos(1),
            &mut s2,
        );
        assert!(matches!(
            s2.0[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::InvalidOrder,
                ..
            }
        ));
        assert!(x2.transport.sent.is_empty());
        x2.observe(
            &Event::Bar(Bar::new(
                InstrumentId::new(0),
                Timestamp::from_nanos(1),
                Price::from_raw(1),
                Price::from_raw(1),
                Price::from_raw(1),
                Price::from_raw(1),
                Qty::from_raw(1),
            )),
            Timestamp::from_nanos(1),
            &mut s2,
        );
    }

    #[test]
    fn order_tag_round_trips() {
        assert_eq!(client_order_tag(ClientOrderId::new(7)), "ak7");
        assert_eq!(parse_client_order_tag("ak7"), Some(ClientOrderId::new(7)));
        assert_eq!(parse_client_order_tag("x"), None);
    }

    #[test]
    fn edge_paths() {
        use akadro_core::TriggerBy;
        assert_eq!(scale_of("100"), 0);
        assert_eq!(raw_to_decimal(-50, 2), "-0.5");
        assert_eq!(Category::Spot.as_str(), "spot");
        // Asset reuse (shared USDT) + a symbol missing its filters (skipped).
        let json = r#"{"retCode":0,"result":{"list":[
            {"symbol":"BTCUSDT","baseCoin":"BTC","quoteCoin":"USDT","status":"Trading","priceFilter":{"tickSize":"0.1"},"lotSizeFilter":{"basePrecision":"0.1","minOrderAmt":"1"}},
            {"symbol":"ETHUSDT","baseCoin":"ETH","quoteCoin":"USDT","status":"Trading","priceFilter":{"tickSize":"0.01"},"lotSizeFilter":{"basePrecision":"0.01","minOrderAmt":"1"}},
            {"symbol":"NOFILT","baseCoin":"X","quoteCoin":"USDT","status":"Trading","priceFilter":{"tickSize":""},"lotSizeFilter":{}}
        ]}}"#;
        let cat = BybitCatalog::from_instruments(json, Category::Spot).unwrap();
        assert_eq!(cat.specs().len(), 2); // NOFILT skipped

        let mut feed = BybitKlineFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 200,
                body: r#"{"result":{"list":[["1"]]}}"#.into(),
            }]),
            "http://x",
            Category::Spot,
            "BTCUSDT",
            InstrumentId::new(0),
            "1",
            2,
            0,
        );
        assert!(feed.next_event().is_none()); // parse error → no events

        let mut x = exec_with(vec![], Category::Spot);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::stop(
                InstrumentId::new(0),
                Side::Buy,
                Qty::from_raw(1),
                Price::from_raw(1),
                TriggerBy::Last,
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(
            s.0[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::InvalidOrder,
                ..
            }
        ));
        assert!(x.transport.sent.is_empty());
    }
}
