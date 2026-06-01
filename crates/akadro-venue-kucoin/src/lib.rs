// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! KuCoin venue connector (spot, REST) for akadro — depends only on `akadro-core`,
//! so it adds a venue with **zero** changes to the engine or strategies (the
//! open/closed goal).
//!
//! It provides a [`KucoinCatalog`] (`/api/v1/symbols` → [`InstrumentSpec`]), a
//! [`KucoinCandleFeed`] ([`DataSource`] over `/api/v1/market/candles`), and a
//! [`KucoinExec`] ([`ExecutionClient`] that signs and submits `/api/v1/orders`).
//! Logic talks to KuCoin through the mockable [`Transport`] seam, so signing,
//! parsing and normalization are fixture-tested with no network; the live
//! `reqwest` transport is behind the `net` feature. Live **fills** arrive on the
//! `/spotMarket/tradeOrders` WebSocket topic (the `akadro-live` shell), so
//! [`KucoinExec::observe`] is a no-op on bars.
//!
//! KuCoin signs `Base64(HMAC-SHA256(secret, timestamp + METHOD + endpoint + body))`
//! and authenticates with the `KC-API-KEY/SIGN/TIMESTAMP/PASSPHRASE` headers plus
//! `KC-API-KEY-VERSION: 2`, where the passphrase header is itself
//! `Base64(HMAC-SHA256(secret, plaintext_passphrase))` (the v2 scheme — see
//! [`encrypt_passphrase`]).
//!
//! Candle rows are `[time, open, close, high, low, volume, turnover]` — note
//! `close` precedes `high`/`low` — with `time` in **seconds** (window open).

use akadro_core::{
    AccountEvent, AssetId, Bar, CapSet, Capability, ClientOrderId, DataSource, Event, EventSink,
    ExecutionClient, InstrumentCatalog, InstrumentId, InstrumentKind, InstrumentSpec, Money,
    OrderKind, OrderRequest, Price, Qty, RejectReason, Side, TimeInForce, Timestamp,
};
use serde::Deserialize;

mod sign {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;

    /// `Base64(HMAC-SHA256(secret, prehash))` where `prehash = timestamp + METHOD +
    /// endpoint + body` — the KuCoin `KC-API-SIGN`.
    ///
    /// # Panics
    /// Never in practice — HMAC-SHA256 accepts a key of any length.
    #[must_use]
    pub fn sign(secret: &[u8], prehash: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC key any length");
        mac.update(prehash.as_bytes());
        STANDARD.encode(mac.finalize().into_bytes())
    }
}
pub use sign::sign;

/// The KuCoin v2 `KC-API-PASSPHRASE` header: `Base64(HMAC-SHA256(secret,
/// plaintext_passphrase))`. The plaintext passphrase is what you set when creating
/// the API key; KuCoin requires it encrypted under the API secret for key v2.
#[must_use]
pub fn encrypt_passphrase(secret: &[u8], passphrase: &str) -> String {
    sign(secret, passphrase)
}

/// Connector errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KucoinError {
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

impl Method {
    /// The uppercase token used in the signing prehash.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
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
    /// [`KucoinError::Transport`] on a network/transport failure.
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, KucoinError>;
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
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, KucoinError> {
        self.sent.push(request.clone());
        self.responses
            .pop_front()
            .ok_or_else(|| KucoinError::Transport("no canned response".into()))
    }
}

// --- fixed-point decimal helpers ---------------------------------------------

/// Number of significant fractional digits in a decimal string (trailing zeros
/// stripped): `"0.1"` → 1, `"1"` → 0.
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
/// [`KucoinError::Parse`] if the digits are invalid.
pub fn decimal_to_raw(s: &str, scale: u32) -> Result<i64, KucoinError> {
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
        .map_err(|_| KucoinError::Parse(format!("bad decimal {s:?}")))?;
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

/// Seconds in a KuCoin candle `type` token (`"1min"`, `"15min"`, `"1hour"`,
/// `"4hour"`, `"1day"`, `"1week"`); `0` if unrecognized.
#[must_use]
pub fn candle_seconds(candle_type: &str) -> i64 {
    let parse = |suffix: &str, unit: i64| -> Option<i64> {
        candle_type
            .strip_suffix(suffix)
            .and_then(|n| n.parse::<i64>().ok())
            .map(|n| n * unit)
    };
    parse("min", 60)
        .or_else(|| parse("hour", 3600))
        .or_else(|| parse("day", 86_400))
        .or_else(|| parse("week", 604_800))
        .unwrap_or(0)
}

// --- catalogue ---------------------------------------------------------------

#[derive(Deserialize)]
struct SymbolsResp {
    #[serde(default)]
    data: Vec<SymbolRow>,
}

#[derive(Deserialize)]
struct SymbolRow {
    symbol: String,
    #[serde(rename = "baseCurrency")]
    base_currency: String,
    #[serde(rename = "quoteCurrency")]
    quote_currency: String,
    #[serde(rename = "baseIncrement")]
    base_increment: String,
    #[serde(rename = "priceIncrement")]
    price_increment: String,
    #[serde(rename = "quoteMinSize", default)]
    quote_min_size: String,
    #[serde(rename = "enableTrading", default = "default_true")]
    enable_trading: bool,
}

fn default_true() -> bool {
    true
}

/// Maps KuCoin symbols to dense [`InstrumentSpec`]s and asset ids.
#[derive(Debug, Clone)]
pub struct KucoinCatalog {
    specs: Vec<InstrumentSpec>,
    symbols: Vec<String>,
    scales: Vec<(u32, u32)>,
}

impl KucoinCatalog {
    /// Parse `/api/v1/symbols` into a catalogue. Only `enableTrading` symbols are
    /// kept; the tick is `priceIncrement`, the lot is `baseIncrement`, and the
    /// minimum notional is `quoteMinSize`. Each gets a dense [`InstrumentId`].
    ///
    /// # Errors
    /// [`KucoinError::Parse`] on unparseable JSON or a bad filter value.
    pub fn from_symbols(json: &str) -> Result<Self, KucoinError> {
        let resp: SymbolsResp =
            serde_json::from_str(json).map_err(|e| KucoinError::Parse(e.to_string()))?;
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
        for row in resp.data {
            if !row.enable_trading {
                continue;
            }
            let price_scale = scale_of(&row.price_increment);
            let qty_scale = scale_of(&row.base_increment);
            let min_notional_raw = if row.quote_min_size.is_empty() {
                0
            } else {
                decimal_to_raw(&row.quote_min_size, price_scale + qty_scale)?
            };
            let id = InstrumentId::new(specs.len() as u32);
            let base = asset_id(&row.base_currency, &mut assets);
            let quote = asset_id(&row.quote_currency, &mut assets);
            specs.push(InstrumentSpec::new(
                id,
                base,
                quote,
                InstrumentKind::Spot,
                Price::from_raw(decimal_to_raw(&row.price_increment, price_scale)?),
                Qty::from_raw(decimal_to_raw(&row.base_increment, qty_scale)?),
                Money::from_raw(i128::from(min_notional_raw)),
                CapSet::empty().with(Capability::LimitOrders),
            ));
            symbols.push(row.symbol);
            scales.push((price_scale, qty_scale));
        }
        Ok(KucoinCatalog {
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

    /// The [`InstrumentId`] for `symbol` (e.g. `"BTC-USDT"`), if present.
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

impl InstrumentCatalog for KucoinCatalog {
    fn spec(&self, id: InstrumentId) -> Option<&InstrumentSpec> {
        self.specs.get(id.index() as usize)
    }
}

// --- candles (DataSource) ----------------------------------------------------

#[derive(Deserialize)]
struct CandlesResp {
    #[serde(default)]
    data: Vec<Vec<String>>,
}

/// Parse a KuCoin `/api/v1/market/candles` body into [`Bar`]s. Rows are
/// `[time, open, close, high, low, volume, turnover]` (`close` before `high`/`low`)
/// with `time` the window **open** time in **seconds**; KuCoin returns them
/// newest-first, so they are sorted ascending and stamped at close time
/// (`time + candle_seconds`).
///
/// # Errors
/// [`KucoinError::Parse`] on bad JSON, a short row, or a bad number.
pub fn parse_candles(
    json: &str,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
    candle_type: &str,
) -> Result<Vec<Bar>, KucoinError> {
    let resp: CandlesResp =
        serde_json::from_str(json).map_err(|e| KucoinError::Parse(e.to_string()))?;
    let interval_s = candle_seconds(candle_type);
    let mut bars = Vec::with_capacity(resp.data.len());
    for row in resp.data {
        if row.len() < 6 {
            return Err(KucoinError::Parse(format!(
                "candle row has {} fields",
                row.len()
            )));
        }
        let open_s: i64 = row[0]
            .parse()
            .map_err(|_| KucoinError::Parse(format!("bad ts {:?}", row[0])))?;
        let close_ns = open_s
            .checked_add(interval_s)
            .and_then(|s| s.checked_mul(1_000_000_000))
            .ok_or_else(|| KucoinError::Parse("ts overflow".into()))?;
        // KuCoin order: [time, open, close, high, low, volume, turnover].
        bars.push(Bar::new(
            instrument,
            Timestamp::from_nanos(close_ns),
            Price::from_raw(decimal_to_raw(&row[1], price_scale)?), // open
            Price::from_raw(decimal_to_raw(&row[3], price_scale)?), // high
            Price::from_raw(decimal_to_raw(&row[4], price_scale)?), // low
            Price::from_raw(decimal_to_raw(&row[2], price_scale)?), // close
            Qty::from_raw(decimal_to_raw(&row[5], qty_scale)?),     // volume
        ));
    }
    bars.sort_by_key(|b| b.ts.as_nanos()); // KuCoin returns newest-first
    Ok(bars)
}

/// A [`DataSource`] streaming KuCoin candles for one instrument (one fetch; KuCoin
/// returns up to 1500 candles per call).
pub struct KucoinCandleFeed<T> {
    transport: T,
    base_url: String,
    symbol: String,
    instrument: InstrumentId,
    candle_type: String,
    price_scale: u32,
    qty_scale: u32,
    buffer: std::collections::VecDeque<Bar>,
    fetched: bool,
}

impl<T: Transport> KucoinCandleFeed<T> {
    /// Create a feed for `symbol` (mapped to `instrument`) at `candle_type`
    /// (e.g. `"1min"`).
    #[must_use]
    pub fn new(
        transport: T,
        base_url: impl Into<String>,
        symbol: impl Into<String>,
        instrument: InstrumentId,
        candle_type: impl Into<String>,
        price_scale: u32,
        qty_scale: u32,
    ) -> Self {
        KucoinCandleFeed {
            transport,
            base_url: base_url.into(),
            symbol: symbol.into(),
            instrument,
            candle_type: candle_type.into(),
            price_scale,
            qty_scale,
            buffer: std::collections::VecDeque::new(),
            fetched: false,
        }
    }

    fn fetch(&mut self) -> Result<(), KucoinError> {
        let url = format!(
            "{}/api/v1/market/candles?type={}&symbol={}",
            self.base_url, self.candle_type, self.symbol
        );
        let resp = self.transport.send(&HttpRequest {
            method: Method::Get,
            url,
            body: None,
            headers: Vec::new(),
        })?;
        if !resp.is_success() {
            return Err(KucoinError::Transport(format!(
                "candles HTTP {}",
                resp.status
            )));
        }
        let bars = parse_candles(
            &resp.body,
            self.instrument,
            self.price_scale,
            self.qty_scale,
            &self.candle_type,
        )?;
        self.buffer.extend(bars);
        Ok(())
    }
}

impl<T: Transport> DataSource for KucoinCandleFeed<T> {
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

impl<T> core::fmt::Debug for KucoinCandleFeed<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KucoinCandleFeed")
            .field("symbol", &self.symbol)
            .field("type", &self.candle_type)
            .field("buffered", &self.buffer.len())
            .finish_non_exhaustive()
    }
}

// --- execution client --------------------------------------------------------

/// One instrument's symbol + scales, for [`KucoinExec`] order encoding.
#[derive(Debug, Clone)]
pub struct InstrumentMeta {
    /// KuCoin symbol (e.g. `"BTC-USDT"`).
    pub symbol: String,
    /// Price fixed-point scale.
    pub price_scale: u32,
    /// Quantity fixed-point scale.
    pub qty_scale: u32,
}

/// The akadro `client_order_id` tag set as KuCoin's `clientOid` for live fill
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

/// An [`ExecutionClient`] that signs and submits KuCoin orders over REST. `submit`
/// posts a signed `/api/v1/orders` and emits [`AccountEvent::OrderAccepted`] when
/// the response `code` is `"200000"`, else [`AccountEvent::OrderRejected`].
/// `observe` is a no-op: live fills arrive on the `/spotMarket/tradeOrders`
/// WebSocket topic.
pub struct KucoinExec<T> {
    transport: T,
    base_url: String,
    api_key: String,
    secret: Vec<u8>,
    passphrase: String,
    meta: Vec<InstrumentMeta>,
    timestamp_ms: String,
}

impl<T: Transport> KucoinExec<T> {
    /// Create with credentials (key, secret, **plaintext** passphrase — it is
    /// v2-encrypted internally) and per-instrument [`InstrumentMeta`] (dense, in id
    /// order — typically from a [`KucoinCatalog`]).
    #[must_use]
    pub fn new(
        transport: T,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        secret: impl Into<Vec<u8>>,
        passphrase: impl Into<String>,
        meta: Vec<InstrumentMeta>,
    ) -> Self {
        KucoinExec {
            transport,
            base_url: base_url.into(),
            api_key: api_key.into(),
            secret: secret.into(),
            passphrase: passphrase.into(),
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

impl<T: Transport> ExecutionClient for KucoinExec<T> {
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
        // post-only is maker-only (limit only); reduce-only does not exist on KuCoin
        // SPOT — both are contradictions here and are rejected locally rather than
        // silently sent as a plain order (bug classes 1, 2).
        if order.post_only && !matches!(order.kind, OrderKind::Limit { .. }) {
            sink.emit(AccountEvent::OrderRejected {
                id,
                reason: RejectReason::InvalidOrder,
                ts: now,
            });
            return;
        }
        if order.reduce_only {
            sink.emit(AccountEvent::OrderRejected {
                id,
                reason: RejectReason::InvalidOrder,
                ts: now,
            });
            return;
        }
        let side = match order.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        };
        let order_size = raw_to_decimal(order.qty.raw(), m.qty_scale);
        let oid = client_order_tag(id);
        let body = match order.kind {
            OrderKind::Market => format!(
                "{{\"clientOid\":\"{oid}\",\"side\":\"{side}\",\"symbol\":\"{}\",\"type\":\"market\",\"size\":\"{order_size}\"}}",
                m.symbol
            ),
            OrderKind::Limit { limit } => {
                // Encode tif / post-only (bug class 1): post_only -> `"postOnly":true`
                // (maker-only), else `"timeInForce":"<IOC|FOK|GTC>"` from the order's
                // TIF. KuCoin defaults to GTC when neither is present.
                let policy = if order.post_only {
                    "\"postOnly\":true".to_owned()
                } else {
                    let tif = match order.tif {
                        TimeInForce::Ioc => "IOC",
                        TimeInForce::Fok => "FOK",
                        _ => "GTC",
                    };
                    format!("\"timeInForce\":\"{tif}\"")
                };
                format!(
                    "{{\"clientOid\":\"{oid}\",\"side\":\"{side}\",\"symbol\":\"{}\",\"type\":\"limit\",\"size\":\"{order_size}\",\"price\":\"{}\",{policy}}}",
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
        let endpoint = "/api/v1/orders";
        // Signing timestamp: the test/driver-set value, or the handler's event time
        // when unset — never sign with an empty timestamp (which KuCoin rejects), and
        // the signed bytes match the header (bug class 7).
        let timestamp_ms = if self.timestamp_ms.is_empty() {
            (now.as_nanos() / 1_000_000).to_string()
        } else {
            self.timestamp_ms.clone()
        };
        let prehash = format!("{}{}{endpoint}{body}", timestamp_ms, Method::Post.as_str());
        let signature = sign(&self.secret, &prehash);
        let req = HttpRequest {
            method: Method::Post,
            url: format!("{}{endpoint}", self.base_url),
            body: Some(body),
            headers: vec![
                ("KC-API-KEY".into(), self.api_key.clone()),
                ("KC-API-SIGN".into(), signature),
                ("KC-API-TIMESTAMP".into(), timestamp_ms),
                (
                    "KC-API-PASSPHRASE".into(),
                    encrypt_passphrase(&self.secret, &self.passphrase),
                ),
                ("KC-API-KEY-VERSION".into(), "2".into()),
                ("Content-Type".into(), "application/json".into()),
            ],
        };
        match self.transport.send(&req) {
            Ok(resp) if resp.is_success() && code_ok(&resp.body) => {
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
        // Live fills arrive on the `/spotMarket/tradeOrders` WebSocket topic.
    }
}

/// `true` if a response carries `code":"200000"` (KuCoin success).
fn code_ok(body: &str) -> bool {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        code: String,
    }
    serde_json::from_str::<Resp>(body).is_ok_and(|r| r.code == "200000")
}

impl<T> core::fmt::Debug for KucoinExec<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KucoinExec")
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

    const SYMBOLS: &str = r#"{"code":"200000","data":[
        {"symbol":"BTC-USDT","baseCurrency":"BTC","quoteCurrency":"USDT","baseIncrement":"0.00000001","priceIncrement":"0.1","quoteMinSize":"0.1","enableTrading":true},
        {"symbol":"DEAD-USDT","baseCurrency":"DEAD","quoteCurrency":"USDT","baseIncrement":"0.1","priceIncrement":"0.1","quoteMinSize":"1","enableTrading":false}
    ]}"#;

    #[test]
    fn sign_and_passphrase_are_base64() {
        assert_eq!(
            sign(b"Jefe", "what do ya want for nothing?"),
            "W9zBRr9gdU5qBCQmCJV1x1oAPwidJzmDnexYuWTsOEM="
        );
        // The v2 passphrase is the same Base64 HMAC over the plaintext passphrase.
        assert_eq!(
            encrypt_passphrase(b"Jefe", "what do ya want for nothing?"),
            sign(b"Jefe", "what do ya want for nothing?")
        );
    }

    #[test]
    fn decimal_and_interval_helpers() {
        assert_eq!(scale_of("0.1"), 1);
        assert_eq!(decimal_to_raw("0.1", 1).unwrap(), 1);
        assert_eq!(raw_to_decimal(12345, 2), "123.45");
        assert!(decimal_to_raw("x", 2).is_err());
        assert_eq!(candle_seconds("1min"), 60);
        assert_eq!(candle_seconds("4hour"), 14_400);
        assert_eq!(candle_seconds("1day"), 86_400);
        assert_eq!(candle_seconds("1week"), 604_800);
        assert_eq!(candle_seconds("weird"), 0);
    }

    #[test]
    fn catalog_keeps_trading_symbols() {
        let cat = KucoinCatalog::from_symbols(SYMBOLS).unwrap();
        assert_eq!(cat.specs().len(), 1); // disabled skipped
        let id = cat.id_of("BTC-USDT").unwrap();
        assert_eq!(cat.scales(id), Some((1, 8)));
        let spec = cat.spec(id).unwrap();
        assert_eq!(spec.tick_size, Price::from_raw(1)); // 0.1 @ 1
        // quoteMinSize 0.1 @ (1+8)=9 → 0.1 * 1e9 = 100_000_000.
        assert_eq!(spec.min_notional, Money::from_raw(100_000_000));
        assert!(cat.id_of("NONE").is_none());
    }

    #[test]
    fn candles_parse_with_kucoin_field_order() {
        // [time_s, open, close, high, low, volume, turnover], newest-first.
        let json = r#"{"code":"200000","data":[
            ["1700000060","100.5","106.0","107.0","100.0","8","x"],
            ["1700000000","100.0","100.5","101.0","99.0","10","x"]
        ]}"#;
        let bars = parse_candles(json, InstrumentId::new(0), 1, 0, "1min").unwrap();
        assert_eq!(bars.len(), 2);
        // Sorted ascending; stamped at (open + 60s) * 1e9 ns.
        let b0 = bars[0];
        assert_eq!(b0.ts.as_nanos(), 1_700_000_060_000_000_000); // 1700000000+60
        assert_eq!(b0.open, Price::from_raw(1000)); // 100.0 @ 1
        assert_eq!(b0.high, Price::from_raw(1010)); // row[3] = 101.0
        assert_eq!(b0.low, Price::from_raw(990)); // row[4] = 99.0
        assert_eq!(b0.close, Price::from_raw(1005)); // row[2] = 100.5
        assert!(
            parse_candles(
                r#"{"data":[["1","2"]]}"#,
                InstrumentId::new(0),
                1,
                0,
                "1min"
            )
            .is_err()
        );
    }

    #[test]
    fn candle_feed_streams_and_http_errors() {
        let body = r#"{"code":"200000","data":[["1700000000","100","100","100","100","1","x"]]}"#;
        let mut feed = KucoinCandleFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 200,
                body: body.into(),
            }]),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1min",
            2,
            0,
        );
        assert!(feed.next_event().is_some());
        assert!(feed.next_event().is_none());
        assert!(format!("{feed:?}").contains("KucoinCandleFeed"));

        let mut err = KucoinCandleFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 500,
                body: "x".into(),
            }]),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1min",
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

    fn exec_with(responses: Vec<HttpResponse>) -> KucoinExec<MockTransport> {
        KucoinExec::new(
            MockTransport::new(responses),
            "http://x",
            "KEY",
            b"secret".to_vec(),
            "phrase",
            vec![InstrumentMeta {
                symbol: "BTC-USDT".into(),
                price_scale: 1,
                qty_scale: 4,
            }],
        )
        .with_timestamp_ms(1_700_000_000_000)
    }

    #[test]
    fn submit_signs_and_accepts_on_code_200000() {
        let mut x = exec_with(vec![HttpResponse {
            status: 200,
            body: r#"{"code":"200000","data":{"orderId":"5bd6e9286d99522a52e458de"}}"#.into(),
        }]);
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
        assert!(body.contains("\"type\":\"market\""));
        assert!(body.contains("\"size\":\"0.01\"")); // 100 @ 4
        assert!(body.contains("\"clientOid\":\"ak0\""));
        let names: Vec<&str> = sent.headers.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"KC-API-SIGN"));
        assert!(names.contains(&"KC-API-PASSPHRASE"));
        assert!(names.contains(&"KC-API-KEY-VERSION"));
    }

    #[test]
    fn submit_limit_and_debug() {
        let mut x = exec_with(vec![HttpResponse {
            status: 200,
            body: r#"{"code":"200000"}"#.into(),
        }]);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::limit(
                InstrumentId::new(0),
                Side::Sell,
                Qty::from_raw(100),
                Price::from_raw(500),
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(s.0[0], AccountEvent::OrderAccepted { .. }));
        let body = x.transport.sent[0].body.as_deref().unwrap();
        assert!(body.contains("\"type\":\"limit\""));
        assert!(body.contains("\"price\":\"50\"")); // 500 @ 1
        assert!(format!("{x:?}").contains("KucoinExec"));
    }

    #[test]
    fn submit_encodes_tif_post_only_rejects_reduce_only() {
        let ok = || HttpResponse {
            status: 200,
            body: r#"{"code":"200000","data":{"orderId":"1"}}"#.into(),
        };
        // IOC limit -> timeInForce IOC (KuCoin defaults to GTC when absent).
        let mut x = exec_with(vec![ok()]);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(
                InstrumentId::new(0),
                Side::Buy,
                Qty::from_raw(100),
                Price::from_raw(10),
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

        // post_only limit -> postOnly:true (maker-only, not a taker limit).
        let mut x = exec_with(vec![ok()]);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::limit(
                InstrumentId::new(0),
                Side::Sell,
                Qty::from_raw(100),
                Price::from_raw(10),
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
                .contains("\"postOnly\":true")
        );

        // post_only on a market order -> rejected, no request.
        let mut x = exec_with(vec![]);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(2),
            OrderRequest::market(InstrumentId::new(0), Side::Buy, Qty::from_raw(100)).post_only(),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(s.0[0], AccountEvent::OrderRejected { .. }));
        assert!(x.transport.sent.is_empty());

        // reduce_only -> rejected (KuCoin spot has no reduce-only).
        let mut x = exec_with(vec![]);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(3),
            OrderRequest::market(InstrumentId::new(0), Side::Sell, Qty::from_raw(100))
                .reduce_only(),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(s.0[0], AccountEvent::OrderRejected { .. }));
    }

    #[test]
    fn submit_rejections_and_noop_observe() {
        let mut x = exec_with(vec![HttpResponse {
            status: 200,
            body: r#"{"code":"200004","msg":"Balance insufficient"}"#.into(),
        }]);
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
        assert_eq!(parse_client_order_tag("uuid-x"), None);
    }

    #[test]
    fn edge_paths() {
        use akadro_core::TriggerBy;
        assert_eq!(Method::Get.as_str(), "GET");
        assert_eq!(scale_of("100"), 0);
        assert_eq!(raw_to_decimal(50, 0), "50");
        // Asset reuse (shared USDT quote).
        let json = r#"{"code":"200000","data":[
            {"symbol":"BTC-USDT","baseCurrency":"BTC","quoteCurrency":"USDT","baseIncrement":"0.1","priceIncrement":"0.1","quoteMinSize":"0.1","enableTrading":true},
            {"symbol":"ETH-USDT","baseCurrency":"ETH","quoteCurrency":"USDT","baseIncrement":"0.01","priceIncrement":"0.01","quoteMinSize":"0.1","enableTrading":true}
        ]}"#;
        let cat = KucoinCatalog::from_symbols(json).unwrap();
        assert_eq!(cat.specs().len(), 2);

        let mut feed = KucoinCandleFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 200,
                body: r#"{"data":[["1"]]}"#.into(),
            }]),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1min",
            1,
            0,
        );
        assert!(feed.next_event().is_none()); // parse error → no events

        let mut x = exec_with(vec![]);
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
