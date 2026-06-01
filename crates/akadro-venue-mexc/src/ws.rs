// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MEXC WebSocket protocol helpers: subscription + heartbeat **control frames**
//! (JSON) and order-tagging for fill attribution.
//!
//! MEXC's spot market-data channels carry **protobuf** payloads
//! (`spot@public.kline.v3.api.pb@<SYMBOL>@<INTERVAL>`). Those binary frames are now
//! decoded in [`crate::ws_proto`] (a standard-protobuf wire reader over the field
//! tags vendored from [`mexcdevelop/websocket-proto`]); the byte transport
//! (`tokio-tungstenite`) lives in `akadro-live`'s `mexc` feature.
//!
//! The **control protocol** below — building subscribe/unsubscribe/ping messages
//! and parsing the JSON control replies (PONG, subscription acks) — is JSON and
//! documented, and fully tested here.
//!
//! [`mexcdevelop/websocket-proto`]: https://github.com/mexcdevelop/websocket-proto

use akadro_core::ClientOrderId;
use serde::Serialize;

#[derive(Serialize)]
struct Op<'a> {
    method: &'a str,
    params: &'a [String],
}

/// The `clientOrderId` to set when submitting, so a live private-deal frame can be
/// attributed back to its akadro order: `client_order_tag(ClientOrderId::new(7))`
/// → `"ak7"`. Mirrors the Binance connector's convention.
#[must_use]
pub fn client_order_tag(id: ClientOrderId) -> String {
    format!("ak{}", id.raw())
}

/// Inverse of [`client_order_tag`]: recover the akadro [`ClientOrderId`] from a
/// venue `clientOrderId`, or `None` if it was not set by this connector.
#[must_use]
pub fn parse_client_order_tag(tag: &str) -> Option<ClientOrderId> {
    tag.strip_prefix("ak")
        .and_then(|n| n.parse::<u64>().ok())
        .map(ClientOrderId::new)
}

/// Build a `SUBSCRIPTION` message for the given channels.
#[must_use]
pub fn subscribe_message(channels: &[String]) -> String {
    serde_json::to_string(&Op {
        method: "SUBSCRIPTION",
        params: channels,
    })
    .unwrap_or_else(|_| String::from(r#"{"method":"SUBSCRIPTION","params":[]}"#))
}

/// Build an `UNSUBSCRIPTION` message for the given channels.
#[must_use]
pub fn unsubscribe_message(channels: &[String]) -> String {
    serde_json::to_string(&Op {
        method: "UNSUBSCRIPTION",
        params: channels,
    })
    .unwrap_or_else(|_| String::from(r#"{"method":"UNSUBSCRIPTION","params":[]}"#))
}

/// The keep-alive ping (send every 10–20s; the server replies `PONG`).
#[must_use]
pub fn ping_message() -> String {
    String::from(r#"{"method":"PING"}"#)
}

/// The protobuf kline channel name for a symbol + WS interval (e.g. `"Min1"`).
#[must_use]
pub fn kline_channel(symbol: &str, ws_interval: &str) -> String {
    format!("spot@public.kline.v3.api.pb@{symbol}@{ws_interval}")
}

/// A parsed JSON control frame from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlFrame {
    /// Reply to a `PING`.
    Pong,
    /// Acknowledgement echoing a subscribed channel (`code == 0`).
    SubscriptionAck(String),
    /// An error reply (`code != 0`).
    Error {
        /// Error code.
        code: i64,
        /// Error message.
        msg: String,
    },
    /// Anything else (e.g. a binary market-data frame routed elsewhere).
    Other,
}

/// Parse a textual control frame. Binary (protobuf) market-data frames are not
/// text and are handled by the (future) protobuf decoder, not here.
#[must_use]
pub fn parse_control_frame(text: &str) -> ControlFrame {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return ControlFrame::Other;
    };
    let msg = v
        .get("msg")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let code = v
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    if msg.eq_ignore_ascii_case("PONG") {
        ControlFrame::Pong
    } else if code != 0 {
        ControlFrame::Error {
            code,
            msg: msg.to_owned(),
        }
    } else if !msg.is_empty() {
        ControlFrame::SubscriptionAck(msg.to_owned())
    } else {
        ControlFrame::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_subscribe_and_ping() {
        let ch = vec![kline_channel("BTCUSDT", "Min1")];
        let sub = subscribe_message(&ch);
        assert!(sub.contains("\"method\":\"SUBSCRIPTION\""));
        assert!(sub.contains("spot@public.kline.v3.api.pb@BTCUSDT@Min1"));
        assert_eq!(ping_message(), r#"{"method":"PING"}"#);
        let unsub = unsubscribe_message(&ch);
        assert!(unsub.contains("UNSUBSCRIPTION"));
    }

    #[test]
    fn order_tag_round_trips() {
        assert_eq!(client_order_tag(ClientOrderId::new(7)), "ak7");
        assert_eq!(parse_client_order_tag("ak7"), Some(ClientOrderId::new(7)));
        assert_eq!(parse_client_order_tag("manual-1"), None);
        assert_eq!(parse_client_order_tag("akz"), None);
    }

    #[test]
    fn parses_control_frames() {
        assert_eq!(
            parse_control_frame(r#"{"id":0,"code":0,"msg":"PONG"}"#),
            ControlFrame::Pong
        );
        assert_eq!(
            parse_control_frame(
                r#"{"id":0,"code":0,"msg":"spot@public.kline.v3.api.pb@BTCUSDT@Min1"}"#
            ),
            ControlFrame::SubscriptionAck("spot@public.kline.v3.api.pb@BTCUSDT@Min1".to_owned())
        );
        assert!(matches!(
            parse_control_frame(r#"{"id":0,"code":500,"msg":"Blocked"}"#),
            ControlFrame::Error { code: 500, .. }
        ));
        assert_eq!(parse_control_frame("not json"), ControlFrame::Other);
        assert_eq!(parse_control_frame(r#"{"code":0}"#), ControlFrame::Other);
    }
}
