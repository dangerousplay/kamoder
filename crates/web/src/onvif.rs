//! The ONVIF account (T15, ONVIF-02).
//!
//! ONVIF's `PasswordDigest` needs the plaintext password, so `onvif`
//! uses one dedicated account kept in `etc/onvif.secret` (`user:password`,
//! mode 0600), separate from the web users. This module writes that file in
//! the format `onvif`'s `secret.rs` reads, then asks the supervisor to
//! restart the `onvif` service so it picks the new account up. The password
//! is never returned.

use crate::auth::{AdminUser, AuthUser};
use crate::error::{ApiError, ApiJson};
use crate::services;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::path::Path;
use supervisor::control::{self, Response};

/// The service restarted after a change.
pub const SERVICE: &str = "onvif";
/// Also restarted: RTSP Digest uses the same account, read at start-up.
pub const RTSP_SERVICE: &str = "rtsp";

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/onvif", get(status))
        .route("/onvif/credentials", put(set_credentials))
}

/// The account in `etc/onvif.secret`.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// Reads the secret file: the first non-comment `user:password` line.
/// `Ok(None)` when the file is missing or has no such line.
pub fn load(path: &Path) -> std::io::Result<Option<Credentials>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(text.lines().find_map(|raw| {
        let raw = raw.trim_end_matches('\r');
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }
        let (user, password) = raw.split_once(':')?;
        let user = user.trim();
        (!user.is_empty() && !password.is_empty()).then(|| Credentials {
            user: user.to_owned(),
            password: password.to_owned(),
        })
    }))
}

/// `GET /api/onvif` answer.
#[derive(Debug, Serialize)]
pub struct OnvifStatus {
    pub user: Option<String>,
    pub configured: bool,
}

/// `api.ts` `OnvifCredentials`.
#[derive(Debug, Deserialize)]
pub struct CredentialsRequest {
    pub user: String,
    pub password: String,
}

/// `PUT /api/onvif/credentials` answer.
#[derive(Debug, Serialize)]
pub struct CredentialsResult {
    /// The `onvif` service was restarted with the new account.
    pub restarted: bool,
    /// Why it was not, when it was not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `GET /api/onvif` → `{user, configured}`.
async fn status(State(state): State<AppState>, _: AuthUser) -> Result<Json<OnvifStatus>, ApiError> {
    let path = state.config.onvif_secret.clone();
    let credentials = tokio::task::spawn_blocking(move || load(&path))
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError::internal(format!("cannot read the ONVIF secret: {error}")))?;
    Ok(Json(OnvifStatus {
        configured: credentials.is_some(),
        user: credentials.map(|c| c.user),
    }))
}

fn validate(request: &CredentialsRequest) -> Result<(), ApiError> {
    let user = &request.user;
    if user.is_empty()
        || user.len() > 64
        || user.contains(':')
        || user.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ApiError::bad_request(
            "user: 1-64 characters, no ':', spaces or control characters",
        ));
    }
    let password = &request.password;
    if password.is_empty() || password.len() > 128 || password.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "password: 1-128 characters, no line breaks or control characters",
        ));
    }
    Ok(())
}

/// `PUT /api/onvif/credentials` (admin) `{user, password}` → rewrites
/// `etc/onvif.secret` and restarts `onvif`. A missing `onvif` service or an
/// unreachable supervisor is reported in `note`, not as an error: the file
/// was written and applies at the service's next start.
async fn set_credentials(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    ApiJson(request): ApiJson<CredentialsRequest>,
) -> Result<Json<CredentialsResult>, ApiError> {
    validate(&request)?;
    let path = state.config.onvif_secret.clone();
    let contents = format!("{}:{}\n", request.user, request.password);
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        ::auth::atomic::write(&path, contents.as_bytes(), 0o600)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(|error| ApiError::write_failed("the ONVIF secret", &error))?;
    tracing::info!(admin = %admin.user, user = %request.user, "ONVIF credentials changed");

    let restart = control::Request::Restart {
        service: SERVICE.to_owned(),
    };
    let result = match services::ask(&services::client(&state), &restart).await {
        Ok(Response::Service { .. }) => CredentialsResult {
            restarted: true,
            note: None,
        },
        Ok(other) => CredentialsResult {
            restarted: false,
            note: Some(format!("unexpected supervisor answer: {other:?}")),
        },
        Err(error) if error.code == "unknown_service" => CredentialsResult {
            restarted: false,
            note: Some("no onvif service in supervisor.toml; saved for when it runs".into()),
        },
        Err(error) => CredentialsResult {
            restarted: false,
            note: Some(format!(
                "saved, but the onvif service was not restarted: {}",
                error.message
            )),
        },
    };
    // RTSP reads the account only at start; without this every client gets
    // 401 until the next reboot. Not running (or not configured) is fine.
    let rtsp = control::Request::Restart {
        service: RTSP_SERVICE.to_owned(),
    };
    match services::ask(&services::client(&state), &rtsp).await {
        Ok(_) => tracing::info!("rtsp restarted for the new ONVIF account"),
        Err(error) => tracing::debug!(reason = %error.message, "rtsp not restarted"),
    }
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_onvif() {
        let dir = std::env::temp_dir().join(format!("anyka-web-onvif-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("onvif.secret");
        assert_eq!(load(&path).unwrap(), None);
        std::fs::write(&path, "# seeded\n\nnvr:pa:ss\n").unwrap();
        let credentials = load(&path).unwrap().unwrap();
        assert_eq!(credentials.user, "nvr");
        assert_eq!(credentials.password, "pa:ss");
        assert!(!format!("{credentials:?}").contains("pa:ss"));
    }
}
