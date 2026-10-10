use std::env;
use std::path::{Path, PathBuf};

/// Default location of the crosstool shipped by the Anyka SDK. Override with
/// `ANYKA_TOOLCHAIN` if it lives somewhere else.
const DEFAULT_TOOLCHAIN: &str = "/opt/arm-anykav200-crosstool";
const TOOLCHAIN_TRIPLE: &str = "arm-anykav200-linux-uclibcgnueabi";

/// The vendored SDK generations, as `(subdirectory, wrapper, output)`.
///
/// Both are generated on every build, into separate files, rather than
/// selected by a cargo feature. Features are unified across a workspace, so a
/// `v8` feature enabled by one binary would silently swap the bindings under
/// the EV200 crates too — which fails to compile, and would be far worse if it
/// did not.
const GENERATIONS: &[(Option<&str>, &str, &str)] = &[
    (None, "wrapper.h", "bindings.rs"),
    (Some("v8"), "wrapper_v8.h", "bindings_v8.rs"),
];

fn main() {
    let root = sdk_dir();
    let toolchain =
        PathBuf::from(env::var("ANYKA_TOOLCHAIN").unwrap_or_else(|_| DEFAULT_TOOLCHAIN.into()));

    println!("cargo:rerun-if-env-changed=ANYKA_SDK");
    println!("cargo:rerun-if-env-changed=ANYKA_TOOLCHAIN");
    // Deliberately no link flags here. Which SDK objects a binary links is
    // decided per binary by `anyka-link`, because loading the whole set
    // breaks the audio path on real hardware — see that crate's docs.
    println!("cargo:include={}", root.join("include").display());
    println!("cargo:lib={}", root.join("lib").display());

    for (subdir, wrapper, output) in GENERATIONS {
        let sdk = match subdir {
            Some(name) => root.join(name),
            None => root.clone(),
        };
        let include = sdk.join("include");
        assert!(
            include.is_dir(),
            "no SDK headers at {} — see sdk/v8/PROVENANCE.md",
            include.display()
        );
        println!("cargo:rerun-if-changed={wrapper}");
        generate_bindings(&include, &toolchain, wrapper, output);
    }
}

fn sdk_dir() -> PathBuf {
    if let Ok(dir) = env::var("ANYKA_SDK") {
        return PathBuf::from(dir);
    }
    // anyka-sys/ -> workspace root -> sdk/
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("anyka-sys has a parent directory")
        .join("sdk")
}

/// clang needs gcc's own `stddef.h`/`stdarg.h` when parsing with this sysroot.
/// The directory is versioned (`.../gcc/<triple>/4.8.5/include`), so it is
/// discovered rather than hard-coded.
fn gcc_internal_include(toolchain: &Path) -> PathBuf {
    let base = toolchain.join("usr/lib/gcc").join(TOOLCHAIN_TRIPLE);
    let versions =
        std::fs::read_dir(&base).unwrap_or_else(|e| panic!("cannot read {}: {e}", base.display()));
    for entry in versions.flatten() {
        let candidate = entry.path().join("include");
        if candidate.join("stddef.h").is_file() {
            return candidate;
        }
    }
    panic!("no gcc internal include directory under {}", base.display());
}

fn generate_bindings(include: &Path, toolchain: &Path, wrapper: &str, output: &str) {
    let sysroot = toolchain.join("usr").join(TOOLCHAIN_TRIPLE).join("sysroot");
    let gcc_include = gcc_internal_include(toolchain);

    let bindings = bindgen::Builder::default()
        .header(wrapper)
        // Match the device ABI so bindgen computes the right struct layouts:
        // 32-bit ARM, soft-float, 8-byte alignment for `long long`.
        .clang_arg("--target=arm-unknown-linux-gnueabi")
        .clang_arg(format!("--sysroot={}", sysroot.display()))
        .clang_arg(format!("-I{}", include.display()))
        .clang_arg(format!("-I{}", gcc_include.display()))
        .clang_arg("-D_POSIX_C_SOURCE=199309L")
        .allowlist_function("ak_.*")
        .allowlist_type("(ak_|vpss_|venc_|vi_|VI_|AK_VI_|RECTANGLE_S|aenc_|audio_|video_|pcm_|encode_|crop_|frame|rtsp_|list_head|profile_mode|bitrate_ctrl_mode|enc_method|ai_|osd_).*")
        .allowlist_var("(AK_|VI_|VIDEO_|VPSS_|AUDIO_|AMR_|ENCODE_|LOG_LEVEL_|BR_MODE_|PROFILE_|H264_|HEVC_|MJPEG_|RTSP_|EQ_|BOX_).*")
        .allowlist_type("LOG_LEVEL")
        // The SDK header enums are plain C enums used as `int` arguments.
        .default_enum_style(bindgen::EnumVariation::ModuleConsts)
        .derive_debug(true)
        .derive_default(true)
        .derive_copy(true)
        // `static inline` helpers such as `set_bit()` would need a shim object
        // file; the safe crate reimplements the handful it needs in Rust.
        .generate_inline_functions(false)
        .layout_tests(false)
        .generate()
        .expect("bindgen failed to parse the Anyka SDK headers");

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    bindings
        .write_to_file(out.join(output))
        .unwrap_or_else(|e| panic!("failed to write {output}: {e}"));
}
