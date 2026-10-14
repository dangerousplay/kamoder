//! Errors returned by the safe SDK wrappers.

use snafu::Snafu;
use std::ffi::CStr;

/// Failure of an `ak_*` SDK call, or of the argument checking around one.
///
/// The SDK reports errors as a bare `-1` plus a process-global error number
/// that the *next* failing call overwrites, so the number is captured eagerly,
/// together with the operation that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    /// An SDK call returned a failure status.
    #[snafu(display("{op} failed (error {code}){}", detail.as_deref().map(|d| format!(": {d}")).unwrap_or_default()))]
    Sdk {
        op: &'static str,
        code: i32,
        detail: Option<String>,
    },

    /// An SDK call that hands out a handle returned null, with no error number.
    #[snafu(display("{op} returned a null handle"))]
    NullHandle { op: &'static str },

    /// An argument the SDK would reject, or silently misinterpret.
    #[snafu(display("{op}: {reason}"))]
    Invalid { op: &'static str, reason: String },

    /// File or socket I/O around an SDK call.
    #[snafu(display("{message}"))]
    Io { message: String, code: i32 },
}

impl Error {
    /// Captures the SDK's current error number for a call that just failed.
    pub(crate) fn from_sdk(op: &'static str) -> Self {
        // SAFETY: both calls are pure reads of SDK-global error state and take
        // no pointers from us. `ak_get_error_str` returns a pointer to storage
        // owned by the SDK, which is copied into an owned `String` right away.
        let (code, detail) = unsafe {
            let code = anyka_sys::ak_get_error_no();
            let ptr = anyka_sys::ak_get_error_str(code);
            let detail = if ptr.is_null() {
                None
            } else {
                Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
            };
            (code, detail)
        };
        Self::Sdk { op, code, detail }
    }

    /// Error for a call that returns a null handle and sets no error number.
    pub(crate) fn null_handle(op: &'static str) -> Self {
        Self::NullHandle { op }
    }

    /// Error for an argument the SDK would reject or misinterpret.
    pub(crate) fn invalid(op: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            op,
            reason: reason.into(),
        }
    }

    /// The failing SDK function, e.g. `"ak_vi_open"`, where there is one.
    pub fn operation(&self) -> Option<&'static str> {
        match self {
            Error::Sdk { op, .. } | Error::NullHandle { op } | Error::Invalid { op, .. } => {
                Some(op)
            }
            Error::Io { .. } => None,
        }
    }

    /// The SDK error number, or 0 when the failure had none.
    pub fn code(&self) -> i32 {
        match self {
            Error::Sdk { code, .. } | Error::Io { code, .. } => *code,
            _ => 0,
        }
    }
}

/// Lets callers mix file and socket I/O with SDK calls behind a single `?`.
impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io {
            code: err.raw_os_error().unwrap_or(0),
            message: err.to_string(),
        }
    }
}

impl From<Error> for std::io::Error {
    fn from(err: Error) -> Self {
        std::io::Error::other(err.to_string())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Maps the SDK's `0 == success, anything else == failure` convention.
pub(crate) fn check(op: &'static str, ret: i32) -> Result<()> {
    if ret == anyka_sys::AK_OK {
        Ok(())
    } else {
        Err(Error::from_sdk(op))
    }
}
