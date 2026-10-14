//! Protocol of the lights ownership socket, and an async client for it.
//!
//! Only one process may drive the camera's light and speaker-amp GPIOs: the
//! `lights` service owns them and serves [`DEFAULT_SOCKET`] (mode 0600);
//! `web` and `onvif` are clients. This crate holds the wire types and the
//! client and nothing that touches a GPIO, so a client never links the owner
//! (the same split as `ptz-proto`).
//!
//! The wire format mirrors the PTZ and supervisor sockets: JSON lines, one
//! [`Request`] object plus `\n` from the client, one [`Response`] object plus
//! `\n` back. A connection may carry several requests in sequence. Lines
//! longer than [`MAX_LINE`] bytes are rejected. Every successful request,
//! including the setters, answers with the full [`State`] after the change.
//!
//! ```text
//! -> {"op":"get"}
//! <- {"result":"state","state":{"mode":"day","white":false,"alarm":{"state":"off"},"muted":true}}
//! -> {"op":"set_mode","mode":"night"}
//! <- {"result":"state","state":{"mode":"night","white":false,"alarm":{"state":"off"},"muted":true}}
//! -> {"op":"set_alarm","alarm":{"state":"blink","period_ms":500}}
//! <- {"result":"state","state":{"mode":"night","white":false,
//!     "alarm":{"state":"blink","period_ms":500},"muted":true}}
//! -> {"op":"set_alarm","alarm":{"state":"blink","period_ms":5}}
//! <- {"result":"error","code":"invalid_argument","message":"period_ms must be within 200..=10000"}
//! ```
//!
//! # Terms
//!
//! - **mode**: `day` = IR-cut filter in, IR LEDs off; `night` = filter out,
//!   IR LEDs on. The owner switches the two in a fixed order so the image
//!   never sees IR light through the filter.
//! - **white**: the white light.
//! - **alarm**: the red and blue lights, steady or blinking.
//! - **muted**: the speaker amplifier is off. It is muted by default and
//!   whenever the owner starts, whatever was set before.

use serde::{Deserialize, Serialize};
use snafu::{ensure, ResultExt, Snafu};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Where the lights owner listens unless told otherwise (`/tmp` is a tmpfs).
pub const DEFAULT_SOCKET: &str = "/tmp/kamoder-lights.sock";

/// Longest accepted request line, in bytes, excluding the newline.
pub const MAX_LINE: usize = 1024;

/// Longest response line the client accepts.
pub const MAX_RESPONSE: usize = 4 * 1024;

/// Default round-trip timeout of [`Client`]. Every operation is a handful of
/// sysfs writes, so this only has to cover a busy owner.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Shortest blink period (one on/off cycle) the owner accepts.
pub const MIN_BLINK_PERIOD_MS: u32 = 200;

/// Longest blink period the owner accepts.
pub const MAX_BLINK_PERIOD_MS: u32 = 10_000;

/// Blink period for clients that do not pick one.
pub const DEFAULT_BLINK_PERIOD_MS: u32 = 1_000;

/// Day/night mode of the IR-cut filter and IR LEDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// IR-cut filter in, IR LEDs off.
    #[default]
    Day,
    /// IR-cut filter out, IR LEDs on.
    Night,
}

impl Mode {
    /// Wire name, for logs and metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Day => "day",
            Mode::Night => "night",
        }
    }
}

/// What the red and blue lights do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Alarm {
    #[default]
    Off,
    /// Lit steadily.
    On,
    /// On for half of `period_ms`, off for the other half.
    Blink { period_ms: u32 },
}

impl Alarm {
    /// Wire name of the variant, for logs and metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Alarm::Off => "off",
            Alarm::On => "on",
            Alarm::Blink { .. } => "blink",
        }
    }
}

/// Everything the owner controls, as last applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub mode: Mode,
    /// White light on.
    pub white: bool,
    pub alarm: Alarm,
    /// Speaker amplifier off.
    pub muted: bool,
}

impl Default for State {
    /// The defaults decided for the camera: day, every light off, amp muted.
    fn default() -> Self {
        Self {
            mode: Mode::Day,
            white: false,
            alarm: Alarm::Off,
            muted: true,
        }
    }
}

/// One request, as sent by a client.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// The current state.
    Get,
    /// Switch day/night.
    SetMode { mode: Mode },
    /// White light on or off.
    SetWhite { on: bool },
    /// Red and blue lights off, on or blinking.
    SetAlarm { alarm: Alarm },
    /// Mute (`true`) or unmute the speaker amplifier.
    SetMute { muted: bool },
}

impl Request {
    /// Short name of the operation, for logs and metrics labels.
    pub fn op(&self) -> &'static str {
        match self {
            Request::Get => "get",
            Request::SetMode { .. } => "set_mode",
            Request::SetWhite { .. } => "set_white",
            Request::SetAlarm { .. } => "set_alarm",
            Request::SetMute { .. } => "set_mute",
        }
    }
}

/// One response, as sent by the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// The request was carried out; the full state after it.
    State { state: State },
    /// The request was not carried out.
    Error { code: ErrorCode, message: String },
}

/// Machine-readable reason a request failed. Suggested HTTP mapping for
/// `web`: `bad_request`/`too_long`/`invalid_argument` → 400, `gpio` → 500
/// (and a connect failure → 503).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The line was not a valid request (unknown op, missing field, not JSON).
    BadRequest,
    /// The line exceeded [`MAX_LINE`].
    TooLong,
    /// A value the owner rejects (blink period out of range).
    InvalidArgument,
    /// A GPIO write failed; the state may be partly applied.
    Gpio,
}

impl ErrorCode {
    /// Wire name, for metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::TooLong => "too_long",
            ErrorCode::InvalidArgument => "invalid_argument",
            ErrorCode::Gpio => "gpio",
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

    #[snafu(display("I/O error on the lights socket: {source}"))]
    Io { source: std::io::Error },

    #[snafu(display("the lights owner closed the connection without answering"))]
    Closed,

    #[snafu(display("the lights owner's response exceeds {MAX_RESPONSE} bytes"))]
    ResponseTooLong,

    #[snafu(display("cannot decode the lights owner's response: {source}"))]
    Decode { source: serde_json::Error },

    #[snafu(display("no response from the lights owner within {} ms", timeout.as_millis()))]
    Timeout { timeout: Duration },
}

/// Client for the lights socket. One connection per request: requests are
/// rare and nothing goes stale when the owner restarts.
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

    /// [`Request::Get`].
    pub async fn get(&self) -> Result<Response, ClientError> {
        self.request(&Request::Get).await
    }

    /// [`Request::SetMode`].
    pub async fn set_mode(&self, mode: Mode) -> Result<Response, ClientError> {
        self.request(&Request::SetMode { mode }).await
    }

    /// [`Request::SetWhite`].
    pub async fn set_white(&self, on: bool) -> Result<Response, ClientError> {
        self.request(&Request::SetWhite { on }).await
    }

    /// [`Request::SetAlarm`].
    pub async fn set_alarm(&self, alarm: Alarm) -> Result<Response, ClientError> {
        self.request(&Request::SetAlarm { alarm }).await
    }

    /// [`Request::SetMute`].
    pub async fn set_mute(&self, muted: bool) -> Result<Response, ClientError> {
        self.request(&Request::SetMute { muted }).await
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
            (Request::Get, json!({"op": "get"})),
            (
                Request::SetMode { mode: Mode::Night },
                json!({"op": "set_mode", "mode": "night"}),
            ),
            (
                Request::SetWhite { on: true },
                json!({"op": "set_white", "on": true}),
            ),
            (
                Request::SetAlarm { alarm: Alarm::Off },
                json!({"op": "set_alarm", "alarm": {"state": "off"}}),
            ),
            (
                Request::SetAlarm {
                    alarm: Alarm::Blink { period_ms: 500 },
                },
                json!({"op": "set_alarm", "alarm": {"state": "blink", "period_ms": 500}}),
            ),
            (
                Request::SetMute { muted: false },
                json!({"op": "set_mute", "muted": false}),
            ),
        ];
        for (request, wire) in cases {
            assert_eq!(serde_json::to_value(request).unwrap(), wire);
            let back: Request = serde_json::from_value(wire).unwrap();
            assert_eq!(back, request);
        }
    }

    #[test]
    fn malformed_requests_are_rejected() {
        for bad in [
            r#"{"op":"reboot"}"#,
            r#"{"op":"set_mode","mode":"auto"}"#,
            r#"{"op":"set_mode"}"#,
            r#"{"op":"set_white","on":"yes"}"#,
            r#"{"op":"set_white","on":true,"gpio":98}"#,
            r#"{"op":"set_alarm","alarm":{"state":"blink"}}"#,
            // Unknown fields are refused on variants that have fields (serde
            // cannot enforce it on the field-less ones such as `on`).
            r#"{"op":"set_alarm","alarm":{"state":"blink","period_ms":1,"duty":2}}"#,
            r#"{"op":"set_mute","muted":1}"#,
            "not json",
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn responses_use_the_documented_wire_format() {
        let state = State {
            mode: Mode::Night,
            white: true,
            alarm: Alarm::Blink { period_ms: 1000 },
            muted: true,
        };
        let cases = [
            (
                Response::State { state },
                json!({"result": "state", "state": {
                    "mode": "night", "white": true,
                    "alarm": {"state": "blink", "period_ms": 1000}, "muted": true
                }}),
            ),
            (
                Response::Error {
                    code: ErrorCode::Gpio,
                    message: "m".into(),
                },
                json!({"result": "error", "code": "gpio", "message": "m"}),
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
            ErrorCode::Gpio,
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), json!(code.as_str()));
        }
    }

    #[test]
    fn defaults_are_day_dark_and_muted() {
        let state = State::default();
        assert_eq!(state.mode, Mode::Day);
        assert!(!state.white);
        assert_eq!(state.alarm, Alarm::Off);
        assert!(state.muted);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_reports_a_missing_owner() {
        let path = std::env::temp_dir().join(format!(
            "kamoder-lights-proto-none-{}.sock",
            std::process::id()
        ));
        let error = Client::new(&path).get().await.unwrap_err();
        assert!(matches!(error, ClientError::Connect { .. }), "{error}");
    }
}
