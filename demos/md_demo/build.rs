//! Selects the SDK objects this binary links.
//!
//! Only what it uses — linking the full SDK crashes the audio path on real
//! hardware. See the `anyka-link` crate for the evidence.

use anyka_link::Module;

fn main() {
    anyka_link::emit(&[Module::VideoIn]);
}
