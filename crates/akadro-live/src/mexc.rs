// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Real MEXC **WebSocket** transport (the `mexc` feature).
//!
//! Like the Binance transport, the async runtime is confined here: each spawn runs
//! a single-threaded tokio runtime owning a `tokio-tungstenite` connection,
//! decodes MEXC's **protobuf** frames with the venue crate's pure decoders
//! ([`akadro_venue_mexc::decode_kline_push`] / [`akadro_venue_mexc::decode_private_deal`]),
//! and hands results to the synchronous engine over std channels.
//!
//! MEXC needs an explicit JSON **subscribe** handshake and a periodic **PING**
//! (both built by [`akadro_venue_mexc::ws`]); the market/user-data payloads then
//! arrive as binary protobuf. A kline push reports the in-progress window, so
//! [`spawn_klines`] runs it through a [`KlineAggregator`](akadro_venue_mexc::KlineAggregator)
//! to surface only **closed** bars (parity-safe).

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use akadro_core::{AccountEvent, AssetId, Event, InstrumentId};
use akadro_venue_mexc::{
    KlineAggregator, decode_kline_push, decode_private_deal, kline_channel, ping_message,
    subscribe_message,
};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::{BoundedBridge, BridgeSender, bounded_bridge};

/// Spot WebSocket base (`wss://wbs.mexc.com/ws`).
pub const SPOT_WS_BASE: &str = "wss://wbs.mexc.com/ws";

const PING_INTERVAL: Duration = Duration::from_secs(15);

/// A kline subscription for one instrument.
#[derive(Debug, Clone)]
pub struct KlineStream {
    /// Venue symbol, e.g. `"BTCUSDT"`.
    pub symbol: String,
    /// MEXC WS interval token, e.g. `"Min1"`, `"Min15"`.
    pub interval: String,
    /// The akadro instrument these bars belong to.
    pub instrument: InstrumentId,
    /// Price fixed-point scale.
    pub price_scale: u32,
    /// Quantity fixed-point scale.
    pub qty_scale: u32,
}

/// How to decode a private deal for one instrument's account stream.
#[derive(Debug, Clone)]
pub struct UserDataSymbol {
    /// The akadro instrument the deals map to.
    pub instrument: InstrumentId,
    /// Quote asset to charge commission in.
    pub quote_asset: AssetId,
    /// Price fixed-point scale.
    pub price_scale: u32,
    /// Quantity fixed-point scale.
    pub qty_scale: u32,
}

fn current_thread_runtime() -> Option<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
}

/// Connect to MEXC's protobuf kline WebSocket for `spec`, subscribing and pinging,
/// and stream **closed** bars into a lossless-or-fail bridge (buffering up to
/// `capacity`). The returned [`BoundedBridge`] is the engine's `DataSource`.
#[must_use]
pub fn spawn_klines(ws_base: &str, spec: KlineStream, capacity: usize) -> BoundedBridge {
    let (tx, bridge) = bounded_bridge(capacity);
    let url = ws_base.to_owned();
    thread::spawn(move || {
        let Some(rt) = current_thread_runtime() else {
            return;
        };
        rt.block_on(run_klines(url, spec, tx));
    });
    bridge
}

async fn run_klines(url: String, spec: KlineStream, tx: BridgeSender) {
    let Ok((ws, _)) = connect_async(&url).await else {
        return;
    };
    let (mut write, mut read) = ws.split();
    let channel = kline_channel(&spec.symbol, &spec.interval);
    if write
        .send(Message::Text(subscribe_message(&[channel])))
        .await
        .is_err()
    {
        return;
    }
    let mut aggregator = KlineAggregator::new();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if write.send(Message::Text(ping_message())).await.is_err() {
                    break;
                }
            }
            incoming = read.next() => {
                let Some(Ok(msg)) = incoming else { break };
                if let Message::Binary(bytes) = msg
                    && let Ok(Some(kline)) =
                        decode_kline_push(bytes.as_ref(), spec.instrument, spec.price_scale, spec.qty_scale)
                    && let Some(bar) = aggregator.push(kline)
                    && tx.send(Event::Bar(bar)).is_err()
                {
                    break; // bridge overflowed → consumer aborts (D7).
                }
            }
        }
    }
}

/// Connect to MEXC's private-deals protobuf stream (after subscribing to
/// `channels`, e.g. `["spot@private.deals.v3.api.pb"]`) and stream fills as
/// [`AccountEvent`]s into the returned channel for a
/// [`ChannelExec`](crate::ChannelExec). Only orders tagged by this connector
/// (`akadro_venue_mexc::client_order_tag`) are attributed.
#[must_use]
pub fn spawn_user_data(
    ws_base: &str,
    channels: Vec<String>,
    symbol: UserDataSymbol,
) -> Receiver<AccountEvent> {
    let (tx, rx) = channel();
    let url = ws_base.to_owned();
    thread::spawn(move || {
        let Some(rt) = current_thread_runtime() else {
            return;
        };
        rt.block_on(run_user_data(url, channels, symbol, tx));
    });
    rx
}

async fn run_user_data(
    url: String,
    channels: Vec<String>,
    symbol: UserDataSymbol,
    tx: Sender<AccountEvent>,
) {
    let Ok((ws, _)) = connect_async(&url).await else {
        return;
    };
    let (mut write, mut read) = ws.split();
    if write
        .send(Message::Text(subscribe_message(&channels)))
        .await
        .is_err()
    {
        return;
    }
    let mut ping = tokio::time::interval(PING_INTERVAL);
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if write.send(Message::Text(ping_message())).await.is_err() {
                    break;
                }
            }
            incoming = read.next() => {
                let Some(Ok(msg)) = incoming else { break };
                if let Message::Binary(bytes) = msg
                    && let Ok(Some(deal)) = decode_private_deal(bytes.as_ref())
                    && let Some(event) = deal.to_fill(
                        symbol.instrument,
                        symbol.quote_asset,
                        symbol.price_scale,
                        symbol.qty_scale,
                    )
                    && tx.send(event).is_err()
                {
                    break; // consumer gone.
                }
            }
        }
    }
}
