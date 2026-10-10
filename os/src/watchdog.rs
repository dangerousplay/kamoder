//! Hardware watchdog via the `/dev/watchdog` character device.
//!
//! This path works even when no Anyka SDK library is installed. For the SDK
//! route (`ak_drv_wdt_*`), see `anyka::watchdog::Watchdog`.

use snafu::{ResultExt, Snafu};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

// `linux/watchdog.h` ioctls, which neither `libc` nor `nix` expose.
// The `_bad` macros take the pre-computed request number, which is what the
// kernel header spells out:
//   WDIOC_KEEPALIVE  = _IOR('W', 5, int)
//   WDIOC_SETTIMEOUT = _IOWR('W', 6, int)
nix::ioctl_none_bad!(wdt_keepalive, nix::request_code_read!(b'W', 5, 4));
nix::ioctl_write_ptr_bad!(
    wdt_set_timeout,
    nix::request_code_readwrite!(b'W', 6, 4),
    libc::c_int
);

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot open the watchdog device {}: {source}", path.display()))]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("cannot feed the watchdog: {source}"))]
    Feed { source: std::io::Error },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// An open watchdog device.
///
/// Dropping this writes the magic byte `V`, the kernel's "magic close"
/// protocol for disarming the timer. Without it the driver keeps counting and
/// the board reboots a few seconds after the process exits — which is exactly
/// what [`CharDevWatchdog::leave_armed`] is for when that is intended.
#[derive(Debug)]
pub struct CharDevWatchdog {
    file: File,
    disarm_on_drop: bool,
}

impl CharDevWatchdog {
    /// Opens the device and requests `timeout_seconds` before reset.
    ///
    /// A driver that does not implement `WDIOC_SETTIMEOUT` keeps its built-in
    /// timeout; that is logged, not treated as an error.
    pub fn open(path: impl AsRef<Path>, timeout_seconds: i32) -> Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .context(OpenSnafu { path })?;

        // SAFETY: the fd is open for writing and the pointer is to a live
        // `c_int`, which is the argument type WDIOC_SETTIMEOUT expects.
        if let Err(errno) = unsafe { wdt_set_timeout(file.as_raw_fd(), &timeout_seconds) } {
            tracing::debug!(%errno, "watchdog driver does not support WDIOC_SETTIMEOUT");
        }
        Ok(Self {
            file,
            disarm_on_drop: true,
        })
    }

    /// Resets the countdown.
    pub fn feed(&mut self) -> Result<()> {
        // SAFETY: the fd is open for writing; this ioctl takes no argument.
        if unsafe { wdt_keepalive(self.file.as_raw_fd()) }.is_ok() {
            return Ok(());
        }
        // Drivers without the ioctl treat any write as a keepalive.
        self.file.write_all(b"\0").context(FeedSnafu)
    }

    /// Leaves the watchdog armed when this value drops, so the board reboots.
    pub fn leave_armed(&mut self) {
        self.disarm_on_drop = false;
    }
}

impl Drop for CharDevWatchdog {
    fn drop(&mut self) {
        if self.disarm_on_drop {
            if let Err(error) = self.file.write_all(b"V") {
                tracing::warn!(%error, "cannot disarm the watchdog; the board will reboot");
            }
        }
    }
}
