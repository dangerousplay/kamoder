//! Service status and control, through the supervisor's control socket (T12).
//!
//! The supervisor owns the processes; this module only translates. Its
//! [`control`] types are shared with the supervisor crate, and each request
//! opens a fresh connection (see [`control::Client`]), so a supervisor restart
//! leaves nothing stale here.
//!
//! The JSON is the shape `webui/src/lib/api.ts` (`ServiceStatus`) reads, which
//! differs from the socket's in spelling: `uptime_s`, `backoff`/`failed`
//! states, a human `last_exit`, and the `log` file name for the logs page.

use crate::auth::{AdminUser, AuthUser};
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::{Path, State as AxState};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use std::time::Duration;
use supervisor::control::{self, ErrorCode, Response, ServiceStatus, State};

/// `status` answers from memory; anything slower means the supervisor is stuck.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/services", get(list))
        .route("/services/{name}/{action}", post(action))
}

/// One service, as `api.ts` `ServiceStatus`.
#[derive(Debug, Clone, Serialize)]
pub struct Service {
    pub name: String,
    /// `running`, `stopped`, `backoff`, `failed` (gave up) or `disabled`.
    pub state: &'static str,
    pub pid: Option<u32>,
    pub uptime_s: Option<u64>,
    pub restarts: u32,
    pub enabled: bool,
    pub critical: bool,
    /// `<name>.log` when that file exists in the log directory.
    pub log: Option<String>,
    /// How the previous process ended, e.g. `exit code 1`, `signal 9`.
    pub last_exit: Option<String>,
    /// The raw exit status (code, or 128 + signal), for scripts.
    pub last_exit_code: Option<i32>,
}

impl Service {
    fn from_status(status: ServiceStatus, logs: &std::path::Path) -> Self {
        let log = format!("{}.log", status.name);
        let has_log = std::fs::symlink_metadata(logs.join(&log)).is_ok_and(|m| m.is_file());
        Self {
            state: match status.state {
                State::Running => "running",
                State::Stopped => "stopped",
                State::BackingOff => "backoff",
                State::GaveUp => "failed",
                State::Disabled => "disabled",
            },
            pid: status.pid,
            uptime_s: status.uptime_secs,
            restarts: status.restarts,
            enabled: status.enabled,
            critical: status.critical,
            log: has_log.then_some(log),
            last_exit: status.last_exit_code.map(describe_exit),
            last_exit_code: status.last_exit_code,
            name: status.name,
        }
    }
}

/// `143` → `signal 15 (SIGTERM)`: the supervisor reports signals the shell way.
fn describe_exit(code: i32) -> String {
    let name = match code - 128 {
        1 => " (SIGHUP)",
        2 => " (SIGINT)",
        6 => " (SIGABRT)",
        9 => " (SIGKILL)",
        11 => " (SIGSEGV)",
        15 => " (SIGTERM)",
        _ => "",
    };
    if (129..=192).contains(&code) {
        format!("signal {}{name}", code - 128)
    } else {
        format!("exit code {code}")
    }
}

/// A client for the configured socket.
pub fn client(state: &AppState) -> control::Client {
    control::Client::new(&state.config.supervisor_socket)
}

/// Sends one request; a supervisor that cannot be asked is 503
/// `supervisor_unavailable`, a refusal is mapped to its HTTP status.
pub async fn ask(
    client: &control::Client,
    request: &control::Request,
) -> Result<Response, ApiError> {
    match client.request(request).await {
        Ok(Response::Error { code, message }) => Err(refusal(code, message)),
        Ok(response) => Ok(response),
        Err(error) => {
            tracing::warn!(%error, op = request.op(), "supervisor unreachable");
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "supervisor_unavailable",
                format!("supervisor unavailable: {error}"),
            ))
        }
    }
}

/// Maps the supervisor's error codes onto HTTP.
fn refusal(code: ErrorCode, message: String) -> ApiError {
    let (status, code) = match code {
        ErrorCode::Critical => (StatusCode::CONFLICT, "critical"),
        ErrorCode::UnknownService => (StatusCode::NOT_FOUND, "unknown_service"),
        ErrorCode::BadRequest | ErrorCode::TooLong => (StatusCode::BAD_REQUEST, "bad_request"),
        ErrorCode::ShuttingDown => (StatusCode::SERVICE_UNAVAILABLE, "shutting_down"),
        ErrorCode::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "supervisor_error"),
    };
    ApiError::new(status, code, message)
}

fn unexpected(response: &Response) -> ApiError {
    ApiError::internal(format!("unexpected supervisor response: {response:?}"))
}

/// `GET /api/services` → `[Service]`, in `supervisor.toml` order.
async fn list(
    AxState(state): AxState<AppState>,
    _: AuthUser,
) -> Result<Json<Vec<Service>>, ApiError> {
    let client = client(&state).with_timeout(STATUS_TIMEOUT);
    match ask(&client, &control::Request::Status).await? {
        Response::Services { services } => Ok(Json(
            services
                .into_iter()
                .map(|s| Service::from_status(s, &state.config.logs))
                .collect(),
        )),
        other => Err(unexpected(&other)),
    }
}

/// `POST /api/services/{name}/{start|stop|restart}` (admin) → the service's
/// state afterwards. 409 `critical` for stop/restart of `wdt_feeder` & co.
async fn action(
    AxState(state): AxState<AppState>,
    AdminUser(user): AdminUser,
    Path((name, action)): Path<(String, String)>,
) -> Result<Json<Service>, ApiError> {
    let service = name.clone();
    let request = match action.as_str() {
        "start" => control::Request::Start { service },
        "stop" => control::Request::Stop { service },
        "restart" => control::Request::Restart { service },
        _ => return Err(ApiError::not_found("action must be start, stop or restart")),
    };
    tracing::info!(user = %user.user, service = %name, action, "service action");
    // Default client timeout: a stop may take the service's stop timeout.
    match ask(&client(&state), &request).await? {
        Response::Service { service } => {
            Ok(Json(Service::from_status(service, &state.config.logs)))
        }
        other => Err(unexpected(&other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_descriptions() {
        assert_eq!(describe_exit(0), "exit code 0");
        assert_eq!(describe_exit(1), "exit code 1");
        assert_eq!(describe_exit(143), "signal 15 (SIGTERM)");
        assert_eq!(describe_exit(137), "signal 9 (SIGKILL)");
        assert_eq!(describe_exit(200), "exit code 200");
    }
}
