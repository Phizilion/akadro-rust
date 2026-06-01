// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decoding MEXC's **protobuf** WebSocket market/user-data frames into akadro
//! types — the integration the `ws` module documented as the remaining open loop.
//!
//! MEXC pushes `PushDataV3ApiWrapper` messages (a `channel` string plus a `oneof
//! body`); the kline body is field **308**, private deals **306** (the official
//! field tags are vendored in `proto/PushDataV3ApiWrapper.proto` +
//! `PublicSpotKlineV3Api.proto` + `PrivateDealsV3Api.proto`, so they are *sourced*,
//! not guessed). Rather than pull a build-time `protoc`/`prost` dependency into
//! the default build, this module reads the handful of fields we need with a small
//! standard-protobuf **wire** decoder — every value we care about is a length-
//! delimited decimal string or a varint, both of which are trivial and stable to
//! parse. The decoders are round-trip tested; final byte-level confirmation is the
//! credential-gated `#[ignore]` live test in `akadro-live`.
//!
//! All prices/quantities cross the boundary as **decimal strings** (per the proto),
//! so the no-float money rule holds — they go straight through [`decimal_to_raw`].
//!
//! MEXC's kline push reports the *in-progress* window repeatedly (no "closed"
//! flag), so [`KlineAggregator`] turns the stream into **closed** bars by emitting
//! a window only once the next window begins — preserving the no-look-ahead /
//! backtest-parity contract.

use akadro_core::{
    AccountEvent, AssetId, Bar, Cost, CostKind, Costs, InstrumentId, Money, Price, Qty, Side,
    Timestamp,
};

use crate::convert::decimal_to_raw;
use crate::error::MexcError;

// --- minimal protobuf wire reader --------------------------------------------

/// A cursor over a protobuf message body.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Read a base-128 varint.
    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            value |= u64::from(byte & 0x7f).checked_shl(shift)?;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
            if shift >= 64 {
                return None; // malformed
            }
        }
    }

    /// Read a `(field_number, wire_type)` tag.
    fn tag(&mut self) -> Option<(u64, u8)> {
        let key = self.varint()?;
        Some((key >> 3, (key & 0x07) as u8))
    }

    /// Read a length-delimited byte slice (wire type 2).
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = self.varint()? as usize;
        let end = self.pos.checked_add(len)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    /// Read a length-delimited UTF-8 string.
    fn string(&mut self) -> Option<&'a str> {
        core::str::from_utf8(self.bytes()?).ok()
    }

    /// Skip a field of the given wire type.
    fn skip(&mut self, wire: u8) -> Option<()> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => self.pos = self.pos.checked_add(8).filter(|&p| p <= self.buf.len())?,
            2 => {
                self.bytes()?;
            }
            5 => self.pos = self.pos.checked_add(4).filter(|&p| p <= self.buf.len())?,
            _ => return None,
        }
        Some(())
    }

    /// Find the length-delimited body of `field`, skipping everything else.
    fn find_message(&mut self, field: u64) -> Result<Option<&'a [u8]>, MexcError> {
        while !self.done() {
            let (f, wire) = self.tag().ok_or_else(|| bad("truncated tag"))?;
            if f == field && wire == 2 {
                return Ok(Some(self.bytes().ok_or_else(|| bad("truncated message"))?));
            }
            self.skip(wire).ok_or_else(|| bad("bad wire type"))?;
        }
        Ok(None)
    }
}

fn bad(msg: &str) -> MexcError {
    MexcError::Parse(format!("ws protobuf: {msg}"))
}

// --- kline (PublicSpotKlineV3Api, wrapper field 308) -------------------------

/// A decoded spot kline (one push frame); prices/qty are already fixed-point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WsKline {
    /// Instrument the kline belongs to.
    pub instrument: InstrumentId,
    /// Window start (epoch seconds) — identifies the window for roll-over.
    pub window_start: i64,
    /// Window end / close time (epoch seconds).
    pub window_end: i64,
    /// Open price.
    pub open: Price,
    /// High price.
    pub high: Price,
    /// Low price.
    pub low: Price,
    /// Close price.
    pub close: Price,
    /// Base-asset volume.
    pub volume: Qty,
}

impl WsKline {
    /// The bar this window represents, stamped at its close time.
    #[must_use]
    pub fn to_bar(self) -> Bar {
        Bar::new(
            self.instrument,
            Timestamp::from_nanos(self.window_end.saturating_mul(1_000_000_000)),
            self.open,
            self.high,
            self.low,
            self.close,
            self.volume,
        )
    }
}

fn decode_kline_body(
    body: &[u8],
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
) -> Result<WsKline, MexcError> {
    let mut r = Reader::new(body);
    let (mut window_start, mut window_end) = (0i64, 0i64);
    let (mut open, mut high, mut low, mut close, mut volume) = (0i64, 0i64, 0i64, 0i64, 0i64);
    while !r.done() {
        let (field, wire) = r.tag().ok_or_else(|| bad("truncated kline tag"))?;
        let px = |r: &mut Reader<'_>| -> Result<i64, MexcError> {
            decimal_to_raw(r.string().ok_or_else(|| bad("kline str"))?, price_scale)
        };
        match (field, wire) {
            (2, 0) => window_start = r.varint().ok_or_else(|| bad("windowStart"))? as i64,
            (9, 0) => window_end = r.varint().ok_or_else(|| bad("windowEnd"))? as i64,
            (3, 2) => open = px(&mut r)?,
            (4, 2) => close = px(&mut r)?,
            (5, 2) => high = px(&mut r)?,
            (6, 2) => low = px(&mut r)?,
            (7, 2) => {
                volume = decimal_to_raw(r.string().ok_or_else(|| bad("volume"))?, qty_scale)?;
            }
            _ => {
                r.skip(wire).ok_or_else(|| bad("kline skip"))?;
            }
        }
    }
    Ok(WsKline {
        instrument,
        window_start,
        window_end,
        open: Price::from_raw(open),
        high: Price::from_raw(high),
        low: Price::from_raw(low),
        close: Price::from_raw(close),
        volume: Qty::from_raw(volume),
    })
}

/// Decode a `PushDataV3ApiWrapper` frame's kline body (field 308), or `Ok(None)`
/// if the frame carries a different body.
///
/// # Errors
/// [`MexcError::Parse`] on malformed protobuf or a bad decimal.
pub fn decode_kline_push(
    bytes: &[u8],
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
) -> Result<Option<WsKline>, MexcError> {
    let mut r = Reader::new(bytes);
    match r.find_message(308)? {
        Some(body) => Ok(Some(decode_kline_body(
            body,
            instrument,
            price_scale,
            qty_scale,
        )?)),
        None => Ok(None),
    }
}

/// Turns the repeated in-progress kline pushes for **one** instrument into closed
/// bars: [`KlineAggregator::push`] returns the previous window's [`Bar`] only when
/// a new window begins, so a partial (current) window is never surfaced.
#[derive(Debug, Default)]
pub struct KlineAggregator {
    last: Option<WsKline>,
}

impl KlineAggregator {
    /// A fresh aggregator with no window seen yet.
    #[must_use]
    pub fn new() -> Self {
        KlineAggregator { last: None }
    }

    /// Feed a freshly-decoded kline; returns `Some(Bar)` for the now-closed prior
    /// window when `kline` opens a new window, else `None`.
    pub fn push(&mut self, kline: WsKline) -> Option<Bar> {
        let closed = match self.last {
            Some(prev) if prev.window_start != kline.window_start => Some(prev.to_bar()),
            _ => None,
        };
        self.last = Some(kline);
        closed
    }
}

// --- private deals (PrivateDealsV3Api, wrapper field 306) --------------------

/// A decoded private fill (one `spot@private.deals` frame).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct WsDeal {
    /// Fill price (decimal string, venue scale).
    pub price: String,
    /// Fill quantity (decimal string, venue scale).
    pub quantity: String,
    /// Side (`BUY`/`SELL` derived from `tradeType` 1/2).
    pub side: Side,
    /// Venue `clientOrderId` (akadro id recovered via [`crate::ws::client_order_tag`]).
    pub client_order_id: String,
    /// Fee amount (decimal string).
    pub fee_amount: String,
    /// Fee currency.
    pub fee_currency: String,
    /// Trade time (epoch ms).
    pub time_ms: i64,
}

/// Decode a `PushDataV3ApiWrapper` frame's private-deal body (field 306), or
/// `Ok(None)` for a different body / unrecognized side.
///
/// # Errors
/// [`MexcError::Parse`] on malformed protobuf.
pub fn decode_private_deal(bytes: &[u8]) -> Result<Option<WsDeal>, MexcError> {
    let mut outer = Reader::new(bytes);
    let Some(body) = outer.find_message(306)? else {
        return Ok(None);
    };
    let mut r = Reader::new(body);
    let (mut price, mut quantity, mut cid, mut fee, mut fee_ccy) = ("", "", "", "", "");
    let (mut trade_type, mut time_ms) = (0i64, 0i64);
    while !r.done() {
        let (field, wire) = r.tag().ok_or_else(|| bad("truncated deal tag"))?;
        match (field, wire) {
            (1, 2) => price = r.string().ok_or_else(|| bad("price"))?,
            (2, 2) => quantity = r.string().ok_or_else(|| bad("quantity"))?,
            (4, 0) => trade_type = r.varint().ok_or_else(|| bad("tradeType"))? as i64,
            (8, 2) => cid = r.string().ok_or_else(|| bad("clientOrderId"))?,
            (10, 2) => fee = r.string().ok_or_else(|| bad("feeAmount"))?,
            (11, 2) => fee_ccy = r.string().ok_or_else(|| bad("feeCurrency"))?,
            (12, 0) => time_ms = r.varint().ok_or_else(|| bad("time"))? as i64,
            _ => {
                r.skip(wire).ok_or_else(|| bad("deal skip"))?;
            }
        }
    }
    let side = match trade_type {
        1 => Side::Buy,
        2 => Side::Sell,
        _ => return Ok(None),
    };
    Ok(Some(WsDeal {
        price: price.to_owned(),
        quantity: quantity.to_owned(),
        side,
        client_order_id: cid.to_owned(),
        fee_amount: fee.to_owned(),
        fee_currency: fee_ccy.to_owned(),
        time_ms,
    }))
}

impl WsDeal {
    /// Build an [`AccountEvent::Fill`] for `instrument` (with `quote_asset` and
    /// scales). Returns `None` unless the order was tagged by this connector
    /// ([`crate::ws::client_order_tag`]). Commission is charged in `quote_asset` at
    /// `price_scale` (the dominant quote-fee case; per-asset fee scale is the same
    /// documented future refinement as the REST path).
    #[must_use]
    pub fn to_fill(
        &self,
        instrument: InstrumentId,
        quote_asset: AssetId,
        price_scale: u32,
        qty_scale: u32,
    ) -> Option<AccountEvent> {
        let id = crate::ws::parse_client_order_tag(&self.client_order_id)?;
        let price = Price::from_raw(decimal_to_raw(&self.price, price_scale).ok()?);
        let qty = Qty::from_raw(decimal_to_raw(&self.quantity, qty_scale).ok()?);
        let mut costs = Costs::new();
        if let Ok(fee) = decimal_to_raw(&self.fee_amount, price_scale)
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
            // A single deal frame is ONE execution and carries no remaining-quantity,
            // so it cannot know whether it completed the order. Reporting `true`
            // here would evict the order from open-tracking on its FIRST partial
            // fill (M3); emit `false` and let the order-update / status path close
            // the order when it is actually done.
            complete: false,
            ts: Timestamp::from_nanos(self.time_ms.saturating_mul(1_000_000)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::ClientOrderId;

    // --- a tiny protobuf encoder, only for building decoder fixtures ----------
    fn varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                break;
            }
        }
    }
    fn tag(out: &mut Vec<u8>, field: u64, wire: u8) {
        varint(out, (field << 3) | u64::from(wire));
    }
    fn field_str(out: &mut Vec<u8>, field: u64, s: &str) {
        tag(out, field, 2);
        varint(out, s.len() as u64);
        out.extend_from_slice(s.as_bytes());
    }
    fn field_varint(out: &mut Vec<u8>, field: u64, v: u64) {
        tag(out, field, 0);
        varint(out, v);
    }
    fn field_msg(out: &mut Vec<u8>, field: u64, body: &[u8]) {
        tag(out, field, 2);
        varint(out, body.len() as u64);
        out.extend_from_slice(body);
    }

    fn kline_wrapper() -> Vec<u8> {
        let mut k = Vec::new();
        field_str(&mut k, 1, "Min1"); // interval
        field_varint(&mut k, 2, 1_700_000_000); // windowStart (s)
        field_str(&mut k, 3, "100.00"); // open
        field_str(&mut k, 4, "100.50"); // close
        field_str(&mut k, 5, "101.00"); // high
        field_str(&mut k, 6, "99.00"); // low
        field_str(&mut k, 7, "12.5"); // volume
        field_str(&mut k, 8, "1250"); // amount (ignored)
        field_varint(&mut k, 9, 1_700_000_059); // windowEnd (s)
        let mut w = Vec::new();
        field_str(&mut w, 1, "spot@public.kline.v3.api.pb@BTCUSDT@Min1"); // channel
        field_msg(&mut w, 308, &k); // publicSpotKline body
        field_str(&mut w, 3, "BTCUSDT"); // symbol
        w
    }

    #[test]
    fn decodes_kline_push() {
        let bytes = kline_wrapper();
        let k = decode_kline_push(&bytes, InstrumentId::new(0), 2, 1)
            .unwrap()
            .expect("a kline body");
        assert_eq!(k.window_start, 1_700_000_000);
        assert_eq!(k.window_end, 1_700_000_059);
        assert_eq!(k.open, Price::from_raw(10_000)); // 100.00 @ 2
        assert_eq!(k.close, Price::from_raw(10_050));
        assert_eq!(k.high, Price::from_raw(10_100));
        assert_eq!(k.low, Price::from_raw(9_900));
        assert_eq!(k.volume, Qty::from_raw(125)); // 12.5 @ 1
        let bar = k.to_bar();
        assert_eq!(bar.ts.as_nanos(), 1_700_000_059_000_000_000);
    }

    #[test]
    fn non_kline_frame_returns_none() {
        // A wrapper carrying only a channel + an unrelated body (field 305).
        let mut w = Vec::new();
        field_str(&mut w, 1, "spot@public.bookTicker.v3.api.pb@BTCUSDT");
        field_msg(&mut w, 305, &[0x08, 0x01]);
        assert!(
            decode_kline_push(&w, InstrumentId::new(0), 2, 1)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn aggregator_emits_only_closed_windows() {
        let mut agg = KlineAggregator::new();
        let mk = |start: i64, close_raw: i64| WsKline {
            instrument: InstrumentId::new(0),
            window_start: start,
            window_end: start + 59,
            open: Price::from_raw(1),
            high: Price::from_raw(1),
            low: Price::from_raw(1),
            close: Price::from_raw(close_raw),
            volume: Qty::from_raw(1),
        };
        // Two pushes for the SAME window → nothing closed yet.
        assert!(agg.push(mk(1_700_000_000, 100)).is_none());
        assert!(agg.push(mk(1_700_000_000, 105)).is_none());
        // A new window closes the previous one (with its last-seen close, 105).
        let bar = agg
            .push(mk(1_700_000_060, 110))
            .expect("prior window closed");
        assert_eq!(bar.close, Price::from_raw(105));
        assert_eq!(bar.ts.as_nanos(), 1_700_000_059_000_000_000);
    }

    fn deal_wrapper(client_order_id: &str, trade_type: u64) -> Vec<u8> {
        let mut d = Vec::new();
        field_str(&mut d, 1, "105.00"); // price
        field_str(&mut d, 2, "1.000"); // quantity
        field_varint(&mut d, 4, trade_type); // tradeType
        field_str(&mut d, 8, client_order_id); // clientOrderId
        field_str(&mut d, 10, "0.05"); // feeAmount
        field_str(&mut d, 11, "USDT"); // feeCurrency
        field_varint(&mut d, 12, 1_700_000_000_500); // time (ms)
        let mut w = Vec::new();
        field_str(&mut w, 1, "spot@private.deals.v3.api.pb");
        field_msg(&mut w, 306, &d);
        w
    }

    #[test]
    fn decodes_private_deal_to_fill() {
        let bytes = deal_wrapper("ak42", 1);
        let deal = decode_private_deal(&bytes).unwrap().expect("a deal");
        assert_eq!(deal.side, Side::Buy);
        assert_eq!(deal.fee_currency, "USDT");
        let fill = deal
            .to_fill(InstrumentId::new(0), AssetId::new(1), 2, 3)
            .expect("tagged → fill");
        let AccountEvent::Fill {
            client_order_id,
            price,
            qty,
            side,
            costs,
            ts,
            ..
        } = fill
        else {
            panic!("expected Fill");
        };
        assert_eq!(client_order_id, ClientOrderId::new(42));
        assert_eq!(price, Price::from_raw(10_500)); // 105.00 @ 2
        assert_eq!(qty, Qty::from_raw(1_000)); // 1.000 @ 3
        assert_eq!(side, Side::Buy);
        assert_eq!(costs.iter().next().unwrap().amount, Money::from_raw(5)); // 0.05 @ 2
        assert_eq!(ts.as_nanos(), 1_700_000_000_500_000_000);
    }

    #[test]
    fn skips_other_wire_types_and_rejects_unknown_ones() {
        let mut k = Vec::new();
        field_varint(&mut k, 2, 1_700_000_000);
        for f in 3..=7 {
            field_str(&mut k, f, "1");
        }
        field_varint(&mut k, 9, 1_700_000_059);
        // Wrapper with a 64-bit field (wire 1) and a 32-bit field (wire 5) that the
        // decoder must skip before reaching the kline body.
        let mut w = Vec::new();
        tag(&mut w, 100, 1);
        w.extend_from_slice(&[0u8; 8]);
        tag(&mut w, 101, 5);
        w.extend_from_slice(&[0u8; 4]);
        field_msg(&mut w, 308, &k);
        assert!(
            decode_kline_push(&w, InstrumentId::new(0), 2, 1)
                .unwrap()
                .is_some()
        );

        // An unknown wire type (3, group-start) cannot be skipped → decode error.
        let mut bad = Vec::new();
        tag(&mut bad, 50, 3);
        assert!(decode_kline_push(&bad, InstrumentId::new(0), 2, 1).is_err());
    }

    #[test]
    fn malformed_varint_and_bad_decimal_error() {
        // A kline body whose first tag is a 10-byte all-continuation varint overflows.
        let mut w = Vec::new();
        tag(&mut w, 308, 2);
        let body = vec![0xffu8; 10];
        varint(&mut w, body.len() as u64);
        w.extend_from_slice(&body);
        assert!(decode_kline_push(&w, InstrumentId::new(0), 2, 1).is_err());

        // A truncated length-delimited field (claims more bytes than remain).
        let mut trunc = Vec::new();
        tag(&mut trunc, 308, 2);
        varint(&mut trunc, 50); // claims 50 bytes, none follow
        assert!(decode_kline_push(&trunc, InstrumentId::new(0), 2, 1).is_err());

        // to_fill with an unparseable price → no fill.
        let mut deal = decode_private_deal(&deal_wrapper("ak1", 1))
            .unwrap()
            .unwrap();
        deal.price = "not-a-number".to_owned();
        assert!(
            deal.to_fill(InstrumentId::new(0), AssetId::new(1), 2, 3)
                .is_none()
        );
    }

    #[test]
    fn untagged_or_unknown_side_deal_yields_no_fill() {
        // A foreign clientOrderId → no engine fill.
        let bytes = deal_wrapper("web_xyz", 2);
        let deal = decode_private_deal(&bytes).unwrap().unwrap();
        assert_eq!(deal.side, Side::Sell);
        assert!(
            deal.to_fill(InstrumentId::new(0), AssetId::new(1), 2, 3)
                .is_none()
        );
        // tradeType 0 is unrecognized → decode returns None.
        assert!(
            decode_private_deal(&deal_wrapper("ak1", 0))
                .unwrap()
                .is_none()
        );
    }
}
