// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end closed-loop test of the MEXC connector through the real engine,
//! using a `MockTransport` fed recorded MEXC JSON. This proves MEXC payloads
//! normalize correctly into akadro `Event`s/`AccountEvent`s and flow through the
//! engine — with no network. (The live round-trip is the `#[ignore]`d test in
//! `live_smoke.rs`.)

use akadro_core::{ExecutionClient, InstrumentId, Money, OrderRequest, Qty, Side};
use akadro_engine::{Ctx, Engine, Strategy};
use akadro_venue_mexc::{HttpResponse, MexcCatalog, MexcKlineFeed, MexcSpotExec, MockTransport};

const EXCHANGE_INFO: &str = r#"{"symbols":[
  {"symbol":"BTCUSDT","baseAsset":"BTC","quoteAsset":"USDT",
   "baseAssetPrecision":2,"quoteAssetPrecision":2,
   "baseSizePrecision":"0.01","quoteAmountPrecision":"1","status":"1"}
]}"#;

// Three closed 1-minute bars (closeTime in ms). Prices at scale 2.
const KLINES: &str = r#"[
  [1700000000000,"100.00","101.00","99.00","100.50","10.00",1700000059999,"1005"],
  [1700000060000,"100.50","106.00","100.00","105.00","8.00",1700000119999,"840"],
  [1700000120000,"105.00","107.00","104.00","106.00","5.00",1700000179999,"530"]
]"#;

const ORDER_ACK: &str = r#"{"symbol":"BTCUSDT","orderId":"VENUE-1","transactTime":1700000060500}"#;
const STATUS_NEW: &str = r#"{"status":"NEW","executedQty":"0","cummulativeQuoteQty":"0"}"#;
// Filled 1.00 BTC for 105.00 USDT each -> cummulativeQuoteQty 105.00 at scale 2 = "105.00".
const STATUS_FILLED: &str =
    r#"{"status":"FILLED","executedQty":"1.00","cummulativeQuoteQty":"105.00"}"#;

/// Buys 1.00 BTC (market) on the first bar, once.
struct BuyOnce {
    done: bool,
}
impl Strategy for BuyOnce {
    fn on_bar(&mut self, _bar: akadro_core::Bar, ctx: &mut Ctx<'_>) {
        if !self.done {
            ctx.submit(OrderRequest::market(
                InstrumentId::new(0),
                Side::Buy,
                Qty::from_raw(100),
            ));
            self.done = true;
        }
    }
}

#[test]
fn mexc_payloads_normalize_and_fill_through_the_engine() {
    let catalog = MexcCatalog::from_exchange_info(EXCHANGE_INFO).unwrap();
    assert_eq!(catalog.id_of("BTCUSDT"), Some(InstrumentId::new(0)));
    let (price_scale, qty_scale) = catalog.scales(InstrumentId::new(0)).unwrap();

    // Data feed: one klines fetch returning three bars.
    let feed = MexcKlineFeed::new(
        MockTransport::new(vec![HttpResponse::ok(KLINES)]),
        "https://api.mexc.com",
        "BTCUSDT",
        InstrumentId::new(0),
        "1m",
        price_scale,
        qty_scale,
    );

    // Execution: submit -> ack; then poll NEW, then poll FILLED.
    let mut exec = MexcSpotExec::new(
        MockTransport::new(vec![
            HttpResponse::ok(ORDER_ACK),
            HttpResponse::ok(STATUS_NEW),
            HttpResponse::ok(STATUS_FILLED),
        ]),
        "https://api.mexc.com",
        "test-key",
        b"test-secret".to_vec(),
        catalog.clone(),
    );
    exec.set_clock_ms(1_700_000_060_000);

    let report = Engine::new(
        catalog.specs(),
        Money::from_raw(100_000_000),
        feed,
        exec,
        BuyOnce { done: false },
    )
    .unwrap()
    .run();

    assert_eq!(report.bars_processed, 3, "all three MEXC bars replayed");
    assert_eq!(report.orders_submitted, 1);
    assert_eq!(
        report.fills.len(),
        1,
        "FILLED status produced exactly one fill"
    );

    let fill = report.fills[0];
    // cummulativeQuoteQty 105.00 (raw 10_500 @ price_scale 2), executedQty 1.00
    // (raw 100 @ qty_scale 2). avg price = 10_500 * 10^2 / 100 = 10_500 raw,
    // i.e. 105.00 in price-scale units (the pre-fix code wrongly gave 105 raw).
    assert_eq!(fill.price, akadro_core::Price::from_raw(10_500));
    assert_eq!(fill.qty, Qty::from_raw(100));
    assert_eq!(fill.side, Side::Buy);
    // Modelled taker fee = notional(10_500 * 100 = 1_050_000) * 5bps = 525.
    assert_eq!(fill.fee, Money::from_raw(525));
}

#[test]
fn unsupported_orders_are_rejected_locally_without_a_venue_call() {
    use akadro_core::TriggerBy;
    let catalog = MexcCatalog::from_exchange_info(EXCHANGE_INFO).unwrap();
    // No canned responses: a local rejection must NOT hit the transport.
    let mut exec = MexcSpotExec::new(
        MockTransport::new(vec![]),
        "https://api.mexc.com",
        "k",
        b"s".to_vec(),
        catalog,
    );
    let mut sink: Vec<akadro_core::AccountEvent> = Vec::new();

    // Stop order: unsupported on spot.
    let stop = OrderRequest::stop(
        InstrumentId::new(0),
        Side::Buy,
        Qty::from_raw(100),
        akadro_core::Price::from_raw(100),
        TriggerBy::Last,
    );
    exec.submit(
        akadro_core::ClientOrderId::new(0),
        stop,
        akadro_core::Timestamp::from_nanos(1),
        &mut sink,
    );
    assert!(matches!(
        sink[0],
        akadro_core::AccountEvent::OrderRejected { .. }
    ));
    assert_eq!(exec.pending_count(), 0);

    // reduce_only on spot: unsupported.
    sink.clear();
    let ro =
        OrderRequest::market(InstrumentId::new(0), Side::Sell, Qty::from_raw(100)).reduce_only();
    exec.submit(
        akadro_core::ClientOrderId::new(1),
        ro,
        akadro_core::Timestamp::from_nanos(1),
        &mut sink,
    );
    assert!(matches!(
        sink[0],
        akadro_core::AccountEvent::OrderRejected { .. }
    ));
}

#[test]
fn venue_error_becomes_order_rejected() {
    let catalog = MexcCatalog::from_exchange_info(EXCHANGE_INFO).unwrap();
    let mut exec = MexcSpotExec::new(
        MockTransport::new(vec![HttpResponse::ok(
            r#"{"code":700002,"msg":"Signature for this request is not valid"}"#,
        )]),
        "https://api.mexc.com",
        "k",
        b"s".to_vec(),
        catalog,
    );
    let mut sink: Vec<akadro_core::AccountEvent> = Vec::new();
    let o = OrderRequest::market(InstrumentId::new(0), Side::Buy, Qty::from_raw(100));
    exec.submit(
        akadro_core::ClientOrderId::new(0),
        o,
        akadro_core::Timestamp::from_nanos(1),
        &mut sink,
    );
    assert!(matches!(
        sink[0],
        akadro_core::AccountEvent::OrderRejected {
            reason: akadro_core::RejectReason::VenueRejected,
            ..
        }
    ));
    assert_eq!(exec.pending_count(), 0);
}
