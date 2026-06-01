// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end closed-loop test of the OKX connector through the real engine, with
//! a `MockTransport` fed recorded OKX JSON — no network. Proves OKX
//! `instruments`/`candles`/order payloads normalize into akadro types and flow
//! through the engine. (The live round-trip is the `#[ignore]`d `live_smoke.rs`.)

use akadro_core::{Bar, InstrumentId, Money, OrderRequest, Qty, Side};
use akadro_engine::{Ctx, Engine, Strategy};
use akadro_venue_okx::{
    HttpResponse, InstType, InstrumentMeta, MockTransport, OkxCandleFeed, OkxCatalog, OkxExec,
};

const INSTRUMENTS: &str = r#"{"code":"0","data":[
  {"instId":"BTC-USDT","tickSz":"0.1","lotSz":"0.0001","minSz":"0.0001","state":"live"}
]}"#;

// Three closed 1-minute candles, newest-first (OKX order), open-time ms.
const CANDLES: &str = r#"{"code":"0","data":[
  ["1700000120000","105.0","107.0","104.0","106.0","5","1","1","1"],
  ["1700000060000","100.5","106.0","100.0","105.0","8","1","1","1"],
  ["1700000000000","100.0","101.0","99.0","100.5","10","1","1","1"]
]}"#;

const ORDER_ACK: &str =
    r#"{"code":"0","data":[{"ordId":"312269865356374016","clOrdId":"ak0","sCode":"0","sMsg":""}]}"#;

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
fn okx_payloads_normalize_and_flow_through_the_engine() {
    let catalog = OkxCatalog::from_instruments(INSTRUMENTS, InstType::Spot).unwrap();
    let id = InstrumentId::new(0);
    let (ps, qs) = catalog.scales(id).unwrap();
    assert_eq!((ps, qs), (1, 4));

    let feed = OkxCandleFeed::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: CANDLES.to_string(),
        }]),
        "https://www.okx.com",
        "BTC-USDT",
        id,
        "1m",
        ps,
        qs,
    );

    let exec = OkxExec::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: ORDER_ACK.to_string(),
        }]),
        "https://www.okx.com",
        "key",
        b"secret".to_vec(),
        "pass",
        vec![InstrumentMeta {
            inst_id: "BTC-USDT".into(),
            price_scale: ps,
            qty_scale: qs,
            inst_type: InstType::Spot,
        }],
    )
    .with_timestamp("2023-11-14T22:13:20.000Z");

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
        "all three OKX candles replayed, ascending"
    );
    assert_eq!(report.orders_submitted, 1);
}

#[test]
fn candles_arrive_in_ascending_close_time_order() {
    use akadro_core::DataSource;
    let catalog = OkxCatalog::from_instruments(INSTRUMENTS, InstType::Spot).unwrap();
    let (ps, qs) = catalog.scales(InstrumentId::new(0)).unwrap();
    let mut feed = OkxCandleFeed::new(
        MockTransport::new(vec![HttpResponse {
            status: 200,
            body: CANDLES.to_string(),
        }]),
        "https://www.okx.com",
        "BTC-USDT",
        InstrumentId::new(0),
        "1m",
        ps,
        qs,
    );
    let mut prev = 0;
    let mut n = 0;
    while let Some(akadro_core::Event::Bar(bar)) = feed.next_event() {
        assert!(bar.ts.as_nanos() > prev, "ascending");
        prev = bar.ts.as_nanos();
        n += 1;
    }
    assert_eq!(n, 3);
    assert_eq!(prev, 1_700_000_180_000_000_000); // last open 1700000120000 + 60s, in ns
}
