//! Links `libplat_drv` (plus `plat_common`/`plat_thread`) when the SDK backend
//! is compiled in.
//!
//! Unlike the demos, this runs in a *library* package: the `rustc-link-lib`
//! lines propagate to every binary that depends on `anyka-ptz`, so a consumer
//! such as `onvif` gets the PTZ objects without listing them. It still
//! has to call `anyka_link::emit(&[Module::Drivers])` from its own `build.rs`
//! for the `$ORIGIN` RPATH (that part is `-bins` only and does not
//! propagate); the two generated scripts are identical, so which one the
//! linker finds first does not matter. A consumer that also needs a module
//! from a different variant cannot link `Drivers` at all — `anyka-link`
//! rejects that mix.
//!
//! Host builds (tests, `--target x86_64-…`) link nothing: the SDK backend is
//! compiled out there.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let arm = std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("arm");
    let sdk = std::env::var_os("CARGO_FEATURE_SDK").is_some();
    if arm && sdk {
        anyka_link::emit(&[anyka_link::Module::Drivers]);
    }
}
