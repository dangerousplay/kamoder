//! "Is this secret file readable by other users?" — on filesystems where that
//! question has an answer.
//!
//! The SD card is FAT. FAT stores no owner or mode: the kernel reports the
//! mount's `fmask` for every file (0755 by default on the camera), and `chmod`
//! cannot change it. Refusing a file there for being mode 0755 would make the
//! credentials unusable anywhere on the card — which is exactly how SSH failed
//! the first time the tests booted a real FAT card. On such filesystems the
//! check is skipped with a warning; what protects the credentials there is the
//! PBKDF2 hash, and physical control of the card.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// `statfs` magic numbers of filesystems without Unix permissions.
const NO_UNIX_PERMISSIONS: &[(i64, &str)] = &[
    (0x4d44, "FAT"),        // MSDOS_SUPER_MAGIC (vfat, msdos)
    (0x2011_bab0, "exFAT"), // EXFAT_SUPER_MAGIC
    (0x5346_544e, "NTFS"),  // NTFS_SB_MAGIC (ntfs/ntfs3)
];

/// The filesystem name if `path` lives on one without Unix permissions.
pub fn permissionless_filesystem(path: &Path) -> Option<&'static str> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: all-zero is a valid `statfs`; `c_path` is NUL-terminated and
    // outlives the call, which only writes into `buf`.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)] // f_type's width differs between libcs
    let kind = buf.f_type as i64;
    NO_UNIX_PERMISSIONS
        .iter()
        .find(|(magic, _)| *magic == kind)
        .map(|(_, name)| *name)
}

/// The file's mode if group or others can access it and that means something
/// on its filesystem; `None` if it is fine.
pub fn too_open(path: &Path, metadata: &std::fs::Metadata) -> Option<u32> {
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 == 0 {
        return None;
    }
    if let Some(fs) = permissionless_filesystem(path) {
        tracing::warn!(
            path = %path.display(),
            filesystem = fs,
            "permissions not enforceable on this filesystem; relying on the card staying physically secure"
        );
        return None;
    }
    Some(mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::OpenOptionsExt;

    fn file_with_mode(mode: u32) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("anyka-auth-perms-{}-{mode:o}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .mode(mode)
            .open(&path)
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn private_file_is_fine() {
        let path = file_with_mode(0o600);
        assert_eq!(too_open(&path, &std::fs::metadata(&path).unwrap()), None);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn open_file_on_a_unix_filesystem_is_refused() {
        let path = file_with_mode(0o644);
        // The host's temp dir is tmpfs or ext4/btrfs — never FAT.
        assert_eq!(permissionless_filesystem(&path), None);
        assert_eq!(
            too_open(&path, &std::fs::metadata(&path).unwrap()),
            Some(0o644)
        );
        std::fs::remove_file(path).unwrap();
    }
}
