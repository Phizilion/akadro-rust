//! LEAK ATTEMPT: capture a branded `Series` inside a boxed closure stored in
//! `self`, then call it on a later bar. The closure would have to outlive the
//! per-call `'bar`, which the invariant brand forbids — this must NOT compile.

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Strategy};

struct Leaky<'a> {
    hook: Option<Box<dyn FnMut() -> Option<Price> + 'a>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        let series = ctx.closes(InstrumentId::new(0));
        self.hook = Some(Box::new(move || series.as_ref().and_then(|s| s.latest())));
    }
}

fn main() {}
