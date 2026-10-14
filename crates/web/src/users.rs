//! Web user management (T15, USR-01/02, AUTH-07).
//!
//! The table itself (`auth::WebUsers`) does validation, the last-admin
//! guard, generation bumps and the atomic rewrite. What this module adds is
//! scheduling: adding a user or setting a password derives a PBKDF2 hash
//! (~1 s of the only core), so every change runs on the blocking pool while
//! holding [`crate::state::Inner::pbkdf2`], on a **clone** of the cached
//! table — the cache lock is never held during a hash — and the clone is
//! adopted afterwards. All changes take the same permit, so they are
//! serialized and a clone cannot overwrite a concurrent change.

use crate::auth::{AdminUser, AuthUser};
use crate::error::{ApiError, ApiJson};
use crate::session::LoginResponse;
use crate::state::{now_secs, AppState};
use ::auth::web_users::Error as UsersError;
use ::auth::{Claims, Role, WebUsers};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long a change waits for the PBKDF2 slot (a login may hold it).
const QUEUE_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest accepted password.
const MAX_PASSWORD: usize = 256;

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/users", get(list).post(create))
        .route("/users/me/password", put(change_own_password))
        .route("/users/{name}", put(update).delete(remove))
}

/// `api.ts` `User`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub role: Role,
}

/// `api.ts` `CreateUser`.
#[derive(Debug, Deserialize)]
pub struct CreateUser {
    pub name: String,
    pub password: String,
    pub role: Role,
}

/// `api.ts` `UpdateUser`.
#[derive(Debug, Deserialize)]
pub struct UpdateUser {
    pub role: Option<Role>,
    pub password: Option<String>,
}

/// `api.ts` `ChangePassword`.
#[derive(Debug, Deserialize)]
pub struct ChangePassword {
    pub current: String,
    pub new: String,
}

fn map_error(error: UsersError) -> ApiError {
    match error {
        UsersError::InvalidUsername { .. } | UsersError::EmptyPassword => {
            ApiError::bad_request(error.to_string())
        }
        UsersError::UserExists { .. } => {
            ApiError::new(StatusCode::CONFLICT, "exists", error.to_string())
        }
        UsersError::UnknownUser { .. } => ApiError::not_found(error.to_string()),
        UsersError::LastAdmin => ApiError::new(
            StatusCode::CONFLICT,
            "last_admin",
            "the last admin cannot be deleted or demoted",
        ),
        UsersError::Write { ref source, .. } => ApiError::write_failed("the users file", source),
        other => ApiError::internal(other),
    }
}

fn check_password(password: &str) -> Result<(), ApiError> {
    if password.is_empty() {
        return Err(ApiError::bad_request("password must not be empty"));
    }
    if password.len() > MAX_PASSWORD {
        return Err(ApiError::bad_request("password too long"));
    }
    Ok(())
}

/// Runs `change` on a clone of the users table on the blocking pool, holding
/// the PBKDF2 permit, and adopts the clone if it succeeded.
async fn modify<T, F>(state: &AppState, change: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&mut WebUsers) -> Result<T, ApiError> + Send + 'static,
{
    let _permit = tokio::time::timeout(QUEUE_TIMEOUT, state.pbkdf2.acquire())
        .await
        .map_err(|_| {
            ApiError::unavailable("busy hashing a password, try again")
                .retry_after(Duration::from_secs(5))
        })?
        .map_err(ApiError::internal)?;
    let mut table = state.users().clone();
    let (table, result) = tokio::task::spawn_blocking(move || {
        let result = change(&mut table);
        (table, result)
    })
    .await
    .map_err(ApiError::internal)?;
    if result.is_ok() {
        *state.users() = table;
    }
    result
}

/// `GET /api/users` (admin) → `[User]`.
async fn list(State(state): State<AppState>, _: AdminUser) -> Json<Vec<User>> {
    Json(
        state
            .users()
            .list()
            .into_iter()
            .map(|u| User {
                name: u.name,
                role: u.role,
            })
            .collect(),
    )
}

/// `POST /api/users` (admin) `{name, password, role}` → 201 `User`.
async fn create(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    ApiJson(request): ApiJson<CreateUser>,
) -> Result<(StatusCode, Json<User>), ApiError> {
    check_password(&request.password)?;
    let CreateUser {
        name,
        password,
        role,
    } = request;
    let user = User {
        name: name.clone(),
        role,
    };
    modify(&state, move |table| {
        table.add(&name, role, &password).map_err(map_error)
    })
    .await?;
    tracing::info!(admin = %admin.user, user = %user.name, role = %role, "web user added");
    Ok((StatusCode::CREATED, Json(user)))
}

/// `PUT /api/users/{name}` (admin) `{role?, password?}` → `User`. Either
/// change ends that user's sessions.
async fn update(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(name): Path<String>,
    ApiJson(request): ApiJson<UpdateUser>,
) -> Result<Json<User>, ApiError> {
    if request.role.is_none() && request.password.is_none() {
        return Err(ApiError::bad_request(
            "nothing to change: give role and/or password",
        ));
    }
    if let Some(password) = &request.password {
        check_password(password)?;
    }
    let target = name.clone();
    let role = modify(&state, move |table| {
        // Role first: a refused demotion must not leave a changed password.
        if let Some(role) = request.role {
            table.set_role(&target, role).map_err(map_error)?;
        }
        if let Some(password) = &request.password {
            table.set_password(&target, password).map_err(map_error)?;
        }
        table
            .lookup(&target)
            .map(|(role, _)| role)
            .ok_or_else(|| ApiError::not_found(format!("no such user {target:?}")))
    })
    .await?;
    tracing::info!(admin = %admin.user, user = %name, %role, "web user updated");
    Ok(Json(User { name, role }))
}

/// `DELETE /api/users/{name}` (admin) → 204. 409 `last_admin`.
async fn remove(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let target = name.clone();
    modify(&state, move |table| {
        table.delete(&target).map_err(map_error)
    })
    .await?;
    tracing::info!(admin = %admin.user, user = %name, "web user deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/users/me/password` (any user) `{current, new}` → a fresh
/// session `{token, role, exp}`: the change bumps the generation, which ends
/// the session used for this request. A wrong `current` is 403
/// `wrong_password` — not 401, which the UI treats as "logged out".
async fn change_own_password(
    State(state): State<AppState>,
    user: AuthUser,
    ApiJson(request): ApiJson<ChangePassword>,
) -> Result<Json<LoginResponse>, ApiError> {
    check_password(&request.new)?;
    if request.current.len() > MAX_PASSWORD {
        return Err(wrong_password());
    }
    let name = user.user.clone();
    let (role, gen) = modify(&state, move |table| {
        if table.verify(&name, &request.current).is_none() {
            return Err(wrong_password());
        }
        table.set_password(&name, &request.new).map_err(map_error)?;
        table
            .lookup(&name)
            .ok_or_else(|| ApiError::unauthorized("user no longer exists"))
    })
    .await?;
    let iat = now_secs();
    let exp = iat + state.config.token_ttl.as_secs();
    let token = state.jwt.sign(&Claims {
        sub: user.user.clone(),
        role,
        gen,
        iat,
        exp,
    });
    tracing::info!(user = %user.user, "own password changed");
    Ok(Json(LoginResponse { token, role, exp }))
}

fn wrong_password() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "wrong_password",
        "current password is wrong",
    )
}
