//! Control protocol of the supervisor's Unix socket, and a client for it.
//!
//! The supervisor used to be steerable only by signals: SIGTERM stopped
//! everything, and nothing could stop, start or inspect a single service. The
//! web admin needs exactly that, so the supervisor listens on a Unix socket
//! (default [`DEFAULT_SOCKET`], mode 0600). A socket rather than HTTP because
//! file permissions make it root-only for free and no port is exposed on the
//! network.
//!
//! The wire format is JSON lines: the client writes one [`Request`] object
//! followed by `\n`, the server answers with one [`Response`] object followed
//! by `\n`. A connection may carry several requests in sequence. Lines longer
//! than [`MAX_LINE`] bytes are rejected, so a confused or hostile client cannot
//! make the supervisor buffer unbounded input on a 32 MB board.
//!
//! ```text
//! -> {"op":"status"}
//! <- {"result":"services","services":[{"name":"sshd","enabled":true,...}]}
//! -> {"op":"restart","service":"sshd"}
//! <- {"result":"service","service":{"name":"sshd","state":"running",...}}
//! -> {"op":"stop","service":"wdt_feeder"}
//! <- {"result":"error","code":"critical","message":"..."}
//! ```
//!
//! This module is the library half of the crate so that `web` shares the
//! exact types the server uses instead of re-declaring them.

use serde::{Deserialize, Serialize};
use snafu::{ensure, ResultExt, Snafu};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Where the supervisor listens unless told otherwise. `/tmp` is a tmpfs on
/// the camera, so a stale socket never survives a reboot.
pub const DEFAULT_SOCKET: &str = "/tmp/anyka-supervisor.sock";

/// Longest accepted line, in bytes, excluding the newline. Requests are a few
/// dozen bytes; this only exists to bound memory.
pub const MAX_LINE: usize = 4 * 1024;

/// Longest response line the client accepts. A status listing for a dozen
/// services is a couple of KiB; this bounds the client the same way
/// [`MAX_LINE`] bounds the server.
pub const MAX_RESPONSE: usize = 64 * 1024;

/// How long [`Client`] waits for a whole round trip by default. A `stop` may
/// legitimately take the service's `stop_timeout_secs` (10 s by default) plus
/// the SIGKILL grace, so this is generous.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// One request, as sent by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Every configured service, enabled or not, in declaration order.
    Status,
    /// Start a service. Clears a previous `stop` and a given-up crash loop.
    /// Starting an `enabled = false` service is allowed and lasts until the
    /// supervisor restarts; it is not written back to the config.
    Start { service: String },
    /// Stop a service and keep it stopped until the next `start`. Refused for
    /// `critical` services.
    Stop { service: String },
    /// Stop, then start. Refused for `critical` services, because the stop
    /// half would leave, say, the watchdog unfed.
    Restart { service: String },
}

impl Request {
    /// Short name of the operation, for logs and metrics labels.
    pub fn op(&self) -> &'static str {
        match self {
            Request::Status => "status",
            Request::Start { .. } => "start",
            Request::Stop { .. } => "stop",
            Request::Restart { .. } => "restart",
        }
    }
}

/// One response, as sent by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// Answer to [`Request::Status`].
    Services { services: Vec<ServiceStatus> },
    /// Answer to start/stop/restart: the service's state after the operation.
    Service { service: ServiceStatus },
    /// The request was not carried out.
    Error { code: ErrorCode, message: String },
}

/// Machine-readable reason a request failed. `web` maps these onto HTTP
/// status codes (`critical` → 409, `unknown_service` → 404, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Stop/restart of a service flagged `critical = true`.
    Critical,
    /// No `[[service]]` with that name.
    UnknownService,
    /// The line was not a valid request.
    BadRequest,
    /// The line exceeded [`MAX_LINE`].
    TooLong,
    /// The supervisor is shutting down and accepts no more changes.
    ShuttingDown,
    /// Something inside the supervisor failed; see the message.
    Internal,
}

/// Lifecycle state of one service, as the supervisor sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// A process is alive.
    Running,
    /// No process and none planned: stopped on request, or exited under a
    /// restart policy that says not to restart.
    Stopped,
    /// The process exited and a restart is scheduled after the backoff delay.
    BackingOff,
    /// Restarted too often inside the window; the supervisor stopped trying
    /// until someone sends `start`.
    GaveUp,
    /// `enabled = false` in the config and not started at runtime.
    Disabled,
}

/// Status of one supervised service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub name: String,
    /// `enabled` from the config — whether it starts with the supervisor.
    pub enabled: bool,
    /// `critical` from the config — stop and restart are refused.
    pub critical: bool,
    pub state: State,
    /// Pid of the running process, if any.
    pub pid: Option<u32>,
    /// Seconds since the running process was spawned, if any.
    pub uptime_secs: Option<u64>,
    /// Process spawns beyond the first, whatever caused them (crash restart,
    /// `restart`, `stop` + `start`).
    pub restarts: u32,
    /// Exit status of the previous process: its exit code, or 128 + signal
    /// number if it was killed (the shell convention). `None` until a process
    /// has exited.
    pub last_exit_code: Option<i32>,
}

/// Errors from [`Client`].
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

    #[snafu(display("I/O error on the supervisor socket: {source}"))]
    Io { source: std::io::Error },

    #[snafu(display("the supervisor closed the connection without answering"))]
    Closed,

    #[snafu(display("the supervisor's response exceeds {MAX_RESPONSE} bytes"))]
    ResponseTooLong,

    #[snafu(display("cannot decode the supervisor's response: {source}"))]
    Decode { source: serde_json::Error },

    #[snafu(display("no response from the supervisor within {} s", timeout.as_secs()))]
    Timeout { timeout: Duration },
}

/// Client for the control socket.
///
/// Opens one connection per request: requests are rare (a page refresh, a
/// button press) and a fresh connection means no state to go stale when the
/// supervisor restarts.
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

    /// Sends one request and waits for its response.
    ///
    /// A [`Response::Error`] is a successful round trip and is returned as
    /// `Ok`; `Err` means the supervisor could not be asked at all.
    pub async fn request(&self, req: &Request) -> Result<Response, ClientError> {
        let timeout = self.timeout;
        tokio::time::timeout(timeout, self::request(&self.path, req))
            .await
            .unwrap_or(Err(ClientError::Timeout { timeout }))
    }

    /// Shorthand for [`Request::Status`].
    pub async fn status(&self) -> Result<Response, ClientError> {
        self.request(&Request::Status).await
    }

    /// Shorthand for [`Request::Start`].
    pub async fn start(&self, service: &str) -> Result<Response, ClientError> {
        self.request(&Request::Start {
            service: service.to_owned(),
        })
        .await
    }

    /// Shorthand for [`Request::Stop`].
    pub async fn stop(&self, service: &str) -> Result<Response, ClientError> {
        self.request(&Request::Stop {
            service: service.to_owned(),
        })
        .await
    }

    /// Shorthand for [`Request::Restart`].
    pub async fn restart(&self, service: &str) -> Result<Response, ClientError> {
        self.request(&Request::Restart {
            service: service.to_owned(),
        })
        .await
    }
}

/// Sends one request over a fresh connection to `path` and reads the response.
///
/// No timeout of its own; wrap it, or use [`Client`], which applies one.
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
        // Either the cap was hit, or EOF arrived before a whole line.
        ensure!(buf.len() <= MAX_RESPONSE, ResponseTooLongSnafu);
        return ClosedSnafu.fail();
    }
    serde_json::from_slice(&buf).context(DecodeSnafu)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status() -> ServiceStatus {
        ServiceStatus {
            name: "sshd".into(),
            enabled: true,
            critical: false,
            state: State::Running,
            pid: Some(42),
            uptime_secs: Some(7),
            restarts: 1,
            last_exit_code: Some(143),
        }
    }

    #[test]
    fn requests_use_the_documented_wire_format() {
        let cases = [
            (Request::Status, json!({"op": "status"})),
            (
                Request::Start {
                    service: "web".into(),
                },
                json!({"op": "start", "service": "web"}),
            ),
            (
                Request::Stop {
                    service: "web".into(),
                },
                json!({"op": "stop", "service": "web"}),
            ),
            (
                Request::Restart {
                    service: "web".into(),
                },
                json!({"op": "restart", "service": "web"}),
            ),
        ];
        for (request, wire) in cases {
            assert_eq!(serde_json::to_value(&request).unwrap(), wire);
            let back: Request = serde_json::from_value(wire).unwrap();
            assert_eq!(back, request);
        }
    }

    #[test]
    fn malformed_requests_are_rejected() {
        for bad in [
            r#"{"op":"reboot"}"#,
            r#"{"op":"stop"}"#,
            r#"{"service":"x"}"#,
            r#"{"op":"stop","service":"x","force":true}"#,
            "not json",
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn responses_round_trip() {
        let cases = [
            Response::Services {
                services: vec![
                    status(),
                    ServiceStatus {
                        name: "onvif".into(),
                        enabled: false,
                        critical: false,
                        state: State::Disabled,
                        pid: None,
                        uptime_secs: None,
                        restarts: 0,
                        last_exit_code: None,
                    },
                ],
            },
            Response::Service { service: status() },
            Response::Error {
                code: ErrorCode::Critical,
                message: "no".into(),
            },
        ];
        for response in cases {
            let text = serde_json::to_string(&response).unwrap();
            assert!(!text.contains('\n'));
            let back: Response = serde_json::from_str(&text).unwrap();
            assert_eq!(back, response);
        }
    }

    #[test]
    fn response_wire_format() {
        assert_eq!(
            serde_json::to_value(Response::Service { service: status() }).unwrap(),
            json!({
                "result": "service",
                "service": {
                    "name": "sshd", "enabled": true, "critical": false,
                    "state": "running", "pid": 42, "uptime_secs": 7,
                    "restarts": 1, "last_exit_code": 143
                }
            })
        );
        assert_eq!(
            serde_json::to_value(Response::Error {
                code: ErrorCode::UnknownService,
                message: "m".into()
            })
            .unwrap(),
            json!({"result": "error", "code": "unknown_service", "message": "m"})
        );
        for (state, wire) in [
            (State::Running, "running"),
            (State::Stopped, "stopped"),
            (State::BackingOff, "backing_off"),
            (State::GaveUp, "gave_up"),
            (State::Disabled, "disabled"),
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), json!(wire));
        }
    }
}
