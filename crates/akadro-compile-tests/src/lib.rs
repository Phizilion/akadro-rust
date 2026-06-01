// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Compile-fail/compile-pass proofs of the look-ahead kill feature.
//!
//! This crate is intentionally a genuinely *downstream* consumer of
//! `akadro-engine`: the `forge_context` trybuild case proves the co-located,
//! truly-private `Ctx` constructor cannot be reached from outside the engine
//! crate (decision D1). The actual cases live under `tests/ui/` and are driven
//! by `trybuild` (added in a later build step).
