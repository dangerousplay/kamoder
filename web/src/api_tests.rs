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
                        P::GotoPreset { token } if token != "1" => R::Error {
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
                timeout_ms: Some(1000)
            },
            P::GotoPreset { token: "1".into() },
            P::Goto {
                pan: 90.0,
                tilt: 30.0
            },
            P::Stop,
            P::Home,
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
        json!({ "mode": "day", "white": false, "alarm": { "state": "off" }, "muted": true })
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
            "alarm": { "state": "blink", "period_ms": 500 }, "muted": false
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
