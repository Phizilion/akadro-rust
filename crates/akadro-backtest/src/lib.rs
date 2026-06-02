// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # akadro-backtest
//!
//! The backtest building blocks: a [`HistoricalFeed`] (a `DataSource` that
//! replays stored events) and a [`SimulatedExchange`] (a conservative,
//! deterministic `ExecutionClient`). Plug them into an
//! [`akadro_engine::Engine`] to run a backtest; swap the feed for a live one and
//! the *same* strategy runs live (goal 2).
//!
//! ```
//! // The raw `HistoricalFeed::from_bars` constructor is gated behind `import-bars`
//! // (D18); this example shows it under that feature. The default data path is
//! // `akadro_data::load_or_cache_feed` (cache → DataSource) or a connector feed.
//! # #[cfg(feature = "import-bars")] {
//! use akadro_backtest::{HistoricalFeed, SimulatedExchange};
//! use akadro_engine::{Engine, Strategy, Ctx};
//! use akadro_core::{Bar, InstrumentSpec, InstrumentId, AssetId, InstrumentKind,
//!     CapSet, Price, Qty, Money, Side, OrderRequest, Timestamp};
//!
//! struct Buy1;
//! impl Strategy for Buy1 {
//!     fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
//!         ctx.submit(OrderRequest::market(InstrumentId::new(0), Side::Buy, Qty::from_raw(1)));
//!     }
//! }
//!
//! let spec = InstrumentSpec::new(InstrumentId::new(0), AssetId::new(0), AssetId::new(1),
//!     InstrumentKind::Spot, Price::from_raw(1), Qty::from_raw(1), Money::ZERO, CapSet::empty());
//! let bars = vec![
//!     Bar::new(InstrumentId::new(0), Timestamp::from_nanos(1), Price::from_raw(100),
//!         Price::from_raw(100), Price::from_raw(100), Price::from_raw(100), Qty::from_raw(1)),
//!     Bar::new(InstrumentId::new(0), Timestamp::from_nanos(2), Price::from_raw(101),
//!         Price::from_raw(101), Price::from_raw(101), Price::from_raw(101), Qty::from_raw(1)),
//! ];
//! let report = Engine::new(
//!     &[spec], Money::from_raw(1_000_000),
//!     HistoricalFeed::from_bars(bars), SimulatedExchange::new(vec![spec], 0), Buy1,
//! ).unwrap().run();
//! assert_eq!(report.bars_processed, 2);
//! # }
//! ```

mod exchange;
mod feed;
pub mod replay;
mod walk_forward_run;

pub use akadro_core::BarOrderError;
pub use exchange::{FillConfig, SimulatedExchange};
pub use feed::HistoricalFeed;
pub use replay::{ReplayDiff, ReplayFeed, ReplayStrategy, diff_reports, replay_trades};
pub use walk_forward_run::{OosFold, WalkForwardBacktest};
