//! Request authentication: extractors that turn a bearer token into a user.
//!
//! Why JWTs rather than server-side sessions: the check has to be cheap and
//! survive restarts without a session store on the SD card. A token is
//! verified with one HMAC-SHA256 (see `auth::jwt`), then the user is
//! looked up in the in-memory users table, which is re-`stat`ed at most once
//! a second. The token carries the user's generation counter, so deleting a
//! user or changing their role or password ends their sessions on the next
//! request — the one thing plain stateless JWTs cannot do.
//!
//! Handlers opt in by argument type:
//!
//! - [`AuthUser`] — any logged-in user (`Authorization: Bearer <jwt>`).
//! - [`AdminUser`] — the same, plus 403 `forbidden` for viewers.
//! - [`StreamUser`] — also accepts `?token=<stream token>` (never a JWT) for
//!   URLs that `<img>`/`EventSource` fetch without headers; see
//!   [`crate::stream_token`]. Use it only on those GET routes.

use crate::error::ApiError;
use crate::state::{lock, now_secs, AppState};
use ::auth::Role;
use axum::extract::{FromRequestParts, Query};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use serde::Deserialize;

/// An authenticated user, of either role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthUser {
    pub user: String,
    pub role: Role,
    /// When the credential used for this request expires (Unix seconds).
    pub exp: u64,
}

impl AuthUser {
    /// Whether this user may change things.
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
}

/// An authenticated admin; viewers get 403.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminUser(pub AuthUser);

/// A user authenticated by bearer header or by a `?token=` stream token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamUser(pub AuthUser);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = bearer(parts)?.ok_or_else(|| ApiError::unauthorized("login required"))?;
        session(state, token)
    }
}

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        if !user.is_admin() {
            return Err(ApiError::forbidden());
        }
        Ok(Self(user))
    }
}

impl FromRequestParts<AppState> for StreamUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if let Some(token) = bearer(parts)? {
            return session(state, token).map(Self);
        }

        #[derive(Deserialize)]
        struct TokenQuery {
            token: Option<String>,
        }
        let token = Query::<TokenQuery>::try_from_uri(&parts.uri)
            .ok()
            .and_then(|Query(query)| query.token)
            .ok_or_else(|| ApiError::unauthorized("login required"))?;
        let redeemed = lock(&state.stream_tokens).redeem(&token, now_secs());
        let (user, gen, exp) =
            redeemed.ok_or_else(|| ApiError::unauthorized("invalid or expired stream token"))?;
        let (role, current_gen) = state
            .lookup(&user)
            .ok_or_else(|| ApiError::unauthorized("user no longer exists"))?;
        if current_gen != gen {
            return Err(ApiError::unauthorized("session ended; log in again"));
        }
        Ok(Self(AuthUser { user, role, exp }))
    }
}

/// The token from `Authorization: Bearer …`, if the header is present.
fn bearer(parts: &Parts) -> Result<Option<&str>, ApiError> {
    let Some(value) = parts.headers.get(AUTHORIZATION) else {
        return Ok(None);
    };
    let malformed = || ApiError::unauthorized("expected Authorization: Bearer <token>");
    let value = value.to_str().map_err(|_| malformed())?;
    let (scheme, token) = value.split_once(' ').ok_or_else(malformed)?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.trim().is_empty() {
        return Err(malformed());
    }
    Ok(Some(token.trim()))
}

/// Verifies a session JWT and that its user still exists with the same role
/// and generation.
fn session(state: &AppState, token: &str) -> Result<AuthUser, ApiError> {
    let claims = state.jwt.verify(token, now_secs()).map_err(|error| {
        tracing::debug!(%error, "rejected session token");
        ApiError::unauthorized("invalid or expired session; log in again")
    })?;
    match state.lookup(&claims.sub) {
        Some((role, gen)) if role == claims.role && gen == claims.gen => Ok(AuthUser {
            user: claims.sub,
            role,
            exp: claims.exp,
        }),
        Some(_) => Err(ApiError::unauthorized("session ended; log in again")),
        None => Err(ApiError::unauthorized("user no longer exists")),
    }
}
