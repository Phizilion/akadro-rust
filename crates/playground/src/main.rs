// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Playground: run a simple SMA-crossover strategy over **real** MEXC klines.
//!
//! The live-data sibling of the `two_week_15m_backtest` integration test. It
//! pulls genuine historical candles from `api.mexc.com` through the
//! `akadro-venue-mexc` connector and runs them through the real engine, so the
//! numbers reflect actual market action.
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

use akadro::analytics::{PerformanceReport, TradeStats};
use akadro::backtest::{diff_reports, replay_trades};
use akadro::data::load_or_cache;
use akadro::prelude::*;
use akadro_venue_mexc::{MexcCatalog, MexcKlineFeed, ReqwestTransport};

type Err = Box<dyn Error>;

/// Directory (relative to the working dir) where saved `RunReport`s land. It is
/// created on demand and git-ignored — see `.gitignore`.
const RUN_ARTEFACTS_DIR: &str = "run_artefacts";

// --- Configuration -------------------------------------------------------------

/// Every tunable, resolved from defaults / config file / CLI at runtime.
#[derive(Debug, Clone)]
struct Config {
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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            base_url: "https://api.mexc.com".to_string(),
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
        }
    }
}

/// One bar length in milliseconds for a MEXC interval string.
fn interval_ms(iv: &str) -> Result<i64, Err> {
    Ok(match iv {
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
        "playground — run the SMA-crossover backtest over real MEXC data.\n\n\
         Resolution: defaults < playground.toml (cwd or --config) < --flag value.\n\n\
         Flags (all optional):\n  \
           --symbol <S>        venue symbol            (default BTCUSDT)\n  \
           --interval <I>      1m|5m|15m|30m|1h|4h|1d  (default 5m)\n  \
           --start <D>         YYYY-MM-DD or epoch-ms  (default 2025-05-01)\n  \
           --end <D>           YYYY-MM-DD or epoch-ms  (default 2026-05-01)\n  \
           --fast <N>          fast SMA window (bars)  (default 48)\n  \
           --slow <N>          slow SMA window (bars)  (default 192)\n  \
           --fee-bps <N>       taker fee, basis points (default 5)\n  \
           --cash <N>          starting cash, quote    (default 1000000)\n  \
           --deploy-pct <N>    %% of capital deployed  (default 90)\n  \
           --enforce-cash <B>  true|false cash guard   (default true)\n  \
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

#[allow(clippy::too_many_lines)] // linear load → fetch → run → report script
fn main() -> Result<(), Err> {
    let cfg = load_config()?;
    let bar_ms = interval_ms(&cfg.interval)?;
    let periods_per_year = 365.0 * 86_400_000.0 / bar_ms as f64;
    eprintln!("config: {cfg:?}");

    let mut transport = ReqwestTransport::new()?;

    // 1. Resolve the real instrument via the connector's exchangeInfo helper.
    eprintln!("Fetching exchangeInfo for {} ...", cfg.symbol);
    let catalog = MexcCatalog::fetch(&mut transport, &cfg.base_url, &cfg.symbol)?;
    let instrument = catalog
        .id_of(&cfg.symbol)
        .ok_or("symbol not present / not tradable")?;
    let (price_scale, qty_scale) = catalog.scales(instrument).ok_or("no scales")?;
    let money_scale = price_scale + qty_scale;

    // 2. Download-and-cache (or load from cache) the candles — the library does
    //    the pagination and the on-disk caching; the user just asks for a range.
    // Cached under a local `market_data/` dir (git-ignored), keyed by the full
    // range so different windows don't collide on disk.
    let cache_dir = std::path::Path::new("market_data");
    std::fs::create_dir_all(cache_dir)?;
    let cache_path = cache_dir.join(format!(
        "{}_{}_{}_{}.feather",
        cfg.symbol, cfg.interval, cfg.start, cfg.end
    ));
    eprintln!(
        "Loading {} {} klines (cache or download) ...",
        cfg.symbol, cfg.interval
    );
    let (base_url, symbol, interval) = (
        cfg.base_url.clone(),
        cfg.symbol.clone(),
        cfg.interval.clone(),
    );
    let (start, end) = (cfg.start, cfg.end);
    let bars = load_or_cache(&cache_path, instrument, price_scale, qty_scale, move || {
        let t = ReqwestTransport::new().expect("transport");
        MexcKlineFeed::new(
            t,
            base_url,
            symbol,
            instrument,
            interval,
            price_scale,
            qty_scale,
        )
        .with_range(start, end)
        .with_limit(1000)
    })?;
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
    let strategy = SmaCrossover::new(instrument, cfg.fast, cfg.slow, cfg.deploy_pct);

    let mut exchange = SimulatedExchange::new(catalog.specs().to_vec(), cfg.fee_bps);
    if cfg.enforce_cash {
        exchange = exchange.with_starting_cash(Money::from_raw(cash_raw));
    }
    let engine = Engine::new(
        catalog.specs(),
        Money::from_raw(cash_raw),
        HistoricalFeed::from_bars(bars.clone()),
        exchange,
        strategy,
    )?;

    // 4. Time ONLY the backtest (engine loop + fills + equity marking).
    let t0 = std::time::Instant::now();
    let report = engine.run();
    let backtest_time = t0.elapsed();

    // 5. The candles are already cached on disk by `load_or_cache` above.
    let on_disk = std::fs::metadata(&cache_path)?.len();

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
        "\n=========  {} {} — real MEXC backtest  =========",
        cfg.symbol, cfg.interval
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
    let secs = backtest_time.as_secs_f64().max(1e-9);
    println!(
        "  backtest runtime : {backtest_time:?}  ({:.0} bars/sec; excludes download & caching)",
        bars.len() as f64 / secs
    );
    println!(
        "  starting equity  : ~{:.2}  ->  final ~{:.2}  ({ret_pct:+.2}%)",
        quote(cash_raw, money_scale),
        quote(final_eq, money_scale)
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
        "  data on disk     : {} bytes ({:.0} KiB) at {}",
        on_disk,
        on_disk as f64 / 1024.0,
        cache_path.display()
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
        // report through the real engine + simulated exchange — with NO strategy
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
