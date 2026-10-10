//! Clock helpers that match the SDK's own notion of time.
//!
//! Stream timestamps come from `ak_get_ostime` (monotonic since boot), while
//! file names use `ak_get_localdate`, so both are wrapped here instead of
//! mixing in `std::time`.

use crate::error::{check, Error, Result};
use std::time::Duration;

/// Sleeps using the SDK's own sleep, which the vendor libraries assume is used
/// around their blocking calls.
pub fn sleep(duration: Duration) {
    let ms = duration.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: plain integer argument.
    unsafe { anyka_sys::ak_sleep_ms(ms) }
}

/// Milliseconds since boot, as the SDK counts them.
pub fn ostime_ms() -> u64 {
    let mut tv = anyka_sys::ak_timeval::default();
    // SAFETY: `tv` is a live, correctly sized out-parameter.
    unsafe { anyka_sys::ak_get_ostime(&mut tv) };
    u64::from(tv.sec) * 1000 + u64::from(tv.usec) / 1000
}

/// Local wall-clock date, used for SDK-compatible file names.
#[derive(Debug, Clone, Copy, Default)]
pub struct Date {
    pub year: i32,
    pub month: i32,
    pub day: i32,
    pub hour: i32,
    pub minute: i32,
    pub second: i32,
}

impl Date {
    /// Reads the current local date from the SDK.
    pub fn now() -> Result<Self> {
        let mut date = anyka_sys::ak_date::default();
        // SAFETY: `date` is a live out-parameter of the expected type.
        let ret = unsafe { anyka_sys::ak_get_localdate(&mut date) };
        check("ak_get_localdate", ret)?;
        Ok(Self {
            year: date.year,
            month: date.month,
            day: date.day,
            hour: date.hour,
            minute: date.minute,
            second: date.second,
        })
    }

    /// Formats as `yyyyMMdd-HHmmss`, the layout the C demos use for recordings.
    ///
    /// `ak_date_to_string` writes into a caller-supplied buffer with no length
    /// argument, so the formatting is done in Rust instead.
    pub fn to_filename_stamp(self) -> String {
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// Builds a `<dir>/<timestamp>.<ext>` path, failing if `dir` is not a directory.
pub fn timestamped_path(dir: &std::path::Path, ext: &str) -> Result<std::path::PathBuf> {
    if !dir.is_dir() {
        return Err(Error::invalid(
            "timestamped_path",
            format!("{} is not a directory", dir.display()),
        ));
    }
    Ok(dir.join(format!("{}.{ext}", Date::now()?.to_filename_stamp())))
}
