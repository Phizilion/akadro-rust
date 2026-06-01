// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Signed REST request building (spot).

use crate::sign::sign;

/// HTTP method for a REST call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    /// GET.
    Get,
    /// POST.
    Post,
    /// DELETE.
    Delete,
}

impl Method {
    /// The uppercase method name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Delete => "DELETE",
        }
    }
}

/// Percent-encode a value, keeping RFC 3986 unreserved characters.
fn encode(value: &str) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                // Writing to a String is infallible.
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// A built, signed request: the path plus the full query string (which already
/// includes `&signature=`). The bytes signed equal the bytes transmitted.
///
/// Signed parameters (including the signature) ride in the **query string** even
/// for `POST`/`DELETE`. MEXC accepts this and it keeps the "signed bytes == sent
/// bytes" invariant trivially true. Moving them into an
/// `application/x-www-form-urlencoded` body (marginally more idiomatic, keeps the
/// signature out of proxy logs over the already-TLS-encrypted URL) is a reserved
/// refinement, not required for correctness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedRequest {
    /// HTTP method.
    pub method: Method,
    /// Request path (e.g. `/api/v3/order`).
    pub path: String,
    /// Full query string including the trailing signature.
    pub query: String,
}

/// Build a signed spot request. `params` are emitted in the given order;
/// `timestamp_ms` and optional `recv_window` are appended, the result is signed,
/// and `&signature=<hex>` is appended last.
#[must_use]
pub fn signed(
    method: Method,
    path: &str,
    params: &[(&str, &str)],
    secret: &[u8],
    timestamp_ms: i64,
    recv_window: Option<u64>,
) -> SignedRequest {
    fn push(qs: &mut String, key: &str, value: &str) {
        if !qs.is_empty() {
            qs.push('&');
        }
        qs.push_str(key);
        qs.push('=');
        qs.push_str(&encode(value));
    }

    let mut qs = String::new();
    for (k, v) in params {
        push(&mut qs, k, v);
    }
    push(&mut qs, "timestamp", &timestamp_ms.to_string());
    if let Some(rw) = recv_window {
        push(&mut qs, "recvWindow", &rw.to_string());
    }
    let signature = sign(secret, &qs);
    qs.push_str("&signature=");
    qs.push_str(&signature);
    SignedRequest {
        method,
        path: path.to_owned(),
        query: qs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_names() {
        assert_eq!(Method::Get.as_str(), "GET");
        assert_eq!(Method::Post.as_str(), "POST");
        assert_eq!(Method::Delete.as_str(), "DELETE");
    }

    #[test]
    fn encoding_keeps_unreserved_and_escapes_rest() {
        assert_eq!(encode("BTCUSDT"), "BTCUSDT");
        assert_eq!(encode("1.5"), "1.5");
        assert_eq!(encode("a b"), "a%20b");
        assert_eq!(encode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn signed_request_is_deterministic_and_matches_signed_string() {
        let req = signed(
            Method::Post,
            "/api/v3/order",
            &[
                ("symbol", "BTCUSDT"),
                ("side", "BUY"),
                ("type", "MARKET"),
                ("quantity", "1.5"),
            ],
            b"secretKey",
            1_700_000_000_000,
            Some(5000),
        );
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.path, "/api/v3/order");
        let signed_part = "symbol=BTCUSDT&side=BUY&type=MARKET&quantity=1.5&timestamp=1700000000000&recvWindow=5000";
        let expected_sig = sign(b"secretKey", signed_part);
        assert_eq!(req.query, format!("{signed_part}&signature={expected_sig}"));
        // Determinism.
        let req2 = signed(
            Method::Post,
            "/api/v3/order",
            &[
                ("symbol", "BTCUSDT"),
                ("side", "BUY"),
                ("type", "MARKET"),
                ("quantity", "1.5"),
            ],
            b"secretKey",
            1_700_000_000_000,
            Some(5000),
        );
        assert_eq!(req, req2);
    }

    #[test]
    fn signed_without_recv_window() {
        let req = signed(Method::Get, "/api/v3/account", &[], b"k", 42, None);
        assert!(req.query.starts_with("timestamp=42&signature="));
        assert!(!req.query.contains("recvWindow"));
    }
}
