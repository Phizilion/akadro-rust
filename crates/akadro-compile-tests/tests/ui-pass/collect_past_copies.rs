//! LEGITIMATE: an elided-lifetime impl (the user never types `'bar`) that
//! collects copies of *past* values into its own state. This MUST compile.

use akadro_core::{Bar, Price};
use akadro_engine::{Ctx, Strategy};

struct History {
    closes: Vec<Price>,
}

impl Strategy for History {
    // Note: no `'bar` anywhere in the signature — fully elided.
    fn on_bar(&mut self, bar: Bar, _ctx: &mut Ctx<'_>) {
        self.closes.push(bar.close);
    }
}

fn main() {
    let h = History { closes: Vec::new() };
    assert!(h.closes.is_empty());
}
