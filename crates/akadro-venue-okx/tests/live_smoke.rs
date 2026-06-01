// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Credential-/network-gated live smoke test (the documented OPEN LOOP). `#[ignore]`d
//! so CI never runs it; run with:
//!
//! ```text
//! cargo test -p akadro-venue-okx --features net -- --ignored
//! ```
//!
//! The first hits the **public** instruments + candles endpoints (no credentials),
//! confirming the real transport + parsing; the second proves Base64 HMAC-SHA256
//! signing is accepted via a signed `GET /api/v5/account/balance`, gated on
//! `OKX_API_KEY` / `OKX_API_SECRET` / `OKX_API_PASSPHRASE`.
#![cfg(feature = "net")]

use akadro_venue_okx::{
    HttpRequest, InstType, Method, OkxCatalog, ReqwestTransport, Transport, iso_timestamp,
    parse_candles, sign,
};

const BASE: &str = "https://www.okx.com";

#[test]
#[ignore = "requires network access to www.okx.com"]
fn live_public_instruments_and_candles() {
    let mut t = ReqwestTransport::new().expect("client");
    let inst = t
        .send(&HttpRequest {
            method: Method::Get,
            url: format!("{BASE}/api/v5/public/instruments?instType=SPOT"),
            body: None,
            headers: Vec::new(),
        })
        .expect("instruments");
    assert!(inst.is_success(), "HTTP {}", inst.status);
    let cat = OkxCatalog::from_instruments(&inst.body, InstType::Spot).expect("parse");
    let id = cat.id_of("BTC-USDT").expect("BTC-USDT");
    let (ps, qs) = cat.scales(id).expect("scales");

    let c = t
        .send(&HttpRequest {
            method: Method::Get,
            url: format!("{BASE}/api/v5/market/candles?instId=BTC-USDT&bar=1m&limit=5"),
            body: None,
            headers: Vec::new(),
        })
        .expect("candles");
    assert!(c.is_success(), "HTTP {}", c.status);
    let bars = parse_candles(&c.body, id, ps, qs, "1m").expect("parse candles");
    assert!(!bars.is_empty());
}

#[test]
#[ignore = "requires OKX_API_KEY/SECRET/PASSPHRASE + network"]
fn live_signed_balance_query() {
    let (Ok(key), Ok(secret), Ok(pass)) = (
        std::env::var("OKX_API_KEY"),
        std::env::var("OKX_API_SECRET"),
        std::env::var("OKX_API_PASSPHRASE"),
    ) else {
        eprintln!("skipping: OKX_API_KEY/SECRET/PASSPHRASE not set");
        return;
    };
    let ts = iso_timestamp();
    let path = "/api/v5/account/balance";
    let prehash = format!("{ts}GET{path}");
    let signature = sign(secret.as_bytes(), &prehash);
    let mut t = ReqwestTransport::new().expect("client");
    let resp = t
        .send(&HttpRequest {
            method: Method::Get,
            url: format!("{BASE}{path}"),
            body: None,
            headers: vec![
                ("OK-ACCESS-KEY".into(), key),
                ("OK-ACCESS-SIGN".into(), signature),
                ("OK-ACCESS-TIMESTAMP".into(), ts),
                ("OK-ACCESS-PASSPHRASE".into(), pass),
            ],
        })
        .expect("balance");
    assert!(
        resp.is_success() && resp.body.contains("\"code\":\"0\""),
        "signed request rejected (HTTP {}): {}",
        resp.status,
        resp.body
    );
}
