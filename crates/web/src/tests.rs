//! Router tests: the 401/403/429 matrix, sessions, stream tokens and the SPA.
//!
//! Requests go through the real [`app`] with `tower::ServiceExt::oneshot`;
//! no socket is opened. Users are written with 10 PBKDF2 iterations so a
//! login costs microseconds instead of a second.

use crate::app::app;
use crate::auth::{AdminUser, AuthUser, StreamUser};
use crate::state::{AppState, Config, Tuning};
use ::auth::{Claims, Jwt, Role, WebUsers};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tower::ServiceExt;

const KEY: [u8; 32] = [7; 32];

/// A fresh directory per test; tests run in parallel.
pub(crate) fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("anyka-web-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A users-file line with a cheap hash.
fn user_line(user: &str, role: Role, password: &str) -> String {
    let line = ::auth::hash_password(user, password, 10).unwrap();
    let (name, hash) = line.split_once(':').unwrap();
    format!("{name}:{role}:{hash}:1\n")
}

/// State with users `admin`/`adminpw` and `bob`/`bobpw` (viewer).
pub(crate) fn state(dir: &Path, www: Option<&Path>) -> AppState {
    state_with(dir, www, |_| {})
}

/// [`state`] with the configuration adjusted by `adjust`.
pub(crate) fn state_with(
    dir: &Path,
    www: Option<&Path>,
    adjust: impl FnOnce(&mut Config),
) -> AppState {
    let users = dir.join("web-users");
    let text =
        user_line("admin", Role::Admin, "adminpw") + &user_line("bob", Role::Viewer, "bobpw");
    ::auth::atomic::write(&users, text.as_bytes(), 0o600).unwrap();
    let config = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        www: www.map_or_else(|| dir.join("no-www"), Path::to_owned),
        users: users.clone(),
        jwt_key: dir.join("web-jwt.key"),
        token_ttl: Duration::from_secs(3600),
        logs: dir.join("logs"),
        camera_conf: dir.join("camera.conf"),
        supervisor_socket: dir.join("sup.sock"),
        ptz_socket: dir.join("ptz.sock"),
        lights_socket: dir.join("lights.sock"),
        rtsp_port: video_proto::DEFAULT_RTSP_PORT,
        board: None,
        snapshot_url: "http://127.0.0.1:9/snapshot.jpg".into(),
        mjpeg_interval: Duration::from_millis(20),
        onvif_secret: dir.join("etc/onvif.secret"),
        init_script: dir.join("init.sh"),
        sys_root: dir.join("sys-root"),
        ssh_passwd: dir.join("etc/anyka-sshd.passwd"),
        ssh_authorized_keys: dir.join("etc/anyka-sshd.authorized_keys"),
        setup_marker: dir.join("etc/setup-complete"),
        wifi: wifi::system::Paths {
            wpa_conf: dir.join("wpa.conf"),
            ctrl_dir: dir.join("ctrl"),
            iface: "anykatest0".into(),
            dhcp_script: dir.join("dhcp"),
            state_dir: dir.join("backup"),
        },
    };
    let tuning = Tuning {
        users_refresh: Duration::ZERO,
        login_failure_delay: Duration::ZERO,
        log_poll: Duration::from_millis(20),
        keepalive: Duration::from_secs(15),
        snapshot_timeout: Duration::from_secs(2),
        reboot_delay: Duration::ZERO,
        reboot: crate::api_tests::record_reboot,
    };
    let mut config = config;
    adjust(&mut config);
    AppState::new(
        config,
        tuning,
        Jwt::new(&KEY),
        WebUsers::load(users).unwrap(),
    )
}

fn from(ip: [u8; 4]) -> ConnectInfo<SocketAddr> {
    ConnectInfo(SocketAddr::from((ip, 40000)))
}

pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) headers: axum::http::HeaderMap,
    pub(crate) body: Vec<u8>,
}

impl Reply {
    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&self.body)))
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Asserts the `{error, code}` shape and returns the code.
    pub(crate) fn code(&self) -> String {
        let body = self.json();
        assert!(body["error"].is_string(), "no error message: {body}");
        body["code"].as_str().unwrap().to_owned()
    }
}

pub(crate) async fn send(router: &Router, request: Request<Body>) -> Reply {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

pub(crate) fn get_req(path: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::get(path);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

fn login_req(user: &str, password: &str, ip: [u8; 4]) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .extension(from(ip))
        .body(Body::from(
            json!({ "user": user, "password": password }).to_string(),
        ))
        .unwrap()
}

pub(crate) async fn login(router: &Router, user: &str, password: &str) -> String {
    let reply = send(router, login_req(user, password, [10, 0, 0, 1])).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    reply.json()["token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn health_needs_no_auth() {
    let dir = scratch("health");
    let router = app(state(&dir, None));
    let reply = send(&router, get_req("/api/health", None)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["ok"], true);
    assert_eq!(reply.json()["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn login_returns_a_token_that_me_accepts() {
    let dir = scratch("login-ok");
    let router = app(state(&dir, None));
    let reply = send(&router, login_req("bob", "bobpw", [10, 0, 0, 1])).await;
    assert_eq!(reply.status, StatusCode::OK);
    let body = reply.json();
    assert_eq!(body["role"], "viewer");
    let exp = body["exp"].as_u64().unwrap();
    assert!(exp > crate::state::now_secs() + 3000);

    let token = body["token"].as_str().unwrap();
    let me = send(&router, get_req("/api/me", Some(token))).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(
        me.json(),
        json!({ "user": "bob", "role": "viewer", "exp": exp })
    );
}

#[tokio::test]
async fn bad_password_and_unknown_user_are_401() {
    let dir = scratch("login-bad");
    let router = app(state(&dir, None));
    for (user, password) in [("bob", "wrong"), ("nobody", "bobpw")] {
        let reply = send(&router, login_req(user, password, [10, 0, 0, 2])).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            reply.json(),
            json!({ "error": "invalid credentials", "code": "auth" })
        );
    }
}

#[tokio::test]
async fn sixth_attempt_after_five_failures_is_429() {
    let dir = scratch("login-429");
    let router = app(state(&dir, None));
    for _ in 0..5 {
        let reply = send(&router, login_req("bob", "wrong", [10, 0, 0, 3])).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    // Blocked even with the right password.
    let reply = send(&router, login_req("bob", "bobpw", [10, 0, 0, 3])).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.code(), "rate_limited");
    assert!(reply.headers.contains_key(header::RETRY_AFTER));
    // Other clients are unaffected.
    let reply = send(&router, login_req("bob", "bobpw", [10, 0, 0, 4])).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn malformed_login_body_uses_the_error_shape() {
    let dir = scratch("login-body");
    let router = app(state(&dir, None));
    let request = Request::post("/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{\"user\":1}"))
        .unwrap();
    let reply = send(&router, request).await;
    assert!(reply.status.is_client_error());
    assert_eq!(reply.code(), "bad_request");
}

#[tokio::test]
async fn missing_malformed_tampered_foreign_and_expired_tokens_are_401() {
    let dir = scratch("tokens");
    let router = app(state(&dir, None));
    let token = login(&router, "admin", "adminpw").await;

    let reply = send(&router, get_req("/api/me", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.code(), "auth");

    let basic = Request::get("/api/me")
        .header(header::AUTHORIZATION, "Basic YWRtaW46YWRtaW5wdw==")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&router, basic).await.status, StatusCode::UNAUTHORIZED);

    // Flip one character of the payload.
    let mut tampered: Vec<u8> = token.clone().into_bytes();
    let dot = tampered.iter().position(|&b| b == b'.').unwrap();
    tampered[dot + 3] = if tampered[dot + 3] == b'A' {
        b'B'
    } else {
        b'A'
    };
    let tampered = String::from_utf8(tampered).unwrap();
    let reply = send(&router, get_req("/api/me", Some(&tampered))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    let now = crate::state::now_secs();
    let claims = |exp| Claims {
        sub: "admin".into(),
        role: Role::Admin,
        gen: 1,
        iat: now - 10,
        exp,
    };
    let foreign = Jwt::new(&[9; 32]).sign(&claims(now + 100));
    let reply = send(&router, get_req("/api/me", Some(&foreign))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    let expired = Jwt::new(&KEY).sign(&claims(now - 1));
    let reply = send(&router, get_req("/api/me", Some(&expired))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    // The genuine one still works.
    let reply = send(&router, get_req("/api/me", Some(&token))).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn deleting_a_user_ends_their_session() {
    let dir = scratch("deleted");
    let state = state(&dir, None);
    let router = app(state.clone());
    let token = login(&router, "bob", "bobpw").await;
    state.users().delete("bob").unwrap();
    let reply = send(&router, get_req("/api/me", Some(&token))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn role_change_ends_the_session() {
    let dir = scratch("role-change");
    let state = state(&dir, None);
    let router = app(state.clone());
    let token = login(&router, "bob", "bobpw").await;
    state.users().set_role("bob", Role::Admin).unwrap();
    let reply = send(&router, get_req("/api/me", Some(&token))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn external_edit_of_the_users_file_is_seen() {
    let dir = scratch("external");
    let state = state(&dir, None);
    let router = app(state.clone());
    let token = login(&router, "bob", "bobpw").await;
    let text = user_line("admin", Role::Admin, "adminpw");
    ::auth::atomic::write(&dir.join("web-users"), text.as_bytes(), 0o600).unwrap();
    let reply = send(&router, get_req("/api/me", Some(&token))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

/// A router with one route per extractor, for checking them in isolation.
fn extractor_router(state: AppState) -> Router {
    Router::new()
        .route("/any", get(|user: AuthUser| async move { user.user }))
        .route(
            "/admin",
            get(|AdminUser(user): AdminUser| async move { user.user }),
        )
        .route(
            "/stream",
            get(|StreamUser(user): StreamUser| async move { user.user }),
        )
        .with_state(state)
}

#[tokio::test]
async fn viewer_gets_403_on_admin_routes() {
    let dir = scratch("admin-only");
    let state = state(&dir, None);
    let router = app(state.clone());
    let viewer = login(&router, "bob", "bobpw").await;
    let admin = login(&router, "admin", "adminpw").await;
    let routes = extractor_router(state);

    let reply = send(&routes, get_req("/admin", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "forbidden");
    let reply = send(&routes, get_req("/admin", Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), "admin");
    let reply = send(&routes, get_req("/any", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = send(&routes, get_req("/admin", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn stream_tokens_work_in_the_query_but_jwts_do_not() {
    let dir = scratch("stream-token");
    let state = state(&dir, None);
    let router = app(state.clone());
    let jwt = login(&router, "bob", "bobpw").await;
    let routes = extractor_router(state.clone());

    let issue = Request::post("/api/logs/stream-token")
        .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
        .body(Body::empty())
        .unwrap();
    let reply = send(&router, issue).await;
    assert_eq!(reply.status, StatusCode::OK);
    let stream = reply.json()["token"].as_str().unwrap().to_owned();
    assert!(reply.json()["exp"].as_u64().unwrap() <= crate::state::now_secs() + 60);

    let reply = send(
        &routes,
        get_req(&format!("/stream?token={stream}&t=1"), None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), "bob");

    // A session JWT is not a stream token.
    let reply = send(&routes, get_req(&format!("/stream?token={jwt}"), None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    // Stream tokens are query-only: not accepted as a bearer on normal routes.
    let reply = send(&routes, get_req("/any", Some(&stream))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    // Normal routes ignore ?token=.
    let reply = send(&routes, get_req(&format!("/any?token={stream}"), None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    // The bearer header still works on stream routes.
    let reply = send(&routes, get_req("/stream", Some(&jwt))).await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = send(&routes, get_req("/stream", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    // Ends with the user's session.
    state.users().set_role("bob", Role::Admin).unwrap();
    let reply = send(&routes, get_req(&format!("/stream?token={stream}"), None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn spa_serves_files_precompressed_and_falls_back_to_index() {
    let dir = scratch("spa");
    let www = dir.join("www");
    std::fs::create_dir_all(www.join("assets")).unwrap();
    std::fs::write(www.join("index.html"), "<!doctype html>app").unwrap();
    std::fs::write(www.join("assets/app.js"), "console.log(1)").unwrap();
    std::fs::write(www.join("assets/app.js.gz"), "gzipped-bytes").unwrap();
    let router = app(state(&dir, Some(&www)));

    let reply = send(&router, get_req("/", None)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), "<!doctype html>app");

    // Client-side route.
    let reply = send(&router, get_req("/logs/sshd", None)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), "<!doctype html>app");

    let plain = send(&router, get_req("/assets/app.js", None)).await;
    assert_eq!(plain.text(), "console.log(1)");
    let request = Request::get("/assets/app.js")
        .header(header::ACCEPT_ENCODING, "gzip")
        .body(Body::empty())
        .unwrap();
    let gzipped = send(&router, request).await;
    assert_eq!(gzipped.headers[header::CONTENT_ENCODING], "gzip");
    assert_eq!(gzipped.text(), "gzipped-bytes");

    // Unknown API paths are JSON 404s, not the SPA.
    let reply = send(&router, get_req("/api/nope", None)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "not_found");
}

#[tokio::test]
async fn missing_spa_gives_a_hint_and_the_api_still_works() {
    let dir = scratch("no-spa");
    let router = app(state(&dir, None));
    let reply = send(&router, get_req("/", None)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(reply.headers[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/plain"));
    assert!(reply.text().contains("./x webui && ./x flash"));
    let reply = send(&router, get_req("/api/health", None)).await;
    assert_eq!(reply.status, StatusCode::OK);
}

/// Test helpers shared with `api_tests`.
pub(crate) mod support {
    pub(crate) use super::{get_req, login, scratch, send, state, state_with, Reply};
}

#[test]
fn load_creates_the_session_key() {
    let dir = scratch("load");
    let key = dir.join("etc/web-jwt.key");
    std::fs::create_dir_all(dir.join("etc")).unwrap();
    let mut config = state(&dir, None).config.clone();
    config.jwt_key = key.clone();
    AppState::load(config, Tuning::default()).unwrap();
    assert_eq!(std::fs::read(&key).unwrap().len(), 32);
}
