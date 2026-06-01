//! LEAK ATTEMPT: heap-box a branded `Series` into `self`. Boxing does not erase
//! the `'bar` lifetime, so the per-call brand still cannot escape — this must NOT
//! compile.

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Series, Strategy};

struct Leaky<'a> {
    saved: Box<Option<Series<'a, Price>>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        *self.saved = ctx.closes(InstrumentId::new(0));
    }
}

fn main() {}
