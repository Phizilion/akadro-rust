//! LEAK ATTEMPT: launder a branded `Series` into `self` through `RefCell`
//! interior mutability, to read it on a later bar. The per-call `'bar` cannot be
//! unified with the struct lifetime, so this must NOT compile.

use std::cell::RefCell;

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Series, Strategy};

struct Leaky<'a> {
    saved: RefCell<Option<Series<'a, Price>>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        *self.saved.borrow_mut() = ctx.closes(InstrumentId::new(0));
    }
}

fn main() {}
