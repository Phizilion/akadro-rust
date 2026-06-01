//! LEAK ATTEMPT: stash a branded `Series` from the `on_stop` hook. Like every
//! other lifecycle hook, `on_stop` gets a fresh per-call `Ctx<'_>` with the
//! invariant brand, so the stash is rejected — this must NOT compile. (Per-hook
//! regression proof for the brand.)

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Series, Strategy};

struct Leaky<'a> {
    saved: Option<Series<'a, Price>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_stop(&mut self, ctx: &mut Ctx<'_>) {
        self.saved = ctx.closes(InstrumentId::new(0));
    }

    fn on_bar(&mut self, _bar: Bar, _ctx: &mut Ctx<'_>) {}
}

fn main() {}
