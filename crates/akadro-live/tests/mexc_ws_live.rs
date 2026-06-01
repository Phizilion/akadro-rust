// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Network-gated live smoke test of the MEXC protobuf WebSocket transport (the
//! documented OPEN LOOP — the one path the API research could not byte-verify from
//! docs alone). `#[ignore]`d so CI never runs it; run deliberately:
//!
//! ```text
//! cargo test -p akadro-live --features mexc -- --ignored --nocapture
//! ```
//!
//! It subscribes to the **public** `Min1` kline stream (no credentials), proving
//! the real `tokio-tungstenite` + TLS path, the JSON subscribe handshake, and the
//! `decode_kline_push` protobuf decoder all work end to end. The
//! [`KlineAggregator`](akadro_venue_mexc::KlineAggregator) emits a closed bar on
//! window roll-over, so the first bar can take up to ~2 minutes of `Min1` windows.
#![cfg(feature = "mexc")]

use akadro_core::{DataSource, Event, InstrumentId};
use akadro_live::mexc::{KlineStream, SPOT_WS_BASE, spawn_klines};

#[test]
#[ignore = "requires network access to wbs.mexc.com (may block ~1–2 min for a closed Min1 bar)"]
fn live_protobuf_kline_stream_yields_a_bar() {
    let mut bridge = spawn_klines(
        SPOT_WS_BASE,
        KlineStream {
            symbol: "BTCUSDT".to_owned(),
            interval: "Min1".to_owned(),
            instrument: InstrumentId::new(0),
            price_scale: 2,
            qty_scale: 6,
        },
        64,
    );
    let event = bridge.next_event().expect("a live closed bar");
    assert!(
        matches!(event, Event::Bar(_)),
        "expected a Bar, got {event:?}"
    );
    if let Event::Bar(bar) = event {
        assert!(bar.high.raw() >= bar.low.raw(), "sane OHLC");
        assert!(bar.close.raw() > 0, "positive price");
    }
}
