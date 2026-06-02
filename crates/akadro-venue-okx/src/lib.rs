// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! OKX venue connector (spot + perpetual **swap**, REST) for akadro — depends only
//! on `akadro-core`, so it adds a venue with **zero** changes to the engine or
//! strategies (the open/closed goal).
//!
//! It provides an [`OkxCatalog`] (`/api/v5/public/instruments` → [`InstrumentSpec`],
//! fetched via [`OkxCatalog::fetch`]), an [`OkxCandleFeed`] ([`DataSource`] over
//! `/api/v5/market/candles`, or — with [`OkxCandleFeed::with_range`] — paged
//! `/api/v5/market/history-candles` for an arbitrary historical window), perpetual
//! **funding** history ([`fetch_funding_history`] → a
//! `SimulatedExchange::with_funding_schedule` schedule), and an
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

// `scale_of` and `raw_to_decimal` are venue-neutral and live in `akadro-core` (DRY),
// re-exported here for this connector's public surface.
pub use akadro_core::{raw_to_decimal, scale_of};

/// Parse a decimal string to a raw fixed-point `i64` at `scale` (truncating excess
/// precision). `"0.1"` @ 1 → `1`; `"123.456"` @ 2 → `12345`. Thin adapter over
/// [`akadro_core::decimal_to_raw`] mapping a rejected value to this venue's error.
///
/// # Errors
/// [`OkxError::Parse`] if `s` is empty, non-numeric, or overflows `i64`.
pub fn decimal_to_raw(s: &str, scale: u32) -> Result<i64, OkxError> {
    akadro_core::decimal_to_raw(s, scale)
        .ok_or_else(|| OkxError::Parse(format!("bad decimal {s:?} at scale {scale}")))
}

/// Milliseconds in an OKX bar token (`"1s"`, `"1m"`, `"15m"`, `"1H"`, `"4H"`,
/// `"1D"`, `"1W"`); `0` if unrecognized (then a candle is stamped at its open time).
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
        "s" => 1_000, // OKX supports 1-second candles (spot)
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
    /// Fetch `/api/v5/public/instruments?instType={SPOT|SWAP}` over `transport` and
    /// parse it into a catalogue (see [`from_instruments`](Self::from_instruments)).
    /// This is the reusable connector entry point — callers never hand-build the URL
    /// or touch the response body.
    ///
    /// # Errors
    /// [`OkxError::Transport`] on a transport failure or a non-2xx status;
    /// [`OkxError::Parse`] on malformed JSON.
    pub fn fetch<T: Transport>(
        transport: &mut T,
        base_url: &str,
        inst_type: InstType,
    ) -> Result<Self, OkxError> {
        let resp = transport.send(&HttpRequest {
            method: Method::Get,
            url: format!(
                "{base_url}/api/v5/public/instruments?instType={}",
                inst_type.query()
            ),
            body: None,
            headers: Vec::new(),
        })?;
        if !resp.is_success() {
            return Err(OkxError::Transport(format!(
                "instruments HTTP {}",
                resp.status
            )));
        }
        Self::from_instruments(&resp.body, inst_type)
    }

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
        // Skip the current, still-forming bar: OKX sets the `confirm` field
        // (index 8) to "0" while a candle is in progress and "1" once closed.
        // Only judge when that field is present (>= 9 columns) so a legitimately
        // shorter row is never mistaken for unclosed.
        if row.len() >= 9 && row[8] != "1" {
            continue;
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

// --- funding-rate history ----------------------------------------------------

/// Funding-rate fixed-point scale — re-exported from `akadro-core` so the connector
/// and the engine's `mul_rate` charge cannot drift. Rates normalize to `1e-8`
/// fractions (8 decimals): OKX's `"0.0001"` (0.01% per settlement) → `10_000` (= 1
/// bp), and a sub-bp `"0.0000466"` → `4_660` instead of rounding to `0` as a
/// basis-point scale would. See `SimulatedExchange::with_funding_schedule`.
pub use akadro_core::FUNDING_RATE_SCALE;

#[derive(Deserialize)]
struct FundingHistResp {
    #[serde(default)]
    data: Vec<FundingHistRow>,
}

#[derive(Deserialize)]
struct FundingHistRow {
    #[serde(rename = "fundingRate")]
    funding_rate: String,
    #[serde(rename = "fundingTime")]
    funding_time: String, // OKX returns the epoch-ms as a string
}

/// Parse `/api/v5/public/funding-rate-history` into a `(timestamp, rate)` schedule
/// for `SimulatedExchange::with_funding_schedule`, sorted ascending by time.
/// `fundingRate` is a decimal fraction normalized to [`FUNDING_RATE_SCALE`] (`1e-8`),
/// so OKX's routinely **sub-bp** rates (e.g. `0.0000466` = 0.47 bp → `4_660`) are
/// preserved and accrue, rather than rounding to `0` at basis-point granularity. OKX
/// returns newest-first, so the result is re-sorted.
///
/// # Errors
/// [`OkxError::Parse`] on bad JSON, a bad rate, or a bad/overflowing time.
pub fn parse_funding_rate_history(json: &str) -> Result<Vec<(Timestamp, i64)>, OkxError> {
    let resp: FundingHistResp =
        serde_json::from_str(json).map_err(|e| OkxError::Parse(e.to_string()))?;
    let mut out: Vec<(Timestamp, i64)> =
        resp.data
            .into_iter()
            .map(|r| {
                let ms: i64 = r.funding_time.trim().parse().map_err(|_| {
                    OkxError::Parse(format!("bad fundingTime {:?}", r.funding_time))
                })?;
                let ns = ms
                    .checked_mul(1_000_000)
                    .ok_or_else(|| OkxError::Parse("fundingTime overflow".into()))?;
                Ok((
                    Timestamp::from_nanos(ns),
                    decimal_to_raw(&r.funding_rate, FUNDING_RATE_SCALE)?,
                ))
            })
            .collect::<Result<_, OkxError>>()?;
    out.sort_by_key(|(t, _)| t.as_nanos());
    Ok(out)
}

/// Fetch `/api/v5/public/funding-rate-history` for `inst_id` over `transport` and
/// parse it into an ascending `(timestamp, rate)` schedule for
/// `SimulatedExchange::with_funding_schedule`. `limit` caps the number of past
/// settlements returned (OKX's own cap is 100, applied here). The reusable
/// connector entry point — callers never build the URL or touch the body.
///
/// Rates are at [`FUNDING_RATE_SCALE`] (`1e-8`), so OKX's typically **sub-bp** rates
/// (e.g. `0.0000466` = 0.47 bp) accrue correctly when fed to `with_funding_schedule`
/// — they are no longer rounded to `0` as they were under the old basis-point scale.
///
/// # Errors
/// [`OkxError::Transport`] on a transport failure or a non-2xx status;
/// [`OkxError::Parse`] on malformed JSON.
pub fn fetch_funding_history<T: Transport>(
    transport: &mut T,
    base_url: &str,
    inst_id: &str,
    limit: u32,
) -> Result<Vec<(Timestamp, i64)>, OkxError> {
    let resp = transport.send(&HttpRequest {
        method: Method::Get,
        url: format!(
            "{base_url}/api/v5/public/funding-rate-history?instId={inst_id}&limit={}",
            limit.min(100)
        ),
        body: None,
        headers: Vec::new(),
    })?;
    if !resp.is_success() {
        return Err(OkxError::Transport(format!(
            "funding-rate-history HTTP {}",
            resp.status
        )));
    }
    parse_funding_rate_history(&resp.body)
}

/// Page `/api/v5/public/funding-rate-history` back over `max_pages` (100/page) into
/// one ascending [`FUNDING_RATE_SCALE`] schedule — enough to cover a long backtest
/// window, where a single page (~33 days at the 8h cadence) under-covers funding.
/// OKX pages by the `after=<ms>` cursor (records strictly older than it); each page
/// advances the cursor to the oldest settlement seen. Stops early on an empty page;
/// de-duplicates by settlement time. The reusable connector entry point.
///
/// # Errors
/// [`OkxError::Transport`] on a transport failure or a non-2xx status;
/// [`OkxError::Parse`] on malformed JSON.
pub fn fetch_funding_history_paged<T: Transport>(
    transport: &mut T,
    base_url: &str,
    inst_id: &str,
    max_pages: u32,
) -> Result<Vec<(Timestamp, i64)>, OkxError> {
    let mut all: Vec<(Timestamp, i64)> = Vec::new();
    let mut after: Option<i64> = None; // ms cursor; None = most-recent page
    for _ in 0..max_pages.max(1) {
        let mut url =
            format!("{base_url}/api/v5/public/funding-rate-history?instId={inst_id}&limit=100");
        if let Some(a) = after {
            use core::fmt::Write as _;
            let _ = write!(url, "&after={a}");
        }
        let resp = transport.send(&HttpRequest {
            method: Method::Get,
            url,
            body: None,
            headers: Vec::new(),
        })?;
        if !resp.is_success() {
            return Err(OkxError::Transport(format!(
                "funding-rate-history HTTP {}",
                resp.status
            )));
        }
        let page = parse_funding_rate_history(&resp.body)?; // ascending
        let Some((oldest, _)) = page.first().copied() else {
            break;
        };
        let oldest_ms = oldest.as_nanos() / 1_000_000;
        all.extend(page);
        if after.is_some_and(|prev| oldest_ms >= prev) {
            break; // no backward progress (duplicate / non-monotonic page)
        }
        after = Some(oldest_ms); // next page: strictly older
    }
    all.sort_by_key(|(t, _)| t.as_nanos());
    all.dedup_by_key(|(t, _)| t.as_nanos());
    Ok(all)
}

/// The settlement period (milliseconds) implied by a funding `schedule`: the
/// smallest positive gap between consecutive settlements, or the standard 8h when
/// there are fewer than two points to infer it from. Divide by the bar length to get
/// `with_funding_schedule`'s `interval_bars`. Inference is venue-neutral, so this is
/// a re-export of [`akadro_core::infer_funding_period_ms`] (DRY across connectors).
pub use akadro_core::infer_funding_period_ms as funding_period_ms;

/// A [`DataSource`] streaming OKX candles for one instrument. By default it does a
/// single `/market/candles` fetch (the recent window, up to `limit`, cap 300); set
/// [`with_range`](Self::with_range) to instead page `/market/history-candles`
/// (newest→oldest, 100/page) back to cover an arbitrary `[start, end)` window.
pub struct OkxCandleFeed<T> {
    transport: T,
    base_url: String,
    inst_id: String,
    instrument: InstrumentId,
    bar: String,
    price_scale: u32,
    qty_scale: u32,
    limit: u32,
    /// `Some((start_ms, end_ms))` → page `history-candles` over `[start, end)`;
    /// `None` → a single recent `/market/candles` fetch.
    range: Option<(i64, i64)>,
    /// Courtesy delay between `history-candles` pages (rate-limit politeness);
    /// also the unit of the bounded 429 back-off. `0` in tests.
    page_delay: std::time::Duration,
    /// Bounded retries on a `429` (rate-limited) page before giving up.
    max_retries: u32,
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
            range: None,
            page_delay: std::time::Duration::from_millis(120),
            max_retries: 8,
            buffer: std::collections::VecDeque::new(),
            fetched: false,
        }
    }

    /// Set the page size (OKX caps `/market/candles` at 300, `history-candles` at 100).
    #[must_use]
    pub fn with_limit(mut self, limit: u32) -> Self {
        // history-candles (range mode) caps at 100; the recent /market/candles at
        // 300. Re-apply the range cap so `with_limit` after `with_range` can't ask
        // for 300 from a 100-capped endpoint regardless of builder order.
        let cap = if self.range.is_some() { 100 } else { 300 };
        self.limit = limit.min(cap);
        self
    }

    /// Backfill an arbitrary historical window `[start_ms, end_ms)` (epoch-ms on the
    /// candle **open** time) by paging `/market/history-candles` newest→oldest
    /// instead of the single recent `/market/candles` fetch — the way to pull, say,
    /// a full day of `1s` bars. The page size is clamped to OKX's 100 cap and the
    /// returned bars are ascending, de-duplicated, and close-stamped exactly like
    /// [`parse_candles`].
    #[must_use]
    pub fn with_range(mut self, start_ms: i64, end_ms: i64) -> Self {
        self.range = Some((start_ms, end_ms));
        self.limit = self.limit.min(100);
        self
    }

    /// Override the inter-page courtesy delay (default 120 ms). Set to
    /// [`Duration::ZERO`](std::time::Duration::ZERO) in tests to page without sleeping.
    #[must_use]
    pub fn with_page_delay(mut self, delay: std::time::Duration) -> Self {
        self.page_delay = delay;
        self
    }

    fn fetch(&mut self) -> Result<(), OkxError> {
        match self.range {
            Some((start_ms, end_ms)) => self.fetch_range(start_ms, end_ms),
            None => self.fetch_recent(),
        }
    }

    /// One recent `/market/candles` page (up to `limit`, ≤300).
    fn fetch_recent(&mut self) -> Result<(), OkxError> {
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

    /// Page `/market/history-candles` newest→oldest until `[start_ms, end_ms)` is
    /// covered, then buffer the window ascending + de-duplicated. `after` is OKX's
    /// exclusive cursor on the candle open time, so each page advances it to the
    /// oldest open seen. A leading-page error surfaces; a mid-range error keeps the
    /// pages already fetched (so a transient failure deep in a backfill is not fatal).
    fn fetch_range(&mut self, start_ms: i64, end_ms: i64) -> Result<(), OkxError> {
        let bar_ms = bar_millis(&self.bar);
        if bar_ms == 0 {
            return Err(OkxError::Parse(format!("unknown bar {:?}", self.bar)));
        }
        // One knob: a zero `page_delay` (tests) also means a zero 429 back-off, so
        // the paging loop runs without ever sleeping.
        let backoff = if self.page_delay.is_zero() {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_secs(2)
        };
        let mut all: Vec<Bar> = Vec::new();
        let mut cursor = end_ms; // OKX `after` is exclusive on the OPEN ts
        let mut retries = 0u32;
        loop {
            let url = format!(
                "{}/api/v5/market/history-candles?instId={}&bar={}&after={}&limit={}",
                self.base_url, self.inst_id, self.bar, cursor, self.limit
            );
            let resp = self.transport.send(&HttpRequest {
                method: Method::Get,
                url,
                body: None,
                headers: Vec::new(),
            })?;
            if !resp.is_success() {
                if resp.status == 429 && retries < self.max_retries {
                    retries += 1;
                    if !backoff.is_zero() {
                        std::thread::sleep(backoff);
                    }
                    continue;
                }
                if all.is_empty() {
                    return Err(OkxError::Transport(format!(
                        "history-candles HTTP {}",
                        resp.status
                    )));
                }
                break; // mid-range error: keep the pages already fetched
            }
            retries = 0;
            let page = parse_candles(
                &resp.body,
                self.instrument,
                self.price_scale,
                self.qty_scale,
                &self.bar,
            )?;
            if page.is_empty() {
                break;
            }
            // Pages are ascending + close-stamped; the oldest OPEN ts drives `after`.
            let oldest_open_ms =
                page.first().expect("non-empty").ts.as_nanos() / 1_000_000 - bar_ms;
            all.extend(page);
            if oldest_open_ms <= start_ms {
                break;
            }
            if oldest_open_ms >= cursor {
                break; // no backward progress (duplicate / non-monotonic page)
            }
            cursor = oldest_open_ms;
            if !self.page_delay.is_zero() {
                std::thread::sleep(self.page_delay);
            }
        }
        all.sort_by_key(|b| b.ts.as_nanos());
        all.dedup_by_key(|b| b.ts.as_nanos());
        self.buffer = all
            .into_iter()
            .filter(|b| {
                let open = b.ts.as_nanos() / 1_000_000 - bar_ms;
                open >= start_ms && open < end_ms
            })
            .collect();
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
            .field("range", &self.range)
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

    /// Set the ISO-8601 timestamp used for signing. When unset, `submit` signs with
    /// the current handler's event time; set this for deterministic signing in tests.
    /// (There is no automatic system-clock derivation yet — once the `akadro-live`
    /// shell lands it will drive [`ExecutionClient::sync_clock`] to keep this current.)
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
        assert_eq!(bar_millis("1s"), 1_000); // OKX 1-second candles
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
    fn one_second_candle_stamps_at_close_not_open() {
        // OKX 1s candle, open ts 1700000000000 ms → must stamp at close = open + 1s,
        // not at open (the bug a missing "s" unit caused).
        let json =
            r#"{"code":"0","data":[["1700000000000","100","100","100","100","1","1","1","1"]]}"#;
        let bars = parse_candles(json, InstrumentId::new(0), 0, 0, "1s").unwrap();
        assert_eq!(bars[0].ts.as_nanos(), 1_700_000_001_000_000_000); // open + 1000ms, in ns
    }

    #[test]
    fn parse_candles_skips_unclosed_bar() {
        // The current bar arrives with confirm="0" (index 8); it must be dropped so a
        // partial OHLCV with a future close-stamp never reaches a strategy.
        let json = r#"{"code":"0","data":[
            ["1700000060000","9","9","9","9","1","1","1","0"],
            ["1700000000000","100","100","100","100","1","1","1","1"]
        ]}"#;
        let bars = parse_candles(json, InstrumentId::new(0), 0, 0, "1m").unwrap();
        assert_eq!(bars.len(), 1); // only the confirmed bar
        assert_eq!(bars[0].ts.as_nanos(), 1_700_000_060_000_000_000); // open 1700000000 + 60s
        // A short row with no confirm field is kept (not mistaken for unclosed).
        let short = r#"{"code":"0","data":[["1700000000000","1","1","1","1","1"]]}"#;
        assert_eq!(
            parse_candles(short, InstrumentId::new(0), 0, 0, "1m")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn funding_rate_history_parses_to_fine_scale_sorted() {
        // OKX returns newest-first; fundingTime is a ms string, fundingRate a decimal.
        let json = r#"{"code":"0","data":[
            {"fundingRate":"0.0001","fundingTime":"1780300800000","instId":"BTC-USDT-SWAP","realizedRate":"0.0001"},
            {"fundingRate":"-0.0000466","fundingTime":"1780272000000","instId":"BTC-USDT-SWAP","realizedRate":"-0.0000466"}
        ]}"#;
        let sched = parse_funding_rate_history(json).unwrap();
        assert_eq!(sched.len(), 2);
        // Sorted ascending; at FUNDING_RATE_SCALE (1e-8): 0.0001 → 10_000 (= 1 bp),
        // and the sub-bp -0.0000466 → -4_660 (preserved, not rounded to 0).
        assert_eq!(
            sched[0],
            (Timestamp::from_nanos(1_780_272_000_000_000_000), -4_660)
        );
        assert_eq!(
            sched[1],
            (Timestamp::from_nanos(1_780_300_800_000_000_000), 10_000)
        );
        assert!(parse_funding_rate_history("not json").is_err());
        assert!(
            parse_funding_rate_history(r#"{"data":[{"fundingRate":"0.0001","fundingTime":"x"}]}"#)
                .is_err()
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

    #[test]
    fn catalog_fetch_via_transport() {
        let mut t = MockTransport::new(vec![HttpResponse {
            status: 200,
            body: INSTRUMENTS.into(),
        }]);
        let cat = OkxCatalog::fetch(&mut t, "http://x", InstType::Spot).unwrap();
        assert!(cat.id_of("BTC-USDT").is_some());
        // The connector built the right public-instruments URL.
        assert!(
            t.sent[0]
                .url
                .contains("/api/v5/public/instruments?instType=SPOT")
        );
        // A non-2xx status is a Transport error, not a silent empty catalogue.
        let mut bad = MockTransport::new(vec![HttpResponse {
            status: 503,
            body: String::new(),
        }]);
        assert!(OkxCatalog::fetch(&mut bad, "http://x", InstType::Spot).is_err());
    }

    /// A `history-candles` page: `opens` are open-times in ms, newest-first (as OKX
    /// returns them), one flat 1-minute candle each.
    fn hist_page(opens: &[i64]) -> HttpResponse {
        let rows: Vec<String> = opens
            .iter()
            .map(|o| format!(r#"["{o}","100","100","100","100","1","1","1","1"]"#))
            .collect();
        HttpResponse {
            status: 200,
            body: format!(r#"{{"code":"0","data":[{}]}}"#, rows.join(",")),
        }
    }

    #[test]
    fn range_feed_pages_window_ascending() {
        // Window [0, 300_000) = five 1m bars (opens 0,60k,120k,180k,240k). OKX pages
        // newest→oldest, 2 per page here, then an empty page terminates.
        let feed = OkxCandleFeed::new(
            MockTransport::new(vec![
                hist_page(&[240_000, 180_000]),
                hist_page(&[120_000, 60_000]),
                hist_page(&[0]),
                hist_page(&[]),
            ]),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1m",
            2,
            0,
        )
        .with_range(0, 300_000)
        .with_page_delay(std::time::Duration::ZERO);
        let mut feed = feed;
        let mut bars = Vec::new();
        while let Some(Event::Bar(b)) = feed.next_event() {
            bars.push(b);
        }
        assert_eq!(bars.len(), 5);
        // Ascending, close-stamped at open + 60s.
        assert_eq!(bars[0].ts.as_nanos(), 60_000_000_000); // open 0 + 60s
        assert_eq!(bars[4].ts.as_nanos(), 300_000_000_000); // open 240k + 60s
        for w in bars.windows(2) {
            assert!(w[0].ts.as_nanos() < w[1].ts.as_nanos());
        }
    }

    #[test]
    fn range_feed_filters_to_window_and_dedups() {
        // A page reaches past `start`; bars with open < start (here -60_000) and a
        // duplicate open are dropped, leaving only [60_000, 120_000).
        let feed = OkxCandleFeed::new(
            MockTransport::new(vec![
                hist_page(&[120_000, 60_000, 60_000, 0, -60_000]),
                hist_page(&[]),
            ]),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1m",
            2,
            0,
        )
        .with_range(60_000, 180_000)
        .with_page_delay(std::time::Duration::ZERO);
        let mut feed = feed;
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 2); // opens 60k & 120k; 0 and -60k out of window, dup 60k collapsed
    }

    #[test]
    fn range_feed_retries_on_429_then_yields() {
        // A leading 429 must be retried (zero back-off at page_delay 0), not fatal.
        let feed = OkxCandleFeed::new(
            MockTransport::new(vec![
                HttpResponse {
                    status: 429,
                    body: String::new(),
                },
                hist_page(&[0]),
                hist_page(&[]),
            ]),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1m",
            2,
            0,
        )
        .with_range(0, 60_000)
        .with_page_delay(std::time::Duration::ZERO);
        let mut feed = feed;
        assert!(feed.next_event().is_some()); // survived the 429
        assert!(feed.next_event().is_none());
    }

    #[test]
    fn range_feed_leading_error_surfaces_as_none() {
        // A non-429 error on the very first page (nothing buffered) yields no events.
        let feed = OkxCandleFeed::new(
            MockTransport::new(vec![HttpResponse {
                status: 500,
                body: String::new(),
            }]),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1m",
            2,
            0,
        )
        .with_range(0, 60_000)
        .with_page_delay(std::time::Duration::ZERO);
        let mut feed = feed;
        assert!(feed.next_event().is_none());
    }

    #[test]
    fn range_feed_unknown_bar_errors() {
        let feed = OkxCandleFeed::new(
            MockTransport::new(vec![hist_page(&[0])]),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "weird", // bar_millis → 0
            2,
            0,
        )
        .with_range(0, 60_000)
        .with_page_delay(std::time::Duration::ZERO);
        let mut feed = feed;
        assert!(feed.next_event().is_none()); // bar_ms == 0 → fetch errors
    }

    fn okx_range_feed(
        responses: Vec<HttpResponse>,
        start: i64,
        end: i64,
    ) -> OkxCandleFeed<MockTransport> {
        OkxCandleFeed::new(
            MockTransport::new(responses),
            "http://x",
            "BTC-USDT",
            InstrumentId::new(0),
            "1m",
            2,
            0,
        )
        .with_range(start, end)
        .with_page_delay(std::time::Duration::ZERO)
    }

    fn ok_page(opens_ms: &[i64]) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: hist_page(opens_ms).body,
        }
    }

    #[test]
    fn range_feed_guards_against_no_progress() {
        // A page whose oldest open does not move the `after` cursor back must NOT
        // loop forever: the same page returned twice terminates via the guard.
        let p = || ok_page(&[1_700_000_300_000, 1_700_000_360_000]);
        let mut feed = okx_range_feed(vec![p(), p()], 1_700_000_000_000, 1_700_000_600_000);
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 2); // de-duplicated; the guard stopped the stall
    }

    #[test]
    fn range_feed_429_exhausted_yields_none() {
        // 429 past max_retries on the first page → error → empty feed (not a hang).
        let r429 = || HttpResponse {
            status: 429,
            body: String::new(),
        };
        let mut feed = okx_range_feed(
            (0..10).map(|_| r429()).collect(),
            1_700_000_000_000,
            1_700_000_060_000,
        );
        assert!(feed.next_event().is_none());
    }

    #[test]
    fn range_feed_mid_range_error_keeps_fetched_pages() {
        // Page 1 succeeds (oldest > start → would page again), then a non-429 error:
        // the already-fetched page is kept, not discarded.
        let mut feed = okx_range_feed(
            vec![
                ok_page(&[1_700_000_300_000, 1_700_000_360_000]),
                HttpResponse {
                    status: 500,
                    body: String::new(),
                },
            ],
            1_700_000_000_000,
            1_700_000_600_000,
        );
        let mut n = 0;
        while feed.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n, 2); // page 1 retained despite the mid-range 500
    }

    #[test]
    fn funding_history_fetch_and_period() {
        // Three 8h-apart settlements (newest-first, as OKX returns).
        let body = r#"{"code":"0","data":[
            {"fundingRate":"0.0001","fundingTime":"1780300800000"},
            {"fundingRate":"0.0002","fundingTime":"1780272000000"},
            {"fundingRate":"-0.0001","fundingTime":"1780243200000"}
        ]}"#;
        let mut t = MockTransport::new(vec![HttpResponse {
            status: 200,
            body: body.into(),
        }]);
        let sched = fetch_funding_history(&mut t, "http://x", "BTC-USDT-SWAP", 100).unwrap();
        assert_eq!(sched.len(), 3);
        assert!(
            t.sent[0]
                .url
                .contains("funding-rate-history?instId=BTC-USDT-SWAP")
        );
        assert_eq!(funding_period_ms(&sched), 28_800_000); // 8h
        // Fallbacks when the period can't be inferred.
        assert_eq!(funding_period_ms(&[]), 28_800_000);
        assert_eq!(
            funding_period_ms(&[(Timestamp::from_nanos(0), 1)]),
            28_800_000
        );
        let mut bad = MockTransport::new(vec![HttpResponse {
            status: 500,
            body: String::new(),
        }]);
        assert!(fetch_funding_history(&mut bad, "http://x", "X", 100).is_err());
    }

    #[test]
    fn funding_history_paged_walks_after_cursor() {
        // Two pages (newest-first within each), then an empty page terminates.
        let page1 = r#"{"code":"0","data":[
            {"fundingRate":"0.0001","fundingTime":"1780300800000"},
            {"fundingRate":"0.0002","fundingTime":"1780272000000"}
        ]}"#;
        let page2 = r#"{"code":"0","data":[
            {"fundingRate":"-0.0001","fundingTime":"1780243200000"},
            {"fundingRate":"0.0003","fundingTime":"1780214400000"}
        ]}"#;
        let empty = r#"{"code":"0","data":[]}"#;
        let mut t = MockTransport::new(vec![
            HttpResponse {
                status: 200,
                body: page1.into(),
            },
            HttpResponse {
                status: 200,
                body: page2.into(),
            },
            HttpResponse {
                status: 200,
                body: empty.into(),
            },
        ]);
        let sched = fetch_funding_history_paged(&mut t, "http://x", "BTC-USDT-SWAP", 10).unwrap();
        assert_eq!(sched.len(), 4); // both pages concatenated
        // Page 1 had no cursor; page 2 paged by `after` = page-1's oldest settlement.
        assert!(!t.sent[0].url.contains("after="));
        assert!(t.sent[1].url.contains("after=1780272000000"));
        assert!(t.sent[2].url.contains("after=1780214400000"));
        // Ascending + de-duplicated.
        for w in sched.windows(2) {
            assert!(w[0].0.as_nanos() < w[1].0.as_nanos());
        }
    }

    #[test]
    fn funding_history_paged_guards_no_progress_and_dedups() {
        // The same page twice: the `after` cursor can't go older → terminate + dedup.
        let page = r#"{"code":"0","data":[
            {"fundingRate":"0.0001","fundingTime":"1780300800000"},
            {"fundingRate":"0.0002","fundingTime":"1780272000000"}
        ]}"#;
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
        let sched = fetch_funding_history_paged(&mut t, "http://x", "X", 10).unwrap();
        assert_eq!(sched.len(), 2); // 2 unique settlements; guard stopped the stall
        assert_eq!(t.sent.len(), 2); // page 1, then page 2 detects no progress → stop
        // A non-2xx mid-paging surfaces as an error.
        let mut bad = MockTransport::new(vec![HttpResponse {
            status: 500,
            body: String::new(),
        }]);
        assert!(fetch_funding_history_paged(&mut bad, "http://x", "X", 10).is_err());
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
