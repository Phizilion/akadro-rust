//! LEAK ATTEMPT: accumulate branded `Series` values into a `Vec` field, then
//! index past the current bar on a later call. Rejected for the same reason as
//! a single stash — the per-call lifetime cannot be unified into the struct.

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Series, Strategy};

struct Hoard<'a> {
    all: Vec<Series<'a, Price>>,
}

impl<'a> Strategy for Hoard<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        if let Some(s) = ctx.closes(InstrumentId::new(0)) {
            self.all.push(s);
        }
    }
}

fn main() {}
