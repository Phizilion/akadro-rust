// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Real HTTP transport via `reqwest::blocking` (the `net` feature).
//!
//! Blocking I/O on a background thread keeps the strategy loop synchronous and
//! deterministic (no async runtime needed for bar-cadence trading). A
//! lower-latency WebSocket path is future work (see `AGENTS.md`).

use std::time::Duration;

use reqwest::blocking::Client;

use crate::error::MexcError;
use crate::request::Method;
use crate::transport::{HttpRequest, HttpResponse, Transport};

/// A [`Transport`] backed by a blocking `reqwest` client.
#[derive(Debug)]
pub struct ReqwestTransport {
    client: Client,
}

impl ReqwestTransport {
    /// Build a transport with a sane default request timeout.
    ///
    /// # Errors
    /// Returns [`MexcError::Transport`] if the underlying client cannot be built.
    pub fn new() -> Result<Self, MexcError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("akadro-venue-mexc")
            .build()
            .map_err(|e| MexcError::Transport(e.to_string()))?;
        Ok(ReqwestTransport { client })
    }
}

impl ReqwestTransport {
    /// One attempt at sending `request`.
    fn try_send(&self, request: &HttpRequest) -> Result<HttpResponse, MexcError> {
        let mut builder = match request.method {
            Method::Get => self.client.get(&request.url),
            Method::Post => self.client.post(&request.url),
            Method::Delete => self.client.delete(&request.url),
        };
        if let Some(key) = &request.api_key {
            builder = builder.header("X-MEXC-APIKEY", key);
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }
        let resp = builder
            .send()
            .map_err(|e| MexcError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .map_err(|e| MexcError::Transport(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

impl Transport for ReqwestTransport {
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, MexcError> {
        // Transient transport failures (a long paginated back-fill occasionally
        // drops a connection / fails to decode a body) are retried with a short
        // linear backoff. Rate-limit handling is different:
        //
        // * A **signed** request (it carries the API key + a timestamp baked into
        //   the signature) must NOT be slept-and-retried: after any back-off beyond
        //   `recvWindow` the venue rejects the stale signature with error 700003
        //   (M7). Return the 429/418 to the caller so the bar-cadence poller
        //   re-signs and retries next bar.
        // * A **418** (IP ban) will not clear in seconds, and sleeping ~minutes on
        //   the (synchronous) engine thread is unacceptable — return it immediately
        //   for the operator/strategy to back off at a higher level.
        // * Only an **unsigned** public request (klines / exchangeInfo) on a 429 is
        //   safe to retry in place, with a short bounded backoff.
        let signed = request.api_key.is_some();
        let mut attempt = 0u32;
        loop {
            match self.try_send(request) {
                Ok(resp) => {
                    if resp.status == 429 && !signed && attempt < 3 {
                        attempt += 1;
                        std::thread::sleep(Duration::from_secs(u64::from(attempt)));
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    attempt += 1;
                    if attempt > 4 {
                        return Err(e);
                    }
                    // ~0.3s..1.2s — well under a 5s recvWindow even for signed
                    // requests, so a transient retry never invalidates the signature.
                    std::thread::sleep(Duration::from_millis(300 * u64::from(attempt)));
                }
            }
        }
    }
}

/// Current Unix time in milliseconds, for request signing. Falls back to 0 if
/// the system clock is before the epoch (never, in practice).
#[must_use]
pub fn unix_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
