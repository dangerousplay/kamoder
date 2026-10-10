//! Events pull-point integration tests: the full WS-BaseNotification
//! subscription lifecycle over a real socket on an ephemeral port
//! (parity with onvif-go server/events_test.go — the golden wire source).
//!
//! create → publish → pull receives the event → renew → unsubscribe →
//! pull after unsubscribe fails, plus wire goldens, long-poll, auth
//! policy, topic filtering, the pull-point cap, lazy expiry, and the
//! GetCapabilities/GetServices advertisement.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use onvif_device_rs::config::DeviceConfig;
use onvif_device_rs::device::{DeviceHandler, DeviceServiceHandlers};
use onvif_device_rs::events::{Event, SimpleItem, EVENTS_SERVICE_PATH};
use onvif_device_rs::server::{OnvifConfig, OnvifServer, OnvifServerHandle};

const USERNAME: &str = "admin";
const PASSWORD: &str = "password";

// ---------------------------------------------------------------------------
// Minimal ONVIF client (what a subscribing NVR implements)
// ---------------------------------------------------------------------------

/// POST a SOAP 1.2 envelope whose body is `body_xml` to `path`. With
/// `auth` a PasswordText UsernameToken is carried, like a real
/// subscribing client (the default policy protects Create*).
async fn post_soap(port: u16, path: &str, body_xml: &str, auth: bool) -> (u16, String) {
    let header = if auth {
        format!(
            "<Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\">\
             <UsernameToken><Username>{USERNAME}</Username>\
             <Password Type=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordText\">{PASSWORD}</Password>\
             </UsernameToken></Security></Header>"
        )
    } else {
        String::new()
    };
    let envelope = format!(
        "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\">{header}\
         <Body>{body_xml}</Body></Envelope>"
    );

    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Content-Type: application/soap+xml; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{envelope}",
        envelope.len()
    );
    sock.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .expect("status line");
    let body = text[text.find("\r\n\r\n").map(|p| p + 4).unwrap_or(text.len())..].to_string();
    (status, body)
}

/// Text content of the first `<tag ...>` element (local-name tolerant).
fn xml_field<'a>(xml: &'a str, tag: &str) -> &'a str {
    let open = format!("<{tag}");
    let mut from = 0;
    while let Some(rel) = xml[from..].find(&open) {
        let after = from + rel + open.len();
        let next = xml[after..].chars().next().unwrap_or('>');
        if next == '>' || next == ' ' || next == '/' {
            let start = xml[after..].find('>').map_or(after, |g| after + g + 1);
            let end = xml[start..].find("</").map_or(start, |e| start + e);
            return xml[start..end].trim();
        }
        from = after;
    }
    ""
}

/// Parse an RFC3339 second-resolution timestamp to unix seconds
/// (days-from-civil inverse — no chrono dependency in the test).
fn rfc3339_to_unix(ts: &str) -> i64 {
    if ts.len() != 20 {
        return 0;
    }
    let num = |from: usize, to: usize| -> i64 { ts[from..to].parse().unwrap_or(0) };
    let (y, mo, d) = (num(0, 4), num(5, 7), num(8, 10));
    let (h, mi, s) = (num(11, 13), num(14, 16), num(17, 19));
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400 + h * 3600 + mi * 60 + s
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// Server scaffolding
// ---------------------------------------------------------------------------

fn base_config(support_events: bool) -> OnvifConfig {
    OnvifConfig {
        port: 0,
        username: USERNAME.to_string(),
        password: PASSWORD.to_string(),
        support_events,
        ..Default::default()
    }
}

fn test_device_config() -> DeviceConfig {
    DeviceConfig {
        name: "Test Cam".into(),
        manufacturer: "MiBee".into(),
        model: "IMX219".into(),
        firmware: "1.0.0".into(),
        hardware_id: "HW-1".into(),
        serial_number: "SN-1".into(),
    }
}

/// Start a server with the events service (and the device service, for
/// advertisement checks) on an ephemeral port; returns the port, the
/// events publish seam, and the handle.
async fn events_server() -> (
    u16,
    Arc<onvif_device_rs::events::EventsService>,
    OnvifServerHandle,
) {
    let mut server = OnvifServer::new(&base_config(true));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let device = Arc::new(
        DeviceServiceHandlers::new(test_device_config(), port, "127.0.0.1".to_string())
            .expect("valid identity")
            .with_events_support(true),
    );
    for action in [
        "GetSystemDateAndTime",
        "GetDeviceInformation",
        "GetCapabilities",
        "GetServices",
        "GetScopes",
    ] {
        server.register_anonymous_action(action);
        server.register_handler(action, Box::new(DeviceHandler(Arc::clone(&device))));
    }

    let events = server.enable_events();
    let handle = server.start_on(listener).await.expect("start");
    (port, events, handle)
}

/// Create a subscription (termination PT10M unless overridden) and return
/// the subscription path (not the absolute address).
async fn subscribe(port: u16, termination: &str) -> String {
    let body = format!(
        "<CreatePullPointSubscription xmlns=\"http://www.onvif.org/ver10/events/wsdl\">\
         <InitialTerminationTime>{termination}</InitialTerminationTime>\
         </CreatePullPointSubscription>"
    );
    let (status, body) = post_soap(port, EVENTS_SERVICE_PATH, &body, true).await;
    assert_eq!(status, 200, "create ({termination}) failed:\n{body}");
    let address = xml_field(&body, "wsa:Address");
    assert!(
        !address.is_empty(),
        "no SubscriptionReference address:\n{body}"
    );
    let id = address.rsplit('/').next().expect("id segment");
    format!("/onvif/events_service/sub/{id}")
}

async fn pull(port: u16, sub_path: &str, timeout: &str, limit: u32) -> (u16, String) {
    let body = format!(
        "<PullMessages xmlns=\"http://www.onvif.org/ver10/events/wsdl\">\
         <Timeout>{timeout}</Timeout><MessageLimit>{limit}</MessageLimit>\
         </PullMessages>"
    );
    post_soap(port, sub_path, &body, false).await
}

// ---------------------------------------------------------------------------
// Wire goldens (deterministic responses, full envelope pinned)
// ---------------------------------------------------------------------------

/// The byte-stable capabilities answer (no time fields → deterministic).
#[tokio::test]
async fn golden_get_service_capabilities_over_the_wire() {
    let (port, _events, mut handle) = events_server().await;

    let (status, body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        "<GetServiceCapabilities xmlns=\"http://www.onvif.org/ver10/events/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200);

    let want = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\">\n  \
<soap:Header>\n  </soap:Header>\n  \
<soap:Body><tev:GetServiceCapabilitiesResponse xmlns:tev=\"http://www.onvif.org/ver10/events/wsdl\">\n  \
<tev:Capabilities WSPullPointSupport=\"true\" MaxPullPoints=\"10\"></tev:Capabilities>\n\
</tev:GetServiceCapabilitiesResponse>\n  \
</soap:Body>\n\
</soap:Envelope>";
    assert_eq!(body, want);

    handle.shutdown().await.expect("shutdown");
}

/// The spec-complete properties answer (no time fields → deterministic):
/// fixed empty topic set, the two mandatory topic-expression dialects,
/// the single empty message-content filter dialect, the ONVIF namespace /
/// schema locations.
#[tokio::test]
async fn golden_get_event_properties_over_the_wire() {
    let (port, _events, mut handle) = events_server().await;

    let (status, body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        "<GetEventProperties xmlns=\"http://www.onvif.org/ver10/events/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200);

    let want = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\">\n  \
<soap:Header>\n  </soap:Header>\n  \
<soap:Body><tev:GetEventPropertiesResponse xmlns:tev=\"http://www.onvif.org/ver10/events/wsdl\" xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\" xmlns:wstop=\"http://docs.oasis-open.org/wsn/t-1\">\n  \
<tev:TopicNamespaceLocation>http://www.onvif.org/ver10/tev/topicns.xml</tev:TopicNamespaceLocation>\n  \
<wsnt:FixedTopicSet>true</wsnt:FixedTopicSet>\n  \
<wstop:TopicSet></wstop:TopicSet>\n  \
<wsnt:TopicExpressionDialect>http://docs.oasis-open.org/wsn/t-1/TopicExpression/Concrete</wsnt:TopicExpressionDialect>\n  \
<wsnt:TopicExpressionDialect>http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet</wsnt:TopicExpressionDialect>\n  \
<wsnt:MessageContentFilterDialect></wsnt:MessageContentFilterDialect>\n  \
<tev:MessageContentSchemaLocation>http://www.onvif.org/ver10/schema/onvif.xsd</tev:MessageContentSchemaLocation>\n\
</tev:GetEventPropertiesResponse>\n  \
</soap:Body>\n\
</soap:Envelope>";
    assert_eq!(body, want);

    handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// The full lifecycle (the required end-to-end dance)
// ---------------------------------------------------------------------------

/// create → publish → pull receives the event (double-layer payload) →
/// renew → unsubscribe → pull after unsubscribe fails as a Sender fault.
#[tokio::test]
async fn full_lifecycle_create_publish_pull_renew_unsubscribe() {
    let (port, events, mut handle) = events_server().await;

    // -- create ------------------------------------------------------------
    let (status, body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        "<CreatePullPointSubscription xmlns=\"http://www.onvif.org/ver10/events/wsdl\">\
         <InitialTerminationTime>PT5M</InitialTerminationTime>\
         </CreatePullPointSubscription>",
        true,
    )
    .await;
    assert_eq!(status, 200, "create failed:\n{body}");

    let address = xml_field(&body, "wsa:Address").to_string();
    assert!(
        address.starts_with("http://127.0.0.1:") && address.contains("/onvif/events_service/sub/"),
        "SubscriptionReference address = {address:?}"
    );
    let id = address.rsplit('/').next().expect("id").to_string();
    assert!(id.len() >= 16, "subscription id {id:?} not opaque");
    let sub_path = format!("/onvif/events_service/sub/{id}");

    let term = rfc3339_to_unix(xml_field(&body, "wsnt:TerminationTime"));
    let remaining = term - unix_now();
    assert!(
        (240..=360).contains(&remaining),
        "PT5M termination remaining {remaining}s"
    );
    assert_ne!(rfc3339_to_unix(xml_field(&body, "wsnt:CurrentTime")), 0);

    // -- publish (host seam) -------------------------------------------------
    events.publish_event(Event {
        topic: "tns1:VideoSource/MotionAlarm".into(),
        source: vec![SimpleItem::new("Source", "CSI")],
        data: vec![
            SimpleItem::new("State", "true"),
            SimpleItem::new("Score", "87"),
        ],
        ..Event::new("tns1:VideoSource/MotionAlarm")
    });

    // -- pull receives the event ---------------------------------------------
    let (status, body) = pull(port, &sub_path, "PT2S", 5).await;
    assert_eq!(status, 200, "pull failed:\n{body}");

    // Canonical double-layer payload (issue-shape pinned by the Go twin):
    // wsnt:NotificationMessage > wsnt:Topic + wsnt:Message > tt:Message.
    assert_eq!(body.matches("<wsnt:NotificationMessage>").count(), 1);
    assert_eq!(
        xml_field(&body, "wsnt:Topic"),
        "tns1:VideoSource/MotionAlarm"
    );
    assert!(
        xml_field(&body, "wsa:Address").starts_with("http://127.0.0.1:"),
        "ProducerReference address missing: {}",
        xml_field(&body, "wsa:Address")
    );
    let tt_message = body
        .split("<tt:Message ")
        .nth(1)
        .and_then(|rest| rest.split("</tt:Message>").next())
        .unwrap_or_default();
    assert!(
        tt_message.contains("PropertyOperation=\"Changed\""),
        "default PropertyOperation missing:\n{tt_message}"
    );
    assert!(
        tt_message.contains("UtcTime=\""),
        "UtcTime attribute missing"
    );
    let source = tt_message
        .split("<tt:Source>")
        .nth(1)
        .and_then(|r| r.split("</tt:Source>").next())
        .unwrap_or_default();
    assert!(
        source.contains("<tt:SimpleItem Name=\"Source\" Value=\"CSI\"/>"),
        "Source SimpleItems:\n{source}"
    );
    let data = tt_message
        .split("<tt:Data>")
        .nth(1)
        .and_then(|r| r.split("</tt:Data>").next())
        .unwrap_or_default();
    assert!(
        data.contains("<tt:SimpleItem Name=\"State\" Value=\"true\"/>")
            && data.contains("<tt:SimpleItem Name=\"Score\" Value=\"87\"/>"),
        "Data SimpleItems:\n{data}"
    );

    // -- renew ---------------------------------------------------------------
    let (status, body) = post_soap(
        port,
        &sub_path,
        "<Renew xmlns=\"http://docs.oasis-open.org/wsn/b-2\">\
         <TerminationTime>PT10M</TerminationTime>\
         </Renew>",
        false,
    )
    .await;
    assert_eq!(status, 200, "renew failed:\n{body}");
    assert!(body.contains("RenewResponse"));
    let remaining = rfc3339_to_unix(xml_field(&body, "wsnt:TerminationTime")) - unix_now();
    assert!(
        (540..=660).contains(&remaining),
        "renewed PT10M remaining {remaining}s"
    );

    // -- unsubscribe -----------------------------------------------------------
    let (status, body) = post_soap(
        port,
        &sub_path,
        "<Unsubscribe xmlns=\"http://docs.oasis-open.org/wsn/b-2\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200, "unsubscribe failed:\n{body}");
    assert!(body.contains("UnsubscribeResponse"));

    // -- pull after unsubscribe fails (unknown subscription) -------------------
    let (status, body) = pull(port, &sub_path, "PT0S", 5).await;
    assert_eq!(status, 400, "pull after unsubscribe must fail:\n{body}");
    assert!(
        body.contains("soap:Sender") && body.contains("Unknown subscription"),
        "must be a Sender fault naming the unknown subscription:\n{body}"
    );

    handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Long-poll, limits, expiry
// ---------------------------------------------------------------------------

/// An empty queue holds the request for the asked Timeout (long-poll),
/// then answers empty.
#[tokio::test]
async fn pull_messages_long_poll_waits() {
    let (port, _events, mut handle) = events_server().await;
    let sub = subscribe(port, "PT10M").await;

    let started = std::time::Instant::now();
    let (status, body) = pull(port, &sub, "PT1S", 5).await;
    let elapsed = started.elapsed();
    assert_eq!(status, 200);
    assert!(
        elapsed >= Duration::from_millis(900),
        "returned after {elapsed:?}"
    );
    assert_eq!(body.matches("<wsnt:NotificationMessage>").count(), 0);

    handle.shutdown().await.expect("shutdown");
}

/// The registry caps concurrent pull points; the create beyond the cap
/// is a Sender fault.
#[tokio::test]
async fn max_pull_points_enforced_over_the_wire() {
    let (port, _events, mut handle) = events_server().await;
    for _ in 0..10 {
        subscribe(port, "PT10M").await;
    }

    let (status, body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        "<CreatePullPointSubscription xmlns=\"http://www.onvif.org/ver10/events/wsdl\"/>",
        true,
    )
    .await;
    assert_eq!(status, 400, "beyond cap:\n{body}");
    assert!(
        body.contains("soap:Sender"),
        "cap exceeded must fault as Sender:\n{body}"
    );

    handle.shutdown().await.expect("shutdown");
}

/// A subscription past its termination time is pruned lazily — later
/// operations fault as unknown.
#[tokio::test]
async fn expired_subscription_rejected_over_the_wire() {
    let (port, _events, mut handle) = events_server().await;
    let sub = subscribe(port, "PT1S").await;

    tokio::time::sleep(Duration::from_millis(1200)).await;

    let (status, _body) = pull(port, &sub, "PT0S", 5).await;
    assert_eq!(status, 400, "expired subscription must fault");

    handle.shutdown().await.expect("shutdown");
}

/// Posting to a subscription address that never existed is a Sender
/// fault, not a bare 404 or a 500.
#[tokio::test]
async fn unknown_subscription_is_sender_fault() {
    let (port, _events, mut handle) = events_server().await;

    let (status, body) = pull(
        port,
        "/onvif/events_service/sub/deadbeefdeadbeef",
        "PT0S",
        5,
    )
    .await;
    assert_eq!(status, 400, "unknown subscription:\n{body}");
    assert!(
        body.contains("soap:Sender"),
        "must fault as Sender:\n{body}"
    );
    assert!(body.contains("Unknown subscription"));

    handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Topic filtering end to end (the honored Concrete / ConcreteSet dialects)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn topic_filtering_end_to_end() {
    let (port, events, mut handle) = events_server().await;

    let create_with_filter = |dialect: &str, expr: &str| {
        format!(
            "<CreatePullPointSubscription xmlns=\"http://www.onvif.org/ver10/events/wsdl\">\
             <Filter><wsnt:TopicExpression xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\" Dialect=\"{dialect}\">{expr}</wsnt:TopicExpression></Filter>\
             <InitialTerminationTime>PT10M</InitialTerminationTime>\
             </CreatePullPointSubscription>"
        )
    };

    let (status, body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        &create_with_filter(
            "http://docs.oasis-open.org/wsn/t-1/TopicExpression/Concrete",
            "tns1:VideoSource/MotionAlarm",
        ),
        true,
    )
    .await;
    assert_eq!(status, 200, "concrete subscribe:\n{body}");
    let concrete = format!(
        "/onvif/events_service/sub/{}",
        xml_field(&body, "wsa:Address").rsplit('/').next().unwrap()
    );

    let (status, body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        &create_with_filter(
            "http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet",
            "tns1:Device/*/*|tns1:VideoSource/*",
        ),
        true,
    )
    .await;
    assert_eq!(status, 200, "concreteset subscribe:\n{body}");
    let wildcard = format!(
        "/onvif/events_service/sub/{}",
        xml_field(&body, "wsa:Address").rsplit('/').next().unwrap()
    );

    let unfiltered = subscribe(port, "PT10M").await;

    events.publish_event(Event::new("tns1:VideoSource/MotionAlarm"));
    events.publish_event(Event::new("tns1:Device/HardwareFailure/StorageFailure"));
    events.publish_event(Event::new("tns1:VideoAnalytics/LineDetector/Crossed"));

    let (_, body) = pull(port, &concrete, "PT0S", 10).await;
    let topics: Vec<&str> = body
        .split("<wsnt:Topic>")
        .skip(1)
        .map(|r| r.split('<').next().unwrap_or(""))
        .collect();
    assert_eq!(
        topics,
        ["tns1:VideoSource/MotionAlarm"],
        "Concrete filter leaked"
    );

    let (_, body) = pull(port, &wildcard, "PT0S", 10).await;
    assert_eq!(
        body.matches("<wsnt:NotificationMessage>").count(),
        2,
        "ConcreteSet filter:\n{body}"
    );

    let (_, body) = pull(port, &unfiltered, "PT0S", 10).await;
    assert_eq!(
        body.matches("<wsnt:NotificationMessage>").count(),
        3,
        "unfiltered subscriber"
    );

    // Unsupported dialect faults instead of being silently ignored.
    let (status, _body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        &create_with_filter(
            "http://www.w3.org/TR/1999/REC-xpath-19991116",
            "tns1:VideoSource",
        ),
        true,
    )
    .await;
    assert_ne!(status, 200, "unsupported dialect accepted");

    handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Auth policy (parity with onvif-go's DefaultAuthPolicy on these actions)
// ---------------------------------------------------------------------------

/// CreatePullPointSubscription (Create* prefix) is protected; the
/// read-style events actions stay open.
#[tokio::test]
async fn events_auth_policy() {
    let (port, _events, mut handle) = events_server().await;

    // Reads are open (Get* — no credentials needed).
    let (status, _) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        "<GetEventProperties xmlns=\"http://www.onvif.org/ver10/events/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200, "GetEventProperties must be pre-auth");

    // Create* requires credentials.
    let (status, body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        "<CreatePullPointSubscription xmlns=\"http://www.onvif.org/ver10/events/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 401, "unauthenticated create must 401:\n{body}");

    // And bad credentials are still refused.
    let bad = post_soap_bad_credentials(port).await;
    assert_eq!(bad, 401, "wrong password must 401");

    handle.shutdown().await.expect("shutdown");
}

async fn post_soap_bad_credentials(port: u16) -> u16 {
    let envelope = "<Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\">\
<Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\">\
<UsernameToken><Username>admin</Username><Password>wrong</Password></UsernameToken></Security></Header>\
<Body><CreatePullPointSubscription xmlns=\"http://www.onvif.org/ver10/events/wsdl\"/></Body></Envelope>";
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let request = format!(
        "POST {EVENTS_SERVICE_PATH} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{envelope}",
        envelope.len()
    );
    sock.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.expect("read");
    String::from_utf8_lossy(&raw)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Routing guards
// ---------------------------------------------------------------------------

/// With the service disabled the events paths have no route — 404 (the
/// historical behavior of every other path is untouched).
#[tokio::test]
async fn events_paths_404_when_disabled() {
    let mut server = OnvifServer::new(&base_config(false));
    let device = Arc::new(
        DeviceServiceHandlers::new(test_device_config(), 0, "127.0.0.1".to_string())
            .expect("valid identity"),
    );
    server.register_anonymous_action("GetSystemDateAndTime");
    server.register_handler(
        "GetSystemDateAndTime",
        Box::new(DeviceHandler(Arc::clone(&device))),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let mut handle = server.start_on(listener).await.expect("start");

    let (status, _body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        "<GetServiceCapabilities xmlns=\"http://www.onvif.org/ver10/events/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 404, "disabled events service must not serve");

    // Device actions on their usual paths keep working unchanged.
    let (status, body) = post_soap(
        port,
        "/onvif/device_service",
        "<GetSystemDateAndTime xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200, "default routing must be unchanged:\n{body}");
    assert!(body.contains("GetSystemDateAndTimeResponse"));

    handle.shutdown().await.expect("shutdown");
}

/// The default action map keeps serving events-service action names on
/// non-events paths is NOT possible — but the reverse guard matters: a
/// regular action on the events path is an unsupported action.
#[tokio::test]
async fn regular_action_on_events_path_is_unsupported() {
    let (port, _events, mut handle) = events_server().await;

    let (status, body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        "<GetProfiles xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 400, "media action on the events endpoint:\n{body}");
    assert!(body.contains("unsupported action: GetProfiles"));

    handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Advertisement (GetCapabilities / GetServices agree with the routes)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn capabilities_and_services_advertise_events() {
    let (port, _events, mut handle) = events_server().await;

    let (status, body) = post_soap(
        port,
        "/onvif/device_service",
        "<GetCapabilities xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        body.contains("<tt:Events WSSubscriptionPolicySupport=\"false\" WSPullPointSupport=\"true\" WSPausableSubscriptionManagerInterfaceSupport=\"false\">"),
        "Events capability block missing:\n{body}"
    );
    assert!(
        body.contains(&format!("http://127.0.0.1:{port}/onvif/events_service")),
        "Events XAddr missing:\n{body}"
    );

    let (status, body) = post_soap(
        port,
        "/onvif/device_service",
        "<GetServices xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        body.contains("http://www.onvif.org/ver10/events/wsdl"),
        "events namespace missing from GetServices:\n{body}"
    );
    assert!(
        body.contains(&format!("http://127.0.0.1:{port}/onvif/events_service")),
        "events XAddr missing from GetServices:\n{body}"
    );

    handle.shutdown().await.expect("shutdown");
}

/// `with_events_support(false)` (the default) keeps the advertisement
/// absent — existing deployments' bytes unchanged.
#[tokio::test]
async fn advertisement_absent_without_events_support() {
    let mut server = OnvifServer::new(&base_config(true));
    let device = Arc::new(
        DeviceServiceHandlers::new(test_device_config(), 0, "127.0.0.1".to_string())
            .expect("valid identity"),
    );
    for action in ["GetCapabilities", "GetServices"] {
        server.register_anonymous_action(action);
        server.register_handler(action, Box::new(DeviceHandler(Arc::clone(&device))));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let mut handle = server.start_on(listener).await.expect("start");

    let (_status, caps) = post_soap(
        port,
        "/onvif/device_service",
        "<GetCapabilities xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>",
        false,
    )
    .await;
    assert!(
        !caps.contains("tt:Events"),
        "Events advertised while unsupported:\n{caps}"
    );

    let (_status, svcs) = post_soap(
        port,
        "/onvif/device_service",
        "<GetServices xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>",
        false,
    )
    .await;
    assert!(
        !svcs.contains("http://www.onvif.org/ver10/events/wsdl"),
        "events service advertised while unsupported:\n{svcs}"
    );

    handle.shutdown().await.expect("shutdown");
}
