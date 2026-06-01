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

// --- Binance Vision bulk-historical downloader -------------------------------
//
// Requires `net` (this whole module is `net`-gated) + `vision-zip`; the
// `vision-net` feature enables both. The pure parse/verify/unzip logic lives in
// `crate::vision`; this is only the live-IO half.

/// Downloads and decodes Binance Vision bulk-historical archives over HTTP
/// (`reqwest::blocking` — no tokio). The pure parsing/verification lives in
/// [`crate::vision`]; this is the live-IO half (exercised by the `#[ignore]`d
/// live test, excluded from the coverage gate).
#[cfg(all(feature = "net", feature = "vision-zip"))]
#[derive(Debug)]
pub struct VisionDownloader {
    client: reqwest::blocking::Client,
    base: String,
}

#[cfg(all(feature = "net", feature = "vision-zip"))]
impl VisionDownloader {
    /// Build a downloader against [`VISION_BASE`](crate::vision::VISION_BASE).
    ///
    /// # Errors
    /// [`BinanceError::Transport`] if the HTTP client cannot be built.
    pub fn new() -> Result<Self, BinanceError> {
        Self::with_base(crate::vision::VISION_BASE)
    }

    /// Build a downloader against a custom base host (e.g. a mirror).
    ///
    /// # Errors
    /// [`BinanceError::Transport`] if the HTTP client cannot be built.
    pub fn with_base(base: impl Into<String>) -> Result<Self, BinanceError> {
        // Generous: a monthly 1-minute archive is a few MB. `from_mins` is still
        // unstable on the MSRV, so spell the timeout in seconds.
        #[allow(clippy::duration_suboptimal_units)]
        let timeout = std::time::Duration::from_secs(60);
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .user_agent("akadro-venue-binance")
            .build()
            .map_err(|e| BinanceError::Transport(e.to_string()))?;
        Ok(VisionDownloader {
            client,
            base: base.into(),
        })
    }

    fn get_bytes(&self, url: &str) -> Result<Vec<u8>, BinanceError> {
        let resp = self
            .client
            .get(url)
            .send()
            .map_err(|e| BinanceError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(BinanceError::Transport(format!(
                "vision HTTP {status} for {url}"
            )));
        }
        resp.bytes()
            .map(|b| b.to_vec())
            .map_err(|e| BinanceError::Transport(e.to_string()))
    }

    /// Download one archive, optionally verify its `.CHECKSUM` (SHA-256), and
    /// return the decompressed CSV body. A missing file surfaces as a
    /// [`BinanceError::Transport`] carrying the HTTP status, so callers can fall
    /// back monthly→daily for the most recent (not-yet-published) period.
    ///
    /// # Errors
    /// [`BinanceError::Transport`] on a network error or non-2xx (incl. 404);
    /// [`BinanceError::Parse`] on a checksum mismatch or a bad archive.
    pub fn fetch_csv(
        &self,
        granularity: crate::vision::VisionGranularity,
        symbol: &str,
        interval: &str,
        date_token: &str,
        verify_checksum: bool,
    ) -> Result<String, BinanceError> {
        let url =
            crate::vision::vision_zip_url(&self.base, granularity, symbol, interval, date_token);
        let zip_bytes = self.get_bytes(&url)?;
        // Download the `.CHECKSUM` body (if verifying); the pure verify+unzip then
        // lives in `crate::vision::verify_and_unzip` (unit-tested without network).
        let checksum_body = if verify_checksum {
            Some(
                String::from_utf8(self.get_bytes(&crate::vision::vision_checksum_url(&url))?)
                    .map_err(|e| BinanceError::Parse(format!("vision checksum utf8: {e}")))?,
            )
        } else {
            None
        };
        crate::vision::verify_and_unzip(&zip_bytes, checksum_body.as_deref())
    }
}
