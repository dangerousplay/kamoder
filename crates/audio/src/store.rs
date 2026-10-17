//! Gains and volumes set over the socket, kept across restarts in
//! `etc/audio.json` and applied at start (AUD-26).
//!
//! ```text
//! {"gains": {"mic0": 5, "spk0": 3}}
//! ```
//!
//! Keyed by the board profile's device ids; a device without an entry uses
//! the profile's `gain`. Entries for devices the board does not have, or
//! outside the device's range, are ignored with a warning (the profile may
//! have changed since).
//!
//! The card is FAT and the camera loses power without warning, so the file
//! is never rewritten in place: `<path>.tmp`, fsync, rename (the lights
//! store's scheme; `auth::atomic` is not reused because `auth` links `ring`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// What is written to the state file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    /// Device id → gain (input) or volume (output), driver units.
    pub gains: BTreeMap<String, u32>,
}

/// Reads the state file. Missing means nothing saved; unreadable or corrupt
/// is logged and also means nothing saved, so a bad file never keeps the
/// service from starting.
pub fn load(path: &Path) -> Saved {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %path.display(), "no saved audio state, using the board's gains");
            return Saved::default();
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read the saved audio state, using the board's gains");
            return Saved::default();
        }
    };
    match serde_json::from_str(&text) {
        Ok(saved) => saved,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "saved audio state is invalid, using the board's gains");
            Saved::default()
        }
    }
}

/// Replaces the state file with `saved`.
pub fn save(path: &Path, saved: &Saved) -> std::io::Result<()> {
    let mut contents = serde_json::to_vec_pretty(saved).map_err(std::io::Error::other)?;
    contents.push(b'\n');
    let tmp = tmp_path(path);
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
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

    #[test]
    fn round_trip() {
        let dir = TempDir::new("store");
        let path = dir.path().join("audio.json");
        let saved = Saved {
            gains: [("mic0".to_owned(), 5), ("spk0".to_owned(), 2)].into(),
        };
        save(&path, &saved).unwrap();
        assert!(!tmp_path(&path).exists());
        assert_eq!(load(&path), saved);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"mic0\": 5"), "{text}");
        save(&path, &Saved::default()).unwrap();
        assert_eq!(load(&path), Saved::default());
    }

    #[test]
    fn missing_or_corrupt_file_means_nothing_saved() {
        let dir = TempDir::new("store-bad");
        let path = dir.path().join("audio.json");
        assert_eq!(load(&path), Saved::default());
        std::fs::write(&path, "{\"gains\":").unwrap();
        assert_eq!(load(&path), Saved::default());
        std::fs::write(&path, r#"{"future":1,"gains":{"mic0":1}}"#).unwrap();
        assert_eq!(load(&path).gains["mic0"], 1);
    }

    #[test]
    fn failure_leaves_the_old_file_intact() {
        let dir = TempDir::new("store-fail");
        let path = dir.path().join("audio.json");
        let saved = Saved {
            gains: [("mic0".to_owned(), 5)].into(),
        };
        save(&path, &saved).unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::create_dir_all(blocked.join("child")).unwrap();
        assert!(save(&blocked, &Saved::default()).is_err());
        assert!(!tmp_path(&blocked).exists());
        assert_eq!(load(&path), saved);
    }
}
