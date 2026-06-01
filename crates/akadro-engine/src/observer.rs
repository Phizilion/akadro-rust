// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Out-of-strategy **observation** of a run — for live dashboards, charts and
//! telemetry (e.g. `akadro-tui`).
//!
//! An [`Observer`] is driven by the engine *outside* the deterministic
//! [`Strategy`](crate::Strategy) path: it only ever **reads** the run, so it
//! cannot affect the strategy, the fills, or determinism (a run with any observer
//! produces the identical [`RunReport`] as [`Engine::run`](crate::Engine::run)).
//! The implementor chooses what to surface and how — a TUI panel layout, a metrics
//! exporter, a log — by handling only the hooks it needs (all default to no-ops).

use akadro_core::{Bar, Money};

use crate::engine::RunReport;
use crate::portfolio::FillRecord;

/// A read-only observer of a backtest or live run. Implement only the hooks you
/// use; the rest are no-ops. Driven via
/// [`Engine::run_observed`](crate::Engine::run_observed).
pub trait Observer {
    /// After each bar is fully processed: the bar, the mark-to-market `equity` at
    /// its close, and **all** fills so far this run (track the slice length across
    /// calls to detect new fills). Use this to drive a live price/equity chart.
    fn on_bar(&mut self, _bar: Bar, _equity: Money, _fills: &[FillRecord]) {}

    /// Once, at the end of the run, with the final report (its `annotations`,
    /// `equity_curve`, totals, etc.).
    fn on_finish(&mut self, _report: &RunReport) {}
}

/// The no-op observer used by [`Engine::run`](crate::Engine::run).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpObserver;

impl Observer for NoOpObserver {}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{ClientOrderId, InstrumentId, Price, Qty, Side, Timestamp};

    #[test]
    fn noop_observer_hooks_are_callable() {
        // The default no-ops are callable and inert.
        let mut obs = NoOpObserver;
        let bar = Bar::new(
            InstrumentId::new(0),
            Timestamp::from_nanos(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Price::from_raw(1),
            Qty::from_raw(1),
        );
        let fill = FillRecord {
            id: ClientOrderId::new(0),
            instrument: InstrumentId::new(0),
            side: Side::Buy,
            price: Price::from_raw(1),
            qty: Qty::from_raw(1),
            fee: Money::ZERO,
            ts: Timestamp::from_nanos(1),
        };
        obs.on_bar(bar, Money::from_raw(100), std::slice::from_ref(&fill));
        obs.on_finish(&RunReport::default());
    }
}
