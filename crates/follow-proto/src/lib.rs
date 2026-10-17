//! Protocol of the follow-movement control socket, and an async client for it.
//!
//! The `follow` service tracks motion from the ISP grid and moves the PTZ head
//! to keep the subject centred. It serves [`DEFAULT_SOCKET`] (mode 0600) so
//! `web` can show what it is doing and switch it on or off without restarting
//! anything (FOL-16, decision D-F3). This crate holds the wire types and the
//! client and nothing of the tracker, so `web` never links the detector (the
//! same split as `ptz-proto` and `lights-proto`).
//!
//! The wire format mirrors the PTZ, lights and supervisor sockets: JSON lines,
//! one [`Request`] object plus `\n` from the client, one [`Response`] object
//! plus `\n` back. A connection may carry several requests in sequence. Lines
//! longer than [`MAX_LINE`] bytes are rejected. `set` answers with `ok`; ask
//! for `status` to see the effect.
//!
//! ```text
//! -> {"op":"status"}
//! <- {"result":"status","enabled":true,"state":"tracking",
//!     "target":{"x_permille":640,"y_permille":480,"cells":6},"moves":12}
//! -> {"op":"set","enabled":false}
//! <- {"result":"ok"}
//! -> {"op":"status"}
//! <- {"result":"status","enabled":false,"state":"disabled","target":null,"moves":12}
//! -> {"op":"set"}
//! <- {"result":"error","code":"bad_request","message":"missing field `enabled`"}
//! ```
//!
//! # Terms
//!
//! - **enabled**: the persisted on/off switch (`etc/follow.json`).
//! - **state**: what the tracker is doing right now, see [`State`].
//! - **target**: the subject being followed, in frame coordinates scaled to
//!   0..=1000 (`0,0` = top left), with the number of grid cells it covers.
//! - **moves**: PTZ moves the tracker has commanded since it started.

use serde::{Deserialize, Serialize};
use snafu::{ensure, ResultExt, Snafu};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Where the follow service listens unless told otherwise (`/tmp` is a tmpfs).
pub const DEFAULT_SOCKET: &str = "/tmp/kamoder-follow.sock";

/// Longest accepted request line, in bytes, excluding the newline.
pub const MAX_LINE: usize = 1024;

/// Longest response line the client accepts.
pub const MAX_RESPONSE: usize = 4 * 1024;

/// Default round-trip timeout of [`Client`]. Answers come from memory (a
/// `set` also writes a small file), so this only has to cover a busy service.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// What the tracker is doing (FOL-15 `kamoder_follow_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Enabled, watching, no target.
    Idle,
    /// A target is being followed (and is inside the dead zone or just moved).
    Tracking,
    /// The head is moving, or just stopped; detection is suspended (FOL-10).
    Settling,
    /// The target was lost long enough; going back home (FOL-11).
    Returning,
    /// A manual PTZ command came in; tracking holds off (FOL-12).
    Paused,
    /// Switched off.
    Disabled,
    /// Enabled but cannot work: the motion grid or the PTZ socket is missing.
    Unavailable,
}

impl State {
    /// Every state, in metric-label order.
    pub const ALL: [State; 7] = [
        State::Idle,
        State::Tracking,
        State::Settling,
        State::Returning,
        State::Paused,
        State::Disabled,
        State::Unavailable,
    ];

    /// Wire name, for logs and metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Tracking => "tracking",
            State::Settling => "settling",
            State::Returning => "returning",
            State::Paused => "paused",
            State::Disabled => "disabled",
            State::Unavailable => "unavailable",
        }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The subject being followed, in frame coordinates scaled to 0..=1000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// Centroid, 0 = left edge, 1000 = right edge.
    pub x_permille: u16,
    /// Centroid, 0 = top edge, 1000 = bottom edge.
    pub y_permille: u16,
    /// Motion-grid cells the subject covers.
    pub cells: u16,
}

/// Everything `status` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowStatus {
    /// The persisted switch.
    pub enabled: bool,
    pub state: State,
    /// The current target, if any.
    pub target: Option<Target>,
    /// PTZ moves commanded since the service started.
    pub moves: u64,
}

/// One request, as sent by a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// The current [`FollowStatus`].
    Status,
    /// Switch following on or off; persisted across restarts.
    Set { enabled: bool },
}

impl Request {
    /// Short name of the operation, for logs and metrics labels.
    pub fn op(&self) -> &'static str {
        match self {
            Request::Status => "status",
            Request::Set { .. } => "set",
        }
    }
}

/// One response, as sent by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// Answer to [`Request::Status`]; the fields sit next to `result`.
    Status(FollowStatus),
    /// The request was carried out.
    Ok,
    /// The request was not carried out.
    Error { code: ErrorCode, message: String },
}

/// Machine-readable reason a request failed. Suggested HTTP mapping for
/// `web`: `bad_request`/`too_long` → 400, `store` → 500 (and a connect
/// failure → 503 `follow_unavailable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The line was not a valid request (unknown op, missing field, not JSON).
    BadRequest,
    /// The line exceeded [`MAX_LINE`].
    TooLong,
    /// The switch could not be persisted; nothing changed.
    Store,
}

impl ErrorCode {
    /// Wire name, for metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::TooLong => "too_long",
            ErrorCode::Store => "store",
        }
    }
}

/// Errors from [`Client`]: the service could not be asked at all.
#[derive(Debug, Snafu)]
#[non_exhaustive]
pub enum ClientError {
    #[snafu(display("cannot connect to {}: {source}", path.display()))]
    Connect {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("cannot encode the request: {source}"))]
    Encode { source: serde_json::Error },

    #[snafu(display("I/O error on the follow socket: {source}"))]
    Io { source: std::io::Error },

    #[snafu(display("the follow service closed the connection without answering"))]
    Closed,

    #[snafu(display("the follow service's response exceeds {MAX_RESPONSE} bytes"))]
    ResponseTooLong,

    #[snafu(display("cannot decode the follow service's response: {source}"))]
    Decode { source: serde_json::Error },

    #[snafu(display("no response from the follow service within {} ms", timeout.as_millis()))]
    Timeout { timeout: Duration },
}

/// Client for the follow socket. One connection per request: requests are
/// rare and nothing goes stale when the service restarts.
#[derive(Debug, Clone)]
pub struct Client {
    path: PathBuf,
    timeout: Duration,
}

impl Default for Client {
    fn default() -> Self {
        Self::new(DEFAULT_SOCKET)
    }
}

impl Client {
    /// A client for the socket at `path`, with [`DEFAULT_TIMEOUT`].
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Replaces the round-trip timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The socket this client talks to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sends one request and waits for its response. A
    /// [`Response::Error`] is a successful round trip and is returned as `Ok`.
    pub async fn request(&self, req: &Request) -> Result<Response, ClientError> {
        let timeout = self.timeout;
        tokio::time::timeout(timeout, self::request(&self.path, req))
            .await
            .unwrap_or(Err(ClientError::Timeout { timeout }))
    }

    /// [`Request::Status`].
    pub async fn status(&self) -> Result<Response, ClientError> {
        self.request(&Request::Status).await
    }

    /// [`Request::Set`].
    pub async fn set_enabled(&self, enabled: bool) -> Result<Response, ClientError> {
        self.request(&Request::Set { enabled }).await
    }
}

/// Sends one request over a fresh connection to `path` and reads the
/// response. No timeout of its own; use [`Client`], which applies one.
pub async fn request(path: impl AsRef<Path>, request: &Request) -> Result<Response, ClientError> {
    let path = path.as_ref();
    let stream = UnixStream::connect(path)
        .await
        .context(ConnectSnafu { path })?;
    let (read, mut write) = stream.into_split();

    let mut line = serde_json::to_vec(request).context(EncodeSnafu)?;
    line.push(b'\n');
    write.write_all(&line).await.context(IoSnafu)?;
    write.flush().await.context(IoSnafu)?;

    let mut reader = BufReader::new(read).take(MAX_RESPONSE as u64 + 1);
    let mut buf = Vec::new();
    reader.read_until(b'\n', &mut buf).await.context(IoSnafu)?;
    if buf.last() != Some(&b'\n') {
        ensure!(buf.len() <= MAX_RESPONSE, ResponseTooLongSnafu);
        return ClosedSnafu.fail();
    }
    serde_json::from_slice(&buf).context(DecodeSnafu)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_use_the_documented_wire_format() {
        let cases = [
            (Request::Status, json!({"op": "status"})),
            (
                Request::Set { enabled: true },
                json!({"op": "set", "enabled": true}),
            ),
            (
                Request::Set { enabled: false },
                json!({"op": "set", "enabled": false}),
            ),
        ];
        for (request, wire) in cases {
            assert_eq!(serde_json::to_value(request).unwrap(), wire);
            let back: Request = serde_json::from_value(wire).unwrap();
            assert_eq!(back, request);
            assert_eq!(request.op(), back.op());
        }
    }

    #[test]
    fn malformed_requests_are_rejected() {
        for bad in [
            r#"{"op":"reboot"}"#,
            r#"{"op":"set"}"#,
            r#"{"op":"set","enabled":"yes"}"#,
            r#"{"op":"set","enabled":1}"#,
            r#"{"op":"set","enabled":true,"speed":3}"#,
            "not json",
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn responses_use_the_documented_wire_format() {
        let tracking = FollowStatus {
            enabled: true,
            state: State::Tracking,
            target: Some(Target {
                x_permille: 640,
                y_permille: 480,
                cells: 6,
            }),
            moves: 12,
        };
        let disabled = FollowStatus {
            enabled: false,
            state: State::Disabled,
            target: None,
            moves: 0,
        };
        let cases = [
            (
                Response::Status(tracking),
                json!({"result": "status", "enabled": true, "state": "tracking",
                       "target": {"x_permille": 640, "y_permille": 480, "cells": 6},
                       "moves": 12}),
            ),
            (
                Response::Status(disabled),
                json!({"result": "status", "enabled": false, "state": "disabled",
                       "target": null, "moves": 0}),
            ),
            (Response::Ok, json!({"result": "ok"})),
            (
                Response::Error {
                    code: ErrorCode::Store,
                    message: "m".into(),
                },
                json!({"result": "error", "code": "store", "message": "m"}),
            ),
        ];
        for (response, wire) in cases {
            let text = serde_json::to_string(&response).unwrap();
            assert!(!text.contains('\n'));
            assert_eq!(serde_json::to_value(&response).unwrap(), wire);
            let back: Response = serde_json::from_value(wire).unwrap();
            assert_eq!(back, response);
        }
        for code in [ErrorCode::BadRequest, ErrorCode::TooLong, ErrorCode::Store] {
            assert_eq!(serde_json::to_value(code).unwrap(), json!(code.as_str()));
        }
    }

    #[test]
    fn states_use_their_wire_names() {
        for state in State::ALL {
            assert_eq!(serde_json::to_value(state).unwrap(), json!(state.as_str()));
            assert_eq!(state.to_string(), state.as_str());
        }
        assert!(serde_json::from_value::<State>(json!("chasing")).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_reports_a_missing_service() {
        let path = std::env::temp_dir().join(format!(
            "kamoder-follow-proto-none-{}.sock",
            std::process::id()
        ));
        let error = Client::new(&path).status().await.unwrap_err();
        assert!(matches!(error, ClientError::Connect { .. }), "{error}");
    }
}
