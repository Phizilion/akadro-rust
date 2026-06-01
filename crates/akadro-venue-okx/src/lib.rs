// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! OKX venue connector (spot + perpetual **swap**, REST) for akadro — depends only
//! on `akadro-core`, so it adds a venue with **zero** changes to the engine or
//! strategies (the open/closed goal).
//!
//! It provides an [`OkxCatalog`] (`/api/v5/public/instruments` → [`InstrumentSpec`]),
//! an [`OkxCandleFeed`] ([`DataSource`] over `/api/v5/market/candles`), and an
//! [`OkxExec`] ([`ExecutionClient`] that signs and submits `/api/v5/trade/order`).
//! Logic talks to OKX through the mockable [`Transport`] seam, so signing, parsing
//! and normalization are fixture-tested with no network; the live `reqwest`
//! transport is behind the `net` feature. Live **fills** arrive on OKX's
//! `orders` WebSocket channel (the `akadro-live` shell), so [`OkxExec::observe`] is
//! a no-op on bars.
//!
//! OKX signs `Base64(HMAC-SHA256(secret, timestamp + METHOD + requestPath + body))`
//! and authenticates with the `OK-ACCESS-KEY/SIGN/TIMESTAMP/PASSPHRASE` headers
//! (a passphrase is required, unlike Binance).

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
    /// requestPath + body` — the OKX `OK-ACCESS-SIGN`.
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

/// Connector errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OkxError {
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

/// An outbound HTTP request: a method, full URL, optional JSON body, and a set of
/// header `(name, value)` pairs (the signed auth headers, when present).
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
    /// [`OkxError::Transport`] on a network/transport failure.
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, OkxError>;
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
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, OkxError> {
        self.sent.push(request.clone());
        self.responses
            .pop_front()
            .ok_or_else(|| OkxError::Transport("no canned response".into()))
    }
}

// --- fixed-point decimal helpers ---------------------------------------------

/// Number of significant fractional digits in a decimal string (trailing zeros
/// stripped): `"0.10"` → 1, `"1"` → 0.
#[must_use]
pub fn scale_of(decimal: &str) -> u32 {
    match decimal.split_once('.') {
        Some((_, frac)) => frac.trim_end_matches('0').len() as u32,
        None => 0,
    }
}

/// Parse a decimal string to a raw fixed-point `i64` at `scale` (truncating excess
/// precision). `"0.1"` @ 1 → `1`; `"123.456"` @ 2 → `12345`.
///
/// # Errors
/// [`OkxError::Parse`] if the digits are invalid.
pub fn decimal_to_raw(s: &str, scale: u32) -> Result<i64, OkxError> {
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
        .map_err(|_| OkxError::Parse(format!("bad decimal {s:?}")))?;
    Ok(if neg { -val } else { val })
}

/// Format a raw fixed-point `i64` at `scale` back to a decimal string for a body
/// field: `(12345, 2)` → `"123.45"`, `(50, 0)` → `"50"`.
#[must_use]
pub fn raw_to_decimal(raw: i64, scale: u32) -> String {
    // Clamp to the largest base-10 exponent a u128 holds (`10u128.pow(39)`
    // overflows); guards a malformed precision row (bug class 3).
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

/// Milliseconds in an OKX bar token (`"1m"`, `"15m"`, `"1H"`, `"4H"`, `"1D"`,
/// `"1W"`); `0` if unrecognized (then a candle is stamped at its open time).
#[must_use]
pub fn bar_millis(bar: &str) -> i64 {
    // Strip an optional `utc` suffix OKX appends to some day/week bars.
    let bar = bar
        .strip_suffix("utc")
        .or_else(|| bar.strip_suffix("UTC"))
        .unwrap_or(bar);
    let (num, unit) = bar.split_at(bar.len().saturating_sub(1));
    let n: i64 = num.parse().unwrap_or(0);
    let unit_ms = match unit {
        "m" => 60_000,
        "H" => 3_600_000,
        "D" => 86_400_000,
        "W" => 604_800_000,
        "M" => 2_592_000_000, // 30 days, nominal
        _ => 0,
    };
    n * unit_ms
}

// --- instrument type ---------------------------------------------------------

/// The OKX instrument type this connector is configured for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstType {
    /// Spot.
    Spot,
    /// Perpetual swap (USDT/USDC/coin-margined linear or inverse).
    Swap,
}

impl InstType {
    /// The `instType` query value (`"SPOT"` / `"SWAP"`) for the instruments and
    /// candles endpoints.
    #[must_use]
    pub fn query(self) -> &'static str {
        match self {
            InstType::Spot => "SPOT",
            InstType::Swap => "SWAP",
        }
    }
    fn td_mode(self) -> &'static str {
        match self {
            InstType::Spot => "cash",
            InstType::Swap => "cross",
        }
    }
    fn kind(self) -> InstrumentKind {
        match self {
            InstType::Spot => InstrumentKind::Spot,
            InstType::Swap => InstrumentKind::PerpetualFuture,
        }
    }
}

// --- catalogue ---------------------------------------------------------------

#[derive(Deserialize)]
struct InstrumentsResp {
    #[serde(default)]
    data: Vec<InstrumentRow>,
}

#[derive(Deserialize)]
struct InstrumentRow {
    #[serde(rename = "instId")]
    inst_id: String,
    #[serde(rename = "tickSz")]
    tick_sz: String,
    #[serde(rename = "lotSz")]
    lot_sz: String,
    #[serde(default)]
    state: String,
}

/// Maps OKX `instId`s to dense [`InstrumentSpec`]s and asset ids.
#[derive(Debug, Clone)]
pub struct OkxCatalog {
    specs: Vec<InstrumentSpec>,
    inst_ids: Vec<String>,
    scales: Vec<(u32, u32)>,
}

impl OkxCatalog {
    /// Parse `/api/v5/public/instruments?instType={SPOT|SWAP}` into a catalogue.
    /// Only `live` instruments are kept; base/quote are derived from the `instId`
    /// (`BTC-USDT` / `BTC-USDT-SWAP`). Each gets a dense [`InstrumentId`].
    ///
    /// # Errors
    /// [`OkxError::Parse`] on unparseable JSON or a bad filter value.
    pub fn from_instruments(json: &str, inst_type: InstType) -> Result<Self, OkxError> {
        let resp: InstrumentsResp =
            serde_json::from_str(json).map_err(|e| OkxError::Parse(e.to_string()))?;
        let mut specs = Vec::new();
        let mut inst_ids = Vec::new();
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
        let caps = if inst_type == InstType::Swap {
            CapSet::empty()
                .with(Capability::LimitOrders)
                .with(Capability::Margin)
                .with(Capability::Funding)
                .with(Capability::ShortSelling)
        } else {
            CapSet::empty().with(Capability::LimitOrders)
        };
        for row in resp.data {
            if !row.state.is_empty() && row.state != "live" {
                continue;
            }
            let mut parts = row.inst_id.split('-');
            let (Some(base), Some(quote)) = (parts.next(), parts.next()) else {
                continue;
            };
            let price_scale = scale_of(&row.tick_sz);
            let qty_scale = scale_of(&row.lot_sz);
            let id = InstrumentId::new(specs.len() as u32);
            let base = asset_id(base, &mut assets);
            let quote = asset_id(quote, &mut assets);
            specs.push(InstrumentSpec::new(
                id,
                base,
                quote,
                inst_type.kind(),
                Price::from_raw(decimal_to_raw(&row.tick_sz, price_scale)?),
                Qty::from_raw(decimal_to_raw(&row.lot_sz, qty_scale)?),
                Money::from_raw(0), // OKX reports min size, not min notional
                caps,
            ));
            inst_ids.push(row.inst_id);
            scales.push((price_scale, qty_scale));
        }
        Ok(OkxCatalog {
            specs,
            inst_ids,
            scales,
        })
    }

    /// All specs (dense, in id order) — pass to `Engine::new`.
    #[must_use]
    pub fn specs(&self) -> &[InstrumentSpec] {
        &self.specs
    }

    /// The [`InstrumentId`] for `inst_id` (e.g. `"BTC-USDT"`), if present.
    #[must_use]
    pub fn id_of(&self, inst_id: &str) -> Option<InstrumentId> {
        self.inst_ids
            .iter()
            .position(|s| s == inst_id)
            .map(|i| InstrumentId::new(i as u32))
    }

    /// `(price_scale, qty_scale)` for an instrument.
    #[must_use]
    pub fn scales(&self, id: InstrumentId) -> Option<(u32, u32)> {
        self.scales.get(id.index() as usize).copied()
    }
}

impl InstrumentCatalog for OkxCatalog {
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

/// Parse an OKX `/market/candles` body into [`Bar`]s. Rows are
/// `[ts, o, h, l, c, vol, …]` with `ts` the window **open** time (ms); OKX returns
/// them newest-first, so they are sorted ascending and stamped at close time
/// (`ts + bar_millis`).
///
/// # Errors
/// [`OkxError::Parse`] on bad JSON, a short row, or a bad number.
pub fn parse_candles(
    json: &str,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
    bar: &str,
) -> Result<Vec<Bar>, OkxError> {
    let resp: CandlesResp =
        serde_json::from_str(json).map_err(|e| OkxError::Parse(e.to_string()))?;
    let interval_ms = bar_millis(bar);
    let mut bars = Vec::with_capacity(resp.data.len());
    for row in resp.data {
        if row.len() < 6 {
            return Err(OkxError::Parse(format!(
                "candle row has {} fields",
                row.len()
            )));
        }
        let open_ms: i64 = row[0]
            .parse()
            .map_err(|_| OkxError::Parse(format!("bad ts {:?}", row[0])))?;
        let close_ms = open_ms
            .checked_add(interval_ms)
            .ok_or_else(|| OkxError::Parse("ts overflow".into()))?;
        bars.push(Bar::new(
            instrument,
            Timestamp::from_nanos(
                close_ms
                    .checked_mul(1_000_000)
                    .ok_or_else(|| OkxError::Parse("ts overflow".into()))?,
            ),
            Price::from_raw(decimal_to_raw(&row[1], price_scale)?),
            Price::from_raw(decimal_to_raw(&row[2], price_scale)?),
            Price::from_raw(decimal_to_raw(&row[3], price_scale)?),
            Price::from_raw(decimal_to_raw(&row[4], price_scale)?),
            Qty::from_raw(decimal_to_raw(&row[5], qty_scale)?),
        ));
    }
    bars.sort_by_key(|b| b.ts.as_nanos()); // OKX returns newest-first
    Ok(bars)
}

/// A [`DataSource`] streaming OKX candles for one instrument (one fetch up to
/// `limit`, OKX's `/market/candles` cap is 300).
pub struct OkxCandleFeed<T> {
    transport: T,
    base_url: String,
    inst_id: String,
    instrument: InstrumentId,
    bar: String,
    price_scale: u32,
    qty_scale: u32,
    limit: u32,
    buffer: std::collections::VecDeque<Bar>,
    fetched: bool,
}

impl<T: Transport> OkxCandleFeed<T> {
    /// Create a feed for `inst_id` (mapped to `instrument`) at `bar` (e.g. `"1m"`).
    #[must_use]
    pub fn new(
        transport: T,
        base_url: impl Into<String>,
        inst_id: impl Into<String>,
        instrument: InstrumentId,
        bar: impl Into<String>,
        price_scale: u32,
        qty_scale: u32,
    ) -> Self {
        OkxCandleFeed {
            transport,
            base_url: base_url.into(),
            inst_id: inst_id.into(),
            instrument,
            bar: bar.into(),
            price_scale,
            qty_scale,
            limit: 100,
            buffer: std::collections::VecDeque::new(),
            fetched: false,
        }
    }

    /// Set the page size (OKX caps `/market/candles` at 300).
    #[must_use]
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = limit.min(300);
        self
    }

    fn fetch(&mut self) -> Result<(), OkxError> {
        let url = format!(
            "{}/api/v5/market/candles?instId={}&bar={}&limit={}",
            self.base_url, self.inst_id, self.bar, self.limit
        );
        let resp = self.transport.send(&HttpRequest {
            method: Method::Get,
            url,
            body: None,
            headers: Vec::new(),
        })?;
        if !resp.is_success() {
            return Err(OkxError::Transport(format!("candles HTTP {}", resp.status)));
        }
        let bars = parse_candles(
            &resp.body,
            self.instrument,
            self.price_scale,
            self.qty_scale,
            &self.bar,
        )?;
        self.buffer.extend(bars);
        Ok(())
    }
}

impl<T: Transport> DataSource for OkxCandleFeed<T> {
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

impl<T> core::fmt::Debug for OkxCandleFeed<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OkxCandleFeed")
            .field("inst_id", &self.inst_id)
            .field("bar", &self.bar)
            .field("buffered", &self.buffer.len())
            .finish_non_exhaustive()
    }
}

// --- execution client --------------------------------------------------------

/// One instrument's `instId` + scales + type, for [`OkxExec`] order encoding.
#[derive(Debug, Clone)]
pub struct InstrumentMeta {
    /// OKX instrument id (e.g. `"BTC-USDT"`).
    pub inst_id: String,
    /// Price fixed-point scale.
    pub price_scale: u32,
    /// Quantity fixed-point scale.
    pub qty_scale: u32,
    /// Spot or swap (selects `tdMode` and the spot market-buy `tgtCcy`).
    pub inst_type: InstType,
}

/// The akadro `client_order_id` tag set as OKX's `clOrdId` so a live fill on the
/// `orders` channel attributes back: `client_order_tag(ClientOrderId::new(7))` →
/// `"ak7"`. OKX `clOrdId` is alphanumeric.
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

/// An [`ExecutionClient`] that signs and submits OKX orders over REST. `submit`
/// posts a signed `/api/v5/trade/order` and emits [`AccountEvent::OrderAccepted`]
/// when the response `sCode` is `"0"`, else [`AccountEvent::OrderRejected`].
/// `observe` is a no-op: live fills arrive on the `orders` WebSocket channel.
pub struct OkxExec<T> {
    transport: T,
    base_url: String,
    api_key: String,
    secret: Vec<u8>,
    passphrase: String,
    meta: Vec<InstrumentMeta>,
    timestamp: String,
}

impl<T: Transport> OkxExec<T> {
    /// Create with credentials (key, secret, passphrase) and per-instrument
    /// [`InstrumentMeta`] (dense, in id order — typically from an [`OkxCatalog`]).
    #[must_use]
    pub fn new(
        transport: T,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        secret: impl Into<Vec<u8>>,
        passphrase: impl Into<String>,
        meta: Vec<InstrumentMeta>,
    ) -> Self {
        OkxExec {
            transport,
            base_url: base_url.into(),
            api_key: api_key.into(),
            secret: secret.into(),
            passphrase: passphrase.into(),
            meta,
            timestamp: String::new(),
        }
    }

    /// Set the ISO-8601 timestamp used for signing (the live transport derives it
    /// from the system clock; tests set it for determinism).
    #[must_use]
    pub fn with_timestamp(mut self, iso8601: impl Into<String>) -> Self {
        self.timestamp = iso8601.into();
        self
    }
}

impl<T: Transport> ExecutionClient for OkxExec<T> {
    #[allow(clippy::too_many_lines)]
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
        // post-only is maker-only (limit only); reduce-only is a swap concept — both
        // are contradictions on the wrong kind/instrument and are rejected locally,
        // mirroring the backtest (bug classes 1, 2).
        if order.post_only && !matches!(order.kind, OrderKind::Limit { .. }) {
            sink.emit(AccountEvent::OrderRejected {
                id,
                reason: RejectReason::InvalidOrder,
                ts: now,
            });
            return;
        }
        if order.reduce_only && m.inst_type == InstType::Spot {
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
        let sz = raw_to_decimal(order.qty.raw(), m.qty_scale);
        let tag = client_order_tag(id);
        let mut body = match order.kind {
            OrderKind::Market => {
                let mut b = format!(
                    "{{\"instId\":\"{}\",\"tdMode\":\"{}\",\"clOrdId\":\"{tag}\",\"side\":\"{side}\",\"ordType\":\"market\",\"sz\":\"{sz}\"",
                    m.inst_id,
                    m.inst_type.td_mode()
                );
                // Spot market orders size in base currency (not the default quote).
                if m.inst_type == InstType::Spot {
                    b.push_str(",\"tgtCcy\":\"base_ccy\"");
                }
                b.push('}');
                b
            }
            OrderKind::Limit { limit } => {
                // Encode tif/post-only into OKX's ordType (bug class 1): post_only ->
                // the maker-only `post_only`, else `ioc`/`fok` from the order's TIF,
                // else a resting `limit` (GTC). Previously hard-coded `"limit"`.
                let ord_type = if order.post_only {
                    "post_only"
                } else {
                    match order.tif {
                        TimeInForce::Ioc => "ioc",
                        TimeInForce::Fok => "fok",
                        _ => "limit",
                    }
                };
                format!(
                    "{{\"instId\":\"{}\",\"tdMode\":\"{}\",\"clOrdId\":\"{tag}\",\"side\":\"{side}\",\"ordType\":\"{ord_type}\",\"sz\":\"{sz}\",\"px\":\"{}\"}}",
                    m.inst_id,
                    m.inst_type.td_mode(),
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
        // reduce-only on a swap (bug class 2): inject before the closing brace.
        if order.reduce_only {
            body.insert_str(body.len() - 1, ",\"reduceOnly\":true");
        }
        let path = "/api/v5/trade/order";
        // Signing timestamp: the test/driver-set ISO string, or — when unset — the
        // handler's event time formatted as OKX ISO-8601, so a request is never
        // signed with an empty timestamp (which OKX rejects), and the signed bytes
        // match the header (bug class 7).
        let timestamp = if self.timestamp.is_empty() {
            format_iso8601_millis(now.as_nanos() / 1_000_000)
        } else {
            self.timestamp.clone()
        };
        let prehash = format!("{}{}{}{}", timestamp, Method::Post.as_str(), path, body);
        let signature = sign(&self.secret, &prehash);
        let req = HttpRequest {
            method: Method::Post,
            url: format!("{}{path}", self.base_url),
            body: Some(std::mem::take(&mut body)),
            headers: vec![
                ("OK-ACCESS-KEY".into(), self.api_key.clone()),
                ("OK-ACCESS-SIGN".into(), signature),
                ("OK-ACCESS-TIMESTAMP".into(), timestamp),
                ("OK-ACCESS-PASSPHRASE".into(), self.passphrase.clone()),
                ("Content-Type".into(), "application/json".into()),
            ],
        };
        match self.transport.send(&req) {
            Ok(resp) if resp.is_success() && order_accepted(&resp.body) => {
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
        // Live fills arrive on the OKX `orders` WebSocket channel (akadro-live).
    }
}

/// `true` if an order response carries `sCode":"0"` (per-order success).
fn order_accepted(body: &str) -> bool {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        data: Vec<OrderData>,
    }
    #[derive(Deserialize)]
    struct OrderData {
        #[serde(rename = "sCode", default)]
        s_code: String,
    }
    serde_json::from_str::<Resp>(body)
        .ok()
        .and_then(|r| r.data.into_iter().next())
        .is_some_and(|d| d.s_code == "0")
}

impl<T> core::fmt::Debug for OkxExec<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OkxExec")
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
pub use net::{ReqwestTransport, iso_timestamp};

/// Format epoch-milliseconds as `YYYY-MM-DDTHH:MM:SS.sssZ` (the OKX timestamp).
#[must_use]
pub fn format_iso8601_millis(ms: i64) -> String {
    // Days since 1970 → civil date (Howard Hinnant's algorithm), no chrono dep.
    let days = ms.div_euclid(86_400_000);
    let msod = ms.rem_euclid(86_400_000);
    let (hour, minute, second, milli) = (
        msod / 3_600_000,
        (msod / 60_000) % 60,
        (msod / 1000) % 60,
        msod % 1000,
    );
    // Civil date from days-since-epoch (Howard Hinnant's algorithm), no chrono.
    let zed = days + 719_468;
    let era = zed.div_euclid(146_097);
    let doe = zed.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year0 = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year0 + 1 } else { year0 };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTRUMENTS: &str = r#"{"code":"0","data":[
        {"instId":"BTC-USDT","tickSz":"0.1","lotSz":"0.00000001","minSz":"0.00001","state":"live"},
        {"instId":"DEAD-USDT","tickSz":"0.1","lotSz":"0.1","minSz":"1","state":"suspend"}
    ]}"#;

    #[test]
    fn sign_is_base64_hmac_sha256() {
        // Base64(HMAC-SHA256("Jefe", "what do ya want for nothing?")) — the RFC-4231
        // §4.3 digest, Base64-encoded.
        assert_eq!(
            sign(b"Jefe", "what do ya want for nothing?"),
            "W9zBRr9gdU5qBCQmCJV1x1oAPwidJzmDnexYuWTsOEM="
        );
    }

    #[test]
    fn decimal_helpers() {
        assert_eq!(scale_of("0.10"), 1);
        assert_eq!(scale_of("1"), 0);
        assert_eq!(decimal_to_raw("0.1", 1).unwrap(), 1);
        assert_eq!(decimal_to_raw("123.456", 2).unwrap(), 12345);
        assert!(decimal_to_raw("x", 2).is_err());
        assert_eq!(raw_to_decimal(12345, 2), "123.45");
        assert_eq!(raw_to_decimal(50, 0), "50");
        assert_eq!(bar_millis("1m"), 60_000);
        assert_eq!(bar_millis("4H"), 14_400_000);
        assert_eq!(bar_millis("1D"), 86_400_000);
        assert_eq!(bar_millis("1Dutc"), 86_400_000);
        assert_eq!(bar_millis("weird"), 0);
    }

    #[test]
    fn catalog_keeps_live_instruments() {
        let cat = OkxCatalog::from_instruments(INSTRUMENTS, InstType::Spot).unwrap();
        assert_eq!(cat.specs().len(), 1); // suspended skipped
        let id = cat.id_of("BTC-USDT").unwrap();
        assert_eq!(cat.scales(id), Some((1, 8)));
        let spec = cat.spec(id).unwrap();
        assert_eq!(spec.tick_size, Price::from_raw(1)); // 0.1 @ scale 1
        assert_eq!(spec.kind, InstrumentKind::Spot);
        assert!(cat.id_of("NONE").is_none());
    }

    #[test]
    fn swap_catalog_is_perpetual() {
        let swap = r#"{"code":"0","data":[{"instId":"BTC-USDT-SWAP","tickSz":"0.1","lotSz":"0.01","state":"live"}]}"#;
        let cat = OkxCatalog::from_instruments(swap, InstType::Swap).unwrap();
        let id = cat.id_of("BTC-USDT-SWAP").unwrap();
        assert_eq!(cat.spec(id).unwrap().kind, InstrumentKind::PerpetualFuture);
    }

    #[test]
    fn candles_parse_sorted_and_close_stamped() {
        // Newest-first, two 1-minute candles.
        let json = r#"{"code":"0","data":[
            ["1700000060000","105","106","104","105.5","8","1","1","1"],
            ["1700000000000","100","101","99","100.5","10","1","1","1"]
        ]}"#;
        let bars = parse_candles(json, InstrumentId::new(0), 2, 0, "1m").unwrap();
        assert_eq!(bars.len(), 2);
        // Sorted ascending; stamped at open + 60s.
        assert_eq!(bars[0].ts.as_nanos(), 1_700_000_060_000_000_000);
        assert_eq!(bars[0].open, Price::from_raw(10_000));
        assert_eq!(bars[1].ts.as_nanos(), 1_700_000_120_000_000_000);
        assert!(
            parse_candles(r#"{"data":[["1","2"]]}"#, InstrumentId::new(0), 2, 0, "1m").is_err()
        );
    }

    #[test]
    fn candle_feed_streams_then_exhausts() {
        let body =
            r#"{"code":"0","data":[["1700000000000","100","100","100","100","1","1","1","1"]]}"#;
        let mut feed = OkxCandleFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 200,
                body: body.into(),
            }]),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1m",
            2,
            0,
        )
        .with_limit(500);
        assert!(feed.next_event().is_some());
        assert!(feed.next_event().is_none());
        assert!(format!("{feed:?}").contains("OkxCandleFeed"));
    }

    struct SinkVec(Vec<AccountEvent>);
    impl EventSink for SinkVec {
        fn emit(&mut self, e: AccountEvent) {
            self.0.push(e);
        }
    }

    fn exec_with(responses: Vec<HttpResponse>, inst_type: InstType) -> OkxExec<MockTransport> {
        OkxExec::new(
            MockTransport::new(responses),
            "http://x",
            "KEY",
            b"secret".to_vec(),
            "pass",
            vec![InstrumentMeta {
                inst_id: "BTC-USDT".into(),
                price_scale: 1,
                qty_scale: 4,
                inst_type,
            }],
        )
        .with_timestamp("2020-12-08T09:08:57.715Z")
    }

    #[test]
    fn submit_signs_and_accepts_on_scode_zero() {
        let mut x = exec_with(
            vec![HttpResponse {
                status: 200,
                body:
                    r#"{"code":"0","data":[{"ordId":"312","clOrdId":"ak0","sCode":"0","sMsg":""}]}"#
                        .into(),
            }],
            InstType::Spot,
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
        assert!(body.contains("\"ordType\":\"market\""));
        assert!(body.contains("\"sz\":\"0.01\"")); // 100 @ qty_scale 4
        assert!(body.contains("\"tgtCcy\":\"base_ccy\"")); // spot market in base
        assert!(body.contains("\"clOrdId\":\"ak0\""));
        // Auth headers present.
        let names: Vec<&str> = sent.headers.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"OK-ACCESS-SIGN"));
        assert!(names.contains(&"OK-ACCESS-PASSPHRASE"));
    }

    #[test]
    fn submit_limit_and_swap_paths() {
        let mut x = exec_with(
            vec![HttpResponse {
                status: 200,
                body: r#"{"code":"0","data":[{"sCode":"0"}]}"#.into(),
            }],
            InstType::Swap,
        );
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
        assert!(body.contains("\"ordType\":\"limit\""));
        assert!(body.contains("\"px\":\"50\"")); // 500 @ price_scale 1
        assert!(body.contains("\"tdMode\":\"cross\"")); // swap
        assert!(!body.contains("tgtCcy")); // not a spot market order
        assert!(format!("{x:?}").contains("OkxExec"));
    }

    #[test]
    fn submit_encodes_tif_post_only_reduce_only() {
        let ok = || HttpResponse {
            status: 200,
            body: r#"{"code":"0","data":[{"sCode":"0"}]}"#.into(),
        };
        // IOC limit -> ordType "ioc" (not the default GTC "limit").
        let mut x = exec_with(vec![ok()], InstType::Spot);
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
                .contains("\"ordType\":\"ioc\"")
        );

        // post_only limit -> ordType "post_only" (maker-only, not a taker limit).
        let mut x = exec_with(vec![ok()], InstType::Spot);
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
                .contains("\"ordType\":\"post_only\"")
        );

        // swap reduce_only -> reduceOnly:true in the body.
        let mut x = exec_with(vec![ok()], InstType::Swap);
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
        let mut x = exec_with(vec![], InstType::Spot);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(3),
            OrderRequest::market(InstrumentId::new(0), Side::Buy, Qty::from_raw(100)).post_only(),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(s.0[0], AccountEvent::OrderRejected { .. }));
        assert!(x.transport.sent.is_empty());

        // reduce_only on a SPOT order -> rejected (spot has no reduce-only).
        let mut x = exec_with(vec![], InstType::Spot);
        let mut s = SinkVec(Vec::new());
        x.submit(
            ClientOrderId::new(4),
            OrderRequest::market(InstrumentId::new(0), Side::Sell, Qty::from_raw(100))
                .reduce_only(),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(s.0[0], AccountEvent::OrderRejected { .. }));
    }

    #[test]
    fn submit_rejections() {
        // Per-order failure (sCode != 0) → rejected.
        let mut x = exec_with(
            vec![HttpResponse {
                status: 200,
                body: r#"{"code":"1","data":[{"sCode":"51008","sMsg":"insufficient balance"}]}"#
                    .into(),
            }],
            InstType::Spot,
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
        let mut x2 = exec_with(vec![], InstType::Spot);
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
        // observe is a no-op.
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
        assert_eq!(parse_client_order_tag("manual"), None);
    }

    #[test]
    fn iso8601_formats_a_known_epoch() {
        // 1700000000000 ms = 2023-11-14T22:13:20.000Z.
        assert_eq!(
            format_iso8601_millis(1_700_000_000_000),
            "2023-11-14T22:13:20.000Z"
        );
        assert_eq!(format_iso8601_millis(715), "1970-01-01T00:00:00.715Z");
    }

    #[test]
    fn candle_feed_http_error_yields_no_events() {
        let mut feed = OkxCandleFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 500,
                body: "x".into(),
            }]),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1m",
            1,
            0,
        );
        assert!(feed.next_event().is_none());
    }

    #[test]
    fn edge_paths() {
        use akadro_core::TriggerBy;
        assert_eq!(Method::Get.as_str(), "GET");
        assert_eq!(InstType::Spot.query(), "SPOT");
        assert_eq!(InstType::Swap.query(), "SWAP");
        // Two live symbols sharing the USDT quote (asset reuse) + a malformed instId.
        let json = r#"{"code":"0","data":[
            {"instId":"BTC-USDT","tickSz":"0.1","lotSz":"0.1","state":"live"},
            {"instId":"ETH-USDT","tickSz":"0.01","lotSz":"0.01","state":"live"},
            {"instId":"BADID","tickSz":"0.1","lotSz":"0.1","state":"live"}
        ]}"#;
        let cat = OkxCatalog::from_instruments(json, InstType::Spot).unwrap();
        assert_eq!(cat.specs().len(), 2); // BADID (no '-') skipped

        // A 200 with malformed candle rows → parse error → no events.
        let mut feed = OkxCandleFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 200,
                body: r#"{"data":[["1"]]}"#.into(),
            }]),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1m",
            1,
            0,
        );
        assert!(feed.next_event().is_none());

        // An unsupported order kind (stop) is rejected locally without a venue call.
        let mut x = OkxExec::new(
            MockTransport::new(vec![]),
            "http://x",
            "k",
            b"s".to_vec(),
            "p",
            vec![InstrumentMeta {
                inst_id: "BTC-USDT".into(),
                price_scale: 1,
                qty_scale: 0,
                inst_type: InstType::Spot,
            }],
        );
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
