// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end closed-loop test of the KuCoin connector through the real engine,
//! with a `MockTransport` fed recorded KuCoin JSON — no network.

use akadro_core::{Bar, DataSource, Event, InstrumentId, Money, OrderRequest, Qty, Side};
use akadro_engine::{Ctx, Engine, Strategy};
use akadro_venue_kucoin::{
    HttpResponse, InstrumentMeta, KucoinCandleFeed, KucoinCatalog, KucoinExec, MockTransport,
};

const SYMBOLS: &str = r#"{"code":"200000","data":[
  {"symbol":"BTC-USDT","baseCurrency":"BTC","quoteCurrency":"USDT","baseIncrement":"0.0001","priceIncrement":"0.1","quoteMinSize":"0.1","enableTrading":true}
]}"#;

// Three closed 1-minute candles: [time_s, open, close, high, low, volume, turnover], newest-first.
const CANDLES: &str = r#"{"code":"200000","data":[
  ["1700000120","105.0","106.0","107.0","104.0","5","x"],
  ["1700000060","100.5","105.0","106.0","100.0","8","x"],
  ["1700000000","100.0","100.5","101.0","99.0","10","x"]
]}"#;

const ORDER_ACK: &str = r#"{"code":"200000","data":{"orderId":"5bd6e9286d99522a52e458de"}}"#;

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
fn kucoin_payloads_normalize_and_flow_through_the_engine() {
    let catalog = KucoinCatalog::from_symbols(SYMBOLS).unwrap();
    let id = InstrumentId::new(0);
    let (ps, qs) = catalog.scales(id).unwrap();
    assert_eq!((ps, qs), (1, 4));

    let feed = KucoinCandleFeed::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: CANDLES.to_string(),
        }]),
        "https://api.kucoin.com",
        "BTC-USDT",
        id,
        "1min",
        ps,
        qs,
    );

    let exec = KucoinExec::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: ORDER_ACK.to_string(),
        }]),
        "https://api.kucoin.com",
        "key",
        b"secret".to_vec(),
        "phrase",
        vec![InstrumentMeta {
            symbol: "BTC-USDT".into(),
            price_scale: ps,
            qty_scale: qs,
        }],
    )
    .with_timestamp_ms(1_700_000_000_000);

    let report = Engine::new(
        catalog.specs(),
        Money::from_raw(100_000_000),
        feed,
        exec,
        BuyOnce { done: false },
    )
    .unwrap()
    .run();

    assert_eq!(
        report.bars_processed, 3,
        "all three KuCoin candles replayed, ascending"
    );
    assert_eq!(report.orders_submitted, 1);
}

#[test]
fn candles_ascending_with_correct_ohlc_mapping() {
    let catalog = KucoinCatalog::from_symbols(SYMBOLS).unwrap();
    let (ps, qs) = catalog.scales(InstrumentId::new(0)).unwrap();
    let mut feed = KucoinCandleFeed::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: CANDLES.to_string(),
        }]),
        "https://api.kucoin.com",
        "BTC-USDT",
        InstrumentId::new(0),
        "1min",
        ps,
        qs,
    );
    let Some(Event::Bar(first)) = feed.next_event() else {
        panic!("expected a bar");
    };
    // First (earliest) candle: time 1700000000 + 60s. close/high/low correctly mapped.
    assert_eq!(first.ts.as_nanos(), 1_700_000_060_000_000_000);
    assert_eq!(first.high, akadro_core::Price::from_raw(1010)); // 101.0 @ 1
    assert_eq!(first.low, akadro_core::Price::from_raw(990)); // 99.0
    assert_eq!(first.close, akadro_core::Price::from_raw(1005)); // 100.5
}
