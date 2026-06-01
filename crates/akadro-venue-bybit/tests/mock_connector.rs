// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end closed-loop test of the Bybit connector through the real engine,
//! with a `MockTransport` fed recorded Bybit v5 JSON — no network.

use akadro_core::{Bar, DataSource, Event, InstrumentId, Money, OrderRequest, Qty, Side};
use akadro_engine::{Ctx, Engine, Strategy};
use akadro_venue_bybit::{
    BybitCatalog, BybitExec, BybitKlineFeed, Category, HttpResponse, InstrumentMeta, MockTransport,
};

const INSTRUMENTS: &str = r#"{"retCode":0,"result":{"list":[
  {"symbol":"BTCUSDT","baseCoin":"BTC","quoteCoin":"USDT","status":"Trading",
   "priceFilter":{"tickSize":"0.1"},
   "lotSizeFilter":{"basePrecision":"0.0001","minOrderAmt":"1"}}
]}}"#;

// Three closed 1-minute klines, newest-first (Bybit order), open-time ms.
const KLINES: &str = r#"{"retCode":0,"result":{"list":[
  ["1700000120000","105.0","107.0","104.0","106.0","5","x"],
  ["1700000060000","100.5","106.0","100.0","105.0","8","x"],
  ["1700000000000","100.0","101.0","99.0","100.5","10","x"]
]}}"#;

const ORDER_ACK: &str =
    r#"{"retCode":0,"retMsg":"OK","result":{"orderId":"abc","orderLinkId":"ak0"}}"#;

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
fn bybit_payloads_normalize_and_flow_through_the_engine() {
    let catalog = BybitCatalog::from_instruments(INSTRUMENTS, Category::Spot).unwrap();
    let id = InstrumentId::new(0);
    let (ps, qs) = catalog.scales(id).unwrap();
    assert_eq!((ps, qs), (1, 4));

    let feed = BybitKlineFeed::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: KLINES.to_string(),
        }]),
        "https://api.bybit.com",
        Category::Spot,
        "BTCUSDT",
        id,
        "1",
        ps,
        qs,
    );

    let exec = BybitExec::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: ORDER_ACK.to_string(),
        }]),
        "https://api.bybit.com",
        "key",
        b"secret".to_vec(),
        vec![InstrumentMeta {
            symbol: "BTCUSDT".into(),
            price_scale: ps,
            qty_scale: qs,
            category: Category::Spot,
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
        "all three Bybit klines replayed, ascending"
    );
    assert_eq!(report.orders_submitted, 1);
}

#[test]
fn klines_arrive_ascending() {
    let catalog = BybitCatalog::from_instruments(INSTRUMENTS, Category::Spot).unwrap();
    let (ps, qs) = catalog.scales(InstrumentId::new(0)).unwrap();
    let mut feed = BybitKlineFeed::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: KLINES.to_string(),
        }]),
        "https://api.bybit.com",
        Category::Spot,
        "BTCUSDT",
        InstrumentId::new(0),
        "1",
        ps,
        qs,
    );
    let mut prev = 0;
    while let Some(Event::Bar(bar)) = feed.next_event() {
        assert!(bar.ts.as_nanos() > prev);
        prev = bar.ts.as_nanos();
    }
    assert_eq!(prev, 1_700_000_180_000_000_000);
}
