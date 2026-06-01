//! LEAK ATTEMPT: stash a branded `Series` through a shared `Rc<RefCell<..>>`
//! (the classic "share it out the side" trick). The invariant `'bar` brand still
//! cannot escape the call, so this must NOT compile.

use std::cell::RefCell;
use std::rc::Rc;

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Series, Strategy};

struct Leaky<'a> {
    saved: Rc<RefCell<Option<Series<'a, Price>>>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        *self.saved.borrow_mut() = ctx.closes(InstrumentId::new(0));
    }
}

fn main() {}
