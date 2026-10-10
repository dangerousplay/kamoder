//! The API's error response and a JSON extractor that uses it.
//!
//! Every non-2xx answer under `/api` is `{"error": "<message>", "code":
//! "<code>"}` — the shape `webui/src/lib/api.ts` (`ApiErrorBody`) decodes.
//! `code` is a stable machine-readable tag (`auth`, `forbidden`,
//! `rate_limited`, …); `error` is for humans and may change.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use std::borrow::Cow;
use std::time::Duration;

/// An error answered as `{error, code}` with an HTTP status.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: Cow<'static, str>,
    /// Sent as `Retry-After` (429/503).
    pub retry_after: Option<Duration>,
}

impl ApiError {
    /// Any status, code and message.
    pub fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
        }
    }

    /// Adds a `Retry-After` header.
    pub fn retry_after(mut self, after: Duration) -> Self {
        self.retry_after = Some(after);
        self
    }

    /// 401 `auth`: missing, bad or stale credentials.
    pub fn unauthorized(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "auth", message)
    }

    /// 403 `forbidden`: authenticated but not allowed (viewer on an admin route).
    pub fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", "admin role required")
    }

    /// 400 `bad_request`.
    pub fn bad_request(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    /// 404 `not_found`.
    pub fn not_found(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    /// 409 `conflict`.
    pub fn conflict(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    /// 503 `unavailable`: a backend (supervisor, video, PTZ) is not there.
    pub fn unavailable(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }

    /// A file could not be written. The SD card being full or read-only (a
    /// FAT card remounted read-only after errors) is 507 `storage` with the
    /// reason, anything else 500 `write_failed`; reads keep working.
    pub fn write_failed(what: &str, error: &std::io::Error) -> Self {
        tracing::error!(%error, what, "write failed");
        let storage = matches!(
            error.raw_os_error(),
            Some(libc::ENOSPC | libc::EROFS | libc::EDQUOT)
        );
        let status = if storage {
            StatusCode::INSUFFICIENT_STORAGE
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        let code = if storage { "storage" } else { "write_failed" };
        Self::new(status, code, format!("cannot write {what}: {error}"))
    }

    /// 500 `internal`. The detail is logged, not sent.
    pub fn internal(detail: impl std::fmt::Display) -> Self {
        tracing::error!(%detail, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error; see the web log",
        )
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.status.as_u16(),
            self.code,
            self.message
        )
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.message, "code": self.code });
        let mut response = (self.status, axum::Json(body)).into_response();
        if let Some(after) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(after.as_secs()));
        }
        response
    }
}

/// `axum::Json`, but a bad body is answered in the `{error, code}` shape
/// instead of axum's plain-text rejection.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(json_rejection(rejection)),
        }
    }
}

fn json_rejection(rejection: JsonRejection) -> ApiError {
    let status = rejection.status();
    ApiError::new(status, "bad_request", rejection.body_text())
}

/// 404 for unknown `/api/*` paths, so they never fall through to the SPA.
pub async fn api_not_found() -> ApiError {
    ApiError::not_found("no such API endpoint")
}
