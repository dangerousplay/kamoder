//! Camera-level helpers that talk to the kernel only.
//!
//! Split out of the `anyka` crate on purpose: linking `anyka` pulls in all 21
//! Anyka SDK shared objects, so a binary that only needs `/dev/watchdog` or a
//! signal handler would otherwise refuse to start on a camera where the SDK
//! libraries are not installed.

pub mod compat;
pub mod shutdown;
pub mod watchdog;

pub use watchdog::CharDevWatchdog;
