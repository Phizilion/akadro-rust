//! LEAK ATTEMPT: stash a branded `Series` through `Arc<Mutex<..>>` (a
//! thread-safe shared cell). The lifetime brand defeats it before `Send`/`Sync`
//! ever matter, so this must NOT compile.

use std::sync::{Arc, Mutex};

use akadro_core::{Bar, InstrumentId, Price};
use akadro_engine::{Ctx, Series, Strategy};

struct Leaky<'a> {
    saved: Arc<Mutex<Option<Series<'a, Price>>>>,
}

impl<'a> Strategy for Leaky<'a> {
    fn on_bar(&mut self, _bar: Bar, ctx: &mut Ctx<'_>) {
        *self.saved.lock().unwrap() = ctx.closes(InstrumentId::new(0));
    }
}

fn main() {}
