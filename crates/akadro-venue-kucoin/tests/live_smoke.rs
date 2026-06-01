// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Credential-/network-gated live smoke test (the documented OPEN LOOP). `#[ignore]`d
//! so CI never runs it; run with:
//!
//! ```text
//! cargo test -p akadro-venue-kucoin --features net -- --ignored
//! ```
//!
//! The first hits the **public** symbols + candles endpoints (no credentials); the
//! second proves Base64 HMAC signing + the v2-encrypted passphrase are accepted via
//! a signed `GET /api/v1/accounts`, gated on `KUCOIN_API_KEY` / `KUCOIN_API_SECRET`
//! / `KUCOIN_API_PASSPHRASE`.
#![cfg(feature = "net")]

use akadro_venue_kucoin::{
    HttpRequest, KucoinCatalog, Method, ReqwestTransport, Transport, encrypt_passphrase,
    parse_candles, sign, unix_millis,
};

const BASE: &str = "https://api.kucoin.com";

#[test]
#[ignore = "requires network access to api.kucoin.com"]
fn live_public_symbols_and_candles() {
    let mut t = ReqwestTransport::new().expect("client");
    let sym = t
        .send(&HttpRequest {
            method: Method::Get,
            url: format!("{BASE}/api/v1/symbols"),
            body: None,
            headers: Vec::new(),
        })
        .expect("symbols");
    assert!(sym.is_success(), "HTTP {}", sym.status);
    let cat = KucoinCatalog::from_symbols(&sym.body).expect("parse");
    let id = cat.id_of("BTC-USDT").expect("BTC-USDT");
    let (ps, qs) = cat.scales(id).expect("scales");

    let c = t
        .send(&HttpRequest {
            method: Method::Get,
            url: format!("{BASE}/api/v1/market/candles?type=1min&symbol=BTC-USDT"),
            body: None,
            headers: Vec::new(),
        })
        .expect("candles");
    assert!(c.is_success(), "HTTP {}", c.status);
    let bars = parse_candles(&c.body, id, ps, qs, "1min").expect("parse candles");
    assert!(!bars.is_empty());
}

#[test]
#[ignore = "requires KUCOIN_API_KEY/SECRET/PASSPHRASE + network"]
fn live_signed_accounts_query() {
    let (Ok(key), Ok(secret), Ok(pass)) = (
        std::env::var("KUCOIN_API_KEY"),
        std::env::var("KUCOIN_API_SECRET"),
        std::env::var("KUCOIN_API_PASSPHRASE"),
    ) else {
        eprintln!("skipping: KUCOIN_API_KEY/SECRET/PASSPHRASE not set");
        return;
    };
    let ts = unix_millis().to_string();
    let endpoint = "/api/v1/accounts";
    let signature = sign(secret.as_bytes(), &format!("{ts}GET{endpoint}"));
    let mut t = ReqwestTransport::new().expect("client");
    let resp = t
        .send(&HttpRequest {
            method: Method::Get,
            url: format!("{BASE}{endpoint}"),
            body: None,
            headers: vec![
                ("KC-API-KEY".into(), key),
                ("KC-API-SIGN".into(), signature),
                ("KC-API-TIMESTAMP".into(), ts),
                (
                    "KC-API-PASSPHRASE".into(),
                    encrypt_passphrase(secret.as_bytes(), &pass),
                ),
                ("KC-API-KEY-VERSION".into(), "2".into()),
            ],
        })
        .expect("accounts");
    assert!(
        resp.is_success() && resp.body.contains("\"code\":\"200000\""),
        "signed request rejected (HTTP {}): {}",
        resp.status,
        resp.body
    );
}
