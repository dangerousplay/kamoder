//! The user-set state, kept across restarts in `etc/lights.json`.
//!
//! Only mode, white and alarm are stored. The speaker amp is not: it starts
//! muted every time (IND-04), whatever a client set before.
//!
//! The card is FAT and the camera loses power without warning, so the file
//! is never rewritten in place: the new contents go to `<path>.tmp`, are
//! fsynced and renamed over the old file, which leaves either the complete
//! old file or the complete new one (the same scheme as `auth::atomic`,
//! which is not reused because `auth` would link `ring` into this binary).

use lights_proto::{Alarm, Mode, State};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// What is written to the state file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub mode: Mode,
    pub white: bool,
    pub alarm: Alarm,
}

impl Saved {
    /// The stored part of `state`.
    pub fn from_state(state: State) -> Self {
        Self {
            mode: state.mode,
            white: state.white,
            alarm: state.alarm,
        }
    }

    /// The state to apply at start: this plus a muted amp.
    pub fn to_state(self) -> State {
        State {
            mode: self.mode,
            white: self.white,
            alarm: self.alarm,
            muted: true,
            ..State::default()
        }
    }
}

/// Reads the state file. A missing file means the defaults; an unreadable or
/// corrupt one is logged and also means the defaults, so a bad file can never
/// keep the service (and with it the safe light state) from starting.
pub fn load(path: &Path) -> Saved {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %path.display(), "no saved light state, using the defaults");
            return Saved::default();
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read the saved light state, using the defaults");
            return Saved::default();
        }
    };
    match serde_json::from_str(&text) {
        Ok(saved) => saved,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "saved light state is invalid, using the defaults");
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kamoder-lights-store-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip() {
        let dir = scratch("round-trip");
        let path = dir.join("lights.json");
        let saved = Saved {
            mode: Mode::Night,
            white: true,
            alarm: Alarm::Blink { period_ms: 800 },
        };
        save(&path, &saved).unwrap();
        assert!(!tmp_path(&path).exists());
        assert_eq!(load(&path), saved);
        // Overwrites, too.
        save(&path, &Saved::default()).unwrap();
        assert_eq!(load(&path), Saved::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_or_corrupt_file_means_defaults() {
        let dir = scratch("defaults");
        let path = dir.join("lights.json");
        assert_eq!(load(&path), Saved::default());
        std::fs::write(&path, "{\"mode\":").unwrap();
        assert_eq!(load(&path), Saved::default());
        // Missing fields take their defaults; unknown ones are ignored.
        std::fs::write(&path, r#"{"white":true,"future":1}"#).unwrap();
        assert_eq!(
            load(&path),
            Saved {
                white: true,
                ..Saved::default()
            }
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_amp_always_starts_muted() {
        let state = State {
            muted: false,
            ..State::default()
        };
        assert!(Saved::from_state(state).to_state().muted);
        assert_eq!(Saved::default().to_state(), State::default());
    }

    #[test]
    fn failure_leaves_the_old_file_intact() {
        let dir = scratch("fail");
        let path = dir.join("lights.json");
        save(&path, &Saved::default()).unwrap();
        // Renaming a file over a non-empty directory fails.
        let blocked = dir.join("blocked");
        std::fs::create_dir_all(blocked.join("child")).unwrap();
        assert!(save(&blocked, &Saved::default()).is_err());
        assert!(!tmp_path(&blocked).exists());
        assert_eq!(load(&path), Saved::default());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
