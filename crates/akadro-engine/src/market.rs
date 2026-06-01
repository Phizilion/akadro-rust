// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Observed market state — the first layer of the kill feature.
//!
//! The engine accumulates the bars it has *already seen* into columnar
//! (struct-of-arrays) storage, one column per OHLCV field per instrument. This
//! layout is cache-friendly for indicator computation and lets us hand out a
//! contiguous [`Series`] slice cheaply.
//!
//! Crucially, the future never enters this structure: the engine pulls one event
//! at a time from the data source and appends it here *before* invoking the
//! strategy, so a [`MarketView`] over this state can only ever reach the past
//! and present. The data source (which holds/produces the future) is never
//! exposed to a strategy.

use std::collections::HashMap;

use akadro_core::{Bar, InstrumentId, Price, Qty, Timestamp};

use crate::brand::Brand;
use crate::series::Series;
use core::marker::PhantomData;

/// Columnar OHLCV history for a single instrument (struct-of-arrays).
#[derive(Debug, Default)]
struct InstrumentSeries {
    ts: Vec<Timestamp>,
    open: Vec<Price>,
    high: Vec<Price>,
    low: Vec<Price>,
    close: Vec<Price>,
    volume: Vec<Qty>,
}

impl InstrumentSeries {
    fn push(&mut self, bar: &Bar) {
        self.ts.push(bar.ts);
        self.open.push(bar.open);
        self.high.push(bar.high);
        self.low.push(bar.low);
        self.close.push(bar.close);
        self.volume.push(bar.volume);
    }
}

/// All observed market data for a run, indexed densely by instrument.
#[derive(Debug)]
pub(crate) struct Market {
    series: Vec<InstrumentSeries>,
    /// Backward-only auxiliary scalar series keyed by `(instrument index,
    /// channel)` — non-OHLCV signals (open interest, long/short ratio, market
    /// liquidations, external scalars). Appended *before* the strategy runs, so a
    /// [`MarketView::signal`] read is look-ahead-safe exactly like an OHLCV series.
    signals: HashMap<(u32, u16), Vec<i64>>,
}

impl Market {
    /// Create storage for `n` densely-numbered instruments.
    pub(crate) fn with_instruments(n: usize) -> Self {
        let mut series = Vec::with_capacity(n);
        series.resize_with(n, InstrumentSeries::default);
        Market {
            series,
            signals: HashMap::new(),
        }
    }

    /// Append a freshly-observed auxiliary signal value for `(instrument,
    /// channel)`. Like [`Market::push`], the engine calls this *before* invoking
    /// the strategy, keeping the signal series strictly backward-only.
    pub(crate) fn push_signal(&mut self, instrument: InstrumentId, channel: u16, value: i64) {
        self.signals
            .entry((instrument.index(), channel))
            .or_default()
            .push(value);
    }

    /// Append a freshly-observed bar. Unknown instruments are ignored (the
    /// engine validates the catalogue up front, so this only guards against a
    /// misbehaving data source rather than panicking mid-run).
    pub(crate) fn push(&mut self, bar: &Bar) {
        // `get_mut` safely ignores an unregistered instrument (the engine
        // validates the catalogue up front, so this only guards a misbehaving
        // data source rather than panicking mid-run).
        if let Some(s) = self.series.get_mut(bar.instrument.index() as usize) {
            s.push(bar);
        }
    }

    fn get(&self, instrument: InstrumentId) -> Option<&InstrumentSeries> {
        self.series.get(instrument.index() as usize)
    }
}

/// A call-scoped, look-ahead-safe view of observed market data.
///
/// Branded with `'bar`: it cannot be stored past the handler call it was created
/// for. Every accessor returns a backward-only [`Series`] (or `None` for an
/// unknown instrument).
pub(crate) struct MarketView<'bar> {
    market: &'bar Market,
    _brand: Brand<'bar>,
}

impl<'bar> MarketView<'bar> {
    pub(crate) fn new(market: &'bar Market) -> Self {
        MarketView {
            market,
            _brand: PhantomData,
        }
    }

    /// Close-price series for `instrument`.
    pub(crate) fn closes(&self, instrument: InstrumentId) -> Option<Series<'bar, Price>> {
        self.market.get(instrument).map(|s| Series::new(&s.close))
    }

    /// Open-price series.
    pub(crate) fn opens(&self, instrument: InstrumentId) -> Option<Series<'bar, Price>> {
        self.market.get(instrument).map(|s| Series::new(&s.open))
    }

    /// High-price series.
    pub(crate) fn highs(&self, instrument: InstrumentId) -> Option<Series<'bar, Price>> {
        self.market.get(instrument).map(|s| Series::new(&s.high))
    }

    /// Low-price series.
    pub(crate) fn lows(&self, instrument: InstrumentId) -> Option<Series<'bar, Price>> {
        self.market.get(instrument).map(|s| Series::new(&s.low))
    }

    /// Volume series.
    pub(crate) fn volumes(&self, instrument: InstrumentId) -> Option<Series<'bar, Qty>> {
        self.market.get(instrument).map(|s| Series::new(&s.volume))
    }

    /// Number of bars observed so far for `instrument`.
    pub(crate) fn bar_count(&self, instrument: InstrumentId) -> usize {
        self.market.get(instrument).map_or(0, |s| s.close.len())
    }

    /// Backward-only auxiliary signal series for `(instrument, channel)`, or
    /// `None` if none has been observed. The values are the producing feed's
    /// fixed-point `i64`s (per-channel scale).
    pub(crate) fn signal(
        &self,
        instrument: InstrumentId,
        channel: u16,
    ) -> Option<Series<'bar, i64>> {
        self.market
            .signals
            .get(&(instrument.index(), channel))
            .map(|v| Series::new(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(inst: u32, close: i64) -> Bar {
        Bar::new(
            InstrumentId::new(inst),
            Timestamp::from_nanos(close),
            Price::from_raw(close),
            Price::from_raw(close + 1),
            Price::from_raw(close - 1),
            Price::from_raw(close),
            Qty::from_raw(100),
        )
    }

    #[test]
    fn push_and_view() {
        let mut m = Market::with_instruments(2);
        m.push(&bar(0, 10));
        m.push(&bar(0, 20));
        m.push(&bar(1, 99));
        let v = MarketView::new(&m);
        assert_eq!(v.bar_count(InstrumentId::new(0)), 2);
        assert_eq!(v.bar_count(InstrumentId::new(1)), 1);
        assert_eq!(
            v.closes(InstrumentId::new(0)).unwrap().latest(),
            Some(Price::from_raw(20))
        );
        assert_eq!(
            v.closes(InstrumentId::new(0)).unwrap().ago(1),
            Some(Price::from_raw(10))
        );
        assert_eq!(
            v.opens(InstrumentId::new(0)).unwrap().latest(),
            Some(Price::from_raw(20))
        );
        assert_eq!(
            v.highs(InstrumentId::new(0)).unwrap().latest(),
            Some(Price::from_raw(21))
        );
        assert_eq!(
            v.lows(InstrumentId::new(0)).unwrap().latest(),
            Some(Price::from_raw(19))
        );
        assert_eq!(
            v.volumes(InstrumentId::new(0)).unwrap().latest(),
            Some(Qty::from_raw(100))
        );
    }

    #[test]
    fn unknown_instrument_is_none_not_panic() {
        let m = Market::with_instruments(1);
        let v = MarketView::new(&m);
        assert_eq!(v.bar_count(InstrumentId::new(5)), 0);
        assert!(v.closes(InstrumentId::new(5)).is_none());
        assert!(v.opens(InstrumentId::new(5)).is_none());
        assert!(v.highs(InstrumentId::new(5)).is_none());
        assert!(v.lows(InstrumentId::new(5)).is_none());
        assert!(v.volumes(InstrumentId::new(5)).is_none());
    }

    #[test]
    fn signal_series_is_backward_only_per_channel() {
        let mut m = Market::with_instruments(2);
        let i0 = InstrumentId::new(0);
        m.push_signal(i0, 7, 100);
        m.push_signal(i0, 7, 200);
        m.push_signal(i0, 9, 5); // a different channel for the same instrument
        let v = MarketView::new(&m);
        let s = v.signal(i0, 7).unwrap();
        assert_eq!(s.latest(), Some(200));
        assert_eq!(s.ago(1), Some(100));
        assert_eq!(s.len(), 2);
        assert_eq!(v.signal(i0, 9).unwrap().latest(), Some(5)); // channels independent
        assert!(v.signal(i0, 99).is_none()); // unobserved channel
        assert!(v.signal(InstrumentId::new(1), 7).is_none()); // no signal for inst 1
        assert!(v.signal(InstrumentId::new(9), 7).is_none()); // out of range
    }

    #[test]
    fn push_unknown_instrument_ignored() {
        let mut m = Market::with_instruments(1);
        // index 9 is out of range; it is silently skipped (no panic).
        m.push(&bar(9, 1));
        let v = MarketView::new(&m);
        assert_eq!(v.bar_count(InstrumentId::new(0)), 0);
        assert_eq!(v.bar_count(InstrumentId::new(9)), 0);
    }
}
