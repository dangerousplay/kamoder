//! Protocol of the update service's control socket, and an async client for it.
//!
//! The `update` service (crate `ota`, bin `update`) checks the GitHub release
//! channel, downloads and verifies signed `.kup` packages, installs them into
//! the inactive A/B slot, and commits a freshly booted slot once it proved
//! healthy (OTA-11, OTA-14..19; design §6). It serves [`DEFAULT_SOCKET`]
//! (mode 0600) so `web` can show what it is doing and drive it. This crate
//! holds the wire types and the client and nothing of the updater, so `web`
//! never links the TLS client or the installer (the split of `follow-proto`,
//! `lights-proto`).
//!
//! The wire format mirrors the other control sockets: JSON lines, one
//! [`Request`] object plus `\n` from the client, one [`Response`] object plus
//! `\n` back. A connection may carry several requests in sequence. Request
//! lines longer than [`MAX_LINE`] bytes are rejected.
//!
//! ```text
//! -> {"op":"status"}
//! <- {"result":"status","version":"0.2.0","slot":"a","trial":false,"committed":true,
//!     "slots":[{"slot":"a","version":"0.2.0","bootable":true,"successful":true,
//!               "tries_left":0,"dir_present":true},
//!              {"slot":"b","version":null,"bootable":false,"successful":false,
//!               "tries_left":0,"dir_present":false}],
//!     "phase":"idle","progress":null,
//!     "available":{"version":"0.3.0","channel":"stable","seq":1760700000,
//!                  "notes_url":"https://…","size":5242880,"downloaded":true,
//!                  "downgrade":false},
//!     "last_check":1792000000,"last_error":null,
//!     "settings":{"channel":"stable","auto_check":true,"auto_install":false,
//!                 "window":null,"repo":"dangerousplay/kamoder"},
//!     "trusted_key":true,"health":null}
//! -> {"op":"check"}
//! <- {"result":"status", …}                       the status after the check
//! -> {"op":"install","source":{"kind":"downloaded"},"force":false,"reboot":true}
//! <- {"result":"ok"}                              verified, install running
//! -> {"op":"install","source":{"kind":"upload","path":"update/upload.kup"}}
//! <- {"result":"error","code":"replay","message":"package seq 5 is not newer than 7 …"}
//! -> {"op":"set_settings","channel":"beta","auto_check":true,"auto_install":true,
//!     "window":{"start_hour":3,"end_hour":5},"repo":"dangerousplay/kamoder"}
//! <- {"result":"ok"}
//! ```
//!
//! # Which requests wait
//!
//! - `status`, `set_settings`, `rollback`, `commit`, `reboot` answer at once
//!   (`reboot` and `rollback`/`install` with `reboot: true` restart the
//!   camera a couple of seconds after answering).
//! - `check` waits for the check (a few HTTPS requests) and answers with the
//!   new [`Status`]; when an update is found, its download starts in the
//!   background.
//! - `download` and `install` validate, then answer `ok` and carry on in the
//!   background; follow [`Status::phase`] and [`Status::progress`]. `install`
//!   verifies the package's signature and the device policy (board, replay,
//!   security floor) *before* answering, so a bad upload is refused with its
//!   reason right away.
//!
//! Only one of check / download / install runs at a time; another one is
//! refused with [`code::BUSY`]. Failures of background work show up in
//! [`Status::last_error`] and [`Phase::Error`].
//!
//! # Uploads
//!
//! `web` streams an uploaded package to [`UPLOAD_PATH`] (relative to the data
//! root, which is the working directory of every service) and then sends
//! `install` with [`Source::Upload`]. The updater only accepts paths inside
//! the data root's `update/` directory, and deletes the upload once it was
//! installed or refused.
//!
//! # Terms
//!
//! - **slot**: one of the two code directories (`a`, `b`). On a card that was
//!   never updated, slot `a` is the legacy tree in the data root itself.
//! - **trial**: the running slot was just installed and has not committed
//!   yet; the boot shim reboots into the other slot unless it does within
//!   the trial window (10 min). [`Status::health`] says what it waits for.
//! - **seq**: the release's monotonic sequence number; a package not newer
//!   than the highest installed one is refused (`replay`) unless forced
//!   (same seq only).

use serde::{Deserialize, Serialize};
use snafu::{ensure, ResultExt, Snafu};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Where the update service listens unless told otherwise (`/tmp` is a tmpfs).
pub const DEFAULT_SOCKET: &str = "/tmp/kamoder-update.sock";

/// Where `web` writes an uploaded package, relative to the data root (the
/// working directory of every service); send it as [`Source::Upload`].
pub const UPLOAD_PATH: &str = "update/upload.kup";

/// Default GitHub repository (`owner/name`) releases are taken from.
pub const DEFAULT_REPO: &str = "dangerousplay/kamoder";

/// Longest accepted request line, in bytes, excluding the newline.
pub const MAX_LINE: usize = 4 * 1024;

/// Longest response line the client accepts.
pub const MAX_RESPONSE: usize = 64 * 1024;

/// Default round-trip timeout of [`Client`]. Covers `check`, which waits for
/// a few HTTPS requests over the camera's Wi-Fi; everything else answers in
/// milliseconds.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Release channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// The newest regular release (GitHub "latest").
    #[default]
    Stable,
    /// The newest release including pre-releases.
    Beta,
}

impl Channel {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        }
    }
}

/// One of the two code slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    A,
    B,
}

impl Slot {
    /// Wire name: `"a"` / `"b"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Slot::A => "a",
            Slot::B => "b",
        }
    }
}

/// Hours (local time, `0..=23`) during which an automatic install may start.
/// `start_hour < end_hour` is the span `[start, end)`; `start_hour >
/// end_hour` wraps midnight (`22`→`4`); equal means the whole day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceWindow {
    pub start_hour: u8,
    pub end_hour: u8,
}

impl MaintenanceWindow {
    /// Whether `hour` (local, `0..=23`) is inside the window.
    pub fn contains(&self, hour: u8) -> bool {
        let (start, end) = (self.start_hour, self.end_hour);
        match start.cmp(&end) {
            std::cmp::Ordering::Equal => true,
            std::cmp::Ordering::Less => (start..end).contains(&hour),
            std::cmp::Ordering::Greater => hour >= start || hour < end,
        }
    }

    /// Both hours are `0..=23`.
    pub fn is_valid(&self) -> bool {
        self.start_hour < 24 && self.end_hour < 24
    }
}

/// Persisted updater settings (OTA-18). [`Request::SetSettings`] replaces
/// all of them; a field left out takes its default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Channel checked for updates. Default `stable`.
    pub channel: Channel,
    /// Check on start and every 12 h. Default on.
    pub auto_check: bool,
    /// Install a downloaded update by itself (inside `window`, if set) and
    /// reboot. Default off.
    pub auto_install: bool,
    /// When `auto_install` may act; `null` = any time.
    pub window: Option<MaintenanceWindow>,
    /// GitHub repository `owner/name`. Default [`DEFAULT_REPO`].
    pub repo: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            channel: Channel::Stable,
            auto_check: true,
            auto_install: false,
            window: None,
            repo: DEFAULT_REPO.to_owned(),
        }
    }
}

/// Where the package to install comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    /// The package the updater downloaded ([`Available::downloaded`]).
    Downloaded,
    /// A package written by `web`, normally to [`UPLOAD_PATH`]. Relative
    /// paths are relative to the data root; it must lie inside `update/`.
    Upload { path: String },
}

/// One request, as sent by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// The current [`Status`].
    Status,
    /// Check the channel now; answers with the [`Status`] afterwards.
    /// Needs an NTP-valid clock (TLS) and a built-in update key.
    Check,
    /// Download the available update now (normally automatic after a check).
    Download,
    /// Verify and install a package into the inactive slot, activate it as a
    /// trial, and (with `reboot`, the default) reboot into it.
    Install {
        source: Source,
        /// Admit a package with the same seq as the highest installed one
        /// (reinstall). Never admits older ones or ones below the floor.
        #[serde(default)]
        force: bool,
        /// Reboot when the install is done. Default true.
        #[serde(default = "default_true")]
        reboot: bool,
    },
    /// Boot the other slot from now on (it must be bootable and committed),
    /// and with `reboot` (default) reboot into it.
    Rollback {
        #[serde(default = "default_true")]
        reboot: bool,
    },
    /// Reboot now (e.g. after "install, reboot later").
    Reboot,
    /// Replace the settings; persisted.
    SetSettings(Settings),
    /// Debug: commit the running slot now, without waiting for the health
    /// check.
    Commit,
}

fn default_true() -> bool {
    true
}

impl Request {
    /// Short name of the operation, for logs and metrics labels.
    pub fn op(&self) -> &'static str {
        match self {
            Request::Status => "status",
            Request::Check => "check",
            Request::Download => "download",
            Request::Install { .. } => "install",
            Request::Rollback { .. } => "rollback",
            Request::Reboot => "reboot",
            Request::SetSettings(_) => "set_settings",
            Request::Commit => "commit",
        }
    }
}

/// What the updater is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Nothing running.
    Idle,
    /// Asking the release channel.
    Checking,
    /// Downloading the package ([`Status::progress`]).
    Downloading,
    /// Checking a downloaded package's signature and every component digest.
    Verifying,
    /// Writing the inactive slot ([`Status::progress`]).
    Installing,
    /// A slot was installed (or rolled back to) and boots next; waiting for
    /// a reboot.
    AwaitingReboot,
    /// The running slot is on trial; see [`Status::health`].
    Trial,
    /// The last background job failed; see [`Status::last_error`]. Cleared by
    /// the next job.
    Error,
}

/// Progress of a download or install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub bytes_done: u64,
    /// `null` while unknown (a download whose server sent no length).
    pub bytes_total: Option<u64>,
    /// `"download"`, `"verify"`, or `"install:<component>"` (`install:slot`,
    /// `install:boot`).
    pub what: String,
}

/// An update newer than the running one, from the last check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Available {
    pub version: String,
    pub channel: Channel,
    pub seq: u64,
    /// Release notes link, when the manifest has one.
    pub notes_url: Option<String>,
    /// Package size in bytes, when known (estimated from the manifest before
    /// the download, exact after).
    pub size: Option<u64>,
    /// Downloaded and verified; `install` with [`Source::Downloaded`] works.
    pub downloaded: bool,
    /// The version is lower than the running one (a deliberate rollback
    /// release with a newer seq).
    pub downgrade: bool,
}

/// The last failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorInfo {
    /// See [`code`].
    pub code: String,
    pub message: String,
    /// Unix seconds; `null` when the clock was not set.
    pub at: Option<u64>,
}

/// Trial health check progress (OTA-11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthInfo {
    /// Seconds every check has passed without interruption (commit at the
    /// configured threshold, 180 s by default).
    pub ok_for_s: u64,
    /// What is not healthy yet: `"service:<name> (<state>)"`, `"supervisor"`,
    /// `"web"`, `"manifest: <reason>"`. Empty while counting up.
    pub waiting_on: Vec<String>,
    /// Seconds left before the boot shim's guard reboots into the other slot
    /// (estimated from uptime); `0` once past.
    pub deadline_s: u64,
}

/// One slot as the boot record describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotInfo {
    pub slot: Slot,
    /// Version recorded at install (or the slot's `VERSION` file); `null` if
    /// unknown.
    pub version: Option<String>,
    /// May be booted.
    pub bootable: bool,
    /// Committed after a healthy boot.
    pub successful: bool,
    /// Trial boots left before the boot shim gives up on it.
    pub tries_left: u8,
    /// Its code is on the card.
    pub dir_present: bool,
}

/// Everything `status` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// Version of the running slot.
    pub version: String,
    /// The running slot.
    pub slot: Slot,
    /// The running slot is on trial (not committed yet).
    pub trial: bool,
    /// The running slot is committed.
    pub committed: bool,
    /// Slot `a`, then slot `b` (OTA-19).
    pub slots: [SlotInfo; 2],
    pub phase: Phase,
    /// While downloading, verifying or installing.
    pub progress: Option<Progress>,
    /// Newer update found by the last check.
    pub available: Option<Available>,
    /// Unix seconds of the last successful check.
    pub last_check: Option<u64>,
    pub last_error: Option<ErrorInfo>,
    pub settings: Settings,
    /// This build has an update key. Without one every package is refused
    /// (`no_trusted_key`) and the health check skips manifest verification.
    pub trusted_key: bool,
    /// While on trial.
    pub health: Option<HealthInfo>,
}

/// One response, as sent by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// Answer to `status` and `check`; the fields sit next to `result`.
    Status(Box<Status>),
    /// The request was carried out (or, for background work, started).
    Ok,
    /// The request was not carried out.
    Error { code: String, message: String },
}

/// Machine-readable error codes ([`Response::Error`], [`ErrorInfo`]).
///
/// Besides these, a refused package carries the `ota-manifest` rejection
/// code: `replay`, `board`, `target`, `below_floor`, `updater_too_old`,
/// `bad_signature`, `unknown_key`, `signature_format`, `schema`,
/// `malformed`, `comment_mismatch`, `invalid`, `too_large`, `no_component`.
///
/// Suggested HTTP mapping for `web`: `bad_request`, `too_long`,
/// `invalid_settings`, `bad_upload_path` → 400; `busy`, `trial`,
/// `no_update`, `not_downloaded`, `no_rollback_target`, `clock_not_set`,
/// `no_trusted_key` → 409; rejection codes and `bad_package` → 422;
/// `network`, `not_found`, `rate_limited` → 502; `no_space` → 507; anything
/// else → 500 (and a connect failure → 503 `update_unavailable`).
pub mod code {
    /// The line was not a valid request.
    pub const BAD_REQUEST: &str = "bad_request";
    /// The line exceeded [`crate::MAX_LINE`].
    pub const TOO_LONG: &str = "too_long";
    /// Another check, download or install is running.
    pub const BUSY: &str = "busy";
    /// The system clock is not NTP-valid yet; TLS cannot work.
    pub const CLOCK_NOT_SET: &str = "clock_not_set";
    /// This build has no update key; nothing can be verified.
    pub const NO_TRUSTED_KEY: &str = "no_trusted_key";
    /// Network or HTTP failure talking to GitHub.
    pub const NETWORK: &str = "network";
    /// The release, manifest or package asset does not exist.
    pub const NOT_FOUND: &str = "not_found";
    /// GitHub API rate limit exhausted (beta channel).
    pub const RATE_LIMITED: &str = "rate_limited";
    /// `download` without an available update.
    pub const NO_UPDATE: &str = "no_update";
    /// `install` of the downloaded package, but none is downloaded.
    pub const NOT_DOWNLOADED: &str = "not_downloaded";
    /// The upload path is missing or outside `update/`.
    pub const BAD_UPLOAD_PATH: &str = "bad_upload_path";
    /// Install refused: the running slot is still on trial (installing would
    /// overwrite the fallback slot).
    pub const TRIAL: &str = "trial";
    /// Not enough free space on the card.
    pub const NO_SPACE: &str = "no_space";
    /// The package is damaged (not a package, truncated, digest mismatch,
    /// or not the release the manifest announced).
    pub const BAD_PACKAGE: &str = "bad_package";
    /// Writing the slot failed (I/O).
    pub const INSTALL_FAILED: &str = "install_failed";
    /// The boot record could not be read or written.
    pub const BOOT_CONTROL: &str = "boot_control";
    /// `rollback`: the other slot is not bootable and committed.
    pub const NO_ROLLBACK_TARGET: &str = "no_rollback_target";
    /// The settings or state file could not be written.
    pub const STATE: &str = "state";
    /// `set_settings` with an invalid repo or window.
    pub const INVALID_SETTINGS: &str = "invalid_settings";
    /// Something else failed; see the message.
    pub const INTERNAL: &str = "internal";
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

    #[snafu(display("I/O error on the update socket: {source}"))]
    Io { source: std::io::Error },

    #[snafu(display("the update service closed the connection without answering"))]
    Closed,

    #[snafu(display("the update service's response exceeds {MAX_RESPONSE} bytes"))]
    ResponseTooLong,

    #[snafu(display("cannot decode the update service's response: {source}"))]
    Decode { source: serde_json::Error },

    #[snafu(display("no response from the update service within {} s", timeout.as_secs()))]
    Timeout { timeout: Duration },
}

/// Client for the update socket. One connection per request: requests are
/// rare and nothing goes stale when the service restarts.
///
/// Every method returns the [`Response`]; a [`Response::Error`] is a
/// successful round trip and comes back as `Ok`.
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

    /// [`Request::Check`].
    pub async fn check(&self) -> Result<Response, ClientError> {
        self.request(&Request::Check).await
    }

    /// [`Request::Download`].
    pub async fn download(&self) -> Result<Response, ClientError> {
        self.request(&Request::Download).await
    }

    /// [`Request::Install`].
    pub async fn install(
        &self,
        source: Source,
        force: bool,
        reboot: bool,
    ) -> Result<Response, ClientError> {
        self.request(&Request::Install {
            source,
            force,
            reboot,
        })
        .await
    }

    /// [`Request::Rollback`].
    pub async fn rollback(&self, reboot: bool) -> Result<Response, ClientError> {
        self.request(&Request::Rollback { reboot }).await
    }

    /// [`Request::Reboot`].
    pub async fn reboot(&self) -> Result<Response, ClientError> {
        self.request(&Request::Reboot).await
    }

    /// [`Request::SetSettings`].
    pub async fn set_settings(&self, settings: Settings) -> Result<Response, ClientError> {
        self.request(&Request::SetSettings(settings)).await
    }

    /// [`Request::Commit`].
    pub async fn commit(&self) -> Result<Response, ClientError> {
        self.request(&Request::Commit).await
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

    fn sample_status() -> Status {
        Status {
            version: "0.2.0".into(),
            slot: Slot::B,
            trial: true,
            committed: false,
            slots: [
                SlotInfo {
                    slot: Slot::A,
                    version: Some("0.1.0".into()),
                    bootable: true,
                    successful: true,
                    tries_left: 0,
                    dir_present: true,
                },
                SlotInfo {
                    slot: Slot::B,
                    version: None,
                    bootable: true,
                    successful: false,
                    tries_left: 2,
                    dir_present: true,
                },
            ],
            phase: Phase::Trial,
            progress: Some(Progress {
                bytes_done: 10,
                bytes_total: None,
                what: "download".into(),
            }),
            available: Some(Available {
                version: "0.3.0".into(),
                channel: Channel::Beta,
                seq: 7,
                notes_url: None,
                size: Some(100),
                downloaded: false,
                downgrade: false,
            }),
            last_check: Some(1_792_000_000),
            last_error: Some(ErrorInfo {
                code: "network".into(),
                message: "m".into(),
                at: None,
            }),
            settings: Settings::default(),
            trusted_key: false,
            health: Some(HealthInfo {
                ok_for_s: 30,
                waiting_on: vec!["web".into()],
                deadline_s: 500,
            }),
        }
    }

    #[test]
    fn requests_use_the_documented_wire_format() {
        let cases = [
            (Request::Status, json!({"op": "status"})),
            (Request::Check, json!({"op": "check"})),
            (Request::Download, json!({"op": "download"})),
            (
                Request::Install {
                    source: Source::Downloaded,
                    force: false,
                    reboot: true,
                },
                json!({"op": "install", "source": {"kind": "downloaded"},
                       "force": false, "reboot": true}),
            ),
            (
                Request::Install {
                    source: Source::Upload {
                        path: UPLOAD_PATH.into(),
                    },
                    force: true,
                    reboot: false,
                },
                json!({"op": "install",
                       "source": {"kind": "upload", "path": "update/upload.kup"},
                       "force": true, "reboot": false}),
            ),
            (
                Request::Rollback { reboot: false },
                json!({"op": "rollback", "reboot": false}),
            ),
            (Request::Reboot, json!({"op": "reboot"})),
            (
                Request::SetSettings(Settings {
                    channel: Channel::Beta,
                    auto_check: false,
                    auto_install: true,
                    window: Some(MaintenanceWindow {
                        start_hour: 22,
                        end_hour: 4,
                    }),
                    repo: "me/fork".into(),
                }),
                json!({"op": "set_settings", "channel": "beta", "auto_check": false,
                       "auto_install": true, "window": {"start_hour": 22, "end_hour": 4},
                       "repo": "me/fork"}),
            ),
            (Request::Commit, json!({"op": "commit"})),
        ];
        for (request, wire) in cases {
            assert_eq!(serde_json::to_value(&request).unwrap(), wire);
            let back: Request = serde_json::from_value(wire).unwrap();
            assert_eq!(back, request);
            assert_eq!(request.op(), back.op());
        }
    }

    #[test]
    fn optional_request_fields_take_their_defaults() {
        let install: Request =
            serde_json::from_str(r#"{"op":"install","source":{"kind":"downloaded"}}"#).unwrap();
        assert_eq!(
            install,
            Request::Install {
                source: Source::Downloaded,
                force: false,
                reboot: true
            }
        );
        let rollback: Request = serde_json::from_str(r#"{"op":"rollback"}"#).unwrap();
        assert_eq!(rollback, Request::Rollback { reboot: true });
        let settings: Request =
            serde_json::from_str(r#"{"op":"set_settings","channel":"beta"}"#).unwrap();
        assert_eq!(
            settings,
            Request::SetSettings(Settings {
                channel: Channel::Beta,
                ..Settings::default()
            })
        );
    }

    #[test]
    fn malformed_requests_are_rejected() {
        for bad in [
            r#"{"op":"explode"}"#,
            r#"{"op":"install"}"#,
            r#"{"op":"install","source":{"kind":"usb"}}"#,
            r#"{"op":"install","source":{"kind":"upload"}}"#,
            r#"{"op":"install","source":{"kind":"downloaded"},"force":"yes"}"#,
            r#"{"op":"install","source":{"kind":"downloaded"},"now":true}"#,
            r#"{"op":"rollback","reboot":1}"#,
            r#"{"op":"set_settings","channel":"nightly"}"#,
            r#"{"op":"set_settings","window":{"start_hour":3}}"#,
            "not json",
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn responses_use_the_documented_wire_format() {
        let status = sample_status();
        let wire = json!({
            "result": "status", "version": "0.2.0", "slot": "b", "trial": true,
            "committed": false,
            "slots": [
                {"slot": "a", "version": "0.1.0", "bootable": true, "successful": true,
                 "tries_left": 0, "dir_present": true},
                {"slot": "b", "version": null, "bootable": true, "successful": false,
                 "tries_left": 2, "dir_present": true}
            ],
            "phase": "trial",
            "progress": {"bytes_done": 10, "bytes_total": null, "what": "download"},
            "available": {"version": "0.3.0", "channel": "beta", "seq": 7,
                          "notes_url": null, "size": 100, "downloaded": false,
                          "downgrade": false},
            "last_check": 1_792_000_000u64,
            "last_error": {"code": "network", "message": "m", "at": null},
            "settings": {"channel": "stable", "auto_check": true, "auto_install": false,
                         "window": null, "repo": "dangerousplay/kamoder"},
            "trusted_key": false,
            "health": {"ok_for_s": 30, "waiting_on": ["web"], "deadline_s": 500}
        });
        let cases = [
            (Response::Status(Box::new(status)), wire),
            (Response::Ok, json!({"result": "ok"})),
            (
                Response::Error {
                    code: "replay".into(),
                    message: "m".into(),
                },
                json!({"result": "error", "code": "replay", "message": "m"}),
            ),
        ];
        for (response, wire) in cases {
            let text = serde_json::to_string(&response).unwrap();
            assert!(!text.contains('\n'));
            assert_eq!(serde_json::to_value(&response).unwrap(), wire);
            let back: Response = serde_json::from_value(wire).unwrap();
            assert_eq!(back, response);
        }
    }

    #[test]
    fn phases_and_enums_use_snake_case() {
        let phases = [
            (Phase::Idle, "idle"),
            (Phase::Checking, "checking"),
            (Phase::Downloading, "downloading"),
            (Phase::Verifying, "verifying"),
            (Phase::Installing, "installing"),
            (Phase::AwaitingReboot, "awaiting_reboot"),
            (Phase::Trial, "trial"),
            (Phase::Error, "error"),
        ];
        for (phase, name) in phases {
            assert_eq!(serde_json::to_value(phase).unwrap(), json!(name));
        }
        for c in [Channel::Stable, Channel::Beta] {
            assert_eq!(serde_json::to_value(c).unwrap(), json!(c.as_str()));
        }
        for s in [Slot::A, Slot::B] {
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.as_str()));
        }
    }

    #[test]
    fn settings_default_and_tolerate_unknown_fields() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
        assert!(s.auto_check && !s.auto_install && s.window.is_none());
        assert_eq!(s.repo, DEFAULT_REPO);
        // A newer state file stays readable.
        let s: Settings = serde_json::from_str(r#"{"auto_install":true,"future":1}"#).unwrap();
        assert!(s.auto_install);
    }

    #[test]
    fn maintenance_window_contains() {
        let w = |start_hour, end_hour| MaintenanceWindow {
            start_hour,
            end_hour,
        };
        // (window, hour, inside)
        let cases = [
            (w(3, 5), 2, false),
            (w(3, 5), 3, true),
            (w(3, 5), 4, true),
            (w(3, 5), 5, false),
            (w(22, 4), 21, false),
            (w(22, 4), 22, true),
            (w(22, 4), 0, true),
            (w(22, 4), 3, true),
            (w(22, 4), 4, false),
            (w(7, 7), 0, true),
            (w(7, 7), 23, true),
        ];
        for (window, hour, inside) in cases {
            assert_eq!(window.contains(hour), inside, "{window:?} at {hour}");
        }
        assert!(w(0, 23).is_valid());
        assert!(!w(0, 24).is_valid());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_reports_a_missing_service() {
        let path = std::env::temp_dir().join(format!(
            "kamoder-update-proto-none-{}.sock",
            std::process::id()
        ));
        let error = Client::new(&path).status().await.unwrap_err();
        assert!(matches!(error, ClientError::Connect { .. }), "{error}");
    }
}
