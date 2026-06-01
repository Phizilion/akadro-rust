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
    OrderKind, OrderRequest, Price, Qty, RejectReason, Side, TimeInForce, Timestamp,
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

/// Number of significant fractional digits in a decimal string (trailing zeros
/// stripped): `"0.01"` → 2, `"1"` → 0.
#[must_use]
pub fn scale_of(decimal: &str) -> u32 {
    match decimal.split_once('.') {
        Some((_, frac)) => frac.trim_end_matches('0').len() as u32,
        None => 0,
    }
}

/// Parse a decimal string to a raw fixed-point `i64` at `scale` (truncating excess
/// precision).
///
/// # Errors
/// [`BybitError::Parse`] if the digits are invalid.
pub fn decimal_to_raw(s: &str, scale: u32) -> Result<i64, BybitError> {
    let neg = s.starts_with('-');
    let s = s.trim_start_matches('-');
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    let int_part = if int_part.is_empty() { "0" } else { int_part };
    let mut frac = frac_part.to_string();
    let scale_us = scale as usize;
    frac.truncate(scale_us);
    while frac.len() < scale_us {
        frac.push('0');
    }
    let combined = format!("{int_part}{frac}");
    let val: i64 = combined
        .parse()
        .map_err(|_| BybitError::Parse(format!("bad decimal {s:?}")))?;
    Ok(if neg { -val } else { val })
}

/// Format a raw fixed-point `i64` at `scale` back to a decimal string.
#[must_use]
pub fn raw_to_decimal(raw: i64, scale: u32) -> String {
    // Clamp to the largest base-10 exponent a u128 holds (bug class 3).
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

/// A [`DataSource`] streaming Bybit klines for one instrument (one fetch up to
/// `limit`, Bybit's `/v5/market/kline` cap is 1000).
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
            buffer: std::collections::VecDeque::new(),
            fetched: false,
        }
    }

    /// Set the page size (Bybit caps `/v5/market/kline` at 1000).
    #[must_use]
    pub fn with_limit(mut self, limit: u32) -> Self {
        // Clamp to [1, 1000]: a 0 limit sends `limit=0` and silently empties the
        // feed (bug class 8).
        self.limit = limit.clamp(1, 1000);
        self
    }

    fn fetch(&mut self) -> Result<(), BybitError> {
        let url = format!(
            "{}/v5/market/kline?category={}&symbol={}&interval={}&limit={}",
            self.base_url,
            self.category.as_str(),
            self.symbol,
            self.interval,
            self.limit
        );
        let resp = self.transport.send(&HttpRequest {
            method: Method::Get,
            url,
            body: None,
            headers: Vec::new(),
        })?;
        if !resp.is_success() {
            return Err(BybitError::Transport(format!("kline HTTP {}", resp.status)));
        }
        let bars = parse_klines(
            &resp.body,
            self.instrument,
            self.price_scale,
            self.qty_scale,
            &self.interval,
        )?;
        self.buffer.extend(bars);
        Ok(())
    }
}

impl<T: Transport> DataSource for BybitKlineFeed<T> {
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

    /// Set the millisecond timestamp used for signing (the live transport derives
    /// it from the system clock; tests set it for determinism).
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
        if order.reduce_only && m.category == Category::Linear {
            body.insert_str(body.len() - 1, ",\"reduceOnly\":true");
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
