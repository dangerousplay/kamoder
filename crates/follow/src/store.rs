//! The follow switch, kept across restarts in `etc/follow.json` (D-F3).
//!
//! ```text
//! { "enabled": true }
//! ```
//!
//! Only the switch is stored; the tuning lives in the board profile. A
//! missing file means "never toggled": the caller falls back to
//! `FOLLOW_ENABLED` from `camera.conf` (default 0, FOL-14), so editing
//! `camera.conf` keeps working until somebody flips the switch in the web UI.
//!
//! The card is FAT and the camera loses power without warning, so the file
//! is never rewritten in place: the new contents go to `<path>.tmp` (mode
//! 0600), are fsynced and renamed over the old file, which leaves either the
//! complete old file or the complete new one (the scheme of
//! `lights/src/store.rs`).

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// What is written to the state file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub enabled: bool,
}

/// Reads the state file. `None` when it is missing; an unreadable or
/// corrupt file is logged and also gives `None`, so a bad file never keeps
/// the service from starting and the seed applies instead.
pub fn load(path: &Path) -> Option<Saved> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %path.display(), "no saved follow state");
            return None;
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read the saved follow state");
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(saved) => Some(saved),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "saved follow state is invalid");
            None
        }
    }
}

/// The switch at start: the saved one, else `seed` (`--enabled` /
/// `FOLLOW_ENABLED`), else off.
pub fn initial(path: Option<&Path>, seed: Option<bool>) -> bool {
    match path.and_then(load) {
        Some(saved) => saved.enabled,
        None => seed.unwrap_or(false),
    }
}

/// Replaces the state file with `saved`.
pub fn save(path: &Path, saved: &Saved) -> std::io::Result<()> {
    let mut contents = serde_json::to_vec_pretty(saved).map_err(std::io::Error::other)?;
    contents.push(b'\n');
    let tmp = tmp_path(path);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    // Best effort: some filesystems refuse to fsync a directory, and the
    // rename has already happened.
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn round_trip() {
        let dir = TempDir::new("store-round-trip");
        let path = dir.path().join("follow.json");
        save(&path, &Saved { enabled: true }).unwrap();
        assert!(!tmp_path(&path).exists());
        assert_eq!(load(&path), Some(Saved { enabled: true }));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"enabled\": true\n}\n"
        );
        save(&path, &Saved { enabled: false }).unwrap();
        assert_eq!(load(&path), Some(Saved { enabled: false }));
    }

    #[test]
    fn missing_file_is_disabled_unless_seeded() {
        let dir = TempDir::new("store-missing");
        let path = dir.path().join("follow.json");
        assert_eq!(load(&path), None);
        assert!(!initial(Some(&path), None));
        assert!(initial(Some(&path), Some(true)));
        assert!(!initial(None, None));
        assert!(initial(None, Some(true)));
    }

    #[test]
    fn a_saved_file_wins_over_the_seed() {
        let dir = TempDir::new("store-wins");
        let path = dir.path().join("follow.json");
        save(&path, &Saved { enabled: false }).unwrap();
        assert!(!initial(Some(&path), Some(true)));
        save(&path, &Saved { enabled: true }).unwrap();
        assert!(initial(Some(&path), Some(false)));
    }

    #[test]
    fn corrupt_file_falls_back_to_the_seed() {
        let dir = TempDir::new("store-corrupt");
        let path = dir.path().join("follow.json");
        std::fs::write(&path, "{\"enabled\":").unwrap();
        assert_eq!(load(&path), None);
        assert!(initial(Some(&path), Some(true)));
        // Unknown fields are ignored, missing ones default.
        std::fs::write(&path, r#"{"future":1}"#).unwrap();
        assert_eq!(load(&path), Some(Saved { enabled: false }));
    }

    #[test]
    fn failure_leaves_the_old_file_intact() {
        let dir = TempDir::new("store-fail");
        let path = dir.path().join("follow.json");
        save(&path, &Saved { enabled: true }).unwrap();
        // Renaming a file over a non-empty directory fails.
        let blocked = dir.path().join("blocked");
        std::fs::create_dir_all(blocked.join("child")).unwrap();
        assert!(save(&blocked, &Saved::default()).is_err());
        assert!(!tmp_path(&blocked).exists());
        assert_eq!(load(&path), Some(Saved { enabled: true }));
    }
}
