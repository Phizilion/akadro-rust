// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The engine's deterministic random source (decision D6).
//!
//! Any probabilistic modelling device (e.g. a stochastic fill or latency model)
//! must draw from this seeded RNG and nothing else — never `thread_rng`, never
//! wall-clock entropy. The seed is part of a run's identity, so a run is exactly
//! reproducible and a parameter sweep across threads is deterministic.
//!
//! Strategies are deliberately given **no** access to randomness: a strategy
//! must behave identically every run, in backtest and live.

use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};

/// A seeded, reproducible RNG (`ChaCha8`). Implements [`rand_core::RngCore`] so
/// it can drive any model that needs randomness, while keeping the concrete
/// `rand_chacha` type out of akadro's public trait signatures.
pub struct DeterministicRng {
    inner: ChaCha8Rng,
    seed: u64,
}

impl DeterministicRng {
    /// Create an RNG from a 64-bit seed.
    #[must_use]
    pub fn seeded(seed: u64) -> Self {
        DeterministicRng {
            inner: ChaCha8Rng::seed_from_u64(seed),
            seed,
        }
    }

    /// The seed this RNG was created with (record it in your run manifest).
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }
}

impl RngCore for DeterministicRng {
    #[inline]
    fn next_u32(&mut self) -> u32 {
        self.inner.next_u32()
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        self.inner.next_u64()
    }

    #[inline]
    fn fill_bytes(&mut self, dst: &mut [u8]) {
        self.inner.fill_bytes(dst);
    }
}

impl core::fmt::Debug for DeterministicRng {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeterministicRng")
            .field("seed", &self.seed)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = DeterministicRng::seeded(42);
        let mut b = DeterministicRng::seeded(42);
        assert_eq!(a.seed(), 42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seed_different_sequence() {
        let mut a = DeterministicRng::seeded(1);
        let mut b = DeterministicRng::seeded(2);
        // Extremely unlikely to match across many draws.
        let differ = (0..16).any(|_| a.next_u64() != b.next_u64());
        assert!(differ);
    }

    #[test]
    fn fill_bytes_and_u32_work() {
        let mut r = DeterministicRng::seeded(7);
        let mut buf = [0u8; 8];
        r.fill_bytes(&mut buf);
        assert!(buf.iter().any(|&b| b != 0));
        let _ = r.next_u32();
        assert!(format!("{r:?}").contains("seed"));
    }
}
