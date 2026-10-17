//! `SdDirStore`: slots as directories on the vfat SD card (design §5,
//! OTA-06, backend `sd-dirs`).
//!
//! ```text
//! payload.tar.gz ──gunzip──> tar entries ──> slots/<x>.tmp/<path>   (sha256 per file, fsync)
//!                                        all manifest files present? component sha256 ok?
//!                                        + manifest.json(.minisig) copy, VERSION
//! rm -r slots/<x>;  mv slots/<x>.tmp slots/<x>;  fsync slots/
//! ```
//!
//! Every entry must be a regular file or a directory (vfat has no links, and
//! a link in a package is either a builder bug or an attack), its path a
//! plain relative path ([`ota_manifest::validate_rel_path`]), and every file
//! must be listed in the manifest with the same size; its SHA-256 is checked
//! while writing. Unknown, duplicate or missing files fail the install.
//!
//! The rename at the end is not atomic on vfat (remove, then rename), which
//! is fine: the slot is invalid in the boot record for the whole install, so
//! a power cut at any point leaves a slot that is never booted, and the next
//! install starts over (a stale `.tmp` is removed by [`SlotStore::begin`]).
//!
//! The store never writes the data root (legacy slot `a`) nor the booted
//! slot's directory: installs go to `slots/<x>` only, and `begin` refuses a
//! target that is, or resolves to, the booted slot's directory.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use bootctl::Slot;
use ota_manifest::{Component, ComponentKind, FileEntry};
use snafu::{ensure, ResultExt};

use crate::kup::{ComponentReader, VerifiedManifest, MANIFEST_NAME, SIGNATURE_NAME};
use crate::layout::Layout;
use crate::progress::Reporter;
use crate::shim;
use crate::store::{
    check_entry_type, create_dirs, entry_path, sync_dir, write_new_file, write_small_file,
    BootedSlotSnafu, Error, FileHashSnafu, FileSizeSnafu, IoSnafu, NoSlotComponentSnafu,
    NoSpaceSnafu, ReservedPathSnafu, Result, SlotStore, COPY_BUF,
};

/// Free space kept on top of the payload size (vfat cluster slack,
/// directory entries, the manifest copy).
pub const SPACE_MARGIN: u64 = 1024 * 1024;

/// Name of the version file in a slot.
pub const VERSION_FILE: &str = "VERSION";

/// Files the installer writes itself into every slot.
const RESERVED: [&str; 2] = [MANIFEST_NAME, SIGNATURE_NAME];

/// Slot directories under `<data root>/slots/`, plus the boot shim.
#[derive(Debug)]
pub struct SdDirStore {
    layout: Layout,
    booted: Slot,
    tree_installed: bool,
    shim_replaced: Vec<String>,
}

impl SdDirStore {
    /// A store for the card at `layout`, running from `booted`.
    pub fn new(layout: Layout, booted: Slot) -> SdDirStore {
        SdDirStore {
            layout,
            booted,
            tree_installed: false,
            shim_replaced: Vec::new(),
        }
    }

    /// The card layout.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Boot-shim files (manifest paths) the last install replaced.
    pub fn shim_replaced(&self) -> &[String] {
        &self.shim_replaced
    }

    fn refuse_booted(&self, slot: Slot) -> Result<()> {
        ensure!(slot != self.booted, BootedSlotSnafu { slot });
        let target = self.layout.install_dir(slot);
        let booted = self.layout.slot_dir(self.booted);
        let same = target == booted
            || matches!(
                (fs::canonicalize(&target), fs::canonicalize(&booted)),
                (Ok(a), Ok(b)) if a == b
            );
        ensure!(
            !same && target != self.layout.data_root(),
            BootedSlotSnafu { slot }
        );
        Ok(())
    }

    fn extract_tree(
        &mut self,
        staging: &Path,
        component: &Component,
        reader: &mut ComponentReader<'_>,
        progress: &mut Reporter<'_>,
    ) -> Result<()> {
        let result = extract_into(staging, component, reader, progress);
        let result = match result {
            // A decoder error caused by the integrity check underneath is
            // reported as that (truncated / wrong component digest).
            Err(Error::Payload { .. }) if reader.failure().is_some() => {
                Err(Error::from(reader.failure().expect("checked")))
            }
            other => other,
        };
        result?;
        reader.finish()?;
        self.tree_installed = true;
        Ok(())
    }
}

/// Streams `component` (gzip tar) into `root`, checking it against the
/// manifest's file list. `root` must exist and be empty.
fn extract_into(
    root: &Path,
    component: &Component,
    reader: &mut ComponentReader<'_>,
    progress: &mut Reporter<'_>,
) -> Result<()> {
    let name = component.name.as_str();
    let expected: HashMap<&str, &FileEntry> = component
        .files
        .iter()
        .map(|f| (f.path.as_str(), f))
        .collect();
    let mut written: HashSet<String> = HashSet::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut buf = vec![0u8; COPY_BUF];
    let payload_err = |source: io::Error| Error::Payload {
        component: name.to_owned(),
        source,
    };

    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(reader));
    for entry in archive.entries().map_err(payload_err)? {
        let mut entry = entry.map_err(payload_err)?;
        let kind = entry.header().entry_type();
        let shown = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        check_entry_type(kind, name, &shown)?;
        let path = entry_path(&entry, name)?;
        let dst = root.join(&path);
        if kind.is_dir() {
            create_dirs(&dst, &mut dirs)?;
            continue;
        }
        let Some(file) = expected.get(path.as_str()) else {
            return crate::store::UnknownFileSnafu {
                component: name,
                path,
            }
            .fail();
        };
        if !written.insert(path.clone()) {
            return crate::store::DuplicateFileSnafu {
                component: name,
                path,
            }
            .fail();
        }
        ensure!(
            entry.size() == file.size,
            FileSizeSnafu {
                component: name,
                path: &path,
                expected: file.size,
                found: entry.size(),
            }
        );
        if let Some(parent) = dst.parent() {
            create_dirs(parent, &mut dirs)?;
        }
        progress.file(Some(&path));
        let found = write_new_file(
            &mut entry,
            &dst,
            file.size,
            file.mode,
            (name, &path),
            &mut buf,
            progress,
        )?;
        ensure!(
            found == file.sha256,
            FileHashSnafu {
                component: name,
                path: &path,
                expected: file.sha256,
                found,
            }
        );
    }
    drop(archive);

    if let Some(missing) = component.files.iter().find(|f| !written.contains(&f.path)) {
        return crate::store::MissingFileSnafu {
            component: name,
            path: &missing.path,
        }
        .fail();
    }
    // Deepest first, then the root, so every new entry is on disk.
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in &dirs {
        sync_dir(d)?;
    }
    sync_dir(root)
}

/// `f_bavail * f_frsize` of the filesystem holding `path`.
pub(crate) fn free_space(path: &Path) -> Result<u64> {
    let st = nix::sys::statvfs::statvfs(path).map_err(|e| Error::Io {
        action: "statvfs",
        path: path.to_path_buf(),
        source: io::Error::from(e),
    })?;
    #[allow(clippy::useless_conversion, reason = "field widths differ per libc")]
    let free = u64::from(st.blocks_available()) * u64::from(st.fragment_size());
    Ok(free)
}

/// Bytes the install writes to the card: every archive file (slot files in
/// staging, shim files as `.new` copies).
fn space_needed(components: &[Component]) -> u64 {
    components
        .iter()
        .filter(|c| matches!(c.kind, ComponentKind::SdTree | ComponentKind::BootShim))
        .flat_map(|c| &c.files)
        .map(|f| f.size)
        .sum::<u64>()
        + SPACE_MARGIN
}

impl SlotStore for SdDirStore {
    fn booted(&self) -> Slot {
        self.booted
    }

    fn handles(&self, kind: &ComponentKind) -> bool {
        matches!(kind, ComponentKind::SdTree | ComponentKind::BootShim)
    }

    fn begin(&mut self, slot: Slot, manifest: &VerifiedManifest) -> Result<()> {
        self.refuse_booted(slot)?;
        self.tree_installed = false;
        self.shim_replaced.clear();
        let m = &manifest.manifest;
        ensure!(
            m.component(&ComponentKind::SdTree).is_some(),
            NoSlotComponentSnafu
        );
        for c in m
            .components
            .iter()
            .filter(|c| c.kind == ComponentKind::SdTree)
        {
            if let Some(f) = c.files.iter().find(|f| RESERVED.contains(&f.path.as_str())) {
                return ReservedPathSnafu {
                    component: &c.name,
                    path: &f.path,
                }
                .fail();
            }
        }

        let slots = self.layout.slots_dir();
        create_dirs(&slots, &mut Vec::new())?;
        let need = space_needed(&m.components);
        let available = free_space(&slots)?;
        ensure!(available >= need, NoSpaceSnafu { need, available });

        let staging = self.layout.staging_dir(slot);
        if staging.exists() {
            tracing::info!(path = %staging.display(), "removing stale staging directory");
            fs::remove_dir_all(&staging).context(IoSnafu {
                action: "remove",
                path: &staging,
            })?;
        }
        fs::create_dir(&staging).context(IoSnafu {
            action: "create directory",
            path: &staging,
        })?;
        sync_dir(&slots)
    }

    fn install_component(
        &mut self,
        slot: Slot,
        component: &Component,
        reader: &mut ComponentReader<'_>,
        progress: &mut Reporter<'_>,
    ) -> Result<()> {
        match component.kind {
            ComponentKind::SdTree => {
                let staging = self.layout.staging_dir(slot);
                self.extract_tree(&staging, component, reader, progress)
            }
            ComponentKind::BootShim => {
                let replaced = shim::install_boot_shim(&self.layout, component, reader, progress)?;
                self.shim_replaced.extend(replaced);
                Ok(())
            }
            ComponentKind::MtdImage { .. } => crate::store::UnsupportedComponentSnafu {
                name: &component.name,
                kind: component.kind.as_str(),
            }
            .fail(),
        }
    }

    fn finish(&mut self, slot: Slot, manifest: &VerifiedManifest) -> Result<()> {
        ensure!(self.tree_installed, NoSlotComponentSnafu);
        let m = &manifest.manifest;
        let staging = self.layout.staging_dir(slot);
        write_small_file(&staging.join(MANIFEST_NAME), &manifest.json)?;
        write_small_file(&staging.join(SIGNATURE_NAME), manifest.signature.as_bytes())?;
        let tree = m
            .component(&ComponentKind::SdTree)
            .expect("checked in begin");
        // A VERSION shipped in the payload is signed content; keep it.
        if !tree.files.iter().any(|f| f.path == VERSION_FILE) {
            write_small_file(
                &staging.join(VERSION_FILE),
                format!("{}\n", m.version).as_bytes(),
            )?;
        }
        sync_dir(&staging)?;

        let target = self.layout.install_dir(slot);
        if target.exists() {
            fs::remove_dir_all(&target).context(IoSnafu {
                action: "remove old slot",
                path: &target,
            })?;
        }
        fs::rename(&staging, &target).context(IoSnafu {
            action: "rename staging to",
            path: &target,
        })?;
        sync_dir(&self.layout.slots_dir())
    }

    fn abort(&mut self, slot: Slot) {
        let staging = self.layout.staging_dir(slot);
        if let Err(e) = fs::remove_dir_all(&staging) {
            if e.kind() != io::ErrorKind::NotFound {
                tracing::warn!(path = %staging.display(), error = %e, "cannot remove staging directory");
            }
        }
        self.tree_installed = false;
    }
}

#[cfg(test)]
mod tests;
