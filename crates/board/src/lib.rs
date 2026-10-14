//! Board profiles: which camera this is, and what it has.
//!
//! Every hardware fact that used to be a constant in some crate (sensor
//! count, stream size, GPIO numbers, Wi-Fi module, PTZ driver, vendor
//! processes to stop) lives in one TOML file per board under `boards/`.
//! Services ask this crate for the profile instead of hard-coding the AV100,
//! so a new camera model is a new file, not a fork.
//!
//! The profile is chosen once, by the supervisor, from what the kernel says
//! about the SoC:
//!
//! ```text
//! /proc/cpuinfo                 Hardware  : AK3918AV100
//! /proc/device-tree/compatible  vendor,board\0vendor,soc\0
//! /proc/device-tree/model       Some Board Rev 2
//! ```
//!
//! Every profile whose `[match]` rules all hold is a candidate; the one with
//! the most rules wins. A tie is an error rather than a guess, and no match
//! falls back to `generic-v4l2` (snapshots from a test pattern) so an
//! unknown camera still boots into something reachable. `BOARD=<name>`
//! skips detection entirely.
//!
//! The result ([`Resolved`]) is written to a JSON file whose path is passed
//! to every child in `KAMODER_BOARD` ([`HANDOFF_ENV`]), so all services agree
//! on one board and none of them re-runs detection. [`current`] reads it, and
//! resolves on its own when started by hand.
//!
//! This crate only reads files; it never touches hardware.

mod handoff;
mod profile;
mod resolve;
#[cfg(test)]
mod test_support;

pub use handoff::{current, read_handoff, write_handoff, DEFAULT_HANDOFF_PATH, HANDOFF_ENV};
pub use profile::{
    load, LightLine, Lights, MatchRules, Platform, Profile, Ptz, PtzBackend, Role, Snapshot,
    Stream, StreamOverride, Video, Violation, Wifi,
};
pub use resolve::{detect, resolve, Detected, Resolved, FALLBACK};

use snafu::Snafu;
use std::path::PathBuf;

/// Everything that can go wrong loading or choosing a profile. Each variant
/// names the file involved, so a bad profile on the SD card is fixable from
/// the log line alone.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("read {}: {source}", path.display()))]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// TOML syntax, a wrong type, or an unknown key (`deny_unknown_fields`);
    /// the toml message carries the key and line.
    #[snafu(display("parse {}: {source}", path.display()))]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// Well-formed TOML that breaks a profile rule (BRD-04).
    #[snafu(display("invalid board profile {}: {key}: {reason}", file.display()))]
    Invalid {
        file: PathBuf,
        key: String,
        reason: String,
    },
    /// `BOARD=<name>` (or the fallback) has no `<name>.toml` in any profile
    /// directory. Deliberately no fallback: an explicit choice that cannot
    /// be honoured must not silently become another board.
    #[snafu(display("unknown board {name:?}: no {name}.toml in {dirs}"))]
    UnknownBoard { name: String, dirs: String },
    /// Two or more profiles match equally well.
    #[snafu(display(
        "board profiles {} all match with score {score}; add a rule to tell them apart",
        names.join(", ")
    ))]
    Tie { names: Vec<String>, score: usize },
    #[snafu(display("write {}: {source}", path.display()))]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("board handoff {}: {source}", path.display()))]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// Needed only to find the default `boards/` next to the binary.
    #[snafu(display("locate the running executable: {source}"))]
    CurrentExe { source: std::io::Error },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
