//! LEAK ATTEMPT: stash a `LastN` iterator (the `Series::last_n` view) into `self`
//! to keep iterating it on a later bar. `LastN` borrows `'bar` and now also
//! carries the invariant brand, so it cannot escape the call — this must NOT
//! compile.

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, LastN, Strategy};

struct Leaky<'a> {
    saved: Option<LastN<'a, Price>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        if let Some(s) = ctx.closes(InstrumentId::new(0)) {
            self.saved = Some(s.last_n(5));
        }
    }
}

fn main() {}
