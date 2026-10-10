//! ONVIF device demo — a complete self-check in one process.
//!
//! Starts the real [`onvif_device_rs::server::OnvifServer`] (Device + Media
//! services) and a [`onvif_device_rs::discovery::DiscoveryServer`], then drives
//! them exactly like a foreign ONVIF client would:
//!
//! 1. `GetSystemDateAndTime` — pre-auth (anonymous) per the ONVIF Core spec
//! 2. `GetDeviceInformation` **without** credentials — must be rejected 401
//! 3. `GetDeviceInformation` **with** WS-Security UsernameToken digest
//!    (computed here with an independent SHA1 implementation)
//! 4. `GetCapabilities` (anonymous)
//! 5. `GetProfiles` / `GetStreamUri` (authenticated) — profile and RTSP URI
//! 6. WS-Discovery Probe over UDP — ProbeMatches XAddrs
//!
//! Exits 0 when every check passes. With `--serve` the servers stay up for
//! manual poking (curl, ONVIF Device Manager, `wsdiscover`):
//!
//! ```sh
//! cargo run --example device_demo [-- --port 8080] [-- --serve]
//! ```

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use rand::RngCore;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

use onvif_device_rs::config::DeviceConfig;
use onvif_device_rs::device::{DeviceHandler, DeviceServiceHandlers};
use onvif_device_rs::discovery::DiscoveryServer;
use onvif_device_rs::media::{
    GetProfilesHandler, GetSnapshotUriHandler, GetStreamUriHandler, OnvifMediaConfig, VideoEncoding,
};
use onvif_device_rs::server::{OnvifConfig, OnvifServer};

const USERNAME: &str = "admin";
const PASSWORD: &str = "12345678";
const SERIAL: &str = "DEMO-0001";

// ---------------------------------------------------------------------------
// Minimal ONVIF client (what a foreign NVR implements)
// ---------------------------------------------------------------------------

fn utc_now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Days-from-civil algorithm (Howard Hinnant) — no chrono dependency.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn soap_envelope(action: &str, auth: bool) -> String {
    let header = if auth {
        let mut nonce_raw = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut nonce_raw);
        let nonce = B64.encode(nonce_raw);
        let created = utc_now_iso8601();
        // ONVIF digest: BASE64(SHA1(nonce_raw + created + password))
        let mut hasher = Sha1::new();
        hasher.update(nonce_raw);
        hasher.update(created.as_bytes());
        hasher.update(PASSWORD.as_bytes());
        let digest = B64.encode(hasher.finalize());
        format!(
            "<s:Header>\
             <Security s:mustUnderstand=\"1\" xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\">\
             <UsernameToken>\
             <Username>{USERNAME}</Username>\
             <Password Type=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest\">{digest}</Password>\
             <Nonce EncodingType=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary\">{nonce}</Nonce>\
             <Created>{created}</Created>\
             </UsernameToken></Security>\
             </s:Header>"
        )
    } else {
        String::new()
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\">{header}\
         <s:Body><{action} xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/></s:Body>\
         </s:Envelope>"
    )
}

async fn soap_call(port: u16, action: &str, auth: bool) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .with_context(|| format!("connect SOAP endpoint 127.0.0.1:{port}"))?;
    let body = soap_envelope(action, auth);
    let request = format!(
        "POST /onvif HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/soap+xml; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .context("no HTTP status line")?;
    let body_start = text.find("\r\n\r\n").map(|p| p + 4).unwrap_or(text.len());
    Ok((status, text[body_start..].to_string()))
}

async fn discovery_probe(port: u16) -> Result<String> {
    let probe = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" \
         xmlns:a=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\">\
         <s:Header>\
         <a:Action>http://schemas.xmlsoap.org/ws/2004/09/discovery/Probe</a:Action>\
         <a:MessageID>urn:uuid:demo-{port}</a:MessageID>\
         </s:Header>\
         <s:Body><Probe xmlns=\"http://schemas.xmlsoap.org/ws/2004/09/discovery\"/></s:Body>\
         </s:Envelope>"
    );
    let sock = UdpSocket::bind("127.0.0.1:0").await?;
    sock.send_to(probe.as_bytes(), "127.0.0.1:3702").await?;
    let mut buf = vec![0u8; 8192];
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut buf))
        .await
        .context("ProbeMatches timeout")?
        .context("ProbeMatches recv")?;
    Ok(String::from_utf8_lossy(&buf[..n]).to_string())
}

/// Extract the text of the first `<tag ...>` element, matching the exact
/// tag name (a prefix like `<tt:Day` must not match `<tt:DaylightSavings`).
fn xml_field(xml: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let mut from = 0;
    while let Some(rel) = xml[from..].find(&open) {
        let after = from + rel + open.len();
        let next = xml[after..].chars().next().unwrap_or('>');
        if next == '>' || next == ' ' || next == '/' {
            let start = xml[after..].find('>').map_or(after, |g| after + g + 1);
            let end = xml[start..].find("</").map_or(start, |e| start + e);
            return xml[start..end].trim().to_string();
        }
        from = after;
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Demo
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    println!("== onvif-device-rs device demo: SOAP + WS-Discovery self-check ==\n");

    let mut port: u16 = 8080;
    let mut serve = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .context("--port needs a number")?;
            }
            "--serve" => serve = true,
            other => bail!("unknown argument: {other}"),
        }
    }

    // -- server side (the wiring a host like notebook-cam does) -----------
    let device_ip = "127.0.0.1".to_string();
    let mut soap = OnvifServer::new(&OnvifConfig {
        port,
        username: USERNAME.to_string(),
        password: PASSWORD.to_string(),
        ..Default::default()
    });

    // Host-side effects for the Device write operations (issue #49): the
    // demo only logs them — a real host re-clocks, reboots, wipes, …
    struct DemoHooks;
    impl onvif_device_rs::device::DeviceHooks for DemoHooks {
        fn set_date_time(&self, utc: (i32, i32, i32, i32, i32, i32), tz: &str) {
            println!("device service: SetSystemDateAndTime {utc:?} tz={tz}");
        }
        fn reboot(&self) {
            println!("device service: SystemReboot confirmed by host");
        }
        fn factory_default(&self, hard: bool) {
            println!("device service: SetSystemFactoryDefault hard={hard}");
        }
        fn system_log(&self) -> String {
            "demo system log".into()
        }
        fn support_info(&self) -> String {
            "demo support information".into()
        }
    }

    let device = Arc::new(
        DeviceServiceHandlers::new(
            DeviceConfig {
                name: "onvif-device-rs demo camera".into(),
                manufacturer: "MiBee".into(),
                model: "DEMO".into(),
                firmware: "0.2.1".into(),
                hardware_id: "demo-hw".into(),
                serial_number: SERIAL.into(),
            },
            port,
            device_ip.clone(),
        )
        .expect("device identity must be configured explicitly (issue #20)")
        .with_hooks(std::sync::Arc::new(DemoHooks)),
    );
    // The full Device service action surface (issue #49). Everything not
    // in the anonymous set below stays behind WS-Security authentication.
    for action in [
        "GetSystemDateAndTime",
        "SetSystemDateAndTime",
        "GetDeviceInformation",
        "GetCapabilities",
        "GetServices",
        "GetServiceCapabilities",
        "GetScopes",
        "AddScopes",
        "RemoveScopes",
        "SetScopes",
        "GetHostname",
        "SetHostname",
        "GetDNS",
        "GetNTP",
        "GetNetworkInterfaces",
        "GetNetworkDefaultGateway",
        "GetNetworkProtocols",
        "GetDiscoveryMode",
        "SetDiscoveryMode",
        "GetUsers",
        "CreateUsers",
        "DeleteUsers",
        "SetUser",
        "GetWsdlUrl",
        "GetEndpointReference",
        "GetSystemLog",
        "GetSystemSupportInformation",
        "SetSystemFactoryDefault",
        "UpgradeSystemFirmware",
        "StartSystemRestore",
        "SystemReboot",
    ] {
        soap.register_handler(action, Box::new(DeviceHandler(Arc::clone(&device))));
    }
    for action in ["GetSystemDateAndTime", "GetCapabilities", "GetServices"] {
        soap.register_anonymous_action(action);
    }

    let media = Arc::new(OnvifMediaConfig {
        camera_width: 1280,
        camera_height: 720,
        camera_fps: 25,
        camera_bitrate: 2_500_000,
        rtsp_port: 8554,
        device_ip: device_ip.clone(),
        stream_path: "/stream".to_string(),
        // Advertise a snapshot endpoint. The demo doesn't serve the JPEG
        // itself; hosts with a real snapshot server point this at their port.
        snapshot_port: 8080,
        snapshot_path: "/snapshot.jpg".to_string(),
        profile_token: "main".to_string(),
        video_source_token: "videoSrc0".to_string(),
        encoder_token: "enc0".to_string(),
        encoding: VideoEncoding::H264,
        video_source_name: "Video Source".to_string(),
        extra_profiles: Vec::new(),
    });
    soap.register_handler(
        "GetProfiles",
        Box::new(GetProfilesHandler::new(Arc::clone(&media))),
    );
    soap.register_handler(
        "GetStreamUri",
        Box::new(GetStreamUriHandler::new(Arc::clone(&media))),
    );
    soap.register_handler(
        "GetSnapshotUri",
        Box::new(GetSnapshotUriHandler::new(Arc::clone(&media))),
    );

    match DiscoveryServer::new(&device_ip, port).start().await {
        Ok(_handle) => println!("[server] WS-Discovery responder on udp/3702"),
        Err(e) => println!("[server] WS-Discovery not started ({e}) — probe check will be skipped"),
    }
    println!("[server] ONVIF SOAP service on tcp/{port} (device {SERIAL})");
    let _soap_handle = tokio::spawn(soap.start());
    tokio::time::sleep(Duration::from_millis(300)).await;

    // -- client side checks -------------------------------------------------
    let (code, body) = soap_call(port, "GetSystemDateAndTime", false).await?;
    if code != 200 {
        bail!("GetSystemDateAndTime must be pre-auth, got HTTP {code}");
    }
    let date = format!(
        "{}-{}-{} {}:{}:{}",
        xml_field(&body, "tt:Year"),
        xml_field(&body, "tt:Month"),
        xml_field(&body, "tt:Day"),
        xml_field(&body, "tt:Hour"),
        xml_field(&body, "tt:Minute"),
        xml_field(&body, "tt:Second")
    );
    println!("[client] GetSystemDateAndTime (anonymous) -> {date} UTC");

    let (code, body) = soap_call(port, "GetDeviceInformation", false).await?;
    if code != 401 {
        bail!("unauthenticated GetDeviceInformation must 401, got HTTP {code}: {body}");
    }
    println!(
        "[client] GetDeviceInformation without credentials -> HTTP 401 (rejected, as required)"
    );

    let (code, body) = soap_call(port, "GetDeviceInformation", true).await?;
    if code != 200 {
        bail!("digest-authenticated GetDeviceInformation failed: HTTP {code}: {body}");
    }
    let model = xml_field(&body, "tds:Model");
    let serial = xml_field(&body, "tds:SerialNumber");
    if serial != SERIAL {
        bail!("SerialNumber mismatch: got {serial:?} want {SERIAL:?}");
    }
    println!(
        "[client] GetDeviceInformation (UsernameToken digest) -> model={model} serial={serial}"
    );

    let (code, body) = soap_call(port, "GetCapabilities", false).await?;
    if code != 200 || !body.contains("Capabilities") {
        bail!("GetCapabilities failed: HTTP {code}");
    }
    println!("[client] GetCapabilities (anonymous) -> OK");

    let (code, body) = soap_call(port, "GetProfiles", true).await?;
    if code != 200 {
        bail!("GetProfiles failed: HTTP {code}: {body}");
    }
    if !body.contains("<Width>1280</Width>") || !body.contains("<Height>720</Height>") {
        bail!("GetProfiles must report 1280x720");
    }
    println!("[client] GetProfiles -> 1280x720 @ 25 fps");

    let (code, body) = soap_call(port, "GetStreamUri", true).await?;
    if code != 200 {
        bail!("GetStreamUri failed: HTTP {code}: {body}");
    }
    let uri = xml_field(&body, "Uri");
    let expected = "rtsp://127.0.0.1:8554/stream".to_string();
    if uri != expected {
        bail!("GetStreamUri got {uri:?} want {expected:?}");
    }
    println!("[client] GetStreamUri -> {uri}");

    let (code, body) = soap_call(port, "GetSnapshotUri", true).await?;
    if code != 200 {
        bail!("GetSnapshotUri failed: HTTP {code}: {body}");
    }
    let snap = xml_field(&body, "Uri");
    let expected_snap = "http://127.0.0.1:8080/snapshot.jpg".to_string();
    if snap != expected_snap {
        bail!("GetSnapshotUri got {snap:?} want {expected_snap:?}");
    }
    println!("[client] GetSnapshotUri -> {snap}");

    match discovery_probe(port).await {
        Ok(matches) => {
            let xaddr = xml_field(&matches, "d:XAddrs");
            if !xaddr.contains(&format!("127.0.0.1:{port}")) {
                bail!("ProbeMatches XAddrs must point at this server, got {xaddr:?}");
            }
            println!("[client] WS-Discovery Probe -> ProbeMatches XAddrs = {xaddr}");
        }
        Err(e) => {
            if serve {
                println!("[client] WS-Discovery probe skipped ({e})");
            } else {
                return Err(e);
            }
        }
    }

    println!("\nonvif-device-rs device demo: all checks passed");
    if serve {
        println!(
            "\nserving for 10 minutes — try:\n\
             curl -s http://127.0.0.1:{port}/onvif -H 'Content-Type: application/soap+xml' \
             --data '<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body>\
             <GetSystemDateAndTime xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/></s:Body></s:Envelope>'\n\
             or point ONVIF Device Manager / an NVR at udp/3702"
        );
        tokio::time::sleep(Duration::from_secs(600)).await;
    }
    Ok(())
}
