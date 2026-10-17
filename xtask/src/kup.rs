//! `./x kup`: build and sign an OTA update package (OTA-01, OTA-02; design
//! §4 and §8 in `.specs/features/ota/`).
//!
//! Input is the tree `./x dist` staged in `dist/anyka_rust`. Output, in
//! `--out` (default `dist/kup`):
//!
//! ```text
//! kamoder-<version>-update.kup   uncompressed tar, members in this order:
//!     manifest.json              Manifest::to_json(), the exact signed bytes
//!     manifest.json.minisig      minisign signature, trusted comment
//!                                "kamoder <version> seq=<seq>"
//!     payload.tar.gz             component "slot" (sd-tree): the slot tree
//!     boot.tar.gz                component "boot" (boot-shim), if kboot was built
//! manifest.json                  the same two files again, as separate release
//! manifest.json.minisig          assets (stable discovery fetches
//!                                releases/latest/download/manifest.json)
//! ```
//!
//! # What goes where
//!
//! - **slot** (`payload.tar.gz`): everything in the dist tree except the
//!   camera's own data ([`crate::tasks::SD_PRESERVED`] plus the data-root
//!   directories of design §1: `media/`, `boot/`, `slots/`, `update/`) and
//!   except the boot helper `bin/kboot`, which lives outside the slots.
//!   Binaries in `bin/` are stripped with the cross `strip`, like the
//!   release SD card tree; `README.md` is `tools/SD-README.md`, as on the card.
//! - **boot** (`boot.tar.gz`): `boot/bin/kboot` (stripped), `boot/lib/sys/*`
//!   (the runtime kboot loads; the same files as the slot's `lib/sys`) and
//!   `factory/config.sh` (`card/Factory/config.sh`). Paths starting with
//!   `boot/` are relative to the data root (`anyka_rust/`); `factory/` maps
//!   to the card's `Factory/` directory, one level up.
//!
//! # Archive format (what the device's installer can rely on)
//!
//! Regular files only (no directory entries, links or specials), relative
//! paths without `./`, sorted by path; uid/gid 0, empty owner names, mtime =
//! `SOURCE_DATE_EPOCH` or the commit time, mode 0755 if the file is
//! executable else 0644 (normalized, so the host's umask cannot change the
//! bytes). GNU long-name records only appear for paths over 100 bytes. gzip
//! level 9 with a zero header mtime and no file name. Same inputs, same
//! bytes: the manifest and its signature stay reproducible too.

use eyre::{bail, eyre, Result, WrapErr};
use flate2::{Compression, GzBuilder};
use ota_manifest::{
    trusted_comment, Channel, Component, ComponentKind, Digest, FileEntry, Manifest, TrustedKeys,
    Version, SCHEMA,
};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::tasks::{cross_toolchain, workspace_root, SD_PRESERVED, TARGET};

/// Data-root entries that are never part of a slot, beyond
/// [`SD_PRESERVED`]: the media store and the A/B machinery itself (design §1).
const DATA_ROOT_DIRS: &[&str] = &["media", "boot", "slots", "update"];

/// Slot paths owned by the boot shim instead (design §3: kboot never
/// depends on a slot).
const BOOT_SHIM_IN_SLOT: &[&str] = &["bin/kboot"];

/// Payload member names inside the `.kup` (design §4).
const PAYLOAD: &str = "payload.tar.gz";
const BOOT_PAYLOAD: &str = "boot.tar.gz";
const MANIFEST: &str = "manifest.json";
const SIGNATURE: &str = "manifest.json.minisig";

/// Default `min_updater`: the first updater release understands schema 1.
const DEFAULT_MIN_UPDATER: &str = "0.1.0";
/// Default `security_floor`: no floor until the maintainer raises it.
const DEFAULT_SECURITY_FLOOR: &str = "0.0.0";

/// Password for an encrypted `--key`, written to minisign's stdin.
const PASSWORD_ENV: &str = "MINISIGN_PASSWORD";

/// Committed TEST-ONLY key pair (`crates/ota-manifest/keys/test/`).
const TEST_KEY: &str = "crates/ota-manifest/keys/test/test.key";
const TEST_PUB: &str = "crates/ota-manifest/keys/test/test.pub";

const USAGE: &str = "usage: ./x kup [--dist DIR] [--out DIR] [--version V] [--channel stable|beta]
              [--seq N] [--security-floor V] [--min-updater V]
              [--key PATH [--pub PATH]... | --test-key] [--boot-dir DIR | --no-boot]
              [--no-strip]";

pub fn help() {
    println!("  kup [--test-key | --key PATH] [--version V] [--channel stable|beta] [--seq N] ...");
    println!("                          signed OTA package from dist/anyka_rust into dist/kup:");
    println!("                          kamoder-<ver>-update.kup + manifest.json(.minisig); ./x kup --help");
}

#[derive(Debug, Default)]
struct Options {
    dist: Option<PathBuf>,
    out: Option<PathBuf>,
    version: Option<Version>,
    channel: Option<Channel>,
    seq: Option<u64>,
    security_floor: Option<Version>,
    min_updater: Option<Version>,
    key: Option<PathBuf>,
    pubs: Vec<PathBuf>,
    test_key: bool,
    boot_dir: Option<PathBuf>,
    no_boot: bool,
    no_strip: bool,
}

fn parse(args: &[String]) -> Result<Options> {
    let mut o = Options::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| eyre!("{arg} needs a value\n{USAGE}"))
        };
        let version = |v: String, what: &str| {
            Version::parse(v.trim_start_matches('v'))
                .wrap_err_with(|| format!("{what} {v:?} is not semver"))
        };
        match arg.as_str() {
            "--dist" => o.dist = Some(value()?.into()),
            "--out" => o.out = Some(value()?.into()),
            "--version" => o.version = Some(version(value()?, "--version")?),
            "--channel" => {
                o.channel = Some(match value()?.as_str() {
                    "stable" => Channel::Stable,
                    "beta" => Channel::Beta,
                    other => bail!("--channel {other:?}: expected stable or beta"),
                })
            }
            "--seq" => {
                let v = value()?;
                o.seq = Some(v.parse().wrap_err_with(|| format!("--seq {v:?}"))?);
            }
            "--security-floor" => o.security_floor = Some(version(value()?, "--security-floor")?),
            "--min-updater" => o.min_updater = Some(version(value()?, "--min-updater")?),
            "--key" => o.key = Some(value()?.into()),
            "--pub" => o.pubs.push(value()?.into()),
            "--test-key" => o.test_key = true,
            "--boot-dir" => o.boot_dir = Some(value()?.into()),
            "--no-boot" => o.no_boot = true,
            "--no-strip" => o.no_strip = true,
            "-h" | "--help" => {
                println!("{USAGE}\n\nSee docs/notes/OTA.md (release packages).");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    if o.test_key == o.key.is_some() {
        bail!("pass exactly one of --key PATH or --test-key\n{USAGE}");
    }
    if o.test_key && !o.pubs.is_empty() {
        bail!("--pub is for --key; --test-key verifies with {TEST_PUB}");
    }
    if o.no_boot && o.boot_dir.is_some() {
        bail!("--no-boot and --boot-dir exclude each other");
    }
    Ok(o)
}

pub fn kup(args: &[String]) -> Result<()> {
    let o = parse(args)?;
    let root = workspace_root();
    let dist = o
        .dist
        .clone()
        .unwrap_or_else(|| root.join("dist/anyka_rust"));
    let out = o.out.clone().unwrap_or_else(|| root.join("dist/kup"));
    if !dist.join("bin").is_dir() {
        bail!("no {}/bin; run ./x dist first", dist.display());
    }
    if !dist.join("www").is_dir() {
        eprintln!("warning: {} has no www/: the slot ships without the web UI (run ./x webui, then ./x dist)", dist.display());
    }

    let git = Git::read(&root);
    let version = match o.version.clone() {
        Some(v) => v,
        None => default_version(&root, &git)?,
    };
    let channel = o.channel.unwrap_or(if version.pre.is_empty() {
        Channel::Stable
    } else {
        Channel::Beta
    });
    let epoch = source_date_epoch()?.or(git.commit_time).unwrap_or_else(now);
    let seq = o.seq.or(git.commit_time).unwrap_or(epoch);
    let security_floor = o
        .security_floor
        .clone()
        .unwrap_or_else(|| Version::parse(DEFAULT_SECURITY_FLOOR).expect("valid default"));
    let min_updater = o
        .min_updater
        .clone()
        .unwrap_or_else(|| Version::parse(DEFAULT_MIN_UPDATER).expect("valid default"));

    let strip = if o.no_strip {
        None
    } else {
        let strip = cross_toolchain().join("usr/bin/arm-anykav200-linux-uclibcgnueabi-strip");
        if !strip.is_file() {
            bail!(
                "strip not found at {} (set ANYKA_TOOLCHAIN, or --no-strip if dist is already stripped)",
                strip.display()
            );
        }
        Some(strip)
    };
    let work = root.join("target/kup");
    if work.exists() {
        std::fs::remove_dir_all(&work)?;
    }
    std::fs::create_dir_all(&work)?;

    // --- slot -------------------------------------------------------------
    let mut slot = Vec::new();
    collect(&dist, &dist, &mut slot)?;
    slot.retain(|f| {
        let top = f.path.split('/').next().unwrap_or_default();
        !SD_PRESERVED.contains(&top)
            && !DATA_ROOT_DIRS.contains(&top)
            && !BOOT_SHIM_IN_SLOT.contains(&f.path.as_str())
            && f.path != "README.md"
    });
    let readme = root.join("tools/SD-README.md");
    slot.push(TarFile::read(&readme, "README.md")?);
    if let Some(strip) = &strip {
        for f in slot.iter_mut().filter(|f| f.path.starts_with("bin/")) {
            f.data = stripped(strip, &f.source, &work)?;
        }
    }
    let boards = board_names(&dist)?;

    // --- boot shim --------------------------------------------------------
    let boot = if o.no_boot {
        None
    } else {
        boot_files(&root, &dist, o.boot_dir.as_deref(), strip.as_deref(), &work)?
    };

    // --- archives + manifest ----------------------------------------------
    let mut payloads = vec![(PAYLOAD, "slot", ComponentKind::SdTree, slot)];
    if let Some(files) = boot {
        payloads.push((BOOT_PAYLOAD, "boot", ComponentKind::BootShim, files));
    }
    let mut members: Vec<(String, Vec<u8>)> = Vec::new();
    let mut components = Vec::new();
    for (path, name, kind, files) in payloads {
        let gz = gzip(&tar_bytes(&files, epoch)?)?;
        components.push(Component {
            name: name.to_owned(),
            kind,
            path: path.to_owned(),
            size: gz.len() as u64,
            sha256: sha256(&gz),
            files: files.iter().map(TarFile::entry).collect(),
        });
        members.push((path.to_owned(), gz));
    }
    let manifest = Manifest {
        schema: SCHEMA,
        version: version.clone(),
        channel,
        seq,
        security_floor,
        min_updater,
        boards,
        target: TARGET.to_owned(),
        created: rfc3339(epoch),
        notes_url: repo(&root).map(|r| format!("https://github.com/{r}/releases/tag/v{version}")),
        components,
    };
    manifest
        .validate()
        .map_err(|e| eyre!("built an invalid manifest: {e}"))?;
    let json = manifest.to_json();

    // --- sign + verify ----------------------------------------------------
    std::fs::create_dir_all(&out)?;
    let manifest_path = out.join(MANIFEST);
    let sig_path = out.join(SIGNATURE);
    std::fs::write(&manifest_path, &json)?;
    let (key, keys) = if o.test_key {
        eprintln!("warning: signing with the TEST-ONLY key; no real camera trusts this package");
        let mut keys = TrustedKeys::empty();
        keys.add_text("test", &std::fs::read_to_string(root.join(TEST_PUB))?)?;
        (root.join(TEST_KEY), keys)
    } else if o.pubs.is_empty() {
        (
            o.key.clone().expect("checked in parse"),
            TrustedKeys::builtin()?,
        )
    } else {
        let mut keys = TrustedKeys::empty();
        for p in &o.pubs {
            let text = std::fs::read_to_string(p)
                .wrap_err_with(|| format!("cannot read {}", p.display()))?;
            keys.add_text(&p.display().to_string(), &text)?;
        }
        (o.key.clone().expect("checked in parse"), keys)
    };
    let password = if o.test_key {
        None
    } else {
        std::env::var(PASSWORD_ENV).ok()
    };
    sign(
        &key,
        &manifest_path,
        &sig_path,
        &trusted_comment(&manifest),
        password,
    )?;
    let sig = std::fs::read_to_string(&sig_path)?;
    ota_manifest::verify(&json, &sig, &keys).map_err(|e| {
        // Never leave a pair behind that no camera would accept.
        let _ = std::fs::remove_file(&manifest_path);
        let _ = std::fs::remove_file(&sig_path);
        eyre!(
            "the signed manifest does not verify against {}: {e}{}",
            if o.test_key {
                TEST_PUB
            } else {
                "the update keys"
            },
            if keys.is_empty() {
                " (no public key: commit crates/ota-manifest/keys/kamoder.pub or pass --pub)"
            } else {
                ""
            }
        )
    })?;

    // --- .kup -------------------------------------------------------------
    let mut kup_members = vec![
        TarFile::bytes(MANIFEST, json.clone(), 0o644),
        TarFile::bytes(SIGNATURE, sig.into_bytes(), 0o644),
    ];
    for (path, data) in members {
        kup_members.push(TarFile::bytes(&path, data, 0o644));
    }
    // Fixed member order (manifest first, so a streaming reader verifies it
    // before touching any payload), not sorted.
    let kup = tar_bytes_ordered(&kup_members, epoch)?;
    let kup_path = out.join(format!("kamoder-{version}-update.kup"));
    std::fs::write(&kup_path, &kup)?;
    check_kup(&kup, &keys)?;

    println!("\n{}", kup_path.display());
    println!(
        "  version {version}  channel {}  seq {seq}",
        channel_str(channel)
    );
    for c in &manifest.components {
        println!(
            "  {:<5} {:<9} {:<15} {:>9} bytes  {:>4} files  {}",
            c.name,
            c.kind.as_str(),
            c.path,
            c.size,
            c.files.len(),
            c.sha256
        );
    }
    println!("  {:<15} {:>9} bytes", "total", kup.len());
    println!("{}", manifest_path.display());
    println!(
        "{}  (trusted comment: {})",
        sig_path.display(),
        trusted_comment(&manifest)
    );
    println!("verified: signature, schema, every payload and file digest");
    Ok(())
}

fn channel_str(c: Channel) -> &'static str {
    match c {
        Channel::Stable => "stable",
        Channel::Beta => "beta",
    }
}

/// One regular file of an archive.
#[derive(Clone, Debug)]
pub(crate) struct TarFile {
    pub path: String,
    pub data: Vec<u8>,
    pub mode: u32,
    /// Where it came from, for strip and error messages.
    source: PathBuf,
}

impl TarFile {
    fn read(source: &Path, path: &str) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(source)
            .wrap_err_with(|| format!("cannot read {}", source.display()))?;
        Ok(TarFile {
            path: path.to_owned(),
            data: std::fs::read(source)
                .wrap_err_with(|| format!("cannot read {}", source.display()))?,
            mode: normalized_mode(meta.permissions().mode()),
            source: source.to_owned(),
        })
    }

    pub(crate) fn bytes(path: &str, data: Vec<u8>, mode: u32) -> Self {
        TarFile {
            path: path.to_owned(),
            data,
            mode,
            source: PathBuf::from(path),
        }
    }

    fn entry(&self) -> FileEntry {
        FileEntry {
            path: self.path.clone(),
            size: self.data.len() as u64,
            sha256: sha256(&self.data),
            mode: self.mode,
        }
    }
}

/// 0755 if any execute bit is set, else 0644: keeps archives independent of
/// the build machine's umask.
fn normalized_mode(mode: u32) -> u32 {
    if mode & 0o111 != 0 {
        0o755
    } else {
        0o644
    }
}

/// Every regular file under `dir`, path relative to `base`. Links and
/// special files are refused: the slot is unpacked onto vfat.
fn collect(base: &Path, dir: &Path, out: &mut Vec<TarFile>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).wrap_err_with(|| format!("cannot read {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(base, &path, out)?;
        } else if kind.is_file() {
            let rel = path
                .strip_prefix(base)
                .expect("walked from base")
                .to_str()
                .ok_or_else(|| eyre!("non-UTF-8 path {}", path.display()))?
                .to_owned();
            out.push(TarFile::read(&path, &rel)?);
        } else {
            bail!("{} is not a regular file or directory", path.display());
        }
    }
    Ok(())
}

/// `strip -o` into the work dir; returns the stripped bytes.
fn stripped(strip: &Path, source: &Path, work: &Path) -> Result<Vec<u8>> {
    let name = source.file_name().ok_or_else(|| eyre!("unnamed binary"))?;
    let destination = work.join(name);
    let status = Command::new(strip)
        .arg("-o")
        .arg(&destination)
        .arg(source)
        .status()
        .wrap_err("cannot run strip")?;
    if !status.success() {
        bail!("strip {} failed with {status}", source.display());
    }
    Ok(std::fs::read(&destination)?)
}

/// Board profile names from the staged `boards/*.toml` (the file stem, which
/// `./x check-boards` keeps equal to the profile's `name`).
fn board_names(dist: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in
        std::fs::read_dir(dist.join("boards")).wrap_err("dist has no boards/; rerun ./x dist")?
    {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "toml") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.to_owned());
            }
        }
    }
    if names.is_empty() {
        bail!("no board profiles in {}/boards", dist.display());
    }
    names.sort();
    Ok(names)
}

/// The boot-shim files, or `None` (with a warning) when kboot was not built.
///
/// kboot and its runtime come from, in order: `--boot-dir` (laid out like
/// the card's `boot/`: `bin/kboot`, `lib/sys/`), the dist tree's own
/// `boot/`, or the dist tree's `bin/kboot` + `lib/sys/`.
fn boot_files(
    root: &Path,
    dist: &Path,
    boot_dir: Option<&Path>,
    strip: Option<&Path>,
    work: &Path,
) -> Result<Option<Vec<TarFile>>> {
    let (kboot, sys) = match boot_dir {
        Some(dir) => (dir.join("bin/kboot"), dir.join("lib/sys")),
        None if dist.join("boot/bin/kboot").is_file() => {
            (dist.join("boot/bin/kboot"), dist.join("boot/lib/sys"))
        }
        None => (dist.join("bin/kboot"), dist.join("lib/sys")),
    };
    if !kboot.is_file() {
        if boot_dir.is_some() {
            bail!("{} not found", kboot.display());
        }
        eprintln!(
            "warning: {} not found: the package has no boot-shim component (build kboot, or --no-boot)",
            kboot.display()
        );
        return Ok(None);
    }
    let mut files = Vec::new();
    let mut kboot_file = TarFile::read(&kboot, "boot/bin/kboot")?;
    if let Some(strip) = strip {
        kboot_file.data = stripped(strip, &kboot, work)?;
    }
    files.push(kboot_file);
    let mut libs = Vec::new();
    collect(&sys, &sys, &mut libs)?;
    for mut lib in libs {
        lib.path = format!("boot/lib/sys/{}", lib.path);
        files.push(lib);
    }
    files.push(TarFile::read(
        &root.join("card/Factory/config.sh"),
        "factory/config.sh",
    )?);
    Ok(Some(files))
}

/// Reproducible tar of `files`, sorted by path (see the module docs).
pub(crate) fn tar_bytes(files: &[TarFile], mtime: u64) -> Result<Vec<u8>> {
    let mut sorted: Vec<&TarFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    for pair in sorted.windows(2) {
        if pair[0].path == pair[1].path {
            bail!("duplicate archive path {}", pair[0].path);
        }
    }
    tar_write(sorted.into_iter(), mtime)
}

/// Reproducible tar of `files` in the given order.
fn tar_bytes_ordered(files: &[TarFile], mtime: u64) -> Result<Vec<u8>> {
    tar_write(files.iter(), mtime)
}

fn tar_write<'a>(files: impl Iterator<Item = &'a TarFile>, mtime: u64) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    for f in files {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(f.data.len() as u64);
        header.set_mode(f.mode);
        header.set_mtime(mtime);
        header.set_uid(0);
        header.set_gid(0);
        builder
            .append_data(&mut header, &f.path, f.data.as_slice())
            .wrap_err_with(|| format!("cannot archive {}", f.path))?;
    }
    Ok(builder.into_inner()?)
}

/// gzip, level 9, no name, zero mtime.
pub(crate) fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut gz = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::best());
    gz.write_all(data)?;
    Ok(gz.finish()?)
}

fn sha256(data: &[u8]) -> Digest {
    Digest(Sha256::digest(data).into())
}

/// Runs `minisign -S` over `manifest`, writing `sig`.
fn sign(
    key: &Path,
    manifest: &Path,
    sig: &Path,
    comment: &str,
    password: Option<String>,
) -> Result<()> {
    let minisign = crate::tasks::which("minisign").ok_or_else(|| {
        eyre!("minisign is not installed (Arch: pacman -S minisign; Debian/Ubuntu: apt install minisign)")
    })?;
    if !key.is_file() {
        bail!("signing key {} not found", key.display());
    }
    eprintln!(
        "$ minisign -S -s {} -m {} -t {comment:?}",
        key.display(),
        manifest.display()
    );
    let mut child = Command::new(minisign)
        .arg("-S")
        .arg("-s")
        .arg(key)
        .arg("-m")
        .arg(manifest)
        .arg("-x")
        .arg(sig)
        .arg("-t")
        .arg(comment)
        .stdin(if password.is_some() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stdout(Stdio::null())
        .spawn()
        .wrap_err("cannot run minisign")?;
    if let Some(password) = password {
        let mut stdin = child.stdin.take().expect("piped");
        writeln!(stdin, "{password}")?;
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("minisign failed with {status} (wrong {PASSWORD_ENV}, or an encrypted key without one?)");
    }
    Ok(())
}

/// Reads the finished `.kup` back the way the device will: manifest first,
/// signature, then every payload against its size and digest and every file
/// inside against the manifest's list.
fn check_kup(kup: &[u8], keys: &TrustedKeys) -> Result<()> {
    let mut members = BTreeMap::new();
    let mut order = Vec::new();
    let mut archive = tar::Archive::new(kup);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        order.push(path.clone());
        members.insert(path, data);
    }
    if order.first().map(String::as_str) != Some(MANIFEST) {
        bail!(".kup does not start with {MANIFEST}: {order:?}");
    }
    let sig = String::from_utf8(members[SIGNATURE].clone())?;
    let manifest = ota_manifest::verify(&members[MANIFEST], &sig, keys)
        .map_err(|e| eyre!(".kup manifest: {e}"))?;
    for c in &manifest.components {
        let gz = members
            .get(&c.path)
            .ok_or_else(|| eyre!("component {} payload {} missing", c.name, c.path))?;
        if gz.len() as u64 != c.size || sha256(gz) != c.sha256 {
            bail!("component {}: payload size/digest mismatch", c.name);
        }
        let mut listed: BTreeMap<&str, &FileEntry> =
            c.files.iter().map(|f| (f.path.as_str(), f)).collect();
        let mut inner = tar::Archive::new(flate2::read::GzDecoder::new(gz.as_slice()));
        for entry in inner.entries()? {
            let mut entry = entry?;
            if entry.header().entry_type() != tar::EntryType::Regular {
                bail!("component {}: non-regular entry", c.name);
            }
            let path = entry.path()?.to_string_lossy().into_owned();
            let mode = entry.header().mode()?;
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            let want = listed
                .remove(path.as_str())
                .ok_or_else(|| eyre!("component {}: {path} not in the manifest", c.name))?;
            if want.size != data.len() as u64 || want.sha256 != sha256(&data) || want.mode != mode {
                bail!("component {}: {path} does not match the manifest", c.name);
            }
        }
        if let Some(missing) = listed.keys().next() {
            bail!(
                "component {}: {missing} listed but not in the payload",
                c.name
            );
        }
    }
    Ok(())
}

/// What git says about HEAD; every field optional (a source tarball has no
/// `.git`).
struct Git {
    describe: Option<String>,
    commit_time: Option<u64>,
}

impl Git {
    fn read(root: &Path) -> Self {
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(root)
                .stderr(Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        Git {
            describe: run(&["describe", "--tags", "--match", "v[0-9]*"]),
            commit_time: run(&["log", "-1", "--format=%ct"]).and_then(|s| s.parse().ok()),
        }
    }
}

/// `git describe` turned into semver, else the workspace version.
///
/// On a tag: the tag without `v`. Past a tag (`v0.2.0-3-gabc1234`): a
/// pre-release of it, `0.2.0-dev.3.gabc1234` (or `0.2.0-rc.1.dev.3.g…` past
/// an rc tag), so local builds land on the beta channel. No `+build`
/// metadata: GitHub rewrites `+` in asset names. Release CI passes
/// `--version` explicitly.
fn default_version(root: &Path, git: &Git) -> Result<Version> {
    if let Some(d) = &git.describe {
        if let Some(v) = describe_to_semver(d) {
            return Ok(v);
        }
        eprintln!("warning: git describe {d:?} is not semver; using the workspace version");
    }
    #[derive(serde::Deserialize)]
    struct CargoToml {
        workspace: Workspace,
    }
    #[derive(serde::Deserialize)]
    struct Workspace {
        package: Package,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        version: String,
    }
    let text = std::fs::read_to_string(root.join("Cargo.toml"))?;
    let cargo: CargoToml = toml::from_str(&text).wrap_err("Cargo.toml [workspace.package]")?;
    Ok(Version::parse(&cargo.workspace.package.version)?)
}

fn describe_to_semver(describe: &str) -> Option<Version> {
    let d = describe.strip_prefix('v')?;
    // "<tag>-<n>-g<hash>" when past the tag.
    let parts: Vec<&str> = d.rsplitn(3, '-').collect();
    if let [hash, n, tag] = parts[..] {
        if hash.starts_with('g') && n.bytes().all(|b| b.is_ascii_digit()) {
            let base = Version::parse(tag).ok()?;
            let sep = if base.pre.is_empty() { "-" } else { "." };
            return Version::parse(&format!("{tag}{sep}dev.{n}.{hash}")).ok();
        }
    }
    Version::parse(d).ok()
}

fn source_date_epoch() -> Result<Option<u64>> {
    match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(v) if !v.is_empty() => Ok(Some(
            v.parse()
                .wrap_err_with(|| format!("SOURCE_DATE_EPOCH {v:?}"))?,
        )),
        _ => Ok(None),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `owner/repo` for the release notes link: `$GITHUB_REPOSITORY` in CI,
/// else cliff.toml's `[remote.github]`.
fn repo(root: &Path) -> Option<String> {
    if let Ok(r) = std::env::var("GITHUB_REPOSITORY") {
        if !r.is_empty() {
            return Some(r);
        }
    }
    #[derive(serde::Deserialize)]
    struct Cliff {
        remote: Remote,
    }
    #[derive(serde::Deserialize)]
    struct Remote {
        github: Github,
    }
    #[derive(serde::Deserialize)]
    struct Github {
        owner: String,
        repo: String,
    }
    let text = std::fs::read_to_string(root.join("cliff.toml")).ok()?;
    let cliff: Cliff = toml::from_str(&text).ok()?;
    Some(format!(
        "{}/{}",
        cliff.remote.github.owner, cliff.remote.github.repo
    ))
}

/// Unix seconds as RFC 3339 UTC (`1970-01-01T00:00:00Z`), via the
/// days-to-civil algorithm (H. Hinnant), to avoid a date crate for one line.
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<TarFile> {
        vec![
            TarFile::bytes("www/index.html", b"<html>".to_vec(), 0o644),
            TarFile::bytes("bin/web", vec![0x7f, b'E', b'L', b'F', 1, 2, 3], 0o755),
            TarFile::bytes(&format!("lib/{}", "x".repeat(120)), b"long".to_vec(), 0o644),
        ]
    }

    #[test]
    fn payload_is_reproducible() {
        let a = gzip(&tar_bytes(&files(), 1_760_700_000).unwrap()).unwrap();
        let mut reversed = files();
        reversed.reverse();
        let b = gzip(&tar_bytes(&reversed, 1_760_700_000).unwrap()).unwrap();
        assert_eq!(a, b, "same files in any order give the same bytes");
        let c = gzip(&tar_bytes(&files(), 1).unwrap()).unwrap();
        assert_ne!(a, c, "mtime is part of the archive");
    }

    #[test]
    fn payload_is_sorted_regular_files_owned_by_root() {
        let tar = tar_bytes(&files(), 42).unwrap();
        let mut archive = tar::Archive::new(tar.as_slice());
        let mut paths = Vec::new();
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let h = entry.header();
            assert_eq!(h.entry_type(), tar::EntryType::Regular);
            assert_eq!(
                (h.uid().unwrap(), h.gid().unwrap(), h.mtime().unwrap()),
                (0, 0, 42)
            );
            paths.push(entry.path().unwrap().to_string_lossy().into_owned());
        }
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
        assert!(paths[1].len() > 100, "long names survive");
    }

    #[test]
    fn duplicate_paths_are_refused() {
        let mut f = files();
        f.push(f[0].clone());
        assert!(tar_bytes(&f, 0).is_err());
    }

    #[test]
    fn modes_are_normalized() {
        assert_eq!(normalized_mode(0o100775), 0o755);
        assert_eq!(normalized_mode(0o100700), 0o755);
        assert_eq!(normalized_mode(0o100664), 0o644);
        assert_eq!(normalized_mode(0o100600), 0o644);
    }

    #[test]
    fn rfc3339_matches_date() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_760_700_000), "2025-10-17T11:20:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn describe_becomes_semver() {
        let v = |s: &str| describe_to_semver(s).map(|v| v.to_string());
        assert_eq!(v("v0.2.0").as_deref(), Some("0.2.0"));
        assert_eq!(v("v0.2.0-rc.1").as_deref(), Some("0.2.0-rc.1"));
        assert_eq!(
            v("v0.2.0-3-gabc1234").as_deref(),
            Some("0.2.0-dev.3.gabc1234")
        );
        assert_eq!(
            v("v0.1.0-rc.2-14-g4449110").as_deref(),
            Some("0.1.0-rc.2.dev.14.g4449110")
        );
        assert_eq!(v("nightly"), None);
    }
}
