//! Named pan/tilt positions, persisted as JSON.
//!
//! Presets are backend-independent — a preset is just a pair of angles — so
//! one store serves every backend and the trait's provided methods do the
//! rest. The file lives on the SD card (FAT), where a power cut mid-write is
//! a real event on a camera, so every change is written to a sibling
//! temporary file, synced and renamed over the original. `rename(2)` on vfat
//! replaces the target in one directory update, which is as atomic as that
//! filesystem gets.
//!
//! Tokens follow ONVIF: opaque strings chosen by the device. This store hands
//! out the smallest unused positive integer, so tokens stay short and stable.

use crate::error::{self, Error, Result};
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// Upper bound on stored presets; matches what the ONVIF node advertises.
pub const MAX_PRESETS: usize = 32;

/// Longest accepted preset name, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// One saved position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    /// Stable identifier (ONVIF `PresetToken`).
    pub token: String,
    /// Human-readable label (ONVIF `PresetName`), unique within the store.
    pub name: String,
    /// Pan angle in degrees.
    pub pan_deg: f32,
    /// Tilt angle in degrees.
    pub tilt_deg: f32,
}

/// On-disk layout. Versioned so a later format can be told apart.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PresetFile {
    version: u32,
    presets: Vec<Preset>,
}

/// Thread-safe preset table, optionally backed by a file.
#[derive(Debug)]
pub struct PresetStore {
    path: Option<PathBuf>,
    presets: Mutex<Vec<Preset>>,
}

impl PresetStore {
    /// A store that is never written to disk (tests, `--ptz sim` without a path).
    pub fn in_memory() -> Self {
        Self {
            path: None,
            presets: Mutex::new(Vec::new()),
        }
    }

    /// Loads `path`, or starts empty if it does not exist yet.
    ///
    /// A file that exists but does not parse is an error rather than silently
    /// discarded: the next save would otherwise overwrite the user's presets.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let presets = match std::fs::read(&path) {
            Ok(bytes) => {
                let file: PresetFile = serde_json::from_slice(&bytes)
                    .context(error::ParsePresetsSnafu { path: &path })?;
                file.presets
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(source) => return Err(Error::ReadPresets { path, source }),
        };
        Ok(Self {
            path: Some(path),
            presets: Mutex::new(presets),
        })
    }

    /// Backing file, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// All presets, in token order.
    pub fn list(&self) -> Vec<Preset> {
        self.lock().clone()
    }

    /// The preset with `token`.
    pub fn get(&self, token: &str) -> Result<Preset> {
        self.lock()
            .iter()
            .find(|p| p.token == token)
            .cloned()
            .ok_or_else(|| Error::UnknownPreset {
                token: token.to_owned(),
            })
    }

    /// Creates or updates a preset at the given angles and persists the table.
    ///
    /// With `token = None` a new preset is created under a fresh token. With
    /// `Some`, that preset is overwritten; an unknown token is an error, as
    /// ONVIF `SetPreset` requires. An empty `name` means "Preset <token>" for
    /// new presets and "keep the old name" for updates.
    pub fn set(
        &self,
        name: &str,
        token: Option<&str>,
        pan_deg: f32,
        tilt_deg: f32,
    ) -> Result<Preset> {
        validate_name(name)?;
        if !pan_deg.is_finite() || !tilt_deg.is_finite() {
            return Err(Error::invalid("preset position", "angles must be finite"));
        }
        let mut guard = self.lock();
        let mut next = guard.clone();

        let index =
            match token {
                Some(token) => next.iter().position(|p| p.token == token).ok_or_else(|| {
                    Error::UnknownPreset {
                        token: token.to_owned(),
                    }
                })?,
                None => {
                    if next.len() >= MAX_PRESETS {
                        return Err(Error::TooManyPresets { max: MAX_PRESETS });
                    }
                    let token = fresh_token(&next);
                    next.push(Preset {
                        name: format!("Preset {token}"),
                        token,
                        pan_deg,
                        tilt_deg,
                    });
                    next.len() - 1
                }
            };

        if !name.is_empty() {
            if next
                .iter()
                .enumerate()
                .any(|(i, p)| i != index && p.name == name)
            {
                return Err(Error::invalid(
                    "preset name",
                    format!("{name:?} is already used by another preset"),
                ));
            }
            next[index].name = name.to_owned();
        }
        next[index].pan_deg = pan_deg;
        next[index].tilt_deg = tilt_deg;
        let saved = next[index].clone();

        next.sort_by_key(|p| token_order(&p.token));
        self.persist(&next)?;
        *guard = next;
        Ok(saved)
    }

    /// Deletes the preset with `token` and persists the table.
    pub fn remove(&self, token: &str) -> Result<()> {
        let mut guard = self.lock();
        let Some(index) = guard.iter().position(|p| p.token == token) else {
            return Err(Error::UnknownPreset {
                token: token.to_owned(),
            });
        };
        let mut next = guard.clone();
        next.remove(index);
        self.persist(&next)?;
        *guard = next;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Preset>> {
        // A panic while holding the lock cannot leave the Vec half-updated:
        // every mutation builds a copy and swaps it in at the end.
        self.presets.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn persist(&self, presets: &[Preset]) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let file = PresetFile {
            version: 1,
            presets: presets.to_vec(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            .context(error::WritePresetsSnafu { path })?;
        bytes.push(b'\n');
        write_atomic(path, &bytes).context(error::WritePresetsSnafu { path })
    }
}

/// Writes `bytes` to `path` via a synced temporary file and a rename.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir)?;
    let mut tmp_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?
        .to_owned();
    tmp_name.push(".tmp");
    let tmp = dir.join(tmp_name);

    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    // Make the rename itself durable. Best effort: opening a directory for
    // fsync is not supported everywhere, and the data is already in place.
    if let Ok(dir) = std::fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(Error::invalid(
            "preset name",
            format!("longer than {MAX_NAME_CHARS} characters"),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::invalid(
            "preset name",
            "contains a control character",
        ));
    }
    Ok(())
}

/// Smallest positive integer not already used as a token.
fn fresh_token(presets: &[Preset]) -> String {
    (1u32..)
        .map(|n| n.to_string())
        .find(|candidate| presets.iter().all(|p| &p.token != candidate))
        .expect("fewer than u32::MAX presets")
}

/// Numeric tokens sort numerically, anything else after them.
fn token_order(token: &str) -> (u32, String) {
    match token.parse::<u32>() {
        Ok(n) => (n, String::new()),
        Err(_) => (u32::MAX, token.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn set_assigns_fresh_tokens_and_default_names() {
        let store = PresetStore::in_memory();
        let a = store.set("", None, 10.0, 20.0).unwrap();
        let b = store.set("Door", None, 30.0, 40.0).unwrap();
        assert_eq!(a.token, "1");
        assert_eq!(a.name, "Preset 1");
        assert_eq!(b.token, "2");
        assert_eq!(b.name, "Door");
        store.remove("1").unwrap();
        let c = store.set("", None, 0.0, 0.0).unwrap();
        assert_eq!(c.token, "1", "freed tokens are reused");
    }

    #[test]
    fn update_keeps_name_when_empty_and_rejects_unknown_token() {
        let store = PresetStore::in_memory();
        store.set("Door", None, 1.0, 2.0).unwrap();
        let updated = store.set("", Some("1"), 5.0, 6.0).unwrap();
        assert_eq!(updated.name, "Door");
        assert_eq!((updated.pan_deg, updated.tilt_deg), (5.0, 6.0));
        assert!(matches!(
            store.set("x", Some("9"), 0.0, 0.0),
            Err(Error::UnknownPreset { .. })
        ));
    }

    #[test]
    fn rejects_duplicate_names_and_bad_input() {
        let store = PresetStore::in_memory();
        store.set("Door", None, 1.0, 2.0).unwrap();
        assert!(store.set("Door", None, 3.0, 4.0).is_err());
        assert_eq!(store.list().len(), 1, "failed set leaves no trace");
        assert!(store.set("a\nb", None, 0.0, 0.0).is_err());
        assert!(store.set("x", None, f32::NAN, 0.0).is_err());
        assert!(store.set(&"x".repeat(65), None, 0.0, 0.0).is_err());
    }

    #[test]
    fn caps_the_table() {
        let store = PresetStore::in_memory();
        for _ in 0..MAX_PRESETS {
            store.set("", None, 0.0, 0.0).unwrap();
        }
        assert!(matches!(
            store.set("", None, 0.0, 0.0),
            Err(Error::TooManyPresets { .. })
        ));
    }

    #[test]
    fn persists_across_reload() {
        let dir = TempDir::new("presets");
        let path = dir.path().join("sub/ptz-presets.json");
        {
            let store = PresetStore::load(&path).unwrap();
            assert!(store.list().is_empty(), "missing file starts empty");
            store.set("Door", None, 12.5, 30.0).unwrap();
            store.set("Gate", None, 100.0, 10.0).unwrap();
            store.remove("1").unwrap();
        }
        let reloaded = PresetStore::load(&path).unwrap();
        let list = reloaded.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Gate");
        assert_eq!(list[0].token, "2");
        assert!(
            !path.with_file_name("ptz-presets.json.tmp").exists(),
            "temporary file is renamed away"
        );
    }

    #[test]
    fn malformed_file_is_an_error_not_a_reset() {
        let dir = TempDir::new("presets-bad");
        let path = dir.path().join("p.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(matches!(
            PresetStore::load(&path),
            Err(Error::ParsePresets { .. })
        ));
    }

    #[test]
    fn failed_write_leaves_memory_unchanged() {
        let dir = TempDir::new("presets-ro");
        // A directory where the file should be makes the rename fail.
        let path = dir.path().join("p.json");
        let store = PresetStore::load(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"x").unwrap();
        assert!(store.set("Door", None, 1.0, 1.0).is_err());
        assert!(store.list().is_empty());
    }
}
