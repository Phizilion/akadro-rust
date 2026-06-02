// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # akadro-data
//!
//! A columnar on-disk cache for downloaded market data. Bars are stored as
//! Apache Arrow IPC / Feather files — a struct-of-arrays of `Int64` columns
//! (`ts, open, high, low, close, volume`) preserving akadro's fixed-point
//! integers losslessly, with the instrument and `(price_scale, qty_scale)`
//! recorded in the file metadata so each file is self-describing.
//!
//! * [`write_partition`] / [`read_partition`] persist and load a partition.
//! * [`FeatherBarSource`] is a [`DataSource`](akadro_core::DataSource) that
//!   replays a cached partition into the engine.
//! * [`Manifest`] tracks which time ranges are already cached and
//!   [`Manifest::plan_fetch`] computes the next incremental download window, so a
//!   downloader (e.g. `akadro-venue-mexc`'s `MexcKlineFeed`) only fetches new bars.
//!
//! v1 loads a partition by reading the file; memory-mapped zero-copy streaming
//! (the `memmap2` path) is a documented future optimization and is why this
//! crate would be the sole place `unsafe` is permitted.

mod bars;
mod manifest;
mod merge;
mod signal;
mod trades;

pub use bars::{
    BarSpec, CachedFeed, DataError, FeatherBarSource, LoadedBars, cache_from_source, load_or_cache,
    load_or_cache_feed, load_or_cache_many, load_or_cache_many_feed, read_partition,
    write_partition,
};
pub use manifest::{CacheEntry, Manifest};
pub use merge::MergeSource;
pub use signal::SignalSource;
pub use trades::{bars_from_trades, interval_to_nanos};
