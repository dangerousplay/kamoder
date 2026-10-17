//! The individual xtask commands.

use eyre::{bail, eyre, Result, WrapErr};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The camera's target triple.
pub const TARGET: &str = "armv5te-unknown-linux-uclibceabi";
/// Tier 3 target: `std` is not shipped for it and has to be built from source,
/// which needs nightly plus the `rust-src` component.
const TOOLCHAIN_ENV: &str = "ANYKA_RUST_TOOLCHAIN";
const DEFAULT_TOOLCHAIN: &str = "nightly";
const TOOLCHAIN_ENV_CROSS: &str = "ANYKA_TOOLCHAIN";
const DEFAULT_CROSS_TOOLCHAIN: &str = "/opt/arm-anykav200-crosstool";
const SD_TREE_ENV: &str = "ANYKA_SD_TREE";
const VM_DIR_ENV: &str = "ANYKA_VM_DIR";

/// Vendor-free binaries staged by `dist`, in the order they are listed to
/// the user. None of them loads an SDK object (PKG-01); `camera` is the
/// `camerad` package's binary.
const BINARIES: &[&str] = &[
    "supervisor",
    "wifi",
    "ntp",
    "wdt_feeder",
    "lights",
    "sshd",
    "web",
    "onvif",
    "rtsp",
    "camera",
    "follow",
    "audio",
    "update",
];

/// Staged in `boot/bin/` instead of `bin/`, with their own `boot/lib/sys/`
/// (OTA design §1, §3): the boot helper runs before a slot is chosen, so it
/// must not live in, or load anything from, either slot. Their link-time
/// RPATH (`$ORIGIN/../lib/sys`) finds that copy.
const BOOT_BINARIES: &[&str] = &["kboot"];

/// SDK demos, staged (with the SDK objects they load) only by
/// `./x dist --with-demos`.
const DEMOS: &[&str] = &[
    "ai_demo",
    "aenc_demo",
    "venc_demo",
    "md_demo",
    "md_demo_v8",
    "rtsp_demo",
    "jpeg_snapshot",
];

/// `dist`/`sd`/`flash` flag that adds [`DEMOS`] and the SDK objects.
pub(crate) const WITH_DEMOS: &str = "--with-demos";

pub fn help() {
    println!("Tasks:");
    println!("  build [cargo args...]   cross-compile the workspace (release)");
    println!("  check [cargo args...]   type-check without linking");
    println!("  clippy [cargo args...]  lint, warnings denied");
    println!("  fmt [--check]           format, or verify formatting");
    println!("  test                    run host-side unit tests");
    println!("  gen-onvif [--check]     regenerate crates/onvif-schema/src/generated from the vendored WSDL/XSD,");
    println!("                          or verify the committed files are up to date");
    println!("  deny                    cargo-deny: advisories, licences, bans, sources");
    println!("  webui                   build the web admin UI (webui/, npm) into webui/dist");
    println!(
        "  dist [--with-demos]     build and stage dist/anyka_rust: vendor-free binaries, boards/,"
    );
    println!("                          www/ if webui is built, boot/ (kboot + its lib/sys, outside the slots);");
    println!("                          --with-demos adds the SDK demos + SDK libs");
    println!("  sd [--with-demos]       dist, strip, and assemble the SD card tree in dist/sdcard");
    println!("  flash [--no-build] [--device /dev/sdXN] [--keep-mounted] [--with-demos]");
    println!("                          sd, then mount the card (label ANYKA) and write it");
    println!("                          sd/flash write the legacy tree and reset OTA A/B state: they delete");
    println!("                          slots/, boot/sel.*, boot/last and update/ packages (keep update/state.json,");
    println!("                          camera.conf, etc/, logs/, backup/, metrics/, media/)");
    println!("  itest [--no-build] [-- pytest args]  system tests: the SD card on a simulated camera (labgrid)");
    println!("  qemu <binary> [args...] run a staged binary under qemu-arm-static");
    println!(
        "  deploy <user@host:dir>  copy dist/anyka_rust over scp (legacy tree only: with A/B slots"
    );
    println!(
        "                          active the camera boots its slot; use an OTA upload instead)"
    );
    crate::kup::help();
    println!("  check-boards            load + validate boards/*.toml, fail on detection ties");
    println!("  check-literals          fail on board literals (AV100, wlan0, /dev/venc...) in generic crates");
    println!(
        "  ci                      fmt --check, gen-onvif --check, check-boards, check-literals,"
    );
    println!("                          clippy, deny, test, build");
    println!();
    println!("Environment:");
    println!("  {TOOLCHAIN_ENV}   rust toolchain to use (default {DEFAULT_TOOLCHAIN})");
    println!(
        "  {TOOLCHAIN_ENV_CROSS}        cross toolchain prefix (default {DEFAULT_CROSS_TOOLCHAIN})"
    );
    println!("  ANYKA_SD_DEVICE        partition for ./x flash (default /dev/disk/by-label/ANYKA)");
    println!(
        "  {SD_TREE_ENV}          SD card tree for ./x sd and ./x flash (default dist/sdcard)"
    );
    println!("  {VM_DIR_ENV}           qemu VM build output for ./x qemu and ./x itest (default tools/qemu-armv5/out)");
}

/// Workspace root, i.e. the directory holding `Cargo.toml` and `./x`.
pub(crate) fn workspace_root() -> PathBuf {
    // xtask/ -> workspace root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent directory")
        .to_path_buf()
}

fn toolchain() -> String {
    std::env::var(TOOLCHAIN_ENV).unwrap_or_else(|_| DEFAULT_TOOLCHAIN.into())
}

pub(crate) fn cross_toolchain() -> PathBuf {
    PathBuf::from(
        std::env::var(TOOLCHAIN_ENV_CROSS).unwrap_or_else(|_| DEFAULT_CROSS_TOOLCHAIN.into()),
    )
}

/// Runs a command in the workspace root and fails loudly if it does.
pub(crate) fn run(program: &str, args: &[&str]) -> Result<()> {
    let root = workspace_root();
    eprintln!("$ {program} {}", args.join(" "));
    let status = Command::new(program)
        .args(args)
        .current_dir(&root)
        .status()
        .wrap_err_with(|| format!("cannot run {program}"))?;
    if !status.success() {
        bail!("{program} {} failed with {status}", args.join(" "));
    }
    Ok(())
}

/// Builds a `cargo +toolchain <subcommand> ...` argument list.
fn cargo_args<'a>(subcommand: &'a str, extra: &'a [String]) -> Vec<String> {
    let mut args = vec![format!("+{}", toolchain()), subcommand.to_owned()];
    args.extend(extra.iter().cloned());
    args
}

/// `rustfmt` from the same toolchain `./x fmt` uses, so formatted output
/// agrees with `./x fmt --check`.
pub(crate) fn rustfmt_command() -> Command {
    let mut command = Command::new("rustfmt");
    command.arg(format!("+{}", toolchain()));
    command
}

pub(crate) fn run_cargo(subcommand: &str, extra: &[String]) -> Result<()> {
    let args = cargo_args(subcommand, extra);
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run("cargo", &borrowed)
}

/// Fails early with an actionable message rather than a wall of cargo output.
fn ensure_prerequisites() -> Result<()> {
    let cross = cross_toolchain();
    if !cross.is_dir() {
        bail!(
            "cross toolchain not found at {}\n\
             install it with tools/install-toolchain.sh, or set {TOOLCHAIN_ENV_CROSS}",
            cross.display()
        );
    }

    let toolchain = toolchain();
    let output = Command::new("rustup")
        .args(["component", "list", "--toolchain", &toolchain])
        .output()
        .wrap_err("cannot run rustup")?;
    let installed = String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.starts_with("rust-src") && line.contains("(installed)"));
    if !installed {
        bail!(
            "the rust-src component is required to build std for a tier 3 target\n\
             run: rustup component add rust-src --toolchain {toolchain}"
        );
    }
    Ok(())
}

pub fn build(extra: &[String]) -> Result<()> {
    ensure_prerequisites()?;
    let mut args = vec!["--release".to_owned()];
    if extra.is_empty() {
        args.push("--workspace".to_owned());
    } else {
        args.extend_from_slice(extra);
    }
    run_cargo("build", &args)
}

pub fn check(extra: &[String]) -> Result<()> {
    ensure_prerequisites()?;
    let mut args = vec!["--workspace".to_owned(), "--all-targets".to_owned()];
    args.extend_from_slice(extra);
    run_cargo("check", &args)
}

pub fn clippy(extra: &[String]) -> Result<()> {
    ensure_prerequisites()?;
    let mut args = vec![
        "--workspace".to_owned(),
        "--all-targets".to_owned(),
        "--".to_owned(),
        "-D".to_owned(),
        "warnings".to_owned(),
    ];
    // Anything the caller passes goes *before* the `--` separator.
    if !extra.is_empty() {
        let split = args.len() - 3;
        for (offset, value) in extra.iter().enumerate() {
            args.insert(split + offset, value.clone());
        }
    }
    run_cargo("clippy", &args)
}

pub fn fmt(extra: &[String]) -> Result<()> {
    let mut args = vec!["--all".to_owned()];
    args.extend_from_slice(extra);
    run_cargo("fmt", &args)?;
    // xtask lives in its own workspace, so it needs a second pass.
    let mut args = vec![
        "--all".to_owned(),
        "--manifest-path".to_owned(),
        "xtask/Cargo.toml".to_owned(),
    ];
    args.extend_from_slice(extra);
    run_cargo("fmt", &args)
}

/// Crates whose tests can run on the host.
///
/// Everything else links the ARM SDK objects, which cannot be linked into an
/// x86-64 test binary, so those are excluded rather than failing loudly.
const HOST_TESTABLE: &[&str] = &[
    "os",
    "telemetry",
    "supervisor",
    "auth",
    "anyka-ptz",
    "ptz-proto",
    "lights-proto",
    "lights",
    "follow-proto",
    "follow",
    "audio-proto",
    "audio",
    "ota-manifest",
    "bootctl",
    "update-proto",
    "ota",
    "kboot",
    "fetch",
    "onvif-schema",
    "sshd",
    "web",
    "onvif",
    "rtsp",
    "anyka-v8",
    "wifi",
    "ntp",
    "anyka-itest-support",
    "camera",
    "anyka-av100",
    "camerad",
    "video-proto",
    "board",
];

pub fn test(extra: &[String]) -> Result<()> {
    // The unit tests are logic-only and target-independent, so they are built
    // for the host rather than cross-compiled and emulated.
    let host = host_triple()?;
    let mut args = vec!["--target".to_owned(), host];
    if extra
        .iter()
        .any(|arg| arg == "-p" || arg.starts_with("--package"))
    {
        args.extend_from_slice(extra);
    } else {
        for package in HOST_TESTABLE {
            args.push("-p".to_owned());
            args.push((*package).to_owned());
        }
        args.extend_from_slice(extra);
    }
    run_cargo("test", &args)
}

pub fn deny(_extra: &[String]) -> Result<()> {
    if which("cargo-deny").is_none() {
        bail!("cargo-deny is not installed; run: cargo install cargo-deny --locked");
    }
    run("cargo", &["deny", "--all-features", "check"])
}

/// Builds the web admin SPA: `webui/` → `webui/dist`, precompressed.
///
/// `build:camera` runs the type check, the Vite build and the gzip/brotli
/// pass, so the camera only ever sends files as they are on the card.
pub fn webui(_extra: &[String]) -> Result<()> {
    let dir = workspace_root().join("webui");
    if !dir.join("package.json").is_file() {
        bail!("{} has no package.json", dir.display());
    }
    let Some(npm) = which("npm") else {
        bail!(
            "npm is not installed; the web UI needs Node.js 20 or newer\n\
             install it (e.g. `pacman -S nodejs npm`, `apt install nodejs npm`), then run ./x webui"
        );
    };
    // `npm ci` is reproducible but needs the lockfile.
    let install = if dir.join("package-lock.json").is_file() {
        "ci"
    } else {
        "install"
    };
    for args in [&[install][..], &["run", "build:camera"]] {
        eprintln!("$ (webui) npm {}", args.join(" "));
        let status = Command::new(&npm)
            .args(args)
            .current_dir(&dir)
            .status()
            .wrap_err("cannot run npm")?;
        if !status.success() {
            bail!("npm {} failed with {status}", args.join(" "));
        }
    }
    let dist = dir.join("dist");
    if !dist.join("index.html").is_file() {
        bail!(
            "the build did not produce {}",
            dist.join("index.html").display()
        );
    }
    let (files, bytes) = tree_size(&dist)?;
    println!(
        "\nweb UI built: {} ({files} files, {bytes} bytes)",
        dist.display()
    );
    Ok(())
}

/// Built SPA, if `./x webui` has run.
fn webui_dist() -> Option<PathBuf> {
    let dist = workspace_root().join("webui/dist");
    dist.join("index.html").is_file().then_some(dist)
}

/// Copies a directory tree, files only (the SPA has no links or specials).
fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else {
            std::fs::copy(entry.path(), &destination)
                .wrap_err_with(|| format!("cannot copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Number of files and total bytes under a directory.
fn tree_size(dir: &Path) -> Result<(u64, u64)> {
    let mut files = 0;
    let mut bytes = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let (f, b) = tree_size(&entry.path())?;
            files += f;
            bytes += b;
        } else {
            files += 1;
            bytes += entry.metadata()?.len();
        }
    }
    Ok((files, bytes))
}

/// Copies every `boards/*.toml` into `target` (replacing it), returning the
/// profile names.
fn stage_boards(root: &Path, target: &Path) -> Result<Vec<String>> {
    if target.exists() {
        std::fs::remove_dir_all(target)?;
    }
    std::fs::create_dir_all(target)?;
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root.join("boards")).wrap_err("cannot read boards/")? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "toml") {
            let file = path.file_name().ok_or_else(|| eyre!("unnamed profile"))?;
            std::fs::copy(&path, target.join(file))
                .wrap_err_with(|| format!("cannot stage {}", path.display()))?;
            names.push(file.to_string_lossy().trim_end_matches(".toml").to_owned());
        }
    }
    if names.is_empty() {
        bail!("no board profiles in boards/");
    }
    names.sort();
    Ok(names)
}

/// Splits `--with-demos` off the arguments `dist`/`sd`/`flash` pass to cargo.
fn take_with_demos(extra: &[String]) -> (bool, Vec<String>) {
    let with_demos = extra.iter().any(|a| a == WITH_DEMOS);
    let rest = extra.iter().filter(|a| *a != WITH_DEMOS).cloned().collect();
    (with_demos, rest)
}

pub fn dist(extra: &[String]) -> Result<()> {
    let (with_demos, extra) = take_with_demos(extra);
    build(&extra)?;

    let root = workspace_root();
    let dist = root.join("dist/anyka_rust");
    let bin_dir = dist.join("bin");
    let lib_dir = dist.join("lib");

    // Always staged from scratch: nothing in dist/anyka_rust is meant to be
    // edited in place (the card's own files live in the SD tree, see
    // SD_PRESERVED), so a stale binary, profile or config never survives.
    if dist.exists() {
        std::fs::remove_dir_all(&dist)
            .wrap_err_with(|| format!("cannot clear {}", dist.display()))?;
    }
    std::fs::create_dir_all(&bin_dir)?;
    std::fs::create_dir_all(&lib_dir)?;

    let built = root.join("target").join(TARGET).join("release");
    let mut staged = Vec::new();
    let demos: &[&str] = if with_demos { DEMOS } else { &[] };
    let boot_bin_dir = dist.join("boot/bin");
    let slot_binaries = BINARIES.iter().chain(demos).map(|b| (b, &bin_dir));
    let boot_binaries = BOOT_BINARIES.iter().map(|b| (b, &boot_bin_dir));
    for (binary, dir) in slot_binaries.chain(boot_binaries) {
        let source = built.join(binary);
        if !source.is_file() {
            continue;
        }
        std::fs::create_dir_all(dir)?;
        let destination = dir.join(binary);
        std::fs::copy(&source, &destination)
            .wrap_err_with(|| format!("cannot copy {}", source.display()))?;
        let name = if dir == &bin_dir {
            (*binary).to_owned()
        } else {
            format!("boot/bin/{binary}")
        };
        staged.push((name, std::fs::metadata(&destination)?.len()));
    }
    if staged.is_empty() {
        bail!("nothing was built; run ./x build first");
    }

    if with_demos {
        // One directory per SDK variant, matching the `$ORIGIN/../lib/<variant>`
        // RPATH that anyka-link bakes into each demo. The variants are not
        // interchangeable — see that crate's docs.
        for entry in std::fs::read_dir(root.join("sdk/lib"))? {
            let variant = entry?.path();
            if !variant.is_dir() {
                continue;
            }
            let name = variant
                .file_name()
                .ok_or_else(|| eyre!("unnamed variant"))?;
            stage_objects(&variant, &lib_dir.join(name))?;
        }

        // The V8 SDK is a whole separate tree, not a variant of the EV200 one, so
        // it lives outside `sdk/lib` — but its binaries carry the same
        // `$ORIGIN/../lib/<variant>` RPATH, with `v8` as the variant.
        let v8 = root.join("sdk/v8/lib");
        if v8.is_dir() {
            stage_objects(&v8, &lib_dir.join("v8"))?;
        }
    }
    stage_runtime(&root, &lib_dir)?;
    stage_boot_runtime(&dist)?;
    check_closure(&dist)?;

    copy_executable(
        &root.join("tools/run-on-camera.sh"),
        &dist.join("run-on-camera.sh"),
    )?;
    copy_executable(&root.join("tools/init.sh"), &dist.join("init.sh"))?;
    std::fs::copy(
        root.join("tools/camera.conf.example"),
        dist.join("camera.conf.example"),
    )
    .wrap_err("cannot stage camera.conf.example")?;
    // The shipped default, always: dist was cleared above, so there is no
    // local copy to preserve (the old "seed only if missing" check could
    // never fire). Per-camera settings belong in camera.conf on the card.
    std::fs::copy(
        root.join("tools/supervisor.toml"),
        dist.join("supervisor.toml"),
    )
    .wrap_err("cannot stage supervisor.toml")?;
    // Board profiles, next to bin/: the supervisor resolves `<base>/boards`.
    let boards = stage_boards(&root, &dist.join("boards"))?;
    std::fs::create_dir_all(dist.join("logs"))?;

    // The SPA is optional for dist: the Rust stack runs without it (the API
    // still answers), and not every machine building firmware has npm.
    let www = match webui_dist() {
        Some(built) => {
            copy_tree(&built, &dist.join("www"))?;
            Some(tree_size(&dist.join("www"))?)
        }
        None => None,
    };

    println!("\nstaged {}:", dist.display());
    for (name, size) in staged {
        println!("  {name:<16} {size:>8} bytes");
    }
    println!("  {:<16} {}", "boards/", boards.join(", "));
    match www {
        Some((files, bytes)) => println!("  {:<16} {bytes:>8} bytes ({files} files)", "www/"),
        None => println!("\nwarning: web UI not built, www/ not staged; run ./x webui"),
    }
    if !with_demos {
        println!("\nSDK demos not staged; ./x dist {WITH_DEMOS} adds them and the SDK objects");
    }
    println!("\ncopy it to the SD card, then on the camera run:");
    println!("  sh /mnt/disc1/anyka_rust/run-on-camera.sh <binary> [args...]");
    Ok(())
}

/// Files on the card that belong to the camera, not to the build. `sd` never
/// overwrites or deletes them: they hold secrets, generated keys, logs and the
/// Wi-Fi backups.
pub(crate) const SD_PRESERVED: &[&str] = &["camera.conf", "etc", "logs", "backup", "metrics"];

/// Further camera-owned data-root entries a reflash keeps (OTA design §1):
/// the media store. Not in [`SD_PRESERVED`] only because `kup` lists it
/// separately.
const SD_PRESERVED_DATA: &[&str] = &["media"];

/// The updater's directory under the data root. A reflash keeps only
/// [`UPDATE_KEPT`] in it and deletes downloaded/uploaded packages.
const UPDATE_DIR: &str = "update";

/// What a reflash keeps in [`UPDATE_DIR`]: the updater's settings and its
/// anti-replay counter (`highest_seq`), which must never go backwards.
const UPDATE_KEPT: &[&str] = &["state.json"];

/// Whether `anyka_rust/<name>` on a card belongs to the camera: never
/// overwritten by `sd`/`flash`, only seeded when missing.
pub(crate) fn is_camera_owned(name: &str) -> bool {
    SD_PRESERVED.contains(&name) || SD_PRESERVED_DATA.contains(&name) || name == UPDATE_DIR
}

/// Clears the entry `path` of a card's `anyka_rust/` before `sd`/`flash`
/// write the freshly built legacy tree.
///
/// A reflash resets the A/B state (OTA design §1) so the card boots exactly
/// what was just written: `slots/` and the whole `boot/` (the selector
/// records `sel.0`/`sel.1`, `last`, and the old kboot) are deleted, and
/// `boot/` is rewritten with only the new kboot and its runtime. With no
/// selector records, kboot and config.sh fall back to the legacy tree (slot
/// a = `anyka_rust/` itself). Keeping the records instead would point the
/// card at stale slots, or at a slot that no longer exists. In `update/`
/// only `state.json` survives (settings and `highest_seq`); packages are
/// deleted. Camera data ([`SD_PRESERVED`], `media/`) is never touched.
pub(crate) fn clear_build_owned(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if SD_PRESERVED.contains(&name) || SD_PRESERVED_DATA.contains(&name) {
        return Ok(());
    }
    let removal = |path: &Path| {
        if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        }
        .wrap_err_with(|| format!("cannot remove {}", path.display()))
    };
    if name == UPDATE_DIR && path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?.path();
            let kept = entry
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| UPDATE_KEPT.contains(&n));
            if !kept {
                removal(&entry)?;
            }
        }
        return Ok(());
    }
    removal(path)
}

/// The tree that mirrors the root of the SD card: `dist/sdcard`, or
/// `$ANYKA_SD_TREE` to keep a tree that lives elsewhere (camera-owned files
/// in it are preserved, see [`SD_PRESERVED`]).
pub(crate) fn sd_contents_dir() -> Result<PathBuf> {
    let card = std::env::var_os(SD_TREE_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("dist/sdcard"));
    std::fs::create_dir_all(&card).wrap_err_with(|| format!("cannot create {}", card.display()))?;
    Ok(card)
}

/// Output of `tools/qemu-armv5/run.sh` (kernel + extracted firmware rootfs),
/// or `$ANYKA_VM_DIR`.
pub(crate) fn vm_out_dir() -> PathBuf {
    std::env::var_os(VM_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("tools/qemu-armv5/out"))
}

/// Builds, strips, and syncs the stack into the SD card tree (`dist/sdcard`).
///
/// `dist` keeps full debug info for `./x qemu` and crash analysis; the card
/// gets stripped copies. Space is not the reason — the SD card has plenty —
/// but these files are committed to git, and a push over the camera's Wi-Fi
/// moves a fifth of the bytes.
pub fn sd(extra: &[String]) -> Result<()> {
    dist(extra)?;

    let root = workspace_root();
    let dist = root.join("dist/anyka_rust");
    let card = sd_contents_dir()?;
    let target = card.join("anyka_rust");
    std::fs::create_dir_all(&target)?;

    // Replace everything the build owns; keep what the camera owns. This
    // also resets the A/B state, see [`clear_build_owned`].
    for entry in std::fs::read_dir(&target)? {
        clear_build_owned(&entry?.path())?;
    }

    let strip = cross_toolchain().join("usr/bin/arm-anykav200-linux-uclibcgnueabi-strip");
    if !strip.is_file() {
        bail!("strip not found at {}", strip.display());
    }
    println!("\nstripped for {}:", target.display());
    for (dir, prefix) in [("bin", ""), ("boot/bin", "boot/bin/")] {
        let from = dist.join(dir);
        if !from.is_dir() {
            continue;
        }
        let bin = target.join(dir);
        std::fs::create_dir_all(&bin)?;
        for entry in std::fs::read_dir(&from)? {
            let source = entry?.path();
            let file = source.file_name().ok_or_else(|| eyre!("unnamed binary"))?;
            let destination = bin.join(file);
            let status = Command::new(&strip)
                .arg("-o")
                .arg(&destination)
                .arg(&source)
                .status()
                .wrap_err("cannot run strip")?;
            if !status.success() {
                bail!("strip {} failed with {status}", source.display());
            }
            println!(
                "  {:<16} {:>8} bytes",
                format!("{prefix}{}", file.to_string_lossy()),
                std::fs::metadata(&destination)?.len()
            );
        }
    }
    // The boot helper's own runtime (see stage_boot_runtime).
    let boot_sys = dist.join("boot/lib/sys");
    if boot_sys.is_dir() {
        stage_objects(&boot_sys, &target.join("boot/lib/sys"))?;
    }

    for entry in std::fs::read_dir(dist.join("lib"))? {
        let variant = entry?.path();
        if variant.is_dir() {
            let name = variant
                .file_name()
                .ok_or_else(|| eyre!("unnamed variant"))?;
            stage_objects(&variant, &target.join("lib").join(name))?;
        }
    }

    // www/ is build-owned: replaced wholesale above, like bin/ and lib/.
    let www = dist.join("www");
    if www.is_dir() {
        copy_tree(&www, &target.join("www"))?;
        let (files, bytes) = tree_size(&target.join("www"))?;
        println!("  {:<16} {bytes:>8} bytes ({files} files)", "www/");
    } else {
        println!("  {:<16} not built; run ./x webui", "www/");
    }

    for script in ["init.sh", "run-on-camera.sh"] {
        copy_executable(&dist.join(script), &target.join(script))?;
    }
    // supervisor.toml is build-owned like bin/ (not in SD_PRESERVED): the
    // card always gets the shipped default; per-camera settings go in
    // camera.conf, which is preserved.
    std::fs::copy(
        root.join("tools/supervisor.toml"),
        target.join("supervisor.toml"),
    )?;
    // boards/ is build-owned too, replaced wholesale above.
    copy_tree(&dist.join("boards"), &target.join("boards"))?;
    std::fs::copy(
        root.join("tools/camera.conf.example"),
        target.join("camera.conf.example"),
    )?;
    std::fs::copy(root.join("tools/SD-README.md"), target.join("README.md"))?;

    // The boot hooks at the card root (card/ in the repository).
    copy_executable(&root.join("card/autorun.sh"), &card.join("autorun.sh"))?;
    std::fs::create_dir_all(card.join("Factory"))?;
    copy_executable(
        &root.join("card/Factory/config.sh"),
        &card.join("Factory/config.sh"),
    )?;
    std::fs::copy(
        root.join("card/Factory/README.md"),
        card.join("Factory/README.md"),
    )?;

    let conf = target.join("camera.conf");
    if !conf.exists() {
        std::fs::copy(target.join("camera.conf.example"), &conf)?;
        println!(
            "\ncreated {} from the example — fill in Wi-Fi, SSH, web admin and ONVIF passwords",
            conf.display()
        );
    }

    println!("\nSD card tree ready: {}", card.display());
    println!("copy autorun.sh, Factory/ and anyka_rust/ to the root of the card,");
    println!("or run ./x flash --no-build to write them to an inserted card.");
    Ok(())
}

pub fn qemu(args: &[String]) -> Result<()> {
    let Some((binary, rest)) = args.split_first() else {
        bail!("usage: ./x qemu <binary> [args...]");
    };
    let qemu = which("qemu-arm-static")
        .or_else(|| which("qemu-arm"))
        .ok_or_else(|| eyre!("qemu-arm-static is not installed"))?;

    let root = workspace_root();
    let sysroot = root.join("target/qemu-sysroot");
    prepare_qemu_sysroot(&sysroot)?;

    let program = root.join("dist/anyka_rust/bin").join(binary);
    if !program.is_file() {
        bail!("{} not found; run ./x dist first", program.display());
    }

    let mut command = Command::new(qemu);
    command
        .arg("-L")
        .arg(&sysroot)
        .arg(&program)
        .args(rest)
        .current_dir(&root);
    let status = command.status().wrap_err("cannot run qemu")?;
    if !status.success() {
        bail!("{binary} exited with {status}");
    }
    Ok(())
}

/// Builds a minimal ARM sysroot for qemu-user out of the camera-like rootfs.
///
/// Only the firmware's `/lib` is needed: everything else a binary loads is
/// found through its `RPATH` in `dist/anyka_rust/lib`, as on the card.
fn prepare_qemu_sysroot(sysroot: &Path) -> Result<()> {
    let source = vm_out_dir().join("rootfs.pristine/lib");
    if !source.is_dir() {
        bail!(
            "no ARM rootfs at {}; build it with tools/qemu-armv5 first",
            source.display()
        );
    }

    let lib = sysroot.join("lib");
    std::fs::create_dir_all(&lib)?;
    for entry in std::fs::read_dir(&source)? {
        let entry = entry?;
        let destination = lib.join(entry.file_name());
        if destination.exists() || entry.file_type()?.is_dir() {
            continue;
        }
        // Symlinks in the source tree are copied as their target's contents,
        // which is fine for a throwaway sysroot.
        let _ = std::fs::copy(entry.path(), &destination);
    }
    Ok(())
}

pub fn deploy(args: &[String]) -> Result<()> {
    let Some(destination) = args.first() else {
        bail!("usage: ./x deploy <user@host:/mnt>");
    };
    let root = workspace_root();
    let dist = root.join("dist/anyka_rust");
    if !dist.is_dir() {
        bail!("{} does not exist; run ./x dist first", dist.display());
    }
    run("scp", &["-r", &dist.to_string_lossy(), destination])
}

/// Every gate, cheapest first. Steps run one at a time on purpose: each
/// cargo invocation is memory-hungry.
pub fn ci(_extra: &[String]) -> Result<()> {
    fmt(&["--check".to_owned()])?;
    crate::gen_onvif::gen_onvif(&["--check".to_owned()])?;
    crate::checks::check_boards(&[])?;
    crate::checks::check_literals(&[])?;
    clippy(&[])?;
    deny(&[])?;
    test(&[])?;
    build(&[])?;
    Ok(())
}

fn host_triple() -> Result<String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .wrap_err("cannot run rustc")?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| eyre!("rustc did not report a host triple"))
}

pub(crate) fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

fn copy_executable(source: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::copy(source, destination)
        .wrap_err_with(|| format!("cannot copy {}", source.display()))?;
    let mut permissions = std::fs::metadata(destination)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(destination, permissions)?;
    Ok(())
}

/// Copies every shared object from `source` into `target`, creating it.
fn stage_objects(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    for object in std::fs::read_dir(source)? {
        let path = object?.path();
        if is_shared_object(&path) {
            let file = path.file_name().ok_or_else(|| eyre!("unnamed library"))?;
            std::fs::copy(&path, target.join(file))?;
        }
    }
    Ok(())
}

/// `libfoo.so` or `libfoo.so.<digits>[.<digits>...]`.
fn is_shared_object(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    match name.split_once(".so") {
        Some((stem, version)) => {
            !stem.is_empty()
                && (version.is_empty()
                    || version.strip_prefix('.').is_some_and(|v| {
                        v.split('.')
                            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                    }))
        }
        None => false,
    }
}

/// Libraries every binary takes from the firmware: the loader named in
/// `PT_INTERP`, and the libc that has to come from the same uClibc build.
const FIRMWARE_LIBS: &[&str] = &["libc.so.0", "ld-uClibc.so.0", "ld-uClibc.so.1"];

/// What the crosstool's uClibc 0.9.33 splits out of libc. The camera's
/// uClibc-ng 1.0.31 has all of it in `libc.so.0` and ships none of these.
const LIBC_STUBS: &[&str] = &["libpthread.so.0", "libm.so.0", "librt.so.0", "libdl.so.0"];

/// `RPATH` given to every staged shared object: its own directory, then
/// `lib/sys`. The camera's ld.so looks up a library's dependencies only in
/// *that library's* `RPATH`, then `LD_LIBRARY_PATH`, then `/lib:/usr/lib` —
/// never in the executable's. The vendor objects carry no `RPATH`, so without
/// this their dependencies would come from the firmware (the vendor app's own
/// SDK in `/usr/lib`, a different generation) or not load at all.
const LIBRARY_RPATH: &str = "$ORIGIN:$ORIGIN/../sys";

/// Stages `lib/sys` (libc stubs and the GCC runtime, see
/// `sdk/runtime/README.md`) and gives [`LIBRARY_RPATH`] to every staged
/// object that needs more than libc.
///
/// Only shared objects are patched: patchelf'd *executables* crash in the
/// camera's ld.so, which is why theirs is set at link time (ak-linker.sh).
fn stage_runtime(root: &Path, lib_dir: &Path) -> Result<()> {
    let patchelf = which("patchelf").ok_or_else(|| {
        eyre!("patchelf is required to stage the SDK objects; install it from your distribution")
    })?;

    let sys = lib_dir.join("sys");
    stage_objects(&root.join("sdk/runtime"), &sys)?;

    let cc = cross_toolchain().join("usr/bin/arm-anykav200-linux-uclibcgnueabi-gcc");
    let source = root.join("target/libc-stub.c");
    std::fs::create_dir_all(source.parent().expect("target/ has a parent"))?;
    std::fs::write(
        &source,
        "/* Empty: the symbols are in the firmware's libc.so.0. */\n",
    )?;
    for stub in LIBC_STUBS {
        let status = Command::new(&cc)
            .args(["-shared", "-nostdlib"])
            .arg(format!("-Wl,-soname,{stub}"))
            .arg("-o")
            .arg(sys.join(stub))
            .arg(&source)
            .status()
            .wrap_err_with(|| format!("cannot run {}", cc.display()))?;
        if !status.success() {
            bail!("building the {stub} stub failed with {status}");
        }
    }

    for entry in std::fs::read_dir(lib_dir)? {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        for object in std::fs::read_dir(&dir)? {
            let object = object?.path();
            if !is_shared_object(&object) {
                continue;
            }
            // Objects that need nothing but libc stay byte-identical to the
            // vendor's: patchelf pads what it rewrites to a page.
            let (needed, _) = dynamic_entries(&object)?;
            if needed.iter().all(|n| FIRMWARE_LIBS.contains(&n.as_str())) {
                continue;
            }
            let status = Command::new(&patchelf)
                .args(["--force-rpath", "--set-rpath", LIBRARY_RPATH])
                .arg(&object)
                .status()
                .wrap_err("cannot run patchelf")?;
            if !status.success() {
                bail!("patchelf {} failed with {status}", object.display());
            }
        }
    }
    Ok(())
}

/// Gives `boot/bin/*` their own `boot/lib/sys/`: each library they need
/// that the slot ships in `lib/sys/` (the same files). Libraries the
/// firmware provides ([`FIRMWARE_LIBS`]) are not copied. Runs after
/// [`stage_runtime`], and before [`check_closure`], which then checks the
/// boot helper the way the camera's ld.so will load it.
fn stage_boot_runtime(dist: &Path) -> Result<()> {
    let boot_bin = dist.join("boot/bin");
    if !boot_bin.is_dir() {
        return Ok(());
    }
    let sys = dist.join("lib/sys");
    let boot_sys = dist.join("boot/lib/sys");
    std::fs::create_dir_all(&boot_sys)?;
    let mut queue = Vec::new();
    for entry in std::fs::read_dir(&boot_bin)? {
        queue.push(entry?.path());
    }
    let mut seen = std::collections::HashSet::new();
    while let Some(object) = queue.pop() {
        let (needed, _) = dynamic_entries(&object)?;
        for library in needed {
            if FIRMWARE_LIBS.contains(&library.as_str()) || !seen.insert(library.clone()) {
                continue;
            }
            let source = sys.join(&library);
            if source.is_file() {
                let destination = boot_sys.join(&library);
                std::fs::copy(&source, &destination)
                    .wrap_err_with(|| format!("cannot copy {}", source.display()))?;
                queue.push(destination);
            }
        }
    }
    Ok(())
}

/// `DT_NEEDED` entries and `RPATH`/`RUNPATH` directories of an ELF object.
fn dynamic_entries(object: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let readelf = cross_toolchain().join("usr/bin/arm-anykav200-linux-uclibcgnueabi-readelf");
    let output = Command::new(&readelf)
        .arg("-d")
        .arg(object)
        .output()
        .wrap_err_with(|| format!("cannot run {}", readelf.display()))?;
    if !output.status.success() {
        bail!("readelf -d {} failed", object.display());
    }
    let mut needed = Vec::new();
    let mut paths = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let value = || {
            line.split_once('[')
                .and_then(|(_, rest)| rest.rsplit_once(']'))
                .map(|(value, _)| value.to_owned())
        };
        if line.contains("(NEEDED)") {
            needed.extend(value());
        } else if line.contains("(RPATH)") || line.contains("(RUNPATH)") {
            paths.extend(value().iter().flat_map(|v| v.split(':').map(str::to_owned)));
        }
    }
    Ok((needed, paths))
}

/// Fails unless every library each staged binary loads — directly or
/// through another library — is found on the card the way the camera's
/// ld.so would find it (the requesting object's own `RPATH`), apart from
/// [`FIRMWARE_LIBS`].
fn check_closure(dist: &Path) -> Result<()> {
    let firmware = firmware_symbols()?;
    let mut problems = Vec::new();
    let mut binaries = Vec::new();
    for dir in ["bin", "boot/bin"] {
        let dir = dist.join(dir);
        if dir.is_dir() {
            for entry in std::fs::read_dir(&dir)? {
                binaries.push(entry?.path());
            }
        }
    }
    for binary in binaries {
        let name = binary
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut queue = vec![binary.clone()];
        let mut seen = std::collections::HashSet::new();
        // Symbols the binary's card-side closure defines.
        let mut defined: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(object) = queue.pop() {
            let object = std::fs::canonicalize(&object)?;
            if !seen.insert(object.clone()) {
                continue;
            }
            if object != std::fs::canonicalize(&binary)? {
                defined.extend(dynamic_symbols(&object)?.defined);
            }
            let origin = object.parent().expect("an object has a directory");
            let (needed, paths) = dynamic_entries(&object)?;
            for library in needed {
                let found = paths
                    .iter()
                    .map(|dir| PathBuf::from(dir.replace("$ORIGIN", &origin.to_string_lossy())))
                    .map(|dir| dir.join(&library))
                    .find(|candidate| candidate.is_file());
                match found {
                    Some(path) => queue.push(path),
                    None if FIRMWARE_LIBS.contains(&library.as_str()) => {}
                    None => problems.push(format!(
                        "  {name}: {} needs {library}, which is not on the card",
                        object.file_name().unwrap_or_default().to_string_lossy()
                    )),
                }
            }
        }
        // Every strong undefined symbol of the binary has to be defined by
        // its closure or the firmware's libc/ld.so. The crosstool links
        // against uClibc 0.9.33, so a symbol it resolves can still be missing
        // on the camera (pthread_atfork was), and the camera's ld.so then
        // refuses to start the binary.
        for symbol in dynamic_symbols(&binary)?.undefined {
            if !defined.contains(&symbol) && !firmware.contains(&symbol) {
                problems.push(format!(
                    "  {name}: symbol {symbol} is defined neither on the card nor by the firmware libc"
                ));
            }
        }
    }
    if !problems.is_empty() {
        problems.sort();
        problems.dedup();
        bail!(
            "these would be loaded from the firmware, or not at all:\n{}",
            problems.join("\n")
        );
    }
    println!(
        "\nlibrary closure: every binary loads only libc and ld.so from the firmware, \
         and every symbol it needs is defined"
    );
    Ok(())
}

/// What the AV100 firmware's `libc.so.0` and `ld-uClibc.so.0` export, as
/// recorded in `sdk/runtime/firmware-libc.symbols` (see its header).
fn firmware_symbols() -> Result<std::collections::HashSet<String>> {
    let path = workspace_root().join("sdk/runtime/firmware-libc.symbols");
    let text = std::fs::read_to_string(&path)
        .wrap_err_with(|| format!("cannot read {}", path.display()))?;
    Ok(text
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect())
}

/// Dynamic symbols of an ELF object: what it defines, and what it needs
/// without a weak fallback.
struct DynamicSymbols {
    defined: Vec<String>,
    undefined: Vec<String>,
}

fn dynamic_symbols(object: &Path) -> Result<DynamicSymbols> {
    let readelf = cross_toolchain().join("usr/bin/arm-anykav200-linux-uclibcgnueabi-readelf");
    let output = Command::new(&readelf)
        .args(["--dyn-syms", "-W"])
        .arg(object)
        .output()
        .wrap_err_with(|| format!("cannot run {}", readelf.display()))?;
    if !output.status.success() {
        bail!("readelf --dyn-syms {} failed", object.display());
    }
    let mut symbols = DynamicSymbols {
        defined: Vec::new(),
        undefined: Vec::new(),
    };
    // `Num: Value Size Type Bind Vis Ndx Name`
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [_, _, _, _, bind, _, ndx, name, ..] = fields[..] else {
            continue;
        };
        let name = name.split('@').next().unwrap_or(name).to_owned();
        match (ndx, bind) {
            ("UND", "GLOBAL") => symbols.undefined.push(name),
            ("UND", _) => {}
            (_, "GLOBAL" | "WEAK") => symbols.defined.push(name),
            _ => {}
        }
    }
    Ok(symbols)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reflash resets the A/B state but keeps camera data and the
    /// updater's state.json.
    #[test]
    fn reflash_resets_ab_state_and_keeps_camera_data() {
        let card = std::env::temp_dir().join(format!("xtask-reflash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&card);
        let files = [
            "boot/sel.0",
            "boot/sel.1",
            "boot/last",
            "boot/bin/kboot",
            "slots/b/init.sh",
            "update/state.json",
            "update/kamoder-1-update.kup",
            "update/download.part",
            "bin/web",
            "camera.conf",
            "etc/web-users",
            "logs/boot.log",
            "media/clip.mp4",
        ];
        for file in files {
            let path = card.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        }
        for entry in std::fs::read_dir(&card).unwrap() {
            clear_build_owned(&entry.unwrap().path()).unwrap();
        }
        let left: Vec<_> = files.iter().filter(|f| card.join(f).exists()).collect();
        assert_eq!(
            left,
            [
                &"update/state.json",
                &"camera.conf",
                &"etc/web-users",
                &"logs/boot.log",
                &"media/clip.mp4"
            ]
        );
        assert!(!card.join("boot").exists() && !card.join("slots").exists());
        assert!(is_camera_owned("update") && is_camera_owned("media"));
        assert!(!is_camera_owned("boot") && !is_camera_owned("slots"));
        std::fs::remove_dir_all(&card).unwrap();
    }
}
