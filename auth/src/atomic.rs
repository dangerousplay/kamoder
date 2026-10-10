//! Crash-safe replacement of small secret files.
//!
//! The camera loses power without warning — it is unplugged, or the watchdog
//! resets it — and its SD card is FAT, where a half-written file is simply
//! truncated. A user file or signing key cut short at that moment would lock
//! everyone out. So files are never rewritten in place: the new contents go to
//! `<path>.tmp`, are fsynced, and are renamed over the old file, which on FAT
//! as on ext4 leaves either the complete old file or the complete new one.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The temporary sibling `write` stages the contents in.
pub fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Replaces `path` with `contents`, creating it with `mode` (e.g. `0o600`).
///
/// A stale `<path>.tmp` left by an earlier crash is removed first, so the file
/// is always created fresh with `mode` rather than inheriting whatever mode the
/// leftover had. The parent directory is fsynced afterwards on a best-effort
/// basis: some filesystems refuse to fsync a directory, and by then the rename
/// has already happened.
pub fn write(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    let tmp = tmp_path(path);
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    if let Some(parent) = path.parent() {
        let dir = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("anyka-auth-atomic-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_with_mode_and_replaces() {
        let dir = scratch("replace");
        let path = dir.join("file");
        write(&path, b"one", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        write(&path, b"two", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert!(!tmp_path(&path).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_tmp_is_replaced_not_reused() {
        let dir = scratch("stale");
        let path = dir.join("file");
        let tmp = tmp_path(&path);
        std::fs::write(&tmp, b"garbage from a crash").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        write(&path, b"fresh", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"fresh");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failure_leaves_the_old_file_intact() {
        let dir = scratch("fail");
        let path = dir.join("file");
        write(&path, b"old", 0o600).unwrap();
        // Renaming a file over a non-empty directory fails.
        let blocked = dir.join("blocked");
        std::fs::create_dir_all(blocked.join("child")).unwrap();
        assert!(write(&blocked, b"new", 0o600).is_err());
        assert!(!tmp_path(&blocked).exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
