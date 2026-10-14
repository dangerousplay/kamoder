//! Lights, through the lights owner's socket (IND-07).
//!
//! The `lights` service owns the IR-cut filter, IR LEDs, white light,
//! red/blue alarm lights and speaker amp, and serves
//! `/tmp/kamoder-lights.sock`; this module only translates between
//! `webui/src/lib/api.ts` and [`lights_proto`]. A missing socket is 503
//! `lights_unavailable` on every route, so the UI hides the panel.
//!
//! ```text
//! GET /api/lights                      (any user)
//!   → {"mode":"day","white":false,"alarm":{"state":"off"},"muted":true}
//! PUT /api/lights                      (admin) any subset of
//!   {"mode":"night","white":true,"alarm":{"state":"blink","period_ms":500},"muted":true}
//!   → the full state after the change
//! ```
//!
//! A PUT applies its fields in the order mode, white, alarm, muted, one
//! owner request each; if one is refused the earlier ones stay applied (the
//! error says which). `alarm.period_ms` defaults to
//! [`DEFAULT_BLINK_PERIOD_MS`] and must be within the owner's range.

use crate::auth::{AdminUser, AuthUser};
use crate::error::{ApiError, ApiJson};
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use lights_proto::{Alarm, Client, ErrorCode, Mode, Request, Response, DEFAULT_BLINK_PERIOD_MS};
use serde::Deserialize;

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/lights", get(status).put(update))
}

/// `api.ts` `LightsAlarm`: the protocol's alarm with an optional period.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum AlarmBody {
    Off,
    On,
    Blink { period_ms: Option<u32> },
}

impl From<AlarmBody> for Alarm {
    fn from(body: AlarmBody) -> Self {
        match body {
            AlarmBody::Off => Alarm::Off,
            AlarmBody::On => Alarm::On,
            AlarmBody::Blink { period_ms } => Alarm::Blink {
                period_ms: period_ms.unwrap_or(DEFAULT_BLINK_PERIOD_MS),
            },
        }
    }
}

/// `api.ts` `LightsUpdate`: any subset of the state.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LightsUpdate {
    pub mode: Option<Mode>,
    pub white: Option<bool>,
    pub alarm: Option<AlarmBody>,
    pub muted: Option<bool>,
}

impl LightsUpdate {
    /// The owner requests, in the documented order.
    fn requests(&self) -> Vec<Request> {
        let mut requests = Vec::new();
        if let Some(mode) = self.mode {
            requests.push(Request::SetMode { mode });
        }
        if let Some(on) = self.white {
            requests.push(Request::SetWhite { on });
        }
        if let Some(alarm) = self.alarm {
            requests.push(Request::SetAlarm {
                alarm: alarm.into(),
            });
        }
        if let Some(muted) = self.muted {
            requests.push(Request::SetMute { muted });
        }
        requests
    }
}

/// Sends one request and returns the state it answers with. A connect
/// failure (no owner) is 503 `lights_unavailable`; the owner's refusals map
/// per the protocol's suggestion.
async fn ask(state: &AppState, request: Request) -> Result<lights_proto::State, ApiError> {
    let op = request.op();
    match Client::new(&state.config.lights_socket)
        .request(&request)
        .await
    {
        Ok(Response::State { state }) => Ok(state),
        Ok(Response::Error { code, message }) => {
            let (status, code) = match code {
                ErrorCode::BadRequest | ErrorCode::TooLong | ErrorCode::InvalidArgument => {
                    (StatusCode::BAD_REQUEST, "bad_request")
                }
                ErrorCode::Gpio => (StatusCode::INTERNAL_SERVER_ERROR, "lights_gpio"),
            };
            Err(ApiError::new(status, code, format!("{op}: {message}")))
        }
        Err(error) => {
            tracing::debug!(%error, op, "lights owner unreachable");
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "lights_unavailable",
                format!("lights unavailable: {error}"),
            ))
        }
    }
}

/// `GET /api/lights` → the owner's state.
async fn status(
    State(state): State<AppState>,
    _: AuthUser,
) -> Result<Json<lights_proto::State>, ApiError> {
    ask(&state, Request::Get).await.map(Json)
}

/// `PUT /api/lights` (admin) `LightsUpdate` → the state after it.
async fn update(
    State(state): State<AppState>,
    _: AdminUser,
    ApiJson(body): ApiJson<LightsUpdate>,
) -> Result<Json<lights_proto::State>, ApiError> {
    let requests = body.requests();
    if requests.is_empty() {
        return Err(ApiError::bad_request(
            "nothing to change: give mode, white, alarm or muted",
        ));
    }
    let mut last = None;
    for request in requests {
        last = Some(ask(&state, request).await?);
    }
    Ok(Json(last.expect("at least one request was sent")))
}
