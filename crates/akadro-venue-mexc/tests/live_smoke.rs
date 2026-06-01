// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Credential-/network-gated live smoke test (the documented OPEN LOOP).
//!
//! This is the only test that touches the real MEXC API. It is `#[ignore]`d so
//! CI and `cargo test` never run it; run it deliberately with:
//!
//! ```text
//! cargo test -p akadro-venue-mexc --features net -- --ignored
//! ```
//!
//! It hits only the *public* klines endpoint (no credentials needed), confirming
//! the real transport + parsing work end to end against live data. Authenticated
//! order flow should be exercised on MEXC testnet with `MEXC_API_KEY` /
//! `MEXC_API_SECRET` before trusting live execution.
#![cfg(feature = "net")]

use akadro_core::InstrumentId;
use akadro_venue_mexc::{
    HttpRequest, Method, ReqwestTransport, Transport, parse_contract_detail, parse_klines, signed,
    unix_millis,
};

#[test]
#[ignore = "requires network access to api.mexc.com"]
fn live_public_klines_parse() {
    let mut transport = ReqwestTransport::new().expect("build client");
    let req = HttpRequest {
        method: Method::Get,
        url: "https://api.mexc.com/api/v3/klines?symbol=BTCUSDT&interval=1m&limit=5".to_owned(),
        api_key: None,
        body: None,
    };
    let resp = transport.send(&req).expect("send klines request");
    assert!(resp.is_success(), "HTTP {}", resp.status);
    // BTCUSDT spot: price scale ~2, qty scale ~6 (approximate; the real catalogue
    // comes from exchangeInfo). Parsing must succeed and yield bars.
    let bars = parse_klines(&resp.body, InstrumentId::new(0), 2, 6).expect("parse klines");
    assert!(!bars.is_empty(), "expected at least one bar");
}

/// Proves our HMAC-SHA256 signing is accepted by the REAL MEXC server: a signed
/// `GET /api/v3/account` must return 200 with a balances array. Requires
/// `MEXC_API_KEY` / `MEXC_API_SECRET` (read-only is sufficient); skips if absent.
#[test]
#[ignore = "requires MEXC_API_KEY/MEXC_API_SECRET + network"]
fn live_signed_account_query() {
    let (Ok(key), Ok(secret)) = (
        std::env::var("MEXC_API_KEY"),
        std::env::var("MEXC_API_SECRET"),
    ) else {
        eprintln!("skipping: MEXC_API_KEY/MEXC_API_SECRET not set");
        return;
    };

    let req = signed(
        Method::Get,
        "/api/v3/account",
        &[],
        secret.as_bytes(),
        unix_millis(),
        Some(5000),
    );
    let mut transport = ReqwestTransport::new().expect("build client");
    let http = HttpRequest {
        method: Method::Get,
        url: format!("https://api.mexc.com{}?{}", req.path, req.query),
        api_key: Some(key),
        body: None,
    };
    let resp = transport.send(&http).expect("send signed account request");
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

/// Validate FUTURES instrument parsing against the live public `contract/detail`.
#[test]
#[ignore = "requires network access to contract.mexc.com"]
fn live_futures_contract_detail() {
    let mut transport = ReqwestTransport::new().expect("build client");
    let req = HttpRequest {
        method: Method::Get,
        url: "https://contract.mexc.com/api/v1/contract/detail".to_owned(),
        api_key: None,
        body: None,
    };
    let resp = transport.send(&req).expect("send contract/detail");
    assert!(resp.is_success(), "HTTP {}", resp.status);
    let contracts = parse_contract_detail(&resp.body).expect("parse contracts");
    assert!(
        !contracts.is_empty(),
        "expected at least one futures contract"
    );
    assert!(
        contracts.iter().any(|c| c.symbol == "BTC_USDT"),
        "expected BTC_USDT perp"
    );
}
