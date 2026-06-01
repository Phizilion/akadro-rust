// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure decoders for Binance **WebSocket** frames — no async, no tokio, so they
//! are fixture-tested with plain JSON. The async transport that pumps these into
//! the engine (`tokio-tungstenite`) lives in `akadro-live` behind its `binance`
//! feature, keeping tokio out of the venue crate (and the core).
//!
//! Two streams matter:
//! * **Market** `<symbol>@kline_<interval>`: [`parse_ws_kline`] yields a [`Bar`]
//!   only when the kline is **closed** (`k.x == true`), stamped at its close time —
//!   the same bar a backtest would see, so live and backtest stay in parity.
//! * **User-data** (`executionReport`): [`parse_execution_report`] decodes a fill /
//!   lifecycle update, and [`WsExecReport::to_account_event`] maps it to an
//!   [`AccountEvent`]. The akadro [`ClientOrderId`] is recovered from Binance's
//!   `clientOrderId` via the [`client_order_tag`] convention this connector sets on
//!   submit (`ak<id>`), closing the order→fill attribution loop.

use akadro_core::{
    AccountEvent, AssetId, Bar, CancelReason, ClientOrderId, Cost, CostKind, Costs, InstrumentId,
    Money, Price, Qty, RejectReason, Side, Timestamp,
};
use serde::Deserialize;

use crate::{BinanceError, decimal_to_raw};

fn ms_to_ns(ms: i64) -> Result<i64, BinanceError> {
    ms.checked_mul(1_000_000)
        .ok_or_else(|| BinanceError::Parse("timestamp overflow".into()))
}

/// The `newClientOrderId` an akadro connector sets when submitting, so a live fill
/// on the user-data stream can be attributed back to the originating order:
/// `client_order_tag(ClientOrderId::new(7))` → `"ak7"`.
#[must_use]
pub fn client_order_tag(id: ClientOrderId) -> String {
    format!("ak{}", id.raw())
}

/// Inverse of [`client_order_tag`]: recover the akadro [`ClientOrderId`] from a
/// Binance `clientOrderId`, or `None` if it was not set by this connector.
#[must_use]
pub fn parse_client_order_tag(tag: &str) -> Option<ClientOrderId> {
    tag.strip_prefix("ak")
        .and_then(|n| n.parse::<u64>().ok())
        .map(ClientOrderId::new)
}

fn parse_side(s: &str) -> Option<Side> {
    match s {
        "BUY" => Some(Side::Buy),
        "SELL" => Some(Side::Sell),
        _ => None,
    }
}

// --- market klines -----------------------------------------------------------

#[derive(Deserialize)]
struct KlineMsg {
    k: KlineData,
}

#[derive(Deserialize)]
struct KlineData {
    #[serde(rename = "T")]
    close_time: i64,
    #[serde(rename = "o")]
    open: String,
    #[serde(rename = "h")]
    high: String,
    #[serde(rename = "l")]
    low: String,
    #[serde(rename = "c")]
    close: String,
    #[serde(rename = "v")]
    volume: String,
    #[serde(rename = "x")]
    closed: bool,
}

/// Decode a `<symbol>@kline_<interval>` frame. Returns `Some(Bar)` only for a
/// **closed** kline (stamped at its close time); `Ok(None)` for an in-progress
/// kline (no look-ahead — a partial bar is never surfaced).
///
/// # Errors
/// [`BinanceError::Parse`] on bad JSON, a bad number, or a timestamp overflow.
pub fn parse_ws_kline(
    json: &str,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
) -> Result<Option<Bar>, BinanceError> {
    let msg: KlineMsg =
        serde_json::from_str(json).map_err(|e| BinanceError::Parse(e.to_string()))?;
    let k = msg.k;
    if !k.closed {
        return Ok(None);
    }
    Ok(Some(Bar::new(
        instrument,
        Timestamp::from_nanos(ms_to_ns(k.close_time)?),
        Price::from_raw(decimal_to_raw(&k.open, price_scale)?),
        Price::from_raw(decimal_to_raw(&k.high, price_scale)?),
        Price::from_raw(decimal_to_raw(&k.low, price_scale)?),
        Price::from_raw(decimal_to_raw(&k.close, price_scale)?),
        Qty::from_raw(decimal_to_raw(&k.volume, qty_scale)?),
    )))
}

// --- user-data listen key ----------------------------------------------------

#[derive(Deserialize)]
struct ListenKeyMsg {
    #[serde(rename = "listenKey")]
    listen_key: String,
}

/// Extract the `listenKey` from a `POST /api/v3/userDataStream` response.
///
/// # Errors
/// [`BinanceError::Parse`] if the body has no `listenKey`.
pub fn parse_listen_key(json: &str) -> Result<String, BinanceError> {
    serde_json::from_str::<ListenKeyMsg>(json)
        .map(|m| m.listen_key)
        .map_err(|e| BinanceError::Parse(e.to_string()))
}

// --- user-data execution report ----------------------------------------------

#[derive(Deserialize)]
struct ExecMsg {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "S")]
    side: String,
    #[serde(rename = "x")]
    exec_type: String,
    #[serde(rename = "X")]
    status: String,
    #[serde(rename = "E", default)]
    event_time: i64,
    #[serde(rename = "T", default)]
    trade_time: i64,
    #[serde(rename = "c", default)]
    client_order_id: String,
    #[serde(rename = "L", default)]
    last_price: String,
    #[serde(rename = "l", default)]
    last_qty: String,
    #[serde(rename = "n", default)]
    commission: String,
    #[serde(rename = "N", default)]
    commission_asset: Option<String>,
}

/// A decoded `executionReport` user-data frame (normalized, venue-neutral fields).
/// Turn it into an [`AccountEvent`] with [`WsExecReport::to_account_event`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct WsExecReport {
    /// Venue symbol the report concerns.
    pub symbol: String,
    /// Fill / order side.
    pub side: Side,
    /// Binance execution type (`NEW`, `TRADE`, `CANCELED`, `EXPIRED`, `REJECTED`…).
    pub exec_type: String,
    /// Order status (`NEW`, `PARTIALLY_FILLED`, `FILLED`, …).
    pub status: String,
    /// The akadro order id recovered from `clientOrderId` (`None` if not set by
    /// this connector — e.g. an order placed out of band).
    pub client_order_id: Option<ClientOrderId>,
    /// Last fill price (decimal string).
    pub last_price: String,
    /// Last fill quantity (decimal string).
    pub last_qty: String,
    /// Commission charged on this fill (decimal string).
    pub commission: String,
    /// Asset the commission was charged in, if present.
    pub commission_asset: Option<String>,
    /// Event time (ms) / transaction time (ms) for a trade.
    pub event_ms: i64,
}

/// Decode a user-data frame; `Ok(None)` for any frame that is not an
/// `executionReport` (e.g. `outboundAccountPosition`, `balanceUpdate`) or whose
/// side is unrecognized.
///
/// # Errors
/// [`BinanceError::Parse`] on bad JSON.
pub fn parse_execution_report(json: &str) -> Result<Option<WsExecReport>, BinanceError> {
    let msg: ExecMsg =
        serde_json::from_str(json).map_err(|e| BinanceError::Parse(e.to_string()))?;
    if msg.event_type != "executionReport" {
        return Ok(None);
    }
    let Some(side) = parse_side(&msg.side) else {
        return Ok(None);
    };
    // A trade carries a transaction time `T`; lifecycle updates use event time `E`.
    let event_ms = if msg.trade_time != 0 {
        msg.trade_time
    } else {
        msg.event_time
    };
    Ok(Some(WsExecReport {
        symbol: msg.symbol,
        side,
        exec_type: msg.exec_type,
        status: msg.status,
        client_order_id: parse_client_order_tag(&msg.client_order_id),
        last_price: msg.last_price,
        last_qty: msg.last_qty,
        commission: msg.commission,
        commission_asset: msg.commission_asset,
        event_ms,
    }))
}

impl WsExecReport {
    /// Map this report to an [`AccountEvent`] for `instrument` (with its
    /// `quote_asset` and scales). Returns `None` when the report carries no akadro
    /// order id, or for an execution type with no engine-visible effect.
    ///
    /// Commission is charged in `quote_asset` at `price_scale` — correct for the
    /// dominant quote-asset taker case; a per-`commissionAsset` scale (BNB/base
    /// discounts) is a documented future refinement (the same limitation the MEXC
    /// connector notes).
    #[must_use]
    pub fn to_account_event(
        &self,
        instrument: InstrumentId,
        quote_asset: AssetId,
        price_scale: u32,
        qty_scale: u32,
    ) -> Option<AccountEvent> {
        let id = self.client_order_id?;
        let ts = Timestamp::from_nanos(ms_to_ns(self.event_ms).ok()?);
        match self.exec_type.as_str() {
            "NEW" => Some(AccountEvent::OrderAccepted { id, ts }),
            "TRADE" => {
                let price = Price::from_raw(decimal_to_raw(&self.last_price, price_scale).ok()?);
                let qty = Qty::from_raw(decimal_to_raw(&self.last_qty, qty_scale).ok()?);
                let mut costs = Costs::new();
                if let Ok(fee) = decimal_to_raw(&self.commission, price_scale)
                    && fee != 0
                {
                    costs.push(Cost::new(
                        quote_asset,
                        Money::from_raw(i128::from(fee)),
                        CostKind::Taker,
                    ));
                }
                Some(AccountEvent::Fill {
                    client_order_id: id,
                    instrument,
                    side: self.side,
                    price,
                    qty,
                    costs,
                    complete: self.status == "FILLED",
                    ts,
                })
            }
            "CANCELED" => Some(AccountEvent::OrderCanceled {
                id,
                reason: CancelReason::Requested,
                ts,
            }),
            "EXPIRED" => Some(AccountEvent::OrderCanceled {
                id,
                reason: CancelReason::Expired,
                ts,
            }),
            "REJECTED" => Some(AccountEvent::OrderRejected {
                id,
                reason: RejectReason::VenueRejected,
                ts,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_tag_round_trips() {
        assert_eq!(client_order_tag(ClientOrderId::new(7)), "ak7");
        assert_eq!(parse_client_order_tag("ak7"), Some(ClientOrderId::new(7)));
        assert_eq!(parse_client_order_tag("web_xyz"), None); // foreign order
        assert_eq!(parse_client_order_tag("akx"), None); // not a number
    }

    #[test]
    fn closed_kline_becomes_a_bar_open_kline_does_not() {
        let closed = r#"{"e":"kline","s":"BTCUSDT","k":{"t":1,"T":1700000059999,"o":"100.00","h":"101.00","l":"99.00","c":"100.50","v":"12.5","x":true}}"#;
        let bar = parse_ws_kline(closed, InstrumentId::new(0), 2, 1)
            .unwrap()
            .expect("closed kline yields a bar");
        assert_eq!(bar.ts.as_nanos(), 1_700_000_059_999_000_000);
        assert_eq!(bar.close.raw(), 10050);
        assert_eq!(bar.volume.raw(), 125);

        let open = r#"{"e":"kline","s":"BTCUSDT","k":{"t":1,"T":1700000059999,"o":"100","h":"100","l":"100","c":"100","v":"1","x":false}}"#;
        assert!(
            parse_ws_kline(open, InstrumentId::new(0), 2, 1)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn listen_key_extracted() {
        assert_eq!(
            parse_listen_key(r#"{"listenKey":"abc123"}"#).unwrap(),
            "abc123"
        );
        assert!(parse_listen_key("{}").is_err());
    }

    #[test]
    fn execution_report_trade_maps_to_fill() {
        let json = r#"{"e":"executionReport","s":"BTCUSDT","S":"BUY","x":"TRADE","X":"FILLED",
            "E":1700000000000,"T":1700000000500,"c":"ak42","L":"105.00","l":"1.000","n":"0.05","N":"USDT","i":9}"#;
        let report = parse_execution_report(json).unwrap().expect("a report");
        assert_eq!(report.client_order_id, Some(ClientOrderId::new(42)));
        let ev = report
            .to_account_event(InstrumentId::new(0), AssetId::new(1), 2, 3)
            .unwrap();
        let AccountEvent::Fill {
            client_order_id,
            side,
            price,
            qty,
            complete,
            costs,
            ts,
            ..
        } = ev
        else {
            panic!("expected a Fill, got {ev:?}");
        };
        assert_eq!(client_order_id, ClientOrderId::new(42));
        assert_eq!(side, Side::Buy);
        assert_eq!(price.raw(), 10_500); // 105.00 @ 2
        assert_eq!(qty.raw(), 1_000); // 1.000 @ 3
        assert!(complete); // status FILLED
        assert_eq!(ts.as_nanos(), 1_700_000_000_500_000_000); // transaction time T
        // Commission 0.05 @ price_scale 2 = 5 raw, charged in the quote asset.
        assert_eq!(costs.iter().count(), 1);
        assert_eq!(costs.iter().next().unwrap().amount, Money::from_raw(5));
    }

    #[test]
    fn execution_report_lifecycle_and_skips() {
        let new = r#"{"e":"executionReport","s":"BTCUSDT","S":"SELL","x":"NEW","X":"NEW","E":1700000000000,"c":"ak1","i":1}"#;
        let ev = parse_execution_report(new)
            .unwrap()
            .unwrap()
            .to_account_event(InstrumentId::new(0), AssetId::new(1), 2, 3)
            .unwrap();
        assert!(matches!(ev, AccountEvent::OrderAccepted { .. }));

        let canceled = r#"{"e":"executionReport","s":"BTCUSDT","S":"SELL","x":"CANCELED","X":"CANCELED","E":1,"c":"ak1","i":1}"#;
        let ev = parse_execution_report(canceled)
            .unwrap()
            .unwrap()
            .to_account_event(InstrumentId::new(0), AssetId::new(1), 2, 3)
            .unwrap();
        assert!(matches!(
            ev,
            AccountEvent::OrderCanceled {
                reason: CancelReason::Requested,
                ..
            }
        ));

        // Non-execution frames are skipped.
        assert!(
            parse_execution_report(
                r#"{"e":"outboundAccountPosition","s":"X","S":"BUY","x":"","X":""}"#
            )
            .unwrap()
            .is_none()
        );
        // A foreign order id (not ours) yields no engine event.
        let foreign = r#"{"e":"executionReport","s":"BTCUSDT","S":"BUY","x":"TRADE","X":"FILLED","E":1,"T":1,"c":"web_1","L":"1","l":"1","i":1}"#;
        assert!(
            parse_execution_report(foreign)
                .unwrap()
                .unwrap()
                .to_account_event(InstrumentId::new(0), AssetId::new(1), 2, 3)
                .is_none()
        );
    }

    #[test]
    fn expired_rejected_and_unknown_exec_types() {
        let ev = |x: &str| {
            let json = format!(
                r#"{{"e":"executionReport","s":"BTCUSDT","S":"BUY","x":"{x}","X":"{x}","E":1,"c":"ak1","i":1}}"#
            );
            parse_execution_report(&json)
                .unwrap()
                .unwrap()
                .to_account_event(InstrumentId::new(0), AssetId::new(1), 2, 3)
        };
        assert!(matches!(
            ev("EXPIRED"),
            Some(AccountEvent::OrderCanceled {
                reason: CancelReason::Expired,
                ..
            })
        ));
        assert!(matches!(
            ev("REJECTED"),
            Some(AccountEvent::OrderRejected {
                reason: RejectReason::VenueRejected,
                ..
            })
        ));
        assert!(ev("REPLACED").is_none()); // an exec type with no engine effect
    }

    #[test]
    fn unrecognized_side_is_skipped_and_bad_json_errors() {
        let weird_side = r#"{"e":"executionReport","s":"BTCUSDT","S":"WAT","x":"NEW","X":"NEW","E":1,"c":"ak1","i":1}"#;
        assert!(parse_execution_report(weird_side).unwrap().is_none());
        assert!(parse_execution_report("not json").is_err());
        assert!(parse_ws_kline("not json", InstrumentId::new(0), 2, 1).is_err());
    }
}
