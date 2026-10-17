//! Boot-shim component (`boot.tar.gz`, kind `boot-shim`; design §3, §5).
//!
//! The shim is what runs *before* any slot: `Factory/config.sh` (started by
//! the stock firmware) and `boot/bin/kboot` with its own `boot/lib/sys/`
//! libraries. It is shared by both slots, so it is updated in place, file by
//! file, and must stay backwards compatible with the slot it boots.
//!
//! Archive paths map onto the card like this (anything else is refused):
//!
//! ```text
//! boot/<rest>      ->  <data root>/boot/<rest>      e.g. boot/bin/kboot, boot/lib/sys/libc.so.0
//! factory/<rest>   ->  <sd root>/Factory/<rest>     e.g. factory/config.sh
//! ```
//!
//! (`factory/` is lower case in the archive so the payload layout does not
//! depend on vfat's case folding; the card directory is `Factory/` because
//! that is the name the stock firmware runs.)
//!
//! Only files whose SHA-256 differs from the installed copy are written.
//! Each is first written as `<file>.new` and fsynced while the stream is
//! checked; nothing is renamed until the whole component verified (every
//! file hash, no missing or unknown file, component SHA-256). Then the
//! renames go in dependency order — libraries, then the rest of `boot/`
//! (`kboot`), then `Factory/config.sh` last — each followed by an fsync of
//! its directory, so a power cut leaves either the old or the new file in
//! place, and a new `config.sh` never runs a `kboot` that is not there yet.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ota_manifest::{Component, Digest, FileEntry};
use snafu::{ensure, OptionExt, ResultExt};

use crate::kup::ComponentReader;
use crate::layout::Layout;
use crate::progress::Reporter;
use crate::store::{
    check_entry_type, create_dirs, entry_path, hash_file, hash_stream, sync_dir, write_new_file,
    DuplicateFileSnafu, Error, FileHashSnafu, FileSizeSnafu, IoSnafu, MissingFileSnafu,
    NoSdRootSnafu, Result, ShimPathSnafu, UnknownFileSnafu, COPY_BUF,
};

/// Archive prefix for files under `<data root>/boot/`.
pub const BOOT_PREFIX: &str = "boot/";
/// Archive prefix for files under `<sd root>/Factory/`.
pub const FACTORY_PREFIX: &str = "factory/";

/// Where one shim file goes and when it is renamed into place.
struct Target {
    dst: PathBuf,
    /// Rename order: 0 libraries, 1 other boot files, 2 Factory files.
    rank: u8,
    /// The installed copy already has the manifest's hash.
    unchanged: bool,
}

fn target_for(layout: &Layout, path: &str) -> Result<(PathBuf, u8)> {
    if let Some(rest) = path.strip_prefix(BOOT_PREFIX) {
        let rank = if rest.starts_with("lib/") { 0 } else { 1 };
        return Ok((layout.boot_dir().join(rest), rank));
    }
    if let Some(rest) = path.strip_prefix(FACTORY_PREFIX) {
        let factory = layout.factory_dir().context(NoSdRootSnafu {
            path: layout.data_root(),
        })?;
        return Ok((factory.join(rest), 2));
    }
    ShimPathSnafu { path }.fail()
}

fn new_path(dst: &Path) -> PathBuf {
    let mut s = dst.as_os_str().to_owned();
    s.push(".new");
    PathBuf::from(s)
}

/// Installs a boot-shim component; returns the manifest paths of the files
/// that were replaced, in rename order.
pub fn install_boot_shim(
    layout: &Layout,
    component: &Component,
    reader: &mut ComponentReader<'_>,
    progress: &mut Reporter<'_>,
) -> Result<Vec<String>> {
    let mut buf = vec![0u8; COPY_BUF];
    // Map and compare everything first: a bad path fails before any write.
    let mut targets: HashMap<&str, (Target, &FileEntry)> = HashMap::new();
    for f in &component.files {
        let (dst, rank) = target_for(layout, &f.path)?;
        let unchanged = hash_file(&dst, &mut buf)? == Some(f.sha256);
        targets.insert(
            f.path.as_str(),
            (
                Target {
                    dst,
                    rank,
                    unchanged,
                },
                f,
            ),
        );
    }

    let mut staged: Vec<(u8, String, PathBuf)> = Vec::new();
    let result = stage(component, reader, progress, &targets, &mut staged, &mut buf);
    let result = match result {
        Err(Error::Payload { .. }) if reader.failure().is_some() => {
            Err(Error::from(reader.failure().expect("checked")))
        }
        other => other,
    }
    .and_then(|()| reader.finish().map_err(Error::from));
    if let Err(e) = result {
        for (_, _, dst) in &staged {
            let _ = fs::remove_file(new_path(dst));
        }
        return Err(e);
    }

    staged.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let mut replaced = Vec::with_capacity(staged.len());
    for (_, path, dst) in staged {
        let tmp = new_path(&dst);
        fs::rename(&tmp, &dst).context(IoSnafu {
            action: "rename into place",
            path: &dst,
        })?;
        if let Some(parent) = dst.parent() {
            sync_dir(parent)?;
        }
        tracing::info!(file = %path, "boot shim file replaced");
        replaced.push(path);
    }
    Ok(replaced)
}

/// Streams the component, writing `.new` copies of changed files and
/// hashing unchanged ones; `staged` collects what was written.
fn stage(
    component: &Component,
    reader: &mut ComponentReader<'_>,
    progress: &mut Reporter<'_>,
    targets: &HashMap<&str, (Target, &FileEntry)>,
    staged: &mut Vec<(u8, String, PathBuf)>,
    buf: &mut [u8],
) -> Result<()> {
    let name = component.name.as_str();
    let payload_err = |source: io::Error| Error::Payload {
        component: name.to_owned(),
        source,
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(reader));
    for entry in archive.entries().map_err(payload_err)? {
        let mut entry = entry.map_err(payload_err)?;
        let kind = entry.header().entry_type();
        let shown = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        check_entry_type(kind, name, &shown)?;
        let path = entry_path(&entry, name)?;
        if kind.is_dir() {
            // Directories are created on demand for the files in them.
            continue;
        }
        let Some((target, file)) = targets.get(path.as_str()) else {
            return UnknownFileSnafu {
                component: name,
                path,
            }
            .fail();
        };
        ensure!(
            seen.insert(path.clone()),
            DuplicateFileSnafu {
                component: name,
                path
            }
        );
        ensure!(
            entry.size() == file.size,
            FileSizeSnafu {
                component: name,
                path: &path,
                expected: file.size,
                found: entry.size(),
            }
        );
        progress.file(Some(&path));
        let found: Digest = if target.unchanged {
            hash_stream(&mut entry, name, buf, progress)?.0
        } else {
            if let Some(parent) = target.dst.parent() {
                create_dirs(parent, &mut Vec::new())?;
            }
            let tmp = new_path(&target.dst);
            match fs::remove_file(&tmp) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::Io {
                        action: "remove stale",
                        path: tmp,
                        source,
                    })
                }
            }
            staged.push((target.rank, path.clone(), target.dst.clone()));
            write_new_file(
                &mut entry,
                &tmp,
                file.size,
                file.mode,
                (name, &path),
                buf,
                progress,
            )?
        };
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
    if let Some(missing) = component.files.iter().find(|f| !seen.contains(&f.path)) {
        return MissingFileSnafu {
            component: name,
            path: &missing.path,
        }
        .fail();
    }
    Ok(())
}
