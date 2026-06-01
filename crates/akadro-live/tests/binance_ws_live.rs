// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Network-gated live smoke test of the Binance WebSocket transport (the
//! documented OPEN LOOP). `#[ignore]`d so CI never runs it; run deliberately:
//!
//! ```text
//! cargo test -p akadro-live --features binance -- --ignored
//! ```
//!
//! It connects to the **public** spot kline stream (no credentials) and asserts a
//! closed `1s` bar arrives — exercising the real `tokio-tungstenite` + TLS path
//! and the `parse_ws_kline` decoder end to end. Authenticated user-data flow is
//! validated separately with a `listenKey` against the live account.
#![cfg(feature = "binance")]

use akadro_core::{DataSource, Event, InstrumentId};
use akadro_live::binance::{KlineStream, SPOT_WS_BASE, spawn_klines};

#[test]
#[ignore = "requires network access to stream.binance.com"]
fn live_kline_stream_yields_a_bar() {
    let bridge = spawn_klines(
        SPOT_WS_BASE,
        KlineStream {
            symbol: "BTCUSDT".to_owned(),
            interval: "1s".to_owned(),
            instrument: InstrumentId::new(0),
            price_scale: 2,
            qty_scale: 5,
        },
        64,
    );
    let mut bridge = bridge;
    // Blocks until the first *closed* 1-second kline arrives (≈1–2s live).
    let event = bridge.next_event().expect("a live bar");
    assert!(
        matches!(event, Event::Bar(_)),
        "expected a Bar, got {event:?}"
    );
    if let Event::Bar(bar) = event {
        assert!(bar.high.raw() >= bar.low.raw(), "sane OHLC");
        assert!(bar.close.raw() > 0, "positive price");
    }
}
