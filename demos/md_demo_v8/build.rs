//! Selects the SDK objects this binary links.
//!
//! `V8VideoIn` resolves to `sdk/v8/lib`, a different SDK generation from every
//! other demo here — see `anyka-link` and `docs/SDK-SOURCES.md`.

use anyka_link::Module;

fn main() {
    anyka_link::emit(&[Module::V8VideoIn]);
}
