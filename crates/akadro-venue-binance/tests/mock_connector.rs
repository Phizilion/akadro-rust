// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end closed-loop test of the Binance spot connector through the real
//! engine, using a `MockTransport` fed recorded Binance JSON — no network. This
//! proves Binance `exchangeInfo`/`klines`/order payloads normalize into akadro
//! types and flow through the engine. (The live round-trip is the `#[ignore]`d
//! test in `live_smoke.rs`.)
//!
//! A spot REST connector submits orders but does **not** poll for fills: live
//! fills arrive on the user-data WebSocket stream (`akadro-live`). So this test
//! asserts the data path and that the order is submitted + accepted; for
//! simulated *fills* a strategy pairs the Binance kline feed with the
//! backtest `SimulatedExchange`.

use akadro_core::{Bar, InstrumentId, Money, OrderRequest, Qty, Side};
use akadro_engine::{Ctx, Engine, Strategy};
use akadro_venue_binance::{
    BinanceCatalog, BinanceKlineFeed, BinanceSpotExec, HttpResponse, MockTransport, SymbolMeta,
};

const EXCHANGE_INFO: &str = r#"{"symbols":[
  {"symbol":"BTCUSDT","baseAsset":"BTC","quoteAsset":"USDT","status":"TRADING","filters":[
    {"filterType":"PRICE_FILTER","tickSize":"0.01000000"},
    {"filterType":"LOT_SIZE","stepSize":"0.00100000"},
    {"filterType":"NOTIONAL","minNotional":"5.00000000"}]}
]}"#;

// Three closed 1-minute bars; closeTime in ms at index 6, prices @ scale 2.
const KLINES: &str = r#"[
  [1700000000000,"100.00","101.00","99.00","100.50","10.000",1700000059999,"x",10,"y","z","0"],
  [1700000060000,"100.50","106.00","100.00","105.00","8.000",1700000119999,"x",8,"y","z","0"],
  [1700000120000,"105.00","107.00","104.00","106.00","5.000",1700000179999,"x",5,"y","z","0"]
]"#;

const ORDER_ACK: &str = r#"{"symbol":"BTCUSDT","orderId":28,"status":"NEW"}"#;

/// Buys (market) once on the first bar.
struct BuyOnce {
    done: bool,
}
impl Strategy for BuyOnce {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        if !self.done {
            ctx.submit(OrderRequest::market(
                InstrumentId::new(0),
                Side::Buy,
                Qty::from_raw(1),
            ));
            self.done = true;
        }
    }
}

#[test]
fn binance_payloads_normalize_and_flow_through_the_engine() {
    let catalog = BinanceCatalog::from_exchange_info(EXCHANGE_INFO).unwrap();
    assert_eq!(catalog.id_of("BTCUSDT"), Some(InstrumentId::new(0)));
    let (price_scale, qty_scale) = catalog.scales(InstrumentId::new(0)).unwrap();
    assert_eq!((price_scale, qty_scale), (2, 3));

    let feed = BinanceKlineFeed::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: KLINES.to_string(),
        }]),
        "https://api.binance.com",
        "BTCUSDT",
        InstrumentId::new(0),
        "1m",
        price_scale,
        qty_scale,
    );

    let exec = BinanceSpotExec::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: ORDER_ACK.to_string(),
        }]),
        "https://api.binance.com",
        "test-key",
        b"test-secret".to_vec(),
        vec![SymbolMeta {
            symbol: "BTCUSDT".into(),
            price_scale,
            qty_scale,
        }],
    );

    let report = Engine::new(
        catalog.specs(),
        Money::from_raw(100_000_000),
        feed,
        exec,
        BuyOnce { done: false },
    )
    .unwrap()
    .run();

    assert_eq!(report.bars_processed, 3, "all three Binance bars replayed");
    assert_eq!(report.orders_submitted, 1);
    // A REST connector does not poll fills (they arrive on the user-data WS), so
    // no fill is recorded here — the order was submitted and accepted (asserted in
    // the unit tests). The first replayed bar's OHLCV normalized correctly:
}

#[test]
fn first_bar_normalized_correctly() {
    use akadro_core::DataSource;
    let catalog = BinanceCatalog::from_exchange_info(EXCHANGE_INFO).unwrap();
    let (ps, qs) = catalog.scales(InstrumentId::new(0)).unwrap();
    let mut feed = BinanceKlineFeed::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: KLINES.to_string(),
        }]),
        "https://api.binance.com",
        "BTCUSDT",
        InstrumentId::new(0),
        "1m",
        ps,
        qs,
    );
    let akadro_core::Event::Bar(bar) = feed.next_event().unwrap() else {
        panic!("expected a bar");
    };
    assert_eq!(bar.open, akadro_core::Price::from_raw(10_000)); // 100.00 @ 2
    assert_eq!(bar.high, akadro_core::Price::from_raw(10_100));
    assert_eq!(bar.close, akadro_core::Price::from_raw(10_050));
    assert_eq!(bar.volume, Qty::from_raw(10_000)); // 10.000 @ qty_scale 3
    assert_eq!(bar.ts.as_nanos(), 1_700_000_059_999_000_000); // closeTime → ns
}
