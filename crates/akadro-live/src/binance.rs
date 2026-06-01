// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Real Binance **WebSocket** transport (the `binance` feature).
//!
//! This is the only place an async runtime touches akadro: each spawn starts a
//! background thread running a single-threaded tokio runtime that owns a
//! `tokio-tungstenite` connection, decodes frames with the venue crate's *pure*
//! decoders ([`akadro_venue_binance::ws`]), and hands the results to the
//! synchronous engine over std channels — tokio types never escape this module.
//!
//! * [`spawn_klines`] streams **closed** bars into a lossless-or-fail
//!   [`BoundedBridge`](crate::BoundedBridge) (the engine's `DataSource`). Wrap it
//!   in [`ReconnectingFeed`](crate::ReconnectingFeed) for auto-reconnect.
//! * [`spawn_user_data`] streams `executionReport` fills/acks as
//!   [`AccountEvent`]s into a channel for a [`ChannelExec`](crate::ChannelExec)
//!   (the engine's `ExecutionClient`).
//!
//! Order submission itself stays on the signed REST path
//! (`akadro_venue_binance::BinanceSpotExec`); this module carries only the two
//! push streams.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use akadro_core::{AccountEvent, AssetId, Event, InstrumentId};
use akadro_venue_binance::{parse_execution_report, parse_ws_kline};
use futures_util::StreamExt;
use tokio_tungstenite::connect_async;

use crate::{BoundedBridge, BridgeSender, bounded_bridge};

/// Spot market-stream base (`wss://stream.binance.com:9443/ws`).
pub const SPOT_WS_BASE: &str = "wss://stream.binance.com:9443/ws";
/// USDⓈ-M futures market-stream base (`wss://fstream.binance.com/ws`).
pub const FUTURES_WS_BASE: &str = "wss://fstream.binance.com/ws";

/// A kline subscription for one instrument.
#[derive(Debug, Clone)]
pub struct KlineStream {
    /// Venue symbol, e.g. `"BTCUSDT"` (lower-cased in the URL automatically).
    pub symbol: String,
    /// Kline interval, e.g. `"1m"`, `"1s"`.
    pub interval: String,
    /// The akadro instrument these bars belong to.
    pub instrument: InstrumentId,
    /// Price fixed-point scale.
    pub price_scale: u32,
    /// Quantity fixed-point scale.
    pub qty_scale: u32,
}

/// How to decode user-data fills for one symbol.
#[derive(Debug, Clone)]
pub struct UserDataSymbol {
    /// Venue symbol the report's `s` field carries.
    pub symbol: String,
    /// The akadro instrument it maps to.
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

/// Connect to Binance's kline WebSocket for `spec` and stream **closed** bars into
/// a lossless-or-fail bridge (buffering up to `capacity`). The returned
/// [`BoundedBridge`] is the engine's `DataSource`; when the socket closes the
/// producer drops and the bridge ends the session (`next_event` → `None`).
#[must_use]
pub fn spawn_klines(ws_base: &str, spec: KlineStream, capacity: usize) -> BoundedBridge {
    let (tx, bridge) = bounded_bridge(capacity);
    let url = format!(
        "{ws_base}/{}@kline_{}",
        spec.symbol.to_lowercase(),
        spec.interval
    );
    thread::spawn(move || {
        let Some(rt) = current_thread_runtime() else {
            return;
        };
        rt.block_on(run_klines(url, spec, tx));
    });
    bridge
}

async fn run_klines(url: String, spec: KlineStream, tx: BridgeSender) {
    let Ok((mut stream, _)) = connect_async(&url).await else {
        return;
    };
    while let Some(Ok(msg)) = stream.next().await {
        let Ok(text) = msg.to_text() else { continue };
        if let Ok(Some(bar)) =
            parse_ws_kline(text, spec.instrument, spec.price_scale, spec.qty_scale)
            && tx.send(Event::Bar(bar)).is_err()
        {
            break; // bridge overflowed → stop producing; the consumer aborts (D7).
        }
    }
}

/// Connect to the user-data stream for `listen_key` and stream `executionReport`
/// fills/acks as [`AccountEvent`]s into the returned channel — hand it to
/// [`ChannelExec::new`](crate::ChannelExec::new). Reports for symbols not in
/// `symbols`, or orders not tagged by this connector, are ignored. The producer
/// ends (and the receiver closes) when the socket closes.
#[must_use]
pub fn spawn_user_data(
    ws_base: &str,
    listen_key: &str,
    symbols: Vec<UserDataSymbol>,
) -> Receiver<AccountEvent> {
    let (tx, rx) = channel();
    let url = format!("{ws_base}/{listen_key}");
    thread::spawn(move || {
        let Some(rt) = current_thread_runtime() else {
            return;
        };
        rt.block_on(run_user_data(url, symbols, tx));
    });
    rx
}

async fn run_user_data(url: String, symbols: Vec<UserDataSymbol>, tx: Sender<AccountEvent>) {
    let Ok((mut stream, _)) = connect_async(&url).await else {
        return;
    };
    while let Some(Ok(msg)) = stream.next().await {
        let Ok(text) = msg.to_text() else { continue };
        let Ok(Some(report)) = parse_execution_report(text) else {
            continue;
        };
        let Some(sym) = symbols.iter().find(|s| s.symbol == report.symbol) else {
            continue;
        };
        if let Some(event) = report.to_account_event(
            sym.instrument,
            sym.quote_asset,
            sym.price_scale,
            sym.qty_scale,
        ) && tx.send(event).is_err()
        {
            break; // consumer gone.
        }
    }
}
