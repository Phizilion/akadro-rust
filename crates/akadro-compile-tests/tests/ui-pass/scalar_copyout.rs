//! LEGITIMATE: copying a scalar value (a `Price`) out of the present and
//! remembering it in `self` is not look-ahead — you are remembering the past.
//! This MUST compile.

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Strategy};

struct Memory {
    last_close: Price,
}

impl Strategy for Memory {
    fn on_bar(&mut self, bar: Bar, ctx: &mut Ctx<'_>) {
        // Copy the current bar's close (a `Copy` scalar).
        self.last_close = bar.close;
        // Or read it from the series and copy the value out.
        if let Some(c) = ctx.closes(InstrumentId::new(0)).and_then(|s| s.latest()) {
            self.last_close = c;
        }
    }
}

fn main() {
    let m = Memory { last_close: Price::ZERO };
    assert_eq!(m.last_close, Price::ZERO);
}
