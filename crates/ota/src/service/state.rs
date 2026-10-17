//! The updater's persistent state, `update/state.json` under the data root.
//!
//! ```text
//! { "settings": {"channel":"stable","auto_check":true,"auto_install":false,
//!                "window":null,"repo":"dangerousplay/kamoder"},
//!   "highest_seq": 1760700000, "security_floor": "0.2.0",
//!   "last_check": 1792000000,
//!   "available": {"version":"0.3.0","channel":"stable","seq":1760800000,
//!                 "notes_url":null,"size":5300000,
//!                 "kup_url":"https://github.com/…/kamoder-0.3.0-update.kup",
//!                 "downgrade":false},
//!   "downloaded": "kamoder-0.3.0-update.kup",
//!   "last_error": null }
//! ```
//!
//! `highest_seq` and `security_floor` are the anti-replay memory of the
//! policy (OTA-04). They are also recovered at start from the running slot's
//! own signed manifest, so losing this file can lower them at most to the
//! running release, never below it.
//!
//! The card is FAT and power goes without warning, so the file is never
//! rewritten in place: `<path>.tmp` (mode 0600), fsync, rename, fsync of the
//! directory (the scheme of `lights/src/store.rs`). Unknown fields are
//! ignored and missing ones default, so older and newer updaters share it; a
//! corrupt file is logged and replaced by defaults rather than keeping the
//! service from starting.

use ota_manifest::Version;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use update_proto::{Channel, ErrorInfo, Settings};

/// File name inside `update/`.
pub const STATE_FILE: &str = "state.json";

/// What the last check found, with what the download needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedAvailable {
    pub version: String,
    pub channel: Channel,
    pub seq: u64,
    #[serde(default)]
    pub notes_url: Option<String>,
    /// Package size: an estimate from the manifest until downloaded.
    #[serde(default)]
    pub size: Option<u64>,
    /// Where the `.kup` is downloaded from.
    pub kup_url: String,
    #[serde(default)]
    pub downgrade: bool,
}

/// Contents of `state.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub settings: Settings,
    /// Highest manifest seq installed (0 = none).
    pub highest_seq: u64,
    /// Highest security floor seen in an installed manifest.
    pub security_floor: Option<Version>,
    /// Unix seconds of the last successful check.
    pub last_check: Option<u64>,
    pub available: Option<SavedAvailable>,
    /// File name (in `update/`) of the verified download of `available`.
    pub downloaded: Option<String>,
    pub last_error: Option<ErrorInfo>,
}

/// Reads the state file; missing or unreadable gives `None` (logged).
pub fn load(path: &Path) -> Option<Saved> {
    let text = match std::fs::read(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %path.display(), "no saved update state");
            return None;
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read the update state");
            return None;
        }
    };
    match serde_json::from_slice(&text) {
        Ok(saved) => Some(saved),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "update state is invalid; using defaults");
            None
        }
    }
}

/// Replaces the state file with `saved` (tmp + fsync + rename).
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
    use update_proto::MaintenanceWindow;

    fn sample() -> Saved {
        Saved {
            settings: Settings {
                channel: Channel::Beta,
                auto_check: false,
                auto_install: true,
                window: Some(MaintenanceWindow {
                    start_hour: 2,
                    end_hour: 4,
                }),
                repo: "me/fork".into(),
            },
            highest_seq: 42,
            security_floor: Some("0.2.0".parse().unwrap()),
            last_check: Some(1_792_000_000),
            available: Some(SavedAvailable {
                version: "0.3.0".into(),
                channel: Channel::Beta,
                seq: 43,
                notes_url: Some("https://example.org/notes".into()),
                size: Some(1234),
                kup_url: "https://example.org/k.kup".into(),
                downgrade: false,
            }),
            downloaded: Some("kamoder-0.3.0-update.kup".into()),
            last_error: Some(ErrorInfo {
                code: "network".into(),
                message: "down".into(),
                at: Some(1_792_000_001),
            }),
        }
    }

    #[test]
    fn round_trip_is_atomic_and_private() {
        let dir = TempDir::new("state-round-trip");
        let path = dir.path().join(STATE_FILE);
        assert_eq!(load(&path), None);
        save(&path, &sample()).unwrap();
        assert!(!tmp_path(&path).exists());
        assert_eq!(load(&path), Some(sample()));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        save(&path, &Saved::default()).unwrap();
        assert_eq!(load(&path), Some(Saved::default()));
    }

    #[test]
    fn corrupt_or_foreign_files() {
        let dir = TempDir::new("state-corrupt");
        let path = dir.path().join(STATE_FILE);
        std::fs::write(&path, "{\"highest_seq\":").unwrap();
        assert_eq!(load(&path), None);
        // Unknown fields are ignored, missing ones default.
        std::fs::write(&path, r#"{"highest_seq":5,"future":{"x":1}}"#).unwrap();
        let saved = load(&path).unwrap();
        assert_eq!(saved.highest_seq, 5);
        assert_eq!(saved.settings, Settings::default());
    }

    #[test]
    fn failed_save_keeps_the_old_file() {
        let dir = TempDir::new("state-fail");
        let path = dir.path().join(STATE_FILE);
        save(&path, &sample()).unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::create_dir_all(blocked.join("child")).unwrap();
        assert!(save(&blocked, &Saved::default()).is_err());
        assert!(!tmp_path(&blocked).exists());
        assert_eq!(load(&path), Some(sample()));
    }
}
