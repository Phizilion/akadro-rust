//! LEAK ATTEMPT: index a `Series` directly (`series[i]`) to escape the backward
//! accessors and reach an arbitrary slot. `Series` implements no `Index`, so this
//! is a type error. This pins sub-property (b) of layer 2: no `Index` impl exists.

use akadro_core::{Bar, InstrumentId};
use akadro_engine::{Ctx, Strategy};

struct Indexer;

impl Strategy for Indexer {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        let series = ctx.closes(InstrumentId::new(0)).unwrap();
        // `Series` is not indexable — there is no `impl Index for Series`.
        let _ = series[0usize];
    }
}

fn main() {}
