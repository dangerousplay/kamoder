//! Media2 (ver20/media, `tr2`) integration tests: the Profile-T entry
//! path served on its own endpoint (`/onvif/media2_service`) over the
//! SAME shared media store as the Media1 face (parity with onvif-go's
//! `SupportMedia2` + server/media2.go — the golden wire source).
//!
//! Coverage: the byte-stable tr2 profile advertisement, stream/snapshot
//! URIs (plain tr2:Uri form), the video-encoder configuration family on
//! the shared store, encoder instances, the sync point + keyframe hook,
//! the Media2 capabilities, routing guards (404 when disabled, unknown
//! action on the path, Media1 coexistence untouched), the write-style
//! auth policy, and the GetServices advertisement behind
//! `with_media2_support`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use onvif_device_rs::config::DeviceConfig;
use onvif_device_rs::device::{DeviceHandler, DeviceServiceHandlers};
use onvif_device_rs::media::{MediaProfileConfig, OnvifMediaConfig, VideoEncoding};
use onvif_device_rs::media2::MEDIA2_SERVICE_PATH;
use onvif_device_rs::server::{OnvifConfig, OnvifServer, OnvifServerHandle};

const USERNAME: &str = "admin";
const PASSWORD: &str = "password";

// ---------------------------------------------------------------------------
// Minimal ONVIF client (what a Media2-discovering NVR implements)
// ---------------------------------------------------------------------------

/// POST a SOAP 1.2 envelope whose body is `body_xml` to `path`. With
/// `auth` a PasswordText UsernameToken is carried (the default policy
/// protects Set*).
async fn post_soap(port: u16, path: &str, body_xml: &str, auth: bool) -> (u16, String) {
    let header = if auth {
        format!(
            "<Header><Security xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\">\
             <UsernameToken><Username>{USERNAME}</Username>\
             <Password Type=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-username-token-profile-1.0#PasswordText\">{PASSWORD}</Password>\
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

// ---------------------------------------------------------------------------
// Server scaffolding
// ---------------------------------------------------------------------------

fn base_config() -> OnvifConfig {
    OnvifConfig {
        port: 0,
        username: USERNAME.to_string(),
        password: PASSWORD.to_string(),
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

/// The shared multi-profile store: primary `main` (1920x1080@30) + extra
/// `sub` (640x360@15), snapshot on 8088 — the same shape the Media1
/// completion tests use.
fn multi_store() -> Arc<RwLock<OnvifMediaConfig>> {
    let mut cfg = OnvifMediaConfig::new(1920, 1080, 30, 4_000_000, 8554, "127.0.0.1".into());
    cfg.snapshot_port = 8088;
    cfg.extra_profiles = vec![MediaProfileConfig {
        token: "sub".to_string(),
        name: "sub".to_string(),
        width: 640,
        height: 360,
        fps: 15,
        bitrate: 400_000,
        encoding: VideoEncoding::H264,
        encoder_token: "sub_encoder".to_string(),
        stream_path: "/sub".to_string(),
    }];
    Arc::new(RwLock::new(cfg))
}

/// Start a server with the Media2 service enabled (and the Device
/// service + Media1 face registered for advertisement/coexistence
/// checks); returns the port, the shared store, and the handle.
async fn media2_server() -> (u16, Arc<RwLock<OnvifMediaConfig>>, OnvifServerHandle) {
    media2_server_with(None).await
}

/// Like [`media2_server`] but with `with_media2_support` controlling the
/// GetServices advertisement (default: advertisement on) and an optional
/// keyframe hook.
async fn media2_server_with(
    media2_advertise: Option<bool>,
) -> (u16, Arc<RwLock<OnvifMediaConfig>>, OnvifServerHandle) {
    let mut server = OnvifServer::new(&base_config());
    let store = multi_store();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let mut device = DeviceServiceHandlers::new(test_device_config(), port, "127.0.0.1".into())
        .expect("valid identity");
    if let Some(advertise) = media2_advertise {
        device = device.with_media2_support(advertise);
    } else {
        device = device.with_media2_support(true);
    }
    let device = Arc::new(device);
    for action in ["GetSystemDateAndTime", "GetServices", "GetCapabilities"] {
        server.register_anonymous_action(action);
        server.register_handler(action, Box::new(DeviceHandler(Arc::clone(&device))));
    }

    // The Media1 face on the default (action-map) routes — both faces
    // read the same store.
    onvif_device_rs::media::register_media_actions(&mut server, Arc::clone(&store), None);

    let _media2 = server.enable_media2(Arc::clone(&store), None);
    let handle = server.start_on(listener).await.expect("start");
    (port, store, handle)
}

// ---------------------------------------------------------------------------
// Wire goldens (deterministic responses, full envelope pinned)
// ---------------------------------------------------------------------------

/// The byte-stable tr2 profile advertisement: both profiles, the
/// Configurations wrapper + its children as tr2-local elements, the
/// configuration bodies (Name/UseCount/Encoding/Resolution/RateControl)
/// resolving to ver10/schema — the namespace split pinned by onvif-go's
/// nsMedia2* decoder test.
#[tokio::test]
async fn golden_get_profiles_over_the_wire() {
    let (port, _store, mut handle) = media2_server().await;

    let (status, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetProfiles xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200);

    let want = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\">\n  \
<soap:Header>\n  </soap:Header>\n  \
<soap:Body><tr2:GetProfilesResponse xmlns:tr2=\"http://www.onvif.org/ver20/media/wsdl\" xmlns:tt=\"http://www.onvif.org/ver10/schema\">\n  \
<tr2:Profiles token=\"main\" fixed=\"true\">\n    \
<tt:Name>main</tt:Name>\n    \
<tr2:Configurations>\n      \
<tr2:VideoSource token=\"videoSrc0\">\n        \
<tt:Name>Video Source</tt:Name>\n        \
<tt:UseCount>1</tt:UseCount>\n        \
<tt:SourceToken>videoSrc0</tt:SourceToken>\n      \
</tr2:VideoSource>\n      \
<tr2:VideoEncoder token=\"enc0\">\n        \
<tt:Name>VideoEncoderConfig</tt:Name>\n        \
<tt:UseCount>1</tt:UseCount>\n        \
<tt:Encoding>H264</tt:Encoding>\n        \
<tt:Resolution>\n          \
<tt:Width>1920</tt:Width>\n          \
<tt:Height>1080</tt:Height>\n        \
</tt:Resolution>\n        \
<tt:RateControl>\n          \
<tt:FrameRateLimit>30</tt:FrameRateLimit>\n          \
<tt:BitrateLimit>4000000</tt:BitrateLimit>\n        \
</tt:RateControl>\n      \
</tr2:VideoEncoder>\n    \
</tr2:Configurations>\n  \
</tr2:Profiles>\n  \
<tr2:Profiles token=\"sub\" fixed=\"true\">\n    \
<tt:Name>sub</tt:Name>\n    \
<tr2:Configurations>\n      \
<tr2:VideoSource token=\"videoSrc0\">\n        \
<tt:Name>Video Source</tt:Name>\n        \
<tt:UseCount>1</tt:UseCount>\n        \
<tt:SourceToken>videoSrc0</tt:SourceToken>\n      \
</tr2:VideoSource>\n      \
<tr2:VideoEncoder token=\"sub_encoder\">\n        \
<tt:Name>VideoEncoderConfig</tt:Name>\n        \
<tt:UseCount>1</tt:UseCount>\n        \
<tt:Encoding>H264</tt:Encoding>\n        \
<tt:Resolution>\n          \
<tt:Width>640</tt:Width>\n          \
<tt:Height>360</tt:Height>\n        \
</tt:Resolution>\n        \
<tt:RateControl>\n          \
<tt:FrameRateLimit>15</tt:FrameRateLimit>\n          \
<tt:BitrateLimit>400000</tt:BitrateLimit>\n        \
</tt:RateControl>\n      \
</tr2:VideoEncoder>\n    \
</tr2:Configurations>\n  \
</tr2:Profiles>\n\
</tr2:GetProfilesResponse>\n  \
</soap:Body>\n\
</soap:Envelope>";
    assert_eq!(body, want);

    handle.shutdown().await.expect("shutdown");
}

/// The plain-Uri stream answer (no MediaUri wrapper — the Media2 form).
#[tokio::test]
async fn golden_get_stream_uri_over_the_wire() {
    let (port, _store, mut handle) = media2_server().await;

    let body_xml = "<GetStreamUri xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
                    <Protocol>RTSP</Protocol><ProfileToken>main</ProfileToken>\
                    </GetStreamUri>";
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, body_xml, false).await;
    assert_eq!(status, 200);

    let want = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\">\n  \
<soap:Header>\n  </soap:Header>\n  \
<soap:Body><tr2:GetStreamUriResponse xmlns:tr2=\"http://www.onvif.org/ver20/media/wsdl\">\n  \
<tr2:Uri>rtsp://127.0.0.1:8554/stream</tr2:Uri>\n\
</tr2:GetStreamUriResponse>\n  \
</soap:Body>\n\
</soap:Envelope>";
    assert_eq!(body, want);

    handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// GetProfiles token filter / GetStreamUri routing / GetSnapshotUri
// ---------------------------------------------------------------------------

/// The optional Token argument narrows the answer to the matching
/// profile (parity with the Go twin's GetProfiles handler).
#[tokio::test]
async fn get_profiles_token_filter() {
    let (port, _store, mut handle) = media2_server().await;

    let body_xml = "<GetProfiles xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
                    <Token>sub</Token></GetProfiles>";
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, body_xml, false).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.matches("<tr2:Profiles ").count(), 1);
    assert!(body.contains(r#"<tr2:Profiles token="sub""#));
    assert!(!body.contains(r#"token="main""#));

    // Unknown token → the valid empty set (not a fault).
    let body_xml = "<GetProfiles xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
                    <Token>nope</Token></GetProfiles>";
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, body_xml, false).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.matches("<tr2:Profiles ").count(), 0);

    handle.shutdown().await.expect("shutdown");
}

/// ProfileToken routing mirrors the Media1 face: a matching extra
/// profile advertises its own stream path, unknown/missing tokens fail
/// open to the primary stream; namespaced elements parse; non-RTSP
/// protocols fault instead of lying.
#[tokio::test]
async fn get_stream_uri_routes_profile_tokens() {
    let (port, _store, mut handle) = media2_server().await;

    let uri_of = |token: &str, protocol: &str| {
        format!(
            "<GetStreamUri xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
             <Protocol>{protocol}</Protocol><ProfileToken>{token}</ProfileToken>\
             </GetStreamUri>"
        )
    };

    let (_, body) = post_soap(port, MEDIA2_SERVICE_PATH, &uri_of("sub", "RTSP"), false).await;
    assert_eq!(xml_field(&body, "tr2:Uri"), "rtsp://127.0.0.1:8554/sub");

    // The rtsp family spellings are all tolerated.
    let (_, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        &uri_of("main", "RtspUnicast"),
        false,
    )
    .await;
    assert_eq!(xml_field(&body, "tr2:Uri"), "rtsp://127.0.0.1:8554/stream");
    let (_, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        &uri_of("main", "rtsp_over_http_typo_honest_case"),
        false,
    )
    .await;
    assert_eq!(xml_field(&body, "tr2:Uri"), "rtsp://127.0.0.1:8554/stream");

    // Unknown / missing tokens fail open to the primary stream.
    let (_, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        &uri_of("profile_7", "RTSP"),
        false,
    )
    .await;
    assert_eq!(xml_field(&body, "tr2:Uri"), "rtsp://127.0.0.1:8554/stream");
    let (_, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetStreamUri xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(xml_field(&body, "tr2:Uri"), "rtsp://127.0.0.1:8554/stream");

    // Namespace prefixes must not defeat the token parse.
    let (_, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetStreamUri xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
         <tt:ProfileToken>sub</tt:ProfileToken></GetStreamUri>",
        false,
    )
    .await;
    assert_eq!(xml_field(&body, "tr2:Uri"), "rtsp://127.0.0.1:8554/sub");

    // A protocol the device cannot serve is a Sender fault, never a
    // silently-wrong URI.
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, &uri_of("main", "ftp"), false).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("protocol"), "{body}");

    handle.shutdown().await.expect("shutdown");
}

/// GetSnapshotUri honors `snapshot_port`: advertised while set, Sender
/// fault while 0 (the Media1 ErrSnapshotNotSupported behavior), through
/// the same shared store.
#[tokio::test]
async fn get_snapshot_uri_follows_snapshot_port() {
    let (port, store, mut handle) = media2_server().await;

    let body_xml = "<GetSnapshotUri xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
                    <ProfileToken>main</ProfileToken></GetSnapshotUri>";
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, body_xml, false).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        xml_field(&body, "tr2:Uri"),
        "http://127.0.0.1:8088/snapshot.jpg"
    );

    // Flip the store: snapshot off → fault (no URI nothing serves).
    store.write().expect("store write").snapshot_port = 0;
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, body_xml, false).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_lowercase().contains("not supported"), "got: {body}");

    handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Video encoder configuration family (shared store, tr2 element forms)
// ---------------------------------------------------------------------------

/// The list answers every advertised encoder configuration as
/// tr2:Configurations blocks; the optional ConfigurationToken narrows.
#[tokio::test]
async fn encoder_configurations_list_and_filter() {
    let (port, _store, mut handle) = media2_server().await;

    let (status, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetVideoEncoderConfigurations xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.matches("<tr2:Configurations ").count(), 2, "{body}");
    assert!(body.contains(r#"<tr2:Configurations token="enc0""#));
    assert!(body.contains(r#"<tr2:Configurations token="sub_encoder""#));
    assert!(body.contains("<tt:Encoding>H264</tt:Encoding>"));
    assert!(body.contains("<tt:Width>640</tt:Width>"));
    // The tr2 form omits the Media1 EncodingInterval child.
    assert!(!body.contains("EncodingInterval"));

    // Optional ConfigurationToken filter narrows to the matching entry.
    let (status, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetVideoEncoderConfigurations xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
         <ConfigurationToken>sub_encoder</ConfigurationToken>\
         </GetVideoEncoderConfigurations>",
        false,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.matches("<tr2:Configurations ").count(), 1);
    assert!(body.contains(r#"token="sub_encoder""#));

    handle.shutdown().await.expect("shutdown");
}

/// The single-get action: known token answers one tr2:Configuration
/// block; unknown/missing tokens are Sender faults (Media1 semantics).
#[tokio::test]
async fn encoder_configuration_get_and_faults() {
    let (port, _store, mut handle) = media2_server().await;

    let body_xml = "<GetVideoEncoderConfiguration xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
                    <ConfigurationToken>enc0</ConfigurationToken>\
                    </GetVideoEncoderConfiguration>";
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, body_xml, false).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.matches("<tr2:Configuration ").count(), 1);
    assert!(body.contains(r#"<tr2:Configuration token="enc0""#));
    assert!(body.contains("<tt:FrameRateLimit>30</tt:FrameRateLimit>"));

    let body_xml = "<GetVideoEncoderConfiguration xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
                    <ConfigurationToken>nope</ConfigurationToken>\
                    </GetVideoEncoderConfiguration>";
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, body_xml, false).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("configuration not found"), "{body}");

    let (status, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetVideoEncoderConfiguration xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("configuration not found"), "{body}");

    handle.shutdown().await.expect("shutdown");
}

/// The tr2 options shape: tr2:Options with child-element
/// ResolutionsAvailable entries (the tt:VideoResolution form — unlike
/// Media1's attribute form) and a FrameRateRange derived from the
/// primary fps.
#[tokio::test]
async fn encoder_configuration_options_shape() {
    let (port, _store, mut handle) = media2_server().await;

    let (status, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetVideoEncoderConfigurationOptions xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    assert!(body.contains("<tr2:Options>"), "{body}");
    // Every advertised resolution, child-element form.
    assert_eq!(body.matches("<tt:ResolutionsAvailable>").count(), 2);
    assert!(body.contains("<tt:ResolutionsAvailable>\n      <tt:Width>1920</tt:Width>"));
    assert!(body.contains("<tt:Width>640</tt:Width>"));
    assert!(body.contains("<tt:Height>360</tt:Height>"));
    // Frame rate range 1..primary-fps.
    assert!(body.contains("<tt:FrameRateRange>"));
    assert!(body.contains("<tt:Min>1</tt:Min>"));
    assert!(body.contains("<tt:Max>30</tt:Max>"));
    // No Media1-style attribute form leaks in.
    assert!(!body.contains("ResolutionsAvailable Width="));

    handle.shutdown().await.expect("shutdown");
}

/// SetVideoEncoderConfiguration: partial updates land in the shared
/// store and are visible to BOTH faces (Media2 GetProfiles, Media1's
/// listing on the default route); JPEG / unknown encodings / unknown
/// tokens fault.
#[tokio::test]
async fn set_video_encoder_configuration_shared_store() {
    let (port, _store, mut handle) = media2_server().await;

    let set_body = "<SetVideoEncoderConfiguration xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
                    <Configuration token=\"enc0\">\
                    <tt:Resolution xmlns:tt=\"http://www.onvif.org/ver10/schema\">\
                    <tt:Width>1280</tt:Width><tt:Height>720</tt:Height></tt:Resolution>\
                    <tt:RateControl xmlns:tt=\"http://www.onvif.org/ver10/schema\">\
                    <tt:FrameRateLimit>15</tt:FrameRateLimit>\
                    <tt:BitrateLimit>1000000</tt:BitrateLimit></tt:RateControl>\
                    </Configuration></SetVideoEncoderConfiguration>";
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, set_body, true).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("<tr2:SetVideoEncoderConfigurationResponse"),
        "{body}"
    );

    // Media2 face reflects it.
    let (_, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetProfiles xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
        false,
    )
    .await;
    assert!(body.contains("<tt:Width>1280</tt:Width>"), "{body}");
    assert!(body.contains("<tt:FrameRateLimit>15</tt:FrameRateLimit>"));

    // Media1 face (default route) reflects it too — one shared store.
    let (_, body) = post_soap(
        port,
        "/onvif/media_service",
        "<GetVideoEncoderConfigurations xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>",
        true,
    )
    .await;
    assert!(body.contains("<Width>1280</Width>"), "{body}");
    assert!(body.contains("<BitrateLimit>1000000</BitrateLimit>"));

    // JPEG is a valid ONVIF token this device cannot honor — fault.
    let jpeg = "<SetVideoEncoderConfiguration xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
                <Configuration token=\"enc0\"><tt:Encoding xmlns:tt=\"http://www.onvif.org/ver10/schema\">JPEG</tt:Encoding></Configuration>\
                </SetVideoEncoderConfiguration>";
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, jpeg, true).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("JPEG"), "{body}");

    // Unknown encoding / unknown token fault.
    let bogus = "<SetVideoEncoderConfiguration xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
                 <Configuration token=\"enc0\"><tt:Encoding xmlns:tt=\"http://www.onvif.org/ver10/schema\">MPG4</tt:Encoding></Configuration>\
                 </SetVideoEncoderConfiguration>";
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, bogus, true).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("encoding"), "{body}");

    let unknown = "<SetVideoEncoderConfiguration xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
                   <Configuration token=\"nope\"/>\
                   </SetVideoEncoderConfiguration>";
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, unknown, true).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("configuration not found"), "{body}");

    handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Encoder instances / sync point / capabilities
// ---------------------------------------------------------------------------

/// GetVideoEncoderInstances: one codec entry (the primary encoding, one
/// instance) and Total 1 — the WSDL's Info{Codec[]{Encoding,Number},
/// Total} shape with tr2-local children.
#[tokio::test]
async fn get_video_encoder_instances_shape() {
    let (port, _store, mut handle) = media2_server().await;

    let body_xml = "<GetVideoEncoderInstances xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
                    <ConfigurationToken>videoSrc0</ConfigurationToken>\
                    </GetVideoEncoderInstances>";
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, body_xml, false).await;
    assert_eq!(status, 200, "{body}");

    assert!(body.contains("<tr2:Info>"), "{body}");
    assert_eq!(body.matches("<tr2:Codec>").count(), 1);
    assert!(body.contains("<tr2:Encoding>H264</tr2:Encoding>"));
    assert!(body.contains("<tr2:Number>1</tr2:Number>"));
    assert!(body.contains("<tr2:Total>1</tr2:Total>"));

    handle.shutdown().await.expect("shutdown");
}

/// SetSynchronizationPoint acks with the tr2 empty response and fires
/// the host keyframe hook once per request.
#[tokio::test]
async fn set_synchronization_point_fires_hook() {
    let counter = Arc::new(AtomicU32::new(0));
    let seen = Arc::clone(&counter);
    let hook: Option<Arc<dyn Fn() + Send + Sync>> = Some(Arc::new(move || {
        seen.fetch_add(1, Ordering::SeqCst);
    }));

    let mut server = OnvifServer::new(&base_config());
    let store = multi_store();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let _media2 = server.enable_media2(store, hook);
    let mut handle = server.start_on(listener).await.expect("start");

    let body_xml = "<SetSynchronizationPoint xmlns=\"http://www.onvif.org/ver20/media/wsdl\">\
                    <ProfileToken>main</ProfileToken></SetSynchronizationPoint>";
    let (status, body) = post_soap(port, MEDIA2_SERVICE_PATH, body_xml, true).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("<tr2:SetSynchronizationPointResponse"),
        "{body}"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    handle.shutdown().await.expect("shutdown");
}

/// The Media2 capabilities: tr2:Capabilities with SnapshotUri following
/// the store, ProfileCapabilities/MaximumNumberOfProfiles = the
/// advertised profile count, StreamingCapabilities RTSPStreaming=true.
#[tokio::test]
async fn media2_service_capabilities() {
    let (port, store, mut handle) = media2_server().await;

    let (status, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetServiceCapabilities xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    assert!(
        body.contains("<tr2:Capabilities SnapshotUri=\"true\""),
        "{body}"
    );
    assert!(body.contains(r#"Rotation="false""#));
    assert!(body.contains("<tr2:ProfileCapabilities MaximumNumberOfProfiles=\"2\"/>"));
    assert!(body.contains("<tr2:StreamingCapabilities RTSPStreaming=\"true\""));
    assert!(body.contains(r#"RTPMulticast="false""#));
    assert!(body.contains(r#"RTP_RTSP_TCP="true""#));
    // The Media1 unprefixed wire style must not leak into the tr2 face.
    assert!(!body.contains("<ProfileCapabilities"));

    // snapshot_port off → the attribute flips through the shared store.
    store.write().expect("store write").snapshot_port = 0;
    let (_, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetServiceCapabilities xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
        false,
    )
    .await;
    assert!(body.contains(r#"SnapshotUri="false""#), "{body}");

    handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Routing guards + auth policy
// ---------------------------------------------------------------------------

/// With the service disabled the path has no route — 404, and every
/// other path keeps the historical behavior.
#[tokio::test]
async fn media2_path_404_when_disabled() {
    let mut server = OnvifServer::new(&base_config());
    let store = multi_store();
    onvif_device_rs::media::register_media_actions(&mut server, store, None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let mut handle = server.start_on(listener).await.expect("start");

    let (status, _body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetProfiles xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 404, "disabled media2 service must not serve");

    // The Media1 face on the default route keeps working unchanged.
    let (status, body) = post_soap(
        port,
        "/onvif/media_service",
        "<GetProfiles xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>",
        true,
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("GetProfilesResponse"));
    assert!(body.contains("<Profiles "));
    assert!(!body.contains("tr2:"));

    handle.shutdown().await.expect("shutdown");
}

/// An action the Media2 endpoint does not own is an unsupported-action
/// Sender fault — including Media1-only actions (the two faces share
/// local names like GetProfiles; the path decides which face answers).
#[tokio::test]
async fn unknown_action_on_media2_path_faults() {
    let (port, _store, mut handle) = media2_server().await;

    let (status, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetVideoSources xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains("unsupported action: GetVideoSources"),
        "{body}"
    );

    // The shared GetServiceCapabilities name answers the tr2 capabilities
    // on this path — not Media1's.
    let (status, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetServiceCapabilities xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<tr2:Capabilities"), "{body}");

    // ...and the Media1 capabilities on the default route (byte-stable).
    let (status, body) = post_soap(
        port,
        "/onvif/media_service",
        "<GetServiceCapabilities xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>",
        true,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<Capabilities"), "{body}");
    assert!(!body.contains("tr2:"), "{body}");

    handle.shutdown().await.expect("shutdown");
}

/// Write-style Media2 actions (Set*) stay behind WS-Security; reads are
/// open — the same policy the events service applies.
#[tokio::test]
async fn media2_auth_policy() {
    let (port, _store, mut handle) = media2_server().await;

    // Reads are open.
    let (status, _) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<GetProfiles xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 200, "GetProfiles must be pre-auth");

    // Set* without credentials → 401.
    let (status, body) = post_soap(
        port,
        MEDIA2_SERVICE_PATH,
        "<SetSynchronizationPoint xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>",
        false,
    )
    .await;
    assert_eq!(status, 401, "unauthenticated Set* must 401:\n{body}");

    handle.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Advertisement (GetServices gains the ver20/media entry behind the flag)
// ---------------------------------------------------------------------------

/// `with_media2_support(true)` appends the ver20/media service entry with
/// the media2 XAddr; the legacy GetCapabilities gains nothing (Media2 has
/// no slot there — GetServices is the discovery probe).
#[tokio::test]
async fn advertisement_behind_the_flag() {
    let (port, _store, mut handle) = media2_server_with(Some(true)).await;

    let (_, body) = post_soap(
        port,
        "/onvif/device_service",
        "<GetServices xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>",
        false,
    )
    .await;
    assert!(
        body.contains("http://www.onvif.org/ver20/media/wsdl"),
        "media2 namespace missing from GetServices:\n{body}"
    );
    assert!(
        body.contains(&format!("http://127.0.0.1:{port}/onvif/media2_service")),
        "media2 XAddr missing from GetServices:\n{body}"
    );

    let (_, body) = post_soap(
        port,
        "/onvif/device_service",
        "<GetCapabilities xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>",
        false,
    )
    .await;
    assert!(
        !body.contains("ver20/media"),
        "GetCapabilities has no Media2 slot:\n{body}"
    );

    handle.shutdown().await.expect("shutdown");
}

/// Default advertisement (flag off) keeps GetServices byte-identical to
/// the pre-Media2 answer.
#[tokio::test]
async fn advertisement_absent_by_default() {
    let (port, _store, mut handle) = media2_server_with(Some(false)).await;

    let (_, body) = post_soap(
        port,
        "/onvif/device_service",
        "<GetServices xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>",
        false,
    )
    .await;
    assert!(
        !body.contains("ver20/media"),
        "media2 advertised while unsupported:\n{body}"
    );

    handle.shutdown().await.expect("shutdown");
}
