//! Snapshot and MJPEG live view (T16, VID-01/02).
//!
//! The camera's JPEGs come from the SDK-based `jpeg_snapshot` service
//! (default `http://127.0.0.1:3000/snapshot.jpg`); this module only proxies.
//! The client is a few lines of HTTP/1.0 over a `TcpStream` rather than a
//! crate: one GET to localhost, `Connection: close`, body read to EOF — an
//! HTTP client dependency would be far more code than the protocol needs.
//!
//! `/api/live.mjpeg` is the same snapshot fetched every
//! [`Config::mjpeg_interval`](crate::state::Config) and sent as
//! `multipart/x-mixed-replace`, which an `<img>` plays natively. One client
//! at a time (each frame is a JPEG encode on the 400 MHz core); a second gets
//! 503 `busy`. When video is down both routes answer 503 `video_unavailable`
//! with the reason, so the UI can say why.

use crate::auth::StreamUser;
use crate::error::ApiError;
use crate::state::AppState;
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Largest JPEG accepted from the snapshot service.
pub const MAX_JPEG: usize = 1024 * 1024;
/// Largest response head accepted.
const MAX_HEAD: usize = 8 * 1024;
/// Multipart boundary of the MJPEG stream.
const BOUNDARY: &str = "anykaframe";
/// A live stream ends after this many failed snapshots in a row.
const MAX_FAILURES: u32 = 10;

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/snapshot.jpg", get(snapshot))
        .route("/live.mjpeg", get(live))
}

/// `?camera=N`: which image sensor, on boards with more than one.
#[derive(Debug, Default, serde::Deserialize)]
pub struct CameraQuery {
    pub camera: Option<u8>,
}

/// The snapshot URL for `camera`: the service's own `camera` query
/// parameter, appended so a configured URL with a query keeps it.
pub fn camera_url(base: &str, camera: Option<u8>) -> String {
    match camera {
        None => base.to_owned(),
        Some(n) if base.contains('?') => format!("{base}&camera={n}"),
        Some(n) => format!("{base}?camera={n}"),
    }
}

/// One image from the snapshot service.
#[derive(Debug, Clone)]
pub struct Frame {
    pub content_type: String,
    pub body: Bytes,
}

/// `http://host[:port]/path` → (`host:port`, host, path).
pub(crate) fn parse_url(url: &str) -> Result<(String, String, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("snapshot URL {url:?} is not http://"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(format!("snapshot URL {url:?} has no host"));
    }
    let has_port = match authority.rfind(':') {
        Some(i) => !authority[i..].contains(']'),
        None => false,
    };
    let address = if has_port {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };
    Ok((address, authority.to_owned(), path.to_owned()))
}

/// Fetches one image; `Err` carries a human reason.
pub async fn fetch(url: &str, timeout: Duration) -> Result<Frame, String> {
    tokio::time::timeout(timeout, fetch_inner(url))
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "snapshot service did not answer within {} s",
                timeout.as_secs_f32()
            ))
        })
}

async fn fetch_inner(url: &str) -> Result<Frame, String> {
    let (address, host, path) = parse_url(url)?;
    let mut stream = TcpStream::connect(&address)
        .await
        .map_err(|e| format!("snapshot service at {address} unreachable: {e}"))?;
    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\nAccept: image/jpeg\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("snapshot request failed: {e}"))?;
    let mut data = Vec::with_capacity(64 * 1024);
    let limit = (MAX_HEAD + MAX_JPEG + 1) as u64;
    (&mut stream)
        .take(limit)
        .read_to_end(&mut data)
        .await
        .map_err(|e| format!("snapshot read failed: {e}"))?;
    parse_response(data)
}

fn parse_response(mut data: Vec<u8>) -> Result<Frame, String> {
    let end = data
        .windows(4)
        .take(MAX_HEAD)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed response from the snapshot service")?;
    let head = String::from_utf8_lossy(&data[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or("malformed status line from the snapshot service")?;
    if status != 200 {
        return Err(format!("snapshot service answered HTTP {status}"));
    }
    let mut content_type = None;
    let mut length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        }
    }
    let body = data.split_off(end + 4);
    if body.len() > MAX_JPEG || length.is_some_and(|l| l > MAX_JPEG) {
        return Err(format!("snapshot larger than {} KiB", MAX_JPEG / 1024));
    }
    if let Some(length) = length {
        if body.len() < length {
            return Err("snapshot truncated".into());
        }
    }
    let mut body = body;
    if let Some(length) = length {
        body.truncate(length);
    }
    if body.is_empty() {
        return Err("snapshot service sent an empty image".into());
    }
    let content_type = content_type
        .filter(|t| t.starts_with("image/"))
        .unwrap_or_else(|| "image/jpeg".to_owned());
    Ok(Frame {
        content_type,
        body: Bytes::from(body),
    })
}

fn unavailable(reason: String) -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "video_unavailable",
        format!("video unavailable: {reason}"),
    )
}

/// `GET /api/snapshot.jpg` (stream token allowed) → one JPEG.
async fn snapshot(
    State(state): State<AppState>,
    _: StreamUser,
    Query(query): Query<CameraQuery>,
) -> Result<Response, ApiError> {
    let url = camera_url(&state.config.snapshot_url, query.camera);
    let frame = fetch(&url, state.tuning.snapshot_timeout)
        .await
        .map_err(unavailable)?;
    Ok((
        [
            (header::CONTENT_TYPE, frame.content_type),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        frame.body,
    )
        .into_response())
}

/// One multipart part.
fn part(frame: &Frame) -> Bytes {
    let mut out = Vec::with_capacity(frame.body.len() + 128);
    out.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n",
            frame.content_type,
            frame.body.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&frame.body);
    out.extend_from_slice(b"\r\n");
    Bytes::from(out)
}

/// `GET /api/live.mjpeg` (stream token allowed) → `multipart/x-mixed-replace`
/// JPEG stream until the client leaves, the server stops, or video fails
/// [`MAX_FAILURES`] times in a row.
async fn live(
    State(state): State<AppState>,
    StreamUser(user): StreamUser,
    Query(query): Query<CameraQuery>,
) -> Result<Response, ApiError> {
    let permit = state.mjpeg.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "the live view is open in another window; one viewer at a time",
        )
        .retry_after(Duration::from_secs(10))
    })?;
    let url = camera_url(&state.config.snapshot_url, query.camera);
    let timeout = state.tuning.snapshot_timeout;
    let first = fetch(&url, timeout).await.map_err(unavailable)?;
    tracing::debug!(user = %user.user, "live view opened");

    struct Live {
        first: Option<Frame>,
        _permit: tokio::sync::OwnedSemaphorePermit,
        shutdown: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        ticker: tokio::time::Interval,
        url: String,
        timeout: Duration,
    }
    let mut ticker =
        tokio::time::interval(state.config.mjpeg_interval.max(Duration::from_millis(20)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let initial = Live {
        first: Some(first),
        _permit: permit,
        shutdown: Box::pin(state.shutdown_signal()),
        ticker,
        url,
        timeout,
    };
    let body = futures_util::stream::unfold(initial, |mut s| async move {
        if let Some(frame) = s.first.take() {
            // The interval's first tick is immediate; use it up here.
            s.ticker.tick().await;
            return Some((Ok::<_, std::convert::Infallible>(part(&frame)), s));
        }
        let mut failures = 0;
        loop {
            tokio::select! {
                () = &mut s.shutdown => return None,
                _ = s.ticker.tick() => {}
            }
            match fetch(&s.url, s.timeout).await {
                Ok(frame) => return Some((Ok(part(&frame)), s)),
                Err(reason) => {
                    failures += 1;
                    tracing::debug!(%reason, failures, "live view frame failed");
                    if failures >= MAX_FAILURES {
                        tracing::warn!(%reason, "live view ended: video unavailable");
                        return None;
                    }
                }
            }
        }
    });
    Ok((
        [
            (
                header::CONTENT_TYPE,
                format!("multipart/x-mixed-replace; boundary={BOUNDARY}"),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        Body::from_stream(body),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            parse_url("http://127.0.0.1:3000/snapshot.jpg").unwrap(),
            (
                "127.0.0.1:3000".into(),
                "127.0.0.1:3000".into(),
                "/snapshot.jpg".into()
            )
        );
        assert_eq!(parse_url("http://cam").unwrap().0, "cam:80");
        assert_eq!(parse_url("http://[::1]/x").unwrap().0, "[::1]:80");
        assert!(parse_url("https://x/").is_err());
    }

    #[test]
    fn camera_selection() {
        let base = "http://127.0.0.1:3000/snapshot.jpg";
        assert_eq!(camera_url(base, None), base);
        assert_eq!(camera_url(base, Some(1)), format!("{base}?camera=1"));
        assert_eq!(
            camera_url("http://h/s?q=1", Some(0)),
            "http://h/s?q=1&camera=0"
        );
    }

    #[test]
    fn responses() {
        let ok = parse_response(
            b"HTTP/1.0 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: 3\r\n\r\nabcdef"
                .to_vec(),
        )
        .unwrap();
        assert_eq!(&ok.body[..], b"abc");
        assert!(parse_response(b"HTTP/1.1 500 Oops\r\n\r\n".to_vec())
            .unwrap_err()
            .contains("500"));
        assert!(
            parse_response(b"HTTP/1.0 200 OK\r\nContent-Length: 10\r\n\r\nabc".to_vec()).is_err()
        );
        assert!(parse_response(b"garbage".to_vec()).is_err());
        let plain =
            parse_response(b"HTTP/1.0 200 OK\r\nContent-Type: text/html\r\n\r\nxyz".to_vec())
                .unwrap();
        assert_eq!(plain.content_type, "image/jpeg");
    }
}
