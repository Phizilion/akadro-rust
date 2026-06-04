// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Playground: run a simple SMA-crossover strategy over historical klines.
//!
//! The live-data sibling of the `two_week_15m_backtest` integration test. It pulls
//! genuine historical candles from a venue — MEXC spot (`--venue mexc`), MEXC futures
//! (`--venue mexc-futures`), or OKX spot/swap (`--venue okx`, supports `1s`) — through
//! the **one-call `akadro::data::load` / `load_perp`** surface: the range-aware,
//! resumable cache resolves the catalogue, downloads only the missing gaps, and returns
//! engine-ready bars (plus the funding schedule for a perp). The playground touches no
//! venue crate directly — it just names a `Venue` and a window. The numbers reflect
//! actual market action.
//!
//! **All parameters are runtime-configurable — no recompile needed.** Settings
//! are resolved as: built-in defaults → `playground.toml` (if present in the
//! working dir, or `--config <path>`) → `--flag value` CLI overrides (highest
//! priority). Run `cargo run -p playground -- --help` for the full list, e.g.:
//!
//! ```sh
//! cargo run -p playground -- --symbol ETHUSDT --interval 15m \
//!     --start 2025-06-01 --end 2026-05-01 --fast 20 --slow 60 --fee-bps 5
//! ```
//!
//! It is a scratch binary, deliberately *not* a `#[test]` (it needs the network
//! and is non-deterministic across data refreshes).

// Display-heavy scratch binary; the lossy casts are only for human-readable
// `$`/`%` output and position sizing, never the engine's exact integer money
// math. `unreadable_literal` is allowed for the erfc / date polynomial constants.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::unreadable_literal
)]

use std::collections::HashMap;
use std::error::Error;

use akadro::analytics::{
    PerformanceReport, TradeStats, WalkForwardSummary, deflated_sharpe, expected_max_sharpe_z,
    kurtosis, per_period_sharpe, probabilistic_sharpe, probability_of_backtest_overfitting,
    returns_from_equity, run_grid, skewness, walk_forward,
};
use akadro::backtest::{FillConfig, WalkForwardBacktest, diff_reports, replay_trades};
use akadro::data::{CacheOptions, DataRequest, Venue, load, load_perp};
use akadro::prelude::*;
use akadro::types::{InstrumentKind, InstrumentSpec, Timestamp};

type Err = Box<dyn Error>;

/// Directory (relative to the working dir) where saved `RunReport`s land. It is
/// created on demand and git-ignored — see `.gitignore`.
const RUN_ARTEFACTS_DIR: &str = "run_artefacts";

// --- Configuration -------------------------------------------------------------

/// Every tunable, resolved from defaults / config file / CLI at runtime.
// A CLI config legitimately has several independent on/off flags.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
struct Config {
    /// Data venue: `mexc` (spot REST klines), `mexc-futures` (perpetual contract
    /// klines + funding, symbol e.g. `BTC_USDT`), or `okx` (paged history-candles,
    /// supports `1s`, perp funding on `…-SWAP`).
    venue: String,
    base_url: String,
    symbol: String,
    interval: String,
    /// Inclusive start / exclusive end, epoch-milliseconds.
    start: i64,
    end: i64,
    fast: usize,
    slow: usize,
    fee_bps: i64,
    /// Starting cash in *quote units* (e.g. USDT); scaled to raw internally.
    cash: i128,
    /// Percent of capital to deploy per position (at the first price).
    deploy_pct: i64,
    /// Enforce the cash balance (reject unaffordable buys).
    enforce_cash: bool,
    /// If non-empty, save the `RunReport` JSON under `run_artefacts/` (loadable
    /// later). A bare file name is placed in that dir; an absolute path is honoured.
    save_report: String,
    /// Re-run the backtest this many times and report the fastest (a warm,
    /// steady-state benchmark — a single sub-100ms run is dominated by CPU
    /// turbo-ramp + cold-cache and understates throughput).
    repeat: usize,
    /// Diagnostic: run the *identical* loaded bars through the engine twice —
    /// once as `Spot`, once as `PerpetualFuture` — to isolate instrument-kind
    /// per-bar cost from data/strategy effects, then exit.
    bench_kind_ab: bool,
    /// Diagnostic: if set to another (cached) OKX symbol, benchmark the primary
    /// `symbol` and this one **interleaved in a single process** (A,B,A,B…),
    /// removing cross-invocation machine drift, then exit.
    bench_vs: String,
    /// Feature demo: comma-separated symbols to gap-fill **concurrently** via
    /// `data::load_many` (rate-limited), then exit. E.g. `BTCUSDT,ETHUSDT,SOLUSDT`.
    symbols: String,
    /// Feature demo: load `--interval` by **aggregating** this finer interval
    /// (`data::load_aggregated`), e.g. `--aggregate-from 1m --interval 5m`, then exit.
    aggregate_from: String,
    /// Fill realism: adverse slippage on liquidity-taking fills, basis points (0 = off).
    slippage_bps: i64,
    /// Fill realism: linear market-impact coefficient, basis points (0 = off).
    impact_bps: i64,
    /// Fill realism: execution latency in bars (0 = next-bar default).
    latency_bars: u32,
    /// Fill realism: max participation of a bar's volume, basis points (0 = uncapped).
    participation_bps: i64,
    /// Feature demo: run a **structural walk-forward analysis** (fit SMA params on each
    /// in-sample train slice, evaluate out-of-sample), then exit. Spot only.
    walk_forward: bool,
    /// Walk-forward in-sample train length, in bars.
    wf_train: usize,
    /// Walk-forward out-of-sample test length, in bars.
    wf_test: usize,
    /// Walk-forward step (roll) between folds, in bars.
    wf_step: usize,
    /// Feature demo: run an **overfitting analysis** over the SMA grid — Deflated
    /// Sharpe (multiple-testing-corrected) + Probability of Backtest Overfitting
    /// (CSCV), then exit. Spot only.
    overfit: bool,
    /// CSCV group count for the PBO demo (even; `combinatorial_splits(n, n/2)`).
    pbo_groups: usize,
    /// Comma-separated fast-SMA windows for the WFA/overfit trial grid (empty = default
    /// `5,10,20,50`). Fewer/narrower trials reduce selection bias.
    grid_fast: String,
    /// Comma-separated slow-SMA windows for the trial grid (empty = default
    /// `20,50,100,200`).
    grid_slow: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            venue: "mexc".to_string(),
            // Empty → use the venue's built-in default base URL (set --base-url to override).
            base_url: String::new(),
            symbol: "BTCUSDT".to_string(),
            interval: "5m".to_string(),
            start: 1_746_057_600_000, // 2025-05-01T00:00:00Z
            end: 1_777_593_600_000,   // 2026-05-01T00:00:00Z
            fast: 48,
            slow: 192,
            fee_bps: 5,
            cash: 1_000_000,
            deploy_pct: 90,
            enforce_cash: true,
            save_report: String::new(),
            repeat: 1,
            bench_kind_ab: false,
            bench_vs: String::new(),
            symbols: String::new(),
            aggregate_from: String::new(),
            slippage_bps: 0,
            impact_bps: 0,
            latency_bars: 0,
            participation_bps: 0,
            walk_forward: false,
            wf_train: 480,
            wf_test: 120,
            wf_step: 120,
            overfit: false,
            pbo_groups: 8,
            grid_fast: String::new(),
            grid_slow: String::new(),
        }
    }
}

/// One bar length in milliseconds for a MEXC interval string.
fn interval_ms(iv: &str) -> Result<i64, Err> {
    Ok(match iv {
        "1s" => 1_000,
        "1m" => 60_000,
        "5m" => 300_000,
        "15m" => 900_000,
        "30m" => 1_800_000,
        "60m" | "1h" => 3_600_000,
        "4h" => 14_400_000,
        "1d" => 86_400_000,
        other => return Err(format!("unsupported interval '{other}'").into()),
    })
}

/// Proleptic-Gregorian civil date → epoch-ms at UTC midnight (Howard Hinnant's
/// `days_from_civil`). Lets the config accept human `YYYY-MM-DD` dates.
fn ymd_to_epoch_ms(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    days * 86_400_000
}

/// Parse a timestamp: either `YYYY-MM-DD` (UTC midnight) or raw epoch-ms.
fn parse_when(s: &str) -> Result<i64, Err> {
    if s.contains('-') {
        let p: Vec<&str> = s.split('-').collect();
        if p.len() == 3 {
            return Ok(ymd_to_epoch_ms(p[0].parse()?, p[1].parse()?, p[2].parse()?));
        }
        return Err(format!("bad date '{s}' (want YYYY-MM-DD or epoch-ms)").into());
    }
    Ok(s.parse()?)
}

fn parse_or<T>(map: &HashMap<String, String>, key: &str, def: T) -> Result<T, Err>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match map.get(key) {
        Some(s) => s
            .parse::<T>()
            .map_err(|e| format!("bad {key}='{s}': {e}").into()),
        None => Ok(def),
    }
}

fn print_usage() {
    eprintln!(
        "playground — run the SMA-crossover backtest over MEXC data.\n\n\
         Resolution: defaults < playground.toml (cwd or --config) < --flag value.\n\n\
         Flags (all optional):\n  \
           --venue <V>         mexc|mexc-futures|okx|binance|binance-futures|bybit|bybit-perp|kucoin (default mexc)\n  \
           --symbols <A,B,C>   demo: gap-fill these symbols CONCURRENTLY (load_many), then exit\n  \
           --aggregate-from <I> demo: build --interval by aggregating this finer interval, then exit\n  \
           --walk-forward <B>  demo: structural walk-forward analysis (fit IS, eval OOS), then exit\n  \
           --wf-train <N>      walk-forward in-sample train length, bars (default 480)\n  \
           --wf-test <N>       walk-forward out-of-sample test length, bars (default 120)\n  \
           --wf-step <N>       walk-forward roll step, bars (default 120)\n  \
           --overfit <B>       demo: Deflated Sharpe + PBO (CSCV) over the SMA grid, then exit\n  \
           --pbo-groups <N>    CSCV group count for PBO (even; default 8)\n  \
           --grid-fast <A,B>   fast-SMA windows for the WFA/overfit grid (default 5,10,20,50)\n  \
           --grid-slow <A,B>   slow-SMA windows for the grid (default 20,50,100,200)\n  \
           --symbol <S>        venue symbol            (default BTCUSDT; okx BTC-USDT-SWAP; mexc-futures BTC_USDT)\n  \
           --interval <I>      1s|1m|5m|15m|30m|1h|4h|1d (default 5m; 1s = okx only)\n  \
           --start <D>         YYYY-MM-DD or epoch-ms  (default 2025-05-01)\n  \
           --end <D>           YYYY-MM-DD or epoch-ms  (default 2026-05-01)\n  \
           --fast <N>          fast SMA window (bars)  (default 48)\n  \
           --slow <N>          slow SMA window (bars)  (default 192)\n  \
           --fee-bps <N>       taker fee, basis points (default 5)\n  \
           --cash <N>          starting cash, quote    (default 1000000)\n  \
           --deploy-pct <N>    %% of capital deployed  (default 90)\n  \
           --enforce-cash <B>  true|false cash guard   (default true)\n  \
           --slippage-bps <N>  fill realism: adverse slippage on taker fills (default 0)\n  \
           --impact-bps <N>    fill realism: linear market-impact bps (default 0)\n  \
           --latency-bars <N>  fill realism: execution latency in bars (default 0)\n  \
           --participation-bps <N> fill realism: cap on bar-volume share (default 0)\n  \
           --repeat <N>        re-run N times, report fastest (warm benchmark)\n  \
           --save-report <P>   save the RunReport JSON as run_artefacts/P (loadable later)\n  \
           --base-url <U>      REST base URL\n  \
           --config <path>     load a TOML-ish key=value file\n  \
           --help"
    );
}

/// Resolve the effective config from defaults, an optional file, and CLI flags.
fn load_config() -> Result<Config, Err> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cli: HashMap<String, String> = HashMap::new();
    let mut config_path: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--help" || a == "-h" {
            print_usage();
            std::process::exit(0);
        }
        let Some(rest) = a.strip_prefix("--") else {
            return Err(format!("unexpected argument '{a}' (try --help)").into());
        };
        let (k, v) = if let Some((k, v)) = rest.split_once('=') {
            (k.to_string(), v.to_string())
        } else {
            let v = args
                .get(i + 1)
                .cloned()
                .ok_or_else(|| format!("missing value for --{rest}"))?;
            i += 1;
            (rest.to_string(), v)
        };
        let k = k.replace('-', "_");
        if k == "config" {
            config_path = Some(v);
        } else {
            cli.insert(k, v);
        }
        i += 1; // advance past the flag (the value, if separate, was consumed above)
    }

    // File (explicit, or a `playground.toml` in the working dir) under the CLI.
    let mut map: HashMap<String, String> = HashMap::new();
    let path = config_path.or_else(|| {
        let p = "playground.toml";
        std::path::Path::new(p).exists().then(|| p.to_string())
    });
    if let Some(p) = path {
        eprintln!("loading config from {p}");
        for line in std::fs::read_to_string(&p)?.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                map.insert(
                    k.trim().replace('-', "_"),
                    v.trim().trim_matches('"').to_string(),
                );
            }
        }
    }
    map.extend(cli); // CLI wins over the file

    let d = Config::default();
    let get_str = |k: &str, def: &str| map.get(k).cloned().unwrap_or_else(|| def.to_string());
    Ok(Config {
        venue: get_str("venue", &d.venue),
        base_url: get_str("base_url", &d.base_url),
        symbol: get_str("symbol", &d.symbol),
        interval: get_str("interval", &d.interval),
        start: map.get("start").map_or(Ok(d.start), |s| parse_when(s))?,
        end: map.get("end").map_or(Ok(d.end), |s| parse_when(s))?,
        fast: parse_or(&map, "fast", d.fast)?,
        slow: parse_or(&map, "slow", d.slow)?,
        fee_bps: parse_or(&map, "fee_bps", d.fee_bps)?,
        cash: parse_or(&map, "cash", d.cash)?,
        deploy_pct: parse_or(&map, "deploy_pct", d.deploy_pct)?,
        enforce_cash: parse_or(&map, "enforce_cash", d.enforce_cash)?,
        save_report: get_str("save_report", &d.save_report),
        repeat: parse_or(&map, "repeat", d.repeat)?,
        bench_kind_ab: parse_or(&map, "bench_kind_ab", d.bench_kind_ab)?,
        bench_vs: get_str("bench_vs", &d.bench_vs),
        symbols: get_str("symbols", &d.symbols),
        aggregate_from: get_str("aggregate_from", &d.aggregate_from),
        slippage_bps: parse_or(&map, "slippage_bps", d.slippage_bps)?,
        impact_bps: parse_or(&map, "impact_bps", d.impact_bps)?,
        latency_bars: parse_or(&map, "latency_bars", d.latency_bars)?,
        participation_bps: parse_or(&map, "participation_bps", d.participation_bps)?,
        walk_forward: parse_or(&map, "walk_forward", d.walk_forward)?,
        wf_train: parse_or(&map, "wf_train", d.wf_train)?,
        wf_test: parse_or(&map, "wf_test", d.wf_test)?,
        wf_step: parse_or(&map, "wf_step", d.wf_step)?,
        overfit: parse_or(&map, "overfit", d.overfit)?,
        pbo_groups: parse_or(&map, "pbo_groups", d.pbo_groups)?,
        grid_fast: get_str("grid_fast", &d.grid_fast),
        grid_slow: get_str("grid_slow", &d.grid_slow),
    })
}

// --- A simple inline SMA-crossover strategy ------------------------------------

/// Fast/slow SMA crossover, **long/flat with equity-based position sizing**: on a
/// golden cross, deploy `deploy_pct`% of *current* mark-to-market equity at the
/// current price; on a death cross, close the whole position back to flat.
///
/// Sizing off `ctx.equity()` (rather than a fixed quantity decided up front) makes
/// the position *compound* with the account and — crucially — keeps every buy
/// affordable, so the cash guard never has to reject a trade. It also means the
/// fee level now feeds back into the **gross** `PnL`, not just the net: fees shrink
/// equity, which shrinks the next position, which shrinks its `PnL`.
///
/// Reads only the backward-looking `Ctx`/`Series` accessors — reading future data
/// is a compile error, so it is simply inexpressible here.
struct SmaCrossover {
    instrument: InstrumentId,
    fast: usize,
    slow: usize,
    /// Percent of current equity to deploy on each entry.
    deploy_pct: i64,
    fast_sum: i64,
    slow_sum: i64,
    count: usize,
    prev_fast_ge_slow: Option<bool>,
}

impl SmaCrossover {
    fn new(instrument: InstrumentId, fast: usize, slow: usize, deploy_pct: i64) -> Self {
        assert!(fast > 0 && fast < slow, "need 0 < fast < slow");
        assert!(deploy_pct > 0, "deploy_pct must be positive");
        Self {
            instrument,
            fast,
            slow,
            deploy_pct,
            fast_sum: 0,
            slow_sum: 0,
            count: 0,
            prev_fast_ge_slow: None,
        }
    }
}

impl Strategy for SmaCrossover {
    fn on_bar(&mut self, bar: Bar, ctx: &mut Ctx<'_>) {
        let close = bar.close.raw();
        self.count += 1;
        self.fast_sum += close;
        self.slow_sum += close;

        if self.count > self.fast
            && let Some(p) = ctx.closes(self.instrument).and_then(|s| s.ago(self.fast))
        {
            self.fast_sum -= p.raw();
        }
        if self.count > self.slow
            && let Some(p) = ctx.closes(self.instrument).and_then(|s| s.ago(self.slow))
        {
            self.slow_sum -= p.raw();
        }

        if self.count < self.slow {
            return; // warm-up
        }

        let fast_sma = self.fast_sum / self.fast as i64;
        let slow_sma = self.slow_sum / self.slow as i64;
        let fast_ge_slow = fast_sma >= slow_sma;

        if let Some(prev) = self.prev_fast_ge_slow {
            let net = ctx.net_qty(self.instrument).raw();
            if fast_ge_slow && !prev && net <= 0 {
                // Enter long: deploy `deploy_pct`% of *current* equity at this bar's
                // close. equity (money_scale) / price (price_scale) → qty (qty_scale),
                // exactly as the fixed-size path computes it in `main`, but live.
                let deploy = ctx.equity().raw() * i128::from(self.deploy_pct) / 100;
                let qty_raw = (deploy / i128::from(close)) as i64;
                if qty_raw > 0 {
                    let order =
                        OrderRequest::market(self.instrument, Side::Buy, Qty::from_raw(qty_raw));
                    ctx.submit(order);
                }
            } else if !fast_ge_slow && prev && net > 0 {
                // Exit to flat: sell exactly what we hold (no fixed quantity to guess).
                ctx.submit(OrderRequest::market(
                    self.instrument,
                    Side::Sell,
                    Qty::from_raw(net),
                ));
            }
        }
        self.prev_fast_ge_slow = Some(fast_ge_slow);
    }
}

// --- Stats / pretty-printing helpers -------------------------------------------

/// Raw fixed-point money → quote units (display only).
fn quote(raw: i128, money_scale: u32) -> f64 {
    raw as f64 / 10f64.powi(money_scale as i32)
}

/// Complementary error function (Abramowitz & Stegun 7.1.26, |error| < 1.5e-7),
/// used to convert a t-score into a two-sided p-value via `erfc(|t| / √2)`.
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * z);
    let poly = t
        * (0.254829592
            + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))));
    let erf = 1.0 - poly * (-z * z).exp();
    if x >= 0.0 { 1.0 - erf } else { 1.0 + erf }
}

// --- venue data loading --------------------------------------------------------

/// What every venue loader returns: the dense specs (for the engine), the resolved
/// instrument + its scales, and the bars (downloaded or from the on-disk cache).
type Market = (Vec<InstrumentSpec>, InstrumentId, u32, u32, Vec<Bar>);

/// Map the config venue string to the umbrella's [`Venue`].
fn venue_of(cfg: &Config) -> Result<Venue, Err> {
    Ok(match cfg.venue.as_str() {
        "mexc" => Venue::Mexc,
        "mexc-futures" => Venue::MexcFutures,
        "okx" => Venue::Okx,
        "binance" => Venue::Binance,
        "binance-futures" => Venue::BinanceFutures,
        "bybit" => Venue::Bybit,
        "bybit-perp" => Venue::BybitPerp,
        "kucoin" => Venue::Kucoin,
        other => {
            return Err(format!(
                "unknown venue '{other}' (mexc|mexc-futures|okx|binance|binance-futures|\
                 bybit|bybit-perp|kucoin)"
            )
            .into());
        }
    })
}

/// Build a one-call [`DataRequest`] for `symbol`, honouring a `--base-url` override
/// (empty → the venue's built-in default).
fn request_for(cfg: &Config, venue: Venue, symbol: &str) -> DataRequest {
    let req = DataRequest::new(venue, symbol, &cfg.interval, cfg.start, cfg.end);
    if cfg.base_url.is_empty() {
        req
    } else {
        req.with_base_url(&cfg.base_url)
    }
}

/// Total size (bytes) of the cached `.feather` chunks in `dir` — the on-disk footprint
/// for the range-aware cache (one or more chunk files per series, plus the manifest).
fn cache_footprint(dir: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension().is_some_and(|x| x == "feather"))
                .then(|| e.metadata().ok().map_or(0, |m| m.len()))
        })
        .sum()
}

/// Feature demo: gap-fill several symbols **concurrently** through
/// [`akadro::data::load_many`] (rate-limited per venue), reporting per-symbol bar
/// counts and the wall time. Exercises the concurrent multi-series cache path.
fn demo_load_many(cfg: &Config, venue: Venue, cache_dir: &std::path::Path) {
    let symbols: Vec<&str> = cfg
        .symbols
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let reqs: Vec<DataRequest> = symbols.iter().map(|s| request_for(cfg, venue, s)).collect();
    // Opt into concurrency (default is sequential): one worker per symbol, capped at 4.
    let mut opts = CacheOptions::default();
    opts.concurrency = symbols.len().clamp(1, 4);
    eprintln!(
        "[{}] concurrent load_many: {} symbols {} at concurrency {} ...",
        cfg.venue,
        reqs.len(),
        cfg.interval,
        opts.concurrency
    );
    let t0 = std::time::Instant::now();
    let results = akadro::data::load_many(&reqs, &opts, cache_dir);
    let dt = t0.elapsed();
    println!(
        "\n=========  concurrent multi-symbol load ({})  =========",
        cfg.venue
    );
    for (sym, res) in symbols.iter().zip(&results) {
        match res {
            Ok(m) => println!(
                "  {sym:<14} {:>6} bars   instrument {:?}   scales ({}, {})",
                m.bars.len(),
                m.instrument,
                m.price_scale,
                m.qty_scale
            ),
            Err(e) => println!("  {sym:<14} ERROR: {e}"),
        }
    }
    println!("  wall time: {dt:?} (downloads ran concurrently, rate-limited)");
    println!("=========================================================\n");
}

/// Feature demo: load the requested (coarse) `--interval` by **aggregating** a cached
/// finer interval through [`akadro::data::load_aggregated`], comparing the rolled-up
/// bars against a direct download of the native coarse interval. Exercises the
/// multi-timeframe cache path.
fn demo_aggregate(
    cfg: &Config,
    req: &DataRequest,
    opts: &CacheOptions,
    cache_dir: &std::path::Path,
) -> Result<(), Err> {
    eprintln!(
        "[{}] aggregating {} -> {} for {} (download finer, roll up) ...",
        cfg.venue, cfg.aggregate_from, cfg.interval, cfg.symbol
    );
    let agg = akadro::data::load_aggregated(req, &cfg.aggregate_from, opts, cache_dir)?;
    // Cross-check against a direct download of the venue's native coarse bars.
    let direct = load(req, opts, cache_dir)?;
    let same_open =
        agg.bars.first().map(|b| b.open.raw()) == direct.bars.first().map(|b| b.open.raw());
    let same_close =
        agg.bars.last().map(|b| b.close.raw()) == direct.bars.last().map(|b| b.close.raw());
    println!(
        "\n=========  multi-timeframe aggregation ({} {})  =========",
        cfg.venue, cfg.symbol
    );
    println!(
        "  aggregated {} -> {}: {} coarse bars (rolled up from cached {} data)",
        cfg.aggregate_from,
        cfg.interval,
        agg.bars.len(),
        cfg.aggregate_from
    );
    println!(
        "  direct {} download    : {} coarse bars",
        cfg.interval,
        direct.bars.len()
    );
    println!("  edges match direct    : first-open {same_open}, last-close {same_close}");
    println!("=========================================================\n");
    Ok(())
}

/// Parse a comma-separated `usize` list, falling back to `default` when empty/blank.
fn usize_list(s: &str, default: &[usize]) -> Vec<usize> {
    if s.trim().is_empty() {
        return default.to_vec();
    }
    s.split(',')
        .filter_map(|x| x.trim().parse::<usize>().ok())
        .collect()
}

/// The SMA `(fast, slow)` candidate grid (fast &lt; slow) — the trial set shared by the
/// walk-forward in-sample optimisation and the overfitting analysis. Tunable via
/// `--grid-fast` / `--grid-slow` (fewer/narrower trials reduce selection bias).
fn sma_candidates(cfg: &Config) -> Vec<(usize, usize)> {
    let fasts = usize_list(&cfg.grid_fast, &[5, 10, 20, 50]);
    let slows = usize_list(&cfg.grid_slow, &[20, 50, 100, 200]);
    let mut v = Vec::new();
    for &f in &fasts {
        for &s in &slows {
            if f < s {
                v.push((f, s));
            }
        }
    }
    v
}

/// In-sample objective for the walk-forward fit: the total return of SMA `(fast, slow)`
/// over the **train** slice only. Built from the *exact same* `FillConfig` the
/// framework uses out-of-sample (`SimulatedExchange::with_config`), so IS and OOS share
/// one fill model bit-for-bit (fees, cash, slippage, impact, latency, participation) —
/// no config drift. An invalid pairing, too-short train, or a blow-up scores
/// `NEG_INFINITY` so it is never selected.
#[allow(clippy::too_many_arguments)]
fn wf_is_return(
    train: &[Bar],
    fast: usize,
    slow: usize,
    specs: &[InstrumentSpec],
    instrument: InstrumentId,
    cash_raw: i128,
    fill: FillConfig,
    deploy_pct: i64,
    ppy: f64,
) -> f64 {
    if fast >= slow || train.len() <= slow {
        return f64::NEG_INFINITY;
    }
    let strategy = SmaCrossover::new(instrument, fast, slow, deploy_pct);
    let exchange = SimulatedExchange::with_config(specs.to_vec(), fill);
    let Ok(engine) = Engine::new(
        specs,
        Money::from_raw(cash_raw),
        HistoricalFeed::from_bars(train.to_vec()),
        exchange,
        strategy,
    ) else {
        return f64::NEG_INFINITY;
    };
    let report = engine.run();
    PerformanceReport::from_equity(&report.equity_curve, ppy)
        .map_or(f64::NEG_INFINITY, |p| p.total_return)
}

/// Feature demo: a **structural walk-forward analysis** (`akadro-analytics` +
/// `akadro-backtest`). For each rolling fold the `fit` step grid-searches SMA
/// `(fast, slow)` on the **in-sample** train slice only (parallel via `run_grid`) and
/// the framework evaluates the chosen params **out-of-sample** — the test slice is
/// structurally invisible to `fit`, so there is no look-ahead and no select-by-OOS. The
/// per-fold OOS reports are pooled into a [`WalkForwardSummary`] (pooled *returns*, not
/// stitched equity) with walk-forward efficiency. Spot only.
// Long but linear (load → windows → fit/run → report); FillConfig is `#[non_exhaustive]`
// so it must be mutated after `default()` (no cross-crate struct literal).
#[allow(clippy::too_many_lines, clippy::field_reassign_with_default)]
fn demo_walk_forward(
    cfg: &Config,
    venue: Venue,
    req: &DataRequest,
    opts: &CacheOptions,
    cache_dir: &std::path::Path,
) -> Result<(), Err> {
    if venue.is_perp(&cfg.symbol) {
        return Err(
            "walk-forward demo is spot-only (a perp needs a funding schedule the \
                    structural runner's FillConfig doesn't carry)"
                .into(),
        );
    }
    let bar_ms = interval_ms(&cfg.interval)?;
    let ppy = 365.0 * 86_400_000.0 / bar_ms as f64;
    eprintln!(
        "[{}] walk-forward: loading {} {} bars ...",
        cfg.venue, cfg.symbol, cfg.interval
    );
    let market = load(req, opts, cache_dir)?;
    let bars = market.bars;
    let (instrument, specs) = (market.instrument, market.specs);
    let money_scale = market.price_scale + market.qty_scale;
    let cash_raw = cfg.cash * 10i128.pow(money_scale);
    if bars.len() < cfg.wf_train + cfg.wf_test {
        return Err(format!(
            "not enough bars ({}) for walk-forward (need >= train {} + test {}); widen the window",
            bars.len(),
            cfg.wf_train,
            cfg.wf_test
        )
        .into());
    }

    // Rolling windows in close-stamp nanoseconds (the cache stamps bars at close-ns).
    let bar_width_ns = bar_ms * 1_000_000;
    let start = bars.first().expect("non-empty").ts;
    let end = bars.last().expect("non-empty").ts;
    let windows = walk_forward(
        start,
        end,
        cfg.wf_train as i64 * bar_width_ns,
        cfg.wf_test as i64 * bar_width_ns,
        cfg.wf_step.max(1) as i64 * bar_width_ns,
    );
    if windows.is_empty() {
        return Err("no walk-forward windows (train+test span exceeds the data)".into());
    }

    // One fill config — fee, cash, and the same fill-realism knobs the main backtest
    // honours — reused verbatim for BOTH the in-sample fit and the out-of-sample run, so
    // there is no config drift between IS and OOS.
    let mut fill = FillConfig::default();
    fill.fee_bps = cfg.fee_bps;
    if cfg.enforce_cash {
        fill.starting_cash = Some(cash_raw);
    }
    if cfg.slippage_bps > 0 {
        fill.slippage_bps = cfg.slippage_bps;
    }
    if cfg.impact_bps > 0 {
        fill.impact_bps = cfg.impact_bps;
    }
    if cfg.latency_bars > 0 {
        fill.latency_bars = cfg.latency_bars;
    }
    if cfg.participation_bps > 0 {
        fill.max_participation_bps = Some(cfg.participation_bps);
    }

    // In-sample optimisation grid: SMA (fast, slow) candidates (fast < slow).
    let candidates = sma_candidates(cfg);
    eprintln!(
        "[{}] walk-forward: {} folds, {} IS candidates/fold (train {} / test {} / step {} bars) ...",
        cfg.venue,
        windows.len(),
        candidates.len(),
        cfg.wf_train,
        cfg.wf_test,
        cfg.wf_step
    );

    let wf = WalkForwardBacktest::new(&specs, Money::from_raw(cash_raw), fill, &bars, &windows);
    let deploy = cfg.deploy_pct;
    let specs_ref = &specs;
    let cand = &candidates;
    let (folds, fit_log) = wf
        .run(
            Vec::<(usize, usize, f64)>::new(),
            |train: &[Bar], st: &mut Vec<(usize, usize, f64)>| {
                // IS grid search (parallel via run_grid) — the train slice ONLY.
                let cells: Vec<_> = cand
                    .iter()
                    .map(|&(f, s)| {
                        move || {
                            wf_is_return(
                                train, f, s, specs_ref, instrument, cash_raw, fill, deploy, ppy,
                            )
                        }
                    })
                    .collect();
                let scores = run_grid(cells);
                let best = scores
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                    .map_or(0, |(i, _)| i);
                let (f, s) = cand[best];
                st.push((f, s, scores[best]));
                (f, s)
            },
            |&(f, s): &(usize, usize)| SmaCrossover::new(instrument, f, s, deploy),
        )
        .map_err(|e| -> Err { format!("walk-forward run: {e}").into() })?;

    // Pool the per-fold OOS reports (pooled returns, not stitched equity) + WFE.
    let oos_reports: Vec<RunReport> = folds.iter().map(|fold| fold.report.clone()).collect();
    let mean_is: f64 = if fit_log.is_empty() {
        0.0
    } else {
        fit_log.iter().map(|&(_, _, r)| r).sum::<f64>() / fit_log.len() as f64
    };
    let summary = WalkForwardSummary::from_reports(&oos_reports, ppy, Some(mean_is));

    println!(
        "\n=========  walk-forward analysis ({} {} {})  =========",
        cfg.venue, cfg.symbol, cfg.interval
    );
    println!(
        "  {:>4}  {:>15}  {:>9}  {:>9}  {:>9}",
        "fold", "OOS start (ns)", "params", "OOS ret%", "OOS Shrp"
    );
    for (i, (fold, &(f, s, _is))) in folds.iter().zip(&fit_log).enumerate() {
        let oos = PerformanceReport::from_equity(&fold.report.equity_curve, ppy);
        let (ret, shrp) = oos.map_or((f64::NAN, f64::NAN), |p| (p.total_return * 100.0, p.sharpe));
        println!(
            "  {:>4}  {:>15}  {:>4}/{:<4}  {:>9.2}  {:>9.3}",
            i + 1,
            fold.window.test_start.as_nanos(),
            f,
            s,
            ret,
            shrp
        );
    }
    println!("  ---- pooled out-of-sample (WalkForwardSummary) ----");
    println!(
        "  folds: {} ({} measured, {} unmeasurable/blow-up), pooled OOS periods: {}",
        summary.folds, summary.measured_folds, summary.unmeasurable_folds, summary.pooled_periods
    );
    if let Some(p) = &summary.pooled {
        println!(
            "  pooled OOS: return {:.2}%  Sharpe {:.3}  Sortino {:.3}  maxDD {:.2}%",
            p.total_return * 100.0,
            p.sharpe,
            p.sortino,
            p.max_drawdown * 100.0
        );
    } else {
        println!("  pooled OOS: NOT COMPUTABLE (a fold blew up / too few points)");
    }
    match summary.efficiency {
        Some(wfe) => println!(
            "  walk-forward efficiency (pooled OOS / mean IS return): {wfe:.3}  \
             (>1 = OOS beat IS; <1 = the usual overfit degradation)"
        ),
        None => println!("  walk-forward efficiency: n/a (non-positive in-sample return)"),
    }
    println!(
        "  mean in-sample return of the fitted params: {:.2}%",
        mean_is * 100.0
    );
    println!("=========================================================\n");
    Ok(())
}

/// Per-bar return series of SMA `(fast, slow)` over the **full** window — one trial /
/// one column of the overfitting analysis. Empty on an invalid pairing or a blow-up
/// (the caller drops short series so every trial shares one length `T`).
#[allow(clippy::too_many_arguments)]
fn full_window_returns(
    bars: &[Bar],
    fast: usize,
    slow: usize,
    specs: &[InstrumentSpec],
    instrument: InstrumentId,
    cash_raw: i128,
    fill: FillConfig,
    deploy_pct: i64,
) -> Vec<f64> {
    if fast >= slow || bars.len() <= slow {
        return Vec::new();
    }
    let strategy = SmaCrossover::new(instrument, fast, slow, deploy_pct);
    let exchange = SimulatedExchange::with_config(specs.to_vec(), fill);
    let Ok(engine) = Engine::new(
        specs,
        Money::from_raw(cash_raw),
        HistoricalFeed::from_bars(bars.to_vec()),
        exchange,
        strategy,
    ) else {
        return Vec::new();
    };
    returns_from_equity(&engine.run().equity_curve).unwrap_or_default()
}

/// Feature demo: an **overfitting analysis** over the SMA grid — the Bailey–López de
/// Prado pair. **Deflated Sharpe** discounts the best trial's Sharpe for having been
/// chosen among many (multiple-testing bias); **PBO** (Probability of Backtest
/// Overfitting, via the library's CSCV `probability_of_backtest_overfitting`) is the
/// chance the in-sample winner is no better than the median out-of-sample. Spot only.
// FillConfig is `#[non_exhaustive]` → mutate after `default()` (no cross-crate literal).
#[allow(clippy::field_reassign_with_default, clippy::too_many_lines)]
fn demo_overfit(
    cfg: &Config,
    venue: Venue,
    req: &DataRequest,
    opts: &CacheOptions,
    cache_dir: &std::path::Path,
) -> Result<(), Err> {
    if venue.is_perp(&cfg.symbol) {
        return Err("overfit demo is spot-only".into());
    }
    eprintln!(
        "[{}] overfit analysis: loading {} {} bars ...",
        cfg.venue, cfg.symbol, cfg.interval
    );
    let market = load(req, opts, cache_dir)?;
    let bars = market.bars;
    let (instrument, specs) = (market.instrument, market.specs);
    let money_scale = market.price_scale + market.qty_scale;
    let cash_raw = cfg.cash * 10i128.pow(money_scale);

    let mut fill = FillConfig::default();
    fill.fee_bps = cfg.fee_bps;
    if cfg.enforce_cash {
        fill.starting_cash = Some(cash_raw);
    }

    let candidates = sma_candidates(cfg);
    eprintln!(
        "[{}] overfit analysis: running {} SMA trials over {} bars ...",
        cfg.venue,
        candidates.len(),
        bars.len()
    );
    // One full-window backtest per trial (parallel) → the per-strategy return matrix.
    let bars_ref = &bars;
    let specs_ref = &specs;
    let deploy = cfg.deploy_pct;
    let cells: Vec<_> = candidates
        .iter()
        .map(|&(f, s)| {
            move || {
                full_window_returns(
                    bars_ref, f, s, specs_ref, instrument, cash_raw, fill, deploy,
                )
            }
        })
        .collect();
    let all_returns = run_grid(cells);

    // Keep only trials that produced the full-length series (drop invalid/blow-up).
    let t_full = all_returns.iter().map(Vec::len).max().unwrap_or(0);
    let trials: Vec<(usize, &Vec<f64>)> = candidates
        .iter()
        .zip(&all_returns)
        .enumerate()
        .filter(|(_, (_, r))| r.len() == t_full && t_full > 0)
        .map(|(i, (_, r))| (i, r))
        .collect();
    if trials.len() < 2 {
        return Err(
            "not enough valid trials for an overfitting analysis (widen the window)".into(),
        );
    }
    let matrix: Vec<Vec<f64>> = trials.iter().map(|(_, r)| (*r).clone()).collect();

    // Per-trial per-period Sharpe → the selected (best) trial + the trial-Sharpe spread.
    let sharpes: Vec<f64> = matrix.iter().map(|r| per_period_sharpe(r)).collect();
    let best = sharpes
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map_or(0, |(i, _)| i);
    let (best_fast, best_slow) = candidates[trials[best].0];
    let n_trials = matrix.len();
    let n = t_full;
    let mean_sr = sharpes.iter().sum::<f64>() / n_trials as f64;
    let sr_variance = sharpes.iter().map(|s| (s - mean_sr).powi(2)).sum::<f64>()
        / (n_trials as f64 - 1.0).max(1.0);
    let sr = sharpes[best];
    let skew = skewness(&matrix[best]);
    let kurt = kurtosis(&matrix[best]);
    let dsr = deflated_sharpe(sr, sr_variance, n_trials, n, skew, kurt);
    let psr = probabilistic_sharpe(sr, 0.0, n, skew, kurt);
    let e_max = expected_max_sharpe_z(n_trials);

    // PBO via the library's CSCV.
    let pbo = probability_of_backtest_overfitting(&matrix, cfg.pbo_groups);

    println!(
        "\n=========  overfitting analysis ({} {} {})  =========",
        cfg.venue, cfg.symbol, cfg.interval
    );
    println!("  trials: {n_trials} SMA (fast,slow) over {n} per-bar returns");
    println!(
        "  per-trial per-period Sharpe: best {best_fast}/{best_slow} SR={sr:.4}  (mean {mean_sr:.4}, σ {:.4})",
        sr_variance.sqrt()
    );
    println!("  ---- Deflated Sharpe (multiple-testing correction) ----");
    println!("  E[max SR | {n_trials} trials] : {e_max:.4}  (the bar a lucky pick must clear)");
    println!(
        "  naive PSR (true SR > 0)      : {:.1}%  (no selection correction)",
        psr * 100.0
    );
    println!(
        "  Deflated SR (true SR > 0)    : {:.1}%  (after correcting for {n_trials} trials)  [low ⇒ likely a fluke]",
        dsr * 100.0
    );
    println!("  ---- Probability of Backtest Overfitting (CSCV) ----");
    match pbo {
        Some(r) => println!(
            "  PBO: {:.1}%  ({} groups, {} splits)  [near 0 ⇒ selection generalises; near 1 ⇒ overfit]",
            r.pbo * 100.0,
            cfg.pbo_groups,
            r.splits
        ),
        None => println!(
            "  PBO: n/a (need an even pbo-groups in 2..=returns, ≥2 trials; got groups={})",
            cfg.pbo_groups
        ),
    }
    println!("=========================================================\n");
    Ok(())
}

/// Diagnostic A/B: run the SAME bars through the engine as `Spot` then as
/// `PerpetualFuture` (only the instrument `kind` differs), each `runs` times,
/// reporting the fastest. Isolates per-bar instrument-kind cost from data/strategy
/// differences between two separately-downloaded datasets.
fn bench_kind_ab(
    specs: &[InstrumentSpec],
    instrument: InstrumentId,
    bars: &[Bar],
    cash_raw: i128,
    cfg: &Config,
) -> Result<(), Err> {
    let runs = cfg.repeat.max(8);
    let idx = instrument.index() as usize;
    eprintln!(
        "kind A/B: {} bars, identical data, only instrument kind flipped, {runs} reps each",
        bars.len()
    );
    for kind in [InstrumentKind::Spot, InstrumentKind::PerpetualFuture] {
        let mut specs2 = specs.to_vec();
        specs2[idx].kind = kind;
        let mut best_ms = f64::MAX;
        for _ in 0..runs {
            let strategy = SmaCrossover::new(instrument, cfg.fast, cfg.slow, cfg.deploy_pct);
            let mut exchange = SimulatedExchange::new(specs2.clone(), cfg.fee_bps);
            if kind == InstrumentKind::PerpetualFuture {
                // Funding is mandatory for a perp; a single zero-rate settlement
                // satisfies that while charging nothing, so the A/B isolates only the
                // instrument-kind per-bar cost (no funding noise).
                exchange = exchange.with_funding_schedule(
                    instrument,
                    1,
                    vec![(Timestamp::from_nanos(0), 0)],
                );
            }
            if cfg.enforce_cash {
                exchange = exchange.with_starting_cash(Money::from_raw(cash_raw));
            }
            let engine = Engine::new(
                &specs2,
                Money::from_raw(cash_raw),
                HistoricalFeed::from_bars(bars.to_vec()),
                exchange,
                strategy,
            )?;
            let t0 = std::time::Instant::now();
            let _ = engine.run();
            best_ms = best_ms.min(t0.elapsed().as_secs_f64() * 1000.0);
        }
        eprintln!(
            "  {:<16?}: fastest {best_ms:>8.3} ms  ({:.0} bars/sec)",
            kind,
            bars.len() as f64 / (best_ms / 1000.0)
        );
    }
    Ok(())
}

/// Time one `engine.run()` over `m`'s bars (ms); rebuild is outside the timed span.
fn time_run(m: &Market, cfg: &Config, cash_raw: i128) -> Result<f64, Err> {
    let (specs, instrument, _ps, _qs, bars) = m;
    let strategy = SmaCrossover::new(*instrument, cfg.fast, cfg.slow, cfg.deploy_pct);
    let mut exchange = SimulatedExchange::new(specs.clone(), cfg.fee_bps);
    if cfg.enforce_cash {
        exchange = exchange.with_starting_cash(Money::from_raw(cash_raw));
    }
    let engine = Engine::new(
        specs,
        Money::from_raw(cash_raw),
        HistoricalFeed::from_bars(bars.clone()),
        exchange,
        strategy,
    )?;
    let t0 = std::time::Instant::now();
    let _ = engine.run();
    Ok(t0.elapsed().as_secs_f64() * 1000.0)
}

/// Interleaved in-process A/B of two datasets (alternate A,B,A,B…) — removes all
/// cross-invocation machine drift, so a surviving gap is a genuine per-dataset cost.
fn bench_interleaved(la: &str, a: &Market, lb: &str, b: &Market, cfg: &Config) -> Result<(), Err> {
    let runs = cfg.repeat.max(12);
    let cash = |m: &Market| cfg.cash * 10i128.pow(m.2 + m.3);
    let (ca, cb) = (cash(a), cash(b));
    let (mut best_a, mut best_b) = (f64::MAX, f64::MAX);
    eprintln!(
        "interleaved A/B in ONE process: {la} ({} bars) vs {lb} ({} bars), {runs} reps each",
        a.4.len(),
        b.4.len()
    );
    for _ in 0..runs {
        best_a = best_a.min(time_run(a, cfg, ca)?);
        best_b = best_b.min(time_run(b, cfg, cb)?);
    }
    let rate = |m: &Market, ms: f64| m.4.len() as f64 / (ms / 1000.0);
    eprintln!(
        "  A {la:<16}: fastest {best_a:>8.3} ms  ({:.0} bars/sec)",
        rate(a, best_a)
    );
    eprintln!(
        "  B {lb:<16}: fastest {best_b:>8.3} ms  ({:.0} bars/sec)",
        rate(b, best_b)
    );
    Ok(())
}

#[allow(clippy::too_many_lines)] // linear load → fetch → run → report script
fn main() -> Result<(), Err> {
    let cfg = load_config()?;
    let bar_ms = interval_ms(&cfg.interval)?;
    let periods_per_year = 365.0 * 86_400_000.0 / bar_ms as f64;
    eprintln!("config: {cfg:?}");

    // 1 + 2. Resolve the instrument and download-only-the-gaps (or replay) the candles
    //    through the ONE-CALL `data::load` / `load_perp` surface: the range-aware,
    //    resumable cache (keyed by venue+symbol+interval, NOT the window) does all the
    //    plumbing — catalogue lookup, gap detection, incremental download, assembly.
    //    For a perp it also fetches the mandatory funding schedule. Cached under a
    //    local `market_data/` dir (git-ignored).
    let cache_dir = std::path::Path::new("market_data");
    std::fs::create_dir_all(cache_dir)?;
    let venue = venue_of(&cfg)?;
    let req = request_for(&cfg, venue, &cfg.symbol);
    let opts = CacheOptions::default();

    // Feature demo: concurrent multi-symbol gap-fill via `data::load_many`, then exit.
    if !cfg.symbols.is_empty() {
        demo_load_many(&cfg, venue, cache_dir);
        return Ok(());
    }
    // Feature demo: multi-timeframe aggregation via `data::load_aggregated`, then exit.
    if !cfg.aggregate_from.is_empty() {
        return demo_aggregate(&cfg, &req, &opts, cache_dir);
    }
    // Feature demo: structural walk-forward analysis (akadro-analytics + akadro-backtest).
    if cfg.walk_forward {
        return demo_walk_forward(&cfg, venue, &req, &opts, cache_dir);
    }
    // Feature demo: overfitting analysis (Deflated Sharpe + PBO/CSCV) over the SMA grid.
    if cfg.overfit {
        return demo_overfit(&cfg, venue, &req, &opts, cache_dir);
    }

    let (specs, instrument, price_scale, qty_scale, bars, funding_sched, funding_interval) =
        if venue.is_perp(&cfg.symbol) {
            let pm = load_perp(&req, &opts, cache_dir)?;
            eprintln!(
                "[{}] funding: {} settlements (cache or download), accruing every {} bars (~{}h)",
                cfg.venue,
                pm.funding_schedule.len(),
                pm.funding_interval_bars,
                i64::from(pm.funding_interval_bars) * bar_ms / 3_600_000
            );
            let m = pm.market;
            (
                m.specs,
                m.instrument,
                m.price_scale,
                m.qty_scale,
                m.bars,
                pm.funding_schedule,
                pm.funding_interval_bars,
            )
        } else {
            let m = load(&req, &opts, cache_dir)?;
            (
                m.specs,
                m.instrument,
                m.price_scale,
                m.qty_scale,
                m.bars,
                Vec::new(),
                0u32,
            )
        };
    let money_scale = price_scale + qty_scale;
    assert!(!bars.is_empty(), "no bars returned");
    let first = &bars[0];
    let last = &bars[bars.len() - 1];
    let expected = ((cfg.end - cfg.start) / bar_ms) as usize;

    // 3. Starting cash. The strategy now sizes each entry off *current* equity
    //    (deploy_pct% at the live price), so there is no single fixed quantity. We
    //    still compute the first-entry estimate (deploy_pct% of cash at the first
    //    price) purely for the summary line below.
    let cash_raw = cfg.cash * 10i128.pow(money_scale);
    let p0 = first.open.raw() as i128;
    let first_entry_est = (cash_raw * i128::from(cfg.deploy_pct) / 100 / p0) as i64;
    assert!(first_entry_est > 0, "position sizing underflowed");

    // Diagnostic: isolate instrument-kind per-bar cost on identical data, then exit.
    if cfg.bench_kind_ab {
        return bench_kind_ab(&specs, instrument, &bars, cash_raw, &cfg);
    }

    // Diagnostic: interleave two datasets in ONE process (removes machine drift).
    if !cfg.bench_vs.is_empty() {
        let mb = load(&request_for(&cfg, venue, &cfg.bench_vs), &opts, cache_dir)?;
        let b: Market = (
            mb.specs,
            mb.instrument,
            mb.price_scale,
            mb.qty_scale,
            mb.bars,
        );
        let a: Market = (
            specs.clone(),
            instrument,
            price_scale,
            qty_scale,
            bars.clone(),
        );
        return bench_interleaved(&cfg.symbol, &a, &cfg.bench_vs, &b, &cfg);
    }
    // 4. Time ONLY the backtest (engine loop + fills + equity marking). Optionally
    //    repeat (`--repeat N`): a single sub-100ms run is a poor benchmark — CPU
    //    turbo takes tens of ms to ramp and the i-cache/branch-predictor start cold,
    //    so the first run understates steady-state throughput. We rebuild the engine
    //    each iteration (the rebuild + `bars.clone()` are OUTSIDE the timed region)
    //    and report the fastest run as the representative figure. Funding (mandatory,
    //    cache-backed) was already resolved by `load_perp` above for a perp.
    let runs = cfg.repeat.max(1);
    let (report, backtest_time) = {
        let mut best: Option<(RunReport, std::time::Duration)> = None;
        for run_idx in 0..runs {
            let strategy = SmaCrossover::new(instrument, cfg.fast, cfg.slow, cfg.deploy_pct);
            let mut exchange = SimulatedExchange::new(specs.clone(), cfg.fee_bps);
            if cfg.enforce_cash {
                exchange = exchange.with_starting_cash(Money::from_raw(cash_raw));
            }
            // Opt-in fill realism (each defaults to the conservative no-op).
            if cfg.slippage_bps > 0 {
                exchange = exchange.with_slippage_bps(cfg.slippage_bps);
            }
            if cfg.impact_bps > 0 {
                exchange = exchange.with_impact_model(cfg.impact_bps);
            }
            if cfg.latency_bars > 0 {
                exchange = exchange.with_latency_bars(cfg.latency_bars);
            }
            if cfg.participation_bps > 0 {
                exchange = exchange.with_participation_bps(cfg.participation_bps);
            }
            if !funding_sched.is_empty() {
                exchange = exchange.with_funding_schedule(
                    instrument,
                    funding_interval,
                    funding_sched.clone(),
                );
            }
            let engine = Engine::new(
                &specs,
                Money::from_raw(cash_raw),
                HistoricalFeed::from_bars(bars.clone()),
                exchange,
                strategy,
            )?;
            let t0 = std::time::Instant::now();
            let r = engine.run();
            let dt = t0.elapsed();
            if runs > 1 {
                eprintln!(
                    "  bench run #{:<2}: {dt:>12.3?}  ({:.0} bars/sec)",
                    run_idx + 1,
                    bars.len() as f64 / dt.as_secs_f64().max(1e-9)
                );
            }
            // Keep the fastest run (steady-state), and its report (all runs are
            // bit-identical — determinism — so any report is fine).
            if best.as_ref().is_none_or(|(_, b)| dt < *b) {
                best = Some((r, dt));
            }
        }
        best.expect("runs >= 1")
    };

    // 5. The candles are already cached on disk by the gap-fill loader above (one or
    //    more LZ4 `.feather` chunks per series under `market_data/`).
    let on_disk = cache_footprint(cache_dir);

    // 6. Analytics + report.
    let perf = PerformanceReport::from_equity(&report.equity_curve, periods_per_year);
    let final_eq = report
        .equity_curve
        .last()
        .map_or(cash_raw, |p| p.equity.raw());
    let ret_pct = (final_eq - cash_raw) as f64 / cash_raw as f64 * 100.0;
    let span_days = (last.ts.as_nanos() / 1_000_000 - (first.ts.as_nanos() / 1_000_000 - bar_ms))
        as f64
        / 86_400_000.0;
    let px = |p: i64| p as f64 / 10f64.powi(price_scale as i32);

    println!(
        "\n=========  {} {} — {} backtest  =========",
        cfg.symbol,
        cfg.interval,
        cfg.venue.to_uppercase()
    );
    println!("  window (req)     : {} .. {} ms", cfg.start, cfg.end);
    println!(
        "  bars fetched     : {} (gapless grid would be {}; venue retention may clamp fine intervals)",
        bars.len(),
        expected
    );
    println!(
        "  covered span     : ~{span_days:.1} days, price ${:.2} -> ${:.2}",
        px(first.close.raw()),
        px(last.close.raw())
    );
    println!(
        "  ---- strategy: SMA crossover (fast {} / slow {}), {} bps taker fee ----",
        cfg.fast, cfg.slow, cfg.fee_bps
    );
    println!(
        "  position sizing  : {}% of CURRENT equity per entry (compounding); first entry ~{first_entry_est} raw qty",
        cfg.deploy_pct
    );
    println!(
        "  cash guard       : {}",
        if cfg.enforce_cash {
            "ON (unaffordable buys rejected)"
        } else {
            "OFF"
        }
    );
    println!("  bars processed   : {}", report.bars_processed);
    println!("  orders submitted : {}", report.orders_submitted);
    println!("  fills            : {}", report.fills.len());
    // Total traded notional (turnover): sum of price×qty over every fill, widened
    // to i128 before multiplying (exact). A high turnover is what makes fees bite.
    let total_notional: i128 = report
        .fills
        .iter()
        .map(|f| f.price.notional(f.qty).raw())
        .sum();
    println!(
        "  total traded     : ~{:.2} quote  ({:.1}x starting capital, over {} fills)",
        quote(total_notional, money_scale),
        total_notional as f64 / cash_raw as f64,
        report.fills.len()
    );
    // Completed round-trip trades reconstructed from the fill log (one entry +
    // one exit = one trade; the 656 fills above are individual executions).
    let trades = TradeStats::from_fills(&report.fills);
    println!(
        "  completed trades : {} ({} win / {} loss, {:.1}% win rate, profit factor {:.2})",
        trades.num_trades,
        trades.wins,
        trades.losses,
        trades.win_rate * 100.0,
        trades.profit_factor
    );
    let elapsed_secs = backtest_time.as_secs_f64().max(1e-9);
    println!(
        "  backtest runtime : {backtest_time:?}  ({:.0} bars/sec; excludes download & caching)",
        bars.len() as f64 / elapsed_secs
    );
    // A debug build (`cargo run` without `--release`) is ~30-50× slower here:
    // opt-level 0 + overflow-checks + debug-assertions dominate the integer money
    // math. The bars/sec above is NOT representative unless this is a release build.
    #[cfg(debug_assertions)]
    println!(
        "  ⚠ build          : DEBUG — speed is ~30-50x slower than release; \
         re-run with `cargo run --release` for a representative bars/sec"
    );
    println!(
        "  starting equity  : ~{:.2}  ->  final ~{:.2}  ({ret_pct:+.2}%)",
        quote(cash_raw, money_scale),
        quote(final_eq, money_scale)
    );
    // Buy-and-hold benchmark over the SAME window (full cash in at the first close,
    // held to the last) — separates strategy timing from the asset's secular drift.
    let bh_mult = last.close.raw() as f64 / first.close.raw() as f64;
    let bh_final_raw = (cash_raw as f64 * bh_mult) as i128;
    println!(
        "  buy & hold       : ~{:.2}  ({:+.2}%, {bh_mult:.1}x)  |  strategy / B&H = {:.2}x",
        quote(bh_final_raw, money_scale),
        (bh_mult - 1.0) * 100.0,
        final_eq as f64 / bh_final_raw as f64
    );
    println!(
        "  realized PnL     : ~{:.2}   trading fees ~{:.2}  funding ~{:.2}",
        quote(report.realized_pnl.raw(), money_scale),
        quote(report.trading_fees.raw(), money_scale),
        quote(report.funding_net.raw(), money_scale)
    );
    if let Some(p) = perf {
        println!("  ---- risk/return (akadro-analytics) ----");
        println!("  annualized return: {:+.2}%", p.annualized_return * 100.0);
        // `volatility` is already annualized by akadro-analytics.
        println!("  volatility (ann) : {:.2}%", p.volatility * 100.0);
        println!("  Sharpe / Sortino : {:.3} / {:.3}", p.sharpe, p.sortino);
        println!("  max drawdown     : {:.2}%", p.max_drawdown * 100.0);
        println!("  Calmar           : {:.3}", p.calmar);
        if p.volatility > 0.0 {
            // t = annualized Sharpe × √(N / periods_per_year), robust to how the
            // volatility field is scaled.
            let t_stat = p.sharpe * (p.periods as f64 / periods_per_year).sqrt();
            let p_value = erfc(t_stat.abs() / std::f64::consts::SQRT_2);
            println!("  t-stat / p-value : {t_stat:.3} / {p_value:.4}  (H0: mean bar-return = 0)");
        }
    } else {
        println!(
            "  ---- risk/return: NOT COMPUTABLE (equity hit <= 0; analytics return None) ----"
        );
    }
    println!(
        "  data on disk     : {} bytes ({:.0} KiB) in {}",
        on_disk,
        on_disk as f64 / 1024.0,
        cache_dir.display()
    );
    if !cfg.save_report.is_empty() {
        // All reports land under `run_artefacts/` (created on demand, git-ignored).
        // We take only the file name of `--save-report` so a stray path can't write
        // outside the artefacts dir; an absolute path is honoured as-is.
        let requested = std::path::Path::new(&cfg.save_report);
        let target = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            let dir = std::path::Path::new(RUN_ARTEFACTS_DIR);
            std::fs::create_dir_all(dir)?;
            dir.join(requested.file_name().unwrap_or(requested.as_os_str()))
        };
        report.save(&target)?;
        // Demonstrate the "save now, analyse later" round-trip: reload the file we
        // just wrote and confirm the fill log + realized PnL survived intact.
        let reloaded = RunReport::load(&target)?;
        assert_eq!(
            reloaded.fills.len(),
            report.fills.len(),
            "reload: fill count"
        );
        assert_eq!(reloaded.realized_pnl, report.realized_pnl, "reload: PnL");
        let saved_bytes = std::fs::metadata(&target)?.len();
        // Resolve to an absolute path so the user can see exactly where (which
        // directory) the report landed, regardless of the working dir.
        let abs = std::fs::canonicalize(&target).unwrap_or(target);
        let dir = abs
            .parent()
            .map_or_else(|| ".".to_string(), |p| p.display().to_string());
        println!(
            "  run report saved : {} ({saved_bytes} bytes, format v{}) — reloaded & verified",
            abs.display(),
            akadro::engine::REPORT_FORMAT_VERSION,
        );
        println!("  report directory : {dir}");

        // Record/replay regression oracle: re-drive the *trades* from the on-disk
        // report through the engine + simulated exchange — with NO strategy
        // logic — and confirm the rebuilt-from-zero result matches. Run the saved
        // report through this with an old vs a new binary to catch an unintended
        // (or a silently-absent) change to trade-simulation behaviour.
        let rebuilt = replay_trades(&reloaded, cfg.fee_bps)?;
        let check = diff_reports(&reloaded, &rebuilt);
        if check.matches {
            println!(
                "  replay oracle    : MATCH ✓ — {} trades re-driven from zero (realized PnL ~{:.2}, fees ~{:.2})",
                rebuilt.fills.len(),
                quote(rebuilt.realized_pnl.raw(), money_scale),
                quote(rebuilt.trading_fees.raw(), money_scale),
            );
        } else {
            println!("  replay oracle    : MISMATCH ✗ — trade-simulation behaviour changed:");
            for issue in &check.issues {
                println!("      - {issue}");
            }
        }
    }
    println!("=========================================================\n");

    Ok(())
}
