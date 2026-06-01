// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A conservative, deterministic simulated exchange.
//!
//! The default fill model is intentionally conservative so a backtest does not
//! flatter a strategy (decision D6): market orders fill at the *next* bar's open,
//! limit orders fill only when a later bar trades through the limit, every fill
//! pays a flat taker fee, and there is no slippage/latency/partial-fill unless
//! you opt in via [`FillConfig`].
//!
//! Supported order kinds (growth pt 4): market, limit, stop, stop-limit,
//! trailing-stop, market-if-touched, post-only, plus OCO grouping. Optional
//! realism knobs (growth pt 5): slippage, latency, partial fills (participation
//! cap), perpetual funding, and a simplified liquidation. Conservative defaults
//! keep results bit-identical for the simple case (so parity still holds).

use std::collections::HashMap;

use akadro_core::{
    AccountEvent, Bar, CancelReason, CancelRejectReason, ClientOrderId, Cost, CostKind, Costs,
    Event, EventSink, ExecutionClient, InstrumentId, InstrumentKind, InstrumentSpec, Money,
    OrderKind, OrderRequest, Price, Qty, RejectReason, Side, TimeInForce, Timestamp, TrailKind,
};
use akadro_engine::DeterministicRng;
use rand_core::RngCore;

/// Tunable fill-model parameters. [`FillConfig::default`] is fully conservative
/// (no fee, slippage, latency, participation cap, funding or liquidation) — all
/// fields are their natural zero/`None` defaults.
#[derive(Debug, Clone, Copy, Default)]
pub struct FillConfig {
    /// Flat taker fee in basis points, charged on every fill's notional.
    pub fee_bps: i64,
    /// Adverse slippage in basis points applied to each fill price (a flat,
    /// size-independent model). For a *size-sensitive* component, see
    /// [`FillConfig::impact_bps`]. The flat model is the conservative default.
    pub slippage_bps: i64,
    /// Volume-share **market-impact** coefficient (bps). When non-zero, a
    /// liquidity-taking fill is pushed adversely by `impact_bps · participation`
    /// *on top of* `slippage_bps`, where `participation = min(1, fill_qty /
    /// bar_volume)` — a fill that takes a large fraction of a bar's volume moves
    /// the price against itself, while a tiny fill barely moves it. `0` (default)
    /// disables it, keeping the conservative path bit-identical (parity preserved).
    /// Set via [`SimulatedExchange::with_impact_model`]. Most credible on fine
    /// (sub-second) bars where `bar_volume` is a meaningful denominator.
    pub impact_bps: i64,
    /// Bars an order waits (latency) before it becomes eligible to fill.
    pub latency_bars: u32,
    /// Max fraction of a bar's volume (in bps) a single order may take per bar;
    /// `None` means unlimited (fill fully).
    pub max_participation_bps: Option<i64>,
    /// Funding rate in bps charged on perpetual position notional every
    /// `funding_interval_bars`; `0` disables funding.
    pub funding_bps: i64,
    /// Number of bars between funding settlements (`0` disables funding).
    pub funding_interval_bars: u32,
    /// Adverse move (bps from average entry) at which a leveraged position is
    /// force-liquidated; `None` disables liquidation.
    pub liquidation_bps: Option<i64>,
    /// Seed for any probabilistic model (recorded for reproducibility).
    pub seed: u64,
    /// Opt-in **probabilistic maker fill**: the probability (in basis points,
    /// `0..=10_000`) that a resting limit order which the bar only *touches* at its
    /// limit price — an ambiguous queue-position fill — actually fills this bar. A
    /// decisive trade-*through* always fills. `None` (default) is deterministic
    /// (a touch always fills), keeping the conservative path bit-identical.
    ///
    /// When set, the per-touch Bernoulli draw comes from an engine-owned seeded
    /// `ChaCha8` ([`Self::seed`]) consumed in deterministic feed order, so the run
    /// stays exactly reproducible and backtest↔live parity is preserved (the model
    /// is backtest-only by construction — a live venue reports its own fills, D6).
    pub maker_fill_prob_bps: Option<i64>,
    /// Opt-in quote-cash balance enforcement. When `Some(starting_cash)`, the
    /// venue mirrors a cash balance (seeded here) across the fills it emits and
    /// **rejects buy orders it cannot afford** ([`RejectReason::InsufficientFunds`]),
    /// so a backtest can never spend quote it does not have. `None` (default)
    /// disables the check, keeping the conservative path bit-identical. Set this
    /// equal to the engine's `initial_cash`. Base-asset / short / margin limits
    /// remain out of scope (see the `AGENTS.md` threat model).
    pub starting_cash: Option<i128>,
}

#[derive(Debug)]
struct Resting {
    id: ClientOrderId,
    order: OrderRequest,
    activate_in: u32,
    armed: bool,
    trail_ref: Option<i64>,
    filled: i64,
}

#[derive(Debug, Default, Clone, Copy)]
struct Shadow {
    net: i64,
    avg: i64,
    bars: u32,
    /// Last observed close — the mark used to estimate buying power at submit.
    mark: i64,
}

impl Shadow {
    fn apply(&mut self, side: Side, price: i64, qty: i64) {
        // Widen BEFORE `.abs()` and use saturating add/mul so an extreme qty can
        // never panic at `i64::MIN` or wrap — matching the engine `Portfolio`'s
        // safety (m4).
        let signed = side.sign().saturating_mul(qty);
        let pos = self.net;
        if pos == 0 || (pos > 0) == (signed > 0) {
            let pos_abs = i128::from(pos).abs();
            let add_abs = i128::from(signed).abs();
            let total = pos_abs * i128::from(self.avg) + add_abs * i128::from(price);
            let total_qty = pos_abs + add_abs;
            if total_qty != 0 {
                // Round half away from zero — symmetric (so the bias does not flip
                // for negative prices), matching the engine portfolio's average.
                let half = total_qty / 2;
                let rounded = if total >= 0 {
                    (total + half) / total_qty
                } else {
                    (total - half) / total_qty
                };
                self.avg = rounded as i64;
            }
            self.net = pos.saturating_add(signed);
        } else {
            let new_net = pos.saturating_add(signed);
            self.net = new_net;
            if new_net == 0 {
                self.avg = 0;
            } else if (new_net > 0) != (pos > 0) {
                self.avg = price;
            }
        }
    }
}

/// What a resting order should do when evaluated against a bar.
enum Outcome {
    /// Fill at this price (subject to slippage/participation by the caller).
    Fill(Price),
    /// Cancel with this reason.
    Cancel(CancelReason),
    /// Stay resting.
    Rest,
}

/// The funding rate (signed bps) effective at or before `now` in a
/// time-ascending schedule; `0` before the first point (and for an empty
/// schedule). Binary search — `O(log n)`.
fn funding_rate_at(schedule: &[(Timestamp, i64)], now: Timestamp) -> i64 {
    let idx = schedule.partition_point(|(t, _)| t.as_nanos() <= now.as_nanos());
    if idx == 0 { 0 } else { schedule[idx - 1].1 }
}

/// A deterministic simulated venue implementing [`ExecutionClient`].
#[derive(Debug)]
pub struct SimulatedExchange {
    specs: Vec<InstrumentSpec>,
    config: FillConfig,
    resting: Vec<Resting>,
    shadow: HashMap<u32, Shadow>,
    /// Mirrored quote-cash balance, `Some` iff cash enforcement is enabled.
    cash: Option<i128>,
    /// Engine-owned seeded RNG, `Some` iff a probabilistic model is enabled
    /// (currently the maker-fill-at-touch model). Drawn in deterministic feed
    /// order, so a run is reproducible and parity holds. Never `thread_rng` (D6).
    rng: Option<DeterministicRng>,
    /// Whether the RNG has actually been drawn — gates [`Self::config_seed`] so a
    /// run that never invoked a probabilistic model reports `None` (not `Some(0)`),
    /// distinguishing "no randomness" from "seed 0" (i4).
    rng_drawn: bool,
    /// Optional time-varying funding-rate schedule per instrument (index →
    /// `(effective_from, bps)` sorted by time). When present for an instrument the
    /// rate effective at the settlement time is used instead of the constant
    /// [`FillConfig::funding_bps`]; absent instruments use the constant, so the
    /// default path stays bit-identical.
    funding_schedule: HashMap<u32, Vec<(Timestamp, i64)>>,
    /// Per-instrument funding settlement interval (bars). Set by
    /// [`with_funding_schedule`](Self::with_funding_schedule) so a per-instrument
    /// schedule does not overwrite the global cadence for every other instrument
    /// (m12). Absent instruments use [`FillConfig::funding_interval_bars`].
    funding_interval: HashMap<u32, u32>,
}

impl SimulatedExchange {
    /// Create a simulated exchange charging a flat `fee_bps` taker fee.
    #[must_use]
    pub fn new(specs: Vec<InstrumentSpec>, fee_bps: i64) -> Self {
        SimulatedExchange {
            specs,
            config: FillConfig {
                fee_bps,
                ..FillConfig::default()
            },
            resting: Vec::new(),
            shadow: HashMap::new(),
            cash: None,
            rng: None,
            rng_drawn: false,
            funding_schedule: HashMap::new(),
            funding_interval: HashMap::new(),
        }
    }

    /// Create with a full [`FillConfig`].
    #[must_use]
    pub fn with_config(specs: Vec<InstrumentSpec>, config: FillConfig) -> Self {
        SimulatedExchange {
            specs,
            cash: config.starting_cash,
            // A probabilistic model in the config wires up the seeded RNG.
            rng: config
                .maker_fill_prob_bps
                .map(|_| DeterministicRng::seeded(config.seed)),
            rng_drawn: false,
            config,
            resting: Vec::new(),
            shadow: HashMap::new(),
            funding_schedule: HashMap::new(),
            funding_interval: HashMap::new(),
        }
    }

    /// Set the deterministic seed (re-seeds the RNG if a probabilistic model is on).
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.config.seed = seed;
        if self.rng.is_some() {
            self.rng = Some(DeterministicRng::seeded(seed));
        }
        self
    }

    /// Enable the **probabilistic maker-fill-at-touch** model: a resting limit order
    /// that the bar only *touches* at its limit (a queue-position-ambiguous fill)
    /// fills only with probability `bps / 10_000`; a decisive trade-*through* always
    /// fills. The per-touch draw comes from the seeded RNG ([`with_seed`](Self::with_seed))
    /// in deterministic feed order, so the run is reproducible and parity holds. `0`
    /// disables it (the default), keeping the conservative path bit-identical.
    #[must_use]
    pub fn with_maker_fill_probability(mut self, bps: i64) -> Self {
        self.config.maker_fill_prob_bps = Some(bps);
        self.rng = Some(DeterministicRng::seeded(self.config.seed));
        self
    }

    /// Set adverse slippage (bps).
    #[must_use]
    pub fn with_slippage_bps(mut self, bps: i64) -> Self {
        self.config.slippage_bps = bps;
        self
    }

    /// Enable the **volume-share market-impact** model: liquidity-taking fills are
    /// pushed adversely by `impact_bps · participation` (participation = the
    /// fraction of the bar's volume the fill takes, capped at 100%) *on top of*
    /// flat slippage — so size has a price. `0` (default) keeps the conservative
    /// path bit-identical (parity preserved). Most meaningful on fine (sub-second)
    /// bars; see [`FillConfig::impact_bps`].
    #[must_use]
    pub fn with_impact_model(mut self, impact_bps: i64) -> Self {
        self.config.impact_bps = impact_bps;
        self
    }

    /// Set order latency (bars before an order can fill).
    #[must_use]
    pub fn with_latency_bars(mut self, bars: u32) -> Self {
        self.config.latency_bars = bars;
        self
    }

    /// Cap per-bar fills at `bps` of bar volume (enables partial fills).
    #[must_use]
    pub fn with_participation_bps(mut self, bps: i64) -> Self {
        self.config.max_participation_bps = Some(bps);
        self
    }

    /// Enable perpetual funding: `bps` of position notional every `interval`
    /// bars. The rate is **signed**: a positive `bps` makes longs pay (and shorts
    /// receive); a negative `bps` is backwardation, where longs receive (and
    /// shorts pay).
    #[must_use]
    pub fn with_funding(mut self, bps: i64, interval_bars: u32) -> Self {
        self.config.funding_bps = bps;
        self.config.funding_interval_bars = interval_bars;
        self
    }

    /// Replay a **time-varying** funding-rate schedule for `instrument` instead of
    /// the constant [`with_funding`](Self::with_funding) rate. `schedule` is a list
    /// of `(effective_from, signed_bps)` points; at each settlement the rate whose
    /// `effective_from` is the latest at-or-before the event time is charged and
    /// reported on the [`AccountEvent::FundingSettlement`]. Settlements still occur
    /// every `interval_bars` (use the same cadence as the venue, e.g. every 8h of
    /// bars). The schedule is sorted on insertion; before its first point the rate
    /// is `0`. Instruments without a schedule fall back to the constant rate, so
    /// the conservative default path remains bit-identical (parity preserved).
    ///
    /// Source real funding via a venue feed (e.g. a Mexc/Binance funding history)
    /// cached through the data layer, so the backtest sees the funding regime the
    /// strategy actually targets rather than a flat mean.
    #[must_use]
    pub fn with_funding_schedule(
        mut self,
        instrument: InstrumentId,
        interval_bars: u32,
        mut schedule: Vec<(Timestamp, i64)>,
    ) -> Self {
        // Per-instrument cadence (m12): do NOT clobber the global
        // `config.funding_interval_bars`, which would change the interval for every
        // other instrument.
        self.funding_interval
            .insert(instrument.index(), interval_bars);
        schedule.sort_by_key(|(t, _)| t.as_nanos());
        self.funding_schedule.insert(instrument.index(), schedule);
        self
    }

    /// Enable liquidation when a position moves `bps` against its average entry.
    #[must_use]
    pub fn with_liquidation_bps(mut self, bps: i64) -> Self {
        self.config.liquidation_bps = Some(bps);
        self
    }

    /// Enable quote-cash enforcement seeded at `cash` (set equal to the engine's
    /// `initial_cash`). Buys that cannot be afforded at the current mark are
    /// rejected with [`RejectReason::InsufficientFunds`].
    #[must_use]
    pub fn with_starting_cash(mut self, cash: Money) -> Self {
        self.config.starting_cash = Some(cash.raw());
        self.cash = Some(cash.raw());
        self
    }

    /// The configured deterministic seed.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.config.seed
    }

    /// The venue's mirrored quote-cash balance, or `None` if cash enforcement is
    /// disabled.
    #[must_use]
    pub fn cash(&self) -> Option<Money> {
        self.cash.map(Money::from_raw)
    }

    /// Mirror the cash effect of an emitted fill onto the tracked balance (a no-op
    /// unless cash enforcement is enabled). Buys pay notional + costs; sells
    /// receive notional minus costs; funding/liquidation flow entirely via `costs`
    /// (zero notional) or as a normal close, respectively.
    fn settle_cash(&mut self, side: Side, price: Price, qty: Qty, costs: &Costs) {
        let Some(cash) = self.cash.as_mut() else {
            return;
        };
        let notional = price.notional(qty).raw();
        // Saturating, consistent with the engine `Portfolio` (m3): an extreme
        // notional must never debug-panic / release-wrap the mirrored balance.
        *cash = match side {
            Side::Buy => cash.saturating_sub(notional),
            Side::Sell => cash.saturating_add(notional),
        };
        for c in costs {
            *cash = cash.saturating_sub(c.amount.raw());
        }
    }

    /// Number of orders currently resting.
    #[must_use]
    pub fn resting_count(&self) -> usize {
        self.resting.len()
    }

    fn spec(&self, instrument: InstrumentId) -> Option<&InstrumentSpec> {
        self.specs.get(instrument.index() as usize)
    }

    fn rejection(&self, order: &OrderRequest) -> Option<RejectReason> {
        let Some(spec) = self.spec(order.instrument) else {
            return Some(RejectReason::InvalidOrder);
        };
        if order.qty.raw() <= 0 {
            return Some(RejectReason::InvalidOrder);
        }
        // post-only is maker-only and only meaningful for a resting limit kind; on
        // a market / plain stop / MIT / trailing order it is a contradiction (those
        // always take liquidity), so reject it rather than silently fill (m1).
        if order.post_only
            && !matches!(
                order.kind,
                OrderKind::Limit { .. } | OrderKind::StopLimit { .. }
            )
        {
            return Some(RejectReason::InvalidOrder);
        }
        // Minimum-notional applies to any kind that carries a resting limit price —
        // plain Limit AND StopLimit (the latter was previously skipped, m12).
        let limit_px = match order.kind {
            OrderKind::Limit { limit } | OrderKind::StopLimit { limit, .. } => Some(limit),
            _ => None,
        };
        if let Some(limit) = limit_px
            && !spec.meets_min_notional(limit, order.qty)
        {
            return Some(RejectReason::InvalidOrder);
        }
        // Reduce-only must not open or increase a position (a futures risk
        // primitive). Reject at accept time, as real venues do, when the mirrored
        // net is flat or on the same side as the order.
        if order.reduce_only {
            let net = self
                .shadow
                .get(&order.instrument.index())
                .map_or(0, |s| s.net);
            if net == 0 || net.signum() == order.side.sign() {
                return Some(RejectReason::WouldIncreasePosition);
            }
        }
        // Buying-power guard (opt-in): a buy must be affordable at the current
        // mark, including slippage + fee, AFTER reserving the cost of buys that
        // are already resting — so two buys in one bar can't each pass yet jointly
        // overspend. Sells generate cash and aren't checked; base-asset / short /
        // margin limits remain out of scope. (The mark is the last close; a fill
        // at a gapped-up next open can still exceed it — an accepted residual.)
        if let Some(cash) = self.cash
            && let Some(required) = self.buy_cost(order)
        {
            // Reserve only each resting buy's UNFILLED remainder — its already-
            // filled portion was debited from mirrored cash at fill time, so
            // reserving the full original qty would double-count and wrongly reject
            // an affordable buy.
            let reserved: i128 = self
                .resting
                .iter()
                .filter_map(|r| {
                    self.buy_cost_qty(&r.order, Qty::from_raw(r.order.qty.raw() - r.filled))
                })
                .sum();
            if cash - reserved < required {
                return Some(RejectReason::InsufficientFunds);
            }
        }
        None
    }

    /// Estimated cash a buy order would consume at the current mark (slipped
    /// notional + fee), or `None` for a sell / an order with no priceable mark.
    fn buy_cost(&self, order: &OrderRequest) -> Option<i128> {
        self.buy_cost_qty(order, order.qty)
    }

    /// As [`Self::buy_cost`] but for an explicit `qty` (the outstanding remainder
    /// of a partially-filled resting buy).
    fn buy_cost_qty(&self, order: &OrderRequest, qty: Qty) -> Option<i128> {
        if order.side != Side::Buy {
            return None;
        }
        // A passive limit (Limit / StopLimit) fills at its limit with NO slippage
        // and NO impact, so reserving a slipped price would over-reserve and
        // spuriously reject an affordable order (M14). A liquidity-taking kind pays
        // flat slippage AND, to bound the guarantee, the WORST-CASE (100%
        // participation) volume-share impact, so the reservation never under-counts
        // what the fill actually debits (M13).
        let (px, taker) = match order.kind {
            OrderKind::Limit { limit } | OrderKind::StopLimit { limit, .. } => (Some(limit), false),
            _ => {
                let m = self
                    .shadow
                    .get(&order.instrument.index())
                    .map_or(0, |s| s.mark);
                ((m > 0).then(|| Price::from_raw(m)), true)
            }
        };
        px.map(|p| {
            let exec = if taker {
                let after_slip = self.slipped(p, Side::Buy);
                // Worst case: participation = 100%, so impact = price·impact_bps/1e4.
                let impact = (i128::from(after_slip.raw())
                    * i128::from(self.config.impact_bps.max(0))
                    / 10_000) as i64;
                Price::from_raw(after_slip.raw().saturating_add(impact)) // buy: adverse = higher
            } else {
                p
            };
            let cost = exec.notional(qty);
            cost.raw() + cost.mul_bps(self.config.fee_bps).raw()
        })
    }

    /// Apply adverse slippage to a fill price (buys pay more, sells receive less).
    fn slipped(&self, base: Price, side: Side) -> Price {
        if self.config.slippage_bps == 0 {
            return base;
        }
        let adj = (i128::from(base.raw()) * i128::from(self.config.slippage_bps) / 10_000) as i64;
        Price::from_raw(base.raw() + side.sign() * adj)
    }

    /// Fill price for a liquidity-taking order: flat [`slipped`](Self::slipped)
    /// slippage plus the volume-share impact (`impact_bps · participation`) when
    /// configured. `fill_qty` is this fill's size, `bar_volume` the bar's volume.
    /// With `impact_bps == 0` (default) it is exactly `slipped`, so the
    /// conservative path is bit-identical.
    fn fill_price(&self, base: Price, side: Side, fill_qty: i64, bar_volume: i64) -> Price {
        let after_slip = self.slipped(base, side);
        if self.config.impact_bps == 0 || bar_volume <= 0 || fill_qty <= 0 {
            return after_slip;
        }
        // participation in bps, capped at 100% (10_000 bps).
        let participation_bps =
            ((i128::from(fill_qty) * 10_000) / i128::from(bar_volume)).min(10_000);
        // impact = price · impact_bps/1e4 · participation/1e4, adverse to `side`.
        let impact = (i128::from(after_slip.raw()) * i128::from(self.config.impact_bps) / 10_000
            * participation_bps
            / 10_000) as i64;
        Price::from_raw(after_slip.raw() + side.sign() * impact)
    }

    /// `true` if the probabilistic maker model is enabled AND this passive-limit
    /// fill is an ambiguous *touch* — the bar reached the limit exactly but did not
    /// trade through it, so whether it actually filled depends on queue position.
    fn maker_touch_is_uncertain(&self, order: &OrderRequest, bar: &Bar) -> bool {
        if self.config.maker_fill_prob_bps.is_none() {
            return false;
        }
        let (OrderKind::Limit { limit } | OrderKind::StopLimit { limit, .. }) = order.kind else {
            return false;
        };
        match order.side {
            Side::Buy => bar.low.raw() == limit.raw(), // touched exactly, not through
            Side::Sell => bar.high.raw() == limit.raw(),
        }
    }

    /// Draw the maker-fill Bernoulli: returns `true` (fills) with probability
    /// `maker_fill_prob_bps / 10_000`, consuming one RNG word in deterministic feed
    /// order. Marks the RNG as drawn so [`config_seed`](Self::config_seed) reports
    /// the seed (i4). With no model configured it always returns `true`.
    fn draw_maker_fills(&mut self) -> bool {
        let Some(p) = self.config.maker_fill_prob_bps else {
            return true;
        };
        let draw = {
            let Some(rng) = self.rng.as_mut() else {
                return true;
            };
            (rng.next_u64() % 10_000) as i64
        };
        self.rng_drawn = true;
        draw < p
    }

    fn fee_cost(&self, instrument: InstrumentId, price: Price, qty: Qty) -> Costs {
        let mut costs = Costs::new();
        if self.config.fee_bps != 0
            && let Some(spec) = self.spec(instrument)
        {
            costs.push(Cost::new(
                spec.quote,
                price.notional(qty).mul_bps(self.config.fee_bps),
                CostKind::Taker,
            ));
        }
        costs
    }

    /// Evaluate one resting order against `bar`, updating its arming/trail state.
    fn evaluate(r: &mut Resting, bar: &Bar) -> Outcome {
        let side = r.order.side;
        match r.order.kind {
            OrderKind::Market => Outcome::Fill(bar.open),
            OrderKind::Limit { limit } => {
                if r.order.post_only && marketable(side, limit, bar.open) {
                    return Outcome::Cancel(CancelReason::VenueCanceled);
                }
                limit_fill(side, limit, bar, bar.open.raw()).map_or(Outcome::Rest, Outcome::Fill)
            }
            OrderKind::Stop { trigger, .. } => {
                if stop_triggered(side, trigger, bar) {
                    Outcome::Fill(stop_fill(side, trigger.raw(), bar.open.raw()))
                } else {
                    Outcome::Rest
                }
            }
            OrderKind::StopLimit { trigger, limit, .. } => {
                let just_armed = !r.armed && stop_triggered(side, trigger, bar);
                if just_armed {
                    r.armed = true;
                }
                if r.armed {
                    // A leg that arms THIS bar only became active mid-bar, when the
                    // price reached `trigger` — so `bar.open` (which predates
                    // activation) must not be used as the reference for either the
                    // marketability test (M9) or price improvement (M8). Use the
                    // trigger as the activation reference; a leg armed on an earlier
                    // bar legitimately rests from this bar's open.
                    let ref_open = if just_armed {
                        trigger.raw()
                    } else {
                        bar.open.raw()
                    };
                    // A post-only leg must never take liquidity: once active, cancel
                    // (rather than fill) if its limit is immediately marketable at
                    // the activation reference.
                    if r.order.post_only && marketable(side, limit, Price::from_raw(ref_open)) {
                        return Outcome::Cancel(CancelReason::VenueCanceled);
                    }
                    limit_fill(side, limit, bar, ref_open).map_or(Outcome::Rest, Outcome::Fill)
                } else {
                    Outcome::Rest
                }
            }
            OrderKind::MarketIfTouched { trigger, .. } => {
                // Fires on a favourable touch (mirror of a stop).
                let touched = match side {
                    Side::Buy => bar.low.raw() <= trigger.raw(),
                    Side::Sell => bar.high.raw() >= trigger.raw(),
                };
                if touched {
                    Outcome::Fill(trigger)
                } else {
                    Outcome::Rest
                }
            }
            OrderKind::TrailingStop { trail, .. } => {
                // A sell trailing-stop trails the high (protects a long); a buy
                // trailing-stop trails the low (protects a short). The offset is an
                // absolute price or a percentage of the (intra-bar) reference.
                let offset = |r0: i64| match trail {
                    TrailKind::Absolute(p) => p.raw(),
                    // i128 widen, then clamp into i64 (a bps far above 10_000 with a
                    // large price would otherwise overflow the `as i64` cast, m8).
                    TrailKind::Percent(bps) => {
                        i64::try_from(i128::from(r0) * i128::from(bps) / 10_000).unwrap_or(i64::MAX)
                    }
                    _ => 0,
                };
                match side {
                    Side::Sell => {
                        let r0 = r.trail_ref.get_or_insert(bar.high.raw());
                        *r0 = (*r0).max(bar.high.raw());
                        let trigger = (*r0).saturating_sub(offset(*r0));
                        if bar.low.raw() <= trigger {
                            // The trail ref is set from this bar's high (intra-bar),
                            // so the trigger isn't known at the open — fill at it.
                            Outcome::Fill(Price::from_raw(trigger))
                        } else {
                            Outcome::Rest
                        }
                    }
                    Side::Buy => {
                        let r0 = r.trail_ref.get_or_insert(bar.low.raw());
                        *r0 = (*r0).min(bar.low.raw());
                        let trigger = (*r0).saturating_add(offset(*r0));
                        if bar.high.raw() >= trigger {
                            Outcome::Fill(Price::from_raw(trigger))
                        } else {
                            Outcome::Rest
                        }
                    }
                }
            }
            _ => Outcome::Rest,
        }
    }

    /// Cancel all other resting orders sharing `group`, emitting cancels.
    fn cancel_oco_siblings(
        &mut self,
        group: u32,
        keep: ClientOrderId,
        now: Timestamp,
        sink: &mut dyn EventSink,
    ) {
        let mut kept = Vec::with_capacity(self.resting.len());
        for r in std::mem::take(&mut self.resting) {
            if r.order.oco_group == Some(group) && r.id != keep {
                sink.emit(AccountEvent::OrderCanceled {
                    id: r.id,
                    reason: CancelReason::OcoTriggered,
                    ts: now,
                });
            } else {
                kept.push(r);
            }
        }
        self.resting = kept;
    }

    fn accrue_funding(&mut self, bar: &Bar, now: Timestamp, sink: &mut dyn EventSink) {
        // Per-instrument interval if a schedule set one, else the global cadence
        // (m12). `0` disables funding for this instrument.
        let interval = self
            .funding_interval
            .get(&bar.instrument.index())
            .copied()
            .unwrap_or(self.config.funding_interval_bars);
        if interval == 0 {
            return;
        }
        let is_perp = matches!(
            self.spec(bar.instrument).map(|s| s.kind),
            Some(InstrumentKind::PerpetualFuture)
        );
        if !is_perp {
            return;
        }
        let quote = self.spec(bar.instrument).map(|s| s.quote);
        let shadow = self.shadow.entry(bar.instrument.index()).or_default();
        shadow.bars += 1;
        if shadow.bars < interval {
            return;
        }
        shadow.bars = 0;
        let net = shadow.net; // copy out → release the &mut self.shadow borrow
        if net == 0 {
            return;
        }
        // Schedule rate (effective at `now`) if one is set for this instrument,
        // else the constant `funding_bps` (default path → bit-identical).
        let rate = match self.funding_schedule.get(&bar.instrument.index()) {
            Some(sched) => funding_rate_at(sched, now),
            None => self.config.funding_bps,
        };
        // Longs pay funding (positive cost), shorts receive (negative). Use the
        // canonical fixed-point path — `Price::notional` widens to i128 before
        // multiplying and `Money::mul_bps` saturates — rather than a bare i128
        // multiply that could overflow at an extreme rate (m5).
        let amount = bar.close.notional(Qty::from_raw(net)).mul_bps(rate);
        if let Some(quote) = quote {
            let cost = Cost::new(quote, amount, CostKind::Funding);
            let mut costs = Costs::new();
            costs.push(cost);
            self.settle_cash(Side::Buy, Price::ZERO, Qty::ZERO, &costs);
            sink.emit(AccountEvent::FundingSettlement {
                instrument: bar.instrument,
                cost,
                rate,
                ts: now,
            });
        }
    }

    fn check_liquidation(&mut self, bar: &Bar, now: Timestamp, sink: &mut dyn EventSink) {
        let Some(bps) = self.config.liquidation_bps else {
            return;
        };
        let shadow = *self.shadow.entry(bar.instrument.index()).or_default();
        if shadow.net == 0 || shadow.avg == 0 {
            return;
        }
        let mark = bar.close.raw();
        // Adverse move in bps from the average entry.
        let adverse = if shadow.net > 0 {
            shadow.avg - mark // long loses when mark < avg
        } else {
            mark - shadow.avg // short loses when mark > avg
        };
        let loss_bps = i128::from(adverse.max(0)) * 10_000 / i128::from(shadow.avg.abs().max(1));
        if loss_bps < i128::from(bps) {
            return;
        }
        // Force-close at mark (with slippage), opposite side.
        let close_side = if shadow.net > 0 {
            Side::Sell
        } else {
            Side::Buy
        };
        let qty = Qty::from_raw(shadow.net.abs());
        let price = self.slipped(bar.close, close_side);
        let costs = self.fee_cost(bar.instrument, price, qty);
        self.settle_cash(close_side, price, qty, &costs);
        sink.emit(AccountEvent::Liquidation {
            instrument: bar.instrument,
            side: close_side,
            price,
            qty,
            costs,
            ts: now,
        });
        let s = self.shadow.entry(bar.instrument.index()).or_default();
        s.apply(close_side, price.raw(), qty.raw());
        // A real venue cancels all working orders on the instrument when the
        // position is force-liquidated; otherwise a resting limit/stop could
        // silently re-open a position the strategy never asked for next bar (M15).
        let inst = bar.instrument;
        let mut kept = Vec::with_capacity(self.resting.len());
        for r in std::mem::take(&mut self.resting) {
            if r.order.instrument == inst {
                sink.emit(AccountEvent::OrderCanceled {
                    id: r.id,
                    reason: CancelReason::VenueCanceled,
                    ts: now,
                });
            } else {
                kept.push(r);
            }
        }
        self.resting = kept;
    }
}

/// `true` if a limit order at `limit` would immediately take liquidity at `open`.
fn marketable(side: Side, limit: Price, open: Price) -> bool {
    match side {
        Side::Buy => limit.raw() >= open.raw(),
        Side::Sell => limit.raw() <= open.raw(),
    }
}

/// Fill price for a limit order against a bar, if the bar trades through it.
///
/// `ref_open` is the price the order was *active from* this bar — normally
/// `bar.open`, but the `trigger` for a stop-limit that armed mid-bar (so price
/// improvement is measured from activation, not the pre-activation open — M8).
fn limit_fill(side: Side, limit: Price, bar: &Bar, ref_open: i64) -> Option<Price> {
    match side {
        // Price improvement: if the market is already past the limit at the
        // activation reference, the fill is at that better price, not the limit.
        // Otherwise it fills at the limit when the bar trades through.
        Side::Buy if bar.low.raw() <= limit.raw() => {
            Some(Price::from_raw(ref_open.min(limit.raw())))
        }
        Side::Sell if bar.high.raw() >= limit.raw() => {
            Some(Price::from_raw(ref_open.max(limit.raw())))
        }
        _ => None,
    }
}

/// Fill price for a fixed market-on-trigger stop. The trigger is known before the
/// bar, so a gap straight through it fills at the (worse) open, not the trigger —
/// a stop can never fill *better* than the open. (Trailing stops derive their
/// trigger from the same bar's high/low intra-bar, so this does not apply there.)
fn stop_fill(side: Side, trigger: i64, open: i64) -> Price {
    Price::from_raw(match side {
        Side::Buy => trigger.max(open),  // a buy stop's worse price is higher
        Side::Sell => trigger.min(open), // a sell stop's worse price is lower
    })
}

/// `true` if a stop's adverse trigger is touched by the bar.
fn stop_triggered(side: Side, trigger: Price, bar: &Bar) -> bool {
    match side {
        Side::Buy => bar.high.raw() >= trigger.raw(),
        Side::Sell => bar.low.raw() <= trigger.raw(),
    }
}

impl ExecutionClient for SimulatedExchange {
    fn submit(
        &mut self,
        id: ClientOrderId,
        order: OrderRequest,
        now: Timestamp,
        sink: &mut dyn EventSink,
    ) {
        if let Some(reason) = self.rejection(&order) {
            sink.emit(AccountEvent::OrderRejected {
                id,
                reason,
                ts: now,
            });
            return;
        }
        sink.emit(AccountEvent::OrderAccepted { id, ts: now });
        self.resting.push(Resting {
            id,
            order,
            activate_in: self.config.latency_bars,
            armed: false,
            trail_ref: None,
            filled: 0,
        });
    }

    fn cancel(&mut self, id: ClientOrderId, now: Timestamp, sink: &mut dyn EventSink) {
        // Remove the resting order if present (which also frees any cash it had
        // reserved) and acknowledge the cancel. An unknown/already-terminal id
        // surfaces an OrderCancelRejected so a strategy polling for the ack stops
        // waiting instead of blocking forever (and behaves like a real venue).
        if let Some(pos) = self.resting.iter().position(|r| r.id == id) {
            self.resting.remove(pos);
            sink.emit(AccountEvent::OrderCanceled {
                id,
                reason: CancelReason::Requested,
                ts: now,
            });
        } else {
            sink.emit(AccountEvent::OrderCancelRejected {
                id,
                reason: CancelRejectReason::UnknownOrder,
                ts: now,
            });
        }
    }

    fn config_seed(&self) -> Option<u64> {
        // Report the seed only if a probabilistic model actually consumed the RNG —
        // so a deterministic run reports `None`, not an ambiguous `Some(0)` (i4).
        self.rng_drawn.then_some(self.config.seed)
    }

    #[allow(clippy::too_many_lines)]
    fn observe(&mut self, event: &Event, now: Timestamp, sink: &mut dyn EventSink) {
        let Event::Bar(bar) = event else { return };

        // Record the mark (this bar's close) for buying-power estimation at submit.
        self.shadow.entry(bar.instrument.index()).or_default().mark = bar.close.raw();

        // Volume cap for partial fills (in raw qty), shared across ALL of this
        // bar's resting orders for this instrument: it depletes as orders fill, so
        // N orders cannot each take the full bar volume (the bar is one instrument,
        // so a single running counter suffices).
        let mut cap = self
            .config
            .max_participation_bps
            .map(|bps| (i128::from(bar.volume.raw()) * i128::from(bps) / 10_000) as i64);

        // Collect fills (id, instrument, side, price, qty, completes_order) + OCO
        // groups to apply after the borrow ends.
        let mut to_fill: Vec<(ClientOrderId, InstrumentId, Side, Price, Qty, bool)> = Vec::new();
        // OCO groups that have taken liquidity this bar, with the keeper id. A
        // group is claimed on its FIRST fill (partial or full); any other leg of a
        // claimed group is canceled (one-cancels-other) instead of also filling.
        let mut claimed_oco: Vec<(u32, ClientOrderId)> = Vec::new();

        // The position as fills are *staged* this bar, so reduce-only caps account
        // for siblings already filled in this same loop (M11). Seeded from the
        // committed shadow; the shadow itself is updated after the loop.
        let mut net_after = self
            .shadow
            .get(&bar.instrument.index())
            .map_or(0, |s| s.net);

        let pending = std::mem::take(&mut self.resting);
        for mut r in pending {
            if r.order.instrument != bar.instrument {
                self.resting.push(r);
                continue;
            }
            if r.activate_in > 0 {
                r.activate_in -= 1; // latency: not yet active
                self.resting.push(r);
                continue;
            }

            // Every conditional order surfaces exactly one OrderTriggered when its
            // condition is first met: a stop-limit arms (set inside `evaluate`)
            // before working as a limit; a stop / market-if-touched / trailing-stop
            // triggers on its first fill (we arm it here so a partial-fill remainder
            // does not re-trigger). `r.armed` is the "already triggered" flag.
            let was_armed = r.armed;
            let outcome = Self::evaluate(&mut r, bar);
            let conditional_fill = matches!(
                r.order.kind,
                OrderKind::Stop { .. }
                    | OrderKind::MarketIfTouched { .. }
                    | OrderKind::TrailingStop { .. }
            ) && matches!(outcome, Outcome::Fill(_));
            if conditional_fill {
                r.armed = true;
            }
            if !was_armed && r.armed {
                sink.emit(AccountEvent::OrderTriggered { id: r.id, ts: now });
            }
            match outcome {
                Outcome::Cancel(reason) => {
                    sink.emit(AccountEvent::OrderCanceled {
                        id: r.id,
                        reason,
                        ts: now,
                    });
                }
                Outcome::Rest => {
                    if r.order.tif == TimeInForce::Gtc {
                        self.resting.push(r);
                    } else {
                        // IOC/FOK that could not act this bar expires per its TIF.
                        sink.emit(AccountEvent::OrderExpired { id: r.id, ts: now });
                    }
                }
                Outcome::Fill(base) => {
                    // Probabilistic maker fill at the touch (opt-in): a passive limit
                    // that the bar only *touched* (did not trade through) fills only
                    // with the configured probability — queue-position uncertainty.
                    // On a miss it did not fill, so it rests (GTC) / expires (IOC/FOK)
                    // WITHOUT claiming its OCO group.
                    if self.maker_touch_is_uncertain(&r.order, bar) && !self.draw_maker_fills() {
                        if r.order.tif == TimeInForce::Gtc {
                            self.resting.push(r);
                        } else {
                            sink.emit(AccountEvent::OrderExpired { id: r.id, ts: now });
                        }
                        continue;
                    }
                    // OCO: if a sibling in this group already took liquidity this
                    // bar, this leg is canceled, not filled — so a single wide bar
                    // cannot execute both legs of a bracket.
                    if let Some(g) = r.order.oco_group
                        && claimed_oco.iter().any(|(cg, cid)| *cg == g && *cid != r.id)
                    {
                        sink.emit(AccountEvent::OrderCanceled {
                            id: r.id,
                            reason: CancelReason::OcoTriggered,
                            ts: now,
                        });
                        continue;
                    }
                    let remaining = r.order.qty.raw() - r.filled;
                    let mut want = cap.map_or(remaining, |c| remaining.min(c.max(0)));
                    // reduce_only: never increase or flip the position. Cap the fill
                    // at what is still reducible given fills already staged this bar,
                    // so an oversized order (M10) or several concurrent reduce-only
                    // orders (M11) can never overshoot zero. The reduce-only side is
                    // opposite the net at submit; if the net has since been flattened
                    // or flipped there is nothing left to reduce.
                    let mut reduce_exhausted = false;
                    if r.order.reduce_only {
                        let reducible =
                            if net_after != 0 && net_after.signum() != r.order.side.sign() {
                                net_after.abs()
                            } else {
                                0
                            };
                        if want >= reducible {
                            want = reducible;
                            reduce_exhausted = true; // position fully reduced by this fill
                        }
                    }
                    // FOK must fill fully in one go.
                    if r.order.tif == TimeInForce::Fok && want < remaining {
                        sink.emit(AccountEvent::OrderExpired { id: r.id, ts: now });
                        continue;
                    }
                    if want <= 0 {
                        if r.order.reduce_only && reduce_exhausted {
                            // Nothing left to reduce (position already flat) — a
                            // reduce-only order cannot rest doing nothing; cancel it.
                            sink.emit(AccountEvent::OrderCanceled {
                                id: r.id,
                                reason: CancelReason::VenueCanceled,
                                ts: now,
                            });
                        } else if r.order.tif == TimeInForce::Gtc {
                            // Volume cap left no room this bar; keep (GTC) or expire.
                            self.resting.push(r);
                        } else {
                            sink.emit(AccountEvent::OrderExpired { id: r.id, ts: now });
                        }
                        continue;
                    }
                    // A passive (price-guaranteed) fill never executes worse than
                    // its limit; adverse slippage applies only to liquidity-taking
                    // kinds (market / stop / trailing / market-if-touched).
                    let price = match r.order.kind {
                        OrderKind::Limit { .. } | OrderKind::StopLimit { .. } => base,
                        // Liquidity-taking: flat slippage + size-sensitive impact.
                        _ => self.fill_price(base, r.order.side, want, bar.volume.raw()),
                    };
                    // A reduce-only order that has exhausted the reducible position
                    // is complete even if its nominal qty was larger (no remainder
                    // rests, M10).
                    let complete = (r.filled + want) >= r.order.qty.raw() || reduce_exhausted;
                    to_fill.push((
                        r.id,
                        r.order.instrument,
                        r.order.side,
                        price,
                        Qty::from_raw(want),
                        complete,
                    ));
                    r.filled += want;
                    // Track the position as fills are staged so a later reduce-only
                    // order in this same bar sees the already-reduced net (M11).
                    net_after += r.order.side.sign() * want;
                    // Deplete the shared bar volume cap so later orders on this
                    // instrument see only what's left.
                    if let Some(c) = cap.as_mut() {
                        *c -= want;
                    }
                    // Claim the OCO group on this (first) fill so siblings — whether
                    // they fill later in this loop or merely rest — are canceled.
                    if let Some(g) = r.order.oco_group
                        && !claimed_oco.iter().any(|(cg, _)| *cg == g)
                    {
                        claimed_oco.push((g, r.id));
                    }

                    if !complete {
                        if r.order.tif == TimeInForce::Gtc {
                            self.resting.push(r); // partial; rest the remainder
                        } else {
                            // IOC: take what we got, the remainder expires.
                            sink.emit(AccountEvent::OrderExpired { id: r.id, ts: now });
                        }
                    }
                }
            }
        }

        // Emit fills + update shadow positions.
        for (id, instrument, side, price, qty, complete) in to_fill {
            let costs = self.fee_cost(instrument, price, qty);
            self.shadow
                .entry(instrument.index())
                .or_default()
                .apply(side, price.raw(), qty.raw());
            self.settle_cash(side, price, qty, &costs);
            sink.emit(AccountEvent::Fill {
                client_order_id: id,
                instrument,
                side,
                price,
                qty,
                costs,
                complete,
                ts: now,
            });
        }

        // OCO sibling cancels: any leg of a claimed group still resting is canceled
        // (the keeper is the leg that took liquidity this bar).
        for (group, keep) in claimed_oco {
            self.cancel_oco_siblings(group, keep, now, sink);
        }

        self.accrue_funding(bar, now, sink);
        self.check_liquidation(bar, now, sink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{AssetId, CapSet, InstrumentKind, TriggerBy};

    const I: InstrumentId = InstrumentId::new(0);

    fn spec_kind(kind: InstrumentKind) -> InstrumentSpec {
        InstrumentSpec::new(
            I,
            AssetId::new(0),
            AssetId::new(1),
            kind,
            Price::from_raw(1),
            Qty::from_raw(1),
            Money::from_raw(10),
            CapSet::empty(),
        )
    }
    fn spec() -> InstrumentSpec {
        spec_kind(InstrumentKind::Spot)
    }
    fn bar(open: i64, high: i64, low: i64, close: i64, vol: i64) -> Event {
        Event::Bar(Bar::new(
            I,
            Timestamp::from_nanos(1),
            Price::from_raw(open),
            Price::from_raw(high),
            Price::from_raw(low),
            Price::from_raw(close),
            Qty::from_raw(vol),
        ))
    }
    fn buy_mkt(q: i64) -> OrderRequest {
        OrderRequest::market(I, Side::Buy, Qty::from_raw(q))
    }
    fn fills(sink: &[AccountEvent]) -> Vec<(Price, Qty, Side)> {
        sink.iter()
            .filter_map(|e| match e {
                AccountEvent::Fill {
                    price, qty, side, ..
                } => Some((*price, *qty, *side)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn stop_limit_same_bar_arm_fills_at_trigger_not_open() {
        // M8: a stop-limit that arms AND fills in one bar must not get price
        // improvement from bar.open (which predates activation). Buy SL trigger=110,
        // limit=115; bar(open=100, high=125, low=103): arms (high>=110) and fills at
        // the trigger (110), NOT the optimistic open (100).
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::stop_limit(
                I,
                Side::Buy,
                Qty::from_raw(1),
                Price::from_raw(110),
                Price::from_raw(115),
                TriggerBy::Last,
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(
            &bar(100, 125, 103, 120, 1000),
            Timestamp::from_nanos(2),
            &mut s,
        );
        assert_eq!(
            fills(&s),
            vec![(Price::from_raw(110), Qty::from_raw(1), Side::Buy)],
            "fills at the trigger, not bar.open (M8)"
        );
    }

    #[test]
    fn stop_limit_same_bar_post_only_rests_not_cancelled() {
        // M9: a just-armed post-only stop-limit's marketability is measured at the
        // trigger (110), not bar.open (108). limit 109 < trigger 110 is
        // non-marketable, so it rests rather than being wrongly cancelled.
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::stop_limit(
                I,
                Side::Buy,
                Qty::from_raw(1),
                Price::from_raw(110),
                Price::from_raw(109),
                TriggerBy::Last,
            )
            .post_only(),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        // Arms (high 115 >= 110); low 110 does not trade through limit 109.
        x.observe(
            &bar(108, 115, 110, 112, 1000),
            Timestamp::from_nanos(2),
            &mut s,
        );
        assert!(fills(&s).is_empty(), "must not fill");
        assert!(
            !s.iter()
                .any(|e| matches!(e, AccountEvent::OrderCanceled { .. })),
            "must not cancel a non-marketable post-only leg (M9)"
        );
        assert_eq!(x.resting_count(), 1, "rests as a working limit");
    }

    #[test]
    fn reduce_only_caps_at_position_and_does_not_flip() {
        // M10: a reduce-only order larger than the position reduces to flat and
        // stops — it never flips. Short 5, then reduce-only BUY 10 fills only 5.
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Sell, Qty::from_raw(5)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(
            &bar(100, 100, 100, 100, 1000),
            Timestamp::from_nanos(2),
            &mut s,
        );
        s.clear();
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(10)).reduce_only(),
            Timestamp::from_nanos(3),
            &mut s,
        );
        s.clear();
        x.observe(
            &bar(100, 100, 100, 100, 1000),
            Timestamp::from_nanos(4),
            &mut s,
        );
        assert_eq!(
            fills(&s),
            vec![(Price::from_raw(100), Qty::from_raw(5), Side::Buy)],
            "reduce-only fills only the 5 that flattens, not 10 (M10)"
        );
    }

    #[test]
    fn post_only_on_market_order_is_rejected() {
        // m1: post_only is a contradiction on a market order (it always takes
        // liquidity) -> rejected, not silently filled.
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(1).post_only(),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(
            s.as_slice(),
            [AccountEvent::OrderRejected {
                reason: RejectReason::InvalidOrder,
                ..
            }]
        ));
    }

    #[test]
    fn liquidation_cancels_resting_orders() {
        // M15: a force-liquidation cancels all working orders on the instrument so a
        // resting order can't silently re-open the position next bar.
        let mut x = SimulatedExchange::new(vec![spec()], 0).with_liquidation_bps(500); // 5%
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(10),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(
            &bar(100, 100, 100, 100, 1000),
            Timestamp::from_nanos(2),
            &mut s,
        ); // long 10 @ 100
        s.clear();
        // A resting take-profit sell @ 200 that won't fill on a crash bar.
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::limit(I, Side::Sell, Qty::from_raw(10), Price::from_raw(200)),
            Timestamp::from_nanos(3),
            &mut s,
        );
        s.clear();
        // Crash: close 90 is a 10% adverse move from avg 100 -> liquidation.
        x.observe(&bar(95, 95, 90, 90, 1000), Timestamp::from_nanos(4), &mut s);
        assert!(
            s.iter()
                .any(|e| matches!(e, AccountEvent::Liquidation { .. })),
            "position liquidated"
        );
        assert!(
            s.iter().any(|e| matches!(
                e,
                AccountEvent::OrderCanceled { id, reason: CancelReason::VenueCanceled, .. }
                    if *id == ClientOrderId::new(1)
            )),
            "the resting order is cancelled on liquidation (M15)"
        );
        assert_eq!(x.resting_count(), 0, "no resting orders remain");
    }

    #[test]
    fn probabilistic_maker_fill_at_touch_is_seed_gated() {
        // A limit BUY @100 that the bar only TOUCHES (low == 100 exactly) fills
        // probabilistically. p=0 never fills (rests); p=10_000 always fills. The
        // seed is reported only once the RNG is drawn (i4).
        let touch = bar(101, 102, 100, 101, 1000); // low == limit, not through
        let mut x0 = SimulatedExchange::new(vec![spec()], 0).with_maker_fill_probability(0);
        let mut s = Vec::new();
        x0.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(10), Price::from_raw(100)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x0.observe(&touch, Timestamp::from_nanos(2), &mut s);
        assert!(fills(&s).is_empty(), "p=0: a touch never fills");
        assert_eq!(x0.resting_count(), 1, "it rests");
        assert_eq!(
            x0.config_seed(),
            Some(0),
            "RNG was drawn -> seed reported (i4)"
        );

        let mut x1 = SimulatedExchange::new(vec![spec()], 0).with_maker_fill_probability(10_000);
        let mut s1 = Vec::new();
        x1.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(10), Price::from_raw(100)),
            Timestamp::from_nanos(1),
            &mut s1,
        );
        s1.clear();
        x1.observe(&touch, Timestamp::from_nanos(2), &mut s1);
        assert_eq!(
            fills(&s1),
            vec![(Price::from_raw(100), Qty::from_raw(10), Side::Buy)],
            "p=10_000: a touch always fills"
        );
    }

    #[test]
    fn probabilistic_through_always_fills_and_default_reports_no_seed() {
        // A decisive trade-THROUGH (low 99 < limit 100) always fills, even at p=0.
        let mut x = SimulatedExchange::new(vec![spec()], 0).with_maker_fill_probability(0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(10), Price::from_raw(100)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(
            &bar(101, 102, 99, 101, 1000),
            Timestamp::from_nanos(2),
            &mut s,
        );
        assert_eq!(
            fills(&s),
            vec![(Price::from_raw(100), Qty::from_raw(10), Side::Buy)],
            "a trade-through always fills regardless of probability"
        );
        // i4: a deterministic exchange (no probabilistic model) reports no seed.
        let plain = SimulatedExchange::new(vec![spec()], 0);
        assert_eq!(plain.config_seed(), None);
    }

    #[test]
    fn market_next_bar_with_slippage() {
        let mut x = SimulatedExchange::new(vec![spec()], 0).with_slippage_bps(100); // 1%
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(10),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(
            &bar(100, 105, 95, 102, 1000),
            Timestamp::from_nanos(2),
            &mut s,
        );
        // open 100 + 1% slippage (buy pays more) = 101.
        assert_eq!(
            fills(&s),
            vec![(Price::from_raw(101), Qty::from_raw(10), Side::Buy)]
        );
    }

    #[test]
    fn limit_fill_is_never_worse_than_limit_under_slippage() {
        // A passive limit provides liquidity; slippage must NOT push its fill
        // through the limit price (regression: it used to fill at 101 > 100).
        let mut x = SimulatedExchange::new(vec![spec()], 0).with_slippage_bps(100); // 1%
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(10), Price::from_raw(100)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        // The bar trades down through the limit (low 99 <= 100), so it fills.
        x.observe(
            &bar(100, 101, 99, 100, 1000),
            Timestamp::from_nanos(2),
            &mut s,
        );
        // Exactly at the limit (100), NOT 100 + 1% = 101.
        assert_eq!(
            fills(&s),
            vec![(Price::from_raw(100), Qty::from_raw(10), Side::Buy)]
        );
    }

    #[test]
    fn oco_both_legs_fillable_on_one_wide_bar_fills_only_one() {
        // Bracket: take-profit sell limit @110 and stop-loss sell stop @90 in one
        // OCO group. A single wide bar (high 112, low 88) makes BOTH fillable; only
        // one may execute, the other cancels (regression: both used to fill).
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Sell, Qty::from_raw(1), Price::from_raw(110)).oco(7),
            Timestamp::from_nanos(1),
            &mut s,
        );
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::stop(
                I,
                Side::Sell,
                Qty::from_raw(1),
                Price::from_raw(90),
                TriggerBy::Last,
            )
            .oco(7),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(
            &bar(100, 112, 88, 100, 10),
            Timestamp::from_nanos(2),
            &mut s,
        );
        assert_eq!(
            fills(&s).len(),
            1,
            "only one OCO leg may execute on a wide bar"
        );
        assert!(s.iter().any(|e| matches!(
            e,
            AccountEvent::OrderCanceled {
                reason: CancelReason::OcoTriggered,
                ..
            }
        )));
        assert_eq!(x.resting_count(), 0);
    }

    #[test]
    fn buy_limit_price_improves_on_gap_down_open() {
        // A resting buy limit @100; the bar GAPS DOWN and opens at 95 (already
        // below the limit) -> fill at the better open (95), not the limit (100).
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(1), Price::from_raw(100)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(95, 96, 94, 95, 10), Timestamp::from_nanos(2), &mut s);
        assert_eq!(
            fills(&s),
            vec![(Price::from_raw(95), Qty::from_raw(1), Side::Buy)]
        );
    }

    #[test]
    fn percent_trailing_stop_fires_at_callback_rate() {
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        // Sell trailing stop, 10% (1000 bps) callback.
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::trailing_stop_pct(I, Side::Sell, Qty::from_raw(1), 1000, TriggerBy::Last),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        // high 100 -> trigger = 100 - 100*1000/10000 = 90; low 85 <= 90 -> fill @90.
        x.observe(&bar(95, 100, 85, 90, 10), Timestamp::from_nanos(2), &mut s);
        assert_eq!(
            fills(&s),
            vec![(Price::from_raw(90), Qty::from_raw(1), Side::Sell)]
        );
    }

    #[test]
    fn participation_cap_depletes_across_orders_in_a_bar() {
        // cap = 10% of bar volume 100 = 10. Two buys of 8 each on the same
        // instrument must together fill at most 10 (was: each filled 8).
        let mut x = SimulatedExchange::new(vec![spec()], 0).with_participation_bps(1000);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(8),
            Timestamp::from_nanos(1),
            &mut s,
        );
        x.submit(
            ClientOrderId::new(1),
            buy_mkt(8),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(
            &bar(100, 100, 100, 100, 100),
            Timestamp::from_nanos(2),
            &mut s,
        );
        let total: i64 = fills(&s).iter().map(|(_, q, _)| q.raw()).sum();
        assert_eq!(
            total, 10,
            "combined fills capped at the bar's participation"
        );
    }

    #[test]
    fn reduce_only_rejects_unless_it_reduces() {
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        // Flat: a reduce-only order has nothing to reduce -> rejected.
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(5).reduce_only(),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(
            s[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::WouldIncreasePosition,
                ..
            }
        ));
        // Open a short with a plain sell that fills next bar.
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::market(I, Side::Sell, Qty::from_raw(5)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        x.observe(
            &bar(100, 100, 100, 100, 1000),
            Timestamp::from_nanos(2),
            &mut s,
        );
        s.clear();
        // Reduce-only BUY reduces the short -> accepted.
        x.submit(
            ClientOrderId::new(2),
            buy_mkt(3).reduce_only(),
            Timestamp::from_nanos(3),
            &mut s,
        );
        assert!(
            matches!(s[0], AccountEvent::OrderAccepted { .. }),
            "reduce-only buy must reduce the short, not be rejected"
        );
        s.clear();
        // Reduce-only SELL would increase the short -> rejected.
        x.submit(
            ClientOrderId::new(3),
            OrderRequest::market(I, Side::Sell, Qty::from_raw(3)).reduce_only(),
            Timestamp::from_nanos(3),
            &mut s,
        );
        assert!(matches!(
            s[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::WouldIncreasePosition,
                ..
            }
        ));
    }

    #[test]
    fn latency_delays_fill() {
        let mut x = SimulatedExchange::new(vec![spec()], 0).with_latency_bars(1);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(1),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(
            &bar(100, 100, 100, 100, 10),
            Timestamp::from_nanos(2),
            &mut s,
        );
        assert!(fills(&s).is_empty()); // still in latency
        x.observe(
            &bar(101, 101, 101, 101, 10),
            Timestamp::from_nanos(3),
            &mut s,
        );
        assert_eq!(fills(&s)[0].0, Price::from_raw(101));
    }

    #[test]
    fn partial_fills_via_participation() {
        // cap = 10% of volume. volume 100 -> 10 per bar. order 25 -> 10,10,5.
        let mut x = SimulatedExchange::new(vec![spec()], 0).with_participation_bps(1000);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(25),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        for t in 2..=4 {
            x.observe(
                &bar(100, 100, 100, 100, 100),
                Timestamp::from_nanos(t),
                &mut s,
            );
        }
        let qs: Vec<i64> = fills(&s).iter().map(|f| f.1.raw()).collect();
        assert_eq!(qs, vec![10, 10, 5]);
        assert_eq!(x.resting_count(), 0);
    }

    #[test]
    fn stop_triggers_and_fills() {
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        let stop = OrderRequest::stop(
            I,
            Side::Buy,
            Qty::from_raw(1),
            Price::from_raw(110),
            TriggerBy::Last,
        );
        x.submit(
            ClientOrderId::new(0),
            stop,
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(
            &bar(100, 105, 99, 102, 10),
            Timestamp::from_nanos(2),
            &mut s,
        ); // below trigger
        assert!(fills(&s).is_empty());
        x.observe(
            &bar(108, 112, 107, 111, 10),
            Timestamp::from_nanos(3),
            &mut s,
        ); // high 112 >= 110
        assert_eq!(fills(&s)[0].0, Price::from_raw(110));
    }

    #[test]
    fn stop_limit_two_stage() {
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        // arm at 110, then limit buy at 109 (fills when price dips to 109).
        let o = OrderRequest::stop_limit(
            I,
            Side::Buy,
            Qty::from_raw(1),
            Price::from_raw(110),
            Price::from_raw(109),
            TriggerBy::Last,
        );
        x.submit(ClientOrderId::new(0), o, Timestamp::from_nanos(1), &mut s);
        s.clear();
        x.observe(
            &bar(108, 112, 111, 111, 10),
            Timestamp::from_nanos(2),
            &mut s,
        ); // arms (high>=110), but low 111 > 109 -> no fill
        assert!(fills(&s).is_empty());
        assert_eq!(x.resting_count(), 1);
        x.observe(
            &bar(110, 110, 108, 109, 10),
            Timestamp::from_nanos(3),
            &mut s,
        ); // low 108 <= 109 -> fill at 109
        assert_eq!(fills(&s)[0].0, Price::from_raw(109));
    }

    #[test]
    fn market_if_touched_fires_favourably() {
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        let o = OrderRequest::market_if_touched(
            I,
            Side::Buy,
            Qty::from_raw(1),
            Price::from_raw(90),
            TriggerBy::Last,
        );
        x.submit(ClientOrderId::new(0), o, Timestamp::from_nanos(1), &mut s);
        s.clear();
        x.observe(&bar(100, 101, 95, 98, 10), Timestamp::from_nanos(2), &mut s); // low 95 > 90 no
        assert!(fills(&s).is_empty());
        x.observe(&bar(95, 96, 89, 92, 10), Timestamp::from_nanos(3), &mut s); // low 89 <= 90 fill
        assert_eq!(fills(&s)[0].0, Price::from_raw(90));
    }

    #[test]
    fn trailing_stop_follows_then_fires() {
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        // sell trailing stop, trail 5: trails the high; fires when low <= high-5.
        let o = OrderRequest::trailing_stop(
            I,
            Side::Sell,
            Qty::from_raw(1),
            Price::from_raw(5),
            TriggerBy::Last,
        );
        x.submit(ClientOrderId::new(0), o, Timestamp::from_nanos(1), &mut s);
        s.clear();
        x.observe(
            &bar(100, 110, 100, 108, 10),
            Timestamp::from_nanos(2),
            &mut s,
        ); // ref=110, trigger 105, low 100<=105 -> fires at 105
        assert_eq!(fills(&s)[0].0, Price::from_raw(105));
    }

    #[test]
    fn post_only_cancels_if_marketable() {
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        // buy limit 105 is marketable vs open 100 (>= open) -> post-only cancels.
        let o =
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(1), Price::from_raw(105)).post_only();
        x.submit(ClientOrderId::new(0), o, Timestamp::from_nanos(1), &mut s);
        s.clear();
        x.observe(
            &bar(100, 106, 99, 102, 10),
            Timestamp::from_nanos(2),
            &mut s,
        );
        assert!(matches!(s[0], AccountEvent::OrderCanceled { .. }));
        assert!(fills(&s).is_empty());
    }

    #[test]
    fn oco_sibling_cancelled_on_fill() {
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        // two market orders in OCO group 1; first fills, second cancels.
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(1).oco(1),
            Timestamp::from_nanos(1),
            &mut s,
        );
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::limit(I, Side::Sell, Qty::from_raw(1), Price::from_raw(200)).oco(1),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(
            &bar(100, 101, 99, 100, 10),
            Timestamp::from_nanos(2),
            &mut s,
        );
        // market (id 0) fills; id 1 cancelled by OCO.
        assert_eq!(fills(&s).len(), 1);
        assert!(s.iter().any(
            |e| matches!(e, AccountEvent::OrderCanceled { id, .. } if *id == ClientOrderId::new(1))
        ));
        assert_eq!(x.resting_count(), 0);
    }

    #[test]
    fn ioc_cancels_unfilled_remainder() {
        let mut x = SimulatedExchange::new(vec![spec()], 0).with_participation_bps(1000); // 10/bar
        let mut s = Vec::new();
        let o = buy_mkt(25).with_tif(TimeInForce::Ioc);
        x.submit(ClientOrderId::new(0), o, Timestamp::from_nanos(1), &mut s);
        s.clear();
        x.observe(
            &bar(100, 100, 100, 100, 100),
            Timestamp::from_nanos(2),
            &mut s,
        );
        // fills 10, the remaining 15 expires per IOC.
        assert_eq!(fills(&s)[0].1, Qty::from_raw(10));
        assert!(
            s.iter()
                .any(|e| matches!(e, AccountEvent::OrderExpired { .. }))
        );
        assert_eq!(x.resting_count(), 0);
    }

    #[test]
    fn fok_cancels_if_cannot_fully_fill() {
        let mut x = SimulatedExchange::new(vec![spec()], 0).with_participation_bps(1000);
        let mut s = Vec::new();
        let o = buy_mkt(25).with_tif(TimeInForce::Fok); // cap 10 < 25 -> kill
        x.submit(ClientOrderId::new(0), o, Timestamp::from_nanos(1), &mut s);
        s.clear();
        x.observe(
            &bar(100, 100, 100, 100, 100),
            Timestamp::from_nanos(2),
            &mut s,
        );
        assert!(fills(&s).is_empty());
        assert!(matches!(s[0], AccountEvent::OrderExpired { .. }));
    }

    #[test]
    fn funding_charged_on_perp_position() {
        let mut x = SimulatedExchange::with_config(
            vec![spec_kind(InstrumentKind::PerpetualFuture)],
            FillConfig {
                fee_bps: 0,
                ..FillConfig::default()
            },
        )
        .with_funding(10, 1); // 10 bps every bar
        let mut s = Vec::new();
        // open a long: market buy fills next bar.
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(10),
            Timestamp::from_nanos(1),
            &mut s,
        );
        x.observe(
            &bar(100, 100, 100, 100, 1000),
            Timestamp::from_nanos(2),
            &mut s,
        ); // fill 10 @ 100
        s.clear();
        // next bar: funding accrues on net 10 @ close 100 -> notional 1000 * 10bps = 1.
        x.observe(
            &bar(100, 100, 100, 100, 1000),
            Timestamp::from_nanos(3),
            &mut s,
        );
        let funding: Vec<_> = s
            .iter()
            .filter_map(|e| match e {
                AccountEvent::FundingSettlement { cost, .. } => Some(*cost),
                _ => None,
            })
            .collect();
        assert_eq!(funding.len(), 1);
        assert_eq!(funding[0].kind, CostKind::Funding);
        assert_eq!(funding[0].amount, Money::from_raw(1));
    }

    #[test]
    fn funding_rate_at_lookup() {
        let sched = vec![
            (Timestamp::from_nanos(10), 7),
            (Timestamp::from_nanos(20), 13),
        ];
        assert_eq!(funding_rate_at(&[], Timestamp::from_nanos(5)), 0); // empty
        assert_eq!(funding_rate_at(&sched, Timestamp::from_nanos(5)), 0); // before first
        assert_eq!(funding_rate_at(&sched, Timestamp::from_nanos(10)), 7); // at first
        assert_eq!(funding_rate_at(&sched, Timestamp::from_nanos(15)), 7); // between
        assert_eq!(funding_rate_at(&sched, Timestamp::from_nanos(20)), 13); // at second
        assert_eq!(funding_rate_at(&sched, Timestamp::from_nanos(99)), 13); // after last
    }

    #[test]
    fn funding_schedule_replays_time_varying_rate() {
        let mut x = SimulatedExchange::with_config(
            vec![spec_kind(InstrumentKind::PerpetualFuture)],
            FillConfig {
                fee_bps: 0,
                ..FillConfig::default()
            },
        )
        .with_funding_schedule(
            InstrumentId::new(0),
            1, // settle every bar
            // Passed UNSORTED on purpose — it is sorted on insertion.
            vec![
                (Timestamp::from_nanos(5), 50),
                (Timestamp::from_nanos(2), 10),
            ],
        );
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(10),
            Timestamp::from_nanos(1),
            &mut s,
        );
        // Fill the long 10 @ 100.
        x.observe(
            &bar(100, 100, 100, 100, 1000),
            Timestamp::from_nanos(2),
            &mut s,
        );

        // Settle each bar from ts 3..=6 and collect (reported_rate, cost).
        let mut seen = Vec::new();
        for t in 3..=6 {
            s.clear();
            x.observe(
                &bar(100, 100, 100, 100, 1000),
                Timestamp::from_nanos(t),
                &mut s,
            );
            for e in &s {
                if let AccountEvent::FundingSettlement { rate, cost, .. } = e {
                    seen.push((*rate, cost.amount));
                }
            }
        }
        // ts 3,4 → rate 10 (notional 1000 · 10bps = 1); ts 5,6 → rate 50 (= 5).
        // Crucially, the FundingSettlement reports the LOOKED-UP rate, not a
        // config constant.
        assert_eq!(
            seen,
            vec![
                (10, Money::from_raw(1)),
                (10, Money::from_raw(1)),
                (50, Money::from_raw(5)),
                (50, Money::from_raw(5)),
            ]
        );
    }

    #[test]
    fn volume_share_impact_scales_with_size() {
        let cfg = FillConfig {
            fee_bps: 0,
            ..FillConfig::default()
        };
        let run = |impact_bps: i64, qty: i64| -> i64 {
            let mut x = SimulatedExchange::with_config(vec![spec_kind(InstrumentKind::Spot)], cfg)
                .with_impact_model(impact_bps);
            let mut s = Vec::new();
            x.submit(
                ClientOrderId::new(0),
                buy_mkt(qty),
                Timestamp::from_nanos(1),
                &mut s,
            );
            s.clear();
            // Bar open 100_000, volume 100.
            x.observe(
                &bar(100_000, 100_000, 100_000, 100_000, 100),
                Timestamp::from_nanos(2),
                &mut s,
            );
            s.iter()
                .find_map(|e| match e {
                    AccountEvent::Fill { price, .. } => Some(price.raw()),
                    _ => None,
                })
                .expect("fill")
        };
        // Impact off → fill at the base (bar open).
        assert_eq!(run(0, 50), 100_000);
        // 50% participation × 100 bps = 50 bps → +500.
        assert_eq!(run(100, 50), 100_500);
        // 1% participation × 100 bps = 1 bp → +10.
        assert_eq!(run(100, 1), 100_010);
        // A larger fill is strictly worse for a buy (size has a price).
        assert!(run(100, 50) > run(100, 1));
    }

    #[test]
    fn liquidation_force_closes_on_adverse_move() {
        let mut x = SimulatedExchange::with_config(
            vec![spec_kind(InstrumentKind::PerpetualFuture)],
            FillConfig::default(),
        )
        .with_liquidation_bps(1000); // liquidate at 10% adverse
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(10),
            Timestamp::from_nanos(1),
            &mut s,
        );
        x.observe(
            &bar(100, 100, 100, 100, 1000),
            Timestamp::from_nanos(2),
            &mut s,
        ); // long 10 @ 100
        s.clear();
        // price drops to 88 (12% below entry) -> liquidate (force sell 10).
        x.observe(&bar(95, 96, 88, 88, 1000), Timestamp::from_nanos(3), &mut s);
        let liq: Vec<_> = s
            .iter()
            .filter_map(|e| match e {
                AccountEvent::Liquidation { side, qty, .. } => Some((*side, *qty)),
                _ => None,
            })
            .collect();
        assert_eq!(liq.len(), 1);
        assert_eq!(liq[0].0, Side::Sell);
        assert_eq!(liq[0].1, Qty::from_raw(10));
    }

    #[test]
    fn rejects_bad_orders() {
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(0),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(matches!(s[0], AccountEvent::OrderRejected { .. }));
        s.clear();
        let bad = OrderRequest::market(InstrumentId::new(9), Side::Buy, Qty::from_raw(1));
        x.submit(ClientOrderId::new(1), bad, Timestamp::from_nanos(1), &mut s);
        assert!(matches!(s[0], AccountEvent::OrderRejected { .. }));
    }

    #[test]
    fn observe_ignores_non_bar_events() {
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        x.observe(
            &Event::Resync {
                instrument: None,
                ts: Timestamp::from_nanos(1),
            },
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert!(s.is_empty());
        assert_eq!(x.seed(), 0);
    }

    #[test]
    fn conservative_default_still_fills_next_open_no_fee() {
        // The simple path used by the parity tests is unchanged.
        let mut x = SimulatedExchange::new(vec![spec()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            buy_mkt(2),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(
            &bar(100, 105, 95, 102, 10),
            Timestamp::from_nanos(2),
            &mut s,
        );
        assert_eq!(
            fills(&s),
            vec![(Price::from_raw(100), Qty::from_raw(2), Side::Buy)]
        );
    }
}

#[cfg(test)]
mod cov_tests {
    use super::*;
    use akadro_core::{AssetId, CapSet, InstrumentKind, TriggerBy};

    const I: InstrumentId = InstrumentId::new(0);
    fn perp() -> InstrumentSpec {
        InstrumentSpec::new(
            I,
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::PerpetualFuture,
            Price::from_raw(1),
            Qty::from_raw(1),
            Money::from_raw(10),
            CapSet::empty(),
        )
    }
    fn spot() -> InstrumentSpec {
        InstrumentSpec::new(
            I,
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(1),
            Qty::from_raw(1),
            Money::from_raw(10),
            CapSet::empty(),
        )
    }
    fn spec_other() -> InstrumentSpec {
        InstrumentSpec::new(
            InstrumentId::new(1),
            AssetId::new(0),
            AssetId::new(1),
            InstrumentKind::Spot,
            Price::from_raw(1),
            Qty::from_raw(1),
            Money::from_raw(10),
            CapSet::empty(),
        )
    }
    fn bar(open: i64, high: i64, low: i64, close: i64, vol: i64) -> Event {
        Event::Bar(Bar::new(
            I,
            Timestamp::from_nanos(1),
            Price::from_raw(open),
            Price::from_raw(high),
            Price::from_raw(low),
            Price::from_raw(close),
            Qty::from_raw(vol),
        ))
    }
    fn t() -> Timestamp {
        Timestamp::from_nanos(2)
    }
    fn fills(s: &[AccountEvent]) -> Vec<(Price, Qty, Side)> {
        s.iter()
            .filter_map(|e| match e {
                AccountEvent::Fill {
                    price, qty, side, ..
                } => Some((*price, *qty, *side)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn stop_sell_triggers_on_low() {
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::stop(
                I,
                Side::Sell,
                Qty::from_raw(1),
                Price::from_raw(90),
                TriggerBy::Last,
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100, 101, 89, 92, 10), t(), &mut s); // low 89 <= 90 -> fire at 90
        assert_eq!(fills(&s)[0].0, Price::from_raw(90));
    }

    #[test]
    fn mit_sell_fires_on_high() {
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market_if_touched(
                I,
                Side::Sell,
                Qty::from_raw(1),
                Price::from_raw(110),
                TriggerBy::Last,
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100, 112, 99, 108, 10), t(), &mut s); // high 112 >= 110
        assert_eq!(fills(&s)[0].0, Price::from_raw(110));
    }

    #[test]
    fn trailing_buy_follows_low_then_fires() {
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        // buy trailing trails the low; fires when high >= low+trail.
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::trailing_stop(
                I,
                Side::Buy,
                Qty::from_raw(1),
                Price::from_raw(5),
                TriggerBy::Last,
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100, 106, 100, 104, 10), t(), &mut s); // ref low 100, trigger 105, high 106>=105 -> fire 105
        assert_eq!(fills(&s)[0].0, Price::from_raw(105));
    }

    #[test]
    fn participation_zero_volume_keeps_order() {
        let mut x = SimulatedExchange::new(vec![spot()], 0).with_participation_bps(1000);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(5)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100, 100, 100, 100, 0), t(), &mut s); // volume 0 -> cap 0 -> no fill, keep
        assert!(fills(&s).is_empty());
        assert_eq!(x.resting_count(), 1);
    }

    #[test]
    fn funding_and_liquidation_on_short() {
        let mut x = SimulatedExchange::with_config(vec![perp()], FillConfig::default())
            .with_funding(10, 1)
            .with_liquidation_bps(1000);
        let mut s = Vec::new();
        // open a short: sell 10.
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Sell, Qty::from_raw(10)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        x.observe(&bar(100, 100, 100, 100, 1000), t(), &mut s); // short 10 @ 100
        s.clear();
        // price rises to 112 (>10% adverse for a short) -> funding (negative) + liquidation (buy to close).
        x.observe(
            &bar(108, 113, 107, 112, 1000),
            Timestamp::from_nanos(3),
            &mut s,
        );
        let funding = s
            .iter()
            .filter(|e| matches!(e, AccountEvent::FundingSettlement { .. }))
            .count();
        assert_eq!(funding, 1); // short pays/receives funding
        let liq = s
            .iter()
            .filter(|e| {
                matches!(e, AccountEvent::Liquidation { side, qty, .. }
                    if qty.raw() == 10 && *side == Side::Buy)
            })
            .count();
        assert_eq!(liq, 1); // forced buy-to-close
    }

    #[test]
    fn shadow_flip_long_to_short() {
        // Drive two fills that flip the shadow position through zero (covers the
        // flip branch of Shadow::apply).
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(5)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        x.observe(&bar(100, 100, 100, 100, 1000), t(), &mut s); // long 5
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::market(I, Side::Sell, Qty::from_raw(8)),
            Timestamp::from_nanos(3),
            &mut s,
        );
        x.observe(
            &bar(120, 120, 120, 120, 1000),
            Timestamp::from_nanos(4),
            &mut s,
        ); // sell 8 -> flip to short 3
        assert!(!fills(&s).is_empty());
    }

    fn cancels(s: &[AccountEvent]) -> Vec<(ClientOrderId, CancelReason)> {
        s.iter()
            .filter_map(|e| match e {
                AccountEvent::OrderCanceled { id, reason, .. } => Some((*id, *reason)),
                _ => None,
            })
            .collect()
    }
    fn expiries(s: &[AccountEvent]) -> Vec<ClientOrderId> {
        s.iter()
            .filter_map(|e| match e {
                AccountEvent::OrderExpired { id, .. } => Some(*id),
                _ => None,
            })
            .collect()
    }
    fn rejects(s: &[AccountEvent]) -> Vec<RejectReason> {
        s.iter()
            .filter_map(|e| match e {
                AccountEvent::OrderRejected { reason, .. } => Some(*reason),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn with_seed_builder_sets_config() {
        // The builder is part of the public surface; exercise it.
        let x = SimulatedExchange::new(vec![spot()], 0).with_seed(7);
        assert_eq!(x.resting_count(), 0);
    }

    #[test]
    fn limit_below_min_notional_is_rejected() {
        // spec min_notional = 10 raw; a limit of 1 * qty 1 = 1 < 10 -> reject.
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(1), Price::from_raw(1)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        assert_eq!(rejects(&s), vec![RejectReason::InvalidOrder]);
        assert_eq!(x.resting_count(), 0);
    }

    #[test]
    fn mit_buy_fires_on_low_touch() {
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market_if_touched(
                I,
                Side::Buy,
                Qty::from_raw(1),
                Price::from_raw(90),
                TriggerBy::Last,
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100, 101, 88, 95, 10), t(), &mut s); // low 88 <= 90 -> fire at 90
        assert_eq!(fills(&s)[0].0, Price::from_raw(90));
    }

    #[test]
    fn marketable_sell_limit_fills_immediately() {
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        // sell limit at 90 with open 100 -> 90 <= 100 -> marketable.
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Sell, Qty::from_raw(1), Price::from_raw(90)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100, 105, 95, 100, 10), t(), &mut s);
        assert_eq!(fills(&s)[0].2, Side::Sell);
    }

    #[test]
    fn trailing_sell_rests_then_fires_on_second_bar() {
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::trailing_stop(
                I,
                Side::Sell,
                Qty::from_raw(1),
                Price::from_raw(5),
                TriggerBy::Last,
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        // bar1: ref high 120, trigger 115, low 116 > 115 -> rest.
        x.observe(&bar(118, 120, 116, 119, 10), t(), &mut s);
        assert!(fills(&s).is_empty());
        assert_eq!(x.resting_count(), 1);
        // bar2: high 118 (ref stays 120 via max), trigger 115, low 114 <= 115 -> fire 115.
        x.observe(
            &bar(117, 118, 114, 115, 10),
            Timestamp::from_nanos(3),
            &mut s,
        );
        assert_eq!(fills(&s)[0].0, Price::from_raw(115));
    }

    #[test]
    fn trailing_buy_rests_then_trails_down_and_fires() {
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::trailing_stop(
                I,
                Side::Buy,
                Qty::from_raw(1),
                Price::from_raw(5),
                TriggerBy::Last,
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        // bar1: ref low 95, trigger 100, high 98 < 100 -> rest.
        x.observe(&bar(97, 98, 95, 96, 10), t(), &mut s);
        assert!(fills(&s).is_empty());
        // bar2: low 93 (ref trails down to 93 via min), trigger 98, high 102 >= 98 -> fire 98.
        x.observe(&bar(95, 102, 93, 100, 10), Timestamp::from_nanos(3), &mut s);
        assert_eq!(fills(&s)[0].0, Price::from_raw(98));
    }

    #[test]
    fn oco_fill_cancels_sibling_keeps_unrelated() {
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        // A: marketable buy limit in group 1 (fills next bar).
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(1), Price::from_raw(110)).oco(1),
            Timestamp::from_nanos(1),
            &mut s,
        );
        // B: non-marketable sell limit in group 1 (rests, becomes the sibling).
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::limit(I, Side::Sell, Qty::from_raw(1), Price::from_raw(200)).oco(1),
            Timestamp::from_nanos(1),
            &mut s,
        );
        // C: non-marketable buy limit with NO group (must be kept).
        x.submit(
            ClientOrderId::new(2),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(1), Price::from_raw(50)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100, 105, 95, 100, 10), t(), &mut s); // A fills
        // B is cancelled (OcoTriggered), C survives.
        let cancelled: Vec<_> = cancels(&s)
            .into_iter()
            .filter(|(_, r)| *r == CancelReason::OcoTriggered)
            .map(|(id, _)| id)
            .collect();
        assert_eq!(cancelled, vec![ClientOrderId::new(1)]);
        assert_eq!(x.resting_count(), 1); // only C remains
    }

    #[test]
    fn funding_ignored_on_spot_instrument() {
        let mut x =
            SimulatedExchange::with_config(vec![spot()], FillConfig::default()).with_funding(10, 1);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(5)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        x.observe(&bar(100, 100, 100, 100, 1000), t(), &mut s);
        s.clear();
        x.observe(
            &bar(101, 101, 101, 101, 1000),
            Timestamp::from_nanos(3),
            &mut s,
        );
        // No zero-qty funding flow on a spot instrument.
        assert!(
            !s.iter()
                .any(|e| matches!(e, AccountEvent::FundingSettlement { .. }))
        );
    }

    #[test]
    fn funding_waits_for_interval() {
        let mut x =
            SimulatedExchange::with_config(vec![perp()], FillConfig::default()).with_funding(10, 3); // every 3 bars
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Sell, Qty::from_raw(10)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        x.observe(&bar(100, 100, 100, 100, 1000), t(), &mut s); // bars=1
        x.observe(
            &bar(100, 100, 100, 100, 1000),
            Timestamp::from_nanos(3),
            &mut s,
        ); // bars=2
        // fewer than 3 bars elapsed -> no funding yet.
        assert!(
            !s.iter()
                .any(|e| matches!(e, AccountEvent::FundingSettlement { .. }))
        );
    }

    #[test]
    fn funding_and_liquidation_skipped_when_flat() {
        let mut x = SimulatedExchange::with_config(vec![perp()], FillConfig::default())
            .with_funding(10, 1)
            .with_liquidation_bps(1000);
        let mut s = Vec::new();
        // No position -> both accrual paths early-return on net == 0.
        x.observe(&bar(100, 100, 100, 100, 1000), t(), &mut s);
        assert!(fills(&s).is_empty());
        assert!(s.is_empty());
    }

    #[test]
    fn ioc_limit_that_cannot_cross_expires() {
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(1), Price::from_raw(50))
                .with_tif(TimeInForce::Ioc),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100, 105, 95, 100, 10), t(), &mut s); // open 100, limit 50 -> no cross
        assert!(fills(&s).is_empty());
        assert_eq!(expiries(&s), vec![ClientOrderId::new(0)]);
        assert_eq!(x.resting_count(), 0);
    }

    #[test]
    fn fok_that_cannot_fully_fill_is_cancelled() {
        // participation cap of 10% of volume 100 -> 10 < order 100 -> FOK cancels.
        let mut x = SimulatedExchange::new(vec![spot()], 0).with_participation_bps(1000);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(100), Price::from_raw(110))
                .with_tif(TimeInForce::Fok),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100, 105, 95, 100, 100), t(), &mut s); // marketable but capped
        assert!(fills(&s).is_empty());
        assert_eq!(expiries(&s), vec![ClientOrderId::new(0)]);
    }

    #[test]
    fn post_only_sell_cancels_when_marketable() {
        // sell limit 95 vs open 100 -> 95 <= 100 -> marketable -> post-only cancels.
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Sell, Qty::from_raw(1), Price::from_raw(95)).post_only(),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100, 106, 99, 102, 10), t(), &mut s);
        assert!(matches!(s[0], AccountEvent::OrderCanceled { .. }));
        assert!(fills(&s).is_empty());
    }

    #[test]
    fn stop_limit_rests_until_stop_triggers() {
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        // buy stop-limit: arms at 110, limit 111. First bar stays below 110.
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::stop_limit(
                I,
                Side::Buy,
                Qty::from_raw(1),
                Price::from_raw(110),
                Price::from_raw(111),
                TriggerBy::Last,
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100, 105, 99, 102, 10), t(), &mut s); // high 105 < 110 -> not armed -> rest
        assert!(fills(&s).is_empty());
        assert_eq!(x.resting_count(), 1);
        // Second bar crosses the trigger and the limit, so it arms and fills.
        x.observe(
            &bar(108, 112, 108, 111, 10),
            Timestamp::from_nanos(3),
            &mut s,
        );
        assert!(!fills(&s).is_empty());
    }

    #[test]
    fn resting_order_skipped_for_other_instrument_bar() {
        let j = InstrumentId::new(1);
        let mut x = SimulatedExchange::new(vec![spot(), spec_other()], 0);
        let mut s = Vec::new();
        // Rest a non-marketable limit on instrument 0.
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(1), Price::from_raw(50)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        // A bar for instrument 1 must not touch it; it stays resting.
        let other = Event::Bar(Bar::new(
            j,
            Timestamp::from_nanos(2),
            Price::from_raw(100),
            Price::from_raw(100),
            Price::from_raw(100),
            Price::from_raw(100),
            Qty::from_raw(10),
        ));
        x.observe(&other, t(), &mut s);
        assert!(fills(&s).is_empty());
        assert_eq!(x.resting_count(), 1);
    }

    #[test]
    fn ioc_with_no_volume_room_expires() {
        // marketable IOC limit but cap = 0 (zero volume) -> want <= 0 -> expire.
        let mut x = SimulatedExchange::new(vec![spot()], 0).with_participation_bps(1000);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(5), Price::from_raw(110))
                .with_tif(TimeInForce::Ioc),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(100, 105, 95, 100, 0), t(), &mut s); // volume 0 -> cap 0
        assert!(fills(&s).is_empty());
        assert_eq!(expiries(&s), vec![ClientOrderId::new(0)]);
    }

    // --- Cash-balance enforcement (Finding 1) ---------------------------------

    #[test]
    fn cash_disabled_by_default_allows_any_buy() {
        // Without `with_starting_cash`, there is no buying-power check.
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        assert_eq!(x.cash(), None);
        let mut s = Vec::new();
        x.observe(&bar(100, 100, 100, 100, 1000), t(), &mut s); // sets mark
        s.clear();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(1_000_000)),
            t(),
            &mut s,
        );
        assert!(matches!(s[0], AccountEvent::OrderAccepted { .. }));
    }

    #[test]
    fn cash_guard_rejects_unaffordable_market_buy() {
        // Cash 1000, mark 100: buy 5 (cost 500) is fine; buy 20 (cost 2000) isn't.
        let mut x =
            SimulatedExchange::new(vec![spot()], 0).with_starting_cash(Money::from_raw(1000));
        assert_eq!(x.cash(), Some(Money::from_raw(1000)));
        let mut s = Vec::new();
        x.observe(&bar(100, 100, 100, 100, 1000), t(), &mut s); // mark = 100
        s.clear();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(5)),
            t(),
            &mut s,
        );
        assert!(matches!(s[0], AccountEvent::OrderAccepted { .. }));
        s.clear();
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(20)),
            t(),
            &mut s,
        );
        assert!(matches!(
            s[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::InsufficientFunds,
                ..
            }
        ));
    }

    #[test]
    fn cash_guard_uses_limit_price_without_a_mark() {
        // No bar observed yet (mark unset): a limit buy is still checked against
        // its limit price. Limit 100 * qty 20 = 2000 > cash 1000 -> rejected.
        let mut x =
            SimulatedExchange::new(vec![spot()], 0).with_starting_cash(Money::from_raw(1000));
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(20), Price::from_raw(100)),
            t(),
            &mut s,
        );
        assert!(matches!(
            s[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::InsufficientFunds,
                ..
            }
        ));
    }

    #[test]
    fn cash_is_tracked_across_buy_and_sell() {
        // Buy 5 @100 spends 500 (cash 1000 -> 500); sell 5 @100 returns 500.
        let mut x =
            SimulatedExchange::new(vec![spot()], 0).with_starting_cash(Money::from_raw(1000));
        let mut s = Vec::new();
        x.observe(&bar(100, 100, 100, 100, 1000), t(), &mut s); // mark = 100
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(5)),
            t(),
            &mut s,
        );
        x.observe(
            &bar(100, 100, 100, 100, 1000),
            Timestamp::from_nanos(3),
            &mut s,
        ); // fills buy @100
        assert_eq!(x.cash(), Some(Money::from_raw(500)));
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::market(I, Side::Sell, Qty::from_raw(5)),
            Timestamp::from_nanos(3),
            &mut s,
        );
        x.observe(
            &bar(100, 100, 100, 100, 1000),
            Timestamp::from_nanos(5),
            &mut s,
        ); // fills sell @100
        assert_eq!(x.cash(), Some(Money::from_raw(1000)));
    }

    #[test]
    fn cash_guard_floors_a_fee_bleeding_account() {
        // The blowup scenario in miniature: a tiny account whipsawing at 100 bps
        // fees. Once cash can no longer fund a buy, the buy is rejected and the
        // balance never goes negative (Finding 1).
        let mut x = SimulatedExchange::new(vec![spot()], 100) // 1% taker fee
            .with_starting_cash(Money::from_raw(1000));
        let mut s = Vec::new();
        for t_ns in 1..200i64 {
            x.observe(
                &bar(100, 100, 100, 100, 10_000),
                Timestamp::from_nanos(t_ns),
                &mut s,
            );
            // Always try to buy the max we could ever afford; the guard rejects
            // once cash is exhausted.
            x.submit(
                ClientOrderId::new(t_ns as u64),
                OrderRequest::market(I, Side::Buy, Qty::from_raw(9)),
                Timestamp::from_nanos(t_ns),
                &mut s,
            );
        }
        // Cash is enforced and never negative.
        let cash = x.cash().expect("enforced").raw();
        assert!(cash >= 0, "cash went negative: {cash}");
    }

    #[test]
    fn cash_guard_skips_market_buy_without_a_mark() {
        // Cash enforcement ON but no bar observed yet -> no mark to price a market
        // buy, so the check is skipped and the order is accepted (can't estimate).
        let mut x = SimulatedExchange::new(vec![spot()], 0).with_starting_cash(Money::from_raw(1));
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(1_000_000)),
            t(),
            &mut s,
        );
        assert!(matches!(s[0], AccountEvent::OrderAccepted { .. }));
    }

    #[test]
    fn stop_fills_at_gapped_open_not_trigger() {
        // Sell stop at 100; the bar GAPS down, opening at 80. A real stop cannot
        // fill at 100 — it fills at the (worse) open, 80.
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::stop(
                I,
                Side::Sell,
                Qty::from_raw(1),
                Price::from_raw(100),
                TriggerBy::Last,
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(80, 85, 78, 82, 10), t(), &mut s); // open 80 < trigger 100
        assert_eq!(fills(&s)[0].0, Price::from_raw(80));

        // Buy stop at 100; the bar gaps up, opening at 120 -> fills at 120.
        let mut x2 = SimulatedExchange::new(vec![spot()], 0);
        let mut s2 = Vec::new();
        x2.submit(
            ClientOrderId::new(1),
            OrderRequest::stop(
                I,
                Side::Buy,
                Qty::from_raw(1),
                Price::from_raw(100),
                TriggerBy::Last,
            ),
            Timestamp::from_nanos(1),
            &mut s2,
        );
        s2.clear();
        x2.observe(&bar(120, 125, 119, 122, 10), t(), &mut s2);
        assert_eq!(fills(&s2)[0].0, Price::from_raw(120));
    }

    #[test]
    fn funding_supports_negative_rate_backwardation() {
        // A negative funding rate (backwardation): a LONG receives funding, so the
        // emitted cost amount is negative.
        let mut x = SimulatedExchange::with_config(vec![perp()], FillConfig::default())
            .with_funding(-10, 1);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(10)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        x.observe(&bar(100, 100, 100, 100, 1000), t(), &mut s); // long 10 @ 100
        s.clear();
        x.observe(
            &bar(100, 100, 100, 100, 1000),
            Timestamp::from_nanos(3),
            &mut s,
        );
        let funding: Vec<_> = s
            .iter()
            .filter_map(|e| match e {
                AccountEvent::FundingSettlement { cost, .. } => Some(*cost),
                _ => None,
            })
            .collect();
        assert_eq!(funding.len(), 1);
        assert!(funding[0].amount.raw() < 0); // long receives under backwardation
    }

    #[test]
    fn cash_reservation_blocks_second_in_bar_buy() {
        // Cash 1000, mark 100. A first buy of 6 (cost 600) rests; a second buy of
        // 6 must be rejected because only 400 of unreserved cash remains — without
        // reservation it would wrongly pass (1000 >= 600).
        let mut x =
            SimulatedExchange::new(vec![spot()], 0).with_starting_cash(Money::from_raw(1000));
        let mut s = Vec::new();
        x.observe(&bar(100, 100, 100, 100, 1000), t(), &mut s); // mark = 100
        s.clear();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(6)),
            t(),
            &mut s,
        );
        assert!(matches!(s[0], AccountEvent::OrderAccepted { .. }));
        s.clear();
        x.submit(
            ClientOrderId::new(1),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(6)),
            t(),
            &mut s,
        );
        assert!(matches!(
            s[0],
            AccountEvent::OrderRejected {
                reason: RejectReason::InsufficientFunds,
                ..
            }
        ));
    }

    #[test]
    fn cancel_removes_resting_order_and_acks() {
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::limit(I, Side::Buy, Qty::from_raw(1), Price::from_raw(50)),
            t(),
            &mut s,
        ); // non-marketable -> rests
        assert_eq!(x.resting_count(), 1);
        s.clear();
        x.cancel(ClientOrderId::new(0), t(), &mut s);
        assert_eq!(x.resting_count(), 0);
        assert_eq!(
            cancels(&s),
            vec![(ClientOrderId::new(0), CancelReason::Requested)]
        );
    }

    #[test]
    fn cancel_unknown_id_is_rejected() {
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.cancel(ClientOrderId::new(99), t(), &mut s);
        // An unknown id surfaces OrderCancelRejected (not a silent no-op) so a
        // strategy polling for the ack stops waiting.
        assert!(matches!(
            s[0],
            AccountEvent::OrderCancelRejected {
                reason: CancelRejectReason::UnknownOrder,
                ..
            }
        ));
    }

    #[test]
    fn stop_limit_arming_emits_order_triggered() {
        // Buy stop-limit: arms at 110, then works as a limit at 90. A bar that
        // crosses 110 (arms) but stays above 90 (no fill) yields OrderTriggered.
        let mut x = SimulatedExchange::new(vec![spot()], 0);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::stop_limit(
                I,
                Side::Buy,
                Qty::from_raw(1),
                Price::from_raw(110),
                Price::from_raw(90),
                TriggerBy::Last,
            ),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        x.observe(&bar(112, 115, 100, 112, 10), t(), &mut s); // arms (115>=110), no fill (100>90)
        assert!(s.iter().any(|e| matches!(e,
            AccountEvent::OrderTriggered { id, .. } if *id == ClientOrderId::new(0))));
        assert!(fills(&s).is_empty());
        assert_eq!(x.resting_count(), 1); // now working as a resting limit
    }

    #[test]
    fn partial_fills_flag_completion() {
        // 10%-of-volume cap: a 25-lot market buy on 100-volume bars fills 10, 10, 5
        // — only the last fill completes the order.
        let mut x = SimulatedExchange::new(vec![spot()], 0).with_participation_bps(1000);
        let mut s = Vec::new();
        x.submit(
            ClientOrderId::new(0),
            OrderRequest::market(I, Side::Buy, Qty::from_raw(25)),
            Timestamp::from_nanos(1),
            &mut s,
        );
        s.clear();
        for t_ns in 2..=4 {
            x.observe(
                &bar(100, 100, 100, 100, 100),
                Timestamp::from_nanos(t_ns),
                &mut s,
            );
        }
        let completes: Vec<bool> = s
            .iter()
            .filter_map(|e| match e {
                AccountEvent::Fill { complete, .. } => Some(*complete),
                _ => None,
            })
            .collect();
        assert_eq!(completes, vec![false, false, true]);
    }
}
