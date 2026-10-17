//! Firmware updates, through the update service's socket (OTA-16..19,
//! design §7).
//!
//! The `update` service checks the release channel, downloads, verifies and
//! installs signed `.kup` packages and keeps the A/B slots; it serves
//! `/tmp/kamoder-update.sock` ([`update_proto`]). This module translates
//! between `webui` and that socket, so `web` never links the TLS client or
//! the installer. A missing socket is 503 `update_unavailable` on every
//! route, so the UI shows the updater as unavailable.
//!
//! ```text
//! GET  /api/update             (any user) → the service's Status, as is
//! POST /api/update/check       (admin)    → Status after the check (~30 s)
//! POST /api/update/download    (admin)    → {"ok":true}
//! POST /api/update/install     (admin) {"force":false,"reboot":true} → {"ok":true}
//! POST /api/update/upload?force=false&reboot=true
//!                              (admin) raw .kup body → {"ok":true}
//! POST /api/update/rollback    (admin) {"reboot":true} → {"ok":true}
//! POST /api/update/reboot      (admin)    → {"ok":true}
//! PUT  /api/update/settings    (admin) Settings → Status
//! ```
//!
//! Refusals keep the service's code (`busy`, `replay`, `bad_signature`, …)
//! and message; the HTTP status says what kind of refusal it is: 409 for
//! "not now" (busy, trial, nothing downloaded / to roll back to, clock not
//! set), 400 for a bad request or a package the device policy refuses, 500
//! for anything else.
//!
//! # Uploads (OTA-17)
//!
//! An offline camera gets its package from the browser. The body is
//! streamed, never buffered: written in [`WRITE_CHUNK`] pieces on the
//! blocking pool to `update/upload.kup.part` in the data root (the working
//! directory), fsynced, renamed to [`update_proto::UPLOAD_PATH`], and then
//! handed to the updater with `install {source: upload}`, which verifies it
//! exactly like a downloaded one. The route takes up to
//! [`UPLOAD_LIMIT`] bytes, the rest of the API keeps its 64 KiB limit. One
//! upload at a time (409 `busy`), and none while the updater is busy, so a
//! second upload never replaces the file an install is still reading. A
//! failed or aborted upload leaves no file behind.

use crate::auth::{AdminUser, AuthUser};
use crate::error::{ApiError, ApiJson};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use update_proto::{code, Client, ClientError, Phase, Request, Response, Settings, Source, Status};

/// Largest accepted upload. A package carries the binaries and the web UI
/// of one slot (a few MiB today); the cap bounds what a client can make the
/// SD card hold.
pub const UPLOAD_LIMIT: u64 = 64 * 1024 * 1024;

/// Bytes collected before each write: few blocking-pool round trips
/// without holding much RAM.
pub const WRITE_CHUNK: usize = 64 * 1024;

/// `check` waits for a few HTTPS requests over the camera's Wi-Fi.
const CHECK_TIMEOUT: Duration = Duration::from_secs(120);

/// Where the upload is written before it is complete, next to
/// [`update_proto::UPLOAD_PATH`].
const UPLOAD_PART: &str = "update/upload.kup.part";

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/update", get(status))
        .route("/update/check", post(check))
        .route("/update/download", post(download))
        .route("/update/install", post(install))
        .route(
            "/update/upload",
            // Overrides the app-wide 64 KiB limit for this route only. The
            // handler reads the raw body (which no limit applies to) and
            // counts against the cap itself; the layer keeps any future
            // body extractor here consistent with it.
            post(upload).layer(DefaultBodyLimit::max(UPLOAD_LIMIT as usize)),
        )
        .route("/update/rollback", post(rollback))
        .route("/update/reboot", post(reboot))
        .route("/update/settings", put(settings))
}

/// `POST /api/update/install` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallBody {
    #[serde(default)]
    pub force: bool,
    #[serde(default = "yes")]
    pub reboot: bool,
}

/// `POST /api/update/upload` query.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadQuery {
    #[serde(default)]
    pub force: bool,
    #[serde(default = "yes")]
    pub reboot: bool,
}

/// `POST /api/update/rollback` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackBody {
    #[serde(default = "yes")]
    pub reboot: bool,
}

fn yes() -> bool {
    true
}

fn client(state: &AppState) -> Client {
    Client::new(&state.config.update_socket)
}

/// HTTP status of a refusal with the service's `code`.
fn refusal_status(code: &str) -> StatusCode {
    match code {
        code::BUSY
        | code::TRIAL
        | code::NO_UPDATE
        | code::NOT_DOWNLOADED
        | code::NO_ROLLBACK_TARGET
        | code::CLOCK_NOT_SET => StatusCode::CONFLICT,
        code::BAD_REQUEST
        | code::TOO_LONG
        | code::INVALID_SETTINGS
        | code::BAD_UPLOAD_PATH
        | code::BAD_PACKAGE
        | code::NO_TRUSTED_KEY
        // `ota-manifest` rejections: the package or the device policy.
        | "replay"
        | "board"
        | "target"
        | "below_floor"
        | "updater_too_old"
        | "bad_signature"
        | "unknown_key"
        | "signature_format"
        | "schema"
        | "malformed"
        | "comment_mismatch"
        | "invalid"
        | "too_large"
        | "no_component" => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The service could not be asked at all.
fn unavailable(op: &str, error: &ClientError) -> ApiError {
    tracing::debug!(%error, op, "update service unreachable");
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "update_unavailable",
        format!("update service unavailable: {error}"),
    )
}

/// Turns a round trip into the answer, or the API error for a refusal or a
/// missing service.
fn answer(op: &str, result: Result<Response, ClientError>) -> Result<Response, ApiError> {
    match result {
        Ok(Response::Error { code, message }) => {
            Err(ApiError::new(refusal_status(&code), code, message))
        }
        Ok(response) => Ok(response),
        Err(error) => Err(unavailable(op, &error)),
    }
}

/// Sends `request` with `client` and returns the answer.
async fn ask_with(client: Client, request: Request) -> Result<Response, ApiError> {
    let op = request.op();
    answer(op, client.request(&request).await)
}

/// Error for an answer that does not fit the request.
fn unexpected(op: &str, response: &Response) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "update_protocol",
        format!("{op}: unexpected answer from the update service: {response:?}"),
    )
}

/// Expects a [`Response::Status`].
fn expect_status(op: &str, response: Response) -> Result<Json<Status>, ApiError> {
    match response {
        Response::Status(status) => Ok(Json(*status)),
        other => Err(unexpected(op, &other)),
    }
}

/// Expects [`Response::Ok`]; answers `{"ok":true}`.
fn expect_ok(op: &str, response: Response) -> Result<Json<Value>, ApiError> {
    match response {
        Response::Ok => Ok(Json(json!({ "ok": true }))),
        other => Err(unexpected(op, &other)),
    }
}

async fn fetch_status(state: &AppState) -> Result<Json<Status>, ApiError> {
    expect_status("status", ask_with(client(state), Request::Status).await?)
}

/// `GET /api/update` → the service's status.
async fn status(State(state): State<AppState>, _: AuthUser) -> Result<Json<Status>, ApiError> {
    fetch_status(&state).await
}

/// `POST /api/update/check` (admin) → the status after the check.
async fn check(State(state): State<AppState>, _: AdminUser) -> Result<Json<Status>, ApiError> {
    let client = client(&state).with_timeout(CHECK_TIMEOUT);
    expect_status("check", ask_with(client, Request::Check).await?)
}

/// `POST /api/update/download` (admin).
async fn download(State(state): State<AppState>, _: AdminUser) -> Result<Json<Value>, ApiError> {
    expect_ok(
        "download",
        ask_with(client(&state), Request::Download).await?,
    )
}

/// `POST /api/update/install` (admin): the downloaded package.
async fn install(
    State(state): State<AppState>,
    _: AdminUser,
    ApiJson(body): ApiJson<InstallBody>,
) -> Result<Json<Value>, ApiError> {
    let request = Request::Install {
        source: Source::Downloaded,
        force: body.force,
        reboot: body.reboot,
    };
    expect_ok("install", ask_with(client(&state), request).await?)
}

/// `POST /api/update/rollback` (admin).
async fn rollback(
    State(state): State<AppState>,
    _: AdminUser,
    ApiJson(body): ApiJson<RollbackBody>,
) -> Result<Json<Value>, ApiError> {
    let request = Request::Rollback {
        reboot: body.reboot,
    };
    expect_ok("rollback", ask_with(client(&state), request).await?)
}

/// `POST /api/update/reboot` (admin).
async fn reboot(State(state): State<AppState>, _: AdminUser) -> Result<Json<Value>, ApiError> {
    expect_ok("reboot", ask_with(client(&state), Request::Reboot).await?)
}

/// `PUT /api/update/settings` (admin) → the status with the new settings.
async fn settings(
    State(state): State<AppState>,
    _: AdminUser,
    ApiJson(body): ApiJson<Settings>,
) -> Result<Json<Status>, ApiError> {
    match ask_with(client(&state), Request::SetSettings(body)).await? {
        Response::Ok => fetch_status(&state).await,
        other => Err(unexpected("set_settings", &other)),
    }
}

/// 409 `busy`.
fn busy(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code::BUSY, message)
}

/// 413 `too_large`.
fn too_large(limit: u64) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
        format!("the package exceeds {} MiB", limit / (1024 * 1024)),
    )
}

/// Removes a file when dropped unless disarmed: an upload that fails, or
/// whose client goes away mid-body (the handler future is dropped), leaves
/// nothing on the card.
struct RemoveOnDrop(Option<PathBuf>);

impl RemoveOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            if let Err(error) = std::fs::remove_file(&path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(%error, path = %path.display(), "cannot remove the upload");
                }
            }
        }
    }
}

/// Runs file I/O on the blocking pool; a write error is 507/500 per
/// [`ApiError::write_failed`].
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(ApiError::write_failed("the upload", &error)),
        Err(error) => Err(ApiError::internal(error)),
    }
}

/// Opens `path` for writing (mode 0600), creating its directory.
fn create(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// Streams `body` into `path` (at most `limit` bytes), fsyncs it and
/// returns its length.
async fn receive(body: Body, path: PathBuf, limit: u64) -> Result<u64, ApiError> {
    let mut file = blocking(move || create(&path)).await?;
    let mut stream = body.into_data_stream();
    let mut buf = Vec::with_capacity(WRITE_CHUNK);
    let mut total = 0u64;
    loop {
        let chunk = stream.next().await;
        if let Some(chunk) = &chunk {
            let chunk = chunk
                .as_ref()
                .map_err(|error| ApiError::bad_request(format!("upload interrupted: {error}")))?;
            total += chunk.len() as u64;
            if total > limit {
                return Err(too_large(limit));
            }
            buf.extend_from_slice(chunk);
        }
        let end = chunk.is_none();
        if buf.len() >= WRITE_CHUNK || (end && !buf.is_empty()) {
            (file, buf) = blocking(move || {
                file.write_all(&buf)?;
                buf.clear();
                Ok((file, buf))
            })
            .await?;
        }
        if end {
            break;
        }
    }
    if total == 0 {
        return Err(ApiError::bad_request("the upload is empty"));
    }
    blocking(move || file.sync_all()).await?;
    Ok(total)
}

/// `POST /api/update/upload?force=&reboot=` (admin): a `.kup` as the raw
/// body, installed like a downloaded package.
async fn upload(
    State(state): State<AppState>,
    _: AdminUser,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    let limit = state.tuning.update_upload_limit;
    let Ok(_permit) = state.update_upload.try_acquire() else {
        return Err(busy("another upload is in progress"));
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|length| length > limit) {
        return Err(too_large(limit));
    }
    // Ask first: a missing service is 503 before the body is read, and a
    // running job keeps its package file.
    let current = fetch_status(&state).await?;
    if matches!(
        current.phase,
        Phase::Checking | Phase::Downloading | Phase::Verifying | Phase::Installing
    ) {
        return Err(busy("the updater is busy"));
    }

    let root = &state.config.data_root;
    let part = root.join(UPLOAD_PART);
    let dest = root.join(update_proto::UPLOAD_PATH);
    let mut part_guard = RemoveOnDrop(Some(part.clone()));
    let size = receive(body, part.clone(), limit).await?;
    let to = dest.clone();
    blocking(move || std::fs::rename(&part, &to)).await?;
    part_guard.disarm();
    let mut dest_guard = RemoveOnDrop(Some(dest));
    tracing::info!(size, "package uploaded; installing");

    let source = Source::Upload {
        path: update_proto::UPLOAD_PATH.to_owned(),
    };
    let result = client(&state)
        .request(&Request::Install {
            source,
            force: query.force,
            reboot: query.reboot,
        })
        .await;
    // Refused or never asked: the file is ours to remove. Anything else
    // (a timeout while verifying, say) may mean the updater is still
    // reading it, and it removes the upload itself.
    if !matches!(
        result,
        Ok(Response::Error { .. }) | Err(ClientError::Connect { .. })
    ) {
        dest_guard.disarm();
    }
    expect_ok("install", answer("install", result)?)
}
