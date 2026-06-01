//! LEAK ATTEMPT: call a forward accessor on `Series`. There is deliberately no
//! `ahead`/`forward`/`peek`/`next_bar` method — only `latest`/`ago`/`last_n`
//! reach backward — so naming one is a hard "no method" error. This pins
//! sub-property (a) of layer 2: no forward accessor exists.

use akadro_core::{Bar, InstrumentId};
use akadro_engine::{Ctx, Strategy};

struct Peeker;

impl Strategy for Peeker {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        let series = ctx.closes(InstrumentId::new(0)).unwrap();
        // No such method: the only accessors go backward in time.
        let _ = series.ahead(1);
    }
}

fn main() {}
