// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Semantic proof of the look-ahead kill feature.
//!
//! * `tests/ui-pass/*.rs` — legitimate strategies that MUST compile (copying
//!   scalars out of the present, collecting past copies, elided-lifetime impls).
//! * `tests/ui/*.rs` — look-ahead / forge attempts that MUST NOT compile. The
//!   committed `.stderr` files pin the exact compiler diagnostic, so a future
//!   rustc wording change (or an accidental loosening of the API) is caught.
//!   These `.stderr` files are toolchain-pinned (generated on rustc 1.95, the
//!   project MSRV); regenerate with `TRYBUILD=overwrite cargo test`.

#[test]
fn kill_feature() {
    let t = trybuild::TestCases::new();
    // Legitimate code compiles and runs.
    t.pass("tests/ui-pass/*.rs");
    // Look-ahead and context-forging do not compile.
    t.compile_fail("tests/ui/*.rs");
}
