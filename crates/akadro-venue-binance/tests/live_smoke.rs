// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Credential-/network-gated live smoke test (the documented OPEN LOOP).
//!
//! The only tests that touch the real Binance API. `#[ignore]`d so CI and
//! `cargo test` never run them; run deliberately with:
//!
//! ```text
//! cargo test -p akadro-venue-binance --features net -- --ignored
//! ```
//!
//! The first hits the *public* klines endpoint (no credentials), confirming the
//! real transport + parsing work end to end. The second proves our HMAC-SHA256
//! signing is accepted by the live server via a signed `GET /api/v3/account`,
//! gated on `BINANCE_API_KEY` / `BINANCE_API_SECRET` (read-only is sufficient).
#![cfg(feature = "net")]

use akadro_venue_binance::{
    BinanceCatalog, FUTURES_BASE_URL, HttpRequest, Method, ReqwestTransport, Transport,
    parse_funding_rate, parse_klines, sign, unix_millis,
};

#[test]
#[ignore = "requires network access to api.binance.com"]
fn live_public_exchange_info_and_klines() {
    let mut transport = ReqwestTransport::new().expect("build client");
    let info = transport
        .send(&HttpRequest {
            method: Method::Get,
            url: "https://api.binance.com/api/v3/exchangeInfo?symbol=BTCUSDT".to_owned(),
            api_key: None,
        })
        .expect("send exchangeInfo");
    assert!(info.is_success(), "HTTP {}", info.status);
    let catalog = BinanceCatalog::from_exchange_info(&info.body).expect("parse exchangeInfo");
    let id = catalog.id_of("BTCUSDT").expect("BTCUSDT present");
    let (ps, qs) = catalog.scales(id).expect("scales");

    let kl = transport
        .send(&HttpRequest {
            method: Method::Get,
            url: "https://api.binance.com/api/v3/klines?symbol=BTCUSDT&interval=1m&limit=5"
                .to_owned(),
            api_key: None,
        })
        .expect("send klines");
    assert!(kl.is_success(), "HTTP {}", kl.status);
    let bars = parse_klines(&kl.body, id, ps, qs).expect("parse klines");
    assert!(!bars.is_empty(), "expected at least one bar");
}

#[test]
#[ignore = "requires network access to fapi.binance.com"]
fn live_futures_exchange_info_and_funding() {
    let mut transport = ReqwestTransport::new().expect("build client");
    let info = transport
        .send(&HttpRequest {
            method: Method::Get,
            url: format!("{FUTURES_BASE_URL}/fapi/v1/exchangeInfo"),
            api_key: None,
        })
        .expect("send futures exchangeInfo");
    assert!(info.is_success(), "HTTP {}", info.status);
    let catalog =
        BinanceCatalog::from_futures_exchange_info(&info.body).expect("parse futures exchangeInfo");
    assert!(
        catalog.id_of("BTCUSDT").is_some(),
        "expected BTCUSDT perpetual"
    );

    let fr = transport
        .send(&HttpRequest {
            method: Method::Get,
            url: format!("{FUTURES_BASE_URL}/fapi/v1/fundingRate?symbol=BTCUSDT&limit=10"),
            api_key: None,
        })
        .expect("send fundingRate");
    assert!(fr.is_success(), "HTTP {}", fr.status);
    let schedule = parse_funding_rate(&fr.body).expect("parse fundingRate");
    assert!(!schedule.is_empty(), "expected funding history");
}

#[test]
#[ignore = "requires BINANCE_API_KEY/BINANCE_API_SECRET + network"]
fn live_signed_account_query() {
    let (Ok(key), Ok(secret)) = (
        std::env::var("BINANCE_API_KEY"),
        std::env::var("BINANCE_API_SECRET"),
    ) else {
        eprintln!("skipping: BINANCE_API_KEY/BINANCE_API_SECRET not set");
        return;
    };
    let params = format!("timestamp={}&recvWindow=5000", unix_millis());
    let signature = sign(secret.as_bytes(), &params);
    let mut transport = ReqwestTransport::new().expect("build client");
    let resp = transport
        .send(&HttpRequest {
            method: Method::Get,
            url: format!("https://api.binance.com/api/v3/account?{params}&signature={signature}"),
            api_key: Some(key),
        })
        .expect("send signed account request");
    assert!(
        resp.is_success(),
        "signed request rejected (HTTP {}): {} — signing or clock is wrong",
        resp.status,
        resp.body
    );
    assert!(
        resp.body.contains("balances"),
        "unexpected account body: {}",
        resp.body
    );
}
