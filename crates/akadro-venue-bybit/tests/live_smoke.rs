// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Credential-/network-gated live smoke test (the documented OPEN LOOP). `#[ignore]`d
//! so CI never runs it; run with:
//!
//! ```text
//! cargo test -p akadro-venue-bybit --features net -- --ignored
//! ```
//!
//! The first hits the **public** instruments + kline endpoints (no credentials);
//! the second proves the v5 hex HMAC signing is accepted via a signed
//! `GET /v5/account/wallet-balance`, gated on `BYBIT_API_KEY` / `BYBIT_API_SECRET`.
#![cfg(feature = "net")]

use akadro_venue_bybit::{
    BybitCatalog, Category, HttpRequest, Method, ReqwestTransport, Transport, parse_klines, sign,
    unix_millis,
};

const BASE: &str = "https://api.bybit.com";

#[test]
#[ignore = "requires network access to api.bybit.com"]
fn live_public_instruments_and_kline() {
    let mut t = ReqwestTransport::new().expect("client");
    let inst = t
        .send(&HttpRequest {
            method: Method::Get,
            url: format!("{BASE}/v5/market/instruments-info?category=spot&symbol=BTCUSDT"),
            body: None,
            headers: Vec::new(),
        })
        .expect("instruments");
    assert!(inst.is_success(), "HTTP {}", inst.status);
    let cat = BybitCatalog::from_instruments(&inst.body, Category::Spot).expect("parse");
    let id = cat.id_of("BTCUSDT").expect("BTCUSDT");
    let (ps, qs) = cat.scales(id).expect("scales");

    let k = t
        .send(&HttpRequest {
            method: Method::Get,
            url: format!("{BASE}/v5/market/kline?category=spot&symbol=BTCUSDT&interval=1&limit=5"),
            body: None,
            headers: Vec::new(),
        })
        .expect("kline");
    assert!(k.is_success(), "HTTP {}", k.status);
    let bars = parse_klines(&k.body, id, ps, qs, "1").expect("parse kline");
    assert!(!bars.is_empty());
}

#[test]
#[ignore = "requires BYBIT_API_KEY/BYBIT_API_SECRET + network"]
fn live_signed_wallet_balance() {
    let (Ok(key), Ok(secret)) = (
        std::env::var("BYBIT_API_KEY"),
        std::env::var("BYBIT_API_SECRET"),
    ) else {
        eprintln!("skipping: BYBIT_API_KEY/BYBIT_API_SECRET not set");
        return;
    };
    let ts = unix_millis().to_string();
    let recv = "5000";
    let query = "accountType=UNIFIED";
    // GET signs over timestamp + api_key + recv_window + queryString.
    let signature = sign(secret.as_bytes(), &format!("{ts}{key}{recv}{query}"));
    let mut t = ReqwestTransport::new().expect("client");
    let resp = t
        .send(&HttpRequest {
            method: Method::Get,
            url: format!("{BASE}/v5/account/wallet-balance?{query}"),
            body: None,
            headers: vec![
                ("X-BAPI-API-KEY".into(), key),
                ("X-BAPI-SIGN".into(), signature),
                ("X-BAPI-TIMESTAMP".into(), ts),
                ("X-BAPI-RECV-WINDOW".into(), recv.into()),
            ],
        })
        .expect("wallet-balance");
    assert!(
        resp.is_success() && resp.body.contains("\"retCode\":0"),
        "signed request rejected (HTTP {}): {}",
        resp.status,
        resp.body
    );
}
