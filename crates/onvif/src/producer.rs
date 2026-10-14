//! Client of the video producer's `GET /status` (the `camera` service on
//! loopback, [`video_proto::DEFAULT_PRODUCER`]), from which the Media
//! service learns each sensor's stream size, frame rate, bitrate and GOP
//! (PRO-02).
//!
//! The client is a few lines of HTTP/1.0 over a tokio `TcpStream`, the same
//! approach as web's `cameras.rs`: the producer answers one small JSON
//! document on loopback, so an HTTP client crate would add code to every
//! binary for nothing.
//!
//! ```text
//! -> GET /status HTTP/1.0
//!    Host: 127.0.0.1:3000
//!    Connection: close
//! <- HTTP/1.0 200 OK
//!    ...
//!
//!    [{"index":0,"state":"running",...,"stream":{"width":1280,...}}, ...]
//! ```
//!
//! NVRs call GetProfiles and the configuration getters in bursts, so an
//! answer — including "unreachable" — is cached for [`CACHE_TTL`], and a
//! fetch gives up after [`FETCH_TIMEOUT`]: a hung producer then costs one
//! timeout per few seconds, not one per SOAP request.

use snafu::{ResultExt, Snafu};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use video_proto::Status;

/// How long one `/status` request may take, connect to last byte. The
/// producer is on loopback and answers from memory.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(1);

/// How long an answer (or a failure) is reused.
pub const CACHE_TTL: Duration = Duration::from_secs(5);

/// Largest reply accepted, bytes. A sensor's status is a few hundred bytes.
const MAX_REPLY: u64 = 64 * 1024;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("producer {address} unreachable: {source}"))]
    Connect {
        address: String,
        source: std::io::Error,
    },
    #[snafu(display("producer {address}: {source}"))]
    Io {
        address: String,
        source: std::io::Error,
    },
    #[snafu(display("producer {address} did not answer within {timeout:?}"))]
    Timeout { address: String, timeout: Duration },
    #[snafu(display("malformed reply from the producer"))]
    Malformed,
    #[snafu(display("producer answered HTTP {code}"))]
    HttpStatus { code: u16 },
    #[snafu(display("bad producer status: {source}"))]
    Json { source: serde_json::Error },
}

type Result<T, E = Error> = std::result::Result<T, E>;

/// `GET http://<address>/status`, within `timeout`.
pub async fn fetch(address: &str, timeout: Duration) -> Result<Vec<Status>> {
    match tokio::time::timeout(timeout, fetch_unbounded(address)).await {
        Ok(result) => result,
        Err(_) => TimeoutSnafu { address, timeout }.fail(),
    }
}

async fn fetch_unbounded(address: &str) -> Result<Vec<Status>> {
    let mut stream = TcpStream::connect(address)
        .await
        .context(ConnectSnafu { address })?;
    let request = format!("GET /status HTTP/1.0\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .context(IoSnafu { address })?;
    let mut reply = Vec::new();
    (&mut stream)
        .take(MAX_REPLY)
        .read_to_end(&mut reply)
        .await
        .context(IoSnafu { address })?;
    parse_reply(&reply)
}

/// The statuses in a whole HTTP reply.
fn parse_reply(reply: &[u8]) -> Result<Vec<Status>> {
    let end = reply
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(Error::Malformed)?;
    let head = std::str::from_utf8(&reply[..end]).map_err(|_| Error::Malformed)?;
    let code: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or(Error::Malformed)?;
    if code != 200 {
        return HttpStatusSnafu { code }.fail();
    }
    serde_json::from_slice(&reply[end + 4..]).context(JsonSnafu)
}

/// A producer whose `/status` is fetched at most once per [`CACHE_TTL`].
#[derive(Debug)]
pub struct Producer {
    address: String,
    timeout: Duration,
    ttl: Duration,
    /// When the last answer was fetched; `None` in it = unreachable.
    cache: Mutex<Option<(Instant, Option<Vec<Status>>)>>,
}

impl Producer {
    /// `address` is `host:port`.
    pub fn new(address: impl Into<String>) -> Self {
        Self::with_timing(address, FETCH_TIMEOUT, CACHE_TTL)
    }

    pub fn with_timing(address: impl Into<String>, timeout: Duration, ttl: Duration) -> Self {
        Self {
            address: address.into(),
            timeout,
            ttl,
            cache: Mutex::new(None),
        }
    }

    /// The producer's sensors, or `None` while it cannot be reached or
    /// answers nonsense (logged at debug; the caller decides what to do).
    pub async fn statuses(&self) -> Option<Vec<Status>> {
        if let Some((at, cached)) = &*self.lock() {
            if at.elapsed() < self.ttl {
                return cached.clone();
            }
        }
        // Not holding the lock across the fetch: two requests racing past
        // an expired entry both fetch, which is harmless.
        let fresh = match fetch(&self.address, self.timeout).await {
            Ok(statuses) => Some(statuses),
            Err(error) => {
                tracing::debug!(%error, "producer status unavailable");
                None
            }
        };
        *self.lock() = Some((Instant::now(), fresh.clone()));
        fresh
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(Instant, Option<Vec<Status>>)>> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A fake producer for tests: answers every connection with `statuses`.
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::AsyncBufReadExt;

    /// Serves `statuses` on an ephemeral loopback port; returns its
    /// `host:port` and a count of requests answered.
    pub async fn serve(statuses: Vec<Status>) -> (String, Arc<AtomicUsize>) {
        let body = serde_json::to_string(&statuses).unwrap();
        serve_raw(format!(
            "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ))
        .await
    }

    /// Serves a fixed reply.
    pub async fn serve_raw(reply: String) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let count = Arc::new(AtomicUsize::new(0));
        let served = Arc::clone(&count);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (read, mut write) = stream.into_split();
                let mut lines = tokio::io::BufReader::new(read).lines();
                // The request line and headers, up to the empty line.
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.is_empty() {
                        break;
                    }
                }
                served.fetch_add(1, Ordering::SeqCst);
                let _ = write.write_all(reply.as_bytes()).await;
            }
        });
        (address, count)
    }

    /// A loopback address nothing listens on.
    pub fn dead_address() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use video_proto::StreamInfo;

    fn status(index: u32, width: u32) -> Status {
        Status {
            index,
            state: "running".into(),
            stream: Some(StreamInfo {
                width,
                ..StreamInfo::default()
            }),
            ..Status::default()
        }
    }

    #[test]
    fn reply_parsing() {
        let ok = b"HTTP/1.0 200 OK\r\nX: y\r\n\r\n[{\"index\":1,\"state\":\"running\"}]";
        let statuses = parse_reply(ok).unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].index, 1);
        assert!(statuses[0].stream.is_none());

        assert!(matches!(
            parse_reply(b"HTTP/1.0 503 Busy\r\n\r\n[]"),
            Err(Error::HttpStatus { code: 503 })
        ));
        assert!(matches!(
            parse_reply(b"HTTP/1.0 200"),
            Err(Error::Malformed)
        ));
        assert!(matches!(
            parse_reply(b"HTTP/1.0 200 OK\r\n\r\nnot json"),
            Err(Error::Json { .. })
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetches_from_a_producer() {
        let (address, _) = fake::serve(vec![status(0, 1280), status(1, 640)]).await;
        let statuses = fetch(&address, FETCH_TIMEOUT).await.unwrap();
        assert_eq!(statuses, vec![status(0, 1280), status(1, 640)]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unreachable_and_silent_producers_fail() {
        let error = fetch(&fake::dead_address(), FETCH_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Connect { .. }), "{error}");

        // Accepts but never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let error = fetch(&address, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Timeout { .. }), "{error}");
        drop(listener);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn answers_are_cached() {
        let (address, count) = fake::serve(vec![status(0, 1280)]).await;
        let producer = Producer::new(&address);
        assert_eq!(producer.statuses().await.unwrap().len(), 1);
        assert_eq!(producer.statuses().await.unwrap().len(), 1);
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Expired: fetched again.
        let producer = Producer::with_timing(&address, FETCH_TIMEOUT, Duration::ZERO);
        producer.statuses().await;
        producer.statuses().await;
        assert_eq!(count.load(Ordering::SeqCst), 3);

        // Failures are cached too, and are `None`.
        let producer = Producer::new(fake::dead_address());
        assert!(producer.statuses().await.is_none());
        assert!(producer.statuses().await.is_none());
    }
}
