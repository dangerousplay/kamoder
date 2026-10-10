//! Events basic-notification integration tests: the wsnt:Subscribe /
//! push-Notify half of the events service (issue #50), over a real socket
//! on an ephemeral port — mirroring the `events_pullpoint.rs` harness.
//!
//! Subscribe → Notify POSTs arrive at a real (loopback) consumer → Renew /
//! Unsubscribe share the pull-point lifecycle → dead consumers are
//! auto-unsubscribed after repeated delivery failures. Plus the events-side
//! SetSynchronizationPoint acknowledgment and the routing guards for the
//! deliberately-unimplemented GetEventInstances.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use onvif_device_rs::config::DeviceConfig;
use onvif_device_rs::device::{DeviceHandler, DeviceServiceHandlers};
use onvif_device_rs::events::{Event, SimpleItem, EVENTS_SERVICE_PATH};
use onvif_device_rs::server::{OnvifConfig, OnvifServer, OnvifServerHandle};

const USERNAME: &str = "admin";
const PASSWORD: &str = "password";

// ---------------------------------------------------------------------------
// Minimal ONVIF client + wsnt consumer (what a subscribing NVR implements)
// ---------------------------------------------------------------------------

/// POST a SOAP 1.2 envelope whose body is `body_xml` to `path`. With
/// `auth` a PasswordText UsernameToken is carried.
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

/// A wsnt consumer on an ephemeral loopback port: accepts Notify POSTs,
/// answers `status`, and forwards each captured request (status line,
/// headers, body — the whole thing) over a channel.
async fn spawn_consumer(status: u16) -> (u16, mpsc::Receiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("consumer bind");
    let port = listener.local_addr().expect("consumer addr").port();
    let (tx, rx) = mpsc::channel::<String>(16);
    let status_line = std::sync::Arc::new(format!("HTTP/1.1 {status} Delivered\r\n"));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            let status_line = Arc::clone(&status_line);
            tokio::spawn(async move {
                let mut raw = Vec::new();
                // One Notify is small; read until EOF or a full response is
                // impossible to know without parsing — Connection: close on
                // the producer side means EOF ends the request.
                let _ = sock.read_to_end(&mut raw).await;
                let captured = String::from_utf8_lossy(&raw).to_string();
                let response =
                    format!("{status_line}Content-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
                let _ = tx.send(captured).await;
            });
        }
    });
    (port, rx)
}

// ---------------------------------------------------------------------------
// Server scaffolding (mirrors events_pullpoint.rs)
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
    for action in ["GetSystemDateAndTime", "GetCapabilities"] {
        server.register_anonymous_action(action);
        server.register_handler(action, Box::new(DeviceHandler(Arc::clone(&device))));
    }

    let events = server.enable_events();
    let handle = server.start_on(listener).await.expect("start");
    (port, events, handle)
}

/// Issue a wsnt:Subscribe for `consumer_url` (termination PT10M unless
/// overridden) and return the subscription path.
async fn basic_subscribe(
    port: u16,
    consumer_url: &str,
    termination: &str,
) -> (u16, String, String) {
    let body = format!(
        "<Subscribe xmlns=\"http://docs.oasis-open.org/wsn/b-2\">\
         <ConsumerReference><wsa:Address xmlns:wsa=\"http://www.w3.org/2005/08/addressing\">{consumer_url}</wsa:Address></ConsumerReference>\
         <TerminationTime>{termination}</TerminationTime>\
         </Subscribe>"
    );
    let (status, body) = post_soap(port, EVENTS_SERVICE_PATH, &body, false).await;
    let address = xml_field(&body, "wsa:Address").to_string();
    let sub_path = format!(
        "/onvif/events_service/sub/{}",
        address.rsplit('/').next().unwrap_or("")
    );
    (status, body, sub_path)
}

// ---------------------------------------------------------------------------
// The basic-notification lifecycle
// ---------------------------------------------------------------------------

/// Subscribe answers a wsnt:SubscribeResponse whose SubscriptionReference
/// is a pull-style address — Renew/Unsubscribe operate on it, PullMessages
/// does not (it is not a pull point).
#[tokio::test]
async fn basic_subscription_lifecycle_over_the_wire() {
    let (port, _events, mut handle) = events_server().await;
    let (consumer_port, _rx) = spawn_consumer(200).await;

    let (status, body, sub_path) = basic_subscribe(
        port,
        &format!("http://127.0.0.1:{consumer_port}/notify"),
        "PT10M",
    )
    .await;
    assert_eq!(status, 200, "subscribe failed:\n{body}");
    assert!(
        body.contains("<wsnt:SubscribeResponse"),
        "wsnt root:\n{body}"
    );
    assert!(
        xml_field(&body, "wsa:Address").starts_with("http://127.0.0.1:")
            && body.contains("/onvif/events_service/sub/"),
        "SubscriptionReference address: {}",
        xml_field(&body, "wsa:Address")
    );
    assert_ne!(xml_field(&body, "wsnt:CurrentTime"), "");
    assert_ne!(xml_field(&body, "wsnt:TerminationTime"), "");

    // Renew extends the basic subscription (shared lifecycle).
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

    // PullMessages on a basic subscription is a Sender fault.
    let (status, body) = post_soap(
        port,
        &sub_path,
        "<PullMessages xmlns=\"http://www.onvif.org/ver10/events/wsdl\">\
         <Timeout>PT0S</Timeout><MessageLimit>5</MessageLimit>\
         </PullMessages>",
        false,
    )
    .await;
    assert_eq!(status, 400, "pull on basic sub must fault:\n{body}");
    assert!(
        body.contains("soap:Sender") && body.contains("not a pull point"),
        "must fault as Sender naming the kind:\n{body}"
    );

    // Unsubscribe removes it; afterwards Renew faults as unknown.
    let (status, body) = post_soap(
        port,
        &sub_path,
        "<Unsubscribe xmlns=\"http://docs.oasis-open.org/wsn/b-2\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200, "unsubscribe failed:\n{body}");
    assert!(body.contains("UnsubscribeResponse"));

    let (status, body) = post_soap(
        port,
        &sub_path,
        "<Renew xmlns=\"http://docs.oasis-open.org/wsn/b-2\">\
         <TerminationTime>PT10M</TerminationTime>\
         </Renew>",
        false,
    )
    .await;
    assert_eq!(status, 400, "renew after unsubscribe:\n{body}");
    assert!(body.contains("Unknown subscription"));

    handle.shutdown().await.expect("shutdown");
}

/// publish_event fans out to a basic subscription as wsnt:Notify POSTs to
/// the consumer address: one NotificationMessage per POST, with the
/// subscription reference (no producer reference), the canonical
/// double-layer payload, and the same inner bytes the pull-point writes.
#[tokio::test]
async fn push_notify_delivered_to_consumer() {
    let (port, events, mut handle) = events_server().await;
    let (consumer_port, mut rx) = spawn_consumer(200).await;

    let (status, body, _sub_path) = basic_subscribe(
        port,
        &format!("http://127.0.0.1:{consumer_port}/notify"),
        "PT10M",
    )
    .await;
    assert_eq!(status, 200, "subscribe failed:\n{body}");
    let sub_address = xml_field(&body, "wsa:Address").to_string();

    events.publish_event(Event {
        topic: "tns1:VideoSource/MotionAlarm".into(),
        source: vec![SimpleItem::new("Source", "CSI")],
        data: vec![
            SimpleItem::new("State", "true"),
            SimpleItem::new("Score", "87"),
        ],
        ..Event::new("tns1:VideoSource/MotionAlarm")
    });

    let captured = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("notify within timeout")
        .expect("captured request");

    // The POST shape: method, path, SOAP content type, exact length.
    assert!(
        captured.starts_with("POST /notify HTTP/1.1\r\n"),
        "request line:\n{captured}"
    );
    assert!(
        captured
            .to_ascii_lowercase()
            .contains("content-type: application/soap+xml"),
        "content type:\n{captured}"
    );
    let headers_end = captured.find("\r\n\r\n").map_or(0, |p| p + 4);
    let notify = captured[headers_end..].to_string();
    let declared_len: usize = captured
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_default();
    assert_eq!(
        declared_len,
        notify.len(),
        "Content-Length must match the body"
    );

    // The payload: standard envelope, wsnt:Notify, subscription reference
    // (no producer reference), the canonical double-layer message.
    assert!(notify.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
    assert!(notify.contains("<soap:Envelope"));
    assert!(notify.contains("<wsnt:Notify"));
    assert_eq!(notify.matches("<wsnt:NotificationMessage>").count(), 1);
    assert_eq!(
        xml_field(&notify, "wsnt:Topic"),
        "tns1:VideoSource/MotionAlarm"
    );
    assert!(
        !notify.contains("ProducerReference"),
        "producer ref omitted"
    );
    let sub_ref = notify
        .split("<wsnt:SubscriptionReference>")
        .nth(1)
        .and_then(|r| r.split("</wsnt:SubscriptionReference>").next())
        .unwrap_or_default();
    assert!(
        sub_ref.contains(&sub_address),
        "Notify must reference the subscription address {sub_address}:\n{sub_ref}"
    );
    let tt_message = notify
        .split("<tt:Message ")
        .nth(1)
        .and_then(|rest| rest.split("</tt:Message>").next())
        .unwrap_or_default();
    assert!(
        tt_message.contains("PropertyOperation=\"Changed\"") && tt_message.contains("UtcTime=\""),
        "stamped inner message:\n{tt_message}"
    );
    assert!(
        tt_message.contains("<tt:SimpleItem Name=\"Source\" Value=\"CSI\"/>")
            && tt_message.contains("<tt:SimpleItem Name=\"State\" Value=\"true\"/>")
            && tt_message.contains("<tt:SimpleItem Name=\"Score\" Value=\"87\"/>"),
        "SimpleItem groups:\n{tt_message}"
    );

    // A second publish → a second Notify POST (one message per POST).
    events.publish_event(Event::new("tns1:VideoSource/SignalLoss"));
    let captured2 = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("second notify within timeout")
        .expect("captured request 2");
    assert!(captured2.contains("tns1:VideoSource/SignalLoss"));

    handle.shutdown().await.expect("shutdown");
}

/// The subscription's topic filter applies to push delivery exactly like
/// it applies to pull delivery (Concrete and ConcreteSet honored).
#[tokio::test]
async fn push_notify_respects_topic_filter() {
    let (port, events, mut handle) = events_server().await;
    let (consumer_port, mut rx) = spawn_consumer(200).await;

    let body = format!(
        "<Subscribe xmlns=\"http://docs.oasis-open.org/wsn/b-2\">\
         <ConsumerReference><wsa:Address>http://127.0.0.1:{consumer_port}/notify</wsa:Address></ConsumerReference>\
         <Filter><wsnt:TopicExpression xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\" Dialect=\"http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet\">tns1:VideoSource/*</wsnt:TopicExpression></Filter>\
         <TerminationTime>PT10M</TerminationTime>\
         </Subscribe>"
    );
    let (status, body) = post_soap(port, EVENTS_SERVICE_PATH, &body, false).await;
    assert_eq!(status, 200, "filtered subscribe:\n{body}");

    events.publish_event(Event::new("tns1:VideoSource/MotionAlarm")); // matches
    events.publish_event(Event::new("tns1:Device/HardwareFailure")); // filtered out
    events.publish_event(Event::new("tns1:VideoSource/SignalLoss")); // matches

    let mut topics = Vec::new();
    while let Ok(captured) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
        if let Some(c) = captured {
            topics.push(xml_field(&c, "wsnt:Topic").to_string());
        }
    }
    assert_eq!(
        topics,
        [
            "tns1:VideoSource/MotionAlarm",
            "tns1:VideoSource/SignalLoss"
        ],
        "push filter leaked or dropped"
    );

    handle.shutdown().await.expect("shutdown");
}

/// A dead consumer gets the subscription auto-unsubscribed after three
/// consecutive delivery failures (spec-permissible housekeeping); later
/// Renew faults as unknown.
#[tokio::test]
async fn dead_consumer_auto_unsubscribes() {
    let (port, events, mut handle) = events_server().await;

    // Reserve then release a loopback port: connections are refused.
    let spare = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("spare bind");
    let dead_port = spare.local_addr().expect("addr").port();
    drop(spare);

    let (status, body, sub_path) = basic_subscribe(
        port,
        &format!("http://127.0.0.1:{dead_port}/notify"),
        "PT10M",
    )
    .await;
    assert_eq!(status, 200, "subscribe failed:\n{body}");

    for i in 0..3 {
        events.publish_event(Event::new(&format!("tns1:Counter/Tick{i}")));
    }

    // The three failures are processed asynchronously; poll for the
    // auto-unsubscribe to land (Renew starts faulting as unknown).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut removed = false;
    while tokio::time::Instant::now() < deadline {
        let (status, body) = post_soap(
            port,
            &sub_path,
            "<Renew xmlns=\"http://docs.oasis-open.org/wsn/b-2\">\
             <TerminationTime>PT10M</TerminationTime>\
             </Renew>",
            false,
        )
        .await;
        if status == 400 && body.contains("Unknown subscription") {
            removed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        removed,
        "subscription must be auto-unsubscribed after 3 failures"
    );

    handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// SetSynchronizationPoint (events side) + GetEventInstances guard
// ---------------------------------------------------------------------------

/// SetSynchronizationPoint is a shared action name with media — the events
/// service serves it on its own endpoint (route-based dispatch); it acks
/// empty. The Set* prefix keeps it credential-protected.
#[tokio::test]
async fn set_synchronization_point_ack_and_auth() {
    let (port, _events, mut handle) = events_server().await;

    // Protected: credentials required (Set* prefix policy).
    let (status, body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        "<SetSynchronizationPoint xmlns=\"http://www.onvif.org/ver10/events/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 401, "unauthenticated sync point:\n{body}");

    let (status, body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        "<SetSynchronizationPoint xmlns=\"http://www.onvif.org/ver10/events/wsdl\"/>",
        true,
    )
    .await;
    assert_eq!(status, 200, "sync point failed:\n{body}");
    assert_eq!(
        xml_field(&body, "tev:SetSynchronizationPointResponse"),
        "",
        "empty ack:\n{body}"
    );
    assert!(body.contains("tev:SetSynchronizationPointResponse"));

    handle.shutdown().await.expect("shutdown");
}

/// GetEventInstances is deliberately not implemented (17.06+ feature,
/// zero demand) — the existing unknown-action fault answers it.
#[tokio::test]
async fn get_event_instances_is_unsupported() {
    let (port, _events, mut handle) = events_server().await;

    let (status, body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        "<GetEventInstances xmlns=\"http://www.onvif.org/ver10/events/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 400, "GetEventInstances:\n{body}");
    assert!(
        body.contains("unsupported action: GetEventInstances"),
        "unknown-action fault:\n{body}"
    );

    handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Auth policy + shared cap
// ---------------------------------------------------------------------------

/// Subscribe stays open under the default prefix policy (parity with
/// onvif-go's DefaultAuthPolicy: only Set/Remove/Create/Go protected).
#[tokio::test]
async fn subscribe_auth_policy() {
    let (port, _events, mut handle) = events_server().await;
    let (consumer_port, _rx) = spawn_consumer(200).await;

    let (status, body, _) = basic_subscribe(
        port,
        &format!("http://127.0.0.1:{consumer_port}/notify"),
        "PT10M",
    )
    .await;
    assert_eq!(status, 200, "Subscribe is pre-auth:\n{body}");

    // ...but a https consumer is refused regardless of credentials.
    let (status, body) = post_soap(
        port,
        EVENTS_SERVICE_PATH,
        "<Subscribe xmlns=\"http://docs.oasis-open.org/wsn/b-2\">\
         <ConsumerReference><wsa:Address>https://consumer.example/notify</wsa:Address></ConsumerReference>\
         </Subscribe>",
        true,
    )
    .await;
    assert_eq!(status, 400, "https consumer refused:\n{body}");
    assert!(body.contains("http://"));

    handle.shutdown().await.expect("shutdown");
}

/// Basic subscriptions share the pull-point cap: ten live pull points and
/// the eleventh subscription — a basic one — faults.
#[tokio::test]
async fn basic_subscriptions_share_the_cap_over_the_wire() {
    let (port, _events, mut handle) = events_server().await;
    let (consumer_port, _rx) = spawn_consumer(200).await;

    for _ in 0..10 {
        let (status, body) = post_soap(
            port,
            EVENTS_SERVICE_PATH,
            "<CreatePullPointSubscription xmlns=\"http://www.onvif.org/ver10/events/wsdl\"/>",
            true,
        )
        .await;
        assert_eq!(status, 200, "pull create:\n{body}");
    }

    let (status, body, _sub_path) = basic_subscribe(
        port,
        &format!("http://127.0.0.1:{consumer_port}/notify"),
        "PT10M",
    )
    .await;
    assert_eq!(status, 400, "beyond shared cap:\n{body}");
    assert!(
        body.contains("Too many active subscriptions"),
        "cap fault:\n{body}"
    );

    handle.shutdown().await.expect("shutdown");
}
