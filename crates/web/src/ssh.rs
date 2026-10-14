//! SSH access: the `sshd` password and its authorized public keys.
//!
//! Both files belong to `sshd` (`etc/anyka-sshd.passwd`,
//! `etc/anyka-sshd.authorized_keys`), which re-reads them when they change,
//! so nothing is restarted here. Passwords are hashed exactly as `sshd hash`
//! does; keys are validated with the same parser `sshd` uses, so what the UI
//! accepts is what logs in.

use crate::auth::AdminUser;
use crate::error::{ApiError, ApiJson};
use crate::state::AppState;
use ::auth::authorized_keys::{self, Key, MAX_KEYS};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Login used when the credentials file does not name one yet.
pub const DEFAULT_USER: &str = "root";

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ssh", get(status))
        .route("/ssh/password", put(set_password))
        .route("/ssh/keys", post(add_key).delete(remove_key))
}

/// `api.ts` `SshKey`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct KeySummary {
    pub algorithm: String,
    pub comment: String,
    pub fingerprint: String,
}

impl From<&Key> for KeySummary {
    fn from(k: &Key) -> Self {
        KeySummary {
            algorithm: k.algorithm.clone(),
            comment: k.comment.clone(),
            fingerprint: k.fingerprint(),
        }
    }
}

/// `api.ts` `SshStatus`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SshStatus {
    pub user: String,
    pub password_set: bool,
    pub keys: Vec<KeySummary>,
}

/// User of the first entry in the credentials file.
fn passwd_user(passwd: &Path) -> Option<String> {
    let text = std::fs::read_to_string(passwd).ok()?;
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .and_then(|l| l.split(':').next())
        .filter(|u| !u.is_empty())
        .map(str::to_owned)
}

pub fn read_status(passwd: &Path, keys: &Path) -> SshStatus {
    let user = passwd_user(passwd);
    SshStatus {
        password_set: user.is_some(),
        user: user.unwrap_or_else(|| DEFAULT_USER.to_owned()),
        keys: authorized_keys::load(keys)
            .iter()
            .map(KeySummary::from)
            .collect(),
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(ApiError::internal)?
}

/// `GET /api/ssh` (admin).
async fn status(State(state): State<AppState>, _: AdminUser) -> Result<Json<SshStatus>, ApiError> {
    let (p, k) = (
        state.config.ssh_passwd.clone(),
        state.config.ssh_authorized_keys.clone(),
    );
    Ok(Json(blocking(move || Ok(read_status(&p, &k))).await?))
}

#[derive(Debug, Deserialize)]
pub struct PasswordRequest {
    pub password: String,
}

/// `PUT /api/ssh/password` (admin) `{password}` → replaces the credentials
/// file with one entry for the current SSH user.
async fn set_password(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    ApiJson(request): ApiJson<PasswordRequest>,
) -> Result<Json<SshStatus>, ApiError> {
    let password = request.password;
    if password.len() < 8 || password.len() > 128 || password.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "password: 8-128 characters, no line breaks or control characters",
        ));
    }
    let _permit = state.pbkdf2.acquire().await.map_err(ApiError::internal)?;
    let (p, k) = (
        state.config.ssh_passwd.clone(),
        state.config.ssh_authorized_keys.clone(),
    );
    let status = blocking(move || {
        let user = passwd_user(&p).unwrap_or_else(|| DEFAULT_USER.to_owned());
        let line = ::auth::hash_password(&user, &password, ::auth::DEFAULT_ITERATIONS)
            .map_err(ApiError::internal)?;
        write_secret(&p, format!("{line}\n").as_bytes())
            .map_err(|e| ApiError::write_failed("the SSH credentials", &e))?;
        Ok(read_status(&p, &k))
    })
    .await?;
    tracing::info!(admin = %admin.user, "SSH password changed");
    Ok(Json(status))
}

#[derive(Debug, Deserialize)]
pub struct KeyRequest {
    pub key: String,
}

/// Writes a secret file (0600), creating its directory.
fn write_secret(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    ::auth::atomic::write(path, contents, 0o600)
}

fn write_keys(path: &Path, keys: &[Key]) -> Result<(), ApiError> {
    write_secret(path, authorized_keys::render(keys).as_bytes())
        .map_err(|e| ApiError::write_failed("the SSH authorized keys", &e))
}

/// `POST /api/ssh/keys` (admin) `{key}` → 201 with the new status. A key
/// already present is not added twice.
async fn add_key(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    ApiJson(request): ApiJson<KeyRequest>,
) -> Result<(StatusCode, Json<SshStatus>), ApiError> {
    let key = Key::parse(&request.key)
        .map_err(|e| ApiError::bad_request(format!("not an SSH public key: {e}")))?;
    let fingerprint = key.fingerprint();
    let (p, k) = (
        state.config.ssh_passwd.clone(),
        state.config.ssh_authorized_keys.clone(),
    );
    let status = blocking(move || {
        let mut keys = authorized_keys::load(&k);
        if !keys.iter().any(|existing| existing.blob == key.blob) {
            if keys.len() >= MAX_KEYS {
                return Err(ApiError::bad_request(format!("at most {MAX_KEYS} keys")));
            }
            keys.push(key);
            write_keys(&k, &keys)?;
        }
        Ok(read_status(&p, &k))
    })
    .await?;
    tracing::info!(admin = %admin.user, %fingerprint, "SSH key added");
    Ok((StatusCode::CREATED, Json(status)))
}

#[derive(Debug, Deserialize)]
pub struct FingerprintQuery {
    pub fingerprint: String,
}

/// `DELETE /api/ssh/keys?fingerprint=SHA256:…` (admin).
async fn remove_key(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Query(q): Query<FingerprintQuery>,
) -> Result<Json<SshStatus>, ApiError> {
    let (p, k) = (
        state.config.ssh_passwd.clone(),
        state.config.ssh_authorized_keys.clone(),
    );
    let fingerprint = q.fingerprint.clone();
    let status = blocking(move || {
        let mut keys = authorized_keys::load(&k);
        let before = keys.len();
        keys.retain(|key| key.fingerprint() != fingerprint);
        if keys.len() == before {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "no such key",
            ));
        }
        write_keys(&k, &keys)?;
        Ok(read_status(&p, &k))
    })
    .await?;
    tracing::info!(admin = %admin.user, fingerprint = %q.fingerprint, "SSH key removed");
    Ok(Json(status))
}
