//! Feature route tests: services, logs, config, Wi-Fi, reboot, users, ONVIF
//! and video, through the real router (`tower::ServiceExt::oneshot`).
//!
//! Backends are faked in-process: a Unix-socket supervisor answering JSON
//! lines, a TCP snapshot server, temp directories for the files.

use crate::app::app;
use crate::state::{AppState, Config};
use crate::tests::support::{get_req, login, scratch, send, state, state_with, Reply};
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use supervisor::control::{
    ErrorCode, Request as Ctl, Response as CtlResponse, ServiceStatus, State,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tower::ServiceExt;

static REBOOTS: AtomicUsize = AtomicUsize::new(0);

/// The `Tuning::reboot` used by every test state.
pub(crate) fn record_reboot(_: &Config) {
    REBOOTS.fetch_add(1, Ordering::SeqCst);
}

fn json_req(method: Method, path: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn bare_req(method: Method, path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

async fn tokens(router: &Router) -> (String, String) {
    (
        login(router, "admin", "adminpw").await,
        login(router, "bob", "bobpw").await,
    )
}

async fn stream_token(router: &Router, jwt: &str) -> String {
    let reply = send(
        router,
        bare_req(Method::POST, "/api/logs/stream-token", jwt),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    reply.json()["token"].as_str().unwrap().to_owned()
}

// ------------------------------------------------------------ services ----

fn service(name: &str, critical: bool) -> ServiceStatus {
    ServiceStatus {
        name: name.into(),
        enabled: true,
        critical,
        state: State::Running,
        pid: Some(100),
        uptime_secs: Some(42),
        restarts: 2,
        last_exit_code: Some(143),
    }
}

/// A supervisor on `path` that knows `wdt_feeder` (critical) and `sshd`.
fn fake_supervisor(path: &Path, services: &'static [&'static str]) {
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: Ctl = serde_json::from_str(&line).unwrap();
                    let answer = match request {
                        Ctl::Status => CtlResponse::Services {
                            services: services
                                .iter()
                                .map(|n| service(n, *n == "wdt_feeder"))
                                .collect(),
                        },
                        Ctl::Stop { service: s }
                        | Ctl::Restart { service: s }
                        | Ctl::Start { service: s }
                            if !services.contains(&s.as_str()) =>
                        {
                            CtlResponse::Error {
                                code: ErrorCode::UnknownService,
                                message: format!("no service {s}"),
                            }
                        }
                        Ctl::Stop { service: s } | Ctl::Restart { service: s }
                            if s == "wdt_feeder" =>
                        {
                            CtlResponse::Error {
                                code: ErrorCode::Critical,
                                message: "wdt_feeder is critical".into(),
                            }
                        }
                        Ctl::Stop { service: s } => CtlResponse::Service {
                            service: ServiceStatus {
                                state: State::Stopped,
                                pid: None,
                                uptime_secs: None,
                                ..service(&s, false)
                            },
                        },
                        Ctl::Start { service: s } | Ctl::Restart { service: s } => {
                            CtlResponse::Service {
                                service: service(&s, false),
                            }
                        }
                    };
                    let mut text = serde_json::to_string(&answer).unwrap();
                    text.push('\n');
                    write.write_all(text.as_bytes()).await.unwrap();
                }
            });
        }
    });
}

#[tokio::test]
async fn services_list_and_control() {
    let dir = scratch("services");
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    std::fs::write(dir.join("logs/sshd.log"), "x\n").unwrap();
    let state = state(&dir, None);
    fake_supervisor(&state.config.supervisor_socket, &["wdt_feeder", "sshd"]);
    let router = app(state);
    let (admin, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/services", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let body = reply.json();
    assert_eq!(body[0]["name"], "wdt_feeder");
    assert_eq!(body[0]["critical"], true);
    assert_eq!(body[0]["log"], Value::Null);
    assert_eq!(
        body[1],
        json!({
            "name": "sshd", "state": "running", "pid": 100, "uptime_s": 42,
            "restarts": 2, "enabled": true, "critical": false, "log": "sshd.log",
            "last_exit": "signal 15 (SIGTERM)", "last_exit_code": 143
        })
    );

    let reply = send(
        &router,
        bare_req(Method::POST, "/api/services/sshd/stop", &admin),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["state"], "stopped");
    let reply = send(
        &router,
        bare_req(Method::POST, "/api/services/sshd/restart", &admin),
    )
    .await;
    assert_eq!(reply.json()["state"], "running");

    let reply = send(
        &router,
        bare_req(Method::POST, "/api/services/sshd/restart", &viewer),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);

    let reply = send(
        &router,
        bare_req(Method::POST, "/api/services/wdt_feeder/stop", &admin),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.code(), "critical");

    let reply = send(
        &router,
        bare_req(Method::POST, "/api/services/nope/start", &admin),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "unknown_service");

    let reply = send(
        &router,
        bare_req(Method::POST, "/api/services/sshd/explode", &admin),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    let reply = send(&router, get_req("/api/services", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn services_without_a_supervisor_are_503() {
    let dir = scratch("services-down");
    let router = app(state(&dir, None));
    let (admin, _) = tokens(&router).await;
    let reply = send(&router, get_req("/api/services", Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "supervisor_unavailable");
    let reply = send(
        &router,
        bare_req(Method::POST, "/api/services/sshd/restart", &admin),
    )
    .await;
    assert_eq!(reply.code(), "supervisor_unavailable");
}

// ---------------------------------------------------------------- logs ----

fn log_dir(dir: &Path) -> std::path::PathBuf {
    let logs = dir.join("logs");
    std::fs::create_dir_all(logs.join("subdir")).unwrap();
    std::fs::write(logs.join("sshd.log"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(logs.join("sshd.log.1"), "older\n").unwrap();
    std::os::unix::fs::symlink(dir.join("web-users"), logs.join("evil.log")).unwrap();
    logs
}

#[tokio::test]
async fn logs_list_and_tail() {
    let dir = scratch("logs");
    log_dir(&dir);
    let router = app(state(&dir, None));
    let (_, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/logs", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK);
    let body = reply.json();
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["sshd.log", "sshd.log.1"]);
    assert_eq!(body[0]["size"], 14);
    assert_eq!(body[0]["rotated"], false);
    assert_eq!(body[1]["rotated"], true);
    assert!(body[0]["modified"].as_u64().unwrap() > 0);

    let reply = send(
        &router,
        get_req("/api/logs/sshd.log?lines=2", Some(&viewer)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json(),
        json!({ "file": "sshd.log", "lines": ["two", "three"] })
    );
    let reply = send(
        &router,
        get_req("/api/logs/sshd.log?lines=999999", Some(&viewer)),
    )
    .await;
    assert_eq!(reply.json()["lines"].as_array().unwrap().len(), 3);
    let reply = send(&router, get_req("/api/logs/sshd.log", Some(&viewer))).await;
    assert_eq!(reply.json()["lines"].as_array().unwrap().len(), 3);

    for bad in [
        "..%2Fweb-users",
        "evil.log",
        "subdir",
        "nope.log",
        "%2Fetc%2Fpasswd",
        "..",
    ] {
        let reply = send(&router, get_req(&format!("/api/logs/{bad}"), Some(&viewer))).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{bad}");
        assert!(!reply.text().contains("pbkdf2"), "{bad}");
    }
    let reply = send(&router, get_req("/api/logs/sshd.log", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

/// Next body chunk as text, or `None` at the end of the stream.
async fn next_chunk(stream: &mut axum::body::BodyDataStream) -> Option<String> {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("no chunk within 5 s")
        .map(|chunk| String::from_utf8_lossy(&chunk.unwrap()).into_owned())
}

async fn open_stream(router: &Router, path: &str) -> axum::response::Response {
    router.clone().oneshot(get_req(path, None)).await.unwrap()
}

#[tokio::test]
async fn log_stream_follows_appends_rotation_and_shutdown() {
    let dir = scratch("log-stream");
    let logs = log_dir(&dir);
    let state = state(&dir, None);
    let router = app(state.clone());
    let (_, viewer) = tokens(&router).await;
    let token = stream_token(&router, &viewer).await;

    let response = open_stream(&router, &format!("/api/logs/sshd.log/stream?token={token}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let mut body = response.into_body().into_data_stream();
    assert_eq!(next_chunk(&mut body).await.unwrap(), ": open\n\n");

    let path = logs.join("sshd.log");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut file, b"four\nfive\n").unwrap();
    assert_eq!(
        next_chunk(&mut body).await.unwrap(),
        "data: four\ndata: five\n\n"
    );

    std::fs::rename(&path, logs.join("sshd.log.1")).unwrap();
    std::io::Write::write_all(&mut file, b"late\n").unwrap();
    std::fs::write(&path, "fresh\n").unwrap();
    let mut seen = String::new();
    while !seen.contains("fresh") {
        seen.push_str(&next_chunk(&mut body).await.unwrap());
    }
    assert_eq!(seen, "data: late\ndata: fresh\n\n");

    state.begin_shutdown();
    assert_eq!(next_chunk(&mut body).await, None);
}

#[tokio::test]
async fn at_most_four_log_streams() {
    let dir = scratch("log-streams-cap");
    log_dir(&dir);
    let router = app(state(&dir, None));
    let (_, viewer) = tokens(&router).await;
    let token = stream_token(&router, &viewer).await;
    let url = format!("/api/logs/sshd.log/stream?token={token}");

    let mut open = Vec::new();
    for _ in 0..4 {
        let response = open_stream(&router, &url).await;
        assert_eq!(response.status(), StatusCode::OK);
        open.push(response);
    }
    let reply = send(&router, get_req(&url, None)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "too_many_streams");
    // Unknown files are still 404, not 503.
    open.pop();
    let reply = send(
        &router,
        get_req(&format!("/api/logs/nope.log/stream?token={token}"), None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    // Closing one frees a slot.
    let response = open_stream(&router, &url).await;
    assert_eq!(response.status(), StatusCode::OK);
}

// -------------------------------------------------------------- config ----

const CONF: &str = "# Settings\nrun_rust=1\n\n# comment kept\nlog_level=info\ncustom=keep me\nwifi_ssid='Home'\nwifi_password='secret pass'\nssh_user=root\nssh_password=\n";

#[tokio::test]
async fn config_round_trip() {
    let dir = scratch("config");
    let conf = dir.join("camera.conf");
    std::fs::write(&conf, CONF).unwrap();
    let router = app(state(&dir, None));
    let (admin, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/config", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(!reply.text().contains("secret pass"));
    assert_eq!(
        reply.json(),
        json!({
            "values": { "run_rust": 1, "log_level": "info", "wifi_ssid": "Home", "ssh_user": "root" },
            "secrets_set": ["wifi_password"]
        })
    );

    let update = json!({ "values": { "log_level": "debug", "wifi_password": "new pass;1", "run_rust": true, "metrics": "textfile:/tmp/m" } });
    let reply = send(
        &router,
        json_req(Method::PUT, "/api/config", &viewer, update.clone()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = send(
        &router,
        json_req(Method::PUT, "/api/config", &admin, update),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.json(),
        json!({ "changed": ["log_level", "metrics", "wifi_password"], "restart": [], "reboot_required": true })
    );
    assert_eq!(
        std::fs::read_to_string(&conf).unwrap(),
        "# Settings\nrun_rust=1\n\n# comment kept\nlog_level=debug\ncustom=keep me\nwifi_ssid='Home'\nwifi_password='new pass;1'\nssh_user=root\nssh_password=\nmetrics=textfile:/tmp/m\n"
    );
    let mode = std::fs::metadata(&conf).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    // Nothing to change: no rewrite.
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/config",
            &admin,
            json!({ "values": { "log_level": "debug" } }),
        ),
    )
    .await;
    assert_eq!(reply.json()["changed"], json!([]));
}

#[tokio::test]
async fn config_rejects_bad_values_and_unknown_keys() {
    let dir = scratch("config-bad");
    std::fs::write(dir.join("camera.conf"), CONF).unwrap();
    let router = app(state(&dir, None));
    let (admin, _) = tokens(&router).await;
    for values in [
        json!({ "wifi_password": "short" }),
        json!({ "wifi_ssid": "x".repeat(33) }),
        json!({ "run_rust": 2 }),
        json!({ "log_level": "info; reboot" }),
        json!({ "metrics": "http:0.0.0.0:9100" }),
        json!({ "ssh_password": "it's" }),
        json!({ "ssh_password": "a\nreboot" }),
        json!({ "nonsense": "1" }),
        json!({ "onvif_password": "x" }),
        json!({ "web_admin_password": "x" }),
        json!({ "log_level": ["info"] }),
    ] {
        let reply = send(
            &router,
            json_req(
                Method::PUT,
                "/api/config",
                &admin,
                json!({ "values": values }),
            ),
        )
        .await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{values}: {}",
            reply.text()
        );
        assert_eq!(reply.code(), "bad_request");
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("camera.conf")).unwrap(),
        CONF
    );
}

// --------------------------------------------------------- wifi/reboot ----

#[tokio::test]
async fn wifi_status_and_reboot() {
    let dir = scratch("wifi-reboot");
    std::fs::write(
        dir.join("camera.conf"),
        "wifi_ssid=Target\nwifi_password=password1\n",
    )
    .unwrap();
    let router = app(state(&dir, None));
    let (admin, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/wifi", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK);
    let body = reply.json();
    assert_eq!(body["iface"], "anykatest0");
    assert_eq!(body["connected"], false);
    assert_eq!(body["target_ssid"], "Target");
    assert_eq!(body["state"], "reconnecting");
    assert!(!reply.text().contains("password1"));

    let reply = send(
        &router,
        bare_req(Method::POST, "/api/system/reboot", &viewer),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let before = REBOOTS.load(Ordering::SeqCst);
    let reply = send(
        &router,
        bare_req(Method::POST, "/api/system/reboot", &admin),
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    for _ in 0..50 {
        if REBOOTS.load(Ordering::SeqCst) > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(REBOOTS.load(Ordering::SeqCst) > before);
}

// --------------------------------------------------------------- users ----

#[tokio::test]
async fn users_crud_and_last_admin() {
    let dir = scratch("users");
    let router = app(state(&dir, None));
    let (admin, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/users", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = send(&router, get_req("/api/users", Some(&admin))).await;
    assert_eq!(
        reply.json(),
        json!([{ "name": "admin", "role": "admin" }, { "name": "bob", "role": "viewer" }])
    );

    let carol = json!({ "name": "carol", "password": "carolpw", "role": "viewer" });
    let reply = send(
        &router,
        json_req(Method::POST, "/api/users", &viewer, carol.clone()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = send(
        &router,
        json_req(Method::POST, "/api/users", &admin, carol.clone()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    assert_eq!(reply.json(), json!({ "name": "carol", "role": "viewer" }));
    let reply = send(&router, json_req(Method::POST, "/api/users", &admin, carol)).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.code(), "exists");
    let reply = send(
        &router,
        json_req(
            Method::POST,
            "/api/users",
            &admin,
            json!({ "name": "../x", "password": "p", "role": "viewer" }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let carol_token = login(&router, "carol", "carolpw").await;

    // The only admin can be neither deleted nor demoted.
    let reply = send(
        &router,
        bare_req(Method::DELETE, "/api/users/admin", &admin),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.code(), "last_admin");
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/users/admin",
            &admin,
            json!({ "role": "viewer" }),
        ),
    )
    .await;
    assert_eq!(reply.code(), "last_admin");

    // Promote carol: her session ends (generation bump), then she is an admin.
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/users/carol",
            &admin,
            json!({ "role": "admin" }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json(), json!({ "name": "carol", "role": "admin" }));
    let reply = send(&router, get_req("/api/me", Some(&carol_token))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    // Password reset by an admin.
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/users/bob",
            &admin,
            json!({ "password": "bobnew" }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    login(&router, "bob", "bobnew").await;
    let reply = send(
        &router,
        json_req(Method::PUT, "/api/users/bob", &admin, json!({})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/users/ghost",
            &admin,
            json!({ "role": "admin" }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // With two admins, one may go.
    let reply = send(
        &router,
        bare_req(Method::DELETE, "/api/users/admin", &admin),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let reply = send(&router, get_req("/api/me", Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let text = std::fs::read_to_string(dir.join("web-users")).unwrap();
    assert!(!text.contains("\nadmin:") && text.contains("carol:admin:"));
}

#[tokio::test]
async fn own_password_change_needs_the_current_one() {
    let dir = scratch("own-password");
    let router = app(state(&dir, None));
    let (_, viewer) = tokens(&router).await;

    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/users/me/password",
            &viewer,
            json!({ "current": "wrong", "new": "bobnew" }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "wrong_password");
    // The session survives a wrong attempt.
    assert_eq!(
        send(&router, get_req("/api/me", Some(&viewer)))
            .await
            .status,
        StatusCode::OK
    );

    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/users/me/password",
            &viewer,
            json!({ "current": "bobpw", "new": "bobnew" }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let fresh = reply.json()["token"].as_str().unwrap().to_owned();
    assert_eq!(reply.json()["role"], "viewer");
    assert_eq!(
        send(&router, get_req("/api/me", Some(&viewer)))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&router, get_req("/api/me", Some(&fresh))).await.status,
        StatusCode::OK
    );
    login(&router, "bob", "bobnew").await;
}

// --------------------------------------------------------------- onvif ----

#[tokio::test]
async fn onvif_credentials_are_written_0600_and_never_returned() {
    let dir = scratch("onvif");
    let state = state(&dir, None);
    fake_supervisor(&state.config.supervisor_socket, &["sshd"]);
    let router = app(state);
    let (admin, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/onvif", Some(&viewer))).await;
    assert_eq!(reply.json(), json!({ "user": null, "configured": false }));

    let body = json!({ "user": "nvr", "password": "s3cret:pw" });
    let reply = send(
        &router,
        json_req(Method::PUT, "/api/onvif/credentials", &viewer, body.clone()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = send(
        &router,
        json_req(Method::PUT, "/api/onvif/credentials", &admin, body),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    // No `onvif` service in this fake supervisor.
    assert_eq!(reply.json()["restarted"], false);
    assert!(reply.json()["note"]
        .as_str()
        .unwrap()
        .contains("no onvif service"));

    let path = dir.join("etc/onvif.secret");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "nvr:s3cret:pw\n");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let reply = send(&router, get_req("/api/onvif", Some(&viewer))).await;
    assert_eq!(reply.json(), json!({ "user": "nvr", "configured": true }));
    let reply = send(&router, get_req("/api/config", Some(&viewer))).await;
    assert_eq!(reply.json()["values"]["onvif_user"], "nvr");
    assert_eq!(reply.json()["secrets_set"], json!(["onvif_password"]));
    assert!(!reply.text().contains("s3cret"));

    for bad in [
        json!({ "user": "a:b", "password": "x" }),
        json!({ "user": "nvr", "password": "" }),
        json!({ "user": "nvr", "password": "a\nb" }),
    ] {
        let reply = send(
            &router,
            json_req(Method::PUT, "/api/onvif/credentials", &admin, bad),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn onvif_credentials_restart_the_service() {
    let dir = scratch("onvif-restart");
    let state = state(&dir, None);
    fake_supervisor(&state.config.supervisor_socket, &["onvif"]);
    let router = app(state);
    let (admin, _) = tokens(&router).await;
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/onvif/credentials",
            &admin,
            json!({ "user": "nvr", "password": "pw" }),
        ),
    )
    .await;
    assert_eq!(reply.json(), json!({ "restarted": true }));
}

// --------------------------------------------------------------- video ----

const JPEG: &[u8] = b"\xff\xd8fake-jpeg\xff\xd9";

/// A snapshot server answering every connection with `response`.
async fn snapshot_server(response: &'static [u8]) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(response).await;
            });
        }
    });
    format!("http://{address}/snapshot.jpg")
}

fn ok_response() -> &'static [u8] {
    let mut response = format!(
        "HTTP/1.0 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
        JPEG.len()
    )
    .into_bytes();
    response.extend_from_slice(JPEG);
    Box::leak(response.into_boxed_slice())
}

fn video_state(dir: &Path, url: String) -> AppState {
    state_with(dir, None, |config| config.snapshot_url = url)
}

#[tokio::test]
async fn snapshot_is_proxied() {
    let dir = scratch("snapshot");
    let router = app(video_state(&dir, snapshot_server(ok_response()).await));
    let (_, viewer) = tokens(&router).await;
    let token = stream_token(&router, &viewer).await;

    let reply = send(
        &router,
        get_req(&format!("/api/snapshot.jpg?token={token}&t=1"), None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(reply.body, JPEG);
    let reply = send(&router, get_req("/api/snapshot.jpg", Some(&viewer))).await;
    assert_eq!(reply.body, JPEG);
    let reply = send(&router, get_req("/api/snapshot.jpg", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

async fn assert_video_unavailable(reply: &Reply, reason: &str) {
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "video_unavailable");
    let message = reply.json()["error"].as_str().unwrap().to_owned();
    assert!(message.contains(reason), "{message}");
}

#[tokio::test]
async fn snapshot_failures_are_503_with_a_reason() {
    let dir = scratch("snapshot-down");
    // Nothing listens on a port that was just freed.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let router = app(video_state(
        &dir,
        format!("http://127.0.0.1:{port}/snapshot.jpg"),
    ));
    let (_, viewer) = tokens(&router).await;
    let reply = send(&router, get_req("/api/snapshot.jpg", Some(&viewer))).await;
    assert_video_unavailable(&reply, "unreachable").await;

    let dir = scratch("snapshot-500");
    let url = snapshot_server(b"HTTP/1.0 500 Internal Server Error\r\n\r\nno sensor").await;
    let router = app(video_state(&dir, url));
    let (_, viewer) = tokens(&router).await;
    let reply = send(&router, get_req("/api/snapshot.jpg", Some(&viewer))).await;
    assert_video_unavailable(&reply, "HTTP 500").await;
    let reply = send(&router, get_req("/api/live.mjpeg", Some(&viewer))).await;
    assert_video_unavailable(&reply, "HTTP 500").await;
}

#[tokio::test]
async fn mjpeg_serves_one_client_at_a_time() {
    let dir = scratch("mjpeg");
    let state = video_state(&dir, snapshot_server(ok_response()).await);
    let router = app(state.clone());
    let (_, viewer) = tokens(&router).await;
    let token = stream_token(&router, &viewer).await;
    let url = format!("/api/live.mjpeg?token={token}");

    let response = open_stream(&router, &url).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "multipart/x-mixed-replace; boundary=anykaframe"
    );
    let mut body = response.into_body().into_data_stream();
    for _ in 0..3 {
        let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(chunk.starts_with(b"--anykaframe\r\nContent-Type: image/jpeg\r\n"));
        assert!(chunk.ends_with(&[JPEG, b"\r\n"].concat()));
    }

    let reply = send(&router, get_req(&url, None)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "busy");

    // The client leaving frees the slot.
    drop(body);
    let response = open_stream(&router, &url).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    assert!(body.next().await.is_some());
    // Shutdown ends it.
    state.begin_shutdown();
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        while body.next().await.is_some() {}
    })
    .await;
    assert!(end.is_ok());
}

// ----------------------------------------------------------------- ptz ----

type Seen = std::sync::Arc<std::sync::Mutex<Vec<ptz_proto::Request>>>;

/// A PTZ owner on `path` recording every request.
fn fake_ptz(path: &Path) -> Seen {
    use ptz_proto::{ErrorCode as E, Request as P, Response as R};
    use ptz_proto::{Limits, Position, Preset};
    let seen: Seen = Seen::default();
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    let record = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let record = record.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: P = serde_json::from_str(&line).unwrap();
                    record.lock().unwrap().push(request.clone());
                    let door = Preset {
                        token: "1".into(),
                        name: "Door".into(),
                        pan_deg: 90.0,
                        tilt_deg: 30.0,
                    };
                    let answer = match request {
                        P::Status => R::Status {
                            backend: "sim".into(),
                            limits: Limits {
                                pan_min_deg: 0.0,
                                pan_max_deg: 350.0,
                                tilt_min_deg: 0.0,
                                tilt_max_deg: 120.0,
                            },
                            position: Position {
                                pan_deg: 175.0,
                                tilt_deg: 60.0,
                                moving: false,
                            },
                            last_manual_ms_ago: None,
                        },
                        P::Presets => R::Presets {
                            presets: vec![door],
                        },
                        P::SetPreset { name, .. } => R::Preset {
                            preset: Preset {
                                token: "2".into(),
                                name,
                                pan_deg: 175.0,
                                tilt_deg: 60.0,
                            },
                        },
                        P::GotoPreset { token, .. } if token != "1" => R::Error {
                            code: E::UnknownPreset,
                            message: format!("no preset with token {token:?}"),
                        },
                        _ => R::Ok,
                    };
                    let mut text = serde_json::to_string(&answer).unwrap();
                    text.push('\n');
                    write.write_all(text.as_bytes()).await.unwrap();
                }
            });
        }
    });
    seen
}

#[tokio::test]
async fn ptz_without_an_owner_is_503() {
    let dir = scratch("ptz-down");
    let router = app(state(&dir, None));
    let (admin, viewer) = tokens(&router).await;
    let reply = send(&router, get_req("/api/ptz", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "ptz_unavailable");
    let reply = send(&router, bare_req(Method::POST, "/api/ptz/stop", &admin)).await;
    assert_eq!(reply.code(), "ptz_unavailable");
}

#[tokio::test]
async fn ptz_routes_translate_to_the_socket() {
    use ptz_proto::Request as P;
    let dir = scratch("ptz");
    let state = state(&dir, None);
    let seen = fake_ptz(&state.config.ptz_socket);
    let router = app(state);
    let (admin, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/ptz", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.json(),
        json!({
            "available": true, "backend": "sim", "pan": 175.0, "tilt": 60.0, "moving": false,
            "limits": { "pan_min_deg": 0.0, "pan_max_deg": 350.0, "tilt_min_deg": 0.0, "tilt_max_deg": 120.0 },
            "presets": [{ "id": 1, "token": "1", "name": "Door", "pan": 90.0, "tilt": 30.0 }]
        })
    );

    let body = json!({ "pan": 1.0, "tilt": -1.0, "speed": 0.5 });
    let reply = send(
        &router,
        json_req(Method::POST, "/api/ptz/move", &viewer, body.clone()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    seen.lock().unwrap().clear();
    let reply = send(
        &router,
        json_req(Method::POST, "/api/ptz/move", &admin, body),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    for (path, body) in [
        ("/api/ptz/goto", json!({ "preset": 1 })),
        ("/api/ptz/goto", json!({ "pan": 90.0, "tilt": 30.0 })),
    ] {
        let reply = send(&router, json_req(Method::POST, path, &admin, body)).await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    }
    for path in ["/api/ptz/stop", "/api/ptz/home"] {
        let reply = send(&router, bare_req(Method::POST, path, &admin)).await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
    }
    let reply = send(
        &router,
        bare_req(Method::DELETE, "/api/ptz/presets/1", &admin),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(
        *seen.lock().unwrap(),
        [
            P::Move {
                pan: 0.5,
                tilt: -0.5,
                timeout_ms: Some(1000),
                source: None,
            },
            P::GotoPreset {
                token: "1".into(),
                source: None,
            },
            P::Goto {
                pan: 90.0,
                tilt: 30.0,
                source: None,
            },
            P::Stop { source: None },
            P::Home { source: None },
            P::RemovePreset { token: "1".into() },
        ]
    );

    let reply = send(
        &router,
        json_req(
            Method::POST,
            "/api/ptz/goto",
            &admin,
            json!({ "preset": 9 }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "unknown_preset");
    let reply = send(
        &router,
        bare_req(Method::DELETE, "/api/ptz/presets/abc", &admin),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = send(
        &router,
        json_req(Method::POST, "/api/ptz/goto", &admin, json!({ "x": 1 })),
    )
    .await;
    assert!(reply.status.is_client_error());

    let reply = send(
        &router,
        json_req(
            Method::POST,
            "/api/ptz/presets",
            &admin,
            json!({ "name": "Gate" }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.json(),
        json!({ "id": 2, "token": "2", "name": "Gate", "pan": 175.0, "tilt": 60.0 })
    );
    let reply = send(&router, get_req("/api/ptz/presets", Some(&viewer))).await;
    assert_eq!(reply.json()[0]["name"], "Door");
}

// -------------------------------------------------------------- lights ----

type LightsSeen = std::sync::Arc<std::sync::Mutex<Vec<lights_proto::Request>>>;

/// A lights owner on `path` keeping the state in memory, recording every
/// request, and refusing blink periods outside the protocol's range.
fn fake_lights(path: &Path) -> LightsSeen {
    use lights_proto::{Alarm, ErrorCode as E, Request as L, Response as R};
    let seen = LightsSeen::default();
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    let record = seen.clone();
    tokio::spawn(async move {
        let mut state = lights_proto::State::default();
        while let Ok((stream, _)) = listener.accept().await {
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: L = serde_json::from_str(&line).unwrap();
                record.lock().unwrap().push(request);
                let answer = match request {
                    L::SetAlarm {
                        alarm: Alarm::Blink { period_ms },
                    } if !(lights_proto::MIN_BLINK_PERIOD_MS
                        ..=lights_proto::MAX_BLINK_PERIOD_MS)
                        .contains(&period_ms) =>
                    {
                        R::Error {
                            code: E::InvalidArgument,
                            message: "period_ms must be within 200..=10000".into(),
                        }
                    }
                    other => {
                        match other {
                            L::Get => {}
                            L::SetMode { mode } => state.mode = mode,
                            L::SetWhite { on } => state.white = on,
                            L::SetAlarm { alarm } => state.alarm = alarm,
                            L::SetMute { muted } => state.muted = muted,
                        }
                        R::State { state }
                    }
                };
                let mut text = serde_json::to_string(&answer).unwrap();
                text.push('\n');
                write.write_all(text.as_bytes()).await.unwrap();
            }
        }
    });
    seen
}

#[tokio::test]
async fn lights_without_an_owner_is_503() {
    let dir = scratch("lights-down");
    let router = app(state(&dir, None));
    let (admin, viewer) = tokens(&router).await;
    let reply = send(&router, get_req("/api/lights", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "lights_unavailable");
    let reply = send(
        &router,
        json_req(Method::PUT, "/api/lights", &admin, json!({ "white": true })),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "lights_unavailable");
}

#[tokio::test]
async fn lights_routes_translate_to_the_socket() {
    use lights_proto::{Alarm, Mode, Request as L};
    let dir = scratch("lights");
    let state = state(&dir, None);
    let seen = fake_lights(&state.config.lights_socket);
    let router = app(state);
    let (admin, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/lights", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = send(&router, get_req("/api/lights", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.json(),
        json!({
            "mode": "day", "white": false, "alarm": { "state": "off" }, "muted": true,
            "supported": ["ir_cut", "ir_leds", "white", "alarm", "amp"]
        })
    );

    // Viewers read; only admins change anything.
    let change = json!({
        "mode": "night", "white": true,
        "alarm": { "state": "blink", "period_ms": 500 }, "muted": false
    });
    let reply = send(
        &router,
        json_req(Method::PUT, "/api/lights", &viewer, change.clone()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    seen.lock().unwrap().clear();
    let reply = send(
        &router,
        json_req(Method::PUT, "/api/lights", &admin, change),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.json(),
        json!({
            "mode": "night", "white": true,
            "alarm": { "state": "blink", "period_ms": 500 }, "muted": false,
            "supported": ["ir_cut", "ir_leds", "white", "alarm", "amp"]
        })
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [
            L::SetMode { mode: Mode::Night },
            L::SetWhite { on: true },
            L::SetAlarm {
                alarm: Alarm::Blink { period_ms: 500 }
            },
            L::SetMute { muted: false },
        ]
    );

    // One field at a time; blink without a period gets the default.
    seen.lock().unwrap().clear();
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/lights",
            &admin,
            json!({ "alarm": { "state": "blink" } }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["alarm"]["period_ms"], 1000);
    let reply = send(
        &router,
        json_req(Method::PUT, "/api/lights", &admin, json!({ "mode": "day" })),
    )
    .await;
    assert_eq!(reply.json()["mode"], "day");
    assert_eq!(reply.json()["white"], true);
    assert_eq!(
        *seen.lock().unwrap(),
        [
            L::SetAlarm {
                alarm: Alarm::Blink { period_ms: 1000 }
            },
            L::SetMode { mode: Mode::Day },
        ]
    );

    // The owner's refusal is a 400; malformed and empty bodies never reach it.
    seen.lock().unwrap().clear();
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/lights",
            &admin,
            json!({ "alarm": { "state": "blink", "period_ms": 5 } }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "bad_request");
    assert_eq!(seen.lock().unwrap().len(), 1);
    seen.lock().unwrap().clear();
    for body in [
        json!({}),
        json!({ "mode": "auto" }),
        json!({ "white": "yes" }),
        json!({ "alarm": { "state": "strobe" } }),
        json!({ "gpio": 98 }),
    ] {
        let reply = send(&router, json_req(Method::PUT, "/api/lights", &admin, body)).await;
        assert!(reply.status.is_client_error(), "{}", reply.text());
    }
    assert!(seen.lock().unwrap().is_empty());
}

// -------------------------------------------------------- setup + ssh ----

const TEST_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAFQSXCNBe1s8MCZkJsvOne+u5PdkPmIhGxUII9Rwk9S test@host";
const TEST_FP: &str = "SHA256:bohEl0PS8JNGiMPm7Sg0jWsqafu3AQGGhI9+adPWk8U";

#[tokio::test]
async fn setup_wizard_marker() {
    let dir = scratch("setup");
    let router = app(state(&dir, None));
    let (admin, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/setup", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["completed"], false);
    assert_eq!(reply.json()["ssh_password_set"], false);

    let reply = send(&router, bare_req(Method::POST, "/api/setup", &viewer)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = send(&router, bare_req(Method::POST, "/api/setup", &admin)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["completed"], true);
    assert!(dir.join("etc/setup-complete").is_file());

    let reply = send(&router, bare_req(Method::DELETE, "/api/setup", &admin)).await;
    assert_eq!(reply.json()["completed"], false);
}

#[tokio::test]
async fn ssh_password_and_keys() {
    let dir = scratch("ssh");
    std::fs::create_dir_all(dir.join("etc")).unwrap();
    let router = app(state(&dir, None));
    let (admin, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/ssh", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = send(&router, get_req("/api/ssh", Some(&admin))).await;
    assert_eq!(
        reply.json(),
        json!({ "user": "root", "password_set": false, "keys": [] })
    );

    let short = json!({ "password": "short" });
    let reply = send(
        &router,
        json_req(Method::PUT, "/api/ssh/password", &admin, short),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let good = json!({ "password": "correct horse" });
    let reply = send(
        &router,
        json_req(Method::PUT, "/api/ssh/password", &admin, good),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["password_set"], true);
    let passwd = dir.join("etc/anyka-sshd.passwd");
    let creds = ::auth::Credentials::load(&passwd).unwrap();
    assert!(creds.verify("root", "correct horse"));
    assert_eq!(
        std::fs::metadata(&passwd).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let bad = json!({ "key": "ssh-dss AAAA nope" });
    let reply = send(
        &router,
        json_req(Method::POST, "/api/ssh/keys", &admin, bad),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    for _ in 0..2 {
        let body = json!({ "key": TEST_KEY });
        let reply = send(
            &router,
            json_req(Method::POST, "/api/ssh/keys", &admin, body),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
        assert_eq!(
            reply.json()["keys"],
            json!([{ "algorithm": "ssh-ed25519", "comment": "test@host", "fingerprint": TEST_FP }])
        );
    }
    let file = std::fs::read_to_string(dir.join("etc/anyka-sshd.authorized_keys")).unwrap();
    assert!(file.contains(TEST_KEY));

    let path = format!("/api/ssh/keys?fingerprint={}", TEST_FP.replace('+', "%2B"));
    let reply = send(&router, bare_req(Method::DELETE, &path, &admin)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["keys"], json!([]));
    let reply = send(&router, bare_req(Method::DELETE, &path, &admin)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

/// A two-sensor board profile, as the supervisor would hand it over.
fn sample_board() -> board::Resolved {
    board::Resolved {
        name: "test-dual".into(),
        path: "boards/test-dual.toml".into(),
        reason: "BOARD=test-dual".into(),
        profile: board::Profile {
            name: "test-dual".into(),
            description: Some("Two-sensor test board".into()),
            match_rules: board::MatchRules::default(),
            platform: None,
            video: board::Video {
                backend: "test-pattern".into(),
                sensors: vec![0, 1],
                tuning_dir: None,
                stream: board::Stream {
                    width: 1280,
                    height: 720,
                    fps: 15,
                    kbps: 2048,
                    gop: 30,
                    profile: "main".into(),
                },
                overrides: Vec::new(),
                snapshot: board::Snapshot::default(),
            },
            lights: None,
            wifi: None,
            ptz: None,
            audio: None,
        },
    }
}

#[tokio::test]
async fn hardware_reports_the_board_profile() {
    let dir = scratch("hardware-board");
    let router = app(state_with(&dir, None, |c| c.board = Some(sample_board())));
    let (_, viewer) = tokens(&router).await;
    let reply = send(&router, get_req("/api/hardware", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.json()["board"],
        json!({
            "name": "test-dual",
            "reason": "BOARD=test-dual",
            "description": "Two-sensor test board",
        })
    );

    let dir = scratch("hardware-no-board");
    let router = app(state(&dir, None));
    let (_, viewer) = tokens(&router).await;
    let reply = send(&router, get_req("/api/hardware", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["board"], Value::Null);
}

fn cameras_req(token: &str, host: Option<&str>) -> Request<Body> {
    let mut builder =
        Request::get("/api/cameras").header(header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(host) = host {
        builder = builder.header(header::HOST, host);
    }
    builder.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn cameras_carry_rtsp_urls_and_expected_sensors() {
    const STATUS: &str = concat!(
        r#"[{"index":0,"state":"running","width":640,"height":360,"format":"NV12","#,
        r#""encoding":"JPEG snapshots (software)","frames":9,"fps":15.0,"#,
        r#""sensor_width":1920,"sensor_height":1080,"sensor_id":"0xcb3e","tuning":null,"#,
        r#""stream":{"codec":"H.264","profile":"Main","width":1280,"height":720,"#,
        r#""fps":15,"gop":30,"target_kbps":2048,"kbps":1987.5,"encoder":"x"},"error":null}]"#
    );
    let response = format!(
        "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{STATUS}",
        STATUS.len()
    );
    let url = snapshot_server(Box::leak(response.into_bytes().into_boxed_slice())).await;
    let dir = scratch("cameras-rtsp");
    let router = app(state_with(&dir, None, |c| {
        c.snapshot_url = url;
        c.board = Some(sample_board());
        c.rtsp_port = 8554;
    }));
    let (_, viewer) = tokens(&router).await;

    let reply = send(&router, cameras_req(&viewer, Some("192.168.1.20:8080"))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let cams = reply.json();
    let cams = cams.as_array().unwrap();
    assert_eq!(cams.len(), 2, "board expects two sensors: {cams:?}");
    assert_eq!(cams[0]["rtsp_path"], "vs0");
    assert_eq!(cams[0]["rtsp_port"], 8554);
    assert_eq!(cams[0]["rtsp_url"], "rtsp://192.168.1.20:8554/vs0");
    assert_eq!(cams[0]["live"]["state"], "running");
    assert_eq!(cams[0]["live"]["stream"]["kbps"], 1987.5);
    assert_eq!(cams[1]["live"]["state"], "missing");
    assert_eq!(cams[1]["rtsp_url"], "rtsp://192.168.1.20:8554/vs1");

    let reply = send(&router, cameras_req(&viewer, None)).await;
    assert_eq!(reply.json()[0]["rtsp_url"], Value::Null);
}

#[tokio::test]
async fn cameras_without_a_board_or_producer() {
    let dir = scratch("cameras-bare");
    let router = app(state(&dir, None));
    let (_, viewer) = tokens(&router).await;
    let reply = send(&router, cameras_req(&viewer, Some("cam"))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), json!([]), "nothing probed, nothing expected");
}

// -------------------------------------------------------------- follow ----

type FollowSeen = std::sync::Arc<std::sync::Mutex<Vec<follow_proto::Request>>>;

/// A follow service on `path` keeping the switch in memory and recording
/// every request. `fail_store` makes every `set` fail like a full flash.
fn fake_follow(path: &Path, fail_store: bool) -> FollowSeen {
    use follow_proto::{
        ErrorCode as E, FollowStatus, Request as F, Response as R, State as S, Target,
    };
    let seen = FollowSeen::default();
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    let record = seen.clone();
    tokio::spawn(async move {
        let mut enabled = true;
        while let Ok((stream, _)) = listener.accept().await {
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: F = serde_json::from_str(&line).unwrap();
                record.lock().unwrap().push(request);
                let answer = match request {
                    F::Status => R::Status(FollowStatus {
                        enabled,
                        state: if enabled { S::Tracking } else { S::Disabled },
                        target: enabled.then_some(Target {
                            x_permille: 640,
                            y_permille: 480,
                            cells: 6,
                        }),
                        moves: 12,
                    }),
                    F::Set { .. } if fail_store => R::Error {
                        code: E::Store,
                        message: "cannot write etc/follow.json".into(),
                    },
                    F::Set { enabled: on } => {
                        enabled = on;
                        R::Ok
                    }
                };
                let mut text = serde_json::to_string(&answer).unwrap();
                text.push('\n');
                write.write_all(text.as_bytes()).await.unwrap();
            }
        }
    });
    seen
}

#[tokio::test]
async fn follow_without_the_service_is_503() {
    let dir = scratch("follow-down");
    let router = app(state(&dir, None));
    let (admin, viewer) = tokens(&router).await;
    let reply = send(&router, get_req("/api/follow", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "follow_unavailable");
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/follow",
            &admin,
            json!({ "enabled": true }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "follow_unavailable");
}

#[tokio::test]
async fn follow_routes_translate_to_the_socket() {
    use follow_proto::Request as F;
    let dir = scratch("follow");
    let state = state(&dir, None);
    let seen = fake_follow(&state.config.follow_socket, false);
    let router = app(state);
    let (admin, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/follow", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = send(&router, get_req("/api/follow", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.json(),
        json!({
            "enabled": true, "state": "tracking",
            "target": { "x_permille": 640, "y_permille": 480, "cells": 6 },
            "moves": 12
        })
    );

    // Viewers read; only admins switch it.
    seen.lock().unwrap().clear();
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/follow",
            &viewer,
            json!({ "enabled": false }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(seen.lock().unwrap().is_empty());

    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/follow",
            &admin,
            json!({ "enabled": false }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.json(),
        json!({ "enabled": false, "state": "disabled", "target": null, "moves": 12 })
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [F::Set { enabled: false }, F::Status]
    );

    // Malformed bodies never reach the service.
    seen.lock().unwrap().clear();
    for body in [
        json!({}),
        json!({ "enabled": "yes" }),
        json!({ "enabled": true, "x": 1 }),
    ] {
        let reply = send(&router, json_req(Method::PUT, "/api/follow", &admin, body)).await;
        assert!(reply.status.is_client_error(), "{}", reply.text());
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn follow_store_failure_is_500() {
    let dir = scratch("follow-store");
    let state = state(&dir, None);
    let _seen = fake_follow(&state.config.follow_socket, true);
    let router = app(state);
    let (admin, _) = tokens(&router).await;
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/follow",
            &admin,
            json!({ "enabled": false }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.code(), "follow_store");
}

// --------------------------------------------------------------- audio ----

/// What the fake audio service saw: frames played, and whether a talker
/// holds the speaker.
#[derive(Default)]
struct AudioSeen {
    played: Vec<audio_proto::Frame>,
    talking: bool,
    sets: Vec<(String, u32)>,
}

type AudioShared = std::sync::Arc<std::sync::Mutex<AudioSeen>>;

/// The samples of capture frame `n` from [`fake_audio`].
fn capture_samples(n: u64) -> Vec<i16> {
    (0..audio_proto::FRAME_SAMPLES as i16)
        .map(|i| i.wrapping_mul(7).wrapping_add(n as i16))
        .collect()
}

/// An audio service on `path`: a mic (gain 0..=7) and a speaker, one talker
/// at a time, capture sending [`capture_samples`] every 20 ms.
fn fake_audio(path: &Path) -> AudioShared {
    use audio_proto::{
        Device, DeviceKind, DeviceState, ErrorCode as E, Frame, Request as A, Response as R,
        StreamFormat,
    };
    let seen = AudioShared::default();
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    let shared = seen.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let shared = shared.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut reader = BufReader::new(read);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let request: A = serde_json::from_str(&line).unwrap();
                let devices = |shared: &AudioShared| {
                    let gain = |id: &str, default| {
                        shared
                            .lock()
                            .unwrap()
                            .sets
                            .iter()
                            .rev()
                            .find(|(d, _)| d == id)
                            .map_or(default, |(_, g)| *g)
                    };
                    vec![
                        Device {
                            id: "mic".into(),
                            kind: DeviceKind::Input,
                            name: "Microphone".into(),
                            state: DeviceState::Active,
                            rate: 8000,
                            gain: Some(gain("mic", 5)),
                            gain_range: [0, 7],
                            error: None,
                        },
                        Device {
                            id: "speaker".into(),
                            kind: DeviceKind::Output,
                            name: "Speaker".into(),
                            state: DeviceState::Idle,
                            rate: 8000,
                            gain: Some(gain("speaker", 3)),
                            gain_range: [0, 7],
                            error: None,
                        },
                    ]
                };
                let answer = match &request {
                    A::Status => R::Status {
                        devices: devices(&shared),
                    },
                    A::Set { device, .. } if device != "mic" && device != "speaker" => R::Error {
                        code: E::UnknownDevice,
                        message: format!("no device {device}"),
                    },
                    A::Set { gain, .. } if *gain > 7 => R::Error {
                        code: E::OutOfRange,
                        message: format!("gain {gain} outside 0..=7"),
                    },
                    A::Set { device, gain } => {
                        shared.lock().unwrap().sets.push((device.clone(), *gain));
                        R::Ok
                    }
                    A::Capture => R::Streaming(StreamFormat::DEFAULT),
                    A::Play { client } => {
                        let mut seen = shared.lock().unwrap();
                        if seen.talking {
                            R::Error {
                                code: E::Busy,
                                message: format!("speaker held; {client} refused"),
                            }
                        } else {
                            seen.talking = true;
                            R::Streaming(StreamFormat::DEFAULT)
                        }
                    }
                };
                let streaming = matches!(answer, R::Streaming(_));
                let mut text = serde_json::to_string(&answer).unwrap();
                text.push('\n');
                write.write_all(text.as_bytes()).await.unwrap();
                match request {
                    A::Capture if streaming => {
                        for n in 0.. {
                            let frame = Frame {
                                ts_ms: n * 20,
                                samples: capture_samples(n),
                            };
                            if write.write_all(&frame.encode()).await.is_err() {
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    }
                    A::Play { .. } if streaming => {
                        loop {
                            let mut head = [0u8; video_proto::HEADER_LEN];
                            if reader.read_exact(&mut head).await.is_err() {
                                break;
                            }
                            let header = video_proto::PacketHeader::decode(&head);
                            let mut payload = vec![0u8; header.len as usize];
                            reader.read_exact(&mut payload).await.unwrap();
                            let frame = Frame::from_parts(&header, &payload).unwrap();
                            shared.lock().unwrap().played.push(frame);
                        }
                        shared.lock().unwrap().talking = false;
                    }
                    _ => {}
                }
            });
        }
    });
    seen
}

#[tokio::test]
async fn audio_without_a_service_is_503() {
    let dir = scratch("audio-down");
    let router = app(state(&dir, None));
    let (admin, viewer) = tokens(&router).await;
    let reply = send(&router, get_req("/api/audio/devices", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "audio_unavailable");
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/audio/devices/mic",
            &admin,
            json!({ "gain": 3 }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.code(), "audio_unavailable");
}

#[tokio::test]
async fn audio_devices_and_gain_translate_to_the_socket() {
    let dir = scratch("audio-devices");
    let state = state_with(&dir, None, |config| config.https_port = Some(8443));
    let seen = fake_audio(&state.config.audio_socket);
    let router = app(state);
    let (admin, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/audio/devices", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = send(&router, get_req("/api/audio/devices", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.json(),
        json!({
            "devices": [
                { "id": "mic", "kind": "input", "name": "Microphone", "state": "active",
                  "rate": 8000, "gain": 5, "gain_range": [0, 7], "error": null },
                { "id": "speaker", "kind": "output", "name": "Speaker", "state": "idle",
                  "rate": 8000, "gain": 3, "gain_range": [0, 7], "error": null },
            ],
            "talk": { "https_port": 8443 }
        })
    );

    let put = |token: &str, id: &str, body: Value| {
        json_req(
            Method::PUT,
            &format!("/api/audio/devices/{id}"),
            token,
            body,
        )
    };
    let reply = send(&router, put(&viewer, "speaker", json!({ "gain": 4 }))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = send(&router, put(&admin, "speaker", json!({ "gain": 4 }))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["id"], "speaker");
    assert_eq!(reply.json()["gain"], 4);
    assert_eq!(seen.lock().unwrap().sets, vec![("speaker".to_owned(), 4)]);

    let reply = send(&router, put(&admin, "mic", json!({ "gain": 99 }))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "out_of_range");
    let reply = send(&router, put(&admin, "horn", json!({ "gain": 1 }))).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "not_found");
    for body in [
        json!({}),
        json!({ "gain": -1 }),
        json!({ "gain": 1, "x": 2 }),
    ] {
        let reply = send(&router, put(&admin, "mic", body)).await;
        assert!(reply.status.is_client_error(), "{}", reply.text());
    }
    assert_eq!(seen.lock().unwrap().sets.len(), 1);
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The router served on a real socket (upgrades need one), plus an admin
/// and a viewer session.
async fn serve_audio(name: &str) -> (std::net::SocketAddr, Router, AudioShared, String, String) {
    let dir = scratch(name);
    let state = state(&dir, None);
    let seen = fake_audio(&state.config.audio_socket);
    let router = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = router
        .clone()
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    let (admin, viewer) = tokens(&router).await;
    (addr, router, seen, admin, viewer)
}

async fn ws_connect(
    addr: std::net::SocketAddr,
    path: &str,
    token: &str,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let url = format!("ws://{addr}{path}?token={token}");
    tokio_tungstenite::connect_async(url)
        .await
        .map(|(ws, _)| ws)
}

/// The close code the server ends `ws` with, skipping other messages.
async fn close_code(ws: &mut Ws) -> u16 {
    use tokio_tungstenite::tungstenite::Message as M;
    let wait = async {
        while let Some(message) = ws.next().await {
            if let M::Close(frame) = message.unwrap() {
                return u16::from(frame.expect("a close frame with a code").code);
            }
        }
        panic!("socket ended without a close frame");
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("no close frame")
}

fn pcm(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

#[tokio::test]
async fn talk_forwards_frames_to_the_speaker() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message as M;
    let (addr, router, seen, admin, viewer) = serve_audio("audio-talk").await;

    // Viewers may listen but not talk; refused before the upgrade.
    let token = stream_token(&router, &viewer).await;
    match ws_connect(addr, "/api/audio/talk", &token).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        other => panic!("viewer talk not refused: {:?}", other.map(|_| ())),
    }

    let token = stream_token(&router, &admin).await;
    let mut ws = ws_connect(addr, "/api/audio/talk", &token).await.unwrap();
    let first: Vec<i16> = (0..160).collect();
    let second: Vec<i16> = (0..320).map(|i| -i).collect();
    ws.send(M::binary(pcm(&first))).await.unwrap();
    ws.send(M::binary(vec![1, 2, 3])).await.unwrap(); // odd: dropped
    ws.send(M::binary(vec![0; 5000])).await.unwrap(); // too long: dropped
    ws.send(M::text("hello")).await.unwrap(); // not a command: dropped
    ws.send(M::binary(pcm(&second))).await.unwrap();
    ws.send(M::text("end")).await.unwrap();
    assert_eq!(close_code(&mut ws).await, 1000);

    // The speaker is released after the end, with both good frames played.
    tokio::time::timeout(Duration::from_secs(5), async {
        while seen.lock().unwrap().talking {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the talker never let go of the speaker");
    let played: Vec<Vec<i16>> = seen
        .lock()
        .unwrap()
        .played
        .iter()
        .map(|f| f.samples.clone())
        .collect();
    assert_eq!(played, vec![first, second]);
}

#[tokio::test]
async fn second_talker_is_closed_busy() {
    let (addr, router, seen, admin, _) = serve_audio("audio-busy").await;
    let token = stream_token(&router, &admin).await;
    let mut first = ws_connect(addr, "/api/audio/talk", &token).await.unwrap();
    let mut second = ws_connect(addr, "/api/audio/talk", &token).await.unwrap();
    assert_eq!(close_code(&mut second).await, crate::audio::CLOSE_BUSY);
    assert!(
        seen.lock().unwrap().talking,
        "the first talker keeps the speaker"
    );
    first.close(None).await.unwrap();
}

#[tokio::test]
async fn talk_and_listen_without_a_service_close_4503() {
    let dir = scratch("audio-ws-down");
    let state = state(&dir, None);
    let router = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = router
        .clone()
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    let (admin, _) = tokens(&router).await;
    for path in ["/api/audio/talk", "/api/audio/listen"] {
        let token = stream_token(&router, &admin).await;
        let mut ws = ws_connect(addr, path, &token).await.unwrap();
        assert_eq!(
            close_code(&mut ws).await,
            crate::audio::CLOSE_UNAVAILABLE,
            "{path}"
        );
    }
}

#[tokio::test]
async fn listen_streams_microphone_frames() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message as M;
    let (addr, router, _seen, _, viewer) = serve_audio("audio-listen").await;

    // No token, no socket.
    assert!(ws_connect(addr, "/api/audio/listen", "nope").await.is_err());

    let token = stream_token(&router, &viewer).await;
    let mut ws = ws_connect(addr, "/api/audio/listen", &token).await.unwrap();
    for n in 0..3 {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("no frame")
            .unwrap()
            .unwrap();
        let M::Binary(bytes) = message else {
            panic!("expected binary, got {message:?}");
        };
        assert_eq!(bytes.len(), audio_proto::FRAME_BYTES);
        assert_eq!(&bytes[..], &pcm(&capture_samples(n))[..]);
    }
    ws.send(M::text("end")).await.unwrap();
    assert_eq!(close_code(&mut ws).await, 1000);
}

// -------------------------------------------------------------- update ----

type UpdateSeen = std::sync::Arc<std::sync::Mutex<Vec<Value>>>;

/// The status the fake update service reports, in `phase`.
fn update_status(phase: update_proto::Phase) -> update_proto::Status {
    use update_proto::{Available, Channel, Settings, Slot, SlotInfo, Status};
    Status {
        version: "0.2.0".into(),
        slot: Slot::A,
        trial: false,
        committed: true,
        slots: [
            SlotInfo {
                slot: Slot::A,
                version: Some("0.2.0".into()),
                bootable: true,
                successful: true,
                tries_left: 0,
                dir_present: true,
            },
            SlotInfo {
                slot: Slot::B,
                version: None,
                bootable: false,
                successful: false,
                tries_left: 0,
                dir_present: false,
            },
        ],
        phase,
        progress: None,
        available: Some(Available {
            version: "0.3.0".into(),
            channel: Channel::Stable,
            seq: 1_760_700_000,
            notes_url: Some("https://example.invalid/notes".into()),
            size: Some(5_242_880),
            downloaded: true,
            downgrade: false,
        }),
        last_check: Some(1_792_000_000),
        last_error: None,
        settings: Settings::default(),
        trusted_key: true,
        health: None,
    }
}

/// [`update_status`] (idle) as the API must return it.
fn update_status_json() -> Value {
    json!({
        "version": "0.2.0", "slot": "a", "trial": false, "committed": true,
        "slots": [
            { "slot": "a", "version": "0.2.0", "bootable": true, "successful": true,
              "tries_left": 0, "dir_present": true },
            { "slot": "b", "version": null, "bootable": false, "successful": false,
              "tries_left": 0, "dir_present": false }
        ],
        "phase": "idle", "progress": null,
        "available": { "version": "0.3.0", "channel": "stable", "seq": 1_760_700_000u64,
                       "notes_url": "https://example.invalid/notes", "size": 5_242_880,
                       "downloaded": true, "downgrade": false },
        "last_check": 1_792_000_000u64, "last_error": null,
        "settings": { "channel": "stable", "auto_check": true, "auto_install": false,
                      "window": null, "repo": "dangerousplay/kamoder" },
        "trusted_key": true, "health": null
    })
}

/// The happy update service: status and check answer the idle status,
/// everything else `ok`.
fn update_ok(request: update_proto::Request) -> update_proto::Response {
    use update_proto::{Phase, Request as U, Response as R};
    match request {
        U::Status | U::Check => R::Status(Box::new(update_status(Phase::Idle))),
        _ => R::Ok,
    }
}

/// An update service on `path` answering with `answer` and recording every
/// request line as JSON.
fn fake_update(
    path: &Path,
    answer: impl Fn(update_proto::Request) -> update_proto::Response + Send + Sync + 'static,
) -> UpdateSeen {
    let seen = UpdateSeen::default();
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    let record = seen.clone();
    let answer = std::sync::Arc::new(answer);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                record
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&line).unwrap());
                let request = serde_json::from_str(&line).unwrap();
                let mut text = serde_json::to_string(&answer(request)).unwrap();
                text.push('\n');
                write.write_all(text.as_bytes()).await.unwrap();
            }
        }
    });
    seen
}

fn take(seen: &UpdateSeen) -> Vec<Value> {
    std::mem::take(&mut *seen.lock().unwrap())
}

/// An upload request whose body arrives in 10 KiB pieces, without a
/// `Content-Length` (like a chunked browser upload).
fn upload_req(token: &str, query: &str, size: usize) -> Request<Body> {
    let pieces: Vec<Result<axum::body::Bytes, std::io::Error>> = upload_bytes(size)
        .chunks(10 * 1024)
        .map(|piece| Ok(axum::body::Bytes::copy_from_slice(piece)))
        .collect();
    Request::builder()
        .method(Method::POST)
        .uri(format!("/api/update/upload{query}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from_stream(futures_util::stream::iter(pieces)))
        .unwrap()
}

fn upload_bytes(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

/// Files left in the data root's `update/`.
fn update_files(state: &AppState) -> Vec<String> {
    let dir = state.config.data_root.join("update");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<_> = entries
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn update_without_the_service_is_503() {
    let dir = scratch("update-down");
    let state = state(&dir, None);
    let router = app(state.clone());
    let (admin, viewer) = tokens(&router).await;
    for request in [
        get_req("/api/update", Some(&viewer)),
        bare_req(Method::POST, "/api/update/check", &admin),
        upload_req(&admin, "", 1000),
    ] {
        let reply = send(&router, request).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.code(), "update_unavailable");
    }
    assert!(update_files(&state).is_empty());
}

#[tokio::test]
async fn update_status_is_passed_through() {
    let dir = scratch("update-status");
    let state = state(&dir, None);
    let seen = fake_update(&state.config.update_socket, update_ok);
    let router = app(state);
    let (_, viewer) = tokens(&router).await;

    let reply = send(&router, get_req("/api/update", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = send(&router, get_req("/api/update", Some(&viewer))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json(), update_status_json());
    assert_eq!(take(&seen), [json!({ "op": "status" })]);
}

#[tokio::test]
async fn update_admin_routes_refuse_viewers() {
    let dir = scratch("update-viewer");
    let state = state(&dir, None);
    let seen = fake_update(&state.config.update_socket, update_ok);
    let router = app(state.clone());
    let (_, viewer) = tokens(&router).await;
    let requests = [
        bare_req(Method::POST, "/api/update/check", &viewer),
        bare_req(Method::POST, "/api/update/download", &viewer),
        json_req(Method::POST, "/api/update/install", &viewer, json!({})),
        upload_req(&viewer, "", 1000),
        json_req(Method::POST, "/api/update/rollback", &viewer, json!({})),
        bare_req(Method::POST, "/api/update/reboot", &viewer),
        json_req(
            Method::PUT,
            "/api/update/settings",
            &viewer,
            json!({ "channel": "beta" }),
        ),
    ];
    for request in requests {
        let path = request.uri().path().to_owned();
        let reply = send(&router, request).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{path}");
    }
    assert!(take(&seen).is_empty());
    assert!(update_files(&state).is_empty());
}

#[tokio::test]
async fn update_routes_forward_requests() {
    let dir = scratch("update-forward");
    let state = state(&dir, None);
    let seen = fake_update(&state.config.update_socket, update_ok);
    let router = app(state);
    let (admin, _) = tokens(&router).await;
    let ok = json!({ "ok": true });

    let reply = send(&router, bare_req(Method::POST, "/api/update/check", &admin)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json(), update_status_json());
    assert_eq!(take(&seen), [json!({ "op": "check" })]);

    let reply = send(
        &router,
        bare_req(Method::POST, "/api/update/download", &admin),
    )
    .await;
    assert_eq!(reply.json(), ok);
    assert_eq!(take(&seen), [json!({ "op": "download" })]);

    let reply = send(
        &router,
        json_req(
            Method::POST,
            "/api/update/install",
            &admin,
            json!({ "force": true, "reboot": false }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json(), ok);
    let reply = send(
        &router,
        json_req(Method::POST, "/api/update/install", &admin, json!({})),
    )
    .await;
    assert_eq!(reply.json(), ok);
    assert_eq!(
        take(&seen),
        [
            json!({ "op": "install", "source": { "kind": "downloaded" },
                    "force": true, "reboot": false }),
            json!({ "op": "install", "source": { "kind": "downloaded" },
                    "force": false, "reboot": true }),
        ]
    );

    let reply = send(
        &router,
        json_req(
            Method::POST,
            "/api/update/rollback",
            &admin,
            json!({ "reboot": false }),
        ),
    )
    .await;
    assert_eq!(reply.json(), ok);
    let reply = send(
        &router,
        bare_req(Method::POST, "/api/update/reboot", &admin),
    )
    .await;
    assert_eq!(reply.json(), ok);
    assert_eq!(
        take(&seen),
        [
            json!({ "op": "rollback", "reboot": false }),
            json!({ "op": "reboot" })
        ]
    );

    let settings = json!({
        "channel": "beta", "auto_check": true, "auto_install": true,
        "window": { "start_hour": 3, "end_hour": 5 }, "repo": "dangerousplay/kamoder"
    });
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/update/settings",
            &admin,
            settings.clone(),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json(), update_status_json());
    let mut set = settings;
    set["op"] = json!("set_settings");
    assert_eq!(take(&seen), [set, json!({ "op": "status" })]);

    // Malformed bodies never reach the service.
    for (path, body) in [
        ("/api/update/install", json!({ "force": "yes" })),
        ("/api/update/install", json!({ "x": 1 })),
        ("/api/update/rollback", json!({ "reboot": 1 })),
    ] {
        let reply = send(&router, json_req(Method::POST, path, &admin, body)).await;
        assert!(reply.status.is_client_error(), "{}", reply.text());
    }
    let reply = send(
        &router,
        json_req(
            Method::PUT,
            "/api/update/settings",
            &admin,
            json!({ "channel": "nightly" }),
        ),
    )
    .await;
    assert!(reply.status.is_client_error(), "{}", reply.text());
    assert!(take(&seen).is_empty());
}

#[tokio::test]
async fn update_refusals_map_to_http_statuses() {
    use update_proto::Response as R;
    let dir = scratch("update-refusals");
    for (code, status) in [
        ("busy", StatusCode::CONFLICT),
        ("trial", StatusCode::CONFLICT),
        ("not_downloaded", StatusCode::CONFLICT),
        ("replay", StatusCode::BAD_REQUEST),
        ("bad_signature", StatusCode::BAD_REQUEST),
        ("no_trusted_key", StatusCode::BAD_REQUEST),
        ("invalid_settings", StatusCode::BAD_REQUEST),
        ("network", StatusCode::INTERNAL_SERVER_ERROR),
        ("install_failed", StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let state = state_with(&dir, None, |c| {
            c.update_socket = dir.join(format!("{code}.sock"));
        });
        let _seen = fake_update(&state.config.update_socket, move |_| R::Error {
            code: code.into(),
            message: format!("refused: {code}"),
        });
        let router = app(state);
        let (admin, _) = tokens(&router).await;
        let reply = send(
            &router,
            json_req(Method::POST, "/api/update/install", &admin, json!({})),
        )
        .await;
        assert_eq!(reply.status, status, "{code}");
        assert_eq!(
            reply.json(),
            json!({ "error": format!("refused: {code}"), "code": code })
        );
    }
}

#[tokio::test]
async fn update_upload_streams_to_the_file_then_installs() {
    use update_proto::{Request as U, Response as R, Source};
    let dir = scratch("update-upload");
    let state = state(&dir, None);
    let root = state.config.data_root.clone();
    let received = std::sync::Arc::new(std::sync::Mutex::new(None));
    let got = received.clone();
    let seen = fake_update(&state.config.update_socket, move |request| {
        if let U::Install {
            source: Source::Upload { path },
            ..
        } = &request
        {
            let file = root.join(path);
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            *got.lock().unwrap() = Some((std::fs::read(&file).unwrap(), mode & 0o777));
            return R::Ok;
        }
        update_ok(request)
    });
    let router = app(state.clone());
    let (admin, _) = tokens(&router).await;

    // Twice the app-wide 64 KiB body limit, under the upload cap.
    let size = 128 * 1024 + 17;
    let reply = send(
        &router,
        upload_req(&admin, "?force=true&reboot=false", size),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json(), json!({ "ok": true }));
    assert_eq!(
        take(&seen),
        [
            json!({ "op": "status" }),
            json!({ "op": "install",
                    "source": { "kind": "upload", "path": "update/upload.kup" },
                    "force": true, "reboot": false }),
        ]
    );
    let (bytes, mode) = received.lock().unwrap().take().expect("no install");
    assert_eq!(bytes, upload_bytes(size));
    assert_eq!(mode, 0o600);
    // The updater removes it once it is done with it; the .part is gone.
    assert_eq!(update_files(&state), ["upload.kup"]);

    // Defaults: no force, reboot.
    let reply = send(&router, upload_req(&admin, "", 10)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        take(&seen)[1],
        json!({ "op": "install",
                "source": { "kind": "upload", "path": "update/upload.kup" },
                "force": false, "reboot": true })
    );
}

#[tokio::test]
async fn update_upload_over_the_cap_is_413_and_leaves_nothing() {
    let dir = scratch("update-upload-big");
    let state = state(&dir, None);
    let seen = fake_update(&state.config.update_socket, update_ok);
    let router = app(state.clone());
    let (admin, _) = tokens(&router).await;
    let limit = state.tuning.update_upload_limit as usize;

    // Streamed: refused once the count passes the cap.
    let reply = send(&router, upload_req(&admin, "", limit + 1)).await;
    assert_eq!(
        reply.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        reply.text()
    );
    assert_eq!(reply.code(), "too_large");
    assert_eq!(take(&seen), [json!({ "op": "status" })], "no install");
    assert!(
        update_files(&state).is_empty(),
        "{:?}",
        update_files(&state)
    );

    // Declared: refused before anything is read or asked.
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/update/upload")
        .header(header::AUTHORIZATION, format!("Bearer {admin}"))
        .header(header::CONTENT_LENGTH, limit + 1)
        .body(Body::from(vec![0u8; limit + 1]))
        .unwrap();
    let reply = send(&router, request).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(take(&seen).is_empty());
    assert!(update_files(&state).is_empty());

    // Exactly the cap is fine.
    let reply = send(&router, upload_req(&admin, "", limit)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
}

#[tokio::test]
async fn update_upload_refused_or_busy_leaves_nothing() {
    use update_proto::{Phase, Request as U, Response as R};
    let dir = scratch("update-upload-refused");
    let state = state(&dir, None);
    let seen = fake_update(&state.config.update_socket, |request| match request {
        U::Install { .. } => R::Error {
            code: "replay".into(),
            message: "package seq 5 is not newer than 7".into(),
        },
        other => update_ok(other),
    });
    let router = app(state.clone());
    let (admin, _) = tokens(&router).await;

    let reply = send(&router, upload_req(&admin, "", 1000)).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "replay");
    assert_eq!(take(&seen).len(), 2);
    assert!(
        update_files(&state).is_empty(),
        "{:?}",
        update_files(&state)
    );

    // An empty body is not a package.
    let reply = send(&router, upload_req(&admin, "", 0)).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "bad_request");
    assert!(update_files(&state).is_empty());

    // One upload at a time.
    {
        let _held = state.update_upload.try_acquire().unwrap();
        let reply = send(&router, upload_req(&admin, "", 1000)).await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(reply.code(), "busy");
    }
    take(&seen);

    // Not while the updater is installing: its package file stays.
    let busy_state = state_with(&dir, None, |c| {
        c.update_socket = dir.join("busy.sock");
    });
    let busy_seen = fake_update(&busy_state.config.update_socket, |_| {
        R::Status(Box::new(update_status(Phase::Installing)))
    });
    let router = app(busy_state.clone());
    let (admin, _) = tokens(&router).await;
    let reply = send(&router, upload_req(&admin, "", 1000)).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.code(), "busy");
    assert_eq!(take(&busy_seen), [json!({ "op": "status" })]);
    assert!(update_files(&busy_state).is_empty());
}
