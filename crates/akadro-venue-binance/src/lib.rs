// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Binance venue connector (spot + USDⓈ-M futures, REST) for akadro — depends
//! only on `akadro-core`, so it adds a venue with **zero** changes to the engine
//! or strategies (the open/closed goal).
//!
//! It provides a [`BinanceCatalog`] (`exchangeInfo` → [`InstrumentSpec`]), a
//! [`BinanceKlineFeed`] ([`DataSource`] over `/api/v3/klines`), and a
//! [`BinanceSpotExec`] ([`ExecutionClient`] that signs and submits orders). All
//! logic talks to Binance through the mockable [`Transport`] seam, so signing,
//! parsing and normalization are fixture-tested with no network; the live
//! `reqwest` transport is behind the `net` feature. Binance signs the exact query
//! string with `HMAC-SHA256` and carries the key in `X-MBX-APIKEY` — identical to
//! the (Binance-derived) MEXC spot scheme.
//!
//! Live **fills** arrive on the user-data WebSocket stream (the `akadro-live`
//! shell), not by REST polling, so [`BinanceSpotExec::observe`] is intentionally a
//! no-op on bars; `submit` acknowledges synchronously via the REST ack.

use core::fmt::Write as _;

use akadro_core::{
    AccountEvent, AssetId, Bar, CapSet, Capability, ClientOrderId, DataSource, Event, EventSink,
    ExecutionClient, InstrumentCatalog, InstrumentId, InstrumentKind, InstrumentSpec, Money,
    OrderKind, OrderRequest, Price, Qty, RejectReason, Side, TimeInForce, Timestamp,
};
use serde::Deserialize;

pub mod futures;
pub mod vision;
pub mod ws;
pub use futures::{
    BinanceSignalFeed, FUTURES_BASE_URL, fetch_funding_history, fetch_funding_history_paged,
    parse_funding_rate, parse_long_short_ratio, parse_open_interest,
};
#[cfg(all(feature = "net", feature = "vision-zip"))]
pub use net::VisionDownloader;
pub use vision::{
    VISION_BASE, VisionBarSource, VisionGranularity, VisionTimeUnit, parse_checksum,
    parse_vision_csv, parse_vision_row, vision_checksum_url, vision_zip_url,
};
#[cfg(feature = "vision-zip")]
pub use vision::{unzip_single_csv, verify_and_unzip};
pub use ws::{
    WsExecReport, client_order_tag, parse_client_order_tag, parse_execution_report,
    parse_listen_key, parse_ws_kline,
};

mod sign {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;

    /// `lowercase_hex(HMAC_SHA256(secret, total_params))` — the Binance signature
    /// over the exact transmitted query string.
    ///
    /// # Panics
    /// Never in practice — HMAC-SHA256 accepts a key of any length.
    #[must_use]
    pub fn sign(secret: &[u8], total_params: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC key any length");
        mac.update(total_params.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}
pub use sign::sign;

/// Connector errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BinanceError {
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
    /// `DELETE`.
    Delete,
}

/// An outbound HTTP request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// Method.
    pub method: Method,
    /// Full URL (base + path + query).
    pub url: String,
    /// `X-MBX-APIKEY` header value, if authenticated.
    pub api_key: Option<String>,
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
    /// [`BinanceError::Transport`] on a network/transport failure.
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, BinanceError>;
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
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, BinanceError> {
        self.sent.push(request.clone());
        self.responses
            .pop_front()
            .ok_or_else(|| BinanceError::Transport("no canned response".into()))
    }
}

// --- fixed-point decimal helpers ---------------------------------------------

/// Number of significant fractional digits in a decimal string (trailing zeros
/// stripped): `"0.01000000"` → 2, `"1.00000000"` → 0.
#[must_use]
pub fn scale_of(decimal: &str) -> u32 {
    match decimal.split_once('.') {
        Some((_, frac)) => frac.trim_end_matches('0').len() as u32,
        None => 0,
    }
}

/// Parse a decimal string to a raw fixed-point `i64` at `scale` (truncating any
/// digits beyond `scale`). `"0.01"` @ scale 2 → `1`; `"123.456"` @ 2 → `12345`.
///
/// # Errors
/// [`BinanceError::Parse`] if the integer/fraction parts are not digits.
pub fn decimal_to_raw(s: &str, scale: u32) -> Result<i64, BinanceError> {
    let neg = s.starts_with('-');
    let s = s.trim_start_matches('-');
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    let int_part = if int_part.is_empty() { "0" } else { int_part };
    let mut frac = frac_part.to_string();
    let scale_us = scale as usize;
    frac.truncate(scale_us); // drop excess precision
    while frac.len() < scale_us {
        frac.push('0');
    }
    let combined = format!("{int_part}{frac}");
    let val: i64 = combined
        .parse()
        .map_err(|_| BinanceError::Parse(format!("bad decimal {s:?}")))?;
    Ok(if neg { -val } else { val })
}

/// Format a raw fixed-point `i64` at `scale` back to a decimal string for a query
/// param: `(12345, 2)` → `"123.45"`, `(50, 0)` → `"50"`.
#[must_use]
pub fn raw_to_decimal(raw: i64, scale: u32) -> String {
    // Clamp to the largest base-10 exponent a u128 holds (`10u128.pow(39)`
    // overflows). No real venue uses a scale this large, but the guard prevents a
    // panic (m13).
    let scale = scale.min(38);
    if scale == 0 {
        return raw.to_string();
    }
    let neg = raw < 0;
    let mag = raw.unsigned_abs();
    let div = 10u128.pow(scale);
    let int = (u128::from(mag) / div).to_string();
    let frac = format!("{:0width$}", u128::from(mag) % div, width = scale as usize);
    let frac = frac.trim_end_matches('0');
    let s = if frac.is_empty() {
        int
    } else {
        format!("{int}.{frac}")
    };
    if neg { format!("-{s}") } else { s }
}

// --- catalogue (exchangeInfo → InstrumentSpec) -------------------------------

#[derive(Deserialize)]
struct ExchangeInfo {
    #[serde(default)]
    symbols: Vec<SymbolInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SymbolInfo {
    symbol: String,
    base_asset: String,
    quote_asset: String,
    #[serde(default)]
    status: String,
    /// Futures only: `"PERPETUAL"`, `"CURRENT_QUARTER"`, … Absent on spot.
    #[serde(default)]
    contract_type: String,
    #[serde(default)]
    filters: Vec<Filter>,
}

#[derive(Deserialize)]
#[serde(tag = "filterType")]
enum Filter {
    #[serde(rename = "PRICE_FILTER")]
    Price {
        #[serde(rename = "tickSize")]
        tick_size: String,
    },
    #[serde(rename = "LOT_SIZE")]
    Lot {
        #[serde(rename = "stepSize")]
        step_size: String,
    },
    // Spot exchangeInfo spells it `NOTIONAL`/`minNotional`; USDⓈ-M futures spells
    // the same constraint `MIN_NOTIONAL`/`notional`. Accept both.
    #[serde(rename = "NOTIONAL")]
    Notional {
        #[serde(rename = "minNotional")]
        min_notional: String,
    },
    #[serde(rename = "MIN_NOTIONAL")]
    MinNotional { notional: String },
    #[serde(other)]
    Other,
}

/// Maps Binance symbols to dense [`InstrumentSpec`]s and asset ids.
#[derive(Debug, Clone)]
pub struct BinanceCatalog {
    specs: Vec<InstrumentSpec>,
    symbols: Vec<String>,
    scales: Vec<(u32, u32)>,
}

impl BinanceCatalog {
    /// Parse a spot `/api/v3/exchangeInfo` body into a catalogue. Only `TRADING`
    /// symbols with a price/lot filter are included; each is assigned a dense
    /// [`InstrumentId`] in listing order.
    ///
    /// # Errors
    /// [`BinanceError::Parse`] on unparseable JSON or a bad filter value.
    pub fn from_exchange_info(json: &str) -> Result<Self, BinanceError> {
        Self::parse(json, InstrumentKind::Spot, false)
    }

    /// Fetch spot `/api/v3/exchangeInfo` over `transport` and parse it into a
    /// catalogue (see [`from_exchange_info`](Self::from_exchange_info)). For USDⓈ-M
    /// futures use [`fetch_futures`](Self::fetch_futures). The reusable connector
    /// entry point — callers never build the URL or touch the response body.
    ///
    /// # Errors
    /// [`BinanceError::Transport`] on a transport failure or a non-2xx status;
    /// [`BinanceError::Parse`] on malformed JSON.
    pub fn fetch<T: Transport>(transport: &mut T, base_url: &str) -> Result<Self, BinanceError> {
        let body = Self::get(transport, &format!("{base_url}/api/v3/exchangeInfo"))?;
        Self::from_exchange_info(&body)
    }

    /// Fetch USDⓈ-M futures `/fapi/v1/exchangeInfo` over `transport` (pass
    /// [`FUTURES_BASE_URL`]) and parse it into a perpetual catalogue (see
    /// [`from_futures_exchange_info`](Self::from_futures_exchange_info)).
    ///
    /// # Errors
    /// [`BinanceError::Transport`] on a transport failure or a non-2xx status;
    /// [`BinanceError::Parse`] on malformed JSON.
    pub fn fetch_futures<T: Transport>(
        transport: &mut T,
        base_url: &str,
    ) -> Result<Self, BinanceError> {
        let body = Self::get(transport, &format!("{base_url}/fapi/v1/exchangeInfo"))?;
        Self::from_futures_exchange_info(&body)
    }

    /// Unsigned `GET` returning the body, erroring on a non-2xx status.
    fn get<T: Transport>(transport: &mut T, url: &str) -> Result<String, BinanceError> {
        let resp = transport.send(&HttpRequest {
            method: Method::Get,
            url: url.to_string(),
            api_key: None,
        })?;
        if !resp.is_success() {
            return Err(BinanceError::Transport(format!(
                "exchangeInfo HTTP {}",
                resp.status
            )));
        }
        Ok(resp.body)
    }

    /// Parse a USDⓈ-M futures `/fapi/v1/exchangeInfo` body into a catalogue of
    /// [`InstrumentKind::PerpetualFuture`]s. Only `TRADING`, `PERPETUAL` contracts
    /// are included (dated futures are skipped); each is assigned a dense
    /// [`InstrumentId`] in listing order.
    ///
    /// # Errors
    /// [`BinanceError::Parse`] on unparseable JSON or a bad filter value.
    pub fn from_futures_exchange_info(json: &str) -> Result<Self, BinanceError> {
        Self::parse(json, InstrumentKind::PerpetualFuture, true)
    }

    fn parse(json: &str, kind: InstrumentKind, perpetual_only: bool) -> Result<Self, BinanceError> {
        let info: ExchangeInfo =
            serde_json::from_str(json).map_err(|e| BinanceError::Parse(e.to_string()))?;
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
        let caps = if kind == InstrumentKind::PerpetualFuture {
            CapSet::empty()
                .with(Capability::LimitOrders)
                .with(Capability::StopOrders)
                .with(Capability::ReduceOnly)
                .with(Capability::Margin)
                .with(Capability::Funding)
                .with(Capability::ShortSelling)
        } else {
            CapSet::empty().with(Capability::LimitOrders)
        };
        for s in info.symbols {
            if s.status != "TRADING" {
                continue;
            }
            if perpetual_only && s.contract_type != "PERPETUAL" {
                continue; // skip dated/quarterly futures
            }
            let (mut tick, mut step, mut min_notional) = (None, None, None);
            for f in &s.filters {
                match f {
                    Filter::Price { tick_size } => tick = Some(tick_size.clone()),
                    Filter::Lot { step_size } => step = Some(step_size.clone()),
                    Filter::Notional { min_notional: m } | Filter::MinNotional { notional: m } => {
                        min_notional = Some(m.clone());
                    }
                    Filter::Other => {}
                }
            }
            let (Some(tick), Some(step)) = (tick, step) else {
                continue; // need both a price and lot grid to trade it
            };
            let price_scale = scale_of(&tick);
            let qty_scale = scale_of(&step);
            let id = InstrumentId::new(specs.len() as u32);
            let base = asset_id(&s.base_asset, &mut assets);
            let quote = asset_id(&s.quote_asset, &mut assets);
            let min_notional_raw = match &min_notional {
                Some(m) => decimal_to_raw(m, price_scale + qty_scale)?,
                None => 0,
            };
            specs.push(InstrumentSpec::new(
                id,
                base,
                quote,
                kind,
                Price::from_raw(decimal_to_raw(&tick, price_scale)?),
                Qty::from_raw(decimal_to_raw(&step, qty_scale)?),
                Money::from_raw(i128::from(min_notional_raw)),
                caps,
            ));
            symbols.push(s.symbol);
            scales.push((price_scale, qty_scale));
        }
        Ok(BinanceCatalog {
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

    /// The [`InstrumentId`] for `symbol`, if present.
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

impl InstrumentCatalog for BinanceCatalog {
    fn spec(&self, id: InstrumentId) -> Option<&InstrumentSpec> {
        self.specs.get(id.index() as usize)
    }
}

// --- klines (DataSource) -----------------------------------------------------

/// Binance's hard cap on the klines page size.
pub const MAX_KLINES_LIMIT: u32 = 1000;

/// Parse a `/api/v3/klines` array body into [`Bar`]s (row layout
/// `[openTime, "o","h","l","c","v", closeTime, …]`; stamped at close time).
///
/// # Errors
/// [`BinanceError::Parse`] on bad JSON, a short row, or a bad number.
pub fn parse_klines(
    json: &str,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
) -> Result<Vec<Bar>, BinanceError> {
    let rows: Vec<Vec<serde_json::Value>> =
        serde_json::from_str(json).map_err(|e| BinanceError::Parse(e.to_string()))?;
    let mut bars = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() < 7 {
            return Err(BinanceError::Parse(format!(
                "kline row has {} fields",
                row.len()
            )));
        }
        let s = |i: usize| -> Result<&str, BinanceError> {
            row[i]
                .as_str()
                .ok_or_else(|| BinanceError::Parse(format!("kline[{i}] not a string")))
        };
        let close_ms = row[6]
            .as_i64()
            .ok_or_else(|| BinanceError::Parse("kline[6] closeTime not int".into()))?;
        bars.push(Bar::new(
            instrument,
            Timestamp::from_nanos(
                close_ms
                    .checked_mul(1_000_000)
                    .ok_or_else(|| BinanceError::Parse("closeTime overflow".into()))?,
            ),
            Price::from_raw(decimal_to_raw(s(1)?, price_scale)?),
            Price::from_raw(decimal_to_raw(s(2)?, price_scale)?),
            Price::from_raw(decimal_to_raw(s(3)?, price_scale)?),
            Price::from_raw(decimal_to_raw(s(4)?, price_scale)?),
            Qty::from_raw(decimal_to_raw(s(5)?, qty_scale)?),
        ));
    }
    Ok(bars)
}

/// A [`DataSource`] streaming Binance spot klines for one instrument, paginating
/// the requested range across the venue's per-call cap.
pub struct BinanceKlineFeed<T> {
    transport: T,
    base_url: String,
    symbol: String,
    instrument: InstrumentId,
    interval: String,
    price_scale: u32,
    qty_scale: u32,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    limit: u32,
    klines_path: &'static str,
    /// Inter-page courtesy delay + 429 back-off unit; `0` in tests.
    page_delay: std::time::Duration,
    /// Bounded retries on a `429`/`418` (rate-limited) page.
    max_retries: u32,
    buffer: std::collections::VecDeque<Bar>,
    fetched: bool,
}

impl<T: Transport> BinanceKlineFeed<T> {
    /// Create a spot feed for `symbol` (mapped to `instrument`) at `interval`.
    /// For USDⓈ-M futures pass the futures base URL and chain [`Self::for_futures`].
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
        BinanceKlineFeed {
            transport,
            base_url: base_url.into(),
            symbol: symbol.into(),
            instrument,
            interval: interval.into(),
            price_scale,
            qty_scale,
            start_ms: None,
            end_ms: None,
            limit: 500,
            klines_path: "/api/v3/klines",
            page_delay: std::time::Duration::from_millis(120),
            max_retries: 8,
            buffer: std::collections::VecDeque::new(),
            fetched: false,
        }
    }

    /// Switch this feed to the USDⓈ-M futures klines path (`/fapi/v1/klines`).
    /// Pair with the futures base URL ([`FUTURES_BASE_URL`]); the row layout is
    /// identical to spot.
    #[must_use]
    pub fn for_futures(mut self) -> Self {
        self.klines_path = "/fapi/v1/klines";
        self
    }

    /// Restrict to `[start_ms, end_ms)` and back-fill the whole window.
    #[must_use]
    pub fn with_range(mut self, start_ms: i64, end_ms: i64) -> Self {
        self.start_ms = Some(start_ms);
        self.end_ms = Some(end_ms);
        self
    }

    /// Set the page size (clamped to [`MAX_KLINES_LIMIT`]).
    #[must_use]
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = limit.min(MAX_KLINES_LIMIT);
        self
    }

    /// Override the inter-page courtesy delay (default 120 ms; also the 429/418
    /// back-off unit) — matters for long back-fills that take many pages. Set to
    /// [`Duration::ZERO`](std::time::Duration::ZERO) in tests to page without sleeping.
    #[must_use]
    pub fn with_page_delay(mut self, delay: std::time::Duration) -> Self {
        self.page_delay = delay;
        self
    }

    fn fetch_page(
        &mut self,
        start: Option<i64>,
        end: Option<i64>,
    ) -> Result<Vec<Bar>, BinanceError> {
        let mut q = format!(
            "symbol={}&interval={}&limit={}",
            self.symbol, self.interval, self.limit
        );
        if let Some(s) = start {
            let _ = write!(q, "&startTime={s}");
        }
        if let Some(e) = end {
            let _ = write!(q, "&endTime={e}");
        }
        let url = format!("{}{}?{}", self.base_url, self.klines_path, q);
        // Bounded back-off on a rate-limit (429) / IP-ban (418); zero at `page_delay`
        // 0 (tests). Klines are an unsigned public request, so a retry is safe.
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
                api_key: None,
            })?;
            if (resp.status == 429 || resp.status == 418) && retries < self.max_retries {
                retries += 1;
                if !backoff.is_zero() {
                    std::thread::sleep(backoff);
                }
                continue;
            }
            if !resp.is_success() {
                return Err(BinanceError::Transport(format!(
                    "klines HTTP {}",
                    resp.status
                )));
            }
            return parse_klines(
                &resp.body,
                self.instrument,
                self.price_scale,
                self.qty_scale,
            );
        }
    }

    fn fetch(&mut self) -> Result<(), BinanceError> {
        if let (Some(start), Some(end)) = (self.start_ms, self.end_ms) {
            let mut cursor = start;
            while cursor < end {
                let page = self.fetch_page(Some(cursor), Some(end))?;
                let Some(last_bar) = page.last() else { break };
                let last = last_bar.ts.as_nanos() / 1_000_000;
                self.buffer.extend(page);
                if last <= cursor {
                    break;
                }
                cursor = last + 1;
                if !self.page_delay.is_zero() {
                    std::thread::sleep(self.page_delay); // pace long back-fills
                }
            }
        } else {
            let page = self.fetch_page(self.start_ms, self.end_ms)?;
            self.buffer.extend(page);
        }
        Ok(())
    }
}

impl<T: Transport> DataSource for BinanceKlineFeed<T> {
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

impl<T> core::fmt::Debug for BinanceKlineFeed<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BinanceKlineFeed")
            .field("symbol", &self.symbol)
            .field("interval", &self.interval)
            .field("buffered", &self.buffer.len())
            .finish_non_exhaustive()
    }
}

// --- spot execution client ---------------------------------------------------

/// One instrument's symbol + scales, for [`BinanceSpotExec`] order encoding.
#[derive(Debug, Clone)]
pub struct SymbolMeta {
    /// Venue symbol (e.g. `"BTCUSDT"`).
    pub symbol: String,
    /// Price fixed-point scale.
    pub price_scale: u32,
    /// Quantity fixed-point scale.
    pub qty_scale: u32,
}

/// A REST [`ExecutionClient`] that signs and submits orders to Binance. `submit`
/// posts a signed order and emits [`AccountEvent::OrderAccepted`] on a 2xx ack or
/// [`AccountEvent::OrderRejected`] otherwise. `observe` is a no-op: live **fills**
/// arrive on the user-data WebSocket stream (the `akadro-live` shell), not by REST
/// polling.
///
/// Defaults to spot (`/api/v3/order`); chain [`Self::for_futures`] (with the
/// futures base URL) for USDⓈ-M futures (`/fapi/v1/order`, where `reduce_only`
/// orders are forwarded as `reduceOnly=true`). The HMAC-SHA256 signing scheme is
/// identical across both.
pub struct BinanceSpotExec<T> {
    transport: T,
    base_url: String,
    api_key: String,
    secret: Vec<u8>,
    meta: Vec<SymbolMeta>,
    order_path: &'static str,
    futures: bool,
    /// Wall-clock (ms) the live driver sets for request signing; `0` falls back to
    /// the handler's event time. Plus a server-time offset (m15).
    clock_ms: i64,
    time_offset_ms: i64,
}

impl<T: Transport> BinanceSpotExec<T> {
    /// Create with credentials and per-instrument [`SymbolMeta`] (dense, in id
    /// order — typically derived from a [`BinanceCatalog`]).
    #[must_use]
    pub fn new(
        transport: T,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        secret: impl Into<Vec<u8>>,
        meta: Vec<SymbolMeta>,
    ) -> Self {
        BinanceSpotExec {
            transport,
            base_url: base_url.into(),
            api_key: api_key.into(),
            secret: secret.into(),
            meta,
            order_path: "/api/v3/order",
            futures: false,
            clock_ms: 0,
            time_offset_ms: 0,
        }
    }

    /// Switch to USDⓈ-M futures order submission (`/fapi/v1/order`); `reduce_only`
    /// requests are forwarded as `reduceOnly=true`. Pair with [`FUTURES_BASE_URL`].
    #[must_use]
    pub fn for_futures(mut self) -> Self {
        self.order_path = "/fapi/v1/order";
        self.futures = true;
        self
    }

    /// Set the wall-clock (epoch ms) used to sign requests, corrected by any synced
    /// server offset. A live driver should call this each session/bar so the
    /// `timestamp` reflects real time (not the logical event clock) and stays within
    /// the venue's `recvWindow`; otherwise signing falls back to the handler's event
    /// time, which can drift past `recvWindow` and draw a `-1021` rejection (m15).
    pub fn set_clock_ms(&mut self, ms: i64) {
        self.clock_ms = ms;
    }

    /// Record a server-time offset (`serverTime − local_clock_ms`) applied to every
    /// signed request, mirroring the MEXC connector. Fetch `serverTime` from
    /// `GET /api/v3/time` in the driver and pass it with the local clock used.
    pub fn sync_server_time(&mut self, server_time_ms: i64, local_clock_ms: i64) {
        self.time_offset_ms = server_time_ms - local_clock_ms;
    }

    /// The signing timestamp (ms): the driver-set wall clock plus server offset, or
    /// the handler's event time as a fallback when no clock has been set (m15).
    fn signing_clock_ms(&self, now: Timestamp) -> i64 {
        if self.clock_ms != 0 {
            self.clock_ms.saturating_add(self.time_offset_ms)
        } else {
            now.as_nanos() / 1_000_000
        }
    }
}

impl<T: Transport> ExecutionClient for BinanceSpotExec<T> {
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
        let side = match order.side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };
        // post-only is only meaningful on a limit order (handled below as
        // LIMIT_MAKER); on any other kind it is a contradiction — reject locally.
        if order.post_only && !matches!(order.kind, OrderKind::Limit { .. }) {
            sink.emit(AccountEvent::OrderRejected {
                id,
                reason: RejectReason::InvalidOrder,
                ts: now,
            });
            return;
        }
        let ts_ms = self.signing_clock_ms(now);
        let qty = raw_to_decimal(order.qty.raw(), m.qty_scale);
        let mut params = match order.kind {
            OrderKind::Market => {
                format!("symbol={}&side={side}&type=MARKET&quantity={qty}", m.symbol)
            }
            OrderKind::Limit { limit } => {
                let price = raw_to_decimal(limit.raw(), m.price_scale);
                if order.post_only {
                    // Maker-only: Binance `LIMIT_MAKER` (no timeInForce), rejected by
                    // the venue if it would cross — mirrors the backtest's post-only
                    // semantics (M1).
                    format!(
                        "symbol={}&side={side}&type=LIMIT_MAKER&quantity={qty}&price={price}",
                        m.symbol
                    )
                } else {
                    // Honour the order's time-in-force instead of hard-coding GTC,
                    // so IOC/FOK behave the same live as in backtest (M1).
                    let tif = match order.tif {
                        TimeInForce::Ioc => "IOC",
                        TimeInForce::Fok => "FOK",
                        // Gtc and any future TIF default to GTC.
                        _ => "GTC",
                    };
                    format!(
                        "symbol={}&side={side}&type=LIMIT&timeInForce={tif}&quantity={qty}&price={price}",
                        m.symbol
                    )
                }
            }
            _ => {
                // stop / trigger kinds need separate fields; not encoded here.
                sink.emit(AccountEvent::OrderRejected {
                    id,
                    reason: RejectReason::InvalidOrder,
                    ts: now,
                });
                return;
            }
        };
        if self.futures && order.reduce_only {
            params.push_str("&reduceOnly=true");
        }
        // Tag the order so a live user-data fill attributes back to this id
        // (see `ws::client_order_tag`); part of the signed query.
        let _ = write!(params, "&newClientOrderId={}", ws::client_order_tag(id));
        let _ = write!(params, "&timestamp={ts_ms}");
        let signature = sign(&self.secret, &params);
        let req = HttpRequest {
            method: Method::Post,
            url: format!(
                "{}{}?{params}&signature={signature}",
                self.base_url, self.order_path
            ),
            api_key: Some(self.api_key.clone()),
        };
        // A 2xx ack is acceptance; any non-2xx or a transport error is a venue
        // rejection. (Live fills then arrive on the user-data WS stream.)
        match self.transport.send(&req) {
            Ok(resp) if resp.is_success() => {
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
        // Live fills arrive on the user-data WebSocket stream (akadro-live), not by
        // REST polling — so there is nothing to do per bar here.
    }
}

impl<T> core::fmt::Debug for BinanceSpotExec<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BinanceSpotExec")
            .field("base_url", &self.base_url)
            .field("instruments", &self.meta.len())
            .finish_non_exhaustive()
    }
}

// --- live network transport (the `net` feature) ------------------------------

// The blocking `reqwest` transport (with rate-limit retry) is live IO, exercised
// only by the `#[ignore]`d live tests, so it lives in its own module to keep it
// out of the coverage gate.
#[cfg(feature = "net")]
mod net;
#[cfg(feature = "net")]
pub use net::{ReqwestTransport, unix_millis};

#[cfg(test)]
mod tests {
    use super::*;

    const EXCHANGE_INFO: &str = r#"{"symbols":[
        {"symbol":"BTCUSDT","baseAsset":"BTC","quoteAsset":"USDT","status":"TRADING","filters":[
            {"filterType":"PRICE_FILTER","tickSize":"0.01000000"},
            {"filterType":"LOT_SIZE","stepSize":"0.00001000"},
            {"filterType":"NOTIONAL","minNotional":"5.00000000"}]},
        {"symbol":"DEADUSDT","baseAsset":"DEAD","quoteAsset":"USDT","status":"BREAK","filters":[]}
    ]}"#;

    #[test]
    fn sign_matches_rfc4231_vector() {
        // RFC 4231 §4.3 HMAC-SHA256 test vector.
        assert_eq!(
            sign(b"Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn decimal_helpers_round_trip() {
        assert_eq!(scale_of("0.01000000"), 2);
        assert_eq!(scale_of("0.00001000"), 5);
        assert_eq!(scale_of("1.00000000"), 0);
        assert_eq!(scale_of("100"), 0);
        assert_eq!(decimal_to_raw("0.01", 2).unwrap(), 1);
        assert_eq!(decimal_to_raw("123.456", 2).unwrap(), 12345); // truncates
        assert_eq!(decimal_to_raw("-0.5", 2).unwrap(), -50);
        assert_eq!(decimal_to_raw("5", 8).unwrap(), 500_000_000);
        assert!(decimal_to_raw("x.y", 2).is_err());
        assert_eq!(raw_to_decimal(12345, 2), "123.45");
        assert_eq!(raw_to_decimal(100, 2), "1"); // trailing zeros trimmed
        assert_eq!(raw_to_decimal(50, 0), "50");
        assert_eq!(raw_to_decimal(-50, 2), "-0.5");
    }

    #[test]
    fn catalog_parses_trading_symbols_only() {
        let cat = BinanceCatalog::from_exchange_info(EXCHANGE_INFO).unwrap();
        assert_eq!(cat.specs().len(), 1); // BREAK symbol skipped
        let id = cat.id_of("BTCUSDT").unwrap();
        assert_eq!(cat.scales(id), Some((2, 5)));
        let spec = cat.spec(id).unwrap();
        assert_eq!(spec.tick_size, Price::from_raw(1)); // 0.01 @ scale 2
        assert_eq!(spec.lot_size, Qty::from_raw(1)); // 0.00001 @ scale 5
        // minNotional 5 @ scale (2+5)=7 → 5 * 1e7.
        assert_eq!(spec.min_notional, Money::from_raw(50_000_000));
        assert!(cat.id_of("NOPE").is_none());
    }

    #[test]
    fn catalog_fetch_via_transport() {
        let mut t = MockTransport::new(vec![HttpResponse {
            status: 200,
            body: EXCHANGE_INFO.into(),
        }]);
        let cat = BinanceCatalog::fetch(&mut t, "http://x").unwrap();
        assert!(cat.id_of("BTCUSDT").is_some());
        assert!(t.sent[0].url.ends_with("/api/v3/exchangeInfo"));
        assert!(t.sent[0].api_key.is_none()); // public — unsigned
        let mut bad = MockTransport::new(vec![HttpResponse {
            status: 503,
            body: String::new(),
        }]);
        assert!(BinanceCatalog::fetch(&mut bad, "http://x").is_err());
    }

    #[test]
    fn kline_feed_retries_on_429() {
        let mut feed = BinanceKlineFeed::new(
            MockTransport::new(vec![
                HttpResponse {
                    status: 429,
                    body: String::new(),
                },
                HttpResponse {
                    status: 200,
                    body: r#"[[1700000000000,"100","100","100","100","1",1700000059999,"x",1,"y","z","0"]]"#.into(),
                },
            ]),
            "http://x",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            2,
            0,
        )
        .with_page_delay(std::time::Duration::ZERO);
        assert!(feed.next_event().is_some()); // survived the 429
    }

    #[test]
    fn klines_parse_to_bars() {
        let json = r#"[[1700000000000,"100.00","102.00","99.50","101.25","12.5",1700000059999,"x",1,"y","z","0"]]"#;
        let bars = parse_klines(json, InstrumentId::new(0), 2, 1).unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].ts.as_nanos(), 1_700_000_059_999_000_000); // closeTime → ns
        assert_eq!(bars[0].open.raw(), 10000);
        assert_eq!(bars[0].close.raw(), 10125);
        assert_eq!(bars[0].volume.raw(), 125); // 12.5 @ scale 1
        assert!(parse_klines("[[1,2]]", InstrumentId::new(0), 2, 1).is_err()); // short row
    }

    #[test]
    fn kline_feed_streams_then_exhausts() {
        let resp = HttpResponse {
            status: 200,
            body:
                r#"[[1700000000000,"100","100","100","100","1",1700000059999,"x",1,"y","z","0"]]"#
                    .to_string(),
        };
        let mut feed = BinanceKlineFeed::new(
            MockTransport::new(vec![resp]),
            "http://x",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            2,
            0,
        );
        assert!(feed.next_event().is_some());
        assert!(feed.next_event().is_none());
    }

    #[test]
    fn kline_feed_range_no_forward_progress_terminates() {
        // A page whose last close is at/before the cursor must not loop forever
        // (the `last <= cursor` guard). Here closeTime == start.
        let body =
            r#"[[1700000000000,"100","100","100","100","1",1700000000000,"x",1,"y","z","0"]]"#;
        let mut feed = BinanceKlineFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 200,
                body: body.into(),
            }]),
            "http://x",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            2,
            0,
        )
        .with_range(1_700_000_000_000, 1_700_001_000_000)
        .with_page_delay(std::time::Duration::ZERO);
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 1); // single bar; no-forward-progress guard ended the loop
    }

    #[test]
    fn kline_feed_429_exhausted_yields_none() {
        let mut feed = BinanceKlineFeed::new(
            MockTransport::new(
                (0..10)
                    .map(|_| HttpResponse {
                        status: 429,
                        body: String::new(),
                    })
                    .collect(),
            ),
            "http://x",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            2,
            0,
        )
        .with_page_delay(std::time::Duration::ZERO);
        assert!(feed.next_event().is_none());
    }

    struct SinkVec(Vec<AccountEvent>);
    impl EventSink for SinkVec {
        fn emit(&mut self, e: AccountEvent) {
            self.0.push(e);
        }
    }

    fn exec_with(responses: Vec<HttpResponse>) -> BinanceSpotExec<MockTransport> {
        BinanceSpotExec::new(
            MockTransport::new(responses),
            "http://x",
            "KEY",
            b"secret".to_vec(),
            vec![SymbolMeta {
                symbol: "BTCUSDT".into(),
                price_scale: 2,
                qty_scale: 5,
            }],
        )
    }

    #[test]
    fn submit_signs_and_accepts_on_ack() {
        let mut x = exec_with(vec![HttpResponse {
            status: 200,
            body: r#"{"orderId":42,"status":"NEW"}"#.into(),
        }]);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(InstrumentId::new(0), Side::Buy, Qty::from_raw(100)),
            Timestamp::from_nanos(1_700_000_000_000_000_000),
            &mut s,
        );
        assert!(matches!(s.0[0], AccountEvent::OrderAccepted { .. }));
        // The request was signed (a `signature=` query param) and carried the key.
        let sent = &x.transport.sent[0];
        assert!(sent.url.contains("signature="));
        assert!(sent.url.contains("type=MARKET"));
        assert!(sent.url.contains("quantity=0.001")); // 100 @ qty_scale 5
        assert_eq!(sent.api_key.as_deref(), Some("KEY"));
    }

    #[test]
    fn limit_order_honours_tif_and_post_only() {
        // M1: an IOC limit must send timeInForce=IOC, not a hard-coded GTC.
        let mut x = exec_with(vec![HttpResponse {
            status: 200,
            body: r#"{"orderId":1,"status":"NEW"}"#.into(),
        }]);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(
                InstrumentId::new(0),
                Side::Buy,
                Qty::from_raw(100),
                Price::from_raw(10_000),
            )
            .with_tif(TimeInForce::Ioc),
            Timestamp::from_nanos(1_700_000_000_000_000_000),
            &mut s,
        );
        assert!(x.transport.sent[0].url.contains("timeInForce=IOC"));
        assert!(!x.transport.sent[0].url.contains("GTC"));

        // A post-only limit must be sent as LIMIT_MAKER (no timeInForce).
        let mut x2 = exec_with(vec![HttpResponse {
            status: 200,
            body: r#"{"orderId":2,"status":"NEW"}"#.into(),
        }]);
        let mut s2 = SinkVec(Vec::new());
        x2.submit(
            ClientOrderId::new(1),
            OrderRequest::limit(
                InstrumentId::new(0),
                Side::Sell,
                Qty::from_raw(100),
                Price::from_raw(10_000),
            )
            .post_only(),
            Timestamp::from_nanos(1_700_000_000_000_000_000),
            &mut s2,
        );
        assert!(x2.transport.sent[0].url.contains("type=LIMIT_MAKER"));
        assert!(!x2.transport.sent[0].url.contains("timeInForce"));

        // post-only on a market order is a contradiction -> rejected, no request.
        let mut x3 = exec_with(vec![]);
        let mut s3 = SinkVec(Vec::new());
        x3.submit(
            ClientOrderId::new(2),
            OrderRequest::market(InstrumentId::new(0), Side::Buy, Qty::from_raw(100)).post_only(),
            Timestamp::from_nanos(1),
            &mut s3,
        );
        assert!(matches!(s3.0[0], AccountEvent::OrderRejected { .. }));
        assert!(x3.transport.sent.is_empty());
    }

    #[test]
    fn submit_rejects_on_venue_error_unknown_instrument_and_stop() {
        // Venue HTTP error → VenueRejected.
        let mut x = exec_with(vec![HttpResponse {
            status: 400,
            body: r#"{"code":-2010,"msg":"insufficient balance"}"#.into(),
        }]);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(InstrumentId::new(0), Side::Buy, Qty::from_raw(100)),
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
        // Unknown instrument → InvalidOrder, no request sent.
        let mut x2 = exec_with(vec![]);
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
    }

    #[test]
    fn observe_is_a_noop() {
        let mut x = exec_with(vec![]);
        let mut s = SinkVec(Vec::new());
        let bar = Event::Bar(Bar::new(
            InstrumentId::new(0),
            Timestamp::from_nanos(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Qty::from_raw(1),
        ));
        x.observe(&bar, Timestamp::from_nanos(1), &mut s);
        assert!(s.0.is_empty());
    }

    #[test]
    fn for_futures_uses_fapi_order_path_and_forwards_reduce_only() {
        let mut x = exec_with(vec![HttpResponse {
            status: 200,
            body: r#"{"orderId":1}"#.into(),
        }])
        .for_futures();
        let mut s = SinkVec(Vec::new());
        let order = OrderRequest::market(InstrumentId::new(0), Side::Sell, Qty::from_raw(100))
            .reduce_only();
        x.submit(
            ClientOrderId::new(0),
            order,
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(s.0[0], AccountEvent::OrderAccepted { .. }));
        let sent = &x.transport.sent[0];
        assert!(
            sent.url.contains("/fapi/v1/order?"),
            "futures path: {}",
            sent.url
        );
        assert!(sent.url.contains("reduceOnly=true"));
        assert!(sent.url.contains("side=SELL"));
        // reduceOnly precedes the signed timestamp (so it is part of the signature).
        let (ro, ts) = (
            sent.url.find("reduceOnly").unwrap(),
            sent.url.find("timestamp").unwrap(),
        );
        assert!(ro < ts);
    }

    #[test]
    fn spot_exec_does_not_forward_reduce_only() {
        // The same reduce_only request on the spot path omits reduceOnly entirely.
        let mut x = exec_with(vec![HttpResponse {
            status: 200,
            body: "{}".into(),
        }]);
        let mut s = SinkVec(Vec::new());
        let order = OrderRequest::market(InstrumentId::new(0), Side::Sell, Qty::from_raw(100))
            .reduce_only();
        x.submit(
            ClientOrderId::new(0),
            order,
            Timestamp::from_nanos(1),
            &mut s,
        );
        let sent = &x.transport.sent[0];
        assert!(sent.url.contains("/api/v3/order?"));
        assert!(!sent.url.contains("reduceOnly"));
    }

    #[test]
    fn kline_feed_for_futures_hits_fapi_klines_path() {
        let mut feed = BinanceKlineFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 200,
                body: "[]".into(),
            }]),
            FUTURES_BASE_URL,
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            1,
            3,
        )
        .for_futures();
        assert!(feed.next_event().is_none()); // empty page → exhausted
        assert!(feed.transport.sent[0].url.contains("/fapi/v1/klines?"));
    }

    #[test]
    fn catalog_error_and_edge_paths() {
        assert!(BinanceCatalog::from_exchange_info("not json").is_err());
        // A TRADING symbol missing its price/lot filters is skipped.
        let only_notional = r#"{"symbols":[{"symbol":"XUSDT","baseAsset":"X","quoteAsset":"USDT",
            "status":"TRADING","filters":[{"filterType":"NOTIONAL","minNotional":"1"}]}]}"#;
        let cat = BinanceCatalog::from_exchange_info(only_notional).unwrap();
        assert_eq!(cat.specs().len(), 0);
        // Out-of-range lookups.
        assert!(cat.spec(InstrumentId::new(9)).is_none());
        assert!(cat.scales(InstrumentId::new(9)).is_none());
        // A symbol with no NOTIONAL filter gets min_notional 0.
        let no_notional = r#"{"symbols":[{"symbol":"YUSDT","baseAsset":"Y","quoteAsset":"USDT",
            "status":"TRADING","filters":[
              {"filterType":"PRICE_FILTER","tickSize":"0.1"},
              {"filterType":"LOT_SIZE","stepSize":"1"}]}]}"#;
        let cat = BinanceCatalog::from_exchange_info(no_notional).unwrap();
        assert_eq!(
            cat.spec(cat.id_of("YUSDT").unwrap()).unwrap().min_notional,
            Money::from_raw(0)
        );
    }

    #[test]
    fn decimal_edge_cases() {
        assert_eq!(decimal_to_raw(".5", 2).unwrap(), 50); // empty integer part
        assert_eq!(raw_to_decimal(-5, 0), "-5"); // scale 0 negative
        assert_eq!(raw_to_decimal(0, 3), "0");
    }

    #[test]
    fn parse_klines_error_paths() {
        assert!(parse_klines("not json", InstrumentId::new(0), 2, 1).is_err());
        // closeTime not an integer.
        let bad_ts = r#"[["x","1","1","1","1","1","y","z",1,"a","b","0"]]"#;
        assert!(parse_klines(bad_ts, InstrumentId::new(0), 2, 1).is_err());
        // a non-string OHLC field.
        let bad_field = r#"[[1,1,"1","1","1","1",1700000059999,"z",1,"a","b","0"]]"#;
        assert!(parse_klines(bad_field, InstrumentId::new(0), 2, 1).is_err());
    }

    #[test]
    fn kline_feed_paginates_a_range_and_reports_http_errors() {
        // Two pages: the first ends before `end`, the second returns one more bar.
        let page1 =
            r#"[[1700000000000,"1","1","1","1","1",1700000059999,"x",1,"y","z","0"]]"#.to_string();
        let page2 =
            r#"[[1700000060000,"1","1","1","1","1",1700000119999,"x",1,"y","z","0"]]"#.to_string();
        let mut feed = BinanceKlineFeed::new(
            MockTransport::new(vec![
                HttpResponse {
                    status: 200,
                    body: page1,
                },
                HttpResponse {
                    status: 200,
                    body: page2,
                },
                HttpResponse {
                    status: 200,
                    body: "[]".into(),
                },
            ]),
            "http://x",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            0,
            0,
        )
        .with_range(1_700_000_000_000, 1_700_000_180_000)
        .with_limit(5000); // clamps to MAX_KLINES_LIMIT
        let mut count = 0;
        while feed.next_event().is_some() {
            count += 1;
        }
        assert_eq!(count, 2);
        assert!(
            feed.transport.sent[0]
                .url
                .contains(&format!("limit={MAX_KLINES_LIMIT}"))
        );
        assert!(format!("{feed:?}").contains("BinanceKlineFeed"));

        // An HTTP error during fetch yields no events.
        let mut err_feed = BinanceKlineFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 500,
                body: "oops".into(),
            }]),
            "http://x",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            0,
            0,
        );
        assert!(err_feed.next_event().is_none());
    }

    #[test]
    fn submit_encodes_limit_orders_and_exposes_debug() {
        let mut x = exec_with(vec![HttpResponse {
            status: 200,
            body: "{}".into(),
        }]);
        let mut s = SinkVec(Vec::new());
        let order = OrderRequest::limit(
            InstrumentId::new(0),
            Side::Buy,
            Qty::from_raw(100),
            Price::from_raw(10_000),
        );
        x.submit(
            ClientOrderId::new(3),
            order,
            Timestamp::from_nanos(1),
            &mut s,
        );
        let url = &x.transport.sent[0].url;
        assert!(url.contains("type=LIMIT"));
        assert!(url.contains("price=100")); // 10_000 @ price_scale 2
        assert!(url.contains("timeInForce=GTC"));
        assert!(url.contains("newClientOrderId=ak3"));
        assert!(format!("{x:?}").contains("BinanceSpotExec"));
        assert!(
            !HttpResponse {
                status: 404,
                body: String::new()
            }
            .is_success()
        );
    }

    #[test]
    fn submit_rejects_unsupported_order_kinds() {
        use akadro_core::TriggerBy;
        let mut x = exec_with(vec![]);
        let mut s = SinkVec(Vec::new());
        let stop = OrderRequest::stop(
            InstrumentId::new(0),
            Side::Buy,
            Qty::from_raw(1),
            Price::from_raw(1),
            TriggerBy::Last,
        );
        x.submit(
            ClientOrderId::new(0),
            stop,
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
        assert!(x.transport.sent.is_empty()); // rejected locally, no venue call
    }
}
