//! LEAK ATTEMPT: stash the `&mut Ctx` itself into `self`, to call its accessors
//! on a later bar over data that has since advanced. The context is branded with
//! the per-call `'bar`, so it cannot be stored in `self` — this must NOT compile.

use akadro_core::Bar;
use akadro_engine::{Ctx, Strategy};

struct Leaky<'a> {
    saved: Option<&'a mut Ctx<'a>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        self.saved = Some(ctx);
    }
}

fn main() {}
