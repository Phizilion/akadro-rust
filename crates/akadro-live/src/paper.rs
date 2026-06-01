// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Paper trading: a live feed driving the engine with simulated execution.

use akadro_backtest::{FillConfig, SimulatedExchange};
use akadro_core::{DataSource, InstrumentSpec, Money, Result};
use akadro_engine::{Engine, RunReport, Strategy};

/// Run **paper trading**: drive `strategy` over a live (or any) `feed` through
/// the real engine, but execute orders against a [`SimulatedExchange`] charging
/// `fee_bps` — live prices, simulated fills. Because it is the same engine and
/// the same strategy code as a backtest, behaviour matches (goal 2).
///
/// `initial_cash` seeds the venue's cash guard as well as the engine, so a paper
/// run cannot spend quote it does not have. For finer realism (slippage, latency,
/// partial fills, funding, …) use [`run_paper_with`].
///
/// # Errors
/// Returns [`AkadroError::Config`](akadro_core::AkadroError::Config) if the
/// engine cannot be built (e.g. bad instrument ids).
pub fn run_paper<S: Strategy, D: DataSource>(
    instruments: &[InstrumentSpec],
    initial_cash: Money,
    feed: D,
    fee_bps: i64,
    strategy: S,
) -> Result<RunReport> {
    let config = FillConfig {
        fee_bps,
        ..FillConfig::default()
    };
    run_paper_with(instruments, initial_cash, feed, config, strategy)
}

/// As [`run_paper`] but with a full [`FillConfig`] for realism knobs (slippage,
/// latency, participation/partial fills, funding, liquidation, the probabilistic
/// maker model). `initial_cash` seeds the engine and — unless the config already
/// sets [`FillConfig::starting_cash`] — the venue's quote-cash guard, so the paper
/// account is bounded by a real balance (i13).
///
/// # Errors
/// As [`run_paper`].
pub fn run_paper_with<S: Strategy, D: DataSource>(
    instruments: &[InstrumentSpec],
    initial_cash: Money,
    feed: D,
    mut config: FillConfig,
    strategy: S,
) -> Result<RunReport> {
    if config.starting_cash.is_none() {
        config.starting_cash = Some(initial_cash.raw());
    }
    let exec = SimulatedExchange::with_config(instruments.to_vec(), config);
    Engine::new(instruments, initial_cash, feed, exec, strategy).map(Engine::run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{
        AssetId, Bar, CapSet, Event, InstrumentId, InstrumentKind, OrderRequest, Price, Qty, Side,
        Timestamp,
    };
    use akadro_engine::Ctx;

    struct BuyFirst {
        done: bool,
    }
    impl Strategy for BuyFirst {
        fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
            if !self.done {
                ctx.submit(OrderRequest::market(
                    InstrumentId::new(0),
                    Side::Buy,
                    Qty::from_raw(1),
                ));
                self.done = true;
            }
        }
    }

    struct VecFeed(std::vec::IntoIter<Event>);
    impl DataSource for VecFeed {
        fn next_event(&mut self) -> Option<Event> {
            self.0.next()
        }
    }

    fn bar(ts: i64, close: i64) -> Event {
        let p = Price::from_raw(close);
        Event::Bar(Bar::new(
            InstrumentId::new(0),
            Timestamp::from_nanos(ts),
            p,
            p,
            p,
            p,
            Qty::from_raw(1),
        ))
    }

    #[test]
    fn paper_trades_with_simulated_fills() {
        let spec = InstrumentSpec::new(
            InstrumentId::new(0),
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(1),
            Qty::from_raw(1),
            Money::ZERO,
            CapSet::empty(),
        );
        let feed = VecFeed(vec![bar(1, 100), bar(2, 110)].into_iter());
        let report = run_paper(
            &[spec],
            Money::from_raw(1_000_000),
            feed,
            0,
            BuyFirst { done: false },
        )
        .unwrap();
        assert_eq!(report.bars_processed, 2);
        // Order submitted on bar 1 fills (simulated) at bar 2's open = 110.
        assert_eq!(report.fills.len(), 1);
        assert_eq!(report.fills[0].price, Price::from_raw(110));
    }

    #[test]
    fn paper_cash_guard_rejects_unaffordable_order() {
        // i13: initial_cash now seeds the venue cash guard, so a paper account
        // cannot spend quote it does not have. 40 < the ~100 needed to buy 1 @ mark
        // 100 -> the order is rejected and never fills.
        let spec = InstrumentSpec::new(
            InstrumentId::new(0),
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(1),
            Qty::from_raw(1),
            Money::ZERO,
            CapSet::empty(),
        );
        let feed = VecFeed(vec![bar(1, 100), bar(2, 110)].into_iter());
        let report = run_paper(
            &[spec],
            Money::from_raw(40),
            feed,
            0,
            BuyFirst { done: false },
        )
        .unwrap();
        assert!(
            report.fills.is_empty(),
            "an unaffordable paper order must be rejected by the cash guard (i13)"
        );
    }
}
