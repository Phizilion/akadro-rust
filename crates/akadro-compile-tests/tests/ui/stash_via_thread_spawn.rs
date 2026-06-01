//! LEAK ATTEMPT: move a branded `Series` into a spawned thread (which requires
//! `'static`). The per-call `'bar` is not `'static`, so this must NOT compile —
//! a `Series` cannot escape the handler even across a thread boundary.

use akadro_core::{Bar, InstrumentId};
use akadro_engine::{Ctx, Strategy};

struct Leaky;

impl Strategy for Leaky {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        let series = ctx.closes(InstrumentId::new(0));
        std::thread::spawn(move || {
            let _ = series.and_then(|s| s.latest());
        });
    }
}

fn main() {}
