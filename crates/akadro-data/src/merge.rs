// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Merging several [`DataSource`]s into one non-decreasing event stream — the
//! sanctioned way to feed a strategy *price bars plus auxiliary signals* (open
//! interest, long/short ratio, liquidations, …) through the engine's single
//! [`DataSource`] slot.
//!
//! The engine consumes exactly one source and relies on it being in
//! non-decreasing timestamp order (the look-ahead contract). [`MergeSource`] is a
//! straight k-way merge: it buffers one "head" event per child and always emits
//! the earliest, refilling that child. Ties (equal timestamps across children)
//! break by child index — **lower index first, deterministically** — so the same
//! set of feeds always yields the same interleaving (parity / reproducibility).
//!
//! It is O(k) per event for `k` children, which is ample for the handful of feeds
//! a strategy combines; the loser-tree upgrade (sub-linear per event) is reserved
//! for the data layer's hot path and is unnecessary here.

use akadro_core::{DataSource, Event};

/// A [`DataSource`] that merges its children into one timestamp-ordered stream.
///
/// Each child must itself be non-decreasing in time; the merge then preserves
/// that property across all of them. Construct with [`MergeSource::new`].
pub struct MergeSource {
    sources: Vec<Box<dyn DataSource>>,
    heads: Vec<Option<Event>>,
    primed: bool,
}

impl MergeSource {
    /// Merge `sources`. Child order matters only for tie-breaking: when two
    /// children offer events with the **same** timestamp, the one earlier in this
    /// `Vec` is emitted first (so put the price feed before signal feeds if you
    /// want a bar observed before same-stamped signals, or vice versa).
    #[must_use]
    pub fn new(sources: Vec<Box<dyn DataSource>>) -> Self {
        let heads = sources.iter().map(|_| None).collect();
        MergeSource {
            sources,
            heads,
            primed: false,
        }
    }

    fn prime(&mut self) {
        for (i, src) in self.sources.iter_mut().enumerate() {
            self.heads[i] = src.next_event();
        }
        self.primed = true;
    }
}

impl DataSource for MergeSource {
    fn next_event(&mut self) -> Option<Event> {
        if !self.primed {
            self.prime();
        }
        // Pick the earliest buffered head; lowest child index wins a tie.
        let mut best: Option<(usize, i64)> = None;
        for (i, head) in self.heads.iter().enumerate() {
            if let Some(ev) = head {
                let ts = ev.ts().as_nanos();
                if best.is_none_or(|(_, best_ts)| ts < best_ts) {
                    best = Some((i, ts));
                }
            }
        }
        let (chosen, _) = best?;
        let event = self.heads[chosen].take();
        // Refill the child we just drained.
        self.heads[chosen] = self.sources[chosen].next_event();
        event
    }
}

impl core::fmt::Debug for MergeSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MergeSource")
            .field("children", &self.sources.len())
            .field(
                "buffered",
                &self.heads.iter().filter(|h| h.is_some()).count(),
            )
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SignalSource;
    use akadro_core::{InstrumentId, Timestamp};

    fn sig(channel: u16, points: &[(i64, i64)]) -> Box<dyn DataSource> {
        Box::new(SignalSource::new(
            InstrumentId::new(0),
            channel,
            points
                .iter()
                .map(|&(t, v)| (Timestamp::from_nanos(t), v))
                .collect(),
        ))
    }

    fn drain(mut s: MergeSource) -> Vec<(i64, u16, i64)> {
        let mut out = Vec::new();
        while let Some(ev) = s.next_event() {
            if let Event::Signal {
                channel, value, ts, ..
            } = ev
            {
                out.push((ts.as_nanos(), channel, value));
            }
        }
        out
    }

    #[test]
    fn merges_two_sources_in_time_order() {
        let a = sig(1, &[(10, 100), (30, 300)]);
        let b = sig(2, &[(20, 200), (40, 400)]);
        let out = drain(MergeSource::new(vec![a, b]));
        assert_eq!(
            out,
            vec![(10, 1, 100), (20, 2, 200), (30, 1, 300), (40, 2, 400)]
        );
    }

    #[test]
    fn ties_break_by_child_index_deterministically() {
        // Both children emit at ts=10; the lower-index child (channel 1) wins.
        let a = sig(1, &[(10, 1)]);
        let b = sig(2, &[(10, 2)]);
        assert_eq!(
            drain(MergeSource::new(vec![a, b])),
            vec![(10, 1, 1), (10, 2, 2)]
        );
        // Swapped order → swapped tie-break, still deterministic.
        let a = sig(1, &[(10, 1)]);
        let b = sig(2, &[(10, 2)]);
        assert_eq!(
            drain(MergeSource::new(vec![b, a])),
            vec![(10, 2, 2), (10, 1, 1)]
        );
    }

    #[test]
    fn debug_reports_children_and_buffered() {
        let mut s = MergeSource::new(vec![sig(1, &[(1, 1), (2, 2)]), sig(2, &[(3, 3)])]);
        // Before priming.
        assert!(format!("{s:?}").contains("MergeSource"));
        // After one pull both children are primed → two buffered heads.
        let _ = s.next_event();
        let dbg = format!("{s:?}");
        assert!(dbg.contains("children: 2"));
        assert!(dbg.contains("buffered: 2"));
    }

    #[test]
    fn handles_empty_and_uneven_children() {
        let empty = sig(1, &[]);
        let one = sig(2, &[(5, 50)]);
        let long = sig(3, &[(1, 10), (9, 90)]);
        let out = drain(MergeSource::new(vec![empty, one, long]));
        assert_eq!(out, vec![(1, 3, 10), (5, 2, 50), (9, 3, 90)]);
        // No children at all → immediately exhausted.
        assert!(MergeSource::new(vec![]).next_event().is_none());
    }
}
