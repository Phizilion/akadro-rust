//! LEAK ATTEMPT: stash a branded `Series` into a `OnceCell` field for later
//! reads. The invariant `'bar` brand cannot be unified with the struct, so this
//! must NOT compile.

use std::cell::OnceCell;

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Series, Strategy};

struct Leaky<'a> {
    saved: OnceCell<Series<'a, Price>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        if let Some(s) = ctx.closes(InstrumentId::new(0)) {
            let _ = self.saved.set(s);
        }
    }
}

fn main() {}
