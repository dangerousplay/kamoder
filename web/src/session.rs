//! Session routes: login, who-am-I, and stream tokens.
//!
//! Login is the only place a password is checked. PBKDF2 at 20 000
//! iterations takes about a second of the single 400 MHz core, so it runs on
//! the blocking pool (the event loop keeps serving), one derivation at a time
//! (a semaphore — two in parallel would only make both slower), and failures
//! are padded to a fixed minimum duration and throttled per IP
//! ([`crate::rate_limit`]).

use crate::auth::AuthUser;
use crate::error::{ApiError, ApiJson};
use crate::rate_limit::WINDOW;
use crate::state::{lock, now_secs, AppState};
use ::auth::{Claims, Role};
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::routing::{get, post};
use axum::{Json, Router};
use metrics::counter;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

/// How long a login waits for the PBKDF2 slot before answering 503.
const LOGIN_QUEUE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest accepted password; PBKDF2 cost does not grow with it, but there
/// is no reason to accept megabytes.
const MAX_PASSWORD: usize = 256;

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/me", get(me))
        .route("/logs/stream-token", post(stream_token))
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub user: String,
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub role: Role,
    pub exp: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Me {
    pub user: String,
    pub role: Role,
    pub exp: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamTokenResponse {
    pub token: String,
    pub exp: u64,
}

/// The peer address, from `ConnectInfo`; unspecified when absent (tests).
///
/// No `X-Forwarded-For`: the camera is reached directly, and trusting that
/// header would let any client pick its own rate-limit bucket.
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |info| info.0.ip()),
        ))
    }
}

/// `POST /api/auth/login` `{user, password}` → `{token, role, exp}`.
async fn login(
    state: axum::extract::State<AppState>,
    ClientIp(ip): ClientIp,
    ApiJson(request): ApiJson<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    let state = state.0;
    check_blocked(&state, ip)?;
    if request.user.is_empty() || request.password.is_empty() {
        return Err(ApiError::bad_request("user and password are required"));
    }
    if request.password.len() > MAX_PASSWORD {
        return Err(ApiError::bad_request("password too long"));
    }

    let permit = tokio::time::timeout(LOGIN_QUEUE_TIMEOUT, state.pbkdf2.acquire())
        .await
        .map_err(|_| {
            ApiError::unavailable("login busy, try again").retry_after(Duration::from_secs(5))
        })?
        .map_err(ApiError::internal)?;
    // Re-check under the permit: queued attempts from one IP must not all
    // slip past the limit together.
    check_blocked(&state, ip)?;

    let started = Instant::now();
    // Clone the (small) table so no lock is held during the derivation.
    let users = state.users().clone();
    let LoginRequest { user, password } = request;
    let (user, verified) = tokio::task::spawn_blocking(move || {
        let verified = users.verify(&user, &password);
        (user, verified)
    })
    .await
    .map_err(ApiError::internal)?;
    drop(permit);

    let Some((role, gen)) = verified else {
        counter!("kamoder_web_login_failures_total").increment(1);
        let blocked = lock(&state.limiter).record_failure(ip, Instant::now());
        tracing::warn!(%ip, ?user, blocked, "failed login");
        // Pad outside the permit so a failing client does not hold up others.
        let pad = state
            .tuning
            .login_failure_delay
            .saturating_sub(started.elapsed());
        tokio::time::sleep(pad).await;
        return Err(ApiError::unauthorized("invalid credentials"));
    };

    lock(&state.limiter).clear(ip);
    let iat = now_secs();
    let exp = iat + state.config.token_ttl.as_secs();
    let token = state.jwt.sign(&Claims {
        sub: user.clone(),
        role,
        gen,
        iat,
        exp,
    });
    tracing::info!(%ip, %user, %role, "login");
    Ok(Json(LoginResponse { token, role, exp }))
}

fn check_blocked(state: &AppState, ip: IpAddr) -> Result<(), ApiError> {
    match lock(&state.limiter).blocked(ip, Instant::now()) {
        Some(left) => Err(ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            format!(
                "too many failed logins; try again in {} minutes",
                left.as_secs().div_ceil(60).max(1)
            ),
        )
        .retry_after(left.max(Duration::from_secs(1)).min(WINDOW))),
        None => Ok(()),
    }
}

/// `GET /api/me` → `{user, role, exp}`.
async fn me(user: AuthUser) -> Json<Me> {
    Json(Me {
        user: user.user,
        role: user.role,
        exp: user.exp,
    })
}

/// `POST /api/logs/stream-token` → `{token, exp}`: a 60 s token for
/// `?token=` on the log-stream, snapshot and MJPEG URLs.
async fn stream_token(
    state: axum::extract::State<AppState>,
    user: AuthUser,
) -> Result<Json<StreamTokenResponse>, ApiError> {
    let (_, gen) = state
        .lookup(&user.user)
        .ok_or_else(|| ApiError::unauthorized("user no longer exists"))?;
    let (token, exp) = lock(&state.stream_tokens)
        .issue(&user.user, gen, now_secs())
        .ok_or_else(|| ApiError::internal("system random generator failed"))?;
    Ok(Json(StreamTokenResponse { token, exp }))
}
