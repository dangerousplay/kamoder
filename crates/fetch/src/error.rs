//! Error type shared by the client, the downloader and the GitHub helpers.
//!
//! The split that matters to callers is [`Error::is_transient`]: the updater
//! retries (and resumes) on anything the flaky Wi-Fi can cause — refused or
//! reset connections, stalls, 5xx — and gives up at once on answers that will
//! not change by asking again (404, bad hash, bad certificate, full disk).

use std::error::Error as StdError;
use std::path::PathBuf;

use snafu::Snafu;

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Which wait ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Connecting, TLS handshake and the response head.
    Headers,
    /// Silence between two body chunks (idle timeout).
    Body,
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Headers => "response headers",
            Self::Body => "response body",
        })
    }
}

/// Everything that can go wrong fetching a URL.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("invalid URL {url:?}: {reason}"))]
    InvalidUrl { url: String, reason: String },

    #[snafu(display("bad header value for {name}"))]
    InvalidHeader { name: String },

    #[snafu(display("TLS setup failed: {source}"))]
    TlsConfig { source: rustls::Error },

    #[snafu(display("TLS failure talking to {url}: {source}"))]
    Tls {
        url: String,
        source: hyper_util::client::legacy::Error,
    },

    #[snafu(display("cannot connect to {url}: {source}"))]
    Connect {
        url: String,
        source: hyper_util::client::legacy::Error,
    },

    #[snafu(display("request to {url} failed: {source}"))]
    Request {
        url: String,
        source: hyper_util::client::legacy::Error,
    },

    #[snafu(display("reading the body of {url} failed: {source}"))]
    Body { url: String, source: hyper::Error },

    #[snafu(display("timed out waiting for the {stage} of {url}"))]
    Timeout { url: String, stage: Stage },

    #[snafu(display("{url} answered HTTP {status}"))]
    Http { url: String, status: u16 },

    #[snafu(display("{url}: redirect {status} without a usable Location"))]
    MissingLocation { url: String, status: u16 },

    #[snafu(display("{url}: more than {max} redirects"))]
    Redirects { url: String, max: u32 },

    #[snafu(display("{url}: body larger than {max} bytes"))]
    TooLarge { url: String, max: u64 },

    #[snafu(display(
        "{url}: 206 with Content-Range {content_range:?}, expected start {expected}"
    ))]
    BadContentRange {
        url: String,
        expected: u64,
        content_range: String,
    },

    #[snafu(display("{url}: body ended after {got} of {expected} bytes"))]
    Incomplete {
        url: String,
        got: u64,
        expected: u64,
    },

    #[snafu(display("size mismatch: expected {expected} bytes, got {actual}"))]
    SizeMismatch { expected: u64, actual: u64 },

    #[snafu(display("SHA-256 mismatch: expected {}, got {}", hex(expected), hex(actual)))]
    HashMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },

    #[snafu(display("{}: {source}", path.display()))]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("{url}: invalid JSON: {source}"))]
    Json {
        url: String,
        source: serde_json::Error,
    },

    #[snafu(display("GitHub API rate limit exhausted (resets at unix time {reset:?})"))]
    RateLimited { reset: Option<u64> },
}

impl Error {
    /// True when the same request may succeed later: connection trouble,
    /// stalls, truncated bodies, 5xx, 408 and 429. A retry resumes from the
    /// `.part` file, so retrying a half-done download costs nothing extra.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Connect { .. }
            | Self::Request { .. }
            | Self::Body { .. }
            | Self::Timeout { .. }
            | Self::Incomplete { .. } => true,
            Self::Http { status, .. } => *status >= 500 || *status == 408 || *status == 429,
            _ => false,
        }
    }

    /// Sorts a client error into TLS / connect / other request failure.
    pub(crate) fn from_client(url: &str, source: hyper_util::client::legacy::Error) -> Self {
        let url = url.to_owned();
        if is_tls(&source) {
            Self::Tls { url, source }
        } else if source.is_connect() {
            Self::Connect { url, source }
        } else {
            Self::Request { url, source }
        }
    }
}

/// Walks the source chain looking for a rustls error. The connector wraps it
/// in `io::Error`s, whose `source()` skips the inner error, so those are
/// unwrapped by hand.
fn is_tls(err: &(dyn StdError + 'static)) -> bool {
    let mut cur = Some(err);
    while let Some(e) = cur {
        if e.is::<rustls::Error>() {
            return true;
        }
        // The connector nests io::Errors (`Other` around `InvalidData`
        // around the rustls error); step into each one.
        if let Some(inner) = e
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
        {
            cur = Some(inner);
            continue;
        }
        cur = e.source();
    }
    false
}

/// Lower-case hex for digests in messages and logs.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_classification() {
        let http = |status| Error::Http {
            url: String::new(),
            status,
        };
        assert!(http(503).is_transient());
        assert!(http(429).is_transient());
        assert!(!http(404).is_transient());
        assert!(!Error::SizeMismatch {
            expected: 1,
            actual: 2
        }
        .is_transient());
        assert!(Error::Timeout {
            url: String::new(),
            stage: Stage::Body
        }
        .is_transient());
    }

    #[test]
    fn hex_formats() {
        assert_eq!(hex(&[0x00, 0xab, 0x10]), "00ab10");
    }
}
