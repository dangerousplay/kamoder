//! The router: `/api/*` JSON routes, the SPA for everything else.
//!
//! Each feature lives in its own module exposing `fn routes() ->
//! Router<AppState>` with paths relative to `/api`; [`api`] merges them.
//! Authorization is per handler, by extractor type (see [`crate::auth`]), so
//! a route's required role is visible in its signature.

use crate::error::api_not_found;
use crate::state::AppState;
use crate::{
    cameras, config, hardware, lights, logs, onvif, ptz, services, session, setup, spa, ssh, stats,
    system, users, video,
};
use axum::extract::{DefaultBodyLimit, MatchedPath, Request};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use metrics::{counter, describe_counter};
use serde_json::{json, Value};

/// Largest request body accepted. The API only takes small JSON documents;
/// axum's 2 MB default is a lot of RAM to let a client fill.
const BODY_LIMIT: usize = 64 * 1024;

/// The whole application.
pub fn app(state: AppState) -> Router {
    let www = state.config.www.clone();
    Router::new()
        .nest("/api", api())
        .fallback_service(spa::service(&www))
        .layer(middleware::from_fn(count_requests))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(state)
}

/// Every `/api` route. Later features add a `.merge(module::routes())` here.
fn api() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .merge(session::routes())
        .merge(services::routes())
        .merge(logs::routes())
        .merge(config::routes())
        .merge(system::routes())
        .merge(hardware::routes())
        .merge(cameras::routes())
        .merge(stats::routes())
        .merge(setup::routes())
        .merge(ssh::routes())
        .merge(users::routes())
        .merge(onvif::routes())
        .merge(video::routes())
        .merge(ptz::routes())
        .merge(lights::routes())
        .fallback(api_not_found)
}

/// `GET /api/health` — unauthenticated readiness probe.
async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

/// Registers metric descriptions.
pub fn describe_metrics() {
    describe_counter!(
        "kamoder_web_requests_total",
        "HTTP requests by matched route and status"
    );
    describe_counter!(
        "kamoder_web_login_failures_total",
        "Failed login attempts (bad user or password)"
    );
}

/// Counts requests by route template (not raw path, so the label set stays
/// bounded) and status.
async fn count_requests(request: Request, next: Next) -> Response {
    let route = match request.extensions().get::<MatchedPath>() {
        Some(path) => path.as_str().to_owned(),
        None if request.uri().path().starts_with("/api/") => "/api/unknown".to_owned(),
        None => "static".to_owned(),
    };
    let response = next.run(request).await;
    counter!(
        "kamoder_web_requests_total",
        "route" => route,
        "status" => response.status().as_str().to_owned()
    )
    .increment(1);
    response
}
