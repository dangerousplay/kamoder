//! Errors for every backend and the preset store.

use snafu::Snafu;
use std::path::PathBuf;

/// Anything a PTZ backend or the preset store can fail with.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    /// An argument outside what the backend accepts (NaN speed, bad name…).
    #[snafu(display("{what}: {reason}"))]
    InvalidArgument { what: &'static str, reason: String },

    /// The requested backend is not available in this build or on this board.
    #[snafu(display("{backend} PTZ backend unavailable: {reason}"))]
    Unsupported {
        backend: &'static str,
        reason: String,
    },

    /// No preset has this token.
    #[snafu(display("no preset with token {token:?}"))]
    UnknownPreset { token: String },

    /// The preset table is full.
    #[snafu(display("preset limit of {max} reached"))]
    TooManyPresets { max: usize },

    /// The preset file exists but could not be read.
    #[snafu(display("cannot read presets from {}", path.display()))]
    ReadPresets {
        path: PathBuf,
        source: std::io::Error,
    },

    /// The preset file is not the JSON this crate writes.
    #[snafu(display("presets file {} is malformed", path.display()))]
    ParsePresets {
        path: PathBuf,
        source: serde_json::Error,
    },

    /// Writing the preset file failed; the in-memory table is left unchanged.
    #[snafu(display("cannot write presets to {}", path.display()))]
    WritePresets {
        path: PathBuf,
        source: std::io::Error,
    },

    /// A vendor SDK call reported failure.
    #[snafu(display("{op} failed (SDK error {code})"))]
    Sdk { op: &'static str, code: i32 },

    /// The driver state is process-global; only one handle may exist.
    #[snafu(display("the PTZ driver is already open in this process"))]
    AlreadyOpen,

    /// An ioctl on `/dev/preset_motor` (or opening it) failed.
    #[snafu(display("preset_motor {op} failed"))]
    Motor {
        op: &'static str,
        source: std::io::Error,
    },

    /// `presetmotor.ko` could not be loaded, or its device node never appeared.
    #[snafu(display("cannot load {}: {reason}", module.display()))]
    LoadModule { module: PathBuf, reason: String },

    /// The vendor `device.config` exists but could not be read.
    #[snafu(display("cannot read motor settings from {}", path.display()))]
    ReadDeviceConfig {
        path: PathBuf,
        source: std::io::Error,
    },

    /// The motor worker thread could not be started.
    #[snafu(display("cannot start the PTZ worker thread"))]
    SpawnWorker { source: std::io::Error },
}

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// Shorthand for [`Error::InvalidArgument`].
    pub(crate) fn invalid(what: &'static str, reason: impl Into<String>) -> Self {
        Error::InvalidArgument {
            what,
            reason: reason.into(),
        }
    }

    /// Shorthand for [`Error::Unsupported`].
    pub(crate) fn unsupported(backend: &'static str, reason: impl Into<String>) -> Self {
        Error::Unsupported {
            backend,
            reason: reason.into(),
        }
    }
}
