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
    OrderKind, OrderRequest, PageSink, Price, Qty, RejectReason, Side, TimeInForce, Timestamp,
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

// `scale_of` and `raw_to_decimal` are venue-neutral and live in `akadro-core` (DRY),
// re-exported here for this connector's public surface.
pub use akadro_core::{raw_to_decimal, scale_of};

/// Parse a decimal string to a raw fixed-point `i64` at `scale` (truncating excess
/// precision). Thin adapter over [`akadro_core::decimal_to_raw`] mapping a rejected
/// value to this venue's error.
///
/// # Errors
/// [`KucoinError::Parse`] if `s` is empty, non-numeric, or overflows `i64`.
pub fn decimal_to_raw(s: &str, scale: u32) -> Result<i64, KucoinError> {
    akadro_core::decimal_to_raw(s, scale)
        .ok_or_else(|| KucoinError::Parse(format!("bad decimal {s:?} at scale {scale}")))
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
    /// Fetch `/api/v1/symbols` over `transport` and parse it into a catalogue (see
    /// [`from_symbols`](Self::from_symbols)). The reusable connector entry point —
    /// callers never build the URL or touch the response body.
    ///
    /// # Errors
    /// [`KucoinError::Transport`] on a transport failure or a non-2xx status;
    /// [`KucoinError::Parse`] on malformed JSON.
    pub fn fetch<T: Transport>(transport: &mut T, base_url: &str) -> Result<Self, KucoinError> {
        let resp = transport.send(&HttpRequest {
            method: Method::Get,
            url: format!("{base_url}/api/v1/symbols"),
            body: None,
            headers: Vec::new(),
        })?;
        if !resp.is_success() {
            return Err(KucoinError::Transport(format!(
                "symbols HTTP {}",
                resp.status
            )));
        }
        Self::from_symbols(&resp.body)
    }

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
/// **Unclosed last candle.** KuCoin candle rows carry no "closed" flag. When the
/// requested range reaches the present, the newest row (sorted last here) is the
/// still-forming candle, whose values keep changing. This parser returns it as-is;
/// for reproducible runs back-fill a window ending before the present, or drop a
/// trailing bar whose `time + candle_seconds` exceeds server time.
///
/// # Errors
/// [`KucoinError::Parse`] on bad JSON, a short row, a bad number, or an
/// unrecognized `candle_type` (e.g. `"1month"` — KuCoin spot tops out at `"1week"`).
/// Failing fast here means *every* path (no-range and back-fill) rejects a bad type
/// rather than silently stamping bars at open time with a zero close offset (m31).
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
    if interval_s == 0 {
        return Err(KucoinError::Parse(format!(
            "unsupported candle type {candle_type:?} (KuCoin spot: 1min…1week)"
        )));
    }
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

// --- funding-rate history (KuCoin Futures) -----------------------------------

/// KuCoin **Futures** REST host — funding lives here, distinct from the spot host
/// this connector otherwise targets.
pub const FUTURES_BASE_URL: &str = "https://api-futures.kucoin.com";

/// Funding-rate fixed-point scale — re-exported from `akadro-core` so the connector
/// and the engine's `mul_rate` charge cannot drift. Rates normalize to `1e-8`
/// fractions: KuCoin's `0.0001` (1 bp) → `10_000`, and a sub-bp `0.0000466` →
/// `4_660` instead of rounding to `0`. See `SimulatedExchange::with_funding_schedule`.
pub use akadro_core::FUNDING_RATE_SCALE;

#[derive(Deserialize)]
struct FundingResp {
    #[serde(default)]
    data: Vec<FundingRow>,
}
#[derive(Deserialize)]
struct FundingRow {
    // Verified live (2026-06-01): the field is `timepoint` (lowercase p) and the
    // rate is `fundingRate`, returned as a JSON number in scientific notation
    // (e.g. `1.49E-4`) — handled by `funding_rate_to_raw` via a fixed-decimal render.
    timepoint: i64, // settlement epoch-ms
    #[serde(rename = "fundingRate")]
    funding_rate: serde_json::Value,
}

/// Convert a JSON funding rate (KuCoin's `value` is a number; a string is accepted
/// too) to a raw fixed-point value at [`FUNDING_RATE_SCALE`]. A JSON number is
/// rendered to a fixed (non-exponential) decimal first, so a tiny sub-bp rate is not
/// emitted as `1e-7` and no `f64` reaches the money path beyond this one venue-data
/// boundary parse.
fn funding_rate_to_raw(v: &serde_json::Value) -> Result<i64, KucoinError> {
    match v {
        serde_json::Value::String(s) => decimal_to_raw(s, FUNDING_RATE_SCALE),
        serde_json::Value::Number(n) => {
            let f = n
                .as_f64()
                .ok_or_else(|| KucoinError::Parse("funding rate not numeric".into()))?;
            decimal_to_raw(&format!("{f:.18}"), FUNDING_RATE_SCALE)
        }
        other => Err(KucoinError::Parse(format!("bad funding value {other}"))),
    }
}

/// Parse a KuCoin Futures `GET /api/v1/contract/funding-rates` body into an
/// ascending `(timestamp, rate)` schedule for `SimulatedExchange::with_funding_schedule`.
/// Each row is `{timePoint (ms), value}`; the `value` is normalized to
/// [`FUNDING_RATE_SCALE`] (`1e-8`), so KuCoin's sub-bp rates are preserved rather
/// than rounded to `0`. The result is re-sorted ascending.
///
/// # Errors
/// [`KucoinError::Parse`] on bad JSON, a bad rate, or a timestamp overflow.
pub fn parse_funding_rate(json: &str) -> Result<Vec<(Timestamp, i64)>, KucoinError> {
    let resp: FundingResp =
        serde_json::from_str(json).map_err(|e| KucoinError::Parse(e.to_string()))?;
    let mut out: Vec<(Timestamp, i64)> = resp
        .data
        .iter()
        .map(|r| {
            let ns = r
                .timepoint
                .checked_mul(1_000_000)
                .ok_or_else(|| KucoinError::Parse("timepoint overflow".into()))?;
            Ok((
                Timestamp::from_nanos(ns),
                funding_rate_to_raw(&r.funding_rate)?,
            ))
        })
        .collect::<Result<_, KucoinError>>()?;
    out.sort_by_key(|(t, _)| t.as_nanos());
    Ok(out)
}

/// Fetch `/api/v1/contract/funding-rates` for the futures `symbol` (e.g.
/// `"XBTUSDTM"`) over the time range `[from_ms, to_ms]` and parse it into a
/// `with_funding_schedule` schedule (rates at [`FUNDING_RATE_SCALE`]). Pass
/// [`FUTURES_BASE_URL`] as `base_url` — funding is a **futures** endpoint, and this
/// spot connector has no perpetual catalogue, so pair the returned schedule with a
/// `PerpetualFuture` `InstrumentSpec` you supply. A public endpoint — no signing.
///
/// # Errors
/// [`KucoinError::Transport`] on a transport failure or a non-2xx status;
/// [`KucoinError::Parse`] on malformed JSON.
pub fn fetch_funding_history<T: Transport>(
    transport: &mut T,
    base_url: &str,
    symbol: &str,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<(Timestamp, i64)>, KucoinError> {
    let resp = transport.send(&HttpRequest {
        method: Method::Get,
        url: format!(
            "{base_url}/api/v1/contract/funding-rates?symbol={symbol}&from={from_ms}&to={to_ms}"
        ),
        body: None,
        headers: Vec::new(),
    })?;
    if !resp.is_success() {
        return Err(KucoinError::Transport(format!(
            "contract/funding-rates HTTP {}",
            resp.status
        )));
    }
    parse_funding_rate(&resp.body)
}

/// Page `/api/v1/contract/funding-rates` back over `[from_ms, to_ms]` into one
/// ascending [`FUNDING_RATE_SCALE`] schedule. KuCoin caps each call at ~100
/// settlements (~33 days at the 8h cadence) anchored on `to`, so a long window needs
/// several pages: each moves `to` to just before the oldest settlement seen, up to
/// `max_pages`. Stops early on an empty page; de-duplicates by time. The reusable
/// connector entry point — pass [`FUTURES_BASE_URL`].
///
/// # Errors
/// [`KucoinError::Transport`] on a transport failure or a non-2xx status;
/// [`KucoinError::Parse`] on malformed JSON.
pub fn fetch_funding_history_paged<T: Transport>(
    transport: &mut T,
    base_url: &str,
    symbol: &str,
    from_ms: i64,
    to_ms: i64,
    max_pages: u32,
) -> Result<Vec<(Timestamp, i64)>, KucoinError> {
    let mut all: Vec<(Timestamp, i64)> = Vec::new();
    let mut cursor_to = to_ms;
    for _ in 0..max_pages.max(1) {
        let page = fetch_funding_history(transport, base_url, symbol, from_ms, cursor_to)?; // ascending
        let Some((oldest, _)) = page.first().copied() else {
            break;
        };
        let oldest_ms = oldest.as_nanos() / 1_000_000;
        all.extend(page);
        if oldest_ms <= from_ms {
            break;
        }
        let next_to = oldest_ms - 1;
        if next_to >= cursor_to {
            break; // no backward progress
        }
        cursor_to = next_to;
    }
    all.sort_by_key(|(t, _)| t.as_nanos());
    all.dedup_by_key(|(t, _)| t.as_nanos());
    all.retain(|(t, _)| {
        let ms = t.as_nanos() / 1_000_000;
        ms >= from_ms && ms <= to_ms
    });
    Ok(all)
}

/// A [`DataSource`] streaming KuCoin candles for one instrument (a single recent
/// fetch — KuCoin returns up to 1500 candles — or, with
/// [`with_range`](KucoinCandleFeed::with_range), a paged historical window).
pub struct KucoinCandleFeed<T> {
    transport: T,
    base_url: String,
    symbol: String,
    instrument: InstrumentId,
    candle_type: String,
    price_scale: u32,
    qty_scale: u32,
    /// `Some((start_ms, end_ms))` → page back over `[start, end)`; `None` → one
    /// recent fetch (KuCoin returns up to 1500 candles).
    range: Option<(i64, i64)>,
    /// Inter-page courtesy delay + 429 back-off unit; `0` in tests.
    page_delay: std::time::Duration,
    /// Bounded retries on a `429` page.
    max_retries: u32,
    /// Opt-in per-page sink for incremental cache journaling (resumable back-fill).
    page_sink: Option<Box<dyn PageSink>>,
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

    /// Back-fill an arbitrary historical window `[start_ms, end_ms)` (epoch-ms on the
    /// candle **open** time) by paging `/api/v1/market/candles` newest→oldest (KuCoin
    /// returns ≤1500 candles per call, newest-first, bounded by `startAt`/`endAt` in
    /// **seconds**), instead of the single recent fetch. Bars come back ascending,
    /// de-duplicated, close-stamped exactly like [`parse_candles`].
    #[must_use]
    pub fn with_range(mut self, start_ms: i64, end_ms: i64) -> Self {
        self.range = Some((start_ms, end_ms));
        self
    }

    /// Override the inter-page courtesy delay (default 120 ms; also the 429 back-off
    /// unit). Set to [`Duration::ZERO`](std::time::Duration::ZERO) in tests.
    #[must_use]
    pub fn with_page_delay(mut self, delay: std::time::Duration) -> Self {
        self.page_delay = delay;
        self
    }

    /// One `/api/v1/market/candles` page bounded by optional `[startAt, endAt]`
    /// (**seconds**), with a bounded 429 back-off (zero at `page_delay` 0).
    fn fetch_page(
        &mut self,
        start_s: Option<i64>,
        end_s: Option<i64>,
    ) -> Result<Vec<Bar>, KucoinError> {
        use core::fmt::Write as _;
        let mut url = format!(
            "{}/api/v1/market/candles?type={}&symbol={}",
            self.base_url, self.candle_type, self.symbol
        );
        if let Some(s) = start_s {
            let _ = write!(url, "&startAt={s}");
        }
        if let Some(e) = end_s {
            let _ = write!(url, "&endAt={e}");
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
                return Err(KucoinError::Transport(format!(
                    "candles HTTP {}",
                    resp.status
                )));
            }
            return parse_candles(
                &resp.body,
                self.instrument,
                self.price_scale,
                self.qty_scale,
                &self.candle_type,
            );
        }
    }

    /// Page `[start_ms, end_ms)` newest→oldest by moving the `endAt` cursor (seconds)
    /// back to just before the oldest bar received, until a page reaches `start`.
    fn backfill(&mut self, start_ms: i64, end_ms: i64) -> Result<Vec<Bar>, KucoinError> {
        let interval_secs = candle_seconds(&self.candle_type);
        if interval_secs == 0 {
            return Err(KucoinError::Parse(format!(
                "unsupported candle type {:?}",
                self.candle_type
            )));
        }
        let start_secs = start_ms / 1000;
        let mut cursor_end_secs = end_ms / 1000;
        let mut all: Vec<Bar> = Vec::new();
        loop {
            let page = self.fetch_page(Some(start_secs), Some(cursor_end_secs))?;
            if page.is_empty() {
                break;
            }
            // `ts` is close time (ns); open = close - interval. Oldest open (secs)
            // drives the next (earlier) `endAt` cursor.
            let oldest_open_secs = page
                .iter()
                .map(|b| b.ts.as_nanos() / 1_000_000_000 - interval_secs)
                .min()
                .expect("non-empty");
            if let Some(sink) = self.page_sink.as_mut() {
                sink.on_page(&page); // incremental journal before buffering (resumable)
            }
            all.extend(page);
            if oldest_open_secs <= start_secs {
                break;
            }
            let next_end = oldest_open_secs - 1; // strictly older than the oldest we have
            if next_end >= cursor_end_secs {
                break; // no backward progress
            }
            cursor_end_secs = next_end;
            if !self.page_delay.is_zero() {
                std::thread::sleep(self.page_delay);
            }
        }
        all.sort_by_key(|b| b.ts.as_nanos());
        all.dedup_by_key(|b| b.ts.as_nanos());
        let interval_ms = interval_secs * 1000;
        all.retain(|b| {
            let open_ms = b.ts.as_nanos() / 1_000_000 - interval_ms;
            open_ms >= start_ms && open_ms < end_ms
        });
        Ok(all)
    }

    fn fetch(&mut self) -> Result<(), KucoinError> {
        let bars = match self.range {
            Some((s, e)) => self.backfill(s, e)?,
            None => self.fetch_page(None, None)?,
        };
        self.buffer.extend(bars);
        Ok(())
    }
}

impl<T: Transport> DataSource for KucoinCandleFeed<T> {
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

    /// Pin a **fixed** millisecond timestamp for request signing — intended for
    /// deterministic tests. When left **unset** (the default), each request signs
    /// with the handler's event time (`now`), which under a live event-time clock
    /// tracks wall-clock; that is the correct live behaviour. Do **not** call this in
    /// live: a pinned timestamp is reused verbatim for every request and soon falls
    /// outside KuCoin's accepted window, so signed orders start being rejected (m36).
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
        assert_eq!(candle_seconds("1month"), 0); // KuCoin spot has no monthly candle
    }

    #[test]
    fn parse_candles_rejects_unrecognized_type() {
        // m31: an unsupported candle type (e.g. "1month") must fail fast on every
        // path, not silently produce open-stamped bars with a zero close offset.
        let body = r#"{"data":[["1700000000","100","101","102","99","10","1"]]}"#;
        assert!(
            parse_candles(body, InstrumentId::new(0), 2, 2, "1month").is_err(),
            "unrecognized candle type rejected"
        );
        // A recognized type still parses (and close-stamps).
        let bars = parse_candles(body, InstrumentId::new(0), 2, 2, "1min").unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].ts.as_nanos(), 1_700_000_060_000_000_000);
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
    fn funding_rate_parses_numbers_to_fine_scale_sorted() {
        // EXACT live shape (verified 2026-06-01): `fundingRate` is a JSON number in
        // SCIENTIFIC notation, the timestamp field is `timepoint` (lowercase p).
        let json = r#"{"code":"200000","data":[
            {"symbol":"XBTUSDTM","fundingRate":1.49E-4,"timepoint":1700028800000},
            {"symbol":"XBTUSDTM","fundingRate":-1.6E-5,"timepoint":1700000000000}
        ]}"#;
        let sched = parse_funding_rate(json).unwrap();
        assert_eq!(sched.len(), 2);
        // Sorted ascending; at FUNDING_RATE_SCALE (1e-8): 1.49E-4 (= 0.000149 = 1.49 bp)
        // → 14_900; the sub-bp -1.6E-5 (-0.16 bp) → -1_600 (preserved, exponent handled).
        assert_eq!(
            sched[0],
            (Timestamp::from_nanos(1_700_000_000_000_000_000), -1_600)
        );
        assert_eq!(
            sched[1],
            (Timestamp::from_nanos(1_700_028_800_000_000_000), 14_900)
        );
        // A string rate is accepted too; bad JSON errors.
        let str_val = r#"{"data":[{"timepoint":1700000000000,"fundingRate":"0.0001"}]}"#;
        assert_eq!(parse_funding_rate(str_val).unwrap()[0].1, 10_000);
        assert!(parse_funding_rate("nope").is_err());
    }

    #[test]
    fn fetch_funding_history_builds_futures_url() {
        let mut t = MockTransport::new(vec![HttpResponse {
            status: 200,
            body: r#"{"code":"200000","data":[{"symbol":"XBTUSDTM","fundingRate":1.49E-4,"timepoint":1700000000000}]}"#.into(),
        }]);
        let sched = fetch_funding_history(
            &mut t,
            FUTURES_BASE_URL,
            "XBTUSDTM",
            1_700_000_000_000,
            1_700_100_000_000,
        )
        .unwrap();
        assert_eq!(sched.len(), 1);
        assert!(
            t.sent[0]
                .url
                .contains("/api/v1/contract/funding-rates?symbol=XBTUSDTM&from=1700000000000")
        );
        assert!(t.sent[0].headers.is_empty()); // public endpoint — unsigned
        let mut bad = MockTransport::new(vec![HttpResponse {
            status: 500,
            body: String::new(),
        }]);
        assert!(fetch_funding_history(&mut bad, FUTURES_BASE_URL, "XBTUSDTM", 0, 1).is_err());
    }

    #[test]
    fn catalog_fetch_via_transport() {
        let mut t = MockTransport::new(vec![HttpResponse {
            status: 200,
            body: SYMBOLS.into(),
        }]);
        let cat = KucoinCatalog::fetch(&mut t, "http://x").unwrap();
        assert!(cat.id_of("BTC-USDT").is_some());
        assert!(t.sent[0].url.ends_with("/api/v1/symbols"));
        let mut bad = MockTransport::new(vec![HttpResponse {
            status: 503,
            body: String::new(),
        }]);
        assert!(KucoinCatalog::fetch(&mut bad, "http://x").is_err());
    }

    /// A `/api/v1/market/candles` page (KuCoin order [time, o, c, h, l, vol, turn],
    /// flat 100) for the given **open** seconds.
    fn kc_candle_page(opens_s: &[i64]) -> String {
        let rows = opens_s
            .iter()
            .map(|t| format!(r#"["{t}","100","100","100","100","1","1"]"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"code":"200000","data":[{rows}]}}"#)
    }

    #[test]
    fn range_feed_pages_backward_by_end() {
        // Two 5-bar 1min pages spanning opens [0..540]s within [0,600)s; newest→oldest.
        let recent = kc_candle_page(&[
            1_700_000_300,
            1_700_000_360,
            1_700_000_420,
            1_700_000_480,
            1_700_000_540,
        ]);
        let older = kc_candle_page(&[
            1_700_000_000,
            1_700_000_060,
            1_700_000_120,
            1_700_000_180,
            1_700_000_240,
        ]);
        let mut feed = KucoinCandleFeed::new(
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
            "BTC-USDT",
            InstrumentId::new(0),
            "1min",
            1,
            0,
        )
        .with_range(1_700_000_000_000, 1_700_000_600_000)
        .with_page_delay(std::time::Duration::ZERO);
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 10);
    }

    fn kc_range_feed(
        bodies: Vec<HttpResponse>,
        start_ms: i64,
        end_ms: i64,
    ) -> KucoinCandleFeed<MockTransport> {
        KucoinCandleFeed::new(
            MockTransport::new(bodies),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1min",
            1,
            0,
        )
        .with_range(start_ms, end_ms)
        .with_page_delay(std::time::Duration::ZERO)
    }

    fn kc_ok(body: String) -> HttpResponse {
        HttpResponse { status: 200, body }
    }

    #[test]
    fn range_feed_guards_against_no_progress() {
        let p = || kc_ok(kc_candle_page(&[1_700_000_300, 1_700_000_360]));
        let mut feed = kc_range_feed(vec![p(), p()], 1_700_000_000_000, 1_700_000_600_000);
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 2); // de-duplicated; guard stopped the stall
    }

    #[test]
    fn range_feed_clips_window_and_dedups() {
        // open seconds; start = 1_700_000_060 s. The 1_700_000_000 bar is < start
        // (clipped); the 1_700_000_060 duplicate collapses.
        let page = kc_candle_page(&[1_700_000_000, 1_700_000_060, 1_700_000_060, 1_700_000_120]);
        let mut feed = kc_range_feed(vec![kc_ok(page)], 1_700_000_060_000, 1_700_000_600_000);
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 2); // 60 & 120; 0 clipped, dup collapsed
    }

    #[test]
    fn range_feed_429_exhausted_yields_none() {
        let mut feed = kc_range_feed(
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
        let mut feed = KucoinCandleFeed::new(
            MockTransport::new(vec![
                HttpResponse {
                    status: 429,
                    body: String::new(),
                },
                HttpResponse {
                    status: 200,
                    body: kc_candle_page(&[1_700_000_000]),
                },
            ]),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1min",
            1,
            0,
        )
        .with_range(1_700_000_000_000, 1_700_000_060_000)
        .with_page_delay(std::time::Duration::ZERO);
        assert!(feed.next_event().is_some()); // survived the 429
    }

    #[test]
    fn funding_history_paged_walks_back() {
        let p1 = r#"{"code":"200000","data":[
            {"symbol":"XBTUSDTM","fundingRate":1.0E-4,"timepoint":1700000120000},
            {"symbol":"XBTUSDTM","fundingRate":2.0E-4,"timepoint":1700000060000}]}"#;
        let p2 = r#"{"code":"200000","data":[
            {"symbol":"XBTUSDTM","fundingRate":-1.0E-4,"timepoint":1700000000000}]}"#;
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
            FUTURES_BASE_URL,
            "XBTUSDTM",
            1_700_000_000_000,
            1_700_000_120_000,
            10,
        )
        .unwrap();
        assert_eq!(sched.len(), 3);
        assert!(t.sent[0].url.contains("to=1700000120000"));
        assert!(t.sent[1].url.contains("to=1700000059999")); // oldest(60000)-1
    }

    #[test]
    fn funding_history_paged_guards_no_progress() {
        // Same page twice (oldest above `from`): the `to` cursor stalls → stop.
        let page = r#"{"code":"200000","data":[
            {"symbol":"XBTUSDTM","fundingRate":1.0E-4,"timepoint":1700000120000},
            {"symbol":"XBTUSDTM","fundingRate":2.0E-4,"timepoint":1700000060000}]}"#;
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
        let sched = fetch_funding_history_paged(
            &mut t,
            FUTURES_BASE_URL,
            "XBTUSDTM",
            0,
            1_700_000_120_000,
            10,
        )
        .unwrap();
        assert_eq!(sched.len(), 2); // de-duplicated; guard stopped the stall
        assert_eq!(t.sent.len(), 2);
        let mut bad = MockTransport::new(vec![HttpResponse {
            status: 500,
            body: String::new(),
        }]);
        assert!(fetch_funding_history_paged(&mut bad, FUTURES_BASE_URL, "X", 0, 1, 10).is_err());
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
    fn debug_hides_secret() {
        // Credential-hiding regression lock (secret = b"secret" via exec_with).
        let dbg = format!("{:?}", exec_with(vec![]));
        assert!(
            dbg.contains("KucoinExec"),
            "Debug should still name the type"
        );
        assert!(
            !dbg.contains("secret"),
            "Debug must not leak the API secret"
        );
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
