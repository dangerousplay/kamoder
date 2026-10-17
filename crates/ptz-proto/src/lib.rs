//! Protocol of the PTZ ownership socket, and an async client for it.
//!
//! # Why a crate of its own
//!
//! `web` needs this protocol and nothing else of `anyka-ptz`. It used to
//! get it as `anyka-ptz`'s `control` feature with default features off, but
//! cargo unifies features across a workspace build: `onvif` enables
//! `anyka-ptz/sdk`, so `anyka-ptz` was compiled once, *with* the SDK backend,
//! and its build script's `rustc-link-lib` lines (`plat_drv`, `plat_common`,
//! `plat_thread`) reached the web binary as well. The web UI then could not
//! start without the vendor libraries — exactly when it is needed to fix a
//! broken camera. This crate depends on neither `anyka-ptz` nor anything that
//! links the SDK, and has no build script, so no feature combination can put
//! an SDK library into a binary that only speaks the protocol.
//!
//! The consequence is that [`Limits`], [`Position`] and [`Preset`] here are
//! *wire* types, mirroring `anyka-ptz`'s (same fields, same JSON). The owner
//! (`onvif`, `ptz_socket.rs`) converts its backend's values and errors
//! into them; `anyka-ptz` itself knows nothing about the socket.
//!
//! Only one process may drive the motors (design decision D4). `onvif`
//! owns the backend and serves [`DEFAULT_SOCKET`] (mode 0600); `web`
//! and tools are clients. Moves arriving over the socket and over ONVIF reach
//! the same backend, so the latest command wins whichever side sent it.
//!
//! The wire format mirrors the supervisor's control socket: JSON lines, one
//! [`Request`] object plus `\n` from the client, one [`Response`] object plus
//! `\n` back. A connection may carry several requests in sequence. Lines
//! longer than [`MAX_LINE`] bytes are rejected.
//!
//! ```text
//! -> {"op":"status"}
//! <- {"result":"status","backend":"sim","limits":{"pan_min_deg":0.0,"pan_max_deg":350.0,
//!     "tilt_min_deg":0.0,"tilt_max_deg":120.0},"position":{"pan_deg":175.0,"tilt_deg":60.0,"moving":false}}
//! -> {"op":"move","pan":-0.5,"tilt":0.0,"timeout_ms":1000}
//! <- {"result":"ok"}
//! -> {"op":"goto","pan":90.0,"tilt":30.0}
//! <- {"result":"ok"}
//! -> {"op":"set_preset","name":"Door"}
//! <- {"result":"preset","preset":{"token":"1","name":"Door","pan_deg":90.0,"tilt_deg":30.0}}
//! -> {"op":"presets"}
//! <- {"result":"presets","presets":[{"token":"1","name":"Door","pan_deg":90.0,"tilt_deg":30.0}]}
//! -> {"op":"goto_preset","token":"9"}
//! <- {"result":"error","code":"unknown_preset","message":"no preset with token \"9\""}
//! ```
//!
//! # Deadman on `move`
//!
//! A continuous move from a browser must not outlive the browser: a closed
//! tab or a dropped Wi-Fi link would otherwise leave the head grinding
//! against its end stop. Every `move` therefore carries a deadline,
//! `timeout_ms` (default [`DEFAULT_MOVE_TIMEOUT_MS`], at most
//! [`MAX_MOVE_TIMEOUT_MS`]); when it passes without a newer motion command,
//! the server stops the motor. A client that wants to keep moving re-sends
//! the same `move` well within the deadline (the web UI repeats it every
//! ~300 ms while a direction is held) and sends `stop` on release. Any newer
//! motion command (`move`, `stop`, `goto`, `goto_preset`, `home`, or an ONVIF
//! move) replaces the deadline.
//!
//! Angles (`goto`, presets, `status`) are degrees in the backend's frame;
//! velocities are normalised to `[-1, 1]`.
//!
//! # Who moved the head: `source`
//!
//! The follow tracker (`follow`) drives the head over this same socket, and
//! must back off while a person is steering. Every motion request (`move`,
//! `stop`, `goto`, `goto_preset`, `home`) therefore takes an optional
//! `source`: `"manual"` or `"follow"`. Absent means `"manual"`, so clients
//! that predate the field (web, tools) count as manual without change; ONVIF
//! moves are always manual. The owner remembers when the last manual motion
//! command arrived, and `status` reports it as `last_manual_ms_ago`
//! (milliseconds; absent if there has been none since the owner started):
//!
//! ```text
//! -> {"op":"move","pan":0.2,"tilt":0.0,"timeout_ms":500,"source":"follow"}
//! <- {"result":"ok"}
//! -> {"op":"status"}
//! <- {"result":"status","backend":"sim","limits":{...},"position":{...},"last_manual_ms_ago":4210}
//! ```
//!
//! A [`Client`] built [`with_source`](Client::with_source) tags all of its
//! motion requests.

use serde::{Deserialize, Serialize};
use snafu::{ensure, ResultExt, Snafu};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Where the PTZ owner listens unless told otherwise (`/tmp` is a tmpfs).
pub const DEFAULT_SOCKET: &str = "/tmp/anyka-ptz.sock";

/// Longest accepted request line, in bytes, excluding the newline.
pub const MAX_LINE: usize = 4 * 1024;

/// Longest response line the client accepts: 32 presets with 64-character
/// names are well under 8 KiB.
pub const MAX_RESPONSE: usize = 32 * 1024;

/// Default round-trip timeout of [`Client`]. Every operation returns without
/// waiting for the motor, so this only has to cover a busy owner.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Deadman used when a `move` has no `timeout_ms`.
pub const DEFAULT_MOVE_TIMEOUT_MS: u32 = 1_000;

/// Longest deadman a `move` may ask for; larger values are clamped.
pub const MAX_MOVE_TIMEOUT_MS: u32 = 60_000;

/// Where the head is (mirrors `anyka_ptz::Position`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Pan angle in degrees.
    pub pan_deg: f32,
    /// Tilt angle in degrees.
    pub tilt_deg: f32,
    /// A move is in progress.
    pub moving: bool,
}

/// Software travel limits in degrees, inclusive (mirrors `anyka_ptz::Limits`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub pan_min_deg: f32,
    pub pan_max_deg: f32,
    pub tilt_min_deg: f32,
    pub tilt_max_deg: f32,
}

/// One saved position (mirrors `anyka_ptz::Preset`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    /// Stable identifier (ONVIF `PresetToken`).
    pub token: String,
    /// Human-readable label (ONVIF `PresetName`).
    pub name: String,
    /// Pan angle in degrees.
    pub pan_deg: f32,
    /// Tilt angle in degrees.
    pub tilt_deg: f32,
}

/// Who issued a motion request (see the module docs). A request without
/// one counts as [`Source::Manual`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A person: web UI, ONVIF client, tools.
    Manual,
    /// The follow tracker; does not count as manual activity.
    Follow,
}

/// One request, as sent by a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Backend name, limits and current position.
    Status,
    /// Continuous move at normalised speeds `pan`, `tilt` ∈ `[-1, 1]`
    /// (clamped), stopped automatically after `timeout_ms` unless a newer
    /// motion command arrives first. Both zero is a stop.
    Move {
        pan: f32,
        tilt: f32,
        /// Deadman in milliseconds; `None` = [`DEFAULT_MOVE_TIMEOUT_MS`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u32>,
        /// Who sent it; `None` = [`Source::Manual`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<Source>,
    },
    /// Stop whatever is running.
    Stop {
        /// Who sent it; `None` = [`Source::Manual`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<Source>,
    },
    /// Absolute move, degrees, clamped to the limits.
    Goto {
        pan: f32,
        tilt: f32,
        /// Who sent it; `None` = [`Source::Manual`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<Source>,
    },
    /// Every preset.
    Presets,
    /// Save the current position as a preset: a new one, or overwrite
    /// `token`. An empty name means "Preset <token>" (new) / "keep" (update).
    SetPreset {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// Move to a saved preset.
    GotoPreset {
        token: String,
        /// Who sent it; `None` = [`Source::Manual`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<Source>,
    },
    /// Delete a preset.
    RemovePreset { token: String },
    /// Move to the home position (the backend's own homing if it has one,
    /// otherwise the centre of the limits).
    Home {
        /// Who sent it; `None` = [`Source::Manual`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<Source>,
    },
}

impl Request {
    /// Short name of the operation, for logs and metrics labels.
    pub fn op(&self) -> &'static str {
        match self {
            Request::Status => "status",
            Request::Move { .. } => "move",
            Request::Stop { .. } => "stop",
            Request::Goto { .. } => "goto",
            Request::Presets => "presets",
            Request::SetPreset { .. } => "set_preset",
            Request::GotoPreset { .. } => "goto_preset",
            Request::RemovePreset { .. } => "remove_preset",
            Request::Home { .. } => "home",
        }
    }

    /// Who sent a motion request (absent = [`Source::Manual`]); `None` for
    /// requests that do not move the head.
    pub fn motion_source(&self) -> Option<Source> {
        match self {
            Request::Move { source, .. }
            | Request::Stop { source }
            | Request::Goto { source, .. }
            | Request::GotoPreset { source, .. }
            | Request::Home { source } => Some(source.unwrap_or(Source::Manual)),
            Request::Status
            | Request::Presets
            | Request::SetPreset { .. }
            | Request::RemovePreset { .. } => None,
        }
    }
}

/// One response, as sent by the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// Answer to [`Request::Status`].
    Status {
        /// Backend identifier (`"sim"`, `"sdk"`, `"preset_motor"`).
        backend: String,
        limits: Limits,
        position: Position,
        /// Milliseconds since the last manual motion command (any motion
        /// request whose source is not [`Source::Follow`], and every ONVIF
        /// move); `None` if there has been none since the owner started.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_manual_ms_ago: Option<u64>,
    },
    /// Answer to [`Request::Presets`].
    Presets { presets: Vec<Preset> },
    /// Answer to [`Request::SetPreset`]: the stored preset.
    Preset { preset: Preset },
    /// A motion or delete command was accepted (motion runs on after this).
    Ok,
    /// The request was not carried out.
    Error { code: ErrorCode, message: String },
}

/// Machine-readable reason a request failed. Suggested HTTP mapping for
/// `web`: `bad_request`/`too_long`/`invalid_argument` → 400,
/// `unknown_preset` → 404, `too_many_presets` → 409, `unsupported` → 501,
/// `backend` → 500 (and a connect failure → 503).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The line was not a valid request (unknown op, missing field, not JSON).
    BadRequest,
    /// The line exceeded [`MAX_LINE`].
    TooLong,
    /// A value the backend rejects (NaN, over-long preset name, …).
    InvalidArgument,
    /// No preset has that token.
    UnknownPreset,
    /// The preset table is full.
    TooManyPresets,
    /// The backend cannot do this.
    Unsupported,
    /// The backend failed (driver error, preset file not writable, …).
    Backend,
}

impl ErrorCode {
    /// Wire name, for metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::TooLong => "too_long",
            ErrorCode::InvalidArgument => "invalid_argument",
            ErrorCode::UnknownPreset => "unknown_preset",
            ErrorCode::TooManyPresets => "too_many_presets",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Backend => "backend",
        }
    }
}

/// Errors from [`Client`]: the owner could not be asked at all.
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

    #[snafu(display("I/O error on the PTZ socket: {source}"))]
    Io { source: std::io::Error },

    #[snafu(display("the PTZ owner closed the connection without answering"))]
    Closed,

    #[snafu(display("the PTZ owner's response exceeds {MAX_RESPONSE} bytes"))]
    ResponseTooLong,

    #[snafu(display("cannot decode the PTZ owner's response: {source}"))]
    Decode { source: serde_json::Error },

    #[snafu(display("no response from the PTZ owner within {} ms", timeout.as_millis()))]
    Timeout { timeout: Duration },
}

/// Client for the PTZ socket. One connection per request, like the
/// supervisor's client: requests are rare and nothing goes stale when the
/// owner restarts.
#[derive(Debug, Clone)]
pub struct Client {
    path: PathBuf,
    timeout: Duration,
    source: Option<Source>,
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
            source: None,
        }
    }

    /// Replaces the round-trip timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Tags every motion request sent through the helper methods
    /// ([`move_`](Self::move_), [`stop`](Self::stop), [`goto`](Self::goto),
    /// [`goto_preset`](Self::goto_preset), [`home`](Self::home)) with
    /// `source`. Without it they carry none, which the owner treats as
    /// manual. [`request`](Self::request) sends its argument unchanged.
    pub fn with_source(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }

    /// The source this client tags motion requests with, if any.
    pub fn source(&self) -> Option<Source> {
        self.source
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

    /// [`Request::Move`].
    pub async fn move_(
        &self,
        pan: f32,
        tilt: f32,
        timeout_ms: Option<u32>,
    ) -> Result<Response, ClientError> {
        self.request(&Request::Move {
            pan,
            tilt,
            timeout_ms,
            source: self.source,
        })
        .await
    }

    /// [`Request::Stop`].
    pub async fn stop(&self) -> Result<Response, ClientError> {
        self.request(&Request::Stop {
            source: self.source,
        })
        .await
    }

    /// [`Request::Goto`].
    pub async fn goto(&self, pan_deg: f32, tilt_deg: f32) -> Result<Response, ClientError> {
        self.request(&Request::Goto {
            pan: pan_deg,
            tilt: tilt_deg,
            source: self.source,
        })
        .await
    }

    /// [`Request::Presets`].
    pub async fn presets(&self) -> Result<Response, ClientError> {
        self.request(&Request::Presets).await
    }

    /// [`Request::SetPreset`].
    pub async fn set_preset(
        &self,
        name: &str,
        token: Option<&str>,
    ) -> Result<Response, ClientError> {
        self.request(&Request::SetPreset {
            name: name.to_owned(),
            token: token.map(str::to_owned),
        })
        .await
    }

    /// [`Request::GotoPreset`].
    pub async fn goto_preset(&self, token: &str) -> Result<Response, ClientError> {
        self.request(&Request::GotoPreset {
            token: token.to_owned(),
            source: self.source,
        })
        .await
    }

    /// [`Request::RemovePreset`].
    pub async fn remove_preset(&self, token: &str) -> Result<Response, ClientError> {
        self.request(&Request::RemovePreset {
            token: token.to_owned(),
        })
        .await
    }

    /// [`Request::Home`].
    pub async fn home(&self) -> Result<Response, ClientError> {
        self.request(&Request::Home {
            source: self.source,
        })
        .await
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
                Request::Move {
                    pan: 0.5,
                    tilt: -1.0,
                    timeout_ms: None,
                    source: None,
                },
                json!({"op": "move", "pan": 0.5, "tilt": -1.0}),
            ),
            (
                Request::Move {
                    pan: 0.0,
                    tilt: 0.25,
                    timeout_ms: Some(1500),
                    source: None,
                },
                json!({"op": "move", "pan": 0.0, "tilt": 0.25, "timeout_ms": 1500}),
            ),
            (Request::Stop { source: None }, json!({"op": "stop"})),
            (
                Request::Goto {
                    pan: 90.0,
                    tilt: 30.0,
                    source: None,
                },
                json!({"op": "goto", "pan": 90.0, "tilt": 30.0}),
            ),
            (Request::Presets, json!({"op": "presets"})),
            (
                Request::SetPreset {
                    name: "Door".into(),
                    token: None,
                },
                json!({"op": "set_preset", "name": "Door"}),
            ),
            (
                Request::SetPreset {
                    name: "".into(),
                    token: Some("2".into()),
                },
                json!({"op": "set_preset", "name": "", "token": "2"}),
            ),
            (
                Request::GotoPreset {
                    token: "1".into(),
                    source: None,
                },
                json!({"op": "goto_preset", "token": "1"}),
            ),
            (
                Request::RemovePreset { token: "1".into() },
                json!({"op": "remove_preset", "token": "1"}),
            ),
            (Request::Home { source: None }, json!({"op": "home"})),
        ];
        for (request, wire) in cases {
            assert_eq!(serde_json::to_value(&request).unwrap(), wire);
            let back: Request = serde_json::from_value(wire).unwrap();
            assert_eq!(back, request);
        }
    }

    #[test]
    fn motion_requests_carry_an_optional_source() {
        let cases = [
            (
                Request::Move {
                    pan: 0.25,
                    tilt: 0.0,
                    timeout_ms: Some(500),
                    source: Some(Source::Follow),
                },
                json!({"op": "move", "pan": 0.25, "tilt": 0.0, "timeout_ms": 500,
                       "source": "follow"}),
            ),
            (
                Request::Stop {
                    source: Some(Source::Manual),
                },
                json!({"op": "stop", "source": "manual"}),
            ),
            (
                Request::Goto {
                    pan: 1.0,
                    tilt: 2.0,
                    source: Some(Source::Follow),
                },
                json!({"op": "goto", "pan": 1.0, "tilt": 2.0, "source": "follow"}),
            ),
            (
                Request::GotoPreset {
                    token: "1".into(),
                    source: Some(Source::Follow),
                },
                json!({"op": "goto_preset", "token": "1", "source": "follow"}),
            ),
            (
                Request::Home {
                    source: Some(Source::Follow),
                },
                json!({"op": "home", "source": "follow"}),
            ),
        ];
        for (request, wire) in cases {
            assert_eq!(serde_json::to_value(&request).unwrap(), wire);
            let back: Request = serde_json::from_value(wire).unwrap();
            assert_eq!(back, request);
        }
        assert!(serde_json::from_str::<Request>(r#"{"op":"stop","source":"robot"}"#).is_err());
    }

    /// Lines from clients that predate `source` still parse, as manual.
    #[test]
    fn requests_without_a_source_are_manual() {
        for old in [
            r#"{"op":"move","pan":0.5,"tilt":0,"timeout_ms":1000}"#,
            r#"{"op":"stop"}"#,
            r#"{"op":"goto","pan":90,"tilt":30}"#,
            r#"{"op":"goto_preset","token":"1"}"#,
            r#"{"op":"home"}"#,
        ] {
            let request: Request = serde_json::from_str(old).unwrap();
            assert_eq!(request.motion_source(), Some(Source::Manual), "{old}");
        }
        for still in [
            Request::Status,
            Request::Presets,
            Request::RemovePreset { token: "1".into() },
        ] {
            assert_eq!(still.motion_source(), None);
        }
        let follow: Request = serde_json::from_str(r#"{"op":"home","source":"follow"}"#).unwrap();
        assert_eq!(follow.motion_source(), Some(Source::Follow));
    }

    #[test]
    fn status_reports_the_last_manual_command() {
        let wire = json!({
            "result": "status", "backend": "sim",
            "limits": {"pan_min_deg": 0.0, "pan_max_deg": 350.0,
                       "tilt_min_deg": 0.0, "tilt_max_deg": 120.0},
            "position": {"pan_deg": 175.0, "tilt_deg": 60.0, "moving": false},
            "last_manual_ms_ago": 4210
        });
        let status: Response = serde_json::from_value(wire.clone()).unwrap();
        let Response::Status {
            last_manual_ms_ago, ..
        } = &status
        else {
            panic!("{status:?}")
        };
        assert_eq!(*last_manual_ms_ago, Some(4210));
        assert_eq!(serde_json::to_value(&status).unwrap(), wire);
    }

    #[test]
    fn a_client_with_a_source_remembers_it() {
        let client = Client::new("/nonexistent");
        assert_eq!(client.source(), None);
        let client = client.with_source(Source::Follow);
        assert_eq!(client.source(), Some(Source::Follow));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_client_with_a_source_tags_every_motion_request() {
        let path = std::env::temp_dir().join(format!("ptz-proto-src-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..6 {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                BufReader::new(read).read_line(&mut line).await.unwrap();
                seen.push(serde_json::from_str::<Request>(&line).unwrap());
                write.write_all(b"{\"result\":\"ok\"}\n").await.unwrap();
            }
            seen
        });
        let client = Client::new(&path).with_source(Source::Follow);
        client.move_(0.1, 0.0, Some(300)).await.unwrap();
        client.stop().await.unwrap();
        client.goto(1.0, 2.0).await.unwrap();
        client.goto_preset("1").await.unwrap();
        client.home().await.unwrap();
        client.presets().await.unwrap();
        let seen = server.await.unwrap();
        let _ = std::fs::remove_file(&path);
        let sources: Vec<_> = seen.iter().map(Request::motion_source).collect();
        let follow = Some(Source::Follow);
        assert_eq!(sources, [follow, follow, follow, follow, follow, None]);
    }

    #[test]
    fn malformed_requests_are_rejected() {
        for bad in [
            r#"{"op":"reboot"}"#,
            r#"{"op":"move","pan":1}"#,
            r#"{"op":"goto_preset"}"#,
            // Unknown fields are refused on ops that have fields (serde
            // cannot enforce it on the field-less ones such as `status`).
            r#"{"op":"goto","pan":1,"tilt":2,"speed":3}"#,
            r#"{"op":"stop","speed":3}"#,
            r#"{"op":"move","pan":"left","tilt":0}"#,
            "not json",
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn responses_use_the_documented_wire_format() {
        let preset = Preset {
            token: "1".into(),
            name: "Door".into(),
            pan_deg: 90.0,
            tilt_deg: 30.0,
        };
        let cases = [
            (
                Response::Status {
                    backend: "sim".into(),
                    limits: Limits {
                        pan_min_deg: 0.0,
                        pan_max_deg: 350.0,
                        tilt_min_deg: 0.0,
                        tilt_max_deg: 120.0,
                    },
                    position: Position {
                        pan_deg: 175.0,
                        tilt_deg: 60.0,
                        moving: true,
                    },
                    last_manual_ms_ago: None,
                },
                json!({
                    "result": "status", "backend": "sim",
                    "limits": {"pan_min_deg": 0.0, "pan_max_deg": 350.0,
                               "tilt_min_deg": 0.0, "tilt_max_deg": 120.0},
                    "position": {"pan_deg": 175.0, "tilt_deg": 60.0, "moving": true}
                }),
            ),
            (
                Response::Presets {
                    presets: vec![preset.clone()],
                },
                json!({"result": "presets", "presets": [
                    {"token": "1", "name": "Door", "pan_deg": 90.0, "tilt_deg": 30.0}
                ]}),
            ),
            (
                Response::Preset {
                    preset: preset.clone(),
                },
                json!({"result": "preset", "preset":
                    {"token": "1", "name": "Door", "pan_deg": 90.0, "tilt_deg": 30.0}}),
            ),
            (Response::Ok, json!({"result": "ok"})),
            (
                Response::Error {
                    code: ErrorCode::UnknownPreset,
                    message: "m".into(),
                },
                json!({"result": "error", "code": "unknown_preset", "message": "m"}),
            ),
        ];
        for (response, wire) in cases {
            let text = serde_json::to_string(&response).unwrap();
            assert!(!text.contains('\n'));
            assert_eq!(serde_json::to_value(&response).unwrap(), wire);
            let back: Response = serde_json::from_value(wire).unwrap();
            assert_eq!(back, response);
        }
        for code in [
            ErrorCode::BadRequest,
            ErrorCode::TooLong,
            ErrorCode::InvalidArgument,
            ErrorCode::UnknownPreset,
            ErrorCode::TooManyPresets,
            ErrorCode::Unsupported,
            ErrorCode::Backend,
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), json!(code.as_str()));
        }
    }
}
