// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Blocking `reqwest` transport for KuCoin (the `net` feature). Live IO, exercised
//! only by the credential-gated `#[ignore]`d tests, so it is excluded from the
//! coverage gate.

use crate::{HttpRequest, HttpResponse, KucoinError, Method, Transport};

/// A [`Transport`] backed by a blocking `reqwest` client.
#[derive(Debug)]
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

impl ReqwestTransport {
    /// Build a transport with a sane default request timeout.
    ///
    /// # Errors
    /// [`KucoinError::Transport`] if the client cannot be built.
    pub fn new() -> Result<Self, KucoinError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("akadro-venue-kucoin")
            .build()
            .map_err(|e| KucoinError::Transport(e.to_string()))?;
        Ok(ReqwestTransport { client })
    }
}

impl Transport for ReqwestTransport {
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, KucoinError> {
        let mut builder = match request.method {
            Method::Get => self.client.get(&request.url),
            Method::Post => self.client.post(&request.url),
        };
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value);
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }
        let resp = builder
            .send()
            .map_err(|e| KucoinError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .map_err(|e| KucoinError::Transport(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

/// Current Unix time in milliseconds, for request signing.
#[must_use]
pub fn unix_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
