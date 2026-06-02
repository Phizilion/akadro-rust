// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # akadro
//!
//! Exchange-agnostic backtesting and live trading for Rust, with a
//! **compile-time guarantee against look-ahead** and **backtest↔live parity**.
//!
//! This umbrella crate re-exports the workspace under tidy module paths and
//! provides a [`prelude`] for the common case. See the individual crates for
//! detail:
//!
//! * [`types`] (`akadro-core`) — domain vocabulary + the exchange-extension traits.
//! * [`engine`] (`akadro-engine`) — the event loop, the `Strategy` trait, and the
//!   look-ahead-safe `Ctx`/`Series` (the kill feature).
//! * [`backtest`] (`akadro-backtest`) — simulated exchange + historical feed
//!   (enabled by the default `backtest` feature).
//!
//! ```
//! // Gated on `backtest` (the default feature) for `SimulatedExchange`, so a
//! // `--no-default-features` build compiles an empty body (m39). Data reaches the
//! // engine via a `DataSource` — the blessed path is `akadro::data::load_or_cache_feed`
//! // (cache → feed) or a venue connector feed; for an ad-hoc source you implement
//! // `DataSource` (shown here with an empty one). The raw `HistoricalFeed::from_bars`
//! // Vec-injection constructor is gated behind the `import-bars` feature (D18).
//! # #[cfg(feature = "backtest")] {
//! use akadro::prelude::*;
//! use akadro::types::{DataSource, Event};
//!
//! struct Flat;
//! impl Strategy for Flat {
//!     fn on_bar(&mut self, _bar: Bar, _ctx: &mut Ctx<'_>) {}
//! }
//! struct NoData;
//! impl DataSource for NoData {
//!     fn next_event(&mut self) -> Option<Event> { None }
//! }
//!
//! let spec = InstrumentSpec::new(InstrumentId::new(0), AssetId::new(0), AssetId::new(1),
//!     InstrumentKind::Spot, Price::from_raw(1), Qty::from_raw(1), Money::ZERO, CapSet::empty());
//! let report = Engine::new(
//!     &[spec], Money::ZERO, NoData,
//!     SimulatedExchange::new(Vec::new(), 0), Flat,
//! ).unwrap().run();
//! assert_eq!(report.bars_processed, 0);
//! # }
//! ```

#[cfg(feature = "analytics")]
#[doc(inline)]
pub use akadro_analytics as analytics;
#[cfg(feature = "backtest")]
#[doc(inline)]
pub use akadro_backtest as backtest;
#[doc(inline)]
pub use akadro_core as types;
#[cfg(feature = "data")]
#[doc(inline)]
pub use akadro_data as data;
#[doc(inline)]
pub use akadro_engine as engine;
#[cfg(feature = "indicators")]
#[doc(inline)]
pub use akadro_indicators as indicators;
#[cfg(feature = "live")]
#[doc(inline)]
pub use akadro_live as live;
#[cfg(feature = "testkit")]
#[doc(inline)]
pub use akadro_testkit as testkit;

/// The common imports for writing and running strategies.
pub mod prelude {
    #[cfg(feature = "backtest")]
    pub use akadro_backtest::{HistoricalFeed, SimulatedExchange};
    pub use akadro_engine::prelude::*;
    #[cfg(feature = "testkit")]
    pub use akadro_testkit::MockLiveFeed;
}
