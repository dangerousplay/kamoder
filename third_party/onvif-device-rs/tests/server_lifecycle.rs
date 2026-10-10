//! Server lifecycle integration tests: fail-closed auth, graceful shutdown,
//! listener injection, and request-size limits (v0.3.0 regressions).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use onvif_device_rs::server::{OnvifActionHandler, OnvifConfig, OnvifServer};
use onvif_device_rs::types::{OnvifError, RequestInfo};

struct OkHandler;
#[async_trait]
impl OnvifActionHandler for OkHandler {
    async fn handle(&self, _body: &str, _info: &RequestInfo) -> Result<String, OnvifError> {
        Ok("<OkResponse/>".to_string())
    }
}

fn config(password: &str) -> OnvifConfig {
    OnvifConfig {
        port: 0,
        username: "admin".to_string(),
        password: password.to_string(),
        ..Default::default()
    }
}

async fn free_listener() -> tokio::net::TcpListener {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind")
}

/// Regression: an empty password with the default `allow_no_auth = false`
/// must FAIL to start (previously it silently disabled authentication for
/// every action).
#[tokio::test]
async fn empty_password_fails_closed() {
    let mut server = OnvifServer::new(&config(""));
    server.register_handler("GetProfiles", Box::new(OkHandler));
    let listener = free_listener().await;
    let err = server
        .start_on(listener)
        .await
        .expect_err("empty password must fail closed");
    assert!(err.to_string().contains("allow_no_auth"), "got: {err}");
}

/// The explicit `allow_no_auth = true` opt-in still works (documented open
/// server).
#[tokio::test]
async fn allow_no_auth_opt_in_works() {
    let cfg = OnvifConfig {
        allow_no_auth: true,
        ..config("")
    };
    let mut server = OnvifServer::new(&cfg);
    server.register_handler("GetProfiles", Box::new(OkHandler));
    let listener = free_listener().await;
    let mut handle = server
        .start_on(listener)
        .await
        .expect("starts when opted in");
    handle.shutdown().await.expect("clean shutdown");
}

/// Regression: graceful shutdown stops the accept loop — the port is
/// released and the handle's task finishes.
#[tokio::test]
async fn shutdown_stops_accept_loop_and_releases_port() {
    let mut server = OnvifServer::new(&config("secret"));
    server.register_handler("GetProfiles", Box::new(OkHandler));
    let listener = free_listener().await;
    let port = listener.local_addr().expect("addr").port();
    let mut handle = server.start_on(listener).await.expect("start");

    tokio::time::timeout(Duration::from_secs(3), handle.shutdown())
        .await
        .expect("shutdown within 3s")
        .expect("join ok");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "port must be released after shutdown"
    );
}

/// End-to-end over a real socket: authenticated action rejects bad
/// credentials with 401 and serves good ones with 200.
#[tokio::test]
async fn auth_enforced_over_the_wire() -> anyhow::Result<()> {
    let mut server = OnvifServer::new(&config("secret"));
    server.register_handler("GetProfiles", Box::new(OkHandler));
    let listener = free_listener().await;
    let port = listener.local_addr()?.port();
    let mut handle = server.start_on(listener).await?;

    let envelope = |user: &str, pass: &str| {
        format!(
            "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\"><Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\"><UsernameToken><Username>{user}</Username><Password>{pass}</Password></UsernameToken></Security></Header><Body><GetProfiles/></Body></Envelope>"
        )
    };

    let post = |body: String| async move {
        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        let req = format!(
            "POST /onvif/device_service HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        sock.write_all(req.as_bytes()).await?;
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await?;
        let head = String::from_utf8_lossy(&buf);
        Ok::<_, std::io::Error>(
            head.lines()
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string(),
        )
    };

    assert_eq!(post(envelope("admin", "wrong")).await?, "401");
    assert_eq!(post(envelope("admin", "secret")).await?, "200");
    handle.shutdown().await?;
    Ok(())
}

/// Regression: a body larger than `max_body_bytes` is rejected with 413
/// instead of being buffered without bound.
#[tokio::test]
async fn oversized_body_rejected() -> anyhow::Result<()> {
    let cfg = OnvifConfig {
        max_body_bytes: 1024,
        ..config("secret")
    };
    let mut server = OnvifServer::new(&cfg);
    server.register_handler("GetProfiles", Box::new(OkHandler));
    let listener = free_listener().await;
    let port = listener.local_addr()?.port();
    let mut handle = server.start_on(listener).await?;

    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let body = "A".repeat(4096);
    let req = format!(
        "POST /onvif/device_service HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    sock.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await?;
    let status = String::from_utf8_lossy(&buf);
    assert!(
        status.starts_with("HTTP/1.1 413"),
        "expected 413, got: {}",
        status.lines().next().unwrap_or_default()
    );
    handle.shutdown().await?;
    Ok(())
}

/// Handler trait object stays usable behind the shared Arc (compile-level
/// smoke — keeps the public seam explicit).
#[test]
fn handler_trait_is_object_safe() {
    let _h: Box<dyn OnvifActionHandler> = Box::new(OkHandler);
    let _ = Arc::new(OkHandler);
}

// ---------------------------------------------------------------------------
// Enterprise hardening P0 (#16 replay guard + lockout, #15 panic isolation)
// ---------------------------------------------------------------------------

struct PanickingHandler;
#[async_trait]
impl OnvifActionHandler for PanickingHandler {
    async fn handle(&self, _body: &str, _info: &RequestInfo) -> Result<String, OnvifError> {
        panic!("handler bug");
    }
}

/// Civil-date formatting from unix seconds (no chrono dep in the test).
fn rfc3339_now(offset_secs: i64) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + offset_secs;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // days-from-civil inverse (Howard Hinnant's algorithm)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn digest_envelope(nonce_b64: &str, created: &str, digest: &str) -> String {
    format!(
        "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\"><Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\"><UsernameToken><Username>admin</Username><Password Type=\"digest\">{digest}</Password><Nonce>{nonce_b64}</Nonce><Created>{created}</Created></UsernameToken></Security></Header><Body><GetProfiles/></Body></Envelope>"
    )
}

async fn post_body(port: u16, body: &str) -> String {
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let req = format!(
        "POST /onvif/device_service HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    sock.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.expect("read");
    String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

async fn start_with(cfg: OnvifConfig) -> (u16, onvif_device_rs::server::OnvifServerHandle) {
    let mut server = OnvifServer::new(&cfg);
    server.register_handler("GetProfiles", Box::new(OkHandler));
    let listener = free_listener().await;
    let port = listener.local_addr().expect("addr").port();
    let handle = server.start_on(listener).await.expect("start");
    (port, handle)
}

/// A captured digest UsernameToken must not be replayable: the second
/// identical token is rejected with 401 (#16).
#[tokio::test]
async fn username_token_replay_rejected() -> anyhow::Result<()> {
    use base64::Engine;
    let (port, mut handle) = start_with(config("secret")).await;

    let nonce = base64::engine::general_purpose::STANDARD.encode(b"replay-nonce-1");
    let created = rfc3339_now(0);
    let digest = onvif_device_rs::auth::compute_password_digest(&nonce, &created, "secret");
    let env = digest_envelope(&nonce, &created, &digest);

    assert_eq!(post_body(port, &env).await, "200");
    assert_eq!(
        post_body(port, &env).await,
        "401",
        "replayed token must be rejected"
    );
    handle.shutdown().await?;
    Ok(())
}

/// A digest whose Created is outside the freshness window is rejected even
/// with a correct password digest (#16).
#[tokio::test]
async fn username_token_stale_created_rejected() -> anyhow::Result<()> {
    use base64::Engine;
    let (port, mut handle) = start_with(config("secret")).await;

    let nonce = base64::engine::general_purpose::STANDARD.encode(b"stale-nonce");
    let created = rfc3339_now(-3600); // an hour old, default window is 300s
    let digest = onvif_device_rs::auth::compute_password_digest(&nonce, &created, "secret");
    assert_eq!(
        post_body(port, &digest_envelope(&nonce, &created, &digest)).await,
        "401"
    );
    handle.shutdown().await?;
    Ok(())
}

/// Repeated bad credentials lock the source out — even the correct
/// password is refused during the window (#16).
#[tokio::test]
async fn auth_failure_lockout() -> anyhow::Result<()> {
    let cfg = OnvifConfig {
        auth_failure_limit: 3,
        auth_lockout_secs: 1,
        ..config("secret")
    };
    let (port, mut handle) = start_with(cfg).await;

    let envelope = |pass: &str| {
        format!(
            "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\"><Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\"><UsernameToken><Username>admin</Username><Password>{pass}</Password></UsernameToken></Security></Header><Body><GetProfiles/></Body></Envelope>"
        )
    };
    for _ in 0..3 {
        assert_eq!(post_body(port, &envelope("wrong")).await, "401");
    }
    // Correct password is refused while locked out.
    assert_eq!(post_body(port, &envelope("secret")).await, "401");

    // The lockout expires.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(post_body(port, &envelope("secret")).await, "200");
    handle.shutdown().await?;
    Ok(())
}

/// A panicking handler must not take the connection or the server down:
/// the request gets a 500 and the server keeps serving (#15 containment).
#[tokio::test]
async fn panicking_handler_isolated() -> anyhow::Result<()> {
    let mut server = OnvifServer::new(&config("secret"));
    server.register_handler("GetProfiles", Box::new(OkHandler));
    server.register_handler("Boom", Box::new(PanickingHandler));
    let listener = free_listener().await;
    let port = listener.local_addr()?.port();
    let mut handle = server.start_on(listener).await?;

    let boom = "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\"><Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\"><UsernameToken><Username>admin</Username><Password>secret</Password></UsernameToken></Security></Header><Body><Boom/></Body></Envelope>";
    assert_eq!(post_body(port, boom).await, "500");

    let ok = "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\"><Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\"><UsernameToken><Username>admin</Username><Password>secret</Password></UsernameToken></Security></Header><Body><GetProfiles/></Body></Envelope>";
    assert_eq!(
        post_body(port, ok).await,
        "200",
        "server must survive the panic"
    );
    handle.shutdown().await?;
    Ok(())
}
