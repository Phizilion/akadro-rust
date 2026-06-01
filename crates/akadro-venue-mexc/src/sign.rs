// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MEXC spot request signing.
//!
//! Spot signed requests use `signature = lowercase_hex(HMAC_SHA256(secretKey,
//! totalParams))`, where `totalParams` is the exact query string (and body, if
//! any) that is transmitted — byte-for-byte. The API key travels in the
//! `X-MEXC-APIKEY` header (added by the transport, not here).

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Sign `total_params` with `secret`, returning a lowercase hex digest.
///
/// # Panics
/// Never. HMAC-SHA256 accepts a key of any length, so construction cannot fail.
#[must_use]
pub fn sign(secret: &[u8], total_params: &str) -> String {
    // HMAC accepts a key of any length, so this never errors.
    let mut mac =
        HmacSha256::new_from_slice(secret).expect("HMAC-SHA256 accepts keys of any length");
    mac.update(total_params.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_rfc4231_test_case_2() {
        // RFC 4231 §4.3: a published HMAC-SHA256 vector.
        let got = sign(b"Jefe", "what do ya want for nothing?");
        assert_eq!(
            got,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn digest_is_lowercase_hex_64_chars() {
        let got = sign(
            b"secretKey",
            "symbol=BTCUSDT&side=BUY&timestamp=1700000000000",
        );
        assert_eq!(got.len(), 64);
        assert!(
            got.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn deterministic_for_same_input() {
        let a = sign(b"k", "a=1&b=2");
        let b = sign(b"k", "a=1&b=2");
        assert_eq!(a, b);
        // Any change in the signed string changes the signature.
        assert_ne!(a, sign(b"k", "a=1&b=3"));
        assert_ne!(a, sign(b"k2", "a=1&b=2"));
    }
}
