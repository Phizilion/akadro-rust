//! LEAK ATTEMPT: stash the auxiliary-signal `Series` returned by `ctx.signal`
//! into `self`, to read it on a later bar (look-ahead via the non-OHLCV channel).
//! The signal channel reuses the same invariant per-call brand as the price
//! series, so the stored lifetime cannot unify with `self`'s — this must NOT
//! compile, exactly like stashing a price `Series`.

use akadro_core::{Bar, InstrumentId};
use akadro_engine::{Ctx, Series, Strategy};

struct Leaky<'a> {
    saved: Option<Series<'a, i64>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        // Branded with this call's lifetime; it cannot escape into `self`.
        self.saved = ctx.signal(InstrumentId::new(0), 1);
    }
}

fn main() {}
