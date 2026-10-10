//! HTTP Digest transport auth + IP filter integration tests (issue #54):
//! raw-TcpStream client driving the real server — challenge shape over
//! the wire, the RFC 7616 MD5/qop="auth" handshake, nonce replay
//! protection, UsernameToken coexistence, and per-connection IP
//! filtering. Mirrors the tests/server_lifecycle.rs harness style.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use onvif_device_rs::device::{IpEntry, IpFilter, IpFilterMode};
use onvif_device_rs::server::{OnvifActionHandler, OnvifConfig, OnvifServer};
use onvif_device_rs::types::{OnvifError, RequestInfo};

/// The library's fixed Digest realm (challenge contract).
const REALM: &str = "onvif";

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

fn digest_config(password: &str) -> OnvifConfig {
    OnvifConfig {
        http_digest: true,
        ..config(password)
    }
}

async fn start(cfg: OnvifConfig) -> (u16, onvif_device_rs::server::OnvifServerHandle) {
    let mut server = OnvifServer::new(&cfg);
    server.register_handler("GetProfiles", Box::new(OkHandler));
    server.register_anonymous_action("GetSystemDateAndTime");
    server.register_handler("GetSystemDateAndTime", Box::new(OkHandler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = server.start_on(listener).await.expect("start");
    (port, handle)
}

/// POST a SOAP body (optionally with an Authorization header); returns
/// the full raw response (status line + headers + body).
async fn post(port: u16, body: &str, authorization: Option<&str>) -> String {
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let auth = authorization
        .map(|a| format!("Authorization: {a}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST /onvif/device_service HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    sock.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.expect("read");
    String::from_utf8_lossy(&buf).into_owned()
}

fn status_of(response: &str) -> String {
    response
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

fn header_of<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    response.lines().find_map(|l| {
        let lower = l.to_ascii_lowercase();
        lower
            .starts_with(&format!("{name}:").to_ascii_lowercase())
            .then(|| l.split_once(':').map(|(_, v)| v.trim()).unwrap_or(""))
    })
}

/// Extract a quoted param (`nonce="..."`) from a Digest header value.
fn param_of(header_value: &str, key: &str) -> Option<String> {
    let (_, rest) = header_value.split_once(&format!("{key}=\""))?;
    let (value, _) = rest.split_once('"')?;
    Some(value.to_string())
}

fn md5hex(s: &str) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Client-side RFC 7616 response computation (MD5, qop=auth).
fn digest_authorization(password: &str, nonce: &str, nc: &str, cnonce: &str, uri: &str) -> String {
    let ha1 = md5hex(&format!("admin:{REALM}:{password}"));
    let ha2 = md5hex(&format!("POST:{uri}"));
    let response = md5hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
    format!(
        "Digest username=\"admin\", realm=\"{REALM}\", nonce=\"{nonce}\", \
         uri=\"{uri}\", qop=auth, nc={nc}, cnonce=\"{cnonce}\", \
         response=\"{response}\", opaque=\"x\", algorithm=MD5"
    )
}

const SOAP_BODY: &str = "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\">\
                         <Body><GetProfiles/></Body></Envelope>";
const ANON_BODY: &str = "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\">\
                         <Body><GetSystemDateAndTime/></Body></Envelope>";

// ---------------------------------------------------------------------------
// Challenge over the wire
// ---------------------------------------------------------------------------

/// A token-less request on a digest-enabled server gets 401 with the
/// `WWW-Authenticate: Digest` challenge (exact shape contract).
#[tokio::test]
async fn challenge_issued_on_missing_credentials() -> anyhow::Result<()> {
    let (port, mut handle) = start(digest_config("secret")).await;

    let resp = post(port, SOAP_BODY, None).await;
    assert_eq!(status_of(&resp), "401");
    let challenge = header_of(&resp, "WWW-Authenticate").expect("challenge header");
    assert!(
        challenge.starts_with("Digest realm=\"onvif\", nonce=\""),
        "{challenge}"
    );
    assert!(challenge.contains("qop=\"auth\""), "{challenge}");
    assert!(challenge.contains("algorithm=MD5"), "{challenge}");
    assert!(challenge.contains("stale=FALSE"), "{challenge}");
    assert!(challenge.contains("opaque=\""), "{challenge}");
    // SOAP fault body (not a bare HTTP error).
    assert!(resp.contains("soap:Fault"), "{resp}");
    handle.shutdown().await?;
    Ok(())
}

/// Digest off (default): the historical 401 — no WWW-Authenticate
/// header, byte-stable responses for existing deployments.
#[tokio::test]
async fn no_challenge_when_digest_disabled() -> anyhow::Result<()> {
    let (port, mut handle) = start(config("secret")).await;

    let resp = post(port, SOAP_BODY, None).await;
    assert_eq!(status_of(&resp), "401");
    assert!(
        header_of(&resp, "WWW-Authenticate").is_none(),
        "no challenge expected: {resp}"
    );
    handle.shutdown().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The handshake
// ---------------------------------------------------------------------------

/// Full challenge→response flow authenticates without any
/// UsernameToken (issue #54 core path).
#[tokio::test]
async fn digest_authenticates_without_username_token() -> anyhow::Result<()> {
    let (port, mut handle) = start(digest_config("secret")).await;

    let challenge = post(port, SOAP_BODY, None).await;
    let nonce = param_of(
        header_of(&challenge, "WWW-Authenticate").unwrap_or_default(),
        "nonce",
    )
    .expect("nonce in challenge");
    let auth = digest_authorization("secret", &nonce, "00000001", "c1", "/onvif/device_service");
    let resp = post(port, SOAP_BODY, Some(&auth)).await;
    assert_eq!(status_of(&resp), "200", "{resp}");
    assert!(resp.contains("OkResponse"), "{resp}");
    handle.shutdown().await?;
    Ok(())
}

/// Wrong password: 401 with a fresh (stale=FALSE) challenge.
#[tokio::test]
async fn digest_wrong_password_rechallenged() -> anyhow::Result<()> {
    let (port, mut handle) = start(digest_config("secret")).await;

    let challenge = post(port, SOAP_BODY, None).await;
    let nonce = param_of(
        header_of(&challenge, "WWW-Authenticate").unwrap_or_default(),
        "nonce",
    )
    .expect("nonce");
    let auth = digest_authorization("WRONG", &nonce, "00000001", "c1", "/onvif/device_service");
    let resp = post(port, SOAP_BODY, Some(&auth)).await;
    assert_eq!(status_of(&resp), "401");
    let challenge = header_of(&resp, "WWW-Authenticate").expect("re-challenge");
    assert!(challenge.contains("stale=FALSE"), "{challenge}");
    handle.shutdown().await?;
    Ok(())
}

/// An unknown nonce (never issued) is answered stale=TRUE so a compliant
/// client re-challenges instead of giving up (RFC 7616 §3.3).
#[tokio::test]
async fn digest_unknown_nonce_marks_stale() -> anyhow::Result<()> {
    let (port, mut handle) = start(digest_config("secret")).await;

    let auth = digest_authorization(
        "secret",
        "never-issued-nonce",
        "00000001",
        "c1",
        "/onvif/device_service",
    );
    let resp = post(port, SOAP_BODY, Some(&auth)).await;
    assert_eq!(status_of(&resp), "401");
    let challenge = header_of(&resp, "WWW-Authenticate").expect("challenge");
    assert!(challenge.contains("stale=TRUE"), "{challenge}");
    handle.shutdown().await?;
    Ok(())
}

/// A captured Authorization header is not replayable: the byte-identical
/// request (same nonce, same nc) is rejected; advancing nc succeeds.
#[tokio::test]
async fn digest_replay_rejected_nc_advance_accepted() -> anyhow::Result<()> {
    let (port, mut handle) = start(digest_config("secret")).await;

    let challenge = post(port, SOAP_BODY, None).await;
    let nonce = param_of(
        header_of(&challenge, "WWW-Authenticate").unwrap_or_default(),
        "nonce",
    )
    .expect("nonce");
    let nc1 = digest_authorization("secret", &nonce, "00000001", "c1", "/onvif/device_service");
    assert_eq!(status_of(&post(port, SOAP_BODY, Some(&nc1)).await), "200");
    // Byte-identical replay.
    assert_eq!(
        status_of(&post(port, SOAP_BODY, Some(&nc1)).await),
        "401",
        "captured header must be refused"
    );
    // Next count on the same nonce is fine.
    let nc2 = digest_authorization("secret", &nonce, "00000002", "c2", "/onvif/device_service");
    assert_eq!(status_of(&post(port, SOAP_BODY, Some(&nc2)).await), "200");
    handle.shutdown().await?;
    Ok(())
}

/// Wrong digest-uri (Authorization uri ≠ request line): refused.
#[tokio::test]
async fn digest_uri_mismatch_rejected() -> anyhow::Result<()> {
    let (port, mut handle) = start(digest_config("secret")).await;

    let challenge = post(port, SOAP_BODY, None).await;
    let nonce = param_of(
        header_of(&challenge, "WWW-Authenticate").unwrap_or_default(),
        "nonce",
    )
    .expect("nonce");
    let auth = digest_authorization("secret", &nonce, "00000001", "c1", "/onvif/other_service");
    let resp = post(port, SOAP_BODY, Some(&auth)).await;
    assert_eq!(status_of(&resp), "401");
    handle.shutdown().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Coexistence with WS-Security
// ---------------------------------------------------------------------------

fn token_envelope(user: &str, pass: &str, body: &str) -> String {
    format!(
        "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\"><Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\"><UsernameToken><Username>{user}</Username><Password>{pass}</Password></UsernameToken></Security></Header><Body>{body}</Body></Envelope>"
    )
}

/// With Digest enabled, an existing valid UsernameToken still passes
/// (precedence: the token path is untouched).
#[tokio::test]
async fn username_token_still_passes_with_digest_enabled() -> anyhow::Result<()> {
    let (port, mut handle) = start(digest_config("secret")).await;

    let body = token_envelope("admin", "secret", "<GetProfiles/>");
    let resp = post(port, &body, None).await;
    assert_eq!(status_of(&resp), "200", "{resp}");
    handle.shutdown().await?;
    Ok(())
}

/// A bad UsernameToken is still refused — and now carries the Digest
/// challenge so the client can switch mechanisms.
#[tokio::test]
async fn bad_username_token_gets_digest_challenge() -> anyhow::Result<()> {
    let (port, mut handle) = start(digest_config("secret")).await;

    let body = token_envelope("admin", "wrong", "<GetProfiles/>");
    let resp = post(port, &body, None).await;
    assert_eq!(status_of(&resp), "401");
    assert!(
        header_of(&resp, "WWW-Authenticate").is_some(),
        "client must be able to switch to Digest: {resp}"
    );
    handle.shutdown().await?;
    Ok(())
}

/// Pre-auth anonymous actions (GetSystemDateAndTime) stay open with
/// Digest enabled and no credentials at all.
#[tokio::test]
async fn anonymous_action_stays_open() -> anyhow::Result<()> {
    let (port, mut handle) = start(digest_config("secret")).await;

    let resp = post(port, ANON_BODY, None).await;
    assert_eq!(status_of(&resp), "200", "{resp}");
    handle.shutdown().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// IP filter enforcement (issue #54)
// ---------------------------------------------------------------------------

async fn start_with_filter(
    cfg: OnvifConfig,
    filter: IpFilter,
) -> (u16, onvif_device_rs::server::OnvifServerHandle) {
    let mut server = OnvifServer::new(&cfg);
    server.register_handler("GetProfiles", Box::new(OkHandler));
    server.register_anonymous_action("GetSystemDateAndTime");
    server.register_handler("GetSystemDateAndTime", Box::new(OkHandler));
    let state: onvif_device_rs::device::IpFilterState = Arc::new(std::sync::RwLock::new(filter));
    server = server.with_ip_filter(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = server.start_on(listener).await.expect("start");
    (port, handle)
}

/// Deny-mode filter matching the loopback peer: refused with 403
/// before any auth processing.
#[tokio::test]
async fn ip_filter_blocks_matching_peer() -> anyhow::Result<()> {
    let filter = IpFilter {
        enabled: true,
        mode: IpFilterMode::Deny,
        entries: vec![IpEntry {
            ipv4: "127.0.0.1".to_string(),
            prefix_len: 32,
        }],
    };
    let (port, mut handle) = start_with_filter(config("secret"), filter).await;

    // Even correct credentials are refused: the gate runs before auth.
    let body = token_envelope("admin", "secret", "<GetProfiles/>");
    let resp = post(port, &body, None).await;
    assert_eq!(status_of(&resp), "403", "{resp}");
    assert!(resp.contains("soap:Fault"), "{resp}");
    handle.shutdown().await?;
    Ok(())
}

/// Allow-mode filter not matching the loopback peer: also 403.
#[tokio::test]
async fn ip_filter_allow_mode_blocks_unlisted_peer() -> anyhow::Result<()> {
    let filter = IpFilter {
        enabled: true,
        mode: IpFilterMode::Allow,
        entries: vec![IpEntry {
            ipv4: "10.99.0.0".to_string(),
            prefix_len: 16,
        }],
    };
    let (port, mut handle) = start_with_filter(config("secret"), filter).await;
    let resp = post(port, SOAP_BODY, None).await;
    assert_eq!(status_of(&resp), "403", "{resp}");
    handle.shutdown().await?;
    Ok(())
}

/// Disabled filter: no enforcement (historical behavior).
#[tokio::test]
async fn ip_filter_disabled_allows_all() -> anyhow::Result<()> {
    let (port, mut handle) = start_with_filter(config("secret"), IpFilter::disabled()).await;
    let body = token_envelope("admin", "secret", "<GetProfiles/>");
    let resp = post(port, &body, None).await;
    assert_eq!(status_of(&resp), "200", "{resp}");
    handle.shutdown().await?;
    Ok(())
}

/// Allow-mode filter that DOES match the loopback peer: request passes
/// the gate and authenticates normally.
#[tokio::test]
async fn ip_filter_matching_peer_passes() -> anyhow::Result<()> {
    let filter = IpFilter {
        enabled: true,
        mode: IpFilterMode::Allow,
        entries: vec![IpEntry {
            ipv4: "127.0.0.0".to_string(),
            prefix_len: 8,
        }],
    };
    let (port, mut handle) = start_with_filter(config("secret"), filter).await;
    let body = token_envelope("admin", "secret", "<GetProfiles/>");
    let resp = post(port, &body, None).await;
    assert_eq!(status_of(&resp), "200", "{resp}");
    handle.shutdown().await?;
    Ok(())
}
