// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Merging several [`DataSource`]s into one non-decreasing event stream — the
//! sanctioned way to feed a strategy *price bars plus auxiliary signals* (open
//! interest, long/short ratio, liquidations, …) through the engine's single
//! [`DataSource`] slot.
//!
//! The engine consumes exactly one source and relies on it being in
//! non-decreasing timestamp order (the look-ahead contract). [`MergeSource`]
//! buffers one "head" event per child and always emits the earliest, refilling
//! that child. Ties (equal timestamps across children) break by child index —
//! **lower index first, deterministically** — so the same set of feeds always
//! yields the same interleaving (parity / reproducibility).
//!
//! The winner is selected with a **tournament tree** ([`TournamentTree`]): each
//! refill restores the tree in `O(log k)` comparisons rather than the `O(k)` of a
//! linear scan over the heads — the sub-linear k-way merge of decision D15. (A
//! loser tree is the dual structure with the same complexity; the winner-tree
//! variant is used here for clarity, and is verified bit-identical to the naive
//! linear merge by a fuzz test — the tie-break and ordering are unchanged.) The
//! `O(log k)` win is established by `benches/merge.rs`.

use akadro_core::{DataSource, Event};

/// A [`DataSource`] that merges its children into one timestamp-ordered stream.
///
/// Each child must itself be non-decreasing in time; the merge then preserves
/// that property across all of them. Construct with [`MergeSource::new`].
pub struct MergeSource {
    sources: Vec<Box<dyn DataSource>>,
    heads: Vec<Option<Event>>,
    /// Tournament tree over the buffered heads' timestamps; selects the next winner
    /// in `O(log k)`. Built lazily on the first pull (`None` until primed).
    tree: Option<TournamentTree>,
    primed: bool,
    /// Timestamp of the last event emitted, to assert the merged stream stays
    /// non-decreasing (a child that violates its own contract is caught in debug).
    last_ts: i64,
}

/// A tournament (winner) tree over `n` slots keyed by `Option<i64>` timestamps, used
/// to merge `n` sorted streams in `O(log n)` per element instead of an `O(n)` linear
/// scan. The winner is the slot with the smallest key; **ties break by lower slot
/// index** and an exhausted slot (`None`) sorts as `+∞` — exactly the rule the linear
/// merge used, so the merged stream is bit-identical (verified by the fuzz test).
///
/// Leaves are padded to a power of two so the tree is complete; padding slots
/// (`>= n`) are permanently exhausted and always lose. After one slot's key changes,
/// [`Self::set`] recomputes only the `O(log n)` ancestors on its root path.
struct TournamentTree {
    /// Padded leaf count (power of two `>= n.max(1)`).
    size: usize,
    /// Current key per slot, length `size`; slots `>= n` are always `None`.
    key: Vec<Option<i64>>,
    /// Heap-layout winner tree, length `2 * size`. `tree[1]` is the overall winner
    /// slot; leaf for slot `s` is `tree[size + s]`.
    tree: Vec<usize>,
}

impl TournamentTree {
    /// Build the tree from one initial key per real slot.
    fn new(keys: &[Option<i64>]) -> Self {
        let n = keys.len();
        let size = n.max(1).next_power_of_two();
        let mut key = vec![None; size];
        key[..n].copy_from_slice(keys);
        let mut t = TournamentTree {
            size,
            key,
            tree: vec![0; 2 * size],
        };
        for s in 0..size {
            t.tree[size + s] = s; // each leaf points at its own slot
        }
        for i in (1..size).rev() {
            t.tree[i] = t.better(t.tree[2 * i], t.tree[2 * i + 1]);
        }
        t
    }

    /// Of two distinct slots, the winner: smaller `(ts, slot)`; `None` (exhausted)
    /// always loses; if both are exhausted, the lower slot index (irrelevant to
    /// output — it carries no event).
    fn better(&self, a: usize, b: usize) -> usize {
        match (self.key[a], self.key[b]) {
            (None, None) => a.min(b),
            (None, Some(_)) => b,
            (Some(_), None) => a,
            (Some(ka), Some(kb)) => {
                if (ka, a) <= (kb, b) {
                    a
                } else {
                    b
                }
            }
        }
    }

    /// The slot currently holding the minimum key (the next to emit).
    fn winner(&self) -> usize {
        self.tree[1]
    }

    /// Set slot `s`'s key and restore the tree along its root path (`O(log n)`).
    fn set(&mut self, s: usize, k: Option<i64>) {
        self.key[s] = k;
        let mut i = usize::midpoint(self.size, s);
        while i >= 1 {
            self.tree[i] = self.better(self.tree[2 * i], self.tree[2 * i + 1]);
            i /= 2;
        }
    }
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
            tree: None,
            primed: false,
            last_ts: i64::MIN,
        }
    }

    fn prime(&mut self) {
        for (i, src) in self.sources.iter_mut().enumerate() {
            self.heads[i] = src.next_event();
        }
        let keys: Vec<Option<i64>> = self
            .heads
            .iter()
            .map(|h| h.as_ref().map(|e| e.ts().as_nanos()))
            .collect();
        self.tree = Some(TournamentTree::new(&keys));
        self.primed = true;
    }
}

impl DataSource for MergeSource {
    fn next_event(&mut self) -> Option<Event> {
        if !self.primed {
            self.prime();
        }
        let tree = self.tree.as_mut().expect("primed above");
        let chosen = tree.winner();
        // The winner is a padding slot (>= the real source count) only when there are
        // no sources at all, or every real source is exhausted — either way, done.
        if chosen >= self.heads.len() {
            return None;
        }
        let event = self.heads[chosen].take()?; // exhausted winner ⇒ all sources drained
        let chosen_ts = event.ts().as_nanos();
        // The merged stream must be non-decreasing (the engine's look-ahead contract).
        // Since each child is required to be non-decreasing and we always emit the
        // smallest head, this holds — unless a child violates its own contract; catch
        // that in debug rather than letting an out-of-order event reach a strategy.
        debug_assert!(
            chosen_ts >= self.last_ts,
            "MergeSource child {chosen} produced an out-of-order event (ts {chosen_ts} < \
             last emitted {}); every child must be non-decreasing in time",
            self.last_ts
        );
        self.last_ts = chosen_ts;
        // Refill the child we just drained and restore its leaf in O(log k).
        let refill = self.sources[chosen].next_event();
        let new_key = refill.as_ref().map(|e| e.ts().as_nanos());
        self.heads[chosen] = refill;
        tree.set(chosen, new_key);
        Some(event)
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

    /// A deterministic LCG (no dependency) so the fuzz is reproducible.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state >> 16
    }

    /// The reference merge — the exact semantics the tournament tree must preserve:
    /// repeatedly emit the non-exhausted source with the smallest `(ts, source_index)`,
    /// the lower index winning a tie. (A plain `O(n)` linear scan, as `MergeSource`
    /// used before the tree.)
    fn naive_merge(sources: &[Vec<(i64, i64)>]) -> Vec<(i64, u16, i64)> {
        let mut idx = vec![0usize; sources.len()];
        let mut out = Vec::new();
        loop {
            let mut best: Option<usize> = None;
            for s in 0..sources.len() {
                if idx[s] < sources[s].len()
                    && best.is_none_or(|b| sources[s][idx[s]].0 < sources[b][idx[b]].0)
                {
                    best = Some(s);
                }
            }
            let Some(s) = best else { break };
            let (ts, v) = sources[s][idx[s]];
            out.push((ts, s as u16, v));
            idx[s] += 1;
        }
        out
    }

    #[test]
    fn tournament_tree_matches_naive_merge_under_fuzz() {
        let mut state = 0x1234_5678_9abc_def0u64;
        for _ in 0..3000 {
            let k = (lcg(&mut state) % 40) as usize; // 0..=39 sources (incl. 0 and 1)
            let mut data: Vec<Vec<(i64, i64)>> = Vec::with_capacity(k);
            for _ in 0..k {
                let len = (lcg(&mut state) % 25) as usize; // 0..=24 events
                let mut ts = 0i64;
                let mut pts = Vec::with_capacity(len);
                for _ in 0..len {
                    // Small steps (incl. 0) → non-decreasing with frequent ties, which
                    // is exactly where the index tie-break matters.
                    ts += (lcg(&mut state) % 3) as i64;
                    pts.push((ts, (lcg(&mut state) % 1000) as i64));
                }
                data.push(pts);
            }
            let srcs: Vec<Box<dyn DataSource>> = data
                .iter()
                .enumerate()
                .map(|(i, pts)| sig(i as u16, pts))
                .collect();
            let got = drain(MergeSource::new(srcs));
            let want = naive_merge(&data);
            assert_eq!(
                got, want,
                "tournament-tree merge diverged from the naive reference for k={k}"
            );
        }
    }
}
