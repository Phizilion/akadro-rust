// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A shared **request-rate limiter** for downloads — so concurrent multi-symbol
//! back-fills stay under a venue's documented requests-per-second ceiling instead of
//! tripping a 429/418 ban.
//!
//! [`RateLimiter`] is a min-interval pacer: each [`acquire`](RateLimiter::acquire)
//! reserves the next allowed grant instant **under the lock**, then sleeps **outside**
//! the lock until that instant. Cloning shares the same internal cursor, so one
//! limiter handed to N worker threads paces them collectively, not per-thread.
//!
//! **On wall-clock here:** like the [flush cadence](crate::FlushPolicy), this is the
//! disk/network-IO layer, not the deterministic engine — pacing only decides *when*
//! a fetch is issued, never *which* bars come back or their order, so it cannot
//! perturb a backtest. [`per_second(0.0)`](RateLimiter::per_second) is a no-op pacer
//! (never sleeps), which makes every consumer fully deterministic and testable.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Inner {
    /// The earliest instant the next request may be issued. `None` until the first
    /// `acquire` (the first request is never delayed).
    next_free: Option<Instant>,
}

/// A cloneable min-interval request pacer shared across download workers.
///
/// Construct with [`per_second`](Self::per_second); pacing is collective across all
/// clones (they share one cursor). A non-positive rate yields a no-op limiter that
/// never sleeps.
#[derive(Clone, Debug)]
pub struct RateLimiter {
    inner: Arc<Mutex<Inner>>,
    /// Minimum spacing between consecutive grants. `ZERO` ⇒ no-op (unbounded).
    min_interval: Duration,
}

impl RateLimiter {
    /// A limiter allowing at most `per_sec` requests per second (collectively across
    /// clones). `per_sec <= 0.0` (or non-finite) ⇒ an **unbounded no-op** limiter
    /// whose `acquire` never sleeps — the deterministic default for tests and for
    /// callers that opt out of pacing.
    #[must_use]
    pub fn per_second(per_sec: f64) -> Self {
        let min_interval = if per_sec.is_finite() && per_sec > 0.0 {
            Duration::from_secs_f64(1.0 / per_sec)
        } else {
            Duration::ZERO
        };
        Self {
            inner: Arc::new(Mutex::new(Inner { next_free: None })),
            min_interval,
        }
    }

    /// An unbounded no-op limiter (never paces). Equivalent to `per_second(0.0)`.
    #[must_use]
    pub fn unbounded() -> Self {
        Self::per_second(0.0)
    }

    /// Whether this limiter actually paces (i.e. `min_interval > 0`). A no-op limiter
    /// returns `false`.
    #[must_use]
    pub fn is_pacing(&self) -> bool {
        self.min_interval > Duration::ZERO
    }

    /// Block until this worker is allowed to issue its next request. Reserves the
    /// grant instant under the lock (so concurrent callers serialize onto a single
    /// monotonically-advancing schedule), then sleeps outside the lock.
    ///
    /// A no-op limiter (`min_interval == 0`) returns immediately without touching the
    /// clock-sleep path.
    pub fn acquire(&self) {
        if self.min_interval.is_zero() {
            return; // unbounded: no reservation, no sleep
        }
        let now = Instant::now();
        let wait = {
            let mut g = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let grant = match g.next_free {
                Some(t) if t > now => t, // queue behind the prior reservation
                _ => now,                // free now
            };
            g.next_free = Some(grant + self.min_interval);
            grant.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            std::thread::sleep(wait); // IO pacing — never reaches a Bar/fill/RunReport
        }
    }
}

/// The default requests-per-second floor for a venue, used when the caller doesn't
/// override it. Values are **conservative, docs-derived** ceilings (golden-rule 27:
/// docs-derived, not live-verified — a real deployment may raise them per its tier).
/// An unknown venue falls back to `1000/120 ≈ 8.3` req/s, which equals today's
/// per-page delay floor, so routing an unrecognized venue through the limiter is no
/// slower than the existing `with_page_delay` pacing (no regression).
#[must_use]
pub fn default_req_per_sec(venue: &str) -> f64 {
    match venue {
        // Higher published REST budgets.
        "okx" | "binance" | "binance-futures" | "bybit" => 10.0,
        // More conservative published budgets.
        "mexc" | "mexc-futures" | "kucoin" => 8.0,
        // Unknown venue → today's page-delay floor (1000ms / 120 ≈ 8.3/s).
        _ => 1000.0 / 120.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_op_never_sleeps() {
        let rl = RateLimiter::per_second(0.0);
        assert!(!rl.is_pacing());
        let t0 = Instant::now();
        for _ in 0..1000 {
            rl.acquire(); // must be effectively instant — no reservation, no sleep
        }
        assert!(t0.elapsed() < Duration::from_millis(50));
        // Negative / non-finite also collapse to no-op.
        assert!(!RateLimiter::per_second(-5.0).is_pacing());
        assert!(!RateLimiter::per_second(f64::NAN).is_pacing());
        assert!(!RateLimiter::unbounded().is_pacing());
    }

    #[test]
    fn clones_share_one_cursor() {
        // A pacing limiter and its clone advance the SAME next_free reservation.
        let rl = RateLimiter::per_second(1000.0); // 1ms spacing
        assert!(rl.is_pacing());
        let clone = rl.clone();
        // First acquire on the original is free (next_free was None) and sets a
        // reservation; the clone sees it (shared Arc<Mutex<Inner>>).
        rl.acquire();
        let reserved = clone
            .inner
            .lock()
            .unwrap()
            .next_free
            .expect("reservation set after first acquire");
        clone.acquire();
        let advanced = clone.inner.lock().unwrap().next_free.unwrap();
        // The clone's acquire pushed the shared cursor forward by ~min_interval.
        assert!(advanced >= reserved);
    }

    #[test]
    fn default_floors_per_venue() {
        let approx = |got: f64, want: f64| assert!((got - want).abs() < 1e-9, "{got} != {want}");
        approx(default_req_per_sec("okx"), 10.0);
        approx(default_req_per_sec("binance"), 10.0);
        approx(default_req_per_sec("binance-futures"), 10.0);
        approx(default_req_per_sec("bybit"), 10.0);
        approx(default_req_per_sec("mexc"), 8.0);
        approx(default_req_per_sec("mexc-futures"), 8.0);
        approx(default_req_per_sec("kucoin"), 8.0);
        // Unknown → today's page-delay floor.
        approx(default_req_per_sec("nope"), 1000.0 / 120.0);
    }

    #[test]
    fn min_interval_matches_rate() {
        // 8/s ⇒ 125ms spacing.
        let rl = RateLimiter::per_second(8.0);
        assert_eq!(rl.min_interval, Duration::from_secs_f64(1.0 / 8.0));
    }
}
