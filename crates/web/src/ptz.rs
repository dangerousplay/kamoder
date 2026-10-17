//! Pan/tilt, through the PTZ owner's socket (T16, PTZ-01/03/05).
//!
//! `onvif` owns the motor (D4) and serves `/tmp/anyka-ptz.sock`; this
//! module only translates between `webui/src/lib/api.ts` and
//! [`ptz_proto`]. No backend linked here: a missing socket means
//! "no PTZ on this camera (or the owner is down)", answered 503
//! `ptz_unavailable` on every route so the UI hides the pad (PTZ-03).
//!
//! Continuous moves carry the protocol's deadman ([`DEFAULT_MOVE_TIMEOUT_MS`]):
//! the UI re-sends the move while a direction is held and the owner stops
//! the motor on its own if the browser goes away. Viewers may read the state;
//! every command is admin-only (PTZ-05).

use crate::auth::{AdminUser, AuthUser};
use crate::error::{ApiError, ApiJson};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use ptz_proto::{Client, ErrorCode, Request, Response, DEFAULT_MOVE_TIMEOUT_MS};
use ptz_proto::{Limits, Preset};
use serde::{Deserialize, Serialize};

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ptz", get(status))
        .route("/ptz/move", post(move_))
        .route("/ptz/stop", post(stop))
        .route("/ptz/goto", post(goto))
        .route("/ptz/home", post(home))
        .route("/ptz/presets", get(presets).post(save_preset))
        .route("/ptz/presets/{id}", delete(delete_preset))
}

/// `api.ts` `PtzPreset`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PtzPreset {
    /// The preset token as a number (tokens the owner issues are numeric).
    pub id: u32,
    pub token: String,
    pub name: String,
    pub pan: f32,
    pub tilt: f32,
}

/// `api.ts` `PtzStatus`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PtzStatus {
    pub available: bool,
    pub backend: String,
    pub pan: f32,
    pub tilt: f32,
    pub moving: bool,
    pub limits: Limits,
    pub presets: Vec<PtzPreset>,
}

/// `api.ts` `PtzMove`: velocities in `[-1, 1]`, scaled by `speed`.
#[derive(Debug, Deserialize)]
pub struct PtzMove {
    pub pan: f32,
    pub tilt: f32,
    pub speed: Option<f32>,
    /// Deadman override; the owner clamps it.
    pub timeout_ms: Option<u32>,
}

/// `api.ts` `PtzGoto`: a preset id or a position in degrees.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum PtzGoto {
    Preset { preset: u32 },
    Position { pan: f32, tilt: f32 },
}

#[derive(Debug, Deserialize)]
pub struct SavePreset {
    pub name: String,
    /// Overwrite this preset instead of creating one.
    pub id: Option<u32>,
}

fn preset(p: Preset) -> Option<PtzPreset> {
    Some(PtzPreset {
        id: p.token.parse().ok()?,
        token: p.token,
        name: p.name,
        pan: p.pan_deg,
        tilt: p.tilt_deg,
    })
}

/// Sends one request. A connect failure (no owner) is 503 `ptz_unavailable`;
/// the owner's refusals map per the protocol's suggestion.
async fn ask(state: &AppState, request: Request) -> Result<Response, ApiError> {
    let op = request.op();
    match Client::new(&state.config.ptz_socket)
        .request(&request)
        .await
    {
        Ok(Response::Error { code, message }) => {
            let (status, code) = match code {
                ErrorCode::BadRequest | ErrorCode::TooLong | ErrorCode::InvalidArgument => {
                    (StatusCode::BAD_REQUEST, "bad_request")
                }
                ErrorCode::UnknownPreset => (StatusCode::NOT_FOUND, "unknown_preset"),
                ErrorCode::TooManyPresets => (StatusCode::CONFLICT, "too_many_presets"),
                ErrorCode::Unsupported => (StatusCode::NOT_IMPLEMENTED, "unsupported"),
                ErrorCode::Backend => (StatusCode::INTERNAL_SERVER_ERROR, "ptz_backend"),
            };
            Err(ApiError::new(status, code, message))
        }
        Ok(response) => Ok(response),
        Err(error) => {
            tracing::debug!(%error, op, "PTZ owner unreachable");
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "ptz_unavailable",
                format!("PTZ unavailable: {error}"),
            ))
        }
    }
}

fn unexpected(response: &Response) -> ApiError {
    ApiError::internal(format!("unexpected PTZ response: {response:?}"))
}

/// Expects `ok`.
async fn command(state: &AppState, request: Request) -> Result<StatusCode, ApiError> {
    match ask(state, request).await? {
        Response::Ok => Ok(StatusCode::NO_CONTENT),
        other => Err(unexpected(&other)),
    }
}

async fn preset_list(state: &AppState) -> Result<Vec<PtzPreset>, ApiError> {
    match ask(state, Request::Presets).await? {
        Response::Presets { presets } => Ok(presets.into_iter().filter_map(preset).collect()),
        other => Err(unexpected(&other)),
    }
}

/// `GET /api/ptz` → `PtzStatus` (position, limits, presets).
async fn status(State(state): State<AppState>, _: AuthUser) -> Result<Json<PtzStatus>, ApiError> {
    let Response::Status {
        backend,
        limits,
        position,
        ..
    } = ask(&state, Request::Status).await?
    else {
        return Err(ApiError::internal("unexpected PTZ response to status"));
    };
    // Presets are optional decoration: a backend without them still moves.
    let presets = preset_list(&state).await.unwrap_or_default();
    Ok(Json(PtzStatus {
        available: true,
        backend,
        pan: position.pan_deg,
        tilt: position.tilt_deg,
        moving: position.moving,
        limits,
        presets,
    }))
}

fn finite(values: &[f32]) -> Result<(), ApiError> {
    if values.iter().all(|v| v.is_finite()) {
        Ok(())
    } else {
        Err(ApiError::bad_request("numbers must be finite"))
    }
}

/// `POST /api/ptz/move` (admin) `{pan, tilt, speed?}` → 204. Moves until
/// `stop`, a newer command, or the deadman.
async fn move_(
    State(state): State<AppState>,
    _: AdminUser,
    ApiJson(body): ApiJson<PtzMove>,
) -> Result<StatusCode, ApiError> {
    let speed = body.speed.unwrap_or(1.0);
    finite(&[body.pan, body.tilt, speed])?;
    let scale = |v: f32| (v * speed.clamp(0.0, 1.0)).clamp(-1.0, 1.0);
    command(
        &state,
        Request::Move {
            pan: scale(body.pan),
            tilt: scale(body.tilt),
            timeout_ms: Some(body.timeout_ms.unwrap_or(DEFAULT_MOVE_TIMEOUT_MS)),
            source: None,
        },
    )
    .await
}

/// `POST /api/ptz/stop` (admin) → 204.
async fn stop(State(state): State<AppState>, _: AdminUser) -> Result<StatusCode, ApiError> {
    command(&state, Request::Stop { source: None }).await
}

/// `POST /api/ptz/goto` (admin) `{pan, tilt}` (degrees) or `{preset}` → 204.
async fn goto(
    State(state): State<AppState>,
    _: AdminUser,
    ApiJson(body): ApiJson<PtzGoto>,
) -> Result<StatusCode, ApiError> {
    let request = match body {
        PtzGoto::Preset { preset } => Request::GotoPreset {
            token: preset.to_string(),
            source: None,
        },
        PtzGoto::Position { pan, tilt } => {
            finite(&[pan, tilt])?;
            Request::Goto {
                pan,
                tilt,
                source: None,
            }
        }
    };
    command(&state, request).await
}

/// `POST /api/ptz/home` (admin) → 204.
async fn home(State(state): State<AppState>, _: AdminUser) -> Result<StatusCode, ApiError> {
    command(&state, Request::Home { source: None }).await
}

/// `GET /api/ptz/presets` → `[PtzPreset]`.
async fn presets(
    State(state): State<AppState>,
    _: AuthUser,
) -> Result<Json<Vec<PtzPreset>>, ApiError> {
    preset_list(&state).await.map(Json)
}

/// `POST /api/ptz/presets` (admin) `{name, id?}` → the stored `PtzPreset`
/// (the current position under that name).
async fn save_preset(
    State(state): State<AppState>,
    _: AdminUser,
    ApiJson(body): ApiJson<SavePreset>,
) -> Result<Json<PtzPreset>, ApiError> {
    let request = Request::SetPreset {
        name: body.name,
        token: body.id.map(|id| id.to_string()),
    };
    match ask(&state, request).await? {
        Response::Preset { preset: stored } => preset(stored)
            .map(Json)
            .ok_or_else(|| ApiError::internal("PTZ owner issued a non-numeric preset token")),
        other => Err(unexpected(&other)),
    }
}

/// `DELETE /api/ptz/presets/{id}` (admin) → 204.
async fn delete_preset(
    State(state): State<AppState>,
    _: AdminUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id: u32 = id
        .parse()
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "unknown_preset", "no such preset"))?;
    command(
        &state,
        Request::RemovePreset {
            token: id.to_string(),
        },
    )
    .await
}
