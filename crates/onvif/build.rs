//! Links the vendor driver library for the SDK PTZ backend (EV200).
//!
//! `anyka-ptz`'s own build script already emits the `rustc-link-lib` lines,
//! and those propagate here, but the `$ORIGIN` RPATH that lets the binary
//! find `libplat_drv.so` next to itself on the SD card is `-bins` only and
//! does not; so this binary repeats the call (see `crates/anyka-ptz/build.rs`).
//! Only with the `sdk` feature (off by default); host builds link nothing.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let arm = std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("arm");
    if arm && std::env::var_os("CARGO_FEATURE_SDK").is_some() {
        anyka_link::emit(&[anyka_link::Module::Drivers]);
    }
}
