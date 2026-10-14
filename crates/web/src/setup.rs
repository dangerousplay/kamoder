//! First-run setup: whether the onboarding wizard has been finished, and
//! what it still has to offer.
//!
//! The wizard itself is the SPA's; each of its steps uses the ordinary API
//! (own password, ONVIF account, SSH password and keys). This module only
//! keeps the "done" marker (`etc/setup-complete`) and summarises the state
//! the wizard starts from, so it can pre-fill and skip what is already set.

use crate::auth::{AdminUser, AuthUser};
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/setup", get(status).post(complete).delete(reopen))
}

/// `api.ts` `SetupStatus`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SetupStatus {
    /// The wizard was finished (or skipped) by an admin.
    pub completed: bool,
    pub onvif_configured: bool,
    pub onvif_user: Option<String>,
    pub ssh_password_set: bool,
    pub ssh_keys: usize,
}

fn read(state: &AppState) -> SetupStatus {
    let c = &state.config;
    let onvif = crate::onvif::load(&c.onvif_secret).ok().flatten();
    let ssh = crate::ssh::read_status(&c.ssh_passwd, &c.ssh_authorized_keys);
    SetupStatus {
        completed: c.setup_marker.is_file(),
        onvif_configured: onvif.is_some(),
        onvif_user: onvif.map(|o| o.user),
        ssh_password_set: ssh.password_set,
        ssh_keys: ssh.keys.len(),
    }
}

async fn read_blocking(state: AppState) -> Result<SetupStatus, ApiError> {
    tokio::task::spawn_blocking(move || read(&state))
        .await
        .map_err(ApiError::internal)
}

/// `GET /api/setup`.
async fn status(State(state): State<AppState>, _: AuthUser) -> Result<Json<SetupStatus>, ApiError> {
    Ok(Json(read_blocking(state).await?))
}

/// `POST /api/setup` (admin): marks setup finished.
async fn complete(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> Result<Json<SetupStatus>, ApiError> {
    let path = state.config.setup_marker.clone();
    let when = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let text = format!("completed_at={when}\ncompleted_by={}\n", admin.user);
    tokio::task::spawn_blocking(move || {
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        ::auth::atomic::write(&path, text.as_bytes(), 0o644)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(|e| ApiError::write_failed("the setup marker", &e))?;
    tracing::info!(admin = %admin.user, "first-run setup completed");
    Ok(Json(read_blocking(state).await?))
}

/// `DELETE /api/setup` (admin): shows the wizard again at next login.
async fn reopen(
    State(state): State<AppState>,
    AdminUser(_): AdminUser,
) -> Result<Json<SetupStatus>, ApiError> {
    let path = state.config.setup_marker.clone();
    tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(|e| ApiError::write_failed("the setup marker", &e))?;
    Ok(Json(read_blocking(state).await?))
}
