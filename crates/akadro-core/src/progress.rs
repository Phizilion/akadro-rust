// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Venue-neutral progress reporting for multi-page historical back-fills.
//!
//! A connector that pages a venue's kline/candle history (a long `with_range`
//! download) can emit one [`BackfillProgress`] after each page through an opt-in
//! callback, so a *consumer* (a CLI, the playground) can render a download bar. The
//! reporting vocabulary lives here in `akadro-core` so the renderer is
//! venue-agnostic and **no UI dependency leaks into the library** — the connector
//! emits plain numbers; the consumer owns the terminal I/O.
//!
//! Total page count is not known ahead of time (venues cap pages, return short
//! pages, and skip retention gaps), so this reports the *running* counts; a consumer
//! that knows the requested range/interval can estimate a total and draw a bar, or
//! just show a live spinner with the counts.

/// A snapshot of a connector back-fill's progress, emitted after each fetched page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackfillProgress {
    /// Pages fetched so far (`>= 1` once the first page lands).
    pub pages: u32,
    /// Bars accumulated so far across all pages.
    pub bars: usize,
    /// The cursor frontier in epoch-**milliseconds** — how far the back-fill has
    /// reached so far (the latest close for a forward-paging venue, the oldest open
    /// for a backward-paging one). Lets a consumer show "reached &lt;date&gt;".
    pub frontier_ms: i64,
}

impl BackfillProgress {
    /// Construct a progress snapshot.
    #[must_use]
    pub const fn new(pages: u32, bars: usize, frontier_ms: i64) -> Self {
        BackfillProgress {
            pages,
            bars,
            frontier_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_round_trip() {
        let p = BackfillProgress::new(3, 1_500, 1_700_000_000_000);
        assert_eq!(p.pages, 3);
        assert_eq!(p.bars, 1_500);
        assert_eq!(p.frontier_ms, 1_700_000_000_000);
        // Copy + Eq for cheap snapshotting in a renderer.
        let q = p;
        assert_eq!(p, q);
    }
}
