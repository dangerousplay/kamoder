//! Build-script helper: link only the SDK objects a binary actually uses.
//!
//! # Why this exists
//!
//! Linking the whole SDK into every binary looks harmless and is not. On a
//! real AK3918 the audio path crashes when all 21 objects are loaded:
//!
//! ```text
//! [ak_rb_init:37] init rb_size=16384 ring buffer OK
//! Segmentation fault (core dumped)
//! ```
//!
//! The core puts the fault in `pthread_mutex_init`, reached from
//! `ak_ai_open+260`. It is not a Rust problem: the vendor's own prebuilt C
//! `ai_demo` crashes in exactly the same place when it is given the full
//! 21-object `LD_LIBRARY_PATH`, and works when given the five objects its
//! makefile actually links. Something in the wider set interposes a symbol the
//! audio path resolves differently.
//!
//! So each binary declares the modules it uses and gets the same minimal set
//! the vendor's `build.sh` uses for the equivalent demo.
//!
//! # A second trap in the vendored objects
//!
//! `cross-compile/` ships **two different builds of `libplat_thread.so`**:
//!
//! ```text
//! a6d3fbf5...  jpeg_snapshot, libre_anyka_app, venc_demo
//! f94db11c...  ai_demo, aenc_demo, md_demo, rtsp
//! ```
//!
//! They are not interchangeable, in both directions. Pairing the `a6d3fbf5`
//! build with the audio objects produced the `pthread_mutex_init` crash above;
//! the `f94db11c` build does not even export the `ak_thread_cond_*` and
//! `ak_thread_mutexattr_*` symbols that `libplat_venc_cb.so` needs, so the
//! video path fails to link against it.
//!
//! `sdk/lib` therefore keeps one directory per coherent **variant**, copied
//! straight from the vendor demo that ships it, and each module resolves
//! against its own variant. Binaries also get an `$ORIGIN`-relative `RPATH`
//! so the right variant is found at runtime without any `LD_LIBRARY_PATH`
//! juggling by the launcher.
//!
//! ```no_run
//! // demos/ai_demo/build.rs
//! anyka_link::emit(&[anyka_link::Module::AudioIn]);
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// An SDK capability a binary needs.
///
/// The library list behind each one is taken from the matching vendor
/// `cross-compile/*/build.sh`, which is the only authority on what the SDK
/// objects need from each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Module {
    /// Video capture and ISP statistics: `ak_vi_*`, `ak_vpss_*`.
    VideoIn,
    /// Video encoding and JPEG stills: adds `ak_venc_*`.
    VideoEncode,
    /// Audio capture: `ak_ai_*`.
    AudioIn,
    /// Audio encoding: adds `ak_aenc_*`.
    AudioEncode,
    /// The SDK's own RTSP server.
    Rtsp,
    /// Misc drivers: watchdog, IR, PTZ.
    Drivers,

    /// Video capture and ISP statistics on the **AK3918AV100 (V8)** SDK.
    ///
    /// A separate module rather than a variant of [`Module::VideoIn`] because
    /// it resolves against an entirely different SDK generation — see
    /// `sdk/v8/PROVENANCE.md`. Unlike the EV200 objects, the V8 set is one
    /// coherent variant with no incompatible `libplat_thread.so` split.
    V8VideoIn,
}

impl Module {
    /// Which coherent set of vendored objects this module resolves against.
    ///
    /// Mixing variants in one binary does not work — see the module docs — so
    /// [`emit`] rejects it rather than producing something that crashes only
    /// on hardware.
    fn variant(self) -> &'static str {
        match self {
            Module::VideoIn => "md",
            Module::VideoEncode => "video",
            Module::AudioIn | Module::AudioEncode => "audio",
            Module::Rtsp | Module::Drivers => "rtsp",
            // Lives under sdk/v8/lib rather than sdk/lib/<variant>; handled
            // in `emit`, which is why this returns a distinct marker.
            Module::V8VideoIn => "v8",
        }
    }

    /// Libraries for this module, without the `lib` prefix or `.so` suffix.
    fn libs(self) -> &'static [&'static str] {
        match self {
            Module::VideoIn => &[
                "akuio",
                "akispsdk",
                "plat_common",
                "plat_thread",
                "plat_vi",
                "plat_vpss",
                "plat_ipcsrv",
            ],
            Module::VideoEncode => &[
                "akuio",
                "akispsdk",
                "akv_encode",
                "plat_common",
                "plat_thread",
                "plat_vi",
                "plat_vpss",
                "plat_ipcsrv",
                "plat_venc_cb",
                "mpi_venc",
                "akstreamenc",
            ],
            Module::AudioIn => &[
                "plat_thread",
                "plat_common",
                "plat_ipcsrv",
                "plat_ai",
                "akaudiofilter",
            ],
            Module::AudioEncode => &[
                "plat_thread",
                "plat_common",
                "plat_ipcsrv",
                "plat_ai",
                "akaudiofilter",
                "akaudiocodec",
                "mpi_aenc",
            ],
            Module::Rtsp => &[
                "plat_vi",
                "plat_thread",
                "akispsdk",
                "plat_common",
                "akuio",
                "plat_vpss",
                "plat_ipcsrv",
                "akstreamenc",
                "mpi_venc",
                "app_rtsp",
                "app_net",
                "plat_ai",
                "akae",
                "plat_drv",
                "mpi_aenc",
                "akmedia",
                "mpi_aed",
                "akaudiocodec",
            ],
            Module::Drivers => &["plat_drv", "plat_common", "plat_thread"],
            // The V8 equivalent of the EV200 `md_demo` link line. `plat_mem`,
            // `plat_log` and `plat_osal` are new in this generation and the
            // video path pulls them in.
            Module::V8VideoIn => &[
                "plat_vi",
                "plat_vpss",
                "plat_isp_sdk",
                "plat_common",
                "plat_thread",
                "plat_mem",
                "plat_log",
                "plat_osal",
                "plat_dbg",
                "plat_timer",
            ],
        }
    }
}

/// Emits the link flags for the given modules.
///
/// Call from a binary crate's `build.rs`.
pub fn emit(modules: &[Module]) {
    println!("cargo:rerun-if-env-changed=ANYKA_SDK");

    let Some(first) = modules.first() else {
        panic!("anyka_link::emit needs at least one module");
    };
    let variant = first.variant();
    for module in modules {
        assert_eq!(
            module.variant(),
            variant,
            "{module:?} and {first:?} come from different SDK variants, which \
             cannot be linked into one binary"
        );
    }

    // The V8 objects live at sdk/v8/lib, not sdk/lib/<variant>.
    let sdk_root = sdk_lib_dir();
    let sdk_lib = if variant == "v8" {
        sdk_root
            .parent()
            .expect("sdk/lib has a parent")
            .join("v8")
            .join("lib")
    } else {
        sdk_root.join(variant)
    };
    assert!(
        sdk_lib.is_dir(),
        "no SDK variant at {}; run ./x to regenerate sdk/lib",
        sdk_lib.display()
    );

    let mut libs: Vec<&'static str> = Vec::new();
    for module in modules {
        for lib in module.libs() {
            if !libs.contains(lib) {
                libs.push(lib);
            }
        }
    }

    // A GNU ld script, linked as one ordinary `-l`, for two reasons:
    //
    // * the SDK objects reference each other in both directions, so they have
    //   to sit inside a single `--start-group`/`--end-group`, and
    //   `cargo:rustc-link-arg` does not propagate the way `rustc-link-lib`
    //   does;
    // * `-lm`/`-lpthread`/`-ldl` belong *inside* that group. The SDK calls
    //   `floor` and the pthread API, and if those are still unresolved when
    //   the linker reaches Rust's `compiler_builtins` it binds them to that
    //   rlib's hidden copies and fails with
    //   "hidden symbol `floor' ... is referenced by DSO".
    let mut script = String::from(
        "/* GNU ld script generated by anyka-link. The Anyka SDK objects have\n\
         \x20  circular references, so they link as one group. */\nGROUP (\n",
    );
    for lib in &libs {
        let _ = writeln!(script, "  -l{lib}");
    }
    script.push_str("  -lm\n  -lpthread\n  -ldl\n)\n");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let script_path = out.join("libanyka_sdk_selected.so");
    std::fs::write(&script_path, script)
        .unwrap_or_else(|e| panic!("cannot write {}: {e}", script_path.display()));

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-search=native={}", sdk_lib.display());
    println!("cargo:rustc-link-lib=dylib=anyka_sdk_selected");
    for name in ["pthread", "dl", "m"] {
        println!("cargo:rustc-link-lib=dylib={name}");
    }

    // Find this binary's own variant at runtime, relative to the executable.
    // Without it the launcher would have to know which variant each binary
    // wants, and a single wrong `LD_LIBRARY_PATH` would silently load the
    // incompatible `libplat_thread.so`.
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib/{variant}");
    // uClibc's loader honours DT_RUNPATH, but only if DT_RPATH is absent;
    // ask for the old tag so both loaders behave the same.
    println!("cargo:rustc-link-arg-bins=-Wl,--disable-new-dtags");
}

/// Finds `sdk/lib`, either from `ANYKA_SDK` or by walking up from the crate.
///
/// The returned directory holds one subdirectory per variant.
fn sdk_lib_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ANYKA_SDK") {
        return PathBuf::from(dir).join("lib");
    }
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let mut dir: &Path = &manifest;
    loop {
        let candidate = dir.join("sdk/lib");
        if candidate.is_dir() {
            return candidate;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => panic!(
                "cannot find sdk/lib above {}; set ANYKA_SDK",
                manifest.display()
            ),
        }
    }
}
