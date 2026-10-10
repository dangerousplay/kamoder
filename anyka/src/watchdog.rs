//! Hardware watchdog through the Anyka SDK (`ak_drv_wdt_*`).
//!
//! For the kernel character device instead — which works without any SDK
//! library installed — use [`CharDevWatchdog`], re-exported here from the
//! `os` crate.

use crate::error::{check, Error, Result};

pub use os::watchdog::CharDevWatchdog;

/// The SDK caps the feed interval at 8 seconds.
pub const MAX_FEED_SECONDS: u32 = 8;

/// Watchdog opened through the Anyka SDK.
///
/// Dropping this calls `ak_drv_wdt_close`.
#[derive(Debug)]
pub struct Watchdog {
    _private: (),
}

impl Watchdog {
    /// Opens and starts the watchdog with the given feed interval.
    pub fn open(feed_seconds: u32) -> Result<Self> {
        if feed_seconds == 0 || feed_seconds > MAX_FEED_SECONDS {
            return Err(Error::invalid(
                "ak_drv_wdt_open",
                format!("feed time must be in 1..={MAX_FEED_SECONDS} seconds"),
            ));
        }
        // SAFETY: plain integer, no pointers.
        let ret = unsafe { anyka_sys::ak_drv_wdt_open(feed_seconds) };
        check("ak_drv_wdt_open", ret)?;
        Ok(Self { _private: () })
    }

    /// Resets the countdown.
    pub fn feed(&self) -> Result<()> {
        // SAFETY: no arguments, no pointers.
        let ret = unsafe { anyka_sys::ak_drv_wdt_feed() };
        check("ak_drv_wdt_feed", ret)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        // SAFETY: no arguments; matches the successful open.
        unsafe { anyka_sys::ak_drv_wdt_close() };
    }
}
