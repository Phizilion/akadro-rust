// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end closed-loop test of the Binance **USDⓈ-M futures** connector: a
//! futures kline feed and an open-interest signal feed are merged
//! (`akadro_data::MergeSource`) into the engine's single source, executed against
//! the `SimulatedExchange` with a funding schedule parsed from Binance's
//! `fundingRate` history. It proves (a) futures klines normalize and replay, (b)
//! the OI signal interleaves correctly and is readable via `ctx.signal`, and (c)
//! funding parsed from Binance JSON actually accrues on the held perp position
//! (fee-free run → any `total_fees` is funding).

use akadro_backtest::SimulatedExchange;
use akadro_core::{Bar, InstrumentId, Money, OrderRequest, Qty, Side, signal_channel};
use akadro_data::MergeSource;
use akadro_engine::{Ctx, Engine, Strategy};
use akadro_venue_binance::{
    BinanceCatalog, BinanceKlineFeed, BinanceSignalFeed, FUTURES_BASE_URL, HttpResponse,
    MockTransport, parse_funding_rate,
};

const FUTURES_INFO: &str = r#"{"symbols":[
  {"symbol":"BTCUSDT","baseAsset":"BTC","quoteAsset":"USDT","status":"TRADING",
   "contractType":"PERPETUAL","filters":[
     {"filterType":"PRICE_FILTER","tickSize":"0.10"},
     {"filterType":"LOT_SIZE","stepSize":"0.001"},
     {"filterType":"MIN_NOTIONAL","notional":"5"}]}
]}"#;

// Three closed 1-minute futures bars (closeTime ms at index 6), prices @ scale 1.
const KLINES: &str = r#"[
  [1700000000000,"100.0","101.0","99.0","100.5","10.000",1700000059999,"x",10,"y","z","0"],
  [1700000060000,"100.5","106.0","100.0","105.0","8.000",1700000119999,"x",8,"y","z","0"],
  [1700000120000,"105.0","107.0","104.0","106.0","5.000",1700000179999,"x",5,"y","z","0"]
]"#;

// Open interest sampled at the same close timestamps as the bars.
const OI: &str = r#"[
  {"symbol":"BTCUSDT","sumOpenInterest":"10","sumOpenInterestValue":"1","timestamp":1700000059999},
  {"symbol":"BTCUSDT","sumOpenInterest":"20","sumOpenInterestValue":"1","timestamp":1700000119999},
  {"symbol":"BTCUSDT","sumOpenInterest":"30","sumOpenInterestValue":"1","timestamp":1700000179999}
]"#;

// A positive funding rate effective from the epoch (1% per interval = 100 bps).
const FUNDING: &str = r#"[{"symbol":"BTCUSDT","fundingTime":0,"fundingRate":"0.01000000"}]"#;

/// Buys one contract on the first bar, and records the open interest it observes
/// each bar through `ctx.annotate` so the harness can assert what it saw.
struct OiReader {
    bought: bool,
}
impl Strategy for OiReader {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        if let Some(oi) = ctx
            .signal(InstrumentId::new(0), signal_channel::OPEN_INTEREST)
            .and_then(|s| s.latest())
        {
            ctx.annotate(format!("oi={oi}"));
        }
        if !self.bought {
            ctx.submit(OrderRequest::market(
                InstrumentId::new(0),
                Side::Buy,
                Qty::from_raw(1),
            ));
            self.bought = true;
        }
    }
}

#[test]
fn futures_klines_oi_and_funding_flow_through_the_engine() {
    let catalog = BinanceCatalog::from_futures_exchange_info(FUTURES_INFO).unwrap();
    let id = InstrumentId::new(0);
    let (ps, qs) = catalog.scales(id).unwrap();

    let klines = BinanceKlineFeed::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: KLINES.to_string(),
        }]),
        FUTURES_BASE_URL,
        "BTCUSDT",
        id,
        "1m",
        ps,
        qs,
    )
    .for_futures();

    let oi = BinanceSignalFeed::open_interest(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: OI.to_string(),
        }]),
        FUTURES_BASE_URL,
        "BTCUSDT",
        id,
        "1m",
        500,
    );

    // Signals first so an equal-timestamp OI point is observed before that bar's
    // `on_bar` (deterministic tie-break by child index).
    let feed = MergeSource::new(vec![Box::new(oi), Box::new(klines)]);

    // Fee-free, so any total_fees comes purely from funding.
    let schedule = parse_funding_rate(FUNDING).unwrap();
    let exchange =
        SimulatedExchange::new(catalog.specs().to_vec(), 0).with_funding_schedule(id, 1, schedule);

    let report = Engine::new(
        catalog.specs(),
        Money::from_raw(1_000_000_000),
        feed,
        exchange,
        OiReader { bought: false },
    )
    .unwrap()
    .run();

    assert_eq!(report.bars_processed, 3, "all three futures bars replayed");
    assert_eq!(report.fills.len(), 1, "the market buy filled once");

    // The OI signal interleaved correctly and was readable each bar.
    let seen: Vec<&str> = report.annotations.iter().map(|(_, s)| s.as_str()).collect();
    assert_eq!(seen, vec!["oi=10", "oi=20", "oi=30"]);

    // Funding accrued on the held long across bars 2 and 3 (fee_bps = 0, so this
    // is purely the funding charge parsed from Binance JSON).
    assert!(
        report.funding_net.raw() > 0,
        "expected funding to accrue, got {:?}",
        report.funding_net
    );
}
