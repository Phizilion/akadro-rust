// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Parsing MEXC spot REST responses into akadro types.

use akadro_core::{AssetId, Bar, Cost, CostKind, InstrumentId, Money, Price, Qty, Timestamp};
use serde::Deserialize;

use crate::convert::decimal_to_raw;
use crate::error::MexcError;

/// Parse a `GET /api/v3/klines` response into bars.
///
/// Each row is `[openTime, open, high, low, close, volume, closeTime, quoteVol]`;
/// prices/volume are decimal strings and `closeTime` is epoch-ms. The bar's
/// timestamp is the close time (a bar is only actionable once closed).
pub fn parse_klines(
    json: &str,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
) -> Result<Vec<Bar>, MexcError> {
    let rows: Vec<Vec<serde_json::Value>> =
        serde_json::from_str(json).map_err(|e| MexcError::Parse(e.to_string()))?;

    let mut bars = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() < 7 {
            return Err(MexcError::Parse(format!(
                "kline row has {} fields (<7)",
                row.len()
            )));
        }
        let s = |i: usize| -> Result<&str, MexcError> {
            row[i]
                .as_str()
                .ok_or_else(|| MexcError::Parse(format!("kline[{i}] not a string")))
        };
        let open = Price::from_raw(decimal_to_raw(s(1)?, price_scale)?);
        let high = Price::from_raw(decimal_to_raw(s(2)?, price_scale)?);
        let low = Price::from_raw(decimal_to_raw(s(3)?, price_scale)?);
        let close = Price::from_raw(decimal_to_raw(s(4)?, price_scale)?);
        let volume = Qty::from_raw(decimal_to_raw(s(5)?, qty_scale)?);
        let close_ms = row[6]
            .as_i64()
            .ok_or_else(|| MexcError::Parse("kline[6] (closeTime) not an int".into()))?;
        let ts = Timestamp::from_nanos(
            close_ms
                .checked_mul(1_000_000)
                .ok_or_else(|| MexcError::Parse("closeTime overflow".into()))?,
        );
        bars.push(Bar::new(instrument, ts, open, high, low, close, volume));
    }
    Ok(bars)
}

/// If `body` is a MEXC error object `{code, msg}` with a non-success code,
/// return the corresponding [`MexcError::Api`].
#[must_use]
pub fn parse_api_error(body: &str) -> Option<MexcError> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let code = v.get("code")?.as_i64()?;
    if code == 0 || code == 200 {
        return None;
    }
    let msg = v
        .get("msg")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    Some(MexcError::Api { code, msg })
}

/// A successfully-acknowledged order from `POST /api/v3/order`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderAck {
    /// The venue's order id (used to poll status).
    pub venue_order_id: String,
    /// `transactTime` in epoch-ms.
    pub transact_time_ms: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderAckRaw {
    order_id: serde_json::Value,
    #[serde(default)]
    transact_time: i64,
}

/// Parse a `POST /api/v3/order` response. Surfaces a structured API error first.
pub fn parse_order_ack(body: &str) -> Result<OrderAck, MexcError> {
    if let Some(err) = parse_api_error(body) {
        return Err(err);
    }
    let raw: OrderAckRaw =
        serde_json::from_str(body).map_err(|e| MexcError::Parse(e.to_string()))?;
    let venue_order_id = match raw.order_id {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        _ => return Err(MexcError::Parse("order response missing orderId".into())),
    };
    Ok(OrderAck {
        venue_order_id,
        transact_time_ms: raw.transact_time,
    })
}

/// Parsed order status (for REST-poll fill detection).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderStatus {
    /// MEXC status string (`NEW`/`PARTIALLY_FILLED`/`FILLED`/`CANCELED`/…).
    pub status: String,
    /// Cumulative executed base quantity.
    pub executed_qty: Qty,
    /// Cumulative quote spent/received.
    pub cummulative_quote: Money,
    /// Base-asset precision (so [`OrderStatus::avg_price`] can re-scale the
    /// quote/qty quotient back into a price).
    pub qty_scale: u32,
}

impl OrderStatus {
    /// `true` once the order can no longer change (filled, canceled, or expired).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            // `EXPIRED` is the terminal state of an unfilled/partly-filled IOC/FOK
            // (and a day order at session end); omitting it leaked such orders in
            // the poll set forever (M4).
            "FILLED" | "CANCELED" | "PARTIALLY_CANCELED" | "EXPIRED"
        )
    }

    /// Volume-weighted average fill price, or `None` if nothing has executed.
    #[must_use]
    pub fn avg_price(&self) -> Option<Price> {
        let q = self.executed_qty.raw();
        if q == 0 {
            return None;
        }
        // `cummulative_quote` carries `price_scale` and `executed_qty` carries
        // `qty_scale`. The real price is `quote/base`, so in raw units:
        //   price_raw = (quote_raw / 10^price_scale) / (base_raw / 10^qty_scale)
        //               * 10^price_scale  =  quote_raw * 10^qty_scale / base_raw.
        // The `10^price_scale` cancels; the `10^qty_scale` must be reintroduced
        // (omitting it under-reports the price by 10^qty_scale for any symbol with
        // nonzero base precision — i.e. essentially all of them).
        let scaled = self
            .cummulative_quote
            .raw()
            .saturating_mul(10i128.saturating_pow(self.qty_scale));
        // `try_from` rather than a wrapping `as i64`: an economically extreme
        // quotient must clamp, not wrap to a negative price (m14).
        Some(Price::from_raw(
            i64::try_from(scaled / i128::from(q)).unwrap_or(i64::MAX),
        ))
    }
}

/// Parse a `GET /api/v3/order` status response.
pub fn parse_order_status(
    body: &str,
    price_scale: u32,
    qty_scale: u32,
) -> Result<OrderStatus, MexcError> {
    if let Some(err) = parse_api_error(body) {
        return Err(err);
    }
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| MexcError::Parse(e.to_string()))?;
    let get = |k: &str| v.get(k).and_then(serde_json::Value::as_str);
    let status = get("status").unwrap_or("").to_owned();
    let executed = decimal_to_raw(get("executedQty").unwrap_or("0"), qty_scale)?;
    let cumq = i128::from(decimal_to_raw(
        get("cummulativeQuoteQty").unwrap_or("0"),
        price_scale,
    )?);
    Ok(OrderStatus {
        status,
        executed_qty: Qty::from_raw(executed),
        cummulative_quote: Money::from_raw(cumq),
        qty_scale,
    })
}

/// One executed trade from `GET /api/v3/myTrades`, carrying the **realised**
/// fee (so the connector can use the true fee instead of a modelled rate).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MexcTrade {
    /// Order this trade belongs to.
    pub order_id: String,
    /// Execution price.
    pub price: Price,
    /// Executed quantity.
    pub qty: Qty,
    /// Realised commission (fee). Parsed at `price_scale` (a documented
    /// approximation; the true scale is the commission asset's).
    pub commission: Money,
    /// Asset the commission was charged in (e.g. `"USDT"`, or `"MX"` if the
    /// MX-token discount applied).
    pub commission_asset: String,
    /// Whether the account was the buyer.
    pub is_buyer: bool,
    /// Event time.
    pub ts: Timestamp,
}

impl MexcTrade {
    /// The realised fee as a [`Cost`] in the given quote asset (taker bucket).
    #[must_use]
    pub fn fee_cost(&self, asset: AssetId) -> Cost {
        Cost::new(asset, self.commission, CostKind::Taker)
    }
}

/// Parse a `GET /api/v3/myTrades` response into trades with realised fees.
pub fn parse_my_trades(
    body: &str,
    price_scale: u32,
    qty_scale: u32,
) -> Result<Vec<MexcTrade>, MexcError> {
    if let Some(err) = parse_api_error(body) {
        return Err(err);
    }
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(body).map_err(|e| MexcError::Parse(e.to_string()))?;
    let mut out = Vec::with_capacity(rows.len());
    for v in rows {
        let s = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
        // Numeric fields default to "0" when absent (an empty string is not a
        // valid decimal); real fills always carry them.
        let num = |k: &str| {
            let raw = v.get(k).and_then(serde_json::Value::as_str).unwrap_or("0");
            if raw.is_empty() { "0" } else { raw }
        };
        out.push(MexcTrade {
            order_id: v.get("orderId").map_or_else(String::new, value_to_id),
            price: Price::from_raw(decimal_to_raw(num("price"), price_scale)?),
            qty: Qty::from_raw(decimal_to_raw(num("qty"), qty_scale)?),
            // Commission is decoded at `price_scale` — correct for the dominant
            // case (a spot taker fee charged in the QUOTE asset). A base-asset fee
            // (rare) would want `qty_scale`; selecting per `commissionAsset` needs
            // the asset symbols threaded in and the per-asset precision confirmed
            // live (the docs do not pin it), so it is left as documented future
            // work — `commission_asset` is captured below for that purpose.
            commission: Money::from_raw(i128::from(decimal_to_raw(
                num("commission"),
                price_scale,
            )?)),
            commission_asset: s("commissionAsset").to_owned(),
            is_buyer: v
                .get("isBuyer")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            ts: Timestamp::from_nanos(
                v.get("time")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0)
                    .saturating_mul(1_000_000),
            ),
        });
    }
    Ok(out)
}

fn value_to_id(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_my_trades_real_fee() {
        let body = r#"[
          {"orderId":"A1","price":"105.00","qty":"2.0","commission":"0.21","commissionAsset":"USDT","isBuyer":true,"time":1700000000123},
          {"orderId":42,"price":"106.00","qty":"1.0","commission":"0.05","commissionAsset":"MX","isBuyer":false,"time":1700000001000}
        ]"#;
        let trades = parse_my_trades(body, 2, 1).unwrap();
        assert_eq!(trades.len(), 2);
        assert_eq!(trades[0].order_id, "A1");
        assert_eq!(trades[0].price, Price::from_raw(10_500));
        assert_eq!(trades[0].qty, Qty::from_raw(20)); // 2.0 @ scale 1
        assert_eq!(trades[0].commission, Money::from_raw(21)); // 0.21 @ scale 2
        assert_eq!(trades[0].commission_asset, "USDT");
        assert!(trades[0].is_buyer);
        let cost = trades[0].fee_cost(AssetId::new(1));
        assert_eq!(cost.amount, Money::from_raw(21));
        assert_eq!(cost.kind, CostKind::Taker);
        // numeric orderId + MX-discount asset
        assert_eq!(trades[1].order_id, "42");
        assert_eq!(trades[1].commission_asset, "MX");
    }

    #[test]
    fn parse_my_trades_error_passthrough() {
        assert!(parse_my_trades(r#"{"code":700003,"msg":"ts"}"#, 2, 1).is_err());
        assert!(parse_my_trades("garbage", 2, 1).is_err());
    }

    #[test]
    fn parse_klines_ok() {
        // [openTime, open, high, low, close, volume, closeTime, quoteVol]
        let json = r#"[
          [1700000000000,"100.00","110.00","90.00","105.00","12.5",1700000059999,"1300"],
          [1700000060000,"105.00","106.00","104.00","104.50","3.0",1700000119999,"315"]
        ]"#;
        let bars = parse_klines(json, InstrumentId::new(0), 2, 1).unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].open, Price::from_raw(10_000));
        assert_eq!(bars[0].high, Price::from_raw(11_000));
        assert_eq!(bars[0].low, Price::from_raw(9_000));
        assert_eq!(bars[0].close, Price::from_raw(10_500));
        assert_eq!(bars[0].volume, Qty::from_raw(125)); // 12.5 @ scale 1
        assert_eq!(
            bars[0].ts,
            Timestamp::from_nanos(1_700_000_059_999 * 1_000_000)
        );
        assert_eq!(bars[1].close, Price::from_raw(10_450));
    }

    #[test]
    fn parse_klines_rejects_short_rows_and_bad_types() {
        assert!(parse_klines(r#"[[1,"2","3"]]"#, InstrumentId::new(0), 2, 1).is_err());
        assert!(parse_klines("[[1,2,3,4,5,6,7]]", InstrumentId::new(0), 2, 1).is_err()); // numbers not strings
        assert!(parse_klines("not json", InstrumentId::new(0), 2, 1).is_err());
    }

    #[test]
    fn parse_order_ack_ok_and_error() {
        let ack = parse_order_ack(
            r#"{"symbol":"BTCUSDT","orderId":"C02__123","transactTime":1700000000123}"#,
        )
        .unwrap();
        assert_eq!(ack.venue_order_id, "C02__123");
        assert_eq!(ack.transact_time_ms, 1_700_000_000_123);
        // numeric orderId is accepted too
        let ack2 = parse_order_ack(r#"{"orderId":98765,"transactTime":1}"#).unwrap();
        assert_eq!(ack2.venue_order_id, "98765");
        // structured API error surfaces
        let err =
            parse_order_ack(r#"{"code":700002,"msg":"Signature for this request is not valid"}"#)
                .unwrap_err();
        assert!(matches!(err, MexcError::Api { code: 700_002, .. }));
    }

    #[test]
    fn parse_api_error_distinguishes_success() {
        assert!(parse_api_error(r#"{"orderId":"1"}"#).is_none());
        assert!(parse_api_error(r#"{"code":0,"msg":"ok"}"#).is_none());
        assert!(parse_api_error(r#"{"code":200}"#).is_none());
        assert!(parse_api_error(r#"{"code":10007,"msg":"bad qty"}"#).is_some());
        assert!(parse_api_error("garbage").is_none());
    }

    #[test]
    fn parse_order_status_and_fill_helpers() {
        let body = r#"{"status":"FILLED","executedQty":"2.0","cummulativeQuoteQty":"21000"}"#;
        let st = parse_order_status(body, 2, 1).unwrap();
        assert_eq!(st.status, "FILLED");
        assert!(st.is_terminal());
        assert_eq!(st.executed_qty, Qty::from_raw(20)); // 2.0 @ scale 1
        // 21000 quote / 2.0 base = 10500.00 price; in raw (price_scale 2):
        // quote_raw(2_100_000) * 10^qty_scale(10) / base_raw(20) = 1_050_000.
        assert_eq!(st.avg_price(), Some(Price::from_raw(1_050_000)));

        let new = parse_order_status(
            r#"{"status":"NEW","executedQty":"0","cummulativeQuoteQty":"0"}"#,
            2,
            1,
        )
        .unwrap();
        assert!(!new.is_terminal());
        assert_eq!(new.avg_price(), None);

        // API error surfaces.
        assert!(parse_order_status(r#"{"code":700003,"msg":"ts"}"#, 2, 1).is_err());
    }
}

#[cfg(test)]
mod cov_tests {
    use super::*;

    #[test]
    fn order_ack_non_scalar_id_errors() {
        assert!(parse_order_ack(r#"{"orderId":[1,2],"transactTime":1}"#).is_err());
    }

    #[test]
    fn order_status_defaults_missing_fields() {
        let st = parse_order_status(r#"{"status":"NEW"}"#, 2, 1).unwrap();
        assert_eq!(st.executed_qty, Qty::from_raw(0));
        assert_eq!(st.cummulative_quote, Money::from_raw(0));
    }

    #[test]
    fn my_trades_defaults_missing_fields() {
        let trades = parse_my_trades(r#"[{"price":"100.00","qty":"1.0"}]"#, 2, 1).unwrap();
        assert_eq!(trades.len(), 1);
        assert!(trades[0].order_id.is_empty());
        assert_eq!(trades[0].commission, Money::from_raw(0));
        assert!(!trades[0].is_buyer);
    }

    #[test]
    fn my_trades_non_scalar_order_id_is_empty() {
        // An explicit non-string, non-number orderId exercises `value_to_id`'s
        // fallback arm (distinct from an *absent* field, handled above).
        let trades =
            parse_my_trades(r#"[{"orderId":null,"price":"100.00","qty":"1.0"}]"#, 2, 1).unwrap();
        assert!(trades[0].order_id.is_empty());
        let trades2 =
            parse_my_trades(r#"[{"orderId":[1],"price":"100.00","qty":"1.0"}]"#, 2, 1).unwrap();
        assert!(trades2[0].order_id.is_empty());
    }
}
