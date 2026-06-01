// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The look-ahead brand.
//!
//! `Brand<'bar>` is a zero-sized, **invariant** lifetime marker. It is the third
//! layer of the kill feature (see the crate docs / `AGENTS.md`): every handler
//! invocation gets a fresh `'bar`, and any type carrying `Brand<'bar>` cannot be
//! stored anywhere that outlives the call, because the borrow checker refuses to
//! unify the per-call `'bar` with a longer-lived lifetime.
//!
//! Invariance is essential. A *covariant* marker (`PhantomData<&'bar ()>`) could
//! be silently shrunk, giving a false sense of safety. The `fn(&'bar ()) ->
//! &'bar ()` shape is invariant in `'bar` (it appears in both argument and
//! return position), so no coercion is possible. Verified on rustc 1.95 during
//! the design review.

use core::marker::PhantomData;

/// An invariant, zero-sized lifetime brand tying a value to a single handler
/// invocation. Carried by [`crate::Ctx`], [`crate::Series`] and the market view
/// so they cannot escape the call they were handed to.
pub(crate) type Brand<'bar> = PhantomData<fn(&'bar ()) -> &'bar ()>;
