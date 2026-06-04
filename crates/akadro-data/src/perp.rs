// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One-call perpetual data loading: bars **and** funding, both cached.
//!
//! A perpetual backtest needs two data series that are *equally* important — the
//! OHLCV bars and the funding-rate history (a real recurring cost). Asking the user
//! to fetch and cache them separately (and remember to apply funding) is exactly the
//! kind of plumbing chore akadro takes off their hands: [`load_or_cache_perp`]
//! downloads-and-caches both with one call, serves both from cache on re-runs, and
//! **fails fast if a perpetual has no funding data** — because a perp backtest
//! without funding is silently wrong, not merely incomplete.

use std::path::Path;

use akadro_core::{DataSource, InstrumentId, Timestamp};

use crate::bars::{CachedFeed, DataError, load_or_cache_feed};
use crate::funding::load_or_cache_funding;

/// Cached perpetual data: a ready bar [`DataSource`] plus its funding schedule.
///
/// `funding` is **guaranteed non-empty** (a perpetual with no funding history is
/// rejected by [`load_or_cache_perp`]), so a consumer can apply it to
/// `SimulatedExchange::with_funding_schedule` unconditionally.
#[derive(Debug)]
#[non_exhaustive]
pub struct CachedPerp {
    /// Bars as a ready engine feed (cache-or-download).
    pub feed: CachedFeed,
    /// Funding settlements `(timestamp, raw_rate at FUNDING_RATE_SCALE)`, ascending,
    /// non-empty.
    pub funding: Vec<(Timestamp, i64)>,
}

/// Load (or download-and-cache) **both** the bars and the funding history for one
/// perpetual instrument — the blessed perp data path.
///
/// Bars are cached at `bars_path` (via [`load_or_cache_feed`]) and funding at
/// `funding_path` (via [`load_or_cache_funding`]); both serve from cache on re-runs.
/// `make_bar_source` builds the venue bar feed (e.g. an `OkxCandleFeed` with a
/// range); `fetch_funding` performs the venue funding fetch (e.g.
/// `okx::fetch_funding_history_paged`, mapping its error into [`DataError`]).
///
/// # Errors
/// - [`DataError::Schema`] if the venue returns **no funding history** for the
///   instrument — a perpetual backtest cannot run without funding, so this is a hard
///   error rather than a silently-zero-funding run.
/// - Any read/write/fetch [`DataError`] from either underlying loader.
pub fn load_or_cache_perp<D, BF, FF>(
    bars_path: &Path,
    funding_path: &Path,
    instrument: InstrumentId,
    price_scale: u32,
    qty_scale: u32,
    make_bar_source: BF,
    fetch_funding: FF,
) -> Result<CachedPerp, DataError>
where
    D: DataSource,
    BF: FnOnce() -> D,
    FF: FnOnce() -> Result<Vec<(Timestamp, i64)>, DataError>,
{
    let feed = load_or_cache_feed(
        bars_path,
        instrument,
        price_scale,
        qty_scale,
        make_bar_source,
    )?;
    let funding = load_or_cache_funding(funding_path, instrument, fetch_funding)?;
    if funding.is_empty() {
        return Err(DataError::Schema(format!(
            "perpetual instrument {} returned no funding-rate history — a perpetual \
             backtest cannot run without funding (it is a real recurring cost). Check the \
             symbol / venue / date range, or use a spot instrument.",
            instrument.index()
        )));
    }
    Ok(CachedPerp { feed, funding })
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::Event;

    struct VecSrc(std::vec::IntoIter<Event>);
    impl DataSource for VecSrc {
        fn next_event(&mut self) -> Option<Event> {
            self.0.next()
        }
    }
    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "akadro_perp_test_{name}_{}.feather",
            std::process::id()
        ))
    }
    fn bar(ts: i64) -> akadro_core::Bar {
        let p = akadro_core::Price::from_raw(1);
        akadro_core::Bar::new(
            InstrumentId::new(0),
            Timestamp::from_nanos(ts),
            p,
            p,
            p,
            p,
            akadro_core::Qty::from_raw(1),
        )
    }

    #[test]
    fn loads_bars_and_funding_together() {
        let (bp, fp) = (tmp("ok_bars"), tmp("ok_fund"));
        let _ = std::fs::remove_file(&bp);
        let _ = std::fs::remove_file(&fp);
        let perp = load_or_cache_perp(
            &bp,
            &fp,
            InstrumentId::new(0),
            2,
            2,
            || VecSrc(vec![Event::Bar(bar(1_000_000_000))].into_iter()),
            || Ok(vec![(Timestamp::from_nanos(1_000), 10_000)]),
        )
        .unwrap();
        assert_eq!(perp.funding.len(), 1);
        let mut feed = perp.feed;
        assert!(matches!(feed.next_event(), Some(Event::Bar(_))));
        let _ = std::fs::remove_file(&bp);
        let _ = std::fs::remove_file(&fp);
    }

    #[test]
    fn errors_when_perp_has_no_funding() {
        let (bp, fp) = (tmp("nofund_bars"), tmp("nofund_fund"));
        let _ = std::fs::remove_file(&bp);
        let _ = std::fs::remove_file(&fp);
        let r = load_or_cache_perp(
            &bp,
            &fp,
            InstrumentId::new(0),
            2,
            2,
            || VecSrc(vec![Event::Bar(bar(1_000_000_000))].into_iter()),
            || Ok(Vec::new()), // venue returned no funding
        );
        assert!(r.is_err(), "a perp with no funding must be a hard error");
        let _ = std::fs::remove_file(&bp);
        let _ = std::fs::remove_file(&fp);
    }
}
