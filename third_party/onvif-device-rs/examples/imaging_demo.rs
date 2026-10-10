//! Imaging service self-check demo.
//!
//! Starts the real ONVIF SOAP server with the three Imaging actions bound to
//! an in-memory [`ImagingParams`] store, then drives it like a foreign NVR:
//!
//! 1. `GetImagingSettings` — defaults reported over the wire
//! 2. `SetImagingSettings` — brightness/contrast written through the seam
//! 3. `GetImagingSettings` — the new values read back
//! 4. Out-of-range value — SOAP fault (the seam rejects it)
//!
//! The in-memory store stands in for a host's V4L2 parameter manager —
//! swap in your own `ImagingParams` implementation and everything else
//! is identical. Exits 0 when all checks pass; `--serve` keeps the server
//! up for manual poking.
//!
//! Run: `cargo run --example imaging_demo`

use anyhow::{bail, Context, Result};
use base64::Engine;
use rand::RngCore;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use onvif_device_rs::imaging::{register_imaging_actions, ImagingParamError, ImagingParams};
use onvif_device_rs::server::{OnvifConfig, OnvifServer};

const USERNAME: &str = "admin";
const PASSWORD: &str = "12345678";

// ---------------------------------------------------------------------------
// Host seam: an in-memory parameter store (stand-in for V4L2 controls)
// ---------------------------------------------------------------------------

struct InMemoryParams {
    values: Mutex<HashMap<String, f64>>,
}

impl InMemoryParams {
    fn new() -> Self {
        let mut values = HashMap::new();
        values.insert("Brightness".to_string(), 0.5);
        values.insert("Contrast".to_string(), 0.5);
        values.insert("Saturation".to_string(), 0.5);
        values.insert("Sharpness".to_string(), 0.5);
        Self {
            values: Mutex::new(values),
        }
    }
}

impl ImagingParams for InMemoryParams {
    fn get_param(&self, name: &str) -> Result<f64, ImagingParamError> {
        self.values
            .lock()
            .expect("params lock")
            .get(name)
            .copied()
            .ok_or_else(|| ImagingParamError::InvalidName(name.to_string()))
    }

    fn set_param(&self, name: &str, value: f64) -> Result<(), ImagingParamError> {
        let mut values = self.values.lock().expect("params lock");
        if !values.contains_key(name) {
            return Err(ImagingParamError::InvalidName(name.to_string()));
        }
        if !(0.0..=1.0).contains(&value) {
            return Err(ImagingParamError::OutOfRange {
                value,
                min: 0.0,
                max: 1.0,
            });
        }
        values.insert(name.to_string(), value);
        Ok(())
    }
}

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
         <s:Body><{action} xmlns=\"http://www.onvif.org/ver20/imaging/wsdl/\">{inner_body}</{action}>\
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

/// Extract an attribute value from the first `<...tag attr="value">`.
fn xml_attr(xml: &str, tag: &str, attr: &str) -> f64 {
    let open = format!("<{tag}");
    let Some(rel) = xml.find(&open) else {
        return f64::NAN;
    };
    let after = rel + open.len();
    let end = xml[after..].find('>').map_or(xml.len(), |g| after + g);
    let elem = &xml[after..end];
    let needle = format!("{attr}=\"");
    if let Some(a) = elem.find(&needle) {
        let vstart = a + needle.len();
        let vend = elem[vstart..].find('"').map_or(elem.len(), |e| vstart + e);
        return elem[vstart..vend].parse().unwrap_or(f64::NAN);
    }
    f64::NAN
}

// ---------------------------------------------------------------------------
// Demo
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    println!("== onvif-device-rs imaging demo: get / set / range-fault self-check ==\n");

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

    // -- server side: imaging actions over an in-memory parameter store -----
    let store = Arc::new(InMemoryParams::new());
    let mut soap = OnvifServer::new(&OnvifConfig {
        port,
        username: USERNAME.to_string(),
        password: PASSWORD.to_string(),
        ..Default::default()
    });
    register_imaging_actions(&mut soap, store.clone());
    println!("[server] ONVIF SOAP + Imaging service on tcp/{port}");
    tokio::spawn(soap.start());
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 1. Defaults reported over the wire.
    let (code, body) = soap_call(port, "GetImagingSettings", "").await?;
    if code != 200 || !body.contains("GetImagingSettingsResponse") {
        bail!("GetImagingSettings failed: HTTP {code}: {body}");
    }
    let b0 = xml_attr(&body, "tt:Brightness", "Value");
    if (b0 - 0.5).abs() > 0.001 {
        bail!("default brightness should be 0.5, got {b0}");
    }
    println!("[client] GetImagingSettings -> defaults (brightness {b0})");

    // 2. Write new values through the seam.
    let inner = "<ImageSource>src</ImageSource>\
                 <Settings>\
                 <Brightness Value=\"0.8\"/>\
                 <Contrast Value=\"0.3\"/>\
                 </Settings>";
    let (code, body) = soap_call(port, "SetImagingSettings", inner).await?;
    if code != 200 || !body.contains("SetImagingSettingsResponse") {
        bail!("SetImagingSettings failed: HTTP {code}: {body}");
    }
    println!("[client] SetImagingSettings -> brightness 0.8, contrast 0.3");

    // 3. Read back — the store (and the wire) must show the new values.
    let (_code, body) = soap_call(port, "GetImagingSettings", "").await?;
    let (b1, c1) = (
        xml_attr(&body, "tt:Brightness", "Value"),
        xml_attr(&body, "tt:Contrast", "Value"),
    );
    if (b1 - 0.8).abs() > 0.001 || (c1 - 0.3).abs() > 0.001 {
        bail!("read-back mismatch: brightness {b1}, contrast {c1}");
    }
    // The untouched parameters must not drift.
    let (s1, sh1) = (
        xml_attr(&body, "tt:ColorSaturation", "Value"),
        xml_attr(&body, "tt:Sharpness", "Value"),
    );
    if (s1 - 0.5).abs() > 0.001 || (sh1 - 0.5).abs() > 0.001 {
        bail!("untouched params drifted: saturation {s1}, sharpness {sh1}");
    }
    println!("[client] GetImagingSettings -> read-back matches, untouched params stable");

    // 4. Out-of-range value — the seam rejects it with a SOAP fault.
    let inner = "<ImageSource>src</ImageSource>\
                 <Settings><Brightness Value=\"1.7\"/></Settings>";
    let (code, body) = soap_call(port, "SetImagingSettings", inner).await?;
    if code == 200 && !body.contains("Fault") {
        bail!("out-of-range brightness must fault, got HTTP {code}: {body}");
    }
    // And the store must still hold the last good value.
    assert_eq!(store.get_param("Brightness").unwrap(), 0.8);
    println!("[client] out-of-range value -> SOAP fault, store unchanged");

    println!("\nonvif-device-rs imaging demo: all checks passed");
    if serve {
        println!("--serve: keeping server on tcp/{port} (Ctrl-C to quit)");
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }
    Ok(())
}
