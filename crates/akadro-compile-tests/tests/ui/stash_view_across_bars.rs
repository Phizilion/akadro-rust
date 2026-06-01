//! LEAK ATTEMPT: stash a `Series` borrowed this bar into `self`, to read it on a
//! later bar (classic look-ahead). The invariant per-call brand makes the
//! stored lifetime un-unifiable with the struct's lifetime, so this must NOT
//! compile.

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Series, Strategy};

struct Leaky<'a> {
    saved: Option<Series<'a, Price>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        // The series is branded with this call's lifetime; it cannot escape into
        // `self` (which outlives the call).
        self.saved = ctx.closes(InstrumentId::new(0));
    }
}

fn main() {}
