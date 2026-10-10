//! PTZ service self-check demo.
//!
//! Starts the real ONVIF SOAP server with the PTZ handler registered for
//! every verb, runs a simulated-motion tick loop against the shared
//! `PtzState`, then drives it like a foreign NVR:
//!
//! 1. `AbsoluteMove` + `GetStatus` — position applied exactly
//! 2. `ContinuousMove` — position drifts while the tick loop advances time
//! 3. `Stop` — motion freezes, MoveStatus goes IDLE
//! 4. `SetPreset` / `GotoPreset` / `GetPresets` / `RemovePreset` — full
//!    preset round-trip over the wire
//! 5. Unknown PTZ verb — SOAP fault (ActionNotSupported)
//!
//! Every request carries a WS-Security UsernameToken digest, so the demo
//! doubles as a smoke test of authenticated PTZ traffic. Exits 0 when all
//! checks pass. `--serve` keeps the server up for manual poking.
//!
//! Run: `cargo run --example ptz_demo`

use anyhow::{bail, Context, Result};
use base64::Engine;
use rand::RngCore;
use sha1::{Digest, Sha1};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use onvif_device_rs::ptz::PtzHandler;
use onvif_device_rs::ptz_state::PtzState;
use onvif_device_rs::server::{OnvifConfig, OnvifServer};

const USERNAME: &str = "admin";
const PASSWORD: &str = "12345678";

// ---------------------------------------------------------------------------
// Minimal ONVIF client (UsernameToken digest, like a foreign NVR)
// ---------------------------------------------------------------------------

fn utc_now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
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

fn soap_envelope(action: &str, inner_body: &str) -> String {
    let mut nonce_raw = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut nonce_raw);
    let nonce = base64::engine::general_purpose::STANDARD.encode(nonce_raw);
    let created = utc_now_iso8601();
    // ONVIF digest: BASE64(SHA1(nonce_raw + created + password))
    let mut hasher = Sha1::new();
    hasher.update(nonce_raw);
    hasher.update(created.as_bytes());
    hasher.update(PASSWORD.as_bytes());
    let digest = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\">\
         <s:Header>\
         <Security s:mustUnderstand=\"1\" xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\">\
         <UsernameToken>\
         <Username>{USERNAME}</Username>\
         <Password Type=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest\">{digest}</Password>\
         <Nonce EncodingType=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary\">{nonce}</Nonce>\
         <Created>{created}</Created>\
         </UsernameToken></Security>\
         </s:Header>\
         <s:Body><{action} xmlns=\"http://www.onvif.org/ver20/ptz/wsdl/\">{inner_body}</{action}>\
         </s:Body>\
         </s:Envelope>"
    )
}

async fn soap_call(port: u16, action: &str, inner_body: &str) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .with_context(|| format!("connect SOAP endpoint 127.0.0.1:{port}"))?;
    let body = soap_envelope(action, inner_body);
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

/// Extract the text of the first `<tag ...>` element, matching the exact
/// tag name (a prefix like `<tt:Pan` must not match `<tt:PantiltXyz`).
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

/// Extract an attribute value from the first `<...tag attr="value">`.
fn xml_attr(xml: &str, tag: &str, attr: &str) -> f64 {
    let open = format!("<{tag}");
    let mut from = 0;
    for _ in 0..xml.len() {
        let Some(rel) = xml[from..].find(&open) else {
            break;
        };
        let after = from + rel + open.len();
        let next = xml[after..].chars().next().unwrap_or('>');
        if next == '>' || next == ' ' || next == '/' {
            let end = xml[after..].find('>').map_or(xml.len(), |g| after + g);
            let elem = &xml[after..end];
            let needle = format!("{attr}=\"");
            if let Some(a) = elem.find(&needle) {
                let vstart = a + needle.len();
                let vend = elem[vstart..].find('"').map_or(elem.len(), |e| vstart + e);
                return elem[vstart..vend].parse().unwrap_or(f64::NAN);
            }
            break;
        }
        from = after;
    }
    f64::NAN
}

fn approx(a: f64, b: f64, eps: f64) -> bool {
    (a - b).abs() <= eps
}

// ---------------------------------------------------------------------------
// Demo
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    println!("== onvif-device-rs PTZ demo: move / status / presets self-check ==\n");

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

    // -- server side: one shared PtzState for all verbs ----------------------
    let state = Arc::new(PtzState::new());
    let mut soap = OnvifServer::new(&OnvifConfig {
        port,
        username: USERNAME.to_string(),
        password: PASSWORD.to_string(),
        ..Default::default()
    });
    for action in [
        "ContinuousMove",
        "AbsoluteMove",
        "RelativeMove",
        "Stop",
        "GetStatus",
        "GetPresets",
        "SetPreset",
        "GotoPreset",
        "RemovePreset",
        "GetNodes",
        "GetConfigurations",
    ] {
        soap.register_handler(action, Box::new(PtzHandler(Arc::clone(&state))));
    }
    println!("[server] ONVIF SOAP + PTZ service on tcp/{port}");
    tokio::spawn(soap.start());
    tokio::time::sleep(Duration::from_millis(300)).await;

    // -- simulated motor: advance PtzState physics every 50 ms ---------------
    let ticker = Arc::clone(&state);
    let motion = tokio::spawn(async move {
        loop {
            ticker.tick(50);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    // 1. AbsoluteMove — the state eases toward the target over up to 20
    //    ticks (~1 s at the demo's 50 ms tick), then snaps. Wait it out.
    let inner = "<ProfileToken>profile_1</ProfileToken>\
                 <Position><PanTilt x=\"0.8\" y=\"0.2\" space=\"a\"/><Zoom x=\"0.5\" space=\"b\"/></Position>";
    let (code, body) = soap_call(port, "AbsoluteMove", inner).await?;
    if code != 200 {
        bail!("AbsoluteMove failed: HTTP {code}: {body}");
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (code, body) =
        soap_call(port, "GetStatus", "<ProfileToken>profile_1</ProfileToken>").await?;
    if code != 200 || !body.contains("GetStatusResponse") {
        bail!("GetStatus failed: HTTP {code}: {body}");
    }
    let (x, y, zoom) = (
        xml_attr(&body, "tt:PanTilt", "x"),
        xml_attr(&body, "tt:PanTilt", "y"),
        xml_attr(&body, "tt:Zoom", "x"),
    );
    if std::env::var_os("PTZ_DEMO_DEBUG").is_some() {
        eprintln!("-- GetStatus body --\n{body}\n-- parsed ({x}, {y}, {zoom}) --");
    }
    if !(approx(x, 0.8, 0.001) && approx(y, 0.2, 0.001) && approx(zoom, 0.5, 0.001)) {
        bail!("AbsoluteMove position not applied: ({x}, {y}, zoom {zoom})");
    }
    println!("[client] AbsoluteMove(0.8, 0.2, z=0.5) -> GetStatus agrees");

    // 2. ContinuousMove — drifts while the tick loop runs.
    let inner = "<ProfileToken>profile_1</ProfileToken>\
                 <Velocity><PanTilt x=\"0.5\" y=\"0.0\" space=\"a\"/><Zoom x=\"0.0\" space=\"b\"/></Velocity>";
    let (code, body) = soap_call(port, "ContinuousMove", inner).await?;
    if code != 200 {
        bail!("ContinuousMove failed: HTTP {code}: {body}");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_code, body) =
        soap_call(port, "GetStatus", "<ProfileToken>profile_1</ProfileToken>").await?;
    let x_moving = xml_attr(&body, "tt:PanTilt", "x");
    if x_moving < x + 0.01 {
        bail!("ContinuousMove did not advance position: before {x}, after {x_moving}");
    }
    println!("[client] ContinuousMove(x=0.5) -> position drifts ({x:.3} -> {x_moving:.3})");

    // 3. Stop — motion freezes.
    let (code, body) = soap_call(port, "Stop", "<ProfileToken>profile_1</ProfileToken>").await?;
    if code != 200 {
        bail!("Stop failed: HTTP {code}: {body}");
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (_code, body) =
        soap_call(port, "GetStatus", "<ProfileToken>profile_1</ProfileToken>").await?;
    let x_stopped = xml_attr(&body, "tt:PanTilt", "x");
    if !approx(x_stopped, x_moving, 0.01) {
        bail!("position moved after Stop: {x_moving} -> {x_stopped}");
    }
    if !body.contains("IDLE") {
        bail!("MoveStatus should be IDLE after Stop: {body}");
    }
    println!("[client] Stop -> frozen at {x_stopped:.3}, MoveStatus IDLE");

    // 4. Presets — set, goto, list, remove.
    let (code, body) = soap_call(
        port,
        "SetPreset",
        "<ProfileToken>profile_1</ProfileToken><PresetName>gate</PresetName>",
    )
    .await?;
    if code != 200 {
        bail!("SetPreset failed: HTTP {code}: {body}");
    }
    let token = xml_field(&body, "tptz:PresetToken");
    if token.is_empty() {
        bail!("SetPreset returned no token: {body}");
    }

    // Move away, wait for the ease to complete, then GotoPreset must land
    // back on the saved position (again after the settle window).
    let inner = "<ProfileToken>profile_1</ProfileToken>\
                 <Position><PanTilt x=\"0.1\" y=\"0.1\" space=\"a\"/><Zoom x=\"0.1\" space=\"b\"/></Position>";
    soap_call(port, "AbsoluteMove", inner).await?;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (code, body) = soap_call(
        port,
        "GotoPreset",
        &format!("<ProfileToken>profile_1</ProfileToken><PresetToken>{token}</PresetToken>"),
    )
    .await?;
    if code != 200 {
        bail!("GotoPreset failed: HTTP {code}: {body}");
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (_code, body) =
        soap_call(port, "GetStatus", "<ProfileToken>profile_1</ProfileToken>").await?;
    let x_back = xml_attr(&body, "tt:PanTilt", "x");
    if !approx(x_back, x_stopped, 0.01) {
        bail!("GotoPreset did not restore position: want {x_stopped}, got {x_back}");
    }

    let (code, body) =
        soap_call(port, "GetPresets", "<ProfileToken>profile_1</ProfileToken>").await?;
    if code != 200 || xml_field(&body, "tt:Name") != "gate" {
        bail!("GetPresets must list preset 'gate': {body}");
    }
    let (_code, body) = soap_call(
        port,
        "RemovePreset",
        &format!("<ProfileToken>profile_1</ProfileToken><PresetToken>{token}</PresetToken>"),
    )
    .await?;
    if !body.contains("RemovePresetResponse") {
        bail!("RemovePreset failed: {body}");
    }
    let (_code, body) =
        soap_call(port, "GetPresets", "<ProfileToken>profile_1</ProfileToken>").await?;
    if body.contains("tt:Name") {
        bail!("preset still listed after RemovePreset: {body}");
    }
    println!("[client] SetPreset('gate') -> GotoPreset restores -> RemovePreset clears");

    // 5. Unknown verb — SOAP fault.
    let (code, body) = soap_call(
        port,
        "GetConfigurationOptions",
        "<ProfileToken>profile_1</ProfileToken>",
    )
    .await?;
    if code == 200 && !body.contains("Fault") {
        bail!("unknown PTZ verb must fault, got HTTP {code}: {body}");
    }
    println!("[client] unknown verb -> SOAP fault (as required)");

    println!("\nonvif-device-rs PTZ demo: all checks passed");
    if serve {
        println!("--serve: keeping server on tcp/{port} (Ctrl-C to quit)");
        motion.abort();
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }
    Ok(())
}
