//! LEAK ATTEMPT: launder the per-call brand to `'static` (which could then be
//! stored anywhere) by passing the series to a `'static`-bound function. The
//! invariant brand is not `'static`, so this must NOT compile.

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Series, Strategy};

fn keep_forever<T: 'static>(_value: T) {}

struct Escaper;

impl Strategy for Escaper {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        let series: Series<'_, Price> = ctx.closes(InstrumentId::new(0)).unwrap();
        keep_forever(series);
    }
}

fn main() {}
