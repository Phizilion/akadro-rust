//! LEAK ATTEMPT: stash a branded `Series` from the `on_account` hook. The hook's
//! `Ctx<'_>` carries the same fresh per-call brand, so the stash cannot escape —
//! this must NOT compile. (Per-hook regression proof for the brand.)

use akadro_core::{AccountEvent, Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Series, Strategy};

struct Leaky<'a> {
    saved: Option<Series<'a, Price>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_account(&mut self, _event: &AccountEvent, ctx: &mut Ctx<'_>) {
        self.saved = ctx.closes(InstrumentId::new(0));
    }

    fn on_bar(&mut self, _bar: Bar, _ctx: &mut Ctx<'_>) {}
}

fn main() {}
