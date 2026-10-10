//! Serving the single-page app from the SD card.
//!
//! The SPA is read from `www/` on each request rather than embedded in the
//! binary: embedding would put ~hundreds of KB into every process image for
//! no benefit, and `./x flash` can update the UI without a rebuild.
//! `./x webui` stores `.gz` and `.br` next to every asset, and `ServeDir`
//! picks the one the browser accepts — the 400 MHz CPU never compresses.
//!
//! Any non-API path that is not a file gets `index.html` so client-side
//! routes (`/logs`, `/settings`) survive a reload. If the UI is not installed
//! at all, the API still works and every page answers with a plain-text hint.

use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get_service, MethodRouter};
use std::path::Path;
use tower_http::services::{ServeDir, ServeFile};

/// Shown instead of the UI when `www/index.html` is missing.
pub const NOT_INSTALLED: &str = "web: web UI not installed; run ./x webui && ./x flash\n\
                                 The JSON API under /api/ is available.\n";

/// The fallback service for everything outside `/api`.
pub fn service(www: &Path) -> MethodRouter {
    let index = www.join("index.html");
    if !index.is_file() {
        tracing::warn!(path = %index.display(), "web UI not installed");
        return axum::routing::any(not_installed);
    }
    let serve = ServeDir::new(www)
        .precompressed_gzip()
        .precompressed_br()
        .fallback(
            ServeFile::new(index)
                .precompressed_gzip()
                .precompressed_br(),
        );
    get_service(serve)
}

async fn not_installed() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        NOT_INSTALLED,
    )
}
