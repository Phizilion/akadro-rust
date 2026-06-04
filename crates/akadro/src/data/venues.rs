// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one-call venue dispatch: turn *(venue, symbol, interval, window)* into
//! engine-ready bars (and, for perpetuals, the funding schedule) with the range-aware
//! resumable cache doing all the plumbing.
//!
//! ```no_run
//! # #[cfg(feature = "venues")] {
//! use akadro::data::{CacheOptions, DataRequest, Venue, load};
//! let req = DataRequest::new(Venue::Mexc, "BTCUSDT", "1m", 1_700_000_000_000, 1_700_086_400_000);
//! let market = load(&req, &CacheOptions::default(), std::path::Path::new("market_data")).unwrap();
//! println!("{} bars", market.bars.len());
//! # }
//! ```
//!
//! [`load`] gap-fills one series; [`load_perp`] also fetches a perpetual's mandatory
//! funding schedule; [`load_aggregated`] satisfies a coarse interval by aggregating a
//! cached finer one; [`load_many`] gap-fills many series concurrently (rate-limited).
//!
//! This is an **inherently live/network path**: it fetches the venue catalogue to
//! resolve the instrument's fixed-point scales, then gap-fills the bars. It is the
//! convenience layer over the fully-offline-testable cache engine
//! ([`load_bars`](akadro_data::load_bars) etc.); the catalogue fetch + first download
//! need connectivity, so this layer is exercised by the playground against the real
//! venues, not the unit suite. `now_ms` for the cache's settled-gap cutoff is read
//! from the system clock here — the one wall-clock read, in this IO layer, never
//! reaching the engine.
//!
//! A caller always speaks the akadro interval vocabulary (`"1m"`, `"5m"`, `"1h"`, …);
//! each venue's native token (Bybit `"1"`/`"60"`, KuCoin `"1min"`/`"1hour"`) is mapped
//! internally, while the cache series stays keyed by the akadro interval.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use akadro_core::{
    AssetId, Bar, DataSource, InstrumentId, InstrumentSpec, PageSink, Timestamp,
    infer_funding_period_ms,
};
use akadro_data::{
    CacheOptions, DataError, RateLimiter, SeriesKey, SeriesRequest, default_req_per_sec,
    interval_to_nanos, load_bars, load_bars_aggregated, load_many as cache_load_many,
    load_or_cache_funding,
};

/// A trading venue for the one-call data API. Each carries a sensible default base
/// URL (override per-request with [`DataRequest::with_base_url`]).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Venue {
    /// MEXC spot (`api.mexc.com`).
    Mexc,
    /// MEXC perpetual futures contracts (`contract.mexc.com`); a perpetual — use [`load_perp`].
    MexcFutures,
    /// OKX spot or perpetual swap (`www.okx.com`); a `…-SWAP` symbol is a perpetual.
    Okx,
    /// Binance spot (`api.binance.com`).
    Binance,
    /// Binance USDⓈ-M perpetual futures (`fapi.binance.com`); a perpetual — use [`load_perp`].
    BinanceFutures,
    /// Bybit spot (`api.bybit.com`, category `spot`).
    Bybit,
    /// Bybit USDT linear perpetual (`api.bybit.com`, category `linear`); a perpetual.
    BybitPerp,
    /// KuCoin spot (`api.kucoin.com`).
    Kucoin,
}

impl Venue {
    /// Short stable name used as the cache series' venue key.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Venue::Mexc => "mexc",
            Venue::MexcFutures => "mexc-futures",
            Venue::Okx => "okx",
            Venue::Binance => "binance",
            Venue::BinanceFutures => "binance-futures",
            Venue::Bybit => "bybit",
            Venue::BybitPerp => "bybit-perp",
            Venue::Kucoin => "kucoin",
        }
    }

    /// The default REST base URL for this venue.
    #[must_use]
    pub fn default_base_url(self) -> &'static str {
        match self {
            Venue::Mexc => "https://api.mexc.com",
            Venue::MexcFutures => akadro_venue_mexc::FUTURES_BASE_URL,
            Venue::Okx => "https://www.okx.com",
            Venue::Binance => "https://api.binance.com",
            Venue::BinanceFutures => akadro_venue_binance::FUTURES_BASE_URL,
            Venue::Bybit | Venue::BybitPerp => "https://api.bybit.com",
            Venue::Kucoin => "https://api.kucoin.com",
        }
    }

    /// `true` if this venue/symbol is a perpetual (which MUST have funding).
    #[must_use]
    pub fn is_perp(self, symbol: &str) -> bool {
        matches!(
            self,
            Venue::MexcFutures | Venue::BinanceFutures | Venue::BybitPerp
        ) || (matches!(self, Venue::Okx) && symbol.ends_with("-SWAP"))
    }

    /// Map an akadro interval (`"1m"`, `"1h"`, …) to this venue's **exact** native
    /// kline token. Each venue speaks a different dialect — MEXC spot wants `"60m"`
    /// (not `"1h"`), OKX wants uppercase `"1H"`/`"1D"`, Bybit wants minute-numbers
    /// `"60"`/`"D"`, KuCoin wants `"1hour"`. The cache stays keyed by the akadro
    /// interval; only the connector sees the native token.
    fn native_interval(self, iv: &str) -> Result<String, MarketError> {
        let token: &str = match self {
            // MEXC spot: 1m/5m/15m/30m/60m/4h/1d/1W/1M (hour is `60m`, week is `1W`).
            Venue::Mexc => match iv {
                "1m" | "5m" | "15m" | "30m" | "4h" | "1d" => iv,
                "1h" | "60m" => "60m",
                "1w" => "1W",
                _ => return Err(self.bad_interval(iv)),
            },
            // MEXC futures: the connector maps the akadro form internally (it accepts
            // `1h`), but wants the uppercase `1W` for week.
            Venue::MexcFutures => match iv {
                "1m" | "5m" | "15m" | "30m" | "1h" | "4h" | "8h" | "1d" => iv,
                "1w" => "1W",
                _ => return Err(self.bad_interval(iv)),
            },
            // Binance: accepts the akadro lowercase form directly.
            Venue::Binance | Venue::BinanceFutures => match iv {
                "1m" | "3m" | "5m" | "15m" | "30m" | "1h" | "2h" | "4h" | "6h" | "8h" | "12h"
                | "1d" | "3d" | "1w" => iv,
                _ => return Err(self.bad_interval(iv)),
            },
            // OKX: minutes/seconds lowercase, hours/days/weeks UPPERCASE.
            Venue::Okx => match iv {
                "1s" | "1m" | "3m" | "5m" | "15m" | "30m" => iv,
                "1h" => "1H",
                "2h" => "2H",
                "4h" => "4H",
                "6h" => "6H",
                "12h" => "12H",
                "1d" => "1D",
                "1w" => "1W",
                _ => return Err(self.bad_interval(iv)),
            },
            // Bybit: minutes-as-number, or D/W.
            Venue::Bybit | Venue::BybitPerp => match iv {
                "1m" => "1",
                "3m" => "3",
                "5m" => "5",
                "15m" => "15",
                "30m" => "30",
                "1h" | "60m" => "60",
                "2h" => "120",
                "4h" => "240",
                "1d" => "D",
                "1w" => "W",
                _ => return Err(self.bad_interval(iv)),
            },
            // KuCoin: `<n>min` / `<n>hour` / `<n>day` / `<n>week`.
            Venue::Kucoin => match iv {
                "1m" => "1min",
                "5m" => "5min",
                "15m" => "15min",
                "30m" => "30min",
                "1h" => "1hour",
                "4h" => "4hour",
                "1d" => "1day",
                "1w" => "1week",
                _ => return Err(self.bad_interval(iv)),
            },
        };
        Ok(token.to_string())
    }

    fn bad_interval(self, iv: &str) -> MarketError {
        MarketError::Venue {
            venue: self.key(),
            message: format!("interval {iv:?} is not supported on this venue"),
        }
    }
}

/// A request for one series over `[start_ms, end_ms)` (epoch ms).
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct DataRequest {
    /// The venue.
    pub venue: Venue,
    /// Venue symbol (e.g. `"BTCUSDT"`, `"BTC_USDT"`, `"BTC-USDT-SWAP"`).
    pub symbol: String,
    /// Bar interval in akadro vocabulary (`"1m"`, `"5m"`, `"1h"`, …).
    pub interval: String,
    /// Window start, epoch ms (inclusive).
    pub start_ms: i64,
    /// Window end, epoch ms (exclusive).
    pub end_ms: i64,
    /// Override the venue's default base URL (e.g. a testnet); `None` uses the default.
    pub base_url: Option<String>,
}

impl DataRequest {
    /// Construct a request using the venue's default base URL.
    #[must_use]
    pub fn new(
        venue: Venue,
        symbol: impl Into<String>,
        interval: impl Into<String>,
        start_ms: i64,
        end_ms: i64,
    ) -> Self {
        Self {
            venue,
            symbol: symbol.into(),
            interval: interval.into(),
            start_ms,
            end_ms,
            base_url: None,
        }
    }

    /// Override the base URL (testnet / region endpoint).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    fn base(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| self.venue.default_base_url().to_owned())
    }
}

/// Engine-ready result of a [`load`]: the instrument specs (the engine's catalogue),
/// the resolved instrument id + its fixed-point scales, and the bars (downloaded once
/// and replayed from the range-aware cache thereafter).
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct LoadedMarket {
    /// All instrument specs from the venue catalogue (the engine catalogue).
    pub specs: Vec<InstrumentSpec>,
    /// The resolved instrument for `symbol`.
    pub instrument: InstrumentId,
    /// Price fixed-point scale.
    pub price_scale: u32,
    /// Quantity fixed-point scale.
    pub qty_scale: u32,
    /// The bars over the requested window.
    pub bars: Vec<Bar>,
}

/// A perpetual's [`LoadedMarket`] plus its funding schedule (mandatory for a perp —
/// it materially moves `PnL`), ready for `SimulatedExchange::with_funding_schedule`.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct PerpMarket {
    /// Bars + specs + instrument + scales.
    pub market: LoadedMarket,
    /// `(settlement_time, rate)` pairs at `FUNDING_RATE_SCALE`.
    pub funding_schedule: Vec<(Timestamp, i64)>,
    /// Funding settlement period expressed in bars (for `with_funding_schedule`).
    pub funding_interval_bars: u32,
}

/// Errors from the one-call venue dispatch.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum MarketError {
    /// A cache read/write (or gap-fill) failure.
    #[error(transparent)]
    Cache(#[from] DataError),
    /// A venue catalogue / network / parse failure (wrapped, not type-leaked).
    #[error("{venue} venue error: {message}")]
    Venue {
        /// The venue key.
        venue: &'static str,
        /// The wrapped error message.
        message: String,
    },
    /// The symbol is not present / not tradable on the venue.
    #[error("symbol {symbol:?} not found or not tradable on {venue}")]
    UnknownSymbol {
        /// The venue key.
        venue: &'static str,
        /// The requested symbol.
        symbol: String,
    },
}

/// Wall-clock now in epoch ms — the cache's settled-gap cutoff (IO layer; never the
/// engine clock).
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Map any `Display` venue error into a [`MarketError::Venue`] for `venue`.
fn venue_msg<E: std::fmt::Display>(venue: &'static str) -> impl Fn(E) -> MarketError {
    move |e| MarketError::Venue {
        venue,
        message: e.to_string(),
    }
}

fn unknown_symbol(venue: &'static str, symbol: &str) -> MarketError {
    MarketError::UnknownSymbol {
        venue,
        symbol: symbol.to_owned(),
    }
}

fn no_scales(venue: &'static str) -> MarketError {
    MarketError::Venue {
        venue,
        message: "catalogue has no scales for the symbol".to_owned(),
    }
}

/// Filename-safe rendering (non-alphanumerics → `_`).
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Collapse a multi-instrument catalogue to the single traded instrument re-tagged to
/// id 0. A perp load MUST do this: the `SimulatedExchange` requires funding for *every*
/// `PerpetualFuture` spec it sees, so handing it a venue's whole perp catalogue would
/// demand funding for all of them. The feed must tag its bars with the same new id.
fn single_instrument(
    specs: &[InstrumentSpec],
    instrument: InstrumentId,
    venue: &'static str,
) -> Result<(Vec<InstrumentSpec>, InstrumentId), MarketError> {
    let mut spec =
        *specs
            .iter()
            .find(|s| s.id == instrument)
            .ok_or_else(|| MarketError::Venue {
                venue,
                message: "resolved instrument missing from catalogue".to_owned(),
            })?;
    spec.id = InstrumentId::new(0);
    Ok((vec![spec], InstrumentId::new(0)))
}

/// Catalogue resolution for one series: fetch the venue catalogue and return the
/// engine specs, the resolved instrument id, and its fixed-point scales. For a
/// perpetual the catalogue is collapsed to the single traded instrument (id 0).
fn resolve(
    req: &DataRequest,
) -> Result<(Vec<InstrumentSpec>, InstrumentId, u32, u32), MarketError> {
    let venue = req.venue.key();
    let base = req.base();
    match req.venue {
        Venue::Mexc => {
            use akadro_venue_mexc::{MexcCatalog, ReqwestTransport};
            let mut t = ReqwestTransport::new().map_err(venue_msg(venue))?;
            let cat = MexcCatalog::fetch(&mut t, &base, &req.symbol).map_err(venue_msg(venue))?;
            let id = cat
                .id_of(&req.symbol)
                .ok_or_else(|| unknown_symbol(venue, &req.symbol))?;
            let (ps, qs) = cat.scales(id).ok_or_else(|| no_scales(venue))?;
            Ok((cat.specs().to_vec(), id, ps, qs))
        }
        Venue::MexcFutures => {
            use akadro_venue_mexc::{self as mexc, ReqwestTransport};
            let mut t = ReqwestTransport::new().map_err(venue_msg(venue))?;
            let c = mexc::fetch_contracts(&mut t, &base)
                .map_err(venue_msg(venue))?
                .into_iter()
                .find(|c| c.symbol == req.symbol)
                .ok_or_else(|| unknown_symbol(venue, &req.symbol))?;
            let inst = InstrumentId::new(0);
            let spec = c.to_spec(inst, AssetId::new(0), AssetId::new(1));
            Ok((vec![spec], inst, c.price_scale, c.qty_scale))
        }
        Venue::Okx => {
            use akadro_venue_okx::{InstType, OkxCatalog, ReqwestTransport};
            let inst_type = if req.symbol.ends_with("-SWAP") {
                InstType::Swap
            } else {
                InstType::Spot
            };
            let mut t = ReqwestTransport::new().map_err(venue_msg(venue))?;
            let cat = OkxCatalog::fetch(&mut t, &base, inst_type).map_err(venue_msg(venue))?;
            let id = cat
                .id_of(&req.symbol)
                .ok_or_else(|| unknown_symbol(venue, &req.symbol))?;
            let (ps, qs) = cat.scales(id).ok_or_else(|| no_scales(venue))?;
            Ok((cat.specs().to_vec(), id, ps, qs))
        }
        Venue::Binance | Venue::BinanceFutures => {
            use akadro_venue_binance::{BinanceCatalog, ReqwestTransport};
            let futures = matches!(req.venue, Venue::BinanceFutures);
            let mut t = ReqwestTransport::new().map_err(venue_msg(venue))?;
            let cat = if futures {
                BinanceCatalog::fetch_futures(&mut t, &base)
            } else {
                BinanceCatalog::fetch(&mut t, &base)
            }
            .map_err(venue_msg(venue))?;
            let id = cat
                .id_of(&req.symbol)
                .ok_or_else(|| unknown_symbol(venue, &req.symbol))?;
            let (ps, qs) = cat.scales(id).ok_or_else(|| no_scales(venue))?;
            let (specs, inst) = if futures {
                single_instrument(cat.specs(), id, venue)?
            } else {
                (cat.specs().to_vec(), id)
            };
            Ok((specs, inst, ps, qs))
        }
        Venue::Bybit | Venue::BybitPerp => {
            use akadro_venue_bybit::{BybitCatalog, Category, ReqwestTransport};
            let category = if matches!(req.venue, Venue::BybitPerp) {
                Category::Linear
            } else {
                Category::Spot
            };
            let mut t = ReqwestTransport::new().map_err(venue_msg(venue))?;
            let cat = BybitCatalog::fetch(&mut t, &base, category).map_err(venue_msg(venue))?;
            let id = cat
                .id_of(&req.symbol)
                .ok_or_else(|| unknown_symbol(venue, &req.symbol))?;
            let (ps, qs) = cat.scales(id).ok_or_else(|| no_scales(venue))?;
            let (specs, inst) = if matches!(req.venue, Venue::BybitPerp) {
                single_instrument(cat.specs(), id, venue)?
            } else {
                (cat.specs().to_vec(), id)
            };
            Ok((specs, inst, ps, qs))
        }
        Venue::Kucoin => {
            use akadro_venue_kucoin::{KucoinCatalog, ReqwestTransport};
            let mut t = ReqwestTransport::new().map_err(venue_msg(venue))?;
            let cat = KucoinCatalog::fetch(&mut t, &base).map_err(venue_msg(venue))?;
            let id = cat
                .id_of(&req.symbol)
                .ok_or_else(|| unknown_symbol(venue, &req.symbol))?;
            let (ps, qs) = cat.scales(id).ok_or_else(|| no_scales(venue))?;
            Ok((cat.specs().to_vec(), id, ps, qs))
        }
    }
}

/// Build the connector feed for one gap, boxed so every venue yields the same type.
/// A fresh transport is created per call (gaps are few); the venue-native `iv` token
/// and venue-specific knobs (MEXC page limit, Binance futures path, Bybit category)
/// are applied here.
// Exhaustive per-venue match; each arm is a few lines but there are eight venues.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn make_feed(
    venue: Venue,
    base: String,
    symbol: String,
    instrument: InstrumentId,
    iv: String,
    ps: u32,
    qs: u32,
    lo: i64,
    hi: i64,
    sink: Box<dyn PageSink>,
) -> Box<dyn DataSource> {
    match venue {
        Venue::Mexc => {
            use akadro_venue_mexc::{MexcKlineFeed, ReqwestTransport};
            Box::new(
                MexcKlineFeed::new(
                    ReqwestTransport::new().expect("reqwest transport"),
                    base,
                    symbol,
                    instrument,
                    iv,
                    ps,
                    qs,
                )
                .with_range(lo, hi)
                .with_limit(1000)
                .with_page_sink(sink),
            )
        }
        Venue::MexcFutures => {
            use akadro_venue_mexc::{MexcFuturesKlineFeed, ReqwestTransport};
            Box::new(
                MexcFuturesKlineFeed::new(
                    ReqwestTransport::new().expect("reqwest transport"),
                    base,
                    symbol,
                    instrument,
                    iv,
                    ps,
                    qs,
                )
                .with_range(lo, hi)
                .with_page_sink(sink),
            )
        }
        Venue::Okx => {
            use akadro_venue_okx::{OkxCandleFeed, ReqwestTransport};
            Box::new(
                OkxCandleFeed::new(
                    ReqwestTransport::new().expect("reqwest transport"),
                    base,
                    symbol,
                    instrument,
                    iv,
                    ps,
                    qs,
                )
                .with_range(lo, hi)
                .with_page_sink(sink),
            )
        }
        Venue::Binance | Venue::BinanceFutures => {
            use akadro_venue_binance::{BinanceKlineFeed, ReqwestTransport};
            let feed = BinanceKlineFeed::new(
                ReqwestTransport::new().expect("reqwest transport"),
                base,
                symbol,
                instrument,
                iv,
                ps,
                qs,
            );
            let feed = if matches!(venue, Venue::BinanceFutures) {
                feed.for_futures()
            } else {
                feed
            };
            Box::new(feed.with_range(lo, hi).with_page_sink(sink))
        }
        Venue::Bybit | Venue::BybitPerp => {
            use akadro_venue_bybit::{BybitKlineFeed, Category, ReqwestTransport};
            let category = if matches!(venue, Venue::BybitPerp) {
                Category::Linear
            } else {
                Category::Spot
            };
            Box::new(
                BybitKlineFeed::new(
                    ReqwestTransport::new().expect("reqwest transport"),
                    base,
                    category,
                    symbol,
                    instrument,
                    iv,
                    ps,
                    qs,
                )
                .with_range(lo, hi)
                .with_page_sink(sink),
            )
        }
        Venue::Kucoin => {
            use akadro_venue_kucoin::{KucoinCandleFeed, ReqwestTransport};
            Box::new(
                KucoinCandleFeed::new(
                    ReqwestTransport::new().expect("reqwest transport"),
                    base,
                    symbol,
                    instrument,
                    iv,
                    ps,
                    qs,
                )
                .with_range(lo, hi)
                .with_page_sink(sink),
            )
        }
    }
}

/// Load bars for `req`, downloading only what isn't cached and replaying the rest.
///
/// For a perpetual (MEXC/Binance futures, Bybit linear, or an OKX `…-SWAP` symbol)
/// prefer [`load_perp`], which also fetches the mandatory funding schedule.
///
/// # Errors
/// [`MarketError`] if the venue catalogue fetch fails, the symbol/interval is unknown,
/// or the cache gap-fill fails.
pub fn load(
    req: &DataRequest,
    opts: &CacheOptions,
    cache_dir: &Path,
) -> Result<LoadedMarket, MarketError> {
    let (specs, instrument, ps, qs) = resolve(req)?;
    let iv = req.venue.native_interval(&req.interval)?;
    let (venue, base, symbol) = (req.venue, req.base(), req.symbol.clone());
    let key = SeriesKey::new(venue.key(), &req.symbol, &req.interval, instrument, ps, qs);
    let bars = load_bars(
        cache_dir,
        &key,
        req.start_ms,
        req.end_ms,
        now_ms(),
        opts,
        move |lo, hi, sink| {
            make_feed(
                venue,
                base.clone(),
                symbol.clone(),
                instrument,
                iv.clone(),
                ps,
                qs,
                lo,
                hi,
                sink,
            )
        },
    )?;
    Ok(LoadedMarket {
        specs,
        instrument,
        price_scale: ps,
        qty_scale: qs,
        bars,
    })
}

/// Load a **perpetual**'s bars *and* its funding schedule (cached like the bars).
///
/// # Errors
/// [`MarketError`] if `req` is not a perpetual venue/symbol, the catalogue/funding
/// fetch fails, the funding history is empty (a perp cannot run without it), or the
/// cache gap-fill fails.
pub fn load_perp(
    req: &DataRequest,
    opts: &CacheOptions,
    cache_dir: &Path,
) -> Result<PerpMarket, MarketError> {
    if !req.venue.is_perp(&req.symbol) {
        return Err(MarketError::Venue {
            venue: req.venue.key(),
            message: format!(
                "symbol {:?} is not a perpetual on this venue; use `load` for spot",
                req.symbol
            ),
        });
    }
    let market = load(req, opts, cache_dir)?;
    let bar_ns = interval_to_nanos(&req.interval).ok_or_else(|| MarketError::Venue {
        venue: req.venue.key(),
        message: format!("unrecognized interval {:?}", req.interval),
    })?;
    let bar_millis = (bar_ns / 1_000_000).max(1);

    let funding_path = cache_dir.join(format!(
        "{}_{}_funding.feather",
        sanitize(req.venue.key()),
        sanitize(&req.symbol)
    ));
    let schedule = load_or_cache_funding(&funding_path, market.instrument, || fetch_funding(req))?;
    if schedule.is_empty() {
        return Err(MarketError::Venue {
            venue: req.venue.key(),
            message: format!(
                "perpetual {} returned no funding history — a perp backtest cannot run \
                 without funding (it materially moves PnL)",
                req.symbol
            ),
        });
    }
    let period_ms = infer_funding_period_ms(&schedule);
    let funding_interval_bars = u32::try_from((period_ms / bar_millis).max(1)).unwrap_or(u32::MAX);
    Ok(PerpMarket {
        market,
        funding_schedule: schedule,
        funding_interval_bars,
    })
}

/// Load `req`'s (coarse) interval by aggregating a cached **finer** interval — the
/// multi-timeframe path. The finer series is gap-filled and rolled up to
/// `req.interval`; only what's missing at the finer granularity is downloaded.
/// `finer_interval` must evenly divide `req.interval` (e.g. `"1m"` → `"5m"`).
///
/// # Errors
/// [`MarketError`] if the intervals aren't aggregable, the catalogue fetch fails, or
/// the cache gap-fill fails.
pub fn load_aggregated(
    req: &DataRequest,
    finer_interval: &str,
    opts: &CacheOptions,
    cache_dir: &Path,
) -> Result<LoadedMarket, MarketError> {
    let coarse_ns = interval_to_nanos(&req.interval).ok_or_else(|| MarketError::Venue {
        venue: req.venue.key(),
        message: format!("unrecognized interval {:?}", req.interval),
    })?;
    let (specs, instrument, ps, qs) = resolve(req)?;
    let finer_iv = req.venue.native_interval(finer_interval)?;
    let (venue, base, symbol) = (req.venue, req.base(), req.symbol.clone());
    let finer_key = SeriesKey::new(venue.key(), &req.symbol, finer_interval, instrument, ps, qs);
    let bars = load_bars_aggregated(
        cache_dir,
        &finer_key,
        coarse_ns,
        req.start_ms,
        req.end_ms,
        now_ms(),
        opts,
        move |lo, hi, sink| {
            make_feed(
                venue,
                base.clone(),
                symbol.clone(),
                instrument,
                finer_iv.clone(),
                ps,
                qs,
                lo,
                hi,
                sink,
            )
        },
    )?;
    Ok(LoadedMarket {
        specs,
        instrument,
        price_scale: ps,
        qty_scale: qs,
        bars,
    })
}

/// Gap-fill **many** series concurrently (rate-limited per the first request's venue,
/// or `opts.rate_limit_per_sec`), up to `opts.concurrency` at a time. Returns one
/// result per input request, in input order; a failing series is its own `Err`.
///
/// Catalogues are resolved sequentially up front (a few network calls); only the bar
/// downloads run concurrently. Requests may mix venues/symbols/intervals.
///
/// # Errors
/// Per-request errors are returned in-slot; the call itself does not fail.
///
/// # Panics
/// Panics only on an internal invariant break (a resolved series key failing to match
/// itself, or a result slot left unfilled) — neither reachable from caller input.
#[must_use]
pub fn load_many(
    reqs: &[DataRequest],
    opts: &CacheOptions,
    cache_dir: &Path,
) -> Vec<Result<LoadedMarket, MarketError>> {
    // Resolve catalogues up front; remember each OK request's resolved params + its
    // original slot so results can be returned in input order.
    struct Resolved {
        req: DataRequest,
        specs: Vec<InstrumentSpec>,
        instrument: InstrumentId,
        ps: u32,
        qs: u32,
        iv: String,
    }
    let mut out: Vec<Option<Result<LoadedMarket, MarketError>>> =
        (0..reqs.len()).map(|_| None).collect();
    let mut resolved: Vec<(usize, Resolved)> = Vec::new();
    for (i, req) in reqs.iter().enumerate() {
        match resolve(req).and_then(|(specs, instrument, ps, qs)| {
            let iv = req.venue.native_interval(&req.interval)?;
            Ok(Resolved {
                req: req.clone(),
                specs,
                instrument,
                ps,
                qs,
                iv,
            })
        }) {
            Ok(r) => resolved.push((i, r)),
            Err(e) => out[i] = Some(Err(e)),
        }
    }
    if resolved.is_empty() {
        return out.into_iter().map(|o| o.expect("all errored")).collect();
    }

    let series: Vec<SeriesRequest> = resolved
        .iter()
        .map(|(_, r)| {
            SeriesRequest::new(
                SeriesKey::new(
                    r.req.venue.key(),
                    &r.req.symbol,
                    &r.req.interval,
                    r.instrument,
                    r.ps,
                    r.qs,
                ),
                r.req.start_ms,
                r.req.end_ms,
            )
        })
        .collect();

    // One shared limiter for the pool (per-venue floor, or the caller's override).
    let per_sec = opts
        .rate_limit_per_sec
        .unwrap_or_else(|| default_req_per_sec(resolved[0].1.req.venue.key()));
    let limiter = RateLimiter::per_second(per_sec);
    let lookup = &resolved;
    let bar_results = cache_load_many(
        cache_dir,
        series,
        now_ms(),
        opts,
        &limiter,
        move |key: &SeriesKey, lo, hi, sink| {
            // Find the resolved request matching this series key (unique per call).
            let r = &lookup
                .iter()
                .find(|(_, r)| {
                    r.req.venue.key() == key.venue
                        && r.req.symbol == key.symbol
                        && r.req.interval == key.interval
                })
                .expect("series key resolves to a request")
                .1;
            make_feed(
                r.req.venue,
                r.req.base(),
                key.symbol.clone(),
                key.instrument,
                r.iv.clone(),
                key.price_scale,
                key.qty_scale,
                lo,
                hi,
                sink,
            )
        },
    );

    // Re-assemble into input order, wrapping each series' bars in a LoadedMarket.
    for ((slot, r), bars) in resolved.into_iter().zip(bar_results) {
        out[slot] = Some(bars.map_err(MarketError::Cache).map(|bars| LoadedMarket {
            specs: r.specs,
            instrument: r.instrument,
            price_scale: r.ps,
            qty_scale: r.qs,
            bars,
        }));
    }
    out.into_iter()
        .map(|o| o.expect("every slot filled"))
        .collect()
}

/// Fetch the perpetual funding history for `req` (used by the funding cache).
fn fetch_funding(req: &DataRequest) -> Result<Vec<(Timestamp, i64)>, DataError> {
    let de = DataError::Schema;
    let pages_for = |per_day: i64| {
        let days = ((req.end_ms - req.start_ms).max(0) / 86_400_000) + 1;
        u32::try_from((days * per_day / 100) + 2).unwrap_or(2)
    };
    match req.venue {
        Venue::Okx => {
            use akadro_venue_okx::{ReqwestTransport, fetch_funding_history};
            let mut t = ReqwestTransport::new().map_err(|e| de(e.to_string()))?;
            fetch_funding_history(&mut t, &req.base(), &req.symbol, 100)
                .map_err(|e| de(e.to_string()))
        }
        Venue::MexcFutures => {
            use akadro_venue_mexc::{self as mexc, ReqwestTransport};
            let mut t = ReqwestTransport::new().map_err(|e| de(e.to_string()))?;
            mexc::fetch_funding_history_paged(&mut t, &req.base(), &req.symbol, pages_for(3))
                .map_err(|e| de(e.to_string()))
        }
        Venue::BinanceFutures => {
            use akadro_venue_binance::{ReqwestTransport, fetch_funding_history_paged};
            let mut t = ReqwestTransport::new().map_err(|e| de(e.to_string()))?;
            fetch_funding_history_paged(
                &mut t,
                &req.base(),
                &req.symbol,
                req.start_ms,
                req.end_ms,
                pages_for(3),
            )
            .map_err(|e| de(e.to_string()))
        }
        Venue::BybitPerp => {
            use akadro_venue_bybit::{ReqwestTransport, fetch_funding_history_paged};
            let mut t = ReqwestTransport::new().map_err(|e| de(e.to_string()))?;
            fetch_funding_history_paged(
                &mut t,
                &req.base(),
                &req.symbol,
                req.start_ms,
                req.end_ms,
                pages_for(3),
            )
            .map_err(|e| de(e.to_string()))
        }
        Venue::Mexc | Venue::Binance | Venue::Bybit | Venue::Kucoin => {
            Err(de("spot has no funding".to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venue_defaults_and_request_builder() {
        assert_eq!(Venue::Mexc.key(), "mexc");
        assert_eq!(Venue::Mexc.default_base_url(), "https://api.mexc.com");
        assert_eq!(Venue::Binance.default_base_url(), "https://api.binance.com");
        assert_eq!(Venue::Bybit.default_base_url(), "https://api.bybit.com");
        assert_eq!(Venue::Kucoin.default_base_url(), "https://api.kucoin.com");
        let r = DataRequest::new(Venue::Okx, "BTC-USDT-SWAP", "1m", 0, 1000);
        assert_eq!(r.base(), "https://www.okx.com");
        assert_eq!(
            r.with_base_url("https://aws.okx.com").base(),
            "https://aws.okx.com"
        );
    }

    #[test]
    fn interval_mapping_per_venue() {
        assert_eq!(Venue::Binance.native_interval("1m").unwrap(), "1m");
        assert_eq!(Venue::Bybit.native_interval("1m").unwrap(), "1");
        assert_eq!(Venue::Bybit.native_interval("1h").unwrap(), "60");
        assert_eq!(Venue::Bybit.native_interval("1d").unwrap(), "D");
        assert_eq!(Venue::Kucoin.native_interval("1m").unwrap(), "1min");
        assert_eq!(Venue::Kucoin.native_interval("1h").unwrap(), "1hour");
        // MEXC spot uses `60m` (not `1h`) and `1W`; OKX uses uppercase `1H`/`1D`.
        assert_eq!(Venue::Mexc.native_interval("1h").unwrap(), "60m");
        assert_eq!(Venue::Mexc.native_interval("1w").unwrap(), "1W");
        assert_eq!(Venue::Okx.native_interval("1h").unwrap(), "1H");
        assert_eq!(Venue::Okx.native_interval("1d").unwrap(), "1D");
        assert_eq!(Venue::Okx.native_interval("1s").unwrap(), "1s");
        assert!(Venue::Bybit.native_interval("7m").is_err());
        assert!(Venue::Kucoin.native_interval("2h").is_err());
    }

    #[test]
    fn perp_classification() {
        assert!(Venue::MexcFutures.is_perp("BTC_USDT"));
        assert!(Venue::BinanceFutures.is_perp("BTCUSDT"));
        assert!(Venue::BybitPerp.is_perp("BTCUSDT"));
        assert!(Venue::Okx.is_perp("BTC-USDT-SWAP"));
        assert!(!Venue::Okx.is_perp("BTC-USDT"));
        assert!(!Venue::Mexc.is_perp("BTCUSDT"));
        assert!(!Venue::Bybit.is_perp("BTCUSDT"));
    }

    #[test]
    fn load_perp_rejects_spot_offline() {
        let req = DataRequest::new(Venue::Binance, "BTCUSDT", "1m", 0, 1000);
        let err = load_perp(&req, &CacheOptions::default(), Path::new("/tmp/akadro_x"));
        assert!(matches!(err, Err(MarketError::Venue { .. })));
    }

    #[test]
    fn load_many_empty_is_empty() {
        assert!(load_many(&[], &CacheOptions::default(), Path::new("/tmp/akadro_x")).is_empty());
    }
}
