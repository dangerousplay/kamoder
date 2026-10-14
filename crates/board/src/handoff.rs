//! Passing the resolved board from the supervisor to every service
//! (BRD-05).
//!
//! The supervisor resolves once and writes [`Resolved`] as JSON; children
//! find the file through `KAMODER_BOARD`. One decision per boot means the
//! camera, RTSP, ONVIF and web services can never disagree about the board,
//! even if `boards/` changes underneath them.

use crate::resolve::{resolve, Resolved};
use crate::{CurrentExeSnafu, JsonSnafu, ReadSnafu, Result, WriteSnafu};
use snafu::ResultExt;
use std::path::{Path, PathBuf};

/// Names the handoff file in a service's environment.
pub const HANDOFF_ENV: &str = "KAMODER_BOARD";

/// Where the supervisor writes the handoff file. `/tmp` is a tmpfs, so a
/// stale choice never survives a reboot.
pub const DEFAULT_HANDOFF_PATH: &str = "/tmp/kamoder-board.json";

/// Profile directories when the services are started by hand (not by the
/// supervisor). Default: `<exe dir>/../boards`, matching the dist layout
/// (`bin/` next to `boards/`).
const BOARDS_DIR_ENV: &str = "KAMODER_BOARDS_DIR";

/// Board name override when resolving without a handoff.
const OVERRIDE_ENV: &str = "BOARD";

/// Writes `resolved` to `path`. Written to a sibling file and renamed, so a
/// service starting at the same moment never reads half a file.
pub fn write_handoff(path: &Path, resolved: &Resolved) -> Result<()> {
    let json = serde_json::to_vec_pretty(resolved).context(JsonSnafu { path })?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, json).context(WriteSnafu { path: &tmp })?;
    std::fs::rename(&tmp, path).context(WriteSnafu { path })
}

/// Reads a handoff file written by [`write_handoff`].
pub fn read_handoff(path: &Path) -> Result<Resolved> {
    let text = std::fs::read(path).context(ReadSnafu { path })?;
    serde_json::from_slice(&text).context(JsonSnafu { path })
}

/// The board this process runs on: the supervisor's choice when
/// `KAMODER_BOARD` is set, else resolved here from `KAMODER_BOARDS_DIR`
/// (or `<exe dir>/../boards`) and `BOARD`, against the real `/`.
pub fn current() -> Result<Resolved> {
    if let Some(path) = std::env::var_os(HANDOFF_ENV).filter(|p| !p.is_empty()) {
        return read_handoff(Path::new(&path));
    }
    let dir = match std::env::var_os(BOARDS_DIR_ENV).filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let exe = std::env::current_exe().context(CurrentExeSnafu)?;
            let bin = exe.parent().unwrap_or(Path::new("/"));
            bin.join("..").join("boards")
        }
    };
    let name = std::env::var(OVERRIDE_ENV).ok().filter(|n| !n.is_empty());
    resolve(&[dir], name.as_deref(), Path::new("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn handoff_roundtrip() {
        let dir = TempDir::new("handoff");
        let boards = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../boards");
        let resolved = resolve(&[boards], Some("anyka-av100-ptz"), dir.path()).unwrap();
        let path = dir.path().join("kamoder-board.json");
        write_handoff(&path, &resolved).unwrap();
        assert_eq!(read_handoff(&path).unwrap(), resolved);
        // No leftover temporary file.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn handoff_json_field_names() {
        let dir = TempDir::new("handoff-json");
        let boards = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../boards");
        let resolved = resolve(&[boards], Some("generic-v4l2"), dir.path()).unwrap();
        let json = serde_json::to_value(&resolved).unwrap();
        assert_eq!(json["name"], "generic-v4l2");
        assert_eq!(json["reason"], "BOARD=generic-v4l2 override");
        assert_eq!(json["profile"]["video"]["backend"], "test-pattern");
        assert!(json["profile"]["match"].is_object());
    }

    #[test]
    fn missing_handoff_file_is_a_read_error() {
        let dir = TempDir::new("handoff-missing");
        let err = read_handoff(&dir.path().join("none.json")).unwrap_err();
        assert!(matches!(err, crate::Error::Read { .. }), "{err:?}");
    }
}
