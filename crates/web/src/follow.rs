//! Follow movement, through the follow service's socket (FOL-16, D-F3).
//!
//! The `follow` service tracks motion and steers the PTZ head; it owns the
//! persisted on/off switch and serves `/tmp/kamoder-follow.sock`. This
//! module only translates between `webui/src/lib/api.ts` and
//! [`follow_proto`]. A missing socket is 503 `follow_unavailable` on every
//! route, so the UI shows the service as unavailable.
//!
//! ```text
//! GET /api/follow                      (any user)
//!   → {"enabled":true,"state":"tracking",
//!      "target":{"x_permille":640,"y_permille":480,"cells":6},"moves":12}
//! PUT /api/follow                      (admin) {"enabled":false}
//!   → the status after the change
//! ```

use crate::auth::{AdminUser, AuthUser};
use crate::error::{ApiError, ApiJson};
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use follow_proto::{Client, ErrorCode, FollowStatus, Request, Response};
use serde::Deserialize;

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/follow", get(status).put(update))
}

/// `api.ts` `FollowUpdate`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowUpdate {
    pub enabled: bool,
}

/// Sends one request and returns the service's answer. A connect failure
/// (no service) is 503 `follow_unavailable`; refusals map per the protocol's
/// suggestion.
async fn ask(state: &AppState, request: Request) -> Result<Response, ApiError> {
    let op = request.op();
    match Client::new(&state.config.follow_socket)
        .request(&request)
        .await
    {
        Ok(Response::Error { code, message }) => {
            let (status, code) = match code {
                ErrorCode::BadRequest | ErrorCode::TooLong => {
                    (StatusCode::BAD_REQUEST, "bad_request")
                }
                ErrorCode::Store => (StatusCode::INTERNAL_SERVER_ERROR, "follow_store"),
            };
            Err(ApiError::new(status, code, format!("{op}: {message}")))
        }
        Ok(response) => Ok(response),
        Err(error) => {
            tracing::debug!(%error, op, "follow service unreachable");
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "follow_unavailable",
                format!("follow unavailable: {error}"),
            ))
        }
    }
}

/// Error for an answer that does not fit the request.
fn unexpected(op: &str, response: &Response) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "follow_protocol",
        format!("{op}: unexpected answer from the follow service: {response:?}"),
    )
}

/// Asks for the current status.
async fn fetch_status(state: &AppState) -> Result<FollowStatus, ApiError> {
    match ask(state, Request::Status).await? {
        Response::Status(status) => Ok(status),
        other => Err(unexpected("status", &other)),
    }
}

/// `GET /api/follow` → the service's status.
async fn status(
    State(state): State<AppState>,
    _: AuthUser,
) -> Result<Json<FollowStatus>, ApiError> {
    fetch_status(&state).await.map(Json)
}

/// `PUT /api/follow` (admin) `{"enabled": bool}` → the status after it.
async fn update(
    State(state): State<AppState>,
    _: AdminUser,
    ApiJson(body): ApiJson<FollowUpdate>,
) -> Result<Json<FollowStatus>, ApiError> {
    let request = Request::Set {
        enabled: body.enabled,
    };
    match ask(&state, request).await? {
        Response::Ok => {}
        other => return Err(unexpected("set", &other)),
    }
    fetch_status(&state).await.map(Json)
}
