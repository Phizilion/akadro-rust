//! LEAK ATTEMPT: stash a branded `Series` through a `Cell` (move-based interior
//! mutability). Same outcome: the per-call brand cannot escape, so this must NOT
//! compile.

use std::cell::Cell;

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Series, Strategy};

struct Leaky<'a> {
    saved: Cell<Option<Series<'a, Price>>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        self.saved.set(ctx.closes(InstrumentId::new(0)));
    }
}

fn main() {}
