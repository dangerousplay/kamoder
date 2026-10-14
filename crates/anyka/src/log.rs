//! Logging through the SDK's own `ak_print` facility.
//!
//! Vendor components on the camera filter their output by the level set with
//! `ak_print_set_level`, so routing messages through the same call keeps Rust
//! and C output interleaved correctly on the serial console.

use std::ffi::CString;

/// Mirrors `enum LOG_LEVEL` from `ak_common.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i32)]
pub enum Level {
    Error = 1,
    Warning = 2,
    Notice = 3,
    Normal = 4,
    Info = 5,
    Debug = 6,
}

/// Sets the process-wide print level, returning the previous one.
pub fn set_level(level: Level) -> i32 {
    // SAFETY: plain integer argument, no pointers involved.
    unsafe { anyka_sys::ak_print_set_level(level as i32) }
}

/// Prints one line through `ak_print`.
///
/// The message is passed as an argument to a literal `"%s"` format rather than
/// as the format string itself, so a `%` in user data cannot turn into a
/// format-string bug.
pub fn print(level: Level, message: &str) {
    let Ok(text) = CString::new(message.replace('\0', "")) else {
        return;
    };
    const FORMAT: &[u8] = b"%s\n\0";
    // SAFETY: `FORMAT` is a NUL-terminated literal with exactly one `%s`, and
    // `text` is a NUL-terminated string that outlives the call.
    unsafe {
        anyka_sys::ak_print(
            level as i32,
            FORMAT.as_ptr() as *const std::os::raw::c_char,
            text.as_ptr(),
        );
    }
}

/// Logs a formatted message at the given [`Level`].
#[macro_export]
macro_rules! ak_log {
    ($level:expr, $($arg:tt)*) => {
        $crate::log::print($level, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! ak_error {
    ($($arg:tt)*) => { $crate::ak_log!($crate::log::Level::Error, $($arg)*) };
}

#[macro_export]
macro_rules! ak_notice {
    ($($arg:tt)*) => { $crate::ak_log!($crate::log::Level::Notice, $($arg)*) };
}

#[macro_export]
macro_rules! ak_normal {
    ($($arg:tt)*) => { $crate::ak_log!($crate::log::Level::Normal, $($arg)*) };
}

#[macro_export]
macro_rules! ak_info {
    ($($arg:tt)*) => { $crate::ak_log!($crate::log::Level::Info, $($arg)*) };
}
