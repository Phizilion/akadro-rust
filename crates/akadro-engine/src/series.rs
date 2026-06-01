// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Backward-only time series — the second layer of the kill feature.
//!
//! A [`Series`] is a read-only view of the values observed *up to and including*
//! the current bar. Its API only goes backward in time:
//!
//! * [`Series::latest`] — the current (most recent) value.
//! * [`Series::ago`] — the value `n` bars ago (`ago(0) == latest`).
//! * [`Series::last_n`] — an iterator over the last `n` values, newest first.
//!
//! There is **no** forward accessor, **no** `Index` impl, and **no** way to
//! borrow the underlying slice — so "read the future" is not expressible, and a
//! bad index is an `Option::None`, never a panic. The `'bar` brand additionally
//! prevents stashing a `Series` to read it after more bars have arrived.

use core::fmt;
use core::marker::PhantomData;

use crate::brand::Brand;

/// A backward-only, call-scoped view of one observed time series.
///
/// Obtain one from the context, e.g. `ctx.closes(instrument)`. You can read the
/// present and the past; you cannot read the future, and you cannot keep this
/// value past the current handler call.
pub struct Series<'bar, T> {
    /// Borrows ONLY the values observed so far (the engine appends as time
    /// advances and never exposes un-arrived data), so even the backing slice
    /// physically contains no future.
    data: &'bar [T],
    _brand: Brand<'bar>,
}

impl<'bar, T> Series<'bar, T> {
    /// Wrap an observed slice. Crate-private: only the engine constructs a
    /// `Series`, over data that excludes the future.
    #[inline]
    pub(crate) fn new(data: &'bar [T]) -> Self {
        Series {
            data,
            _brand: PhantomData,
        }
    }

    /// Number of observed values.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// `true` if nothing has been observed yet.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

impl<'bar, T: Copy> Series<'bar, T> {
    /// The most recent (current) value, or `None` if empty.
    #[inline]
    pub fn latest(&self) -> Option<T> {
        self.data.last().copied()
    }

    /// The value `n` bars ago: `ago(0)` is the latest, `ago(1)` one bar back.
    /// Returns `None` if `n` reaches before the start of history. There is no
    /// way to pass a "negative" / forward offset — the type is `usize`.
    #[inline]
    pub fn ago(&self, n: usize) -> Option<T> {
        let len = self.data.len();
        if n >= len {
            None
        } else {
            Some(self.data[len - 1 - n])
        }
    }

    /// Iterate the last `n` values, newest first (yields at most `len` items).
    #[inline]
    pub fn last_n(&self, n: usize) -> LastN<'bar, T> {
        let len = self.data.len();
        LastN {
            data: self.data,
            pos: len,
            remaining: n.min(len),
            _brand: PhantomData,
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Series<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Series")
            .field("len", &self.data.len())
            .finish()
    }
}

/// Iterator over the last `n` values of a [`Series`], newest first.
///
/// Yields `Copy` values, so anything you collect is a copy of the *past* (which
/// you are free to keep). The iterator itself borrows `'bar` and cannot be
/// stashed across calls.
///
/// Like [`Series`], it carries the **invariant** `Brand<'bar>` so it is tied to
/// the handler call it was produced in. Today the `&'bar [T]` borrow alone would
/// block a stash, but the explicit brand is defense-in-depth: it keeps `LastN`
/// invariant in `'bar` (not covariant), so the no-stash guarantee cannot silently
/// regress if a by-reference accessor (e.g. an `as_slice(&self) -> &'bar [T]`) is
/// ever added. Do not remove it without re-proving the look-ahead guarantee.
pub struct LastN<'bar, T> {
    data: &'bar [T],
    pos: usize,
    remaining: usize,
    _brand: Brand<'bar>,
}

// Manual Debug: print only how many items remain, NOT the borrowed `data` slice —
// a derived Debug would dump the whole observed history into debug output (i8),
// mirroring `Series`'s deliberately slice-hiding Debug above.
impl<T> fmt::Debug for LastN<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LastN")
            .field("remaining", &self.remaining)
            .finish_non_exhaustive()
    }
}

impl<T: Copy> Iterator for LastN<'_, T> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        self.pos -= 1;
        Some(self.data[self.pos])
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<T: Copy> ExactSizeIterator for LastN<'_, T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_series() {
        let s: Series<'_, i64> = Series::new(&[]);
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.latest(), None);
        assert_eq!(s.ago(0), None);
        assert_eq!(s.last_n(3).count(), 0);
    }

    #[test]
    fn backward_access() {
        let data = [10i64, 20, 30, 40];
        let s = Series::new(&data);
        assert_eq!(s.len(), 4);
        assert!(!s.is_empty());
        assert_eq!(s.latest(), Some(40));
        assert_eq!(s.ago(0), Some(40));
        assert_eq!(s.ago(1), Some(30));
        assert_eq!(s.ago(3), Some(10));
        // Reaching before the start is None, never a panic and never the future.
        assert_eq!(s.ago(4), None);
        assert_eq!(s.ago(usize::MAX), None);
    }

    #[test]
    fn last_n_newest_first() {
        let data = [1i64, 2, 3, 4, 5];
        let s = Series::new(&data);
        let got: Vec<_> = s.last_n(3).collect();
        assert_eq!(got, vec![5, 4, 3]);
        // Asking for more than exists is clamped.
        let all: Vec<_> = s.last_n(100).collect();
        assert_eq!(all, vec![5, 4, 3, 2, 1]);
        // size_hint / ExactSizeIterator
        let it = s.last_n(2);
        assert_eq!(it.size_hint(), (2, Some(2)));
        assert_eq!(it.len(), 2);
    }

    #[test]
    fn debug_impls() {
        let data = [1i64, 2];
        let s = Series::new(&data);
        assert!(format!("{s:?}").contains("len"));
        let it = s.last_n(1);
        assert!(format!("{it:?}").contains("LastN"));
    }
}
