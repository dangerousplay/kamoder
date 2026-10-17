//! A/B boot control for Kamoder OTA updates (OTA-07..09, OTA-21; design §2).
//!
//! The camera has two code slots (`a`, `b`). Which one boots is decided by a
//! tiny shim (`kboot select`, run from `Factory/config.sh`) from a persistent
//! [`Record`]: per slot `bootable` / `successful` / `tries_left` / `priority` /
//! `version`, the Android/RAUC A/B state machine.
//!
//! Why the pieces look the way they do:
//!
//! - **The SD card is vfat**: no symlinks, and `rename` is not power-safe. So
//!   the record lives in two fixed-size 512-byte copies (`boot/sel.0`,
//!   `boot/sel.1`) carrying a sequence number and a CRC-32, overwritten in place
//!   alternately, exactly like a redundant U-Boot environment. A torn write
//!   only ever damages the copy being written; the reader falls back to the
//!   other one ([`SdSelector`], design D-O3).
//! - **Decisions are pure functions** ([`choose`], [`activate`],
//!   [`mark_successful`], [`invalidate`], [`rollback`]) over a `Record`, so the
//!   whole trial/rollback lifecycle is host-tested without touching storage.
//!   Persistence and the sequence number belong to a [`BootControl`] backend.
//! - **No tokio, no tracing**: `kboot` must stay small and start instantly at
//!   boot. Anything worth logging is returned as a value ([`Note`]).
//! - **U-Boot env backend** (P2, OTA-21): the vendor U-Boot keeps a 4 KiB CRC'd
//!   environment on NOR. [`uboot`] holds an own codec for single and redundant
//!   images (no maintained crate exists) and a [`UbootEnvControl`] that maps the
//!   record onto `kamoder_*` variables. Raw MTD writes need an erase first; that
//!   is left to the storage implementation (the `ota` crate's `Flash` trait).

use std::fmt;
use std::path::PathBuf;

use snafu::Snafu;

mod control;
mod machine;
mod record;
mod sd;
pub mod uboot;

#[cfg(test)]
mod test_support;

pub use control::{BootControl, MemoryControl};
pub use machine::{activate, choose, invalidate, mark_successful, rollback, Choice, Note};
pub use record::{
    seq_newer, DecodeError, Record, SlotState, Version, FORMAT, MAGIC, RECORD_LEN, VERSION_LEN,
};
pub use sd::{Loaded, SdSelector, SELECTOR_FILES};
pub use uboot::{EnvStorage, FileEnvStorage, UbootEnvControl};

/// One of the two code slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    /// Both slots, in tie-break order (A wins ties).
    pub const ALL: [Slot; 2] = [Slot::A, Slot::B];

    /// The other slot.
    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }

    /// Lower-case name used in paths (`slots/a`) and env vars: `"a"` / `"b"`.
    pub fn name(self) -> &'static str {
        match self {
            Slot::A => "a",
            Slot::B => "b",
        }
    }

    /// Parses `"a"` / `"b"` (case-insensitive).
    pub fn from_name(name: &str) -> Option<Slot> {
        match name {
            "a" | "A" => Some(Slot::A),
            "b" | "B" => Some(Slot::B),
            _ => None,
        }
    }

    /// Array index (A = 0, B = 1).
    pub fn index(self) -> usize {
        match self {
            Slot::A => 0,
            Slot::B => 1,
        }
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Errors of the storage backends.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("{action} {}: {source}", path.display()))]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("U-Boot env copy {copy}: {action}: {source}"))]
    EnvIo {
        action: &'static str,
        copy: usize,
        source: std::io::Error,
    },
    #[snafu(display("U-Boot env: {source}"))]
    Env { source: uboot::EnvError },
    #[snafu(display("U-Boot env variable {name}={value:?} is invalid"))]
    EnvVar { name: String, value: String },
    #[snafu(display("U-Boot env variable {name} is missing"))]
    EnvVarMissing { name: String },
    #[snafu(display("no other slot that is bootable and successful to roll back to"))]
    NoRollbackTarget,
}

/// Result alias for the backends.
pub type Result<T, E = Error> = std::result::Result<T, E>;
