//! Small HTTPS downloader for the updater (OTA-14, OTA-15; design §6).
//!
//! Why its own crate and not `reqwest`: reqwest's defaults pull HTTP/2, a
//! cookie/proxy/decompression stack and aws-lc, none of which builds or fits
//! on an ARMv5TE board with 32 MB of RAM. This is the minimum the updater
//! needs, on the pieces already in the tree:
//!
//! - hyper 1 HTTP/1 client (`hyper-util` legacy pool) over `hyper-rustls` with
//!   the `ring` backend and Mozilla's roots compiled in (`webpki-roots`) — the
//!   camera firmware carries no CA bundle. HTTPS only.
//! - Redirects followed by hand (≤ [`DEFAULT_MAX_REDIRECTS`]) and re-resolved
//!   on every attempt: GitHub asset URLs redirect to short-lived signed CDN
//!   URLs, so a resume must never reuse an old hop.
//! - Bodies are streamed: hyper's read buffer is capped at [`READ_BUF`]
//!   (64 KiB) and every chunk goes straight to the SHA-256 context and the
//!   file. Nothing ever holds a whole update in RAM.
//! - A token-bucket rate cap (default [`DEFAULT_RATE_LIMIT_BPS`]) because the
//!   SSV6x5x Wi-Fi falls off the network under sustained full-speed transfers.
//! - `Range` resume from `<dest>.part`, see [`Fetcher::download_to`].
//!
//! The crate is runtime-agnostic as far as it can be: it never builds a
//! runtime, and works on the current-thread runtime every service uses.

mod download;
mod error;
pub mod github;
mod rate;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::header::{CONTENT_LENGTH, LOCATION, USER_AGENT};
use hyper::{Request, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use snafu::ResultExt;
use tracing::debug;

pub use download::{part_path, Downloaded, Expected, Progress, RetryPolicy};
pub use error::{hex, Error, Result, Stage};
pub use hyper::header::{HeaderMap, HeaderName, HeaderValue};
pub use hyper::StatusCode;
pub use rustls::pki_types::CertificateDer;

use error::{InvalidHeaderSnafu, InvalidUrlSnafu, JsonSnafu, TlsConfigSnafu};

/// Default rate cap: 256 KiB/s keeps the camera's Wi-Fi up while a 5 MB
/// update still lands in well under a minute.
pub const DEFAULT_RATE_LIMIT_BPS: u32 = 256 * 1024;

/// Default redirect limit. GitHub uses one or two (latest → tag → CDN).
pub const DEFAULT_MAX_REDIRECTS: u32 = 5;

/// Largest chunk held in memory: hyper's read buffer cap and the prefix
/// hashing buffer.
pub const READ_BUF: usize = 64 * 1024;

/// Client settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// `User-Agent` sent on every request (GitHub's API rejects requests
    /// without one).
    pub user_agent: String,
    /// TCP connect timeout. TLS and the response head get this plus
    /// `read_timeout`.
    pub connect_timeout: Duration,
    /// Idle timeout: the longest silence allowed between two body chunks.
    pub read_timeout: Duration,
    /// Redirects followed before giving up.
    pub max_redirects: u32,
    /// Download rate cap in bytes per second; `None` = unlimited.
    pub rate_limit_bps: Option<u32>,
    /// Extra trust anchors on top of the Mozilla set (tests, private mirrors).
    pub extra_roots: Vec<CertificateDer<'static>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            user_agent: concat!("fetch/", env!("CARGO_PKG_VERSION")).to_owned(),
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            rate_limit_bps: Some(DEFAULT_RATE_LIMIT_BPS),
            extra_roots: Vec::new(),
        }
    }
}

/// A buffered response of any status, returned by [`Fetcher::get`].
#[derive(Debug, Clone)]
pub struct Response {
    /// Final URL after redirects.
    pub url: String,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

type HttpClient = Client<HttpsConnector<HttpConnector>, Empty<Bytes>>;

/// HTTPS client. Cheap to share by reference; holds a small idle pool.
pub struct Fetcher {
    client: HttpClient,
    config: Config,
    user_agent: HeaderValue,
    bucket: Option<Mutex<rate::TokenBucket>>,
}

impl std::fmt::Debug for Fetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetcher")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Fetcher {
    /// Builds the TLS config (ring provider, Mozilla roots + `extra_roots`)
    /// and the pooled HTTP/1 client.
    pub fn new(config: Config) -> Result<Self> {
        let user_agent =
            HeaderValue::from_str(&config.user_agent).map_err(|_| Error::InvalidHeader {
                name: USER_AGENT.to_string(),
            })?;

        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for cert in &config.extra_roots {
            roots.add(cert.clone()).context(TlsConfigSnafu)?;
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .context(TlsConfigSnafu)?
        .with_root_certificates(roots)
        .with_no_client_auth();

        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(config.connect_timeout));
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_only()
            .enable_http1()
            .wrap_connector(http);

        let client = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .timer(TokioTimer::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(1)
            .http1_max_buf_size(READ_BUF)
            .build(https);

        let bucket = config
            .rate_limit_bps
            .map(|bps| Mutex::new(rate::TokenBucket::new(bps, Instant::now())));
        Ok(Self {
            client,
            config,
            user_agent,
            bucket,
        })
    }

    /// The settings this client was built with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// GETs `url` (following redirects) and buffers a body of at most
    /// `max_len` bytes. Any status is returned; see [`Self::get_bytes`] for
    /// the "2xx or error" form.
    pub async fn get(&self, url: &str, headers: &HeaderMap, max_len: u64) -> Result<Response> {
        let (uri, resp) = self.send(url, headers).await?;
        let final_url = uri.to_string();
        let (parts, mut body) = resp.into_parts();
        let too_large = || Error::TooLarge {
            url: final_url.clone(),
            max: max_len,
        };
        let declared = content_length(&parts.headers);
        if declared.is_some_and(|n| n > max_len) {
            return Err(too_large());
        }
        let cap = declared.unwrap_or(0).min(max_len).min(READ_BUF as u64) as usize;
        let mut buf = BytesMut::with_capacity(cap);
        while let Some(chunk) = self.next_chunk(&final_url, &mut body).await? {
            if (buf.len() + chunk.len()) as u64 > max_len {
                return Err(too_large());
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(Response {
            url: final_url,
            status: parts.status,
            headers: parts.headers,
            body: buf.freeze(),
        })
    }

    /// GETs a small body (manifest, signature, JSON), at most `max_len`
    /// bytes; non-2xx is [`Error::Http`].
    pub async fn get_bytes(&self, url: &str, max_len: u64) -> Result<Bytes> {
        self.get_bytes_with(url, &HeaderMap::new(), max_len).await
    }

    /// [`Self::get_bytes`] with extra request headers (`Accept`, API version).
    pub async fn get_bytes_with(
        &self,
        url: &str,
        headers: &HeaderMap,
        max_len: u64,
    ) -> Result<Bytes> {
        let resp = self.get(url, headers, max_len).await?;
        if !resp.status.is_success() {
            return Err(Error::Http {
                url: resp.url,
                status: resp.status.as_u16(),
            });
        }
        Ok(resp.body)
    }

    /// GETs and deserializes a JSON body of at most `max_len` bytes.
    pub async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        max_len: u64,
    ) -> Result<T> {
        self.get_json_with(url, &HeaderMap::new(), max_len).await
    }

    /// [`Self::get_json`] with extra request headers.
    pub async fn get_json_with<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        headers: &HeaderMap,
        max_len: u64,
    ) -> Result<T> {
        let body = self.get_bytes_with(url, headers, max_len).await?;
        serde_json::from_slice(&body).context(JsonSnafu { url })
    }

    /// Sends a GET, following redirects. Headers (including `Range`) are sent
    /// on every hop; the hop URLs are never cached.
    async fn send(
        &self,
        url: &str,
        headers: &HeaderMap,
    ) -> Result<(Uri, hyper::Response<Incoming>)> {
        let mut uri = parse_url(url)?;
        for _ in 0..=self.config.max_redirects {
            let mut req = Request::get(uri.clone())
                .body(Empty::<Bytes>::new())
                .map_err(|e| Error::InvalidUrl {
                    url: uri.to_string(),
                    reason: e.to_string(),
                })?;
            req.headers_mut()
                .insert(USER_AGENT, self.user_agent.clone());
            req.headers_mut().extend(headers.clone());

            let wait = self.config.connect_timeout + self.config.read_timeout;
            let current = uri.to_string();
            let resp = tokio::time::timeout(wait, self.client.request(req))
                .await
                .map_err(|_| Error::Timeout {
                    url: current.clone(),
                    stage: Stage::Headers,
                })?
                .map_err(|e| Error::from_client(&current, e))?;

            let status = resp.status().as_u16();
            if !matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Ok((uri, resp));
            }
            let next = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|loc| resolve_location(&uri, loc))
                .ok_or_else(|| Error::MissingLocation {
                    url: current.clone(),
                    status,
                })?;
            debug!(from = %current, to = %next, status, "redirect");
            uri = next;
        }
        Err(Error::Redirects {
            url: url.to_owned(),
            max: self.config.max_redirects,
        })
    }

    /// Next non-empty data chunk, with the idle timeout and the rate cap
    /// applied. `None` at the end of the body.
    async fn next_chunk(&self, url: &str, body: &mut Incoming) -> Result<Option<Bytes>> {
        loop {
            let frame = tokio::time::timeout(self.config.read_timeout, body.frame())
                .await
                .map_err(|_| Error::Timeout {
                    url: url.to_owned(),
                    stage: Stage::Body,
                })?;
            let frame = match frame {
                None => return Ok(None),
                Some(frame) => frame.map_err(|source| Error::Body {
                    url: url.to_owned(),
                    source,
                })?,
            };
            // Trailers carry nothing we use.
            let Ok(data) = frame.into_data() else {
                continue;
            };
            if data.is_empty() {
                continue;
            }
            self.throttle(data.len()).await;
            return Ok(Some(data));
        }
    }

    async fn throttle(&self, n: usize) {
        let Some(bucket) = &self.bucket else { return };
        let wait = bucket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(n, Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

/// Builds a header map from static names and string values, e.g. for
/// `Accept` and `X-GitHub-Api-Version`.
pub fn headers<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        let header = HeaderName::from_bytes(name.as_bytes()).ok();
        let value = HeaderValue::from_str(value).ok();
        let (Some(header), Some(value)) = (header, value) else {
            return InvalidHeaderSnafu { name }.fail();
        };
        map.append(header, value);
    }
    Ok(map)
}

fn parse_url(url: &str) -> Result<Uri> {
    let uri: Uri = url
        .parse()
        .map_err(|e: hyper::http::uri::InvalidUri| Error::InvalidUrl {
            url: url.to_owned(),
            reason: e.to_string(),
        })?;
    if uri.scheme_str() != Some("https") || uri.authority().is_none() {
        return InvalidUrlSnafu {
            url,
            reason: "only absolute https:// URLs are supported",
        }
        .fail();
    }
    Ok(uri)
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok())
}

/// Resolves a `Location` value against the URL that returned it (RFC 3986
/// reference resolution, minus dot-segment removal, which no real redirect
/// target needs).
fn resolve_location(base: &Uri, loc: &str) -> Option<Uri> {
    let loc = loc.trim();
    if loc.contains("://") {
        return loc.parse().ok();
    }
    let scheme = base.scheme_str()?;
    let authority = base.authority()?.as_str();
    let joined = if let Some(rest) = loc.strip_prefix("//") {
        format!("{scheme}://{rest}")
    } else if loc.starts_with('/') {
        format!("{scheme}://{authority}{loc}")
    } else if loc.starts_with('?') {
        format!("{scheme}://{authority}{}{loc}", base.path())
    } else {
        let path = base.path();
        let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
        let dir = if dir.is_empty() { "/" } else { dir };
        format!("{scheme}://{authority}{dir}{loc}")
    };
    joined.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(base: &str, loc: &str) -> String {
        resolve_location(&base.parse().unwrap(), loc)
            .unwrap()
            .to_string()
    }

    #[test]
    fn location_resolution() {
        let base = "https://github.com/o/r/releases/latest/download/a.kup";
        assert_eq!(
            res(base, "https://cdn.example/x?sig=1"),
            "https://cdn.example/x?sig=1"
        );
        assert_eq!(res(base, "//cdn.example/y"), "https://cdn.example/y");
        assert_eq!(
            res(base, "/o/r/releases/download/v1/a.kup"),
            "https://github.com/o/r/releases/download/v1/a.kup"
        );
        assert_eq!(
            res(base, "b.kup"),
            "https://github.com/o/r/releases/latest/download/b.kup"
        );
        assert_eq!(
            res(base, "?x=1"),
            "https://github.com/o/r/releases/latest/download/a.kup?x=1"
        );
    }

    #[test]
    fn only_https() {
        assert!(parse_url("http://example.com/").is_err());
        assert!(parse_url("/relative").is_err());
        assert!(parse_url("https://example.com/a").is_ok());
    }

    #[test]
    fn header_helper() {
        let h = headers([("Accept", "application/json"), ("X-A", "1")]).unwrap();
        assert_eq!(h["accept"], "application/json");
        assert!(headers([("bad name", "x")]).is_err());
    }
}
