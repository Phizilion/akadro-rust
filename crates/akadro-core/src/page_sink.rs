// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Venue-neutral **per-page data sink** for incremental cache persistence.
//!
//! As a connector pages a venue's kline/candle history (a long `with_range`
//! back-fill), it can hand each freshly-fetched page of [`Bar`]s to an opt-in
//! [`PageSink`] so a *consumer* — the cache's incremental writer — persists progress
//! **as it arrives** instead of only after the whole download completes. An
//! interrupt then preserves the pages already flushed, and the next run resumes from
//! the gap instead of re-downloading.
//!
//! This mirrors the [`BackfillProgress`](crate::BackfillProgress) callback seam, but
//! the sink receives the bars themselves rather than a progress snapshot. The
//! vocabulary lives here in `akadro-core` so every connector can reference it without
//! depending on the cache layer (avoiding a crate cycle) and **no storage concern
//! leaks into a venue crate** — the connector emits plain bars; the consumer owns the
//! disk I/O.
//!
//! **Error handling is intentionally out-of-band.** [`on_page`](PageSink::on_page)
//! returns `()`, exactly like the progress callback: an incremental-write hiccup must
//! not abort an in-flight download. A well-behaved sink keeps the un-written bars
//! buffered for the next flush and surfaces any real failure *authoritatively* when
//! it is finalized (where the compact, durable file is written). So a transient flush
//! error degrades to "no resume point this run," never "the download failed."

use crate::Bar;

/// Receives each downloaded page of bars during a connector back-fill.
///
/// Registered on a feed via its `with_page_sink` builder (the analog of
/// `with_progress`). See the module-level docs for the error-handling contract.
pub trait PageSink {
    /// Receive one freshly-fetched page of bars, in ascending close-stamp order.
    ///
    /// Called once per page, *before* the connector folds the page into its own
    /// in-memory buffer. Must not panic on an I/O problem — buffer for retry and
    /// surface at finalize instead (see the module-level docs).
    fn on_page(&mut self, page: &[Bar]);

    /// Signal that the connector **aborted its back-fill early** because a page
    /// fetch/parse failed — i.e. the download is *truncated*, not genuinely
    /// complete. Called (with the venue error rendered to a string) once, when the
    /// connector gives up on the remaining pages, regardless of whether it discards
    /// or keeps the pages already buffered.
    ///
    /// This is the connector→consumer direction of the out-of-band error contract
    /// (the module-level docs cover the consumer→connector flush-hiccup direction).
    /// It lets the consumer distinguish "the venue has no more data here" (a clean,
    /// short result the connector simply ran out of) from "the fetch errored partway"
    /// — so the cache loader can refuse to record the unfetched remainder as
    /// verified-empty (which would be silent data loss) and instead surface the
    /// failure / leave the gap to be retried. Default: no-op (a sink that doesn't
    /// care, e.g. a progress logger, ignores it).
    fn on_error(&mut self, _error: &str) {}
}
