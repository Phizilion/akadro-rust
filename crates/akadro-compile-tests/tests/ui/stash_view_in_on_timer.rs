//! LEAK ATTEMPT: stash a branded `Series` from the `on_timer` hook. The hook's
//! `Ctx<'_>` carries the same fresh per-call brand, so the stash cannot escape —
//! this must NOT compile. (Per-hook regression proof for the brand, i6.)

use akadro_core::{Bar, InstrumentId, Price, Timestamp};
use akadro_engine::{Ctx, Series, Strategy};

struct Leaky<'a> {
    saved: Option<Series<'a, Price>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_timer(&mut self, _at: Timestamp, ctx: &mut Ctx<'_>) {
        self.saved = ctx.closes(InstrumentId::new(0));
    }

    fn on_bar(&mut self, _bar: Bar, _ctx: &mut Ctx<'_>) {}
}

fn main() {}
