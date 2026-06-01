// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # akadro-venue-dex
//!
//! A *basic* automated-market-maker (Uniswap-style) execution connector,
//! demonstrating that a fundamentally different venue — **swap-only, no order
//! book** — fits akadro's venue traits with zero core changes, and exercising
//! the multi-cost [`Cost`] model: a swap pays an **LP fee** (in the quote asset)
//! *and* **gas** (in a native asset), two costs in two different assets.
//!
//! ## Model (v1, simplified)
//!
//! A market swap of `qty` fills at the bar price adjusted by a linear price
//! impact `impact_bps = qty * impact_coef_bps / depth` (buys pay more, sells
//! receive less) — capturing "bigger swap → worse price" deterministically. The
//! exact constant-product (`x·y=k`) curve and on-chain reserve/precision
//! (`sqrtPriceX96`) modelling are documented future work; this v1 keeps the
//! economics and the cost shape honest without bignum precision.
//!
//! Only [`OrderKind::Market`] swaps are supported (a DEX has no resting book);
//! other kinds are rejected locally.

use akadro_core::{
    AccountEvent, AssetId, Bar, ClientOrderId, Cost, CostKind, Costs, Event, EventSink,
    ExecutionClient, InstrumentCatalog, InstrumentSpec, Money, OrderKind, OrderRequest, Price, Qty,
    RejectReason, Side, Timestamp,
};

/// Configuration for the simplified AMM.
#[derive(Debug, Clone, Copy)]
pub struct DexConfig {
    /// Liquidity depth (raw base units) scaling the price impact: a swap of this
    /// size moves the price by roughly `impact_coef_bps`.
    pub depth: i64,
    /// Price-impact coefficient in basis points (impact at `qty == depth`).
    pub impact_coef_bps: i64,
    /// Liquidity-provider fee in basis points (charged in the quote asset).
    pub lp_fee_bps: i64,
    /// Flat gas cost per swap (raw `Money`), charged in `gas_asset`.
    pub gas: i64,
    /// Asset gas is paid in (e.g. the chain's native token).
    pub gas_asset: AssetId,
}

impl Default for DexConfig {
    fn default() -> Self {
        DexConfig {
            depth: 1,
            impact_coef_bps: 0,
            lp_fee_bps: 30, // 0.30% — the canonical Uniswap v2 fee
            gas: 0,
            gas_asset: AssetId::new(0),
        }
    }
}

#[derive(Debug)]
struct Resting {
    id: ClientOrderId,
    order: OrderRequest,
}

/// A basic AMM/DEX execution client.
#[derive(Debug)]
pub struct DexExchange {
    specs: Vec<InstrumentSpec>,
    config: DexConfig,
    resting: Vec<Resting>,
}

impl DexExchange {
    /// Create a DEX execution client over `specs` with the given AMM `config`.
    #[must_use]
    pub fn new(specs: Vec<InstrumentSpec>, config: DexConfig) -> Self {
        DexExchange {
            specs,
            config,
            resting: Vec::new(),
        }
    }

    /// Orders pending a swap on the next bar.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.resting.len()
    }

    /// Effective fill price after linear price impact.
    fn impacted(&self, mid: Price, side: Side, qty: Qty) -> Price {
        // `<= 0` (not `== 0`): a negative coefficient would *invert* the model,
        // rewarding size with a better price (m9).
        if self.config.depth <= 0 || self.config.impact_coef_bps <= 0 {
            return mid;
        }
        let impact_bps = i128::from(qty.raw()) * i128::from(self.config.impact_coef_bps)
            / i128::from(self.config.depth);
        // Keep the arithmetic in i128 and saturate; a wrapping `as i64` on the
        // adjustment could flip its sign under an extreme config (m10).
        let adj = i128::from(mid.raw()) * impact_bps / 10_000;
        let signed = i128::from(side.sign()).saturating_mul(adj);
        let raw = i128::from(mid.raw()).saturating_add(signed);
        // Never let impact drive the fill price non-positive (m11): a negative
        // price yields negative notional / phantom profits. Floor at one raw tick.
        let clamped = raw.clamp(1, i128::from(i64::MAX));
        Price::from_raw(clamped as i64)
    }
}

impl ExecutionClient for DexExchange {
    fn submit(
        &mut self,
        id: ClientOrderId,
        order: OrderRequest,
        now: Timestamp,
        sink: &mut dyn EventSink,
    ) {
        let valid_instrument = self.specs.spec(order.instrument).is_some();
        let is_swap = matches!(order.kind, OrderKind::Market);
        if !valid_instrument || !is_swap || order.qty.raw() <= 0 {
            sink.emit(AccountEvent::OrderRejected {
                id,
                reason: RejectReason::InvalidOrder,
                ts: now,
            });
            return;
        }
        sink.emit(AccountEvent::OrderAccepted { id, ts: now });
        self.resting.push(Resting { id, order });
    }

    fn observe(&mut self, event: &Event, now: Timestamp, sink: &mut dyn EventSink) {
        let Event::Bar(bar) = event else { return };
        let pending = std::mem::take(&mut self.resting);
        for r in pending {
            if r.order.instrument != bar.instrument {
                self.resting.push(r);
                continue;
            }
            let Bar {
                open, instrument, ..
            } = *bar;
            let price = self.impacted(open, r.order.side, r.order.qty);
            let notional = price.notional(r.order.qty);

            let mut costs = Costs::new();
            // `> 0`, not `!= 0`: a negative LP fee would pay the trader a rebate
            // (an AMM never does), the same sign bug as the impact coefficient (m9).
            if self.config.lp_fee_bps > 0
                && let Some(spec) = self.specs.spec(instrument)
            {
                costs.push(Cost::new(
                    spec.quote,
                    notional.mul_bps(self.config.lp_fee_bps),
                    CostKind::LpFee,
                ));
            }
            if self.config.gas != 0 {
                costs.push(Cost::new(
                    self.config.gas_asset,
                    Money::from_raw(i128::from(self.config.gas)),
                    CostKind::Gas,
                ));
            }

            sink.emit(AccountEvent::Fill {
                client_order_id: r.id,
                instrument,
                side: r.order.side,
                price,
                qty: r.order.qty,
                costs,
                complete: true, // an AMM swap fills the whole order at once
                ts: now,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{CapSet, InstrumentId, InstrumentKind};

    const I: InstrumentId = InstrumentId::new(0);

    fn specs() -> Vec<InstrumentSpec> {
        vec![InstrumentSpec::new(
            I,
            AssetId::new(0), // base
            AssetId::new(1), // quote
            InstrumentKind::Spot,
            Price::from_raw(1),
            Qty::from_raw(1),
            Money::ZERO,
            CapSet::empty(),
        )]
    }
    fn cfg() -> DexConfig {
        DexConfig {
            depth: 1000,
            impact_coef_bps: 100,
            lp_fee_bps: 30,
            gas: 5,
            gas_asset: AssetId::new(9),
        }
    }
    fn bar(open: i64) -> Event {
        Event::Bar(Bar::new(
            I,
            Timestamp::from_nanos(1),
            Price::from_raw(open),
            Price::from_raw(open),
            Price::from_raw(open),
            Price::from_raw(open),
            Qty::from_raw(10_000),
        ))
    }

    fn fill(s: &[AccountEvent]) -> (Price, Qty, Side, Costs) {
        s.iter()
            .find_map(|e| match e {
                AccountEvent::Fill {
                    price,
                    qty,
                    side,
                    costs,
                    ..
                } => Some((*price, *qty, *side, costs.clone())),
                _ => None,
            })
            .expect("a fill")
    }

    #[test]
    fn extreme_sell_impact_floors_price_non_negative() {
        // m11: a sell with impact > 100% must not drive the fill price negative.
        let mut c = cfg();
        c.impact_coef_bps = 10_000;
        c.depth = 1;
        let mut x = DexExchange::new(specs(), c);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Sell, Qty::from_raw(100)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100), Timestamp::from_nanos(2), &mut s);
        let (price, _, side, _) = fill(&s);
        assert_eq!(side, Side::Sell);
        assert!(
            price.raw() >= 1,
            "price floored non-negative, got {}",
            price.raw()
        );
    }

    #[test]
    fn buy_swap_has_impact_lp_fee_and_gas() {
        let mut x = DexExchange::new(specs(), cfg());
        let mut s = Vec::new();
        // buy 500 base; impact = 500 * 100bps / 1000 = 50bps; price 100 -> 100.5.
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(500)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100), Timestamp::from_nanos(2), &mut s);
        let (price, qty, side, costs) = fill(&s);
        assert_eq!(side, Side::Buy);
        assert_eq!(qty, Qty::from_raw(500));
        assert_eq!(price, Price::from_raw(100)); // 100 + 100*50/10000 = 100.5 -> truncates to 100 (raw)
        // costs: LP fee (quote asset 1) + gas (asset 9).
        assert_eq!(costs.len(), 2);
        assert_eq!(costs[0].kind, CostKind::LpFee);
        assert_eq!(costs[0].asset, AssetId::new(1));
        assert_eq!(costs[1].kind, CostKind::Gas);
        assert_eq!(costs[1].asset, AssetId::new(9));
        assert_eq!(costs[1].amount, Money::from_raw(5));
    }

    #[test]
    fn impact_scales_with_size_and_direction() {
        // Larger price so impact is visible after truncation.
        let mut x = DexExchange::new(
            specs(),
            DexConfig {
                depth: 100,
                impact_coef_bps: 100,
                lp_fee_bps: 0,
                gas: 0,
                gas_asset: AssetId::new(9),
            },
        );
        let mut s = Vec::new();
        // sell 100 base @ price 10000; impact = 100*100/100 = 100bps = 1% -> 10000*1% = 100 worse (lower for sell).
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Sell, Qty::from_raw(100)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(10_000), Timestamp::from_nanos(2), &mut s);
        let (price, _, _, _) = fill(&s);
        assert_eq!(price, Price::from_raw(9_900)); // sell receives less
    }

    #[test]
    fn no_impact_when_disabled() {
        let mut x = DexExchange::new(
            specs(),
            DexConfig {
                depth: 1000,
                impact_coef_bps: 0,
                lp_fee_bps: 0,
                gas: 0,
                gas_asset: AssetId::new(0),
            },
        );
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(500)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100), Timestamp::from_nanos(2), &mut s);
        assert_eq!(fill(&s).0, Price::from_raw(100));
    }

    #[test]
    fn non_swap_orders_rejected() {
        let mut x = DexExchange::new(specs(), cfg());
        let mut s = Vec::new();
        let limit = OrderRequest::limit(I, Side::Buy, Qty::from_raw(1), Price::from_raw(100));
        x.submit(
            ClientOrderId::new(0),
            limit,
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(s[0], AccountEvent::OrderRejected { .. }));
        assert_eq!(x.pending_count(), 0);
    }

    #[test]
    fn unknown_instrument_and_zero_qty_rejected() {
        let mut x = DexExchange::new(specs(), cfg());
        let mut s = Vec::new();
        let bad = OrderRequest::market(InstrumentId::new(9), Side::Buy, Qty::from_raw(1));
        x.submit(ClientOrderId::new(0), bad, Timestamp::from_nanos(1), &mut s);
        assert!(matches!(s[0], AccountEvent::OrderRejected { .. }));
        s.clear();
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(0)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(s[0], AccountEvent::OrderRejected { .. }));
    }

    #[test]
    fn default_config_uses_uniswap_fee() {
        assert_eq!(DexConfig::default().lp_fee_bps, 30);
    }
}

#[cfg(test)]
mod cov_tests {
    use super::*;
    use akadro_core::{CapSet, InstrumentId, InstrumentKind};

    fn specs() -> Vec<InstrumentSpec> {
        vec![InstrumentSpec::new(
            InstrumentId::new(0),
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(1),
            Qty::from_raw(1),
            Money::ZERO,
            CapSet::empty(),
        )]
    }

    #[test]
    fn depth_zero_guards_impact_and_zero_fee_skips_cost() {
        let cfg = DexConfig {
            depth: 0,
            impact_coef_bps: 100,
            lp_fee_bps: 0,
            gas: 0,
            gas_asset: AssetId::new(0),
        };
        let mut x = DexExchange::new(specs(), cfg);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(InstrumentId::new(0), Side::Buy, Qty::from_raw(500)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        let b = Event::Bar(Bar::new(
            InstrumentId::new(0),
            Timestamp::from_nanos(2),
            Price::from_raw(100),
            Price::from_raw(100),
            Price::from_raw(100),
            Price::from_raw(100),
            Qty::from_raw(10),
        ));
        x.observe(&b, Timestamp::from_nanos(2), &mut s);
        match &s[0] {
            AccountEvent::Fill { price, costs, .. } => {
                assert_eq!(*price, Price::from_raw(100)); // depth 0 -> no impact
                assert!(costs.is_empty()); // lp_fee 0 + gas 0 -> no costs
            }
            other => panic!("expected fill, got {other:?}"),
        }
    }

    #[test]
    fn resting_order_skipped_for_other_instrument_bar() {
        let mut x = DexExchange::new(specs(), DexConfig::default());
        let mut s = Vec::new();
        // Rest a swap on instrument 0.
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(InstrumentId::new(0), Side::Buy, Qty::from_raw(1)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        // A bar for a *different* instrument must not fill it; it stays resting.
        let other = Event::Bar(Bar::new(
            InstrumentId::new(5),
            Timestamp::from_nanos(2),
            Price::from_raw(100),
            Price::from_raw(100),
            Price::from_raw(100),
            Price::from_raw(100),
            Qty::from_raw(10),
        ));
        x.observe(&other, Timestamp::from_nanos(2), &mut s);
        assert!(s.is_empty()); // no fill
        assert_eq!(x.pending_count(), 1); // still resting
    }
}
