//! Errors from the V8 SDK.
//!
//! Unlike the EV200 SDK there is no `ak_get_error_no()` here — the V8 calls
//! return their error code directly, as a packed `(module << 24) | code`
//! value (see `enum ak_vi_errno`). So the code is carried as-is and decoded
//! for display rather than fetched from global state.

use snafu::Snafu;

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    /// An SDK call returned a non-zero status.
    #[snafu(display("{op} failed: {}", describe(*code)))]
    Sdk { op: &'static str, code: i32 },

    /// An argument the SDK would reject or silently misinterpret.
    #[snafu(display("{op}: {reason}"))]
    Invalid { op: &'static str, reason: String },

    /// File or path handling around an SDK call.
    #[snafu(display("{message}"))]
    Io { message: String },
}

impl Error {
    pub(crate) fn invalid(op: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            op,
            reason: reason.into(),
        }
    }

    /// The raw SDK status, or 0 when the failure did not come from one.
    pub fn code(&self) -> i32 {
        match self {
            Error::Sdk { code, .. } => *code,
            _ => 0,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io {
            message: err.to_string(),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Splits the packed status into module id and code.
///
/// The SDK builds these as `(MODULE_ID << 24) + n`, so the high byte says
/// which subsystem complained — worth surfacing, because a VI error raised
/// from inside VPSS is a very different bug from a VI error raised by VI.
fn describe(code: i32) -> String {
    if code == 0 {
        return "no error".to_owned();
    }
    let module = (code >> 24) & 0xff;
    let detail = code & 0x00ff_ffff;
    if module == 0 {
        format!("error {code}")
    } else {
        format!("module {module} error {detail} (raw {code:#010x})")
    }
}

/// Maps the SDK's `0 == success` convention.
pub(crate) fn check(op: &'static str, ret: i32) -> Result<()> {
    if ret == 0 {
        Ok(())
    } else {
        Err(Error::Sdk { op, code: ret })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_is_ok() {
        assert!(check("ak_vi_open", 0).is_ok());
    }

    #[test]
    fn packed_status_is_split_into_module_and_code() {
        // ERROR_VI_DEV_NOT_ENABLE = (MODULE_ID_VI << 24) + 5
        let packed = (7 << 24) + 5;
        let err = check("ak_vi_enable_chn", packed).unwrap_err();
        assert_eq!(err.code(), packed);
        let shown = err.to_string();
        assert!(shown.contains("module 7"), "{shown}");
        assert!(shown.contains("error 5"), "{shown}");
        assert!(shown.contains("ak_vi_enable_chn"), "{shown}");
    }

    #[test]
    fn plain_negative_status_still_reported() {
        let err = check("ak_vi_open", -1).unwrap_err();
        assert!(err.to_string().contains("ak_vi_open failed"));
    }
}
