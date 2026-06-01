// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # akadro-venue-mexc
//!
//! A [MEXC](https://www.mexc.com) exchange connector for akadro. It is the first
//! concrete venue and the worked proof that a new exchange is a self-contained
//! crate implementing akadro's three extension traits with **zero changes** to
//! `akadro-core`.
//!
//! ## Scope (v1)
//!
//! - **Spot, over REST** (verified, fully fixture-tested): signed order
//!   submission, order-status polling for fills, `exchangeInfo` → instrument
//!   specs, and `klines` → bars for the data feed.
//! - All pure logic — signing, decimal↔fixed-point conversion, request building,
//!   response parsing, error mapping — is unit-tested against recorded fixtures
//!   through a mockable [`Transport`]. The real network transport is behind the
//!   `net` feature (reqwest); the live round-trip is a credential-gated,
//!   `#[ignore]`d integration test.
//!
//! Deferred (documented in the repo `AGENTS.md`): the protobuf WebSocket streams
//! (lower-latency market + private data), futures private stream, and the async
//! `akadro-live` bridge. These do not change the public surface.

mod client;
mod convert;
mod error;
mod futures;
mod instrument;
mod parse;
mod request;
mod sign;
mod transport;
mod ws;
mod ws_proto;

#[cfg(feature = "net")]
mod net;
#[cfg(feature = "net")]
pub use net::{ReqwestTransport, unix_millis};

pub use client::{MAX_KLINES_LIMIT, MexcKlineFeed, MexcSpotExec};
pub use convert::{decimal_to_raw, raw_to_decimal};
pub use error::{MexcError, map_reject_code};
pub use futures::{
    FUNDING_RATE_SCALE, FUTURES_BASE_URL, FuturesContract, MexcFuturesKlineFeed, fetch_contracts,
    fetch_funding_history, fetch_funding_history_paged, futures_interval, futures_side,
    futures_type, parse_contract_detail, parse_funding_rate, parse_futures_klines, sign_futures,
};
pub use instrument::MexcCatalog;
pub use parse::{
    MexcTrade, OrderAck, OrderStatus, parse_api_error, parse_klines, parse_my_trades,
    parse_order_ack, parse_order_status,
};
pub use request::{Method, SignedRequest, signed};
pub use sign::sign;
pub use transport::{HttpRequest, HttpResponse, MockTransport, Transport};
pub use ws::{
    ControlFrame, client_order_tag, kline_channel, parse_client_order_tag, parse_control_frame,
    ping_message, subscribe_message, unsubscribe_message,
};
pub use ws_proto::{KlineAggregator, WsDeal, WsKline, decode_kline_push, decode_private_deal};
