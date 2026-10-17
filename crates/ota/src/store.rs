//! The [`SlotStore`] trait: how a verified package is written into the
//! inactive slot (design §5), plus the shared error type and file helpers.
//!
//! ```text
//! install_to_inactive(control, store, kup, manifest)
//!   slot = store.inactive()
//!   control.invalidate(slot)         <- slot can no longer be booted
//!   store.install(slot, …)
//!     begin(slot)                    <- refuse booted slot, space check, staging
//!     install_component(…) × N       <- slot components, then the boot shim
//!     finish(slot)                   <- swap staging into place, VERSION, manifest copy
//!     (abort(slot) on any error)
//!   control.activate(slot, version, 3)   <- trial boot (OTA-08)
//! ```
//!
//! **Invalidate first.** A store writes a slot non-atomically (vfat has no
//! atomic directory replace; NOR is erased block by block), so the boot
//! record must say "not bootable" before the first byte changes. Callers that
//! drive a store by hand must call [`bootctl::BootControl::invalidate`]
//! themselves; [`install_to_inactive`] does the whole sequence. A failed or
//! interrupted install leaves the slot invalid and the booted slot untouched
//! (OTA-06).
//!
//! Backends: [`crate::sd::SdDirStore`] (`slots/<x>` on the SD card, plus the
//! boot shim) and [`crate::mtd::MtdStore`] (raw images on NOR partitions,
//! P2). A component kind no backend handles fails the install up front.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use bootctl::{BootControl, Slot};
use ota_manifest::{Component, ComponentKind, Digest, PathError};
use ring::digest::{Context, SHA256};
use snafu::{ensure, ResultExt, Snafu};

use crate::kup::{self, digest_of, ComponentReader, Kup, VerifiedManifest};
use crate::mtd;
use crate::progress::{manifest_total, Progress, Reporter};

/// Boot attempts a freshly installed slot gets before kboot rolls back.
pub const INSTALL_TRIES: u8 = 3;

/// Copy buffer for payload files (heap, one per install).
pub(crate) const COPY_BUF: usize = 64 * 1024;

/// Errors of an install.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("{source}"))]
    Kup { source: kup::Error },
    #[snafu(display("{action} {}: {source}", path.display()))]
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[snafu(display("refusing to install into slot {slot}: it is the booted slot"))]
    BootedSlot { slot: Slot },
    #[snafu(display("not enough space on the card: need {need} bytes, {available} free"))]
    NoSpace { need: u64, available: u64 },
    #[snafu(display("no installer for component {name:?} of kind {kind}"))]
    UnsupportedComponent { name: String, kind: &'static str },
    #[snafu(display("package has no slot (sd-tree) component"))]
    NoSlotComponent,
    #[snafu(display("component {component:?}: payload is corrupt: {source}"))]
    Payload {
        component: String,
        source: io::Error,
    },
    #[snafu(display(
        "component {component:?}: {path:?} is a {kind}, only files and directories are allowed"
    ))]
    EntryType {
        component: String,
        path: String,
        kind: String,
    },
    #[snafu(display("component {component:?}: unsafe path {path:?}: {source}"))]
    BadPath {
        component: String,
        path: String,
        source: PathError,
    },
    #[snafu(display("component {component:?}: {path:?} is reserved for the installer"))]
    ReservedPath { component: String, path: String },
    #[snafu(display("component {component:?}: {path:?} is not listed in the manifest"))]
    UnknownFile { component: String, path: String },
    #[snafu(display("component {component:?}: {path:?} appears twice in the payload"))]
    DuplicateFile { component: String, path: String },
    #[snafu(display(
        "component {component:?}: {path:?} is {found} bytes, manifest says {expected}"
    ))]
    FileSize {
        component: String,
        path: String,
        expected: u64,
        found: u64,
    },
    #[snafu(display(
        "component {component:?}: {path:?} sha256 {found} does not match manifest {expected}"
    ))]
    FileHash {
        component: String,
        path: String,
        expected: Digest,
        found: Digest,
    },
    #[snafu(display(
        "component {component:?}: {path:?} is listed in the manifest but missing from the payload"
    ))]
    MissingFile { component: String, path: String },
    #[snafu(display("boot-shim file {path:?} must be under boot/ or factory/"))]
    ShimPath { path: String },
    #[snafu(display("data root {} has no parent directory for Factory/", path.display()))]
    NoSdRoot { path: PathBuf },
    #[snafu(display("MTD: {source}"))]
    Mtd { source: mtd::Error },
    #[snafu(display(
        "partition {name:?} belongs to no slot (use a {{slot}} placeholder or an _A/_B suffix)"
    ))]
    SharedPartition { name: String },
    #[snafu(display("image for {partition:?} is {size} bytes, the partition holds {capacity}"))]
    ImageTooLarge {
        partition: String,
        size: u64,
        capacity: u64,
    },
    #[snafu(display("flash {partition:?}: {action} at {offset:#x}: {source}"))]
    Flash {
        partition: String,
        action: &'static str,
        offset: u64,
        source: io::Error,
    },
    #[snafu(display("flash {partition:?}: read-back sha256 {found} does not match {expected}"))]
    Verify {
        partition: String,
        expected: Digest,
        found: Digest,
    },
    #[snafu(display("boot control: {source}"))]
    Boot { source: bootctl::Error },
}

/// Result alias for installs.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<kup::Error> for Error {
    fn from(source: kup::Error) -> Self {
        Error::Kup { source }
    }
}

/// A place slots are written to (design §5).
///
/// The lifecycle is `begin` → `install_component` for every component →
/// `finish`, with `abort` on failure; [`SlotStore::install`] drives it.
/// Implementations must never write into [`SlotStore::booted`].
pub trait SlotStore {
    /// The slot the device is running from.
    fn booted(&self) -> Slot;

    /// The slot an update goes to.
    fn inactive(&self) -> Slot {
        self.booted().other()
    }

    /// Whether this store installs components of `kind`.
    fn handles(&self, kind: &ComponentKind) -> bool;

    /// Prepares `slot` (refuses the booted slot, checks space, creates
    /// staging). The slot must already be invalid in the boot record.
    fn begin(&mut self, slot: Slot, manifest: &VerifiedManifest) -> Result<()>;

    /// Installs one component from `reader`. The implementation must call
    /// [`ComponentReader::finish`] before relying on the data being
    /// authentic (it may skip a component that is not for `slot`).
    fn install_component(
        &mut self,
        slot: Slot,
        component: &Component,
        reader: &mut ComponentReader<'_>,
        progress: &mut Reporter<'_>,
    ) -> Result<()>;

    /// Makes the installed slot complete (only after every component
    /// succeeded).
    fn finish(&mut self, slot: Slot, manifest: &VerifiedManifest) -> Result<()>;

    /// Cleans up after a failed install (best effort; the slot stays invalid).
    fn abort(&mut self, slot: Slot);

    /// Installs every component of `manifest` from `kup` into `slot`:
    /// slot components in manifest order, then boot-shim components (so a
    /// bad payload never touches the shared boot files), then `finish`.
    fn install(
        &mut self,
        slot: Slot,
        kup: &Kup,
        manifest: &VerifiedManifest,
        progress: &mut dyn FnMut(&Progress),
    ) -> Result<()> {
        ensure!(slot != self.booted(), BootedSlotSnafu { slot });
        let m = &manifest.manifest;
        if let Some(c) = m.components.iter().find(|c| !self.handles(&c.kind)) {
            return UnsupportedComponentSnafu {
                name: &c.name,
                kind: c.kind.as_str(),
            }
            .fail();
        }
        let mut ordered: Vec<&Component> = m.components.iter().collect();
        ordered.sort_by_key(|c| matches!(c.kind, ComponentKind::BootShim));

        let mut reporter = Reporter::new(manifest_total(m), progress);
        self.begin(slot, manifest)?;
        let result = (|| {
            for c in ordered {
                reporter.component(&c.name);
                let mut reader = kup.component_reader(c)?;
                self.install_component(slot, c, &mut reader, &mut reporter)?;
            }
            self.finish(slot, manifest)
        })();
        match result {
            Ok(()) => {
                reporter.file(None);
                tracing::info!(slot = %slot, version = %m.version, "slot installed");
                Ok(())
            }
            Err(e) => {
                tracing::warn!(slot = %slot, error = %e, "install failed, slot left invalid");
                self.abort(slot);
                Err(e)
            }
        }
    }
}

/// The whole install sequence against a boot record: invalidate the
/// inactive slot, install, then activate it as a trial with
/// [`INSTALL_TRIES`] boots. Returns the installed slot. On error the slot
/// stays invalid and the booted slot remains the one that boots.
pub fn install_to_inactive<C, S>(
    control: &mut C,
    store: &mut S,
    kup: &Kup,
    manifest: &VerifiedManifest,
    progress: &mut dyn FnMut(&Progress),
) -> Result<Slot>
where
    C: BootControl + ?Sized,
    S: SlotStore + ?Sized,
{
    let slot = store.inactive();
    control.invalidate(slot).context(BootSnafu)?;
    store.install(slot, kup, manifest, progress)?;
    let version = manifest.manifest.version.to_string();
    control
        .activate(slot, &version, INSTALL_TRIES)
        .context(BootSnafu)?;
    Ok(slot)
}

/// `fsync` on a directory, so renames and new entries in it are durable.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir).and_then(|d| d.sync_all()).context(IoSnafu {
        action: "sync directory",
        path: dir,
    })
}

/// Creates every missing directory from `dir` up, remembering the new ones
/// in `created` (they are fsynced at the end).
pub(crate) fn create_dirs(dir: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        create_dirs(parent, created)?;
    }
    match fs::create_dir(dir) {
        Ok(()) => {
            created.push(dir.to_path_buf());
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        Err(source) => Err(Error::Io {
            action: "create directory",
            path: dir.to_path_buf(),
            source,
        }),
    }
}

/// Copies exactly `size` bytes from `src` into a new file `dst` (created
/// with `mode`, must not exist), hashing on the way, then fsyncs it.
/// Read errors become [`Error::Payload`]; more or fewer bytes than `size`
/// become [`Error::FileSize`].
pub(crate) fn write_new_file(
    src: &mut dyn Read,
    dst: &Path,
    size: u64,
    mode: u32,
    names: (&str, &str),
    buf: &mut [u8],
    progress: &mut Reporter<'_>,
) -> Result<Digest> {
    let (component, path) = names;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(dst)
        .context(IoSnafu {
            action: "create",
            path: dst,
        })?;
    let mut ctx = Context::new(&SHA256);
    let mut done = 0u64;
    loop {
        let n = match src.read(buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(Error::Payload {
                    component: component.to_owned(),
                    source,
                })
            }
        };
        done += n as u64;
        ensure!(
            done <= size,
            FileSizeSnafu {
                component,
                path,
                expected: size,
                found: done,
            }
        );
        ctx.update(&buf[..n]);
        out.write_all(&buf[..n]).context(IoSnafu {
            action: "write",
            path: dst,
        })?;
        progress.advance(n as u64);
    }
    ensure!(
        done == size,
        FileSizeSnafu {
            component,
            path,
            expected: size,
            found: done,
        }
    );
    out.sync_all().context(IoSnafu {
        action: "sync",
        path: dst,
    })?;
    Ok(digest_of(ctx))
}

/// SHA-256 of everything `src` yields (an entry that is verified but not
/// written). Read errors become [`Error::Payload`].
pub(crate) fn hash_stream(
    src: &mut dyn Read,
    component: &str,
    buf: &mut [u8],
    progress: &mut Reporter<'_>,
) -> Result<(Digest, u64)> {
    let mut ctx = Context::new(&SHA256);
    let mut done = 0u64;
    loop {
        match src.read(buf) {
            Ok(0) => break,
            Ok(n) => {
                ctx.update(&buf[..n]);
                done += n as u64;
                progress.advance(n as u64);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(source) => {
                return Err(Error::Payload {
                    component: component.to_owned(),
                    source,
                })
            }
        }
    }
    Ok((digest_of(ctx), done))
}

/// SHA-256 of an installed file, `None` if it does not exist or is not a
/// regular file.
pub(crate) fn hash_file(path: &Path, buf: &mut [u8]) -> Result<Option<Digest>> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Io {
                action: "open",
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let meta = f.metadata().context(IoSnafu {
        action: "stat",
        path,
    })?;
    if !meta.is_file() {
        return Ok(None);
    }
    let mut ctx = Context::new(&SHA256);
    loop {
        let n = f.read(buf).context(IoSnafu {
            action: "read",
            path,
        })?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(Some(digest_of(ctx)))
}

/// Writes a small file the installer generates (VERSION, manifest copy):
/// create/truncate, write, fsync.
pub(crate) fn write_small_file(path: &Path, data: &[u8]) -> Result<()> {
    let mut f = File::create(path).context(IoSnafu {
        action: "create",
        path,
    })?;
    f.write_all(data)
        .and_then(|()| f.sync_all())
        .context(IoSnafu {
            action: "write",
            path,
        })
}

/// The path of a payload tar entry as a manifest-style string: `./` prefix
/// and trailing `/` dropped, then [`ota_manifest::validate_rel_path`].
pub(crate) fn entry_path<R: Read>(entry: &tar::Entry<'_, R>, component: &str) -> Result<String> {
    let raw = entry.path_bytes();
    let text = std::str::from_utf8(&raw).map_err(|_| Error::BadPath {
        component: component.to_owned(),
        path: String::from_utf8_lossy(&raw).into_owned(),
        source: PathError::Control,
    })?;
    let mut path = text.strip_prefix("./").unwrap_or(text);
    if entry.header().entry_type().is_dir() {
        path = path.strip_suffix('/').unwrap_or(path);
    }
    ota_manifest::validate_rel_path(path).context(BadPathSnafu { component, path })?;
    Ok(path.to_owned())
}

/// Rejects every tar entry type but regular files and directories.
pub(crate) fn check_entry_type(kind: tar::EntryType, component: &str, path: &str) -> Result<()> {
    if kind.is_file() || kind.is_dir() {
        return Ok(());
    }
    let name = match kind {
        tar::EntryType::Symlink => "symlink".to_owned(),
        tar::EntryType::Link => "hard link".to_owned(),
        tar::EntryType::Char => "character device".to_owned(),
        tar::EntryType::Block => "block device".to_owned(),
        tar::EntryType::Fifo => "fifo".to_owned(),
        other => format!("{other:?} entry"),
    };
    EntryTypeSnafu {
        component,
        path,
        kind: name,
    }
    .fail()
}
