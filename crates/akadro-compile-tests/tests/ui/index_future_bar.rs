//! LEAK ATTEMPT: read a *future* bar. There is no forward accessor at all, and
//! offsets are `usize` ("bars ago"), so a negative/forward index is not even
//! expressible — this is a type error, not a runtime check.

use akadro_core::{Bar, InstrumentId};
use akadro_engine::{Ctx, Strategy};

struct Peeker;

impl Strategy for Peeker {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        let series = ctx.closes(InstrumentId::new(0)).unwrap();
        // `ago` takes a usize offset into the PAST. There is no `ahead`, no
        // `Index`, and no slice. Trying to express "one bar ahead" as `-1` is a
        // type mismatch.
        let _ = series.ago(-1);
    }
}

fn main() {}
