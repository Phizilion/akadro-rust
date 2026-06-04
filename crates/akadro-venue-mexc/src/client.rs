// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The MEXC spot connector: a [`DataSource`] (REST klines) and an
//! [`ExecutionClient`] (signed order submit + REST-poll fills), both over the
//! [`Transport`] seam so they are fully testable without a network.
//!
//! Fills in this REST-only v1 are detected by polling order status each bar; the
//! fee is modelled as the venue's flat spot taker rate (`fee_bps`, default 5 =
//! 0.05%) because order-status does not report the realised fee. Account-event
//! timestamps use the engine's event time (`now`) so behaviour matches the
//! backtest; the *signing* timestamp is wall-clock and set via
//! [`MexcSpotExec::set_clock_ms`].

use std::collections::VecDeque;
use std::fmt;

use akadro_core::{
    AccountEvent, BackfillProgress, CancelReason, ClientOrderId, Cost, CostKind, Costs, DataSource,
    Event, EventSink, ExecutionClient, InstrumentCatalog, InstrumentId, OrderKind, OrderRequest,
    PageSink, Qty, RejectReason, Side, TimeInForce, Timestamp,
};

use crate::convert::raw_to_decimal;
use crate::error::{MexcError, map_reject_code};
use crate::instrument::MexcCatalog;
use crate::parse::{parse_klines, parse_order_ack, parse_order_status};
use crate::request::{Method, signed};
use crate::transport::{HttpRequest, Transport};

/// MEXC's hard cap on the klines page size (bars per request).
pub const MAX_KLINES_LIMIT: u32 = 1000;

/// A [`DataSource`] that fetches MEXC spot klines for one instrument and replays
/// them as [`Event::Bar`]s (historical backfill / bar-cadence live polling).
pub struct MexcKlineFeed<T> {
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
    /// Opt-in per-page progress callback for a long `with_range` back-fill (D18:
    /// the connector emits venue-neutral numbers; the consumer renders the bar).
    progress: Option<Box<dyn FnMut(BackfillProgress)>>,
    /// Opt-in per-page sink for incremental cache journaling (the cache layer
    /// installs its `.partial` writer here so a long back-fill is resumable).
    page_sink: Option<Box<dyn PageSink>>,
    buffer: VecDeque<akadro_core::Bar>,
    fetched: bool,
}

impl<T: Transport> MexcKlineFeed<T> {
    /// Create a feed for `symbol` (mapped to `instrument`) at `interval`
    /// (e.g. `"1m"`). `price_scale`/`qty_scale` come from the catalogue.
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
        MexcKlineFeed {
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
            progress: None,
            page_sink: None,
            buffer: VecDeque::new(),
            fetched: false,
        }
    }

    /// Register a per-page progress callback, invoked after each page of a
    /// [`with_range`](Self::with_range) back-fill with the running
    /// [`BackfillProgress`] (pages, bars, frontier). A consumer renders a download
    /// bar from it; the connector itself does no terminal I/O. The callback never
    /// fires on a cache hit (no download happens) or a single-page recent fetch.
    #[must_use]
    pub fn with_progress(mut self, f: impl FnMut(BackfillProgress) + 'static) -> Self {
        self.progress = Some(Box::new(f));
        self
    }

    /// Install a per-page [`PageSink`] (the cache's incremental journal), handed each
    /// page of a [`with_range`](Self::with_range) back-fill before it is buffered, so
    /// the download is resumable after an interrupt. Like `with_progress`, it never
    /// fires on a single recent fetch. The cache layer supplies this; user code does
    /// not need it.
    #[must_use]
    pub fn with_page_sink(mut self, sink: Box<dyn PageSink>) -> Self {
        self.page_sink = Some(sink);
        self
    }

    /// Restrict the feed to `[start_ms, end_ms)` (epoch-ms) and **back-fill the
    /// whole window**, paginating across as many requests as needed (MEXC caps
    /// each call, so the window is fetched page by page; any leading gap from the
    /// venue's finite retention of fine intervals is skipped). Without a range, a
    /// single recent page is fetched.
    #[must_use]
    pub fn with_range(mut self, start_ms: i64, end_ms: i64) -> Self {
        self.start_ms = Some(start_ms);
        self.end_ms = Some(end_ms);
        self
    }

    /// Set the page size (bars per request; MEXC caps this, default 500). Clamped
    /// to MEXC's hard cap of [`MAX_KLINES_LIMIT`] (1000) — a larger value would be
    /// silently truncated by the venue and could leave gaps. With a range, more
    /// bars are still fetched by paginating.
    #[must_use]
    pub fn with_limit(mut self, limit: u32) -> Self {
        // Clamp to [1, MAX]: a `0` limit would make the leading-gap cursor advance
        // by zero and spin the back-fill loop forever (m17).
        self.limit = limit.clamp(1, MAX_KLINES_LIMIT);
        self
    }

    /// One bar length in milliseconds for the configured interval, or `0` if the
    /// interval is unrecognized (then leading-gap skipping is disabled).
    fn interval_ms(&self) -> i64 {
        match self.interval.as_str() {
            "1m" => 60_000,
            "5m" => 300_000,
            "15m" => 900_000,
            "30m" => 1_800_000,
            "60m" | "1h" => 3_600_000,
            "4h" => 14_400_000,
            "1d" => 86_400_000,
            "1W" => 7 * 86_400_000,
            "1M" => 30 * 86_400_000, // conservative 30-day month for gap-skipping
            _ => 0,
        }
    }

    /// Fetch one klines page (optional `startTime`/`endTime`), parsed to bars.
    fn fetch_page(
        &mut self,
        start: Option<i64>,
        end: Option<i64>,
    ) -> Result<Vec<akadro_core::Bar>, MexcError> {
        // klines is a public endpoint — no signature needed.
        use core::fmt::Write as _;
        let mut query = format!(
            "symbol={}&interval={}&limit={}",
            self.symbol, self.interval, self.limit
        );
        if let Some(s) = start {
            let _ = write!(query, "&startTime={s}");
        }
        if let Some(e) = end {
            let _ = write!(query, "&endTime={e}");
        }
        let req = HttpRequest {
            method: Method::Get,
            url: format!("{}/api/v3/klines?{}", self.base_url, query),
            api_key: None,
            body: None,
        };
        let resp = self.transport.send(&req)?;
        if !resp.is_success() {
            return Err(MexcError::Transport(format!("klines HTTP {}", resp.status)));
        }
        parse_klines(
            &resp.body,
            self.instrument,
            self.price_scale,
            self.qty_scale,
        )
    }

    /// Back-fill the whole `[start, end)` window by paging the endpoint (MEXC caps
    /// each call at `limit`), advancing the cursor to each page's last close until
    /// the window is covered or the venue returns an empty page. Skips a leading
    /// gap left by the venue's finite retention of fine intervals — so callers get
    /// the full range without hand-rolling pagination.
    fn backfill(&mut self, start: i64, end: i64) -> Result<Vec<akadro_core::Bar>, MexcError> {
        let bar_ms = self.interval_ms();
        let mut all: Vec<akadro_core::Bar> = Vec::new();
        let mut cursor = start;
        let mut pages: u32 = 0;
        while cursor < end {
            let page = match self.fetch_page(Some(cursor), Some(end - 1)) {
                Ok(p) => p,
                // A mid-range page error must not discard the pages already fetched:
                // return the partial result so a transient hiccup yields the bars up
                // to the failure rather than zero (m18). An error on the very first
                // page (nothing buffered yet) is still surfaced.
                Err(e) => {
                    if all.is_empty() {
                        return Err(e);
                    }
                    // Mid-range failure with pages already buffered: keep them (m18),
                    // but signal the truncation so the cache loader does not dead-mark
                    // the unfetched remainder as verified-empty (silent data loss).
                    if let Some(sink) = self.page_sink.as_mut() {
                        sink.on_error(&e.to_string());
                    }
                    break;
                }
            };
            if page.is_empty() {
                if all.is_empty() && bar_ms > 0 {
                    cursor += i64::from(self.limit) * bar_ms; // skip a leading gap
                    continue;
                }
                break; // reached the end of available history
            }
            // Advance the cursor to the last close and keep paging. (Do NOT stop on
            // a page shorter than `limit`: MEXC caps a klines response below the
            // requested limit, so a short page is normal mid-range, not the end.)
            let last_close_ms = page.last().expect("non-empty").ts.as_nanos() / 1_000_000;
            if let Some(sink) = self.page_sink.as_mut() {
                sink.on_page(&page); // incremental journal before buffering (resumable)
            }
            all.extend(page);
            pages += 1;
            if let Some(cb) = self.progress.as_mut() {
                cb(BackfillProgress::new(pages, all.len(), last_close_ms));
            }
            if last_close_ms <= cursor {
                break; // no forward progress
            }
            cursor = last_close_ms;
        }
        // Pages can overlap at the cursor boundary (the cursor advances to a page's last
        // close, which the next page may repeat). Sort + dedup by close-stamp so the
        // assembled series has no duplicate bars — matching the other venue back-fills.
        all.sort_by_key(|b| b.ts.as_nanos());
        all.dedup_by_key(|b| b.ts.as_nanos());
        Ok(all)
    }

    fn fetch(&mut self) -> Result<(), MexcError> {
        let bars = match (self.start_ms, self.end_ms) {
            (Some(start), Some(end)) => self.backfill(start, end)?,
            _ => self.fetch_page(self.start_ms, self.end_ms)?,
        };
        self.buffer.extend(bars);
        Ok(())
    }
}

impl<T: Transport> DataSource for MexcKlineFeed<T> {
    fn next_event(&mut self) -> Option<Event> {
        if !self.fetched {
            self.fetched = true;
            if let Err(e) = self.fetch() {
                // Truncated download (first-page failure): tell the page-sink so the
                // cache loader does not dead-mark the remainder verified-empty.
                if let Some(sink) = self.page_sink.as_mut() {
                    sink.on_error(&e.to_string());
                }
                return None;
            }
        }
        self.buffer.pop_front().map(Event::Bar)
    }
}

impl<T> fmt::Debug for MexcKlineFeed<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MexcKlineFeed")
            .field("symbol", &self.symbol)
            .field("interval", &self.interval)
            .field("buffered", &self.buffer.len())
            .finish_non_exhaustive()
    }
}

impl MexcCatalog {
    /// Fetch and parse spot `exchangeInfo` for `symbol` from `base_url` (issues the
    /// `/api/v3/exchangeInfo?symbol=…` request, so callers need not build it).
    ///
    /// # Errors
    /// Returns a [`MexcError`] on transport failure, a non-2xx response, or
    /// unparseable JSON.
    pub fn fetch<T: Transport>(
        transport: &mut T,
        base_url: &str,
        symbol: &str,
    ) -> Result<MexcCatalog, MexcError> {
        let req = HttpRequest {
            method: Method::Get,
            url: format!("{base_url}/api/v3/exchangeInfo?symbol={symbol}"),
            api_key: None,
            body: None,
        };
        let resp = transport.send(&req)?;
        if !resp.is_success() {
            return Err(MexcError::Transport(format!(
                "exchangeInfo HTTP {}",
                resp.status
            )));
        }
        MexcCatalog::from_exchange_info(&resp.body)
    }
}

/// One order awaiting fills, tracked for REST-poll fill detection.
#[derive(Debug, Clone)]
struct Pending {
    client_id: ClientOrderId,
    venue_order_id: String,
    instrument: InstrumentId,
    side: Side,
    price_scale: u32,
    qty_scale: u32,
    /// The order's total quantity (raw) — lets a fill be marked `complete` by
    /// quantity rather than by the venue's status string, so an order that reports
    /// its full quantity under `PARTIALLY_FILLED` before a later `FILLED` does not
    /// leak as a never-closed open order (M5).
    order_qty: i64,
    prev_filled: i64,
    /// Cumulative quote (raw, `price_scale`) already attributed to prior fills, so
    /// each new fill's price is the **marginal** tranche price, not the cumulative
    /// VWAP (M6).
    prev_quote: i128,
}

/// A MEXC spot [`ExecutionClient`].
pub struct MexcSpotExec<T> {
    transport: T,
    base_url: String,
    api_key: String,
    api_secret: Vec<u8>,
    catalog: MexcCatalog,
    fee_bps: i64,
    recv_window: Option<u64>,
    clock_ms: i64,
    /// Correction (server − local) applied to the signing clock, set by
    /// [`MexcSpotExec::sync_time`]. Guards against host clock drift exceeding
    /// `recv_window` (which the venue would reject with `700003`).
    time_offset_ms: i64,
    pending: Vec<Pending>,
}

impl<T: Transport> MexcSpotExec<T> {
    /// Create a spot execution client.
    #[must_use]
    pub fn new(
        transport: T,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        api_secret: impl Into<Vec<u8>>,
        catalog: MexcCatalog,
    ) -> Self {
        MexcSpotExec {
            transport,
            base_url: base_url.into(),
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            catalog,
            fee_bps: 5, // MEXC spot taker default (0.05%)
            recv_window: Some(5000),
            clock_ms: 0,
            time_offset_ms: 0,
            pending: Vec::new(),
        }
    }

    /// The signing timestamp (ms): the driver-set wall clock plus the synced server
    /// offset, or the handler's event time as a fallback when no clock has been set.
    /// A never-set clock previously signed every request at epoch 0, which MEXC
    /// rejects with `700003`; the fallback keeps a request signable even if a driver
    /// forgets [`set_clock_ms`](Self::set_clock_ms) (m16).
    fn sign_clock(&self, now: Timestamp) -> i64 {
        if self.clock_ms != 0 {
            self.clock_ms.saturating_add(self.time_offset_ms)
        } else {
            now.as_nanos() / 1_000_000
        }
    }

    /// Synchronise the signing clock with the venue: fetch `GET /api/v3/time` and
    /// record `offset = serverTime − clock_ms`, applied to every subsequent signed
    /// request. Call it after `set_clock_ms` once per session (and on a `700003`
    /// timestamp rejection). Guards against host clock drift larger than the
    /// `recv_window`.
    ///
    /// # Errors
    /// Returns a [`MexcError`] if the request fails or the response lacks
    /// `serverTime`.
    pub fn sync_time(&mut self) -> Result<(), MexcError> {
        let req = HttpRequest {
            method: Method::Get,
            url: format!("{}/api/v3/time", self.base_url),
            api_key: None,
            body: None,
        };
        let resp = self.transport.send(&req)?;
        if !resp.is_success() {
            return Err(MexcError::Transport(format!("time HTTP {}", resp.status)));
        }
        let v: serde_json::Value =
            serde_json::from_str(&resp.body).map_err(|e| MexcError::Parse(e.to_string()))?;
        let server_ms = v
            .get("serverTime")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| MexcError::Parse("time response missing serverTime".to_owned()))?;
        self.time_offset_ms = server_ms - self.clock_ms;
        Ok(())
    }

    /// Override the modelled fee (basis points).
    #[must_use]
    pub fn with_fee_bps(mut self, fee_bps: i64) -> Self {
        self.fee_bps = fee_bps;
        self
    }

    /// Set the wall-clock millisecond timestamp used for request signing. The
    /// live driver updates this before each batch of calls.
    pub fn set_clock_ms(&mut self, ms: i64) {
        self.clock_ms = ms;
    }

    /// Number of orders currently awaiting fills.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn fee_costs(&self, instrument: InstrumentId, price: akadro_core::Price, qty: Qty) -> Costs {
        let mut costs = Costs::new();
        if self.fee_bps != 0
            && let Some(spec) = self.catalog.spec(instrument)
        {
            let notional = price.notional(qty);
            costs.push(Cost::new(
                spec.quote,
                notional.mul_bps(self.fee_bps),
                CostKind::Taker,
            ));
        }
        costs
    }

    /// Translate an akadro `OrderRequest` into MEXC `(type, optional price)`,
    /// or `None` if MEXC spot cannot express it (rejected locally).
    fn order_type(order: &OrderRequest) -> Option<(&'static str, Option<akadro_core::Price>)> {
        if order.reduce_only {
            return None; // spot has no reduce-only
        }
        // Post-only is maker-only: a limit becomes a `LIMIT_MAKER` (rejected by the
        // venue if it would cross), matching the backtest's post-only semantics. On
        // any non-limit kind post-only is a contradiction — reject locally (M2/m1).
        if order.post_only {
            return match order.kind {
                OrderKind::Limit { limit } => Some(("LIMIT_MAKER", Some(limit))),
                _ => None,
            };
        }
        match (order.kind, order.tif) {
            (OrderKind::Market, _) => Some(("MARKET", None)),
            (OrderKind::Limit { limit }, TimeInForce::Gtc) => Some(("LIMIT", Some(limit))),
            (OrderKind::Limit { limit }, TimeInForce::Ioc) => {
                Some(("IMMEDIATE_OR_CANCEL", Some(limit)))
            }
            (OrderKind::Limit { limit }, TimeInForce::Fok) => Some(("FILL_OR_KILL", Some(limit))),
            // Stop orders are unsupported on the spot order endpoint.
            _ => None,
        }
    }
}

impl<T: Transport> ExecutionClient for MexcSpotExec<T> {
    fn submit(
        &mut self,
        id: ClientOrderId,
        order: OrderRequest,
        now: Timestamp,
        sink: &mut dyn EventSink,
    ) {
        let reject = |sink: &mut dyn EventSink, reason| {
            sink.emit(AccountEvent::OrderRejected {
                id,
                reason,
                ts: now,
            });
        };

        let (Some(symbol), Some((price_scale, qty_scale))) = (
            self.catalog.symbol_of(order.instrument).map(str::to_owned),
            self.catalog.scales(order.instrument),
        ) else {
            reject(sink, RejectReason::InvalidOrder);
            return;
        };
        let Some((otype, price)) = Self::order_type(&order) else {
            reject(sink, RejectReason::InvalidOrder);
            return;
        };

        let side = if order.side == Side::Buy {
            "BUY"
        } else {
            "SELL"
        };
        let coid = format!("akadro-{}", id.raw());
        let mut params: Vec<(&str, String)> = vec![
            ("symbol", symbol),
            ("side", side.to_owned()),
            ("type", otype.to_owned()),
            ("quantity", raw_to_decimal(order.qty.raw(), qty_scale)),
            ("newClientOrderId", coid),
        ];
        if let Some(p) = price {
            params.push(("price", raw_to_decimal(p.raw(), price_scale)));
        }
        let pairs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let req = signed(
            Method::Post,
            "/api/v3/order",
            &pairs,
            &self.api_secret,
            self.sign_clock(now),
            self.recv_window,
        );
        let http = HttpRequest {
            method: Method::Post,
            url: format!("{}{}?{}", self.base_url, req.path, req.query),
            api_key: Some(self.api_key.clone()),
            body: None,
        };

        match self.transport.send(&http) {
            Ok(resp) => match parse_order_ack(&resp.body) {
                Ok(ack) => {
                    sink.emit(AccountEvent::OrderAccepted { id, ts: now });
                    self.pending.push(Pending {
                        client_id: id,
                        venue_order_id: ack.venue_order_id,
                        instrument: order.instrument,
                        side: order.side,
                        price_scale,
                        qty_scale,
                        order_qty: order.qty.raw(),
                        prev_filled: 0,
                        prev_quote: 0,
                    });
                }
                Err(MexcError::Api { code, .. }) => reject(sink, map_reject_code(code)),
                Err(_) => reject(sink, RejectReason::VenueRejected),
            },
            Err(_) => reject(sink, RejectReason::VenueRejected),
        }
    }

    fn observe(&mut self, event: &Event, now: Timestamp, sink: &mut dyn EventSink) {
        // Poll resting orders once per bar (a reasonable cadence for bar
        // strategies; lower-latency fills are a WebSocket enhancement).
        if !matches!(event, Event::Bar(_)) {
            return;
        }
        let mut still = Vec::with_capacity(self.pending.len());
        for mut p in std::mem::take(&mut self.pending) {
            let symbol = self
                .catalog
                .symbol_of(p.instrument)
                .unwrap_or_default()
                .to_owned();
            let pairs: [(&str, &str); 2] = [
                ("symbol", symbol.as_str()),
                ("orderId", p.venue_order_id.as_str()),
            ];
            let req = signed(
                Method::Get,
                "/api/v3/order",
                &pairs,
                &self.api_secret,
                self.sign_clock(now),
                self.recv_window,
            );
            let http = HttpRequest {
                method: Method::Get,
                url: format!("{}{}?{}", self.base_url, req.path, req.query),
                api_key: Some(self.api_key.clone()),
                body: None,
            };
            let parsed = self
                .transport
                .send(&http)
                .and_then(|r| parse_order_status(&r.body, p.price_scale, p.qty_scale));
            match parsed {
                Ok(status) => {
                    let filled = status.executed_qty.raw();
                    if filled > p.prev_filled {
                        let delta_qty = filled - p.prev_filled;
                        // MARGINAL tranche price: this fill's quote / this fill's qty,
                        // re-scaled by 10^qty_scale (same re-scale as the cumulative
                        // avg). Using the cumulative VWAP here would misprice every
                        // fill after the first when tranches differ (M6).
                        let cum_quote = status.cummulative_quote.raw();
                        let delta_quote = cum_quote - p.prev_quote;
                        let scaled = delta_quote.saturating_mul(10i128.saturating_pow(p.qty_scale));
                        let price = akadro_core::Price::from_raw(
                            i64::try_from(scaled / i128::from(delta_qty)).unwrap_or(i64::MAX),
                        );
                        let delta = Qty::from_raw(delta_qty);
                        let costs = self.fee_costs(p.instrument, price, delta);
                        sink.emit(AccountEvent::Fill {
                            client_order_id: p.client_id,
                            instrument: p.instrument,
                            side: p.side,
                            price,
                            qty: delta,
                            costs,
                            // Complete by QUANTITY, not the status string, so a full
                            // quantity reported under PARTIALLY_FILLED still closes
                            // the order (M5).
                            complete: filled >= p.order_qty,
                            ts: now,
                        });
                        p.prev_filled = filled;
                        p.prev_quote = cum_quote;
                    }
                    if status.is_terminal() {
                        match status.status.as_str() {
                            "CANCELED" | "PARTIALLY_CANCELED" => {
                                sink.emit(AccountEvent::OrderCanceled {
                                    id: p.client_id,
                                    reason: CancelReason::VenueCanceled,
                                    ts: now,
                                });
                            }
                            // An IOC/FOK remainder that the venue expired: surface a
                            // terminal lifecycle event so the order is closed and not
                            // re-polled forever (M4).
                            "EXPIRED" => sink.emit(AccountEvent::OrderExpired {
                                id: p.client_id,
                                ts: now,
                            }),
                            // FILLED: closed by the complete-by-quantity fill above.
                            _ => {}
                        }
                    } else {
                        still.push(p); // still working
                    }
                }
                // Transient error — keep the order and retry next bar.
                Err(_) => still.push(p),
            }
        }
        self.pending = still;
    }

    fn sync_clock(&mut self, wall_ms: i64) {
        // The live shell drives the signing clock through this seam (m4). Equivalent
        // to `set_clock_ms`, but reachable generically through the trait — including
        // when this client is wrapped by `ChannelExec`. `sync_time` still layers the
        // server offset on top via `time_offset_ms`.
        self.clock_ms = wall_ms;
    }
}

impl<T> fmt::Debug for MexcSpotExec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the API secret.
        f.debug_struct("MexcSpotExec")
            .field("base_url", &self.base_url)
            .field("fee_bps", &self.fee_bps)
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::MexcCatalog;
    use crate::transport::{HttpResponse, MockTransport};
    use akadro_core::{Money, Price, TimeInForce};

    const EXINFO: &str = r#"{"symbols":[{"symbol":"BTCUSDT","baseAsset":"BTC","quoteAsset":"USDT","baseAssetPrecision":2,"quoteAssetPrecision":2,"baseSizePrecision":"0.01","quoteAmountPrecision":"1","status":"1"}]}"#;
    const ACK: &str = r#"{"orderId":"V1","transactTime":1700000000000}"#;
    const I: InstrumentId = InstrumentId::new(0);

    fn exec(responses: Vec<HttpResponse>) -> MexcSpotExec<MockTransport> {
        let cat = MexcCatalog::from_exchange_info(EXINFO).unwrap();
        let mut x = MexcSpotExec::new(
            MockTransport::new(responses),
            "https://api.mexc.com",
            "key",
            b"secret".to_vec(),
            cat,
        );
        x.set_clock_ms(1_700_000_000_000);
        x
    }
    fn now() -> Timestamp {
        Timestamp::from_nanos(1)
    }

    #[test]
    fn sync_time_parses_server_time_and_errors_on_missing() {
        // serverTime present -> offset recorded, Ok.
        let mut x = exec(vec![HttpResponse::ok(r#"{"serverTime":1700000005000}"#)]);
        x.sync_time().unwrap();
        // serverTime missing -> parse error.
        let mut y = exec(vec![HttpResponse::ok(r#"{"nope":1}"#)]);
        assert!(y.sync_time().is_err());
        // non-2xx -> transport error.
        let mut z = exec(vec![HttpResponse::error(503, "down")]);
        assert!(z.sync_time().is_err());
    }

    fn fills(s: &[AccountEvent]) -> Vec<(Price, Qty)> {
        s.iter()
            .filter_map(|e| match e {
                AccountEvent::Fill { price, qty, .. } => Some((*price, *qty)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sync_clock_sets_the_signing_timestamp() {
        // m4: the ExecutionClient::sync_clock seam drives the signing clock (reachable
        // generically, e.g. through ChannelExec), so a submitted order signs with it.
        use akadro_core::ExecutionClient as _;
        let mut x = exec(vec![HttpResponse::ok(ACK)]);
        x.sync_clock(1_700_000_009_000);
        let mut s: Vec<AccountEvent> = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(100)),
            now(),
            &mut s,
        );
        let url = x.transport.last_url().unwrap();
        assert!(
            url.contains("timestamp=1700000009000"),
            "order signs with the sync_clock-set timestamp, url={url}"
        );
    }

    #[test]
    fn submit_limit_includes_price_and_type() {
        let mut x = exec(vec![HttpResponse::ok(ACK)]);
        let mut s: Vec<AccountEvent> = Vec::new();
        let o = OrderRequest::limit(I, Side::Buy, Qty::from_raw(100), Price::from_raw(10_000))
            .with_tif(TimeInForce::Ioc);
        x.submit(ClientOrderId::new(0), o, now(), &mut s);
        assert!(matches!(s[0], AccountEvent::OrderAccepted { .. }));
        let url = x.transport.last_url().unwrap();
        assert!(url.contains("type=IMMEDIATE_OR_CANCEL"), "url={url}");
        assert!(url.contains("price="), "url={url}");
        assert_eq!(x.pending_count(), 1);
    }

    #[test]
    fn submit_unknown_instrument_rejected_without_call() {
        let mut x = exec(vec![]);
        let mut s = Vec::new();
        let o = OrderRequest::market(InstrumentId::new(9), Side::Buy, Qty::from_raw(1));
        x.submit(ClientOrderId::new(0), o, now(), &mut s);
        assert!(matches!(
            s[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::InvalidOrder,
                ..
            }
        ));
        assert_eq!(x.transport.sent_count(), 0);
    }

    #[test]
    fn submit_transport_error_is_venue_rejected() {
        let mut x = exec(vec![]); // no canned response -> transport errors
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(100)),
            now(),
            &mut s,
        );
        assert!(matches!(
            s[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::VenueRejected,
                ..
            }
        ));
    }

    #[test]
    fn submit_api_error_maps_reason() {
        let mut x = exec(vec![HttpResponse::ok(
            r#"{"code":30005,"msg":"insufficient"}"#,
        )]);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(100)),
            now(),
            &mut s,
        );
        assert!(matches!(
            s[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::InsufficientFunds,
                ..
            }
        ));
    }

    #[test]
    fn observe_partial_then_full_fill() {
        // ack, then poll: partial (0.50 filled), then full (1.00 filled).
        let partial =
            r#"{"status":"PARTIALLY_FILLED","executedQty":"0.50","cummulativeQuoteQty":"52.50"}"#;
        let full = r#"{"status":"FILLED","executedQty":"1.00","cummulativeQuoteQty":"105.00"}"#;
        let mut x = exec(vec![
            HttpResponse::ok(ACK),
            HttpResponse::ok(partial),
            HttpResponse::ok(full),
        ]);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(100)),
            now(),
            &mut s,
        );
        s.clear();
        let bar = Event::Bar(akadro_core::Bar::new(
            I,
            now(),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Qty::from_raw(1),
        ));
        // 52.50 quote / 0.50 base = 105.00 price; raw = 5250 * 10^2 / 50 = 10_500.
        x.observe(&bar, now(), &mut s); // partial -> fill 50 @ 105.00
        assert_eq!(
            fills(&s),
            vec![(Price::from_raw(10_500), Qty::from_raw(50))]
        );
        assert_eq!(x.pending_count(), 1); // still working
        s.clear();
        x.observe(&bar, now(), &mut s); // full -> fill remaining 50 @ 105.00
        assert_eq!(
            fills(&s),
            vec![(Price::from_raw(10_500), Qty::from_raw(50))]
        );
        assert_eq!(x.pending_count(), 0); // terminal
    }

    #[test]
    fn observe_canceled_status_emits_cancel() {
        let canceled = r#"{"status":"CANCELED","executedQty":"0","cummulativeQuoteQty":"0"}"#;
        let mut x = exec(vec![HttpResponse::ok(ACK), HttpResponse::ok(canceled)]);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(100)),
            now(),
            &mut s,
        );
        s.clear();
        let bar = Event::Bar(akadro_core::Bar::new(
            I,
            now(),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Qty::from_raw(1),
        ));
        x.observe(&bar, now(), &mut s);
        assert!(
            s.iter()
                .any(|e| matches!(e, AccountEvent::OrderCanceled { .. }))
        );
        assert_eq!(x.pending_count(), 0);
    }

    #[test]
    fn observe_transport_error_keeps_pending() {
        let mut x = exec(vec![HttpResponse::ok(ACK)]); // ack only; poll will error
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(100)),
            now(),
            &mut s,
        );
        s.clear();
        let bar = Event::Bar(akadro_core::Bar::new(
            I,
            now(),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Qty::from_raw(1),
        ));
        x.observe(&bar, now(), &mut s); // poll errors -> keep pending
        assert!(fills(&s).is_empty());
        assert_eq!(x.pending_count(), 1);
    }

    #[test]
    fn observe_ignores_non_bar() {
        let mut x = exec(vec![]);
        let mut s = Vec::new();
        x.observe(
            &Event::Resync {
                instrument: None,
                ts: now(),
            },
            now(),
            &mut s,
        );
        assert!(s.is_empty());
    }

    #[test]
    fn fee_uses_configured_bps_and_debug_hides_secret() {
        let x = exec(vec![]).with_fee_bps(10);
        let costs = x.fee_costs(I, Price::from_raw(100), Qty::from_raw(10));
        assert_eq!(costs[0].amount, Money::from_raw(1)); // 100*10=1000 * 10bps = 1
        let dbg = format!("{x:?}");
        assert!(dbg.contains("MexcSpotExec"));
        assert!(!dbg.contains("secret"));
    }
}

#[cfg(test)]
mod feed_cov {
    use super::*;
    use crate::transport::{HttpResponse, MockTransport};

    const KLINES: &str =
        r#"[[1700000000000,"100.00","101.00","99.00","100.50","10.00",1700000059999,"1005"]]"#;

    #[test]
    fn backfill_paginates_full_range() {
        // Page size 2: a full first page (2 bars) then a short page (1) ends it.
        let p1 = r#"[[1700000000000,"1","1","1","1","1",1700000060000,"1"],
                     [1700000060000,"1","1","1","1","1",1700000120000,"1"]]"#;
        let p2 = r#"[[1700000120000,"1","1","1","1","1",1700000180000,"1"]]"#;
        // p1, p2, then an empty page (past the data) terminates the back-fill.
        let t = MockTransport::new(vec![
            HttpResponse::ok(p1),
            HttpResponse::ok(p2),
            HttpResponse::ok("[]"),
        ]);
        let mut feed = MexcKlineFeed::new(
            t,
            "https://api.mexc.com",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            2,
            2,
        )
        .with_range(1_700_000_000_000, 1_700_000_600_000)
        .with_limit(2);
        let n = core::iter::from_fn(|| feed.next_event()).count();
        assert_eq!(n, 3); // paginated across both pages
    }

    #[test]
    fn backfill_dedups_overlapping_pages() {
        // The cursor advances to a page's last close, which the next page may include
        // again (boundary overlap). The back-fill must dedup by close-stamp so the
        // assembled series has no duplicate bars (matching the other venue connectors).
        let p1 = r#"[[1700000000000,"1","1","1","1","1",1700000060000,"1"],
                     [1700000060000,"1","1","1","1","1",1700000120000,"1"]]"#;
        // p2 repeats the bar closing at 1700000120000, then adds a new one (180000).
        let p2 = r#"[[1700000060000,"1","1","1","1","1",1700000120000,"1"],
                     [1700000120000,"1","1","1","1","1",1700000180000,"1"]]"#;
        let t = MockTransport::new(vec![
            HttpResponse::ok(p1),
            HttpResponse::ok(p2),
            HttpResponse::ok("[]"),
        ]);
        let mut feed = MexcKlineFeed::new(
            t,
            "https://api.mexc.com",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            2,
            2,
        )
        .with_range(1_700_000_000_000, 1_700_000_600_000)
        .with_limit(2);
        let closes: Vec<i64> = core::iter::from_fn(|| feed.next_event())
            .filter_map(|e| match e {
                Event::Bar(b) => Some(b.ts.as_nanos()),
                _ => None,
            })
            .collect();
        assert_eq!(
            closes.len(),
            3,
            "4 bars fetched across overlapping pages dedup to 3 unique close-stamps"
        );
        let mut uniq = closes.clone();
        uniq.dedup();
        assert_eq!(
            uniq, closes,
            "no duplicate close-stamps remain (ascending, deduped)"
        );
    }

    #[test]
    fn with_progress_reports_each_page() {
        use std::cell::RefCell;
        use std::rc::Rc;
        // Same 2-page setup as backfill_paginates_full_range; assert the callback
        // fires once per non-empty page with monotonically rising counts.
        let p1 = r#"[[1700000000000,"1","1","1","1","1",1700000060000,"1"],
                     [1700000060000,"1","1","1","1","1",1700000120000,"1"]]"#;
        let p2 = r#"[[1700000120000,"1","1","1","1","1",1700000180000,"1"]]"#;
        let t = MockTransport::new(vec![
            HttpResponse::ok(p1),
            HttpResponse::ok(p2),
            HttpResponse::ok("[]"),
        ]);
        let seen: Rc<RefCell<Vec<akadro_core::BackfillProgress>>> =
            Rc::new(RefCell::new(Vec::new()));
        let sink = seen.clone();
        let mut feed = MexcKlineFeed::new(
            t,
            "https://api.mexc.com",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            2,
            2,
        )
        .with_range(1_700_000_000_000, 1_700_000_600_000)
        .with_limit(2)
        .with_progress(move |p| sink.borrow_mut().push(p));
        let _ = core::iter::from_fn(|| feed.next_event()).count();
        let s = seen.borrow();
        assert_eq!(s.len(), 2, "one callback per non-empty page");
        assert_eq!((s[0].pages, s[0].bars), (1, 2));
        assert_eq!((s[1].pages, s[1].bars), (2, 3)); // cumulative bars
        assert!(s[1].frontier_ms > s[0].frontier_ms, "frontier advances");
    }

    #[test]
    fn with_page_sink_receives_each_page() {
        use std::cell::RefCell;
        use std::rc::Rc;
        // A test PageSink recording the size of each page handed to it. Same 2-page
        // backfill; assert the connector journals each page through the sink.
        #[derive(Default)]
        struct RecSink(Rc<RefCell<Vec<usize>>>);
        impl akadro_core::PageSink for RecSink {
            fn on_page(&mut self, page: &[akadro_core::Bar]) {
                self.0.borrow_mut().push(page.len());
            }
        }
        let p1 = r#"[[1700000000000,"1","1","1","1","1",1700000060000,"1"],
                     [1700000060000,"1","1","1","1","1",1700000120000,"1"]]"#;
        let p2 = r#"[[1700000120000,"1","1","1","1","1",1700000180000,"1"]]"#;
        let t = MockTransport::new(vec![
            HttpResponse::ok(p1),
            HttpResponse::ok(p2),
            HttpResponse::ok("[]"),
        ]);
        let pages: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));
        let mut feed = MexcKlineFeed::new(
            t,
            "https://api.mexc.com",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            2,
            2,
        )
        .with_range(1_700_000_000_000, 1_700_000_600_000)
        .with_limit(2)
        .with_page_sink(Box::new(RecSink(pages.clone())));
        let _ = core::iter::from_fn(|| feed.next_event()).count();
        assert_eq!(
            *pages.borrow(),
            vec![2, 1],
            "each non-empty page is journaled"
        );
    }

    #[test]
    fn backfill_skips_leading_gap() {
        // An empty first page (start older than the venue keeps) is skipped, then
        // the data page is fetched.
        let data = r#"[[1700000000000,"1","1","1","1","1",1700000060000,"1"]]"#;
        let t = MockTransport::new(vec![
            HttpResponse::ok("[]"),
            HttpResponse::ok(data),
            HttpResponse::ok("[]"),
        ]);
        let mut feed = MexcKlineFeed::new(
            t,
            "https://api.mexc.com",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            2,
            2,
        )
        .with_range(1_700_000_000_000 - 120_000, 1_700_000_600_000)
        .with_limit(2);
        let n = core::iter::from_fn(|| feed.next_event()).count();
        assert_eq!(n, 1);
    }

    #[test]
    fn catalog_fetch_issues_exchange_info_request() {
        let info = r#"{"symbols":[{"symbol":"BTCUSDT","baseAsset":"BTC","quoteAsset":"USDT",
            "baseAssetPrecision":8,"quoteAssetPrecision":2,"status":"1"}]}"#;
        let mut t = MockTransport::new(vec![HttpResponse::ok(info)]);
        let cat = MexcCatalog::fetch(&mut t, "https://api.mexc.com", "BTCUSDT").unwrap();
        assert_eq!(cat.id_of("BTCUSDT"), Some(InstrumentId::new(0)));
        assert_eq!(cat.scales(InstrumentId::new(0)), Some((2, 8)));
        assert!(
            t.last_url()
                .unwrap()
                .contains("exchangeInfo?symbol=BTCUSDT")
        );
    }

    #[test]
    fn catalog_fetch_surfaces_http_error() {
        let mut t = MockTransport::new(vec![HttpResponse::error(500, "nope")]);
        assert!(MexcCatalog::fetch(&mut t, "https://api.mexc.com", "BTCUSDT").is_err());
    }

    #[test]
    fn interval_ms_maps_known_intervals() {
        let mk = |iv: &str| {
            MexcKlineFeed::new(
                MockTransport::default(),
                "u",
                "S",
                InstrumentId::new(0),
                iv,
                0,
                0,
            )
            .interval_ms()
        };
        assert_eq!(mk("1m"), 60_000);
        assert_eq!(mk("5m"), 300_000);
        assert_eq!(mk("15m"), 900_000);
        assert_eq!(mk("30m"), 1_800_000);
        assert_eq!(mk("60m"), 3_600_000);
        assert_eq!(mk("1h"), 3_600_000);
        assert_eq!(mk("4h"), 14_400_000);
        assert_eq!(mk("1d"), 86_400_000);
        assert_eq!(mk("1W"), 7 * 86_400_000);
        assert_eq!(mk("1M"), 30 * 86_400_000);
        assert_eq!(mk("bogus"), 0);
    }

    #[test]
    fn with_limit_clamps_to_max() {
        let f = MexcKlineFeed::new(
            MockTransport::default(),
            "u",
            "S",
            InstrumentId::new(0),
            "1m",
            0,
            0,
        )
        .with_limit(5000); // above MEXC's 1000 hard cap
        assert_eq!(f.limit, MAX_KLINES_LIMIT);
    }

    #[test]
    fn backfill_stops_on_no_forward_progress() {
        // A degenerate page whose last close is <= the cursor breaks the loop
        // rather than spinning.
        let p = r#"[[1699999940000,"1","1","1","1","1",1700000000000,"1"]]"#; // close == start
        let t = MockTransport::new(vec![HttpResponse::ok(p)]);
        let mut feed = MexcKlineFeed::new(t, "u", "BTCUSDT", InstrumentId::new(0), "1m", 2, 2)
            .with_range(1_700_000_000_000, 1_700_000_600_000);
        let n = core::iter::from_fn(|| feed.next_event()).count();
        assert_eq!(n, 1);
    }

    #[test]
    fn kline_feed_builders_and_replay() {
        let t = MockTransport::new(vec![HttpResponse::ok(KLINES), HttpResponse::ok("[]")]);
        let mut feed = MexcKlineFeed::new(
            t,
            "https://api.mexc.com",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            2,
            2,
        )
        .with_range(1_700_000_000_000, 1_700_000_060_000)
        .with_limit(100);
        assert!(format!("{feed:?}").contains("MexcKlineFeed"));
        assert!(matches!(feed.next_event(), Some(Event::Bar(_))));
        assert!(feed.next_event().is_none());
    }

    #[test]
    fn kline_feed_http_error_yields_none() {
        let t = MockTransport::new(vec![HttpResponse::error(500, "boom")]);
        let mut feed = MexcKlineFeed::new(
            t,
            "https://api.mexc.com",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            2,
            2,
        );
        assert!(feed.next_event().is_none());
    }

    #[test]
    fn kline_feed_malformed_body_yields_none() {
        // 200 OK but an unparseable klines payload -> the `?` in fetch surfaces a
        // parse error, so next_event yields None rather than panicking.
        let t = MockTransport::new(vec![HttpResponse::ok("[[1,2,3]]")]);
        let mut feed = MexcKlineFeed::new(
            t,
            "https://api.mexc.com",
            "BTCUSDT",
            InstrumentId::new(0),
            "1m",
            2,
            2,
        );
        assert!(feed.next_event().is_none());
    }
}

#[cfg(test)]
mod submit_cov {
    use super::*;
    use crate::instrument::MexcCatalog;
    use crate::transport::{HttpResponse, MockTransport};
    use akadro_core::{Price, TimeInForce, TriggerBy};

    const EXINFO: &str = r#"{"symbols":[{"symbol":"BTCUSDT","baseAsset":"BTC","quoteAsset":"USDT","baseAssetPrecision":2,"quoteAssetPrecision":2,"baseSizePrecision":"0.01","quoteAmountPrecision":"1","status":"1"}]}"#;
    const ACK: &str = r#"{"orderId":"V1","transactTime":1700000000000}"#;
    const I: InstrumentId = InstrumentId::new(0);

    fn exec(responses: Vec<HttpResponse>) -> MexcSpotExec<MockTransport> {
        let cat = MexcCatalog::from_exchange_info(EXINFO).unwrap();
        let mut x = MexcSpotExec::new(
            MockTransport::new(responses),
            "https://api.mexc.com",
            "key",
            b"secret".to_vec(),
            cat,
        );
        x.set_clock_ms(1_700_000_000_000);
        x
    }
    fn now() -> Timestamp {
        Timestamp::from_nanos(1)
    }

    #[test]
    fn gtc_limit_maps_to_plain_limit() {
        let mut x = exec(vec![HttpResponse::ok(ACK)]);
        let mut s = Vec::new();
        let o = OrderRequest::limit(I, Side::Buy, Qty::from_raw(100), Price::from_raw(10_000));
        x.submit(ClientOrderId::new(0), o, now(), &mut s);
        assert!(x.transport.last_url().unwrap().contains("type=LIMIT"));
    }

    #[test]
    fn fok_limit_maps_to_fill_or_kill() {
        let mut x = exec(vec![HttpResponse::ok(ACK)]);
        let mut s = Vec::new();
        let o = OrderRequest::limit(I, Side::Buy, Qty::from_raw(100), Price::from_raw(10_000))
            .with_tif(TimeInForce::Fok);
        x.submit(ClientOrderId::new(0), o, now(), &mut s);
        assert!(
            x.transport
                .last_url()
                .unwrap()
                .contains("type=FILL_OR_KILL")
        );
    }

    #[test]
    fn sell_side_is_encoded() {
        let mut x = exec(vec![HttpResponse::ok(ACK)]);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Sell, Qty::from_raw(100)),
            now(),
            &mut s,
        );
        assert!(x.transport.last_url().unwrap().contains("side=SELL"));
    }

    #[test]
    fn stop_order_unsupported_on_spot_is_rejected_locally() {
        let mut x = exec(vec![]); // no transport call expected
        let mut s = Vec::new();
        let stop = OrderRequest::stop(
            I,
            Side::Sell,
            Qty::from_raw(1),
            Price::from_raw(9_000),
            TriggerBy::Last,
        );
        x.submit(ClientOrderId::new(0), stop, now(), &mut s);
        assert!(matches!(
            s[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::InvalidOrder,
                ..
            }
        ));
        assert_eq!(x.transport.sent_count(), 0);
    }

    #[test]
    fn unparseable_ack_body_is_venue_rejected() {
        // 200 OK but neither a valid ack nor a structured API error -> the
        // non-Api parse error maps to a conservative VenueRejected.
        let mut x = exec(vec![HttpResponse::ok("not json at all")]);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(100)),
            now(),
            &mut s,
        );
        assert!(matches!(
            s[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::VenueRejected,
                ..
            }
        ));
    }
}
