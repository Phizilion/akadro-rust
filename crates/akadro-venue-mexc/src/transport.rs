// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The HTTP transport seam.
//!
//! All connector logic talks to MEXC through this trait, so the parsing /
//! signing / normalization can be exercised with a [`MockTransport`] returning
//! recorded responses — no network. The real `reqwest` transport lives behind
//! the `net` feature.

use std::collections::VecDeque;

use crate::error::MexcError;
use crate::request::Method;

/// An outbound HTTP request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// HTTP method.
    pub method: Method,
    /// Full URL (base + path + query).
    pub url: String,
    /// Value of the `X-MEXC-APIKEY` header, if this is an authenticated call.
    pub api_key: Option<String>,
    /// Optional request body.
    pub body: Option<String>,
}

/// An HTTP response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body.
    pub body: String,
}

impl HttpResponse {
    /// A `200 OK` response with `body`.
    #[must_use]
    pub fn ok(body: impl Into<String>) -> Self {
        HttpResponse {
            status: 200,
            body: body.into(),
        }
    }

    /// A non-2xx response.
    #[must_use]
    pub fn error(status: u16, body: impl Into<String>) -> Self {
        HttpResponse {
            status,
            body: body.into(),
        }
    }

    /// `true` if the status is 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Something that can send an [`HttpRequest`] and return an [`HttpResponse`].
pub trait Transport {
    /// Send one request.
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, MexcError>;
}

/// A test transport that returns pre-canned responses in order and records every
/// request it was asked to send.
#[derive(Debug, Default)]
pub struct MockTransport {
    responses: VecDeque<HttpResponse>,
    /// Every request sent, in order (for assertions).
    pub sent: Vec<HttpRequest>,
}

impl MockTransport {
    /// Create a mock that will hand out `responses` in order.
    #[must_use]
    pub fn new(responses: Vec<HttpResponse>) -> Self {
        MockTransport {
            responses: responses.into(),
            sent: Vec::new(),
        }
    }

    /// The URL of the most recent request, if any.
    #[must_use]
    pub fn last_url(&self) -> Option<&str> {
        self.sent.last().map(|r| r.url.as_str())
    }

    /// Number of requests sent.
    #[must_use]
    pub fn sent_count(&self) -> usize {
        self.sent.len()
    }
}

impl Transport for MockTransport {
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, MexcError> {
        self.sent.push(request.clone());
        self.responses
            .pop_front()
            .ok_or_else(|| MexcError::Transport("MockTransport: no canned response left".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_returns_in_order_and_records() {
        let mut t = MockTransport::new(vec![HttpResponse::ok("a"), HttpResponse::error(400, "b")]);
        let req = HttpRequest {
            method: Method::Get,
            url: "https://api.mexc.com/api/v3/time".into(),
            api_key: None,
            body: None,
        };
        assert_eq!(t.send(&req).unwrap(), HttpResponse::ok("a"));
        assert_eq!(t.last_url(), Some("https://api.mexc.com/api/v3/time"));
        let r2 = t.send(&req).unwrap();
        assert_eq!(r2.status, 400);
        assert!(!r2.is_success());
        assert_eq!(t.sent_count(), 2);
        // Exhausted -> transport error.
        assert!(t.send(&req).is_err());
    }

    #[test]
    fn response_helpers() {
        assert!(HttpResponse::ok("x").is_success());
        assert!(!HttpResponse::error(500, "x").is_success());
    }
}
