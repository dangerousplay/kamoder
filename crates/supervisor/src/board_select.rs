//! Choosing the board once per boot and telling everyone (BRD-05, PKG-03).
//!
//! The supervisor resolves the board profile at start, writes the result to
//! the handoff file and points every child at it through `KAMODER_BOARD`.
//! `supervisor board --shell` gives `init.sh` the same answer as shell
//! assignments, before any service runs.
//!
//! Resolution failing is never fatal here: the watchdog feeder and Wi-Fi
//! must start whatever is wrong with `boards/`, or the camera reboot-loops
//! or drops off the network. The caller logs the error and carries on
//! without a board.

use board::Resolved;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Resolves the board from the profiles in `boards_dir` against the real
/// `/`. An empty `override_name` (`BOARD=` left blank in camera.conf) means
/// no override.
pub fn resolve(boards_dir: &Path, override_name: Option<&str>) -> board::Result<Resolved> {
    let name = override_name.map(str::trim).filter(|n| !n.is_empty());
    board::resolve(&[boards_dir.to_path_buf()], name, Path::new("/"))
}

/// `<exe dir>/../boards`: the dist layout, `bin/supervisor` next to
/// `boards/`. Used by `supervisor board`, which has no config file to be
/// relative to.
pub fn default_boards_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|bin| bin.join("..").join("boards")))
        .unwrap_or_else(|| PathBuf::from("boards"))
}

/// What `supervisor board` prints.
pub fn human(resolved: &Resolved) -> String {
    format!(
        "board:   {}\nreason:  {}\nprofile: {}\n",
        resolved.name,
        resolved.reason,
        resolved.path.display()
    )
}

/// What `supervisor board --shell` prints: assignments for `init.sh` to
/// `eval`. Every value is single-quoted, so nothing in a profile can run
/// as shell code.
pub fn shell(resolved: &Resolved) -> String {
    let platform = resolved.profile.platform.as_ref();
    let vendor = platform
        .map(|p| p.vendor_processes.join(" "))
        .unwrap_or_default();
    let wait = platform.is_some_and(|p| p.watchdog_wait);
    let mut out = String::new();
    let _ = writeln!(out, "KAMODER_BOARD_NAME={}", quote(&resolved.name));
    let _ = writeln!(out, "VENDOR_PROCESSES={}", quote(&vendor));
    let _ = writeln!(out, "WATCHDOG_WAIT={}", u8::from(wait));
    out
}

/// POSIX single-quoting: `'` becomes `'\''`.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boards() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../boards")
    }

    #[test]
    fn quoting() {
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("$(reboot)`x`"), "'$(reboot)`x`'");
    }

    #[test]
    fn shell_output_for_the_av100() {
        let resolved = resolve(&boards(), Some("anyka-av100-ptz")).unwrap();
        let platform = resolved.profile.platform.clone().unwrap();
        let expected = format!(
            "KAMODER_BOARD_NAME='anyka-av100-ptz'\nVENDOR_PROCESSES='{}'\nWATCHDOG_WAIT={}\n",
            platform.vendor_processes.join(" "),
            u8::from(platform.watchdog_wait)
        );
        assert_eq!(shell(&resolved), expected);
        assert!(!platform.vendor_processes.is_empty());
    }

    #[test]
    fn shell_output_without_a_platform_section() {
        let mut resolved = resolve(&boards(), Some("generic-v4l2")).unwrap();
        resolved.profile.platform = None;
        assert_eq!(
            shell(&resolved),
            "KAMODER_BOARD_NAME='generic-v4l2'\nVENDOR_PROCESSES=''\nWATCHDOG_WAIT=0\n"
        );
    }

    #[test]
    fn shell_output_escapes_quotes() {
        let mut resolved = resolve(&boards(), Some("generic-v4l2")).unwrap();
        resolved.name = "o'brien".into();
        resolved.profile.platform = Some(board::Platform {
            vendor_processes: vec!["anyka_ipc".into(), "x';reboot;'".into()],
            watchdog_wait: true,
        });
        assert_eq!(
            shell(&resolved),
            "KAMODER_BOARD_NAME='o'\\''brien'\n\
             VENDOR_PROCESSES='anyka_ipc x'\\'';reboot;'\\'''\n\
             WATCHDOG_WAIT=1\n"
        );
    }

    /// The output really is what `sh` reads back.
    #[test]
    fn shell_output_round_trips_through_sh() {
        let mut resolved = resolve(&boards(), Some("generic-v4l2")).unwrap();
        resolved.name = "a'b $c".into();
        resolved.profile.platform = Some(board::Platform {
            vendor_processes: vec!["p1".into(), "p'2".into()],
            watchdog_wait: true,
        });
        let script = format!(
            "{}printf '%s|%s|%s' \"$KAMODER_BOARD_NAME\" \"$VENDOR_PROCESSES\" \"$WATCHDOG_WAIT\"",
            shell(&resolved)
        );
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "a'b $c|p1 p'2|1");
    }

    #[test]
    fn blank_override_means_detect() {
        // `/` of the test host matches no shipped profile: generic fallback.
        let resolved = resolve(&boards(), Some("  ")).unwrap();
        assert!(!resolved.reason.contains("override"), "{}", resolved.reason);
    }

    #[test]
    fn unknown_override_is_an_error() {
        assert!(resolve(&boards(), Some("no-such-board")).is_err());
    }

    #[test]
    fn human_output_names_board_reason_and_path() {
        let resolved = resolve(&boards(), Some("generic-v4l2")).unwrap();
        let text = human(&resolved);
        assert!(text.contains("generic-v4l2"));
        assert!(text.contains(&resolved.reason));
        assert!(text.contains(&resolved.path.display().to_string()));
    }
}
