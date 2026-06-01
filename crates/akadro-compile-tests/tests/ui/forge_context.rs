//! FORGE ATTEMPT (the decision-D1 proof): construct a `Ctx` here, in a
//! genuinely downstream crate, over data we control — which would defeat every
//! other layer. `Ctx`'s constructor is `pub(crate)` and co-located with the
//! engine loop, so it is unreachable from outside `akadro-engine`. This must NOT
//! compile (E0624: associated function is private).

use akadro_engine::Ctx;

fn main() {
    // No public constructor exists; this references a private associated fn.
    let _forged = Ctx::new();
}
