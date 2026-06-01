// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Blocking `reqwest` transport for Binance (the `net` feature). Bar-cadence
//! trading is synchronous and deterministic, so blocking I/O needs no async
//! runtime; the lower-latency WebSocket path lives in `akadro-live`. This is live
//! IO, exercised only by the credential-gated `#[ignore]`d tests, so it is
//! excluded from the coverage gate.

use crate::{BinanceError, HttpRequest, HttpResponse, Method, Transport};

/// A [`Transport`] backed by a blocking `reqwest` client.
#[derive(Debug)]
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

impl ReqwestTransport {
    /// Build a transport with a sane default request timeout.
    ///
    /// # Errors
    /// [`BinanceError::Transport`] if the client cannot be built.
    pub fn new() -> Result<Self, BinanceError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("akadro-venue-binance")
            .build()
            .map_err(|e| BinanceError::Transport(e.to_string()))?;
        Ok(ReqwestTransport { client })
    }

    fn try_send(&self, request: &HttpRequest) -> Result<HttpResponse, BinanceError> {
        let mut builder = match request.method {
            Method::Get => self.client.get(&request.url),
            Method::Post => self.client.post(&request.url),
            Method::Delete => self.client.delete(&request.url),
        };
        if let Some(key) = &request.api_key {
            builder = builder.header("X-MBX-APIKEY", key);
        }
        let resp = builder
            .send()
            .map_err(|e| BinanceError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .map_err(|e| BinanceError::Transport(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

impl Transport for ReqwestTransport {
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, BinanceError> {
        // Retry transient failures and rate-limit responses (429 Too Many
        // Requests, 418 IP banned) with linear backoff; other non-2xx are returned
        // as `Ok` for the caller to interpret.
        let mut attempt = 0u32;
        loop {
            match self.try_send(request) {
                Ok(resp) => {
                    if matches!(resp.status, 429 | 418) && attempt < 4 {
                        attempt += 1;
                        let base = if resp.status == 418 { 60 } else { 1 };
                        std::thread::sleep(std::time::Duration::from_secs(
                            base * u64::from(attempt),
                        ));
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    attempt += 1;
                    if attempt > 4 {
                        return Err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(300 * u64::from(attempt)));
                }
            }
        }
    }
}

/// Current Unix time in milliseconds, for request signing (`timestamp` param).
/// Falls back to 0 if the system clock predates the epoch (never, in practice).
#[must_use]
pub fn unix_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
