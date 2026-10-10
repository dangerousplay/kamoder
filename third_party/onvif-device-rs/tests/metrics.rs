//! MetricsHooks observability seam (issue #18): real requests over the
//! wire must drive the hooks — hosts bridge them to Prometheus or any
//! backend without the library taking a dependency.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use onvif_device_rs::metrics::MetricsHooks;
use onvif_device_rs::server::OnvifActionHandler;
use onvif_device_rs::types::{OnvifError, RequestInfo};
use onvif_device_rs::{DiscoveryServer, OnvifConfig, OnvifServer};

#[derive(Default)]
struct CountingHooks {
    soap_requests: AtomicU32,
    soap_faults: AtomicU32,
    auth_fails: AtomicU32,
    probes_answered: AtomicU32,
    actions: Mutex<Vec<String>>,
}

impl MetricsHooks for CountingHooks {
    fn soap_request(&self, action: &str) {
        self.soap_requests.fetch_add(1, Ordering::Relaxed);
        self.actions.lock().unwrap().push(action.to_string());
    }
    fn soap_fault(&self, _action: &str) {
        self.soap_faults.fetch_add(1, Ordering::Relaxed);
    }
    fn auth_fail(&self) {
        self.auth_fails.fetch_add(1, Ordering::Relaxed);
    }
    fn discovery_probe_answered(&self) {
        self.probes_answered.fetch_add(1, Ordering::Relaxed);
    }
}

struct OkHandler;
#[async_trait]
impl OnvifActionHandler for OkHandler {
    async fn handle(&self, _body: &str, _info: &RequestInfo) -> Result<String, OnvifError> {
        Ok("<OkResponse/>".to_string())
    }
}

struct BoomHandler;
#[async_trait]
impl OnvifActionHandler for BoomHandler {
    async fn handle(&self, _body: &str, _info: &RequestInfo) -> Result<String, OnvifError> {
        Err(OnvifError::Internal("boom".to_string()))
    }
}

fn config(password: &str) -> OnvifConfig {
    OnvifConfig {
        port: 0,
        username: "admin".to_string(),
        password: password.to_string(),
        ..OnvifConfig::default()
    }
}

async fn free_listener() -> tokio::net::TcpListener {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
}

async fn post(port: u16, body: &str) -> String {
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let req = format!(
        "POST /onvif/device_service HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    tokio::io::AsyncWriteExt::write_all(&mut sock, req.as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut sock, &mut buf)
        .await
        .unwrap();
    String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

fn envelope(user: &str, pass: &str, action: &str) -> String {
    format!(
        "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\"><Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\"><UsernameToken><Username>{user}</Username><Password>{pass}</Password></UsernameToken></Security></Header><Body><{action}/></Body></Envelope>"
    )
}

/// Over-the-wire: dispatched requests count by action, handler errors
/// count as faults, and failed credentials count as auth failures.
#[tokio::test]
async fn server_hooks_fire_over_the_wire() -> anyhow::Result<()> {
    let hooks = Arc::new(CountingHooks::default());
    let mut server = OnvifServer::new(&config("secret")).with_metrics(hooks.clone());
    server.register_handler("GetProfiles", Box::new(OkHandler));
    server.register_handler("BoomAction", Box::new(BoomHandler));
    let listener = free_listener().await;
    let port = listener.local_addr()?.port();
    let mut handle = server.start_on(listener).await?;

    // Good credentials + healthy handler → 200, one dispatched request.
    assert_eq!(
        post(port, &envelope("admin", "secret", "GetProfiles")).await,
        "200"
    );
    // Bad credentials → 401, nothing dispatched, one auth failure.
    assert_eq!(
        post(port, &envelope("admin", "wrong", "GetProfiles")).await,
        "401"
    );
    // Good credentials + failing handler → 500, dispatched and faulted.
    assert_eq!(
        post(port, &envelope("admin", "secret", "BoomAction")).await,
        "500"
    );

    handle.shutdown().await?;

    assert_eq!(hooks.soap_requests.load(Ordering::Relaxed), 2);
    assert_eq!(hooks.soap_faults.load(Ordering::Relaxed), 1);
    assert_eq!(hooks.auth_fails.load(Ordering::Relaxed), 1);
    assert_eq!(
        *hooks.actions.lock().unwrap(),
        vec!["GetProfiles".to_string(), "BoomAction".to_string()]
    );
    Ok(())
}

/// The discovery responder fires `discovery_probe_answered` for every
/// ProbeMatches it sends. The listener binds the fixed WS-Discovery port
/// 3702; when a real responder already holds it locally the test yields
/// (CI runners have it free).
#[tokio::test]
async fn discovery_probe_hook_fires() -> anyhow::Result<()> {
    let hooks = Arc::new(CountingHooks::default());
    let server = DiscoveryServer::new("127.0.0.1", 8080).with_metrics(hooks.clone());
    let mut handle = match server.start().await {
        Ok(h) => h,
        Err(e) => {
            // Port 3702 already bound by a real discovery responder — the
            // hook wiring is identical, just not observable here.
            eprintln!("skipping: discovery listener unavailable: {e}");
            return Ok(());
        }
    };

    // A unicast Probe reaches the 0.0.0.0:3702 listener just fine.
    let probe = "<?xml version=\"1.0\"?><Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\"><Header><Action xmlns=\"http://schemas.xmlsoap.org/ws/2004/09/discovery\">http://schemas.xmlsoap.org/ws/2004/09/discovery/Probe</Action><MessageID xmlns=\"http://schemas.xmlsoap.org/ws/2004/09/discovery\">uuid:metrics-test-1</MessageID></Header><Body><Probe xmlns=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\"/></Body></Envelope>";
    let sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
    sock.send_to(probe.as_bytes(), "127.0.0.1:3702")?;

    for _ in 0..50 {
        if hooks.probes_answered.load(Ordering::Relaxed) >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    handle.shutdown().await?;

    assert!(
        hooks.probes_answered.load(Ordering::Relaxed) >= 1,
        "ProbeMatches must have been sent and counted"
    );
    Ok(())
}
