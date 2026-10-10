//! Raw, generated FFI bindings to the Anyka AK39xx IPC SDK.
//!
//! Everything in here is `unsafe` and mirrors the C headers one-to-one. Prefer
//! the `anyka` crate, which wraps these in handles that cannot be used after
//! they are closed and that release frames and streams on drop.
//!
//! The bindings are produced at build time from `sdk/include` so they always
//! match the exact SDK drop that is vendored in this repository.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
#![allow(rustdoc::broken_intra_doc_links)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// `AK_SUCCESS`, the return value every `ak_*` call uses for "no error".
pub const AK_OK: ::std::os::raw::c_int = 0;

unsafe extern "C" {
    /// Sets the audio input volume, 0 (mute) to 12 (maximum).
    ///
    /// Declared by hand because `ak_ai.h` in this SDK drop exposes it only as
    /// a macro over `ak_ai_set_adc_volume` and `ak_ai_set_aslc_volume` — two
    /// functions the vendored `libplat_ai.so` does not actually export.
    /// The real, exported entry point is this one.
    pub fn ak_ai_set_volume(
        handle: *mut ::std::os::raw::c_void,
        volume: ::std::os::raw::c_int,
    ) -> ::std::os::raw::c_int;
}

/// Bindings for the **AK3918AV100 (V8)** SDK in `sdk/v8`.
///
/// A module rather than a cargo feature: features unify across the workspace,
/// so a feature-selected generation would leak into every other crate in the
/// same build. The two generations share no types, and nothing here links
/// anything — `anyka-link` decides that per binary.
pub mod v8 {
    #![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
    #![allow(rustdoc::broken_intra_doc_links)]
    // `ak_vi_open` and friends exist in both generations with different
    // signatures — that is the whole reason this port exists. The lint assumes
    // one symbol means one ABI, which holds per binary but not per crate: no
    // binary links both `libakispsdk.so` and `libplat_vi.so`, and `anyka-link`
    // is what enforces that.
    #![allow(clashing_extern_declarations)]

    include!(concat!(env!("OUT_DIR"), "/bindings_v8.rs"));
}
