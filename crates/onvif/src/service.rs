//! Wires `onvif-device-rs` into the camera's ONVIF service: which actions
//! exist, what they report, who may call them.

use crate::device::{Endpoints, GetCapabilities, GetServices};
use crate::imaging;
use crate::media::{self, Media, VideoStream};
use crate::producer::Producer;
use crate::ptz::Ptz;
use crate::ptz_soap;
use crate::secret::Secret;
use metrics::{counter, describe_counter};
use onvif_device_rs::config::DeviceConfig;
use onvif_device_rs::device::{DeviceHandler, DeviceServiceHandlers};
use onvif_device_rs::metrics::MetricsHooks;
use onvif_device_rs::server::{OnvifConfig, OnvifServer};
use onvif_device_rs::types::OnvifError;
use std::sync::Arc;

/// Device service actions answered by the library's `DeviceHandler`, whose
/// output for these is already namespace-qualified. GetCapabilities and
/// GetServices are ours (`device.rs`).
const DEVICE_ACTIONS: &[&str] = &["GetSystemDateAndTime", "GetDeviceInformation", "GetScopes"];

/// Everything the SOAP side needs to know.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Port the SOAP server is reachable on (goes into service XAddrs).
    pub port: u16,
    /// Fallback address for URLs. Responses normally use the local address of
    /// the request's connection; this is only used when that is loopback
    /// (a local proxy) or unknown.
    pub device_ip: String,
    pub identity: DeviceConfig,
    pub rtsp_port: u16,
    /// The video producer's `host:port`, whose `/status` gives each
    /// sensor's stream parameters; `None` = use `video` only.
    pub producer: Option<String>,
    /// The board's streams, one per sensor: the Media profiles while the
    /// producer has none. Must not be empty.
    pub video: Vec<VideoStream>,
    pub web_port: u16,
    /// With a leading `/`.
    pub snapshot_path: String,
    /// `None` = `--no-auth`.
    pub secret: Option<Secret>,
    /// The motor, when this camera has one (`--ptz`); PTZ is served and
    /// advertised only then.
    pub ptz: Option<Arc<Ptz>>,
    /// The lights owner's socket, through which Imaging (IR-cut filter) is
    /// served; `None` = no Imaging service. Advertised only while it answers.
    pub lights: Option<lights_proto::Client>,
}

/// Builds the server with every handler registered; start it with
/// `start_on(listener)`.
pub fn build(settings: &Settings) -> Result<OnvifServer, OnvifError> {
    let (username, password) = match &settings.secret {
        Some(secret) => (secret.user.clone(), secret.password.clone()),
        None => (String::new(), String::new()),
    };
    let config = OnvifConfig {
        port: settings.port,
        username,
        password,
        // Only reachable with an explicit --no-auth; the library refuses an
        // empty password otherwise.
        allow_no_auth: settings.secret.is_none(),
        // SOAP requests are a few KiB. The library default (1 MiB) would let
        // one client pin a megabyte of a 32 MB board.
        max_body_bytes: 64 * 1024,
        read_timeout: std::time::Duration::from_secs(10),
        ..Default::default()
    };
    let first = settings
        .video
        .first()
        .ok_or_else(|| OnvifError::Internal("the board profile lists no video sensor".into()))?;
    // The motor turns the whole head; its configuration hangs off the first
    // sensor's profile, which is what single-stream clients pick.
    let ptz_profile = media::profile_token(first.index);
    let mut server = OnvifServer::new(&config).with_metrics(Arc::new(Metrics));

    if let Some(motor) = &settings.ptz {
        ptz_soap::register(&mut server, motor, &ptz_profile);
    }
    let ptz = settings.ptz.is_some();
    if let Some(lights) = &settings.lights {
        imaging::register(&mut server, lights);
    }

    let device = Arc::new(DeviceServiceHandlers::new(
        settings.identity.clone(),
        settings.port,
        settings.device_ip.clone(),
    )?);
    // ONVIF Core §5.9.1: clients need the camera's clock before they can
    // compute a PasswordDigest, so this one is pre-auth.
    server.register_anonymous_action("GetSystemDateAndTime");
    for action in DEVICE_ACTIONS {
        server.register_handler(action, Box::new(DeviceHandler(Arc::clone(&device))));
    }
    let endpoints = Endpoints {
        port: settings.port,
        device_ip: settings.device_ip.clone(),
        ptz,
        lights: settings.lights.clone(),
    };
    server.register_handler(
        "GetCapabilities",
        Box::new(GetCapabilities(endpoints.clone())),
    );
    server.register_handler("GetServices", Box::new(GetServices(endpoints)));

    media::register(
        &mut server,
        Media {
            producer: settings.producer.as_deref().map(Producer::new),
            fallback: settings.video.clone(),
            rtsp_port: settings.rtsp_port,
            snapshot_port: settings.web_port,
            snapshot_path: settings.snapshot_path.clone(),
            device_ip: settings.device_ip.clone(),
            ptz_profile: ptz.then_some(ptz_profile),
            on_fallback: Default::default(),
        },
    );

    Ok(server)
}

pub fn describe_metrics() {
    describe_counter!(
        "kamoder_onvif_requests_total",
        "SOAP requests that passed authentication and reached a handler, by action"
    );
    describe_counter!(
        "kamoder_onvif_faults_total",
        "SOAP requests answered with a fault by the handler, by action"
    );
    describe_counter!(
        "kamoder_onvif_auth_failures_total",
        "WS-UsernameToken verifications that failed (bad credentials or replay)"
    );
    describe_counter!(
        "kamoder_onvif_auth_lockouts_total",
        "Requests refused because their source is locked out after repeated failures"
    );
    describe_counter!(
        "kamoder_onvif_probes_answered_total",
        "WS-Discovery Probes answered with a ProbeMatch"
    );
    describe_counter!(
        "kamoder_onvif_ptz_socket_requests_total",
        "Requests on the PTZ ownership socket, by op and result"
    );
    describe_counter!(
        "kamoder_onvif_ptz_socket_refused_total",
        "PTZ socket connections refused because all slots were busy"
    );
    describe_counter!(
        "kamoder_onvif_ptz_deadman_stops_total",
        "Continuous moves stopped because their timeout passed"
    );
}

/// Bridges the library's observation hooks to the `metrics` facade.
///
/// The `action` label is bounded: the library fires `soap_request` only for
/// registered actions, never for arbitrary client-supplied names.
pub struct Metrics;

impl MetricsHooks for Metrics {
    fn soap_request(&self, action: &str) {
        counter!("kamoder_onvif_requests_total", "action" => action.to_owned()).increment(1);
    }
    fn soap_fault(&self, action: &str) {
        counter!("kamoder_onvif_faults_total", "action" => action.to_owned()).increment(1);
    }
    fn auth_fail(&self) {
        tracing::warn!("ONVIF authentication failed");
        counter!("kamoder_onvif_auth_failures_total").increment(1);
    }
    fn auth_lockout(&self) {
        counter!("kamoder_onvif_auth_lockouts_total").increment(1);
    }
    fn discovery_probe_answered(&self) {
        counter!("kamoder_onvif_probes_answered_total").increment(1);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    use onvif_device_rs::namespaces::{
        DEVICE_SERVICE, IMAGING_SERVICE, MEDIA_SERVICE, PTZ_SERVICE,
    };
    use onvif_schema::{tds, timg, tptz, trt, tt};
    use sha1::{Digest, Sha1};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub fn identity() -> DeviceConfig {
        DeviceConfig {
            name: "Test camera".into(),
            manufacturer: "Kamoder".into(),
            model: "AK3918AV100".into(),
            firmware: "0.1.0".into(),
            hardware_id: "AK3918AV100".into(),
            serial_number: "001a2b3c4d5e".into(),
        }
    }

    fn settings(ptz: Option<Arc<Ptz>>) -> Settings {
        Settings {
            port: 8000,
            device_ip: "192.0.2.10".into(),
            identity: identity(),
            rtsp_port: 554,
            producer: None,
            video: vec![VideoStream {
                index: 0,
                width: 1280,
                height: 720,
                fps: 25,
                kbps: 2000,
                gop: 50,
                profile: "main".into(),
            }],
            web_port: 80,
            snapshot_path: "/api/snapshot.jpg".into(),
            secret: Some(Secret {
                user: "nvr".into(),
                password: "correct horse".into(),
            }),
            ptz,
            lights: None,
        }
    }

    /// Starts the server on an ephemeral loopback port.
    async fn start(
        settings: Settings,
    ) -> (std::net::SocketAddr, onvif_device_rs::OnvifServerHandle) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = build(&settings).unwrap().start_on(listener).await.unwrap();
        (addr, handle)
    }

    /// ISO 8601 UTC, the WS-Security `Created` format.
    fn created_now() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60
        )
    }

    /// WS-Security header with a PasswordDigest token, computed the way a
    /// client does: Base64(SHA1(nonce + created + password)).
    fn digest_header(user: &str, password: &str) -> String {
        let nonce: Vec<u8> = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_le_bytes()
            .to_vec();
        let created = created_now();
        let mut sha = Sha1::new();
        sha.update(&nonce);
        sha.update(created.as_bytes());
        sha.update(password.as_bytes());
        let digest = BASE64.encode(sha.finalize());
        format!(
            r#"<s:Header><wsse:Security xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd"><wsse:UsernameToken><wsse:Username>{user}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password><wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{nonce}</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security></s:Header>"#,
            nonce = BASE64.encode(&nonce),
        )
    }

    fn text_header(user: &str, password: &str) -> String {
        format!(
            r#"<s:Header><wsse:Security xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"><wsse:UsernameToken><wsse:Username>{user}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordText">{password}</wsse:Password></wsse:UsernameToken></wsse:Security></s:Header>"#
        )
    }

    /// POSTs one SOAP request and returns (status, body). Every prefix the
    /// bodies use is declared on the Envelope only, the way gSOAP clients
    /// (ONVIF Device Manager) write requests.
    async fn call(addr: std::net::SocketAddr, header: &str, body: &str) -> (u16, String) {
        post(
            addr,
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:timg="http://www.onvif.org/ver20/imaging/wsdl">{header}<s:Body>{body}</s:Body></s:Envelope>"#
            ),
        )
        .await
    }

    /// POSTs a whole SOAP envelope and returns (status, body).
    async fn post(addr: std::net::SocketAddr, envelope: &str) -> (u16, String) {
        let request = format!(
            "POST /onvif/device_service HTTP/1.1\r\nHost: {addr}\r\n\
             Content-Type: application/soap+xml; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{envelope}",
            envelope.len()
        );
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        let status = response
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_owned())
            .unwrap_or_default();
        (status, body)
    }

    /// The Body child of a response envelope, parsed as `T` the way a
    /// WSDL-driven client reads it: by namespace, so an unqualified or
    /// misqualified answer fails here.
    fn parse<T: onvif_schema::xsd_parser_types::quick_xml::WithDeserializer>(envelope: &str) -> T {
        let soap = onvif_device_rs::server::parse_soap_request(envelope)
            .unwrap_or_else(|error| panic!("{error}\n{envelope}"));
        onvif_schema::from_xml(&soap.body_xml).unwrap_or_else(|error| panic!("{error}\n{envelope}"))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn soap_end_to_end() {
        let (addr, mut handle) = start(settings(None)).await;

        // (a) The clock is readable without credentials.
        let (status, body) = call(addr, "", "<tds:GetSystemDateAndTime/>").await;
        assert_eq!(status, 200, "{body}");
        let clock: tds::GetSystemDateAndTimeResponse = parse(&body);
        assert!(clock.system_date_and_time.utc_date_time.is_some(), "{body}");

        // (b) Everything else needs a token.
        let (status, body) = call(addr, "", "<tds:GetDeviceInformation/>").await;
        assert_eq!(status, 401, "{body}");
        assert!(body.contains("soap:Fault"), "{body}");
        assert!(!body.contains("Kamoder"), "{body}");

        // (c) PasswordDigest.
        let (status, body) = call(
            addr,
            &digest_header("nvr", "correct horse"),
            "<tds:GetDeviceInformation/>",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let info: tds::GetDeviceInformationResponse = parse(&body);
        assert_eq!(info.manufacturer, "Kamoder");
        assert_eq!(info.model, "AK3918AV100");
        assert_eq!(info.serial_number, "001a2b3c4d5e");

        // PasswordText works too; a wrong digest does not.
        let (status, body) = call(
            addr,
            &text_header("nvr", "correct horse"),
            "<tds:GetScopes/>",
        )
        .await;
        assert_eq!(status, 200);
        let scopes: tds::GetScopesResponse = parse(&body);
        assert!(scopes
            .scopes
            .iter()
            .any(|scope| scope.scope_item == "onvif://www.onvif.org/name/Test camera"));
        let (status, body) = call(
            addr,
            &digest_header("nvr", "wrong"),
            "<tds:GetDeviceInformation/>",
        )
        .await;
        assert_eq!(status, 401, "{body}");

        // (d) Stream and snapshot URIs. The test connects over loopback, so
        // the fallback device address is reported.
        let auth = || text_header("nvr", "correct horse");
        let (status, body) = call(
            addr,
            &auth(),
            r#"<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>Profile_vs0</trt:ProfileToken></trt:GetStreamUri>"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let uri: trt::GetStreamUriResponse = parse(&body);
        assert_eq!(uri.media_uri.uri, "rtsp://192.0.2.10:554/vs0");

        let (status, body) = call(
            addr,
            &auth(),
            "<trt:GetSnapshotUri><trt:ProfileToken>Profile_vs0</trt:ProfileToken></trt:GetSnapshotUri>",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let uri: trt::GetSnapshotUriResponse = parse(&body);
        assert_eq!(
            uri.media_uri.uri,
            "http://192.0.2.10:80/api/snapshot.jpg?camera=0"
        );
        let (status, _) = call(
            addr,
            &auth(),
            "<trt:GetSnapshotUri><trt:ProfileToken>other</trt:ProfileToken></trt:GetSnapshotUri>",
        )
        .await;
        assert_eq!(status, 400);

        let (status, body) = call(addr, &auth(), "<trt:GetProfiles/>").await;
        assert_eq!(status, 200, "{body}");
        let profiles: trt::GetProfilesResponse = parse(&body);
        assert_eq!(profiles.profiles.len(), 1);
        let profile = &profiles.profiles[0];
        assert_eq!(profile.token, "Profile_vs0");
        let encoder = profile.video_encoder_configuration.as_ref().unwrap();
        assert!(matches!(encoder.encoding, tt::VideoEncodingType::H264));
        assert_eq!(encoder.resolution.width, 1280);
        assert!(profile.ptz_configuration.is_none());

        // No PTZ backend (`--ptz none` or none found) and no lights socket
        // (`--lights-socket ""`): neither PTZ nor Imaging is advertised.
        let (status, body) = call(addr, &auth(), "<tds:GetCapabilities/>").await;
        assert_eq!(status, 200, "{body}");
        let caps: tds::GetCapabilitiesResponse = parse(&body);
        let caps = caps.capabilities;
        assert_eq!(
            caps.media.unwrap().x_addr,
            "http://192.0.2.10:8000/onvif/media_service"
        );
        assert!(caps.ptz.is_none() && caps.imaging.is_none() && caps.events.is_none());
        let (_, body) = call(addr, &auth(), "<tds:GetServices/>").await;
        let services: tds::GetServicesResponse = parse(&body);
        let namespaces: Vec<_> = services
            .service
            .iter()
            .map(|service| service.namespace.as_str())
            .collect();
        assert_eq!(namespaces, [DEVICE_SERVICE, MEDIA_SERVICE]);

        // Unknown actions fault (ONVIF-05; see SPEC_DEVIATION in main.rs).
        let (status, body) = call(
            addr,
            &auth(),
            "<tptz:ContinuousMove xmlns:tptz=\"http://www.onvif.org/ver20/ptz/wsdl\"/>",
        )
        .await;
        assert_eq!(status, 400, "{body}");
        assert!(
            body.contains("soap:Fault") && body.contains("unsupported action"),
            "{body}"
        );

        handle.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn profile_s_media_configuration_calls() {
        let (addr, mut handle) = start(settings(None)).await;
        let auth = || text_header("nvr", "correct horse");
        let ok = |(status, body): (u16, String)| {
            assert_eq!(status, 200, "{body}");
            body
        };

        // WSDL shape: tds:Service entries directly under the response.
        let services: tds::GetServicesResponse = parse(&ok(call(
            addr,
            &auth(),
            "<tds:GetServices><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>",
        )
        .await));
        assert_eq!(services.service.len(), 2);
        assert_eq!(
            services.service[1].x_addr,
            "http://192.0.2.10:8000/onvif/media_service"
        );

        let sources: trt::GetVideoSourcesResponse =
            parse(&ok(call(addr, &auth(), "<trt:GetVideoSources/>").await));
        assert_eq!(sources.video_sources[0].token, "videoSrc0");
        assert_eq!(sources.video_sources[0].framerate, 25.0);

        let body = ok(call(addr, &auth(), "<trt:GetVideoSourceConfigurations/>").await);
        let listing: trt::GetVideoSourceConfigurationsResponse = parse(&body);
        let source = &listing.configurations[0];
        assert_eq!(
            (source.token.as_str(), source.source_token.as_str()),
            ("videoSrc0", "videoSrc0")
        );
        assert_eq!(
            (source.bounds.width, source.bounds.height),
            (1280, 720),
            "{body}"
        );

        let one: trt::GetVideoSourceConfigurationResponse = parse(&ok(call(
            addr,
            &auth(),
            "<trt:GetVideoSourceConfiguration><trt:ConfigurationToken>videoSrc0</trt:ConfigurationToken></trt:GetVideoSourceConfiguration>",
        )
        .await));
        assert_eq!(one.configuration.token, "videoSrc0");
        let (status, _) = call(
            addr,
            &auth(),
            "<trt:GetVideoSourceConfiguration><trt:ConfigurationToken>nope</trt:ConfigurationToken></trt:GetVideoSourceConfiguration>",
        )
        .await;
        assert_eq!(status, 400);

        let listing: trt::GetVideoEncoderConfigurationsResponse = parse(&ok(call(
            addr,
            &auth(),
            "<trt:GetVideoEncoderConfigurations/>",
        )
        .await));
        let encoder = &listing.configurations[0];
        assert_eq!(encoder.token, "enc0");
        assert!(matches!(encoder.encoding, tt::VideoEncodingType::H264));
        let rate = encoder.rate_control.as_ref().unwrap();
        assert_eq!((rate.frame_rate_limit, rate.bitrate_limit), (25, 2000));

        let one: trt::GetVideoEncoderConfigurationResponse = parse(&ok(call(
            addr,
            &auth(),
            "<trt:GetVideoEncoderConfiguration><trt:ConfigurationToken>enc0</trt:ConfigurationToken></trt:GetVideoEncoderConfiguration>",
        )
        .await));
        assert_eq!(one.configuration.token, "enc0");

        let compatible: trt::GetCompatibleVideoEncoderConfigurationsResponse = parse(&ok(call(
            addr,
            &auth(),
            "<trt:GetCompatibleVideoEncoderConfigurations><trt:ProfileToken>Profile_vs0</trt:ProfileToken></trt:GetCompatibleVideoEncoderConfigurations>",
        )
        .await));
        assert_eq!(compatible.configurations.len(), 1);

        let options: trt::GetVideoEncoderConfigurationOptionsResponse = parse(&ok(call(
            addr,
            &auth(),
            "<trt:GetVideoEncoderConfigurationOptions/>",
        )
        .await));
        let h264 = options.options.h264.unwrap();
        assert_eq!(h264.resolutions_available[0].width, 1280);

        let audio: trt::GetAudioSourcesResponse =
            parse(&ok(call(addr, &auth(), "<trt:GetAudioSources/>").await));
        assert!(audio.audio_sources.is_empty());

        handle.shutdown().await.unwrap();
    }

    /// A simulated head on a manual clock, wrapped as the service uses it.
    fn manual_sim() -> (Arc<Ptz>, Arc<anyka_ptz::ManualClock>) {
        let clock = Arc::new(anyka_ptz::ManualClock::new());
        let sim = anyka_ptz::Sim::with_clock(
            anyka_ptz::SimConfig::default(),
            anyka_ptz::PresetStore::in_memory(),
            clock.clone(),
        )
        .unwrap();
        (Ptz::new(Box::new(sim)), clock)
    }

    /// `(x, y, move status)` from a GetStatus response.
    fn ptz_status(body: &str) -> (f32, f32, &'static str) {
        let status: tptz::GetStatusResponse = parse(body);
        let status = status.ptz_status;
        let position = status.position.and_then(|p| p.pan_tilt).expect("Position");
        let moving = match status.move_status.and_then(|m| m.pan_tilt) {
            Some(tt::MoveStatusType::Idle) => "IDLE",
            Some(tt::MoveStatusType::Moving) => "MOVING",
            other => panic!("MoveStatus {other:?}"),
        };
        (position.x, position.y, moving)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ptz_over_soap() {
        let (ptz, clock) = manual_sim();
        let (addr, mut handle) = start(settings(Some(Arc::clone(&ptz)))).await;
        let auth = || text_header("nvr", "correct horse");
        let ok = |(status, body): (u16, String)| {
            assert_eq!(status, 200, "{body}");
            body
        };
        let fault = |(status, body): (u16, String)| {
            assert_eq!(status, 400, "{body}");
            assert!(body.contains("soap:Fault"), "{body}");
            body
        };
        async fn get_status_of(addr: std::net::SocketAddr) -> (u16, String) {
            call(
                addr,
                &text_header("nvr", "correct horse"),
                "<tptz:GetStatus><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken></tptz:GetStatus>",
            )
            .await
        }
        let get_status = || get_status_of(addr);

        // Advertised, and attached to the media profile.
        let caps: tds::GetCapabilitiesResponse =
            parse(&ok(call(addr, &auth(), "<tds:GetCapabilities/>").await));
        assert_eq!(
            caps.capabilities.ptz.unwrap().x_addr,
            "http://192.0.2.10:8000/onvif/ptz_service"
        );
        assert!(caps.capabilities.imaging.is_none());
        let services: tds::GetServicesResponse =
            parse(&ok(call(addr, &auth(), "<tds:GetServices/>").await));
        assert!(services.service.iter().any(|s| s.namespace == PTZ_SERVICE));
        let profiles: trt::GetProfilesResponse =
            parse(&ok(call(addr, &auth(), "<trt:GetProfiles/>").await));
        let configuration = profiles.profiles[0].ptz_configuration.as_ref().unwrap();
        assert_eq!(configuration.token, "ptz_config");
        assert_eq!(configuration.node_token, "ptz_node");

        // Node and configuration discovery, as clients do first.
        let nodes: tptz::GetNodesResponse =
            parse(&ok(call(addr, &auth(), "<tptz:GetNodes/>").await));
        let node = &nodes.ptz_node[0];
        assert_eq!(node.token, "ptz_node");
        assert_eq!(
            node.supported_ptz_spaces.continuous_pan_tilt_velocity_space[0].uri,
            "http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace"
        );
        assert!(node.home_supported);
        ok(call(
            addr,
            &auth(),
            "<tptz:GetNode><tptz:NodeToken>ptz_node</tptz:NodeToken></tptz:GetNode>",
        )
        .await);
        fault(
            call(
                addr,
                &auth(),
                "<tptz:GetNode><tptz:NodeToken>other</tptz:NodeToken></tptz:GetNode>",
            )
            .await,
        );
        let configurations: tptz::GetConfigurationsResponse =
            parse(&ok(call(addr, &auth(), "<tptz:GetConfigurations/>").await));
        let configuration = &configurations.ptz_configuration[0];
        assert_eq!(configuration.token, "ptz_config");
        assert_eq!(configuration.default_ptz_timeout.as_deref(), Some("PT10S"));
        ok(call(addr, &auth(), "<tptz:GetConfiguration><tptz:PTZConfigurationToken>ptz_config</tptz:PTZConfigurationToken></tptz:GetConfiguration>").await);
        let options: tptz::GetConfigurationOptionsResponse = parse(&ok(call(addr, &auth(), "<tptz:GetConfigurationOptions><tptz:ConfigurationToken>ptz_config</tptz:ConfigurationToken></tptz:GetConfigurationOptions>").await));
        let options = options.ptz_configuration_options;
        assert_eq!(options.spaces.absolute_pan_tilt_position_space.len(), 1);
        assert_eq!(
            (
                options.ptz_timeout.min.as_str(),
                options.ptz_timeout.max.as_str()
            ),
            ("PT0.1S", "PT60S")
        );

        // Starts centred and idle.
        let body = ok(get_status().await);
        let status: tptz::GetStatusResponse = parse(&body);
        assert!(status.ptz_status.utc_time.ends_with('Z'), "{body}");
        assert_eq!(ptz_status(&body), (0.0, 0.0, "IDLE"));

        // ContinuousMove, then Stop.
        ok(call(addr, &auth(), r#"<tptz:ContinuousMove><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x="1" y="0" space="http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace"/></tptz:Velocity><tptz:Timeout>PT60S</tptz:Timeout></tptz:ContinuousMove>"#).await);
        clock.advance(std::time::Duration::from_secs(1));
        let (x, y, status) = ptz_status(&ok(get_status().await));
        assert!(x > 0.0 && y == 0.0, "{x} {y}");
        assert_eq!(status, "MOVING");
        ok(call(
            addr,
            &auth(),
            "<tptz:Stop><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken></tptz:Stop>",
        )
        .await);
        assert_eq!(ptz_status(&ok(get_status().await)).2, "IDLE");

        // AbsoluteMove in the generic space.
        ok(call(addr, &auth(), r#"<tptz:AbsoluteMove><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken><tptz:Position><tt:PanTilt x="-1" y="1"/></tptz:Position></tptz:AbsoluteMove>"#).await);
        clock.advance(std::time::Duration::from_secs(60));
        assert_eq!(ptz_status(&ok(get_status().await)), (-1.0, 1.0, "IDLE"));
        fault(call(addr, &auth(), r#"<tptz:AbsoluteMove><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken><tptz:Position><tt:PanTilt x="2" y="0"/></tptz:Position></tptz:AbsoluteMove>"#).await);

        // Presets: set here, move away, go back, remove.
        let set: tptz::SetPresetResponse = parse(&ok(call(addr, &auth(), "<tptz:SetPreset><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken><tptz:PresetName>Door&amp;hall</tptz:PresetName></tptz:SetPreset>").await));
        assert_eq!(set.preset_token, "1");
        let body = ok(call(
            addr,
            &auth(),
            "<tptz:GetPresets><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken></tptz:GetPresets>",
        )
        .await);
        // (onvif-device-rs trims the text around an entity when it copies the
        // body out, so "Door &amp; hall" would arrive as "Door&hall".)
        let presets: tptz::GetPresetsResponse = parse(&body);
        let preset = &presets.preset[0];
        assert_eq!(preset.token.as_deref(), Some("1"));
        assert_eq!(preset.name.as_deref(), Some("Door&hall"));
        assert!(body.contains("Door&amp;hall"), "{body}");
        let position = preset
            .ptz_position
            .as_ref()
            .unwrap()
            .pan_tilt
            .as_ref()
            .unwrap();
        assert_eq!((position.x, position.y), (-1.0, 1.0));
        ok(call(addr, &auth(), r#"<tptz:RelativeMove><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken><tptz:Translation><tt:PanTilt x="1" y="-1"/></tptz:Translation></tptz:RelativeMove>"#).await);
        clock.advance(std::time::Duration::from_secs(60));
        assert_eq!(ptz_status(&ok(get_status().await)), (0.0, 0.0, "IDLE"));
        ok(call(addr, &auth(), "<tptz:GotoPreset><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken><tptz:PresetToken>1</tptz:PresetToken></tptz:GotoPreset>").await);
        assert_eq!(ptz_status(&ok(get_status().await)).2, "MOVING");
        clock.advance(std::time::Duration::from_secs(60));
        assert_eq!(ptz_status(&ok(get_status().await)), (-1.0, 1.0, "IDLE"));
        fault(call(addr, &auth(), "<tptz:GotoPreset><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken><tptz:PresetToken>9</tptz:PresetToken></tptz:GotoPreset>").await);
        ok(call(addr, &auth(), "<tptz:RemovePreset><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken><tptz:PresetToken>1</tptz:PresetToken></tptz:RemovePreset>").await);
        let presets: tptz::GetPresetsResponse = parse(&ok(call(
            addr,
            &auth(),
            "<tptz:GetPresets><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken></tptz:GetPresets>",
        )
        .await));
        assert!(presets.preset.is_empty());

        // Home: the sim's homing run ends at the minimum of both axes.
        ok(call(addr, &auth(), "<tptz:GotoHomePosition><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken></tptz:GotoHomePosition>").await);
        clock.advance(std::time::Duration::from_secs(60));
        assert_eq!(ptz_status(&ok(get_status().await)), (-1.0, -1.0, "IDLE"));

        // A ContinuousMove Timeout stops the head on its own.
        ok(call(addr, &auth(), r#"<tptz:ContinuousMove><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x="1" y="1"/></tptz:Velocity><tptz:Timeout>PT0.1S</tptz:Timeout></tptz:ContinuousMove>"#).await);
        assert_eq!(ptz_status(&ok(get_status().await)).2, "MOVING");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(ptz_status(&ok(get_status().await)).2, "IDLE");

        // Client mistakes and unsupported operations fault.
        fault(
            call(
                addr,
                &auth(),
                "<tptz:GetStatus><tptz:ProfileToken>other</tptz:ProfileToken></tptz:GetStatus>",
            )
            .await,
        );
        let body = fault(call(addr, &auth(), r#"<tptz:ContinuousMove><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken><tptz:Velocity><tt:Zoom x="1"/></tptz:Velocity></tptz:ContinuousMove>"#).await);
        assert!(body.contains("no zoom"), "{body}");
        let body = fault(call(addr, &auth(), r#"<tptz:ContinuousMove><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x="1" y="0" space="http://example.com/other"/></tptz:Velocity></tptz:ContinuousMove>"#).await);
        assert!(body.contains("unsupported space"), "{body}");
        // Not schema-valid: ProfileToken is required.
        fault(call(addr, &auth(), r#"<tptz:ContinuousMove><tptz:Velocity><tt:PanTilt x="1" y="0"/></tptz:Velocity></tptz:ContinuousMove>"#).await);
        let body = fault(call(addr, &auth(), "<tptz:SetHomePosition><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken></tptz:SetHomePosition>").await);
        assert!(body.contains("unsupported action"), "{body}");

        // PTZ needs credentials like everything else.
        let (status, _) = call(addr, "", "<tptz:Stop/>").await;
        assert_eq!(status, 401);

        handle.shutdown().await.unwrap();
    }

    /// gSOAP clients (ONVIF Device Manager) declare every prefix on the
    /// Envelope and pick their own names for them; zeep declares them on
    /// the elements. Both must reach the motor.
    #[tokio::test(flavor = "current_thread")]
    async fn continuous_move_with_prefixes_declared_on_the_envelope() {
        let (ptz, clock) = manual_sim();
        let (addr, mut handle) = start(settings(Some(Arc::clone(&ptz)))).await;
        let auth = text_header("nvr", "correct horse");

        let (status, body) = post(
            addr,
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:sch="http://www.onvif.org/ver10/schema" xmlns:ptz="http://www.onvif.org/ver20/ptz/wsdl">{}<SOAP-ENV:Body><ptz:ContinuousMove><ptz:ProfileToken>Profile_vs0</ptz:ProfileToken><ptz:Velocity><sch:PanTilt x="-0.5" y="0.5"/></ptz:Velocity><ptz:Timeout>PT30S</ptz:Timeout></ptz:ContinuousMove></SOAP-ENV:Body></SOAP-ENV:Envelope>"#,
                auth.replace("s:Header", "SOAP-ENV:Header")
            ),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let _: tptz::ContinuousMoveResponse = parse(&body);
        clock.advance(std::time::Duration::from_secs(1));
        let (status, body) = call(
            addr,
            &auth,
            "<tptz:GetStatus><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken></tptz:GetStatus>",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (x, y, moving) = ptz_status(&body);
        assert!(x < 0.0 && y > 0.0, "{x} {y}");
        assert_eq!(moving, "MOVING");

        // zeep style: ns0/ns1, declared where used.
        let (status, body) = post(
            addr,
            &format!(
                r#"<soap-env:Envelope xmlns:soap-env="http://www.w3.org/2003/05/soap-envelope">{}<soap-env:Body><ns0:Stop xmlns:ns0="http://www.onvif.org/ver20/ptz/wsdl"><ns0:ProfileToken>Profile_vs0</ns0:ProfileToken><ns0:PanTilt>true</ns0:PanTilt></ns0:Stop></soap-env:Body></soap-env:Envelope>"#,
                text_header("nvr", "correct horse").replace("s:Header", "soap-env:Header")
            ),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = call(
            addr,
            &auth,
            "<tptz:GetStatus><tptz:ProfileToken>Profile_vs0</tptz:ProfileToken></tptz:GetStatus>",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(ptz_status(&body).2, "IDLE");

        handle.shutdown().await.unwrap();
    }

    /// The whole profile, as a WSDL-driven client reads it.
    #[tokio::test(flavor = "current_thread")]
    async fn get_profiles_round_trips_through_the_schema() {
        let (ptz, _clock) = manual_sim();
        let (addr, mut handle) = start(settings(Some(ptz))).await;
        let (status, body) = call(
            addr,
            &text_header("nvr", "correct horse"),
            "<trt:GetProfiles/>",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let profiles: trt::GetProfilesResponse = parse(&body);
        let [profile] = profiles.profiles.as_slice() else {
            panic!("one profile expected: {body}");
        };
        assert_eq!(
            (profile.token.as_str(), profile.name.as_str()),
            ("Profile_vs0", "Profile_vs0")
        );

        let source = profile.video_source_configuration.as_ref().unwrap();
        assert_eq!(source.token, "videoSrc0");
        assert_eq!(source.source_token, "videoSrc0");
        let bounds = &source.bounds;
        assert_eq!(
            (bounds.x, bounds.y, bounds.width, bounds.height),
            (0, 0, 1280, 720)
        );

        let encoder = profile.video_encoder_configuration.as_ref().unwrap();
        assert_eq!(encoder.token, "enc0");
        assert!(matches!(encoder.encoding, tt::VideoEncodingType::H264));
        assert_eq!(
            (encoder.resolution.width, encoder.resolution.height),
            (1280, 720)
        );
        let rate = encoder.rate_control.as_ref().unwrap();
        assert_eq!((rate.frame_rate_limit, rate.bitrate_limit), (25, 2000));
        assert!(matches!(
            encoder.h264.as_ref().unwrap().h264_profile,
            tt::H264ProfileType::Main
        ));

        let ptz = profile.ptz_configuration.as_ref().unwrap();
        assert_eq!(
            (ptz.token.as_str(), ptz.node_token.as_str()),
            ("ptz_config", "ptz_node")
        );
        let limits = &ptz.pan_tilt_limits.as_ref().unwrap().range;
        assert_eq!((limits.x_range.min, limits.x_range.max), (-1.0, 1.0));

        // Writing the parsed value back gives the same element.
        let soap = onvif_device_rs::server::parse_soap_request(&body).unwrap();
        let again = onvif_schema::to_xml(&profiles, "trt:GetProfilesResponse").unwrap();
        let reparsed: trt::GetProfilesResponse = onvif_schema::from_xml(&again).unwrap();
        assert_eq!(
            onvif_schema::to_xml(&reparsed, "trt:GetProfilesResponse").unwrap(),
            again
        );
        assert_eq!(soap.action, "GetProfilesResponse");

        handle.shutdown().await.unwrap();
    }

    /// Settings for the AV100 board profile (two sensors, 1280x720 at 15 fps,
    /// 2048 kbit/s) with the producer at `producer`.
    fn av100_settings(producer: String) -> Settings {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../boards/anyka-av100-ptz.toml");
        let video = board::load(&path).unwrap().video;
        Settings {
            producer: Some(producer),
            video: video
                .sensors
                .iter()
                .map(|&i| VideoStream::from_board(i, &video.stream_for(i)))
                .collect(),
            ..settings(None)
        }
    }

    /// `(token, width, height, fps, kbit/s, GOP, H.264 profile)` of each
    /// profile.
    fn summary(
        profiles: &trt::GetProfilesResponse,
    ) -> Vec<(String, i32, i32, i32, i32, i32, String)> {
        profiles
            .profiles
            .iter()
            .map(|p| {
                let encoder = p.video_encoder_configuration.as_ref().unwrap();
                let rate = encoder.rate_control.as_ref().unwrap();
                let h264 = encoder.h264.as_ref().unwrap();
                (
                    p.token.clone(),
                    encoder.resolution.width,
                    encoder.resolution.height,
                    rate.frame_rate_limit,
                    rate.bitrate_limit,
                    h264.gov_length,
                    format!("{:?}", h264.h264_profile),
                )
            })
            .collect()
    }

    fn running(index: u32, stream: video_proto::StreamInfo) -> video_proto::Status {
        video_proto::Status {
            index,
            state: "running".into(),
            stream: Some(stream),
            ..Default::default()
        }
    }

    /// PRO-02: one profile per sensor the producer reports, with its
    /// stream's parameters; each profile has its own stream and snapshot.
    #[tokio::test(flavor = "current_thread")]
    async fn profiles_follow_the_producer() {
        let statuses = vec![
            running(
                0,
                video_proto::StreamInfo {
                    codec: "H.264".into(),
                    profile: "High".into(),
                    width: 1920,
                    height: 1080,
                    fps: 25,
                    gop: 50,
                    target_kbps: 4096,
                    kbps: 4000.0,
                    encoder: "fake".into(),
                },
            ),
            running(
                1,
                video_proto::StreamInfo {
                    codec: "H.264".into(),
                    profile: "Baseline".into(),
                    width: 640,
                    height: 360,
                    fps: 10,
                    gop: 20,
                    target_kbps: 512,
                    kbps: 500.0,
                    encoder: "fake".into(),
                },
            ),
        ];
        let (producer, requests) = crate::producer::fake::serve(statuses).await;
        let (addr, mut handle) = start(av100_settings(producer)).await;
        let auth = || text_header("nvr", "correct horse");

        let (status, body) = call(addr, &auth(), "<trt:GetProfiles/>").await;
        assert_eq!(status, 200, "{body}");
        let profiles: trt::GetProfilesResponse = parse(&body);
        let s = |t: &str| t.to_owned();
        assert_eq!(
            summary(&profiles),
            [
                (s("Profile_vs0"), 1920, 1080, 25, 4096, 50, s("High")),
                (s("Profile_vs1"), 640, 360, 10, 512, 20, s("Baseline")),
            ]
        );
        let source = profiles.profiles[1]
            .video_source_configuration
            .as_ref()
            .unwrap();
        assert_eq!(
            (source.token.as_str(), source.bounds.width),
            ("videoSrc1", 640)
        );
        assert_eq!(
            profiles.profiles[1]
                .video_encoder_configuration
                .as_ref()
                .unwrap()
                .token,
            "enc1"
        );

        for (index, token) in [(0, "Profile_vs0"), (1, "Profile_vs1")] {
            let (status, body) = call(addr, &auth(), &format!(
                "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>{token}</trt:ProfileToken></trt:GetStreamUri>"
            ))
            .await;
            assert_eq!(status, 200, "{body}");
            let uri: trt::GetStreamUriResponse = parse(&body);
            assert_eq!(
                uri.media_uri.uri,
                format!("rtsp://192.0.2.10:554/vs{index}")
            );
            let (status, body) = call(addr, &auth(), &format!(
                "<trt:GetSnapshotUri><trt:ProfileToken>{token}</trt:ProfileToken></trt:GetSnapshotUri>"
            ))
            .await;
            assert_eq!(status, 200, "{body}");
            let uri: trt::GetSnapshotUriResponse = parse(&body);
            assert_eq!(
                uri.media_uri.uri,
                format!("http://192.0.2.10:80/api/snapshot.jpg?camera={index}")
            );
        }
        let (status, _) = call(
            addr,
            &auth(),
            "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>Profile_vs2</trt:ProfileToken></trt:GetStreamUri>",
        )
        .await;
        assert_eq!(status, 400);

        // Per-sensor configuration lookups.
        let one: trt::GetVideoEncoderConfigurationResponse = parse(&call(
            addr,
            &auth(),
            "<trt:GetVideoEncoderConfiguration><trt:ConfigurationToken>enc1</trt:ConfigurationToken></trt:GetVideoEncoderConfiguration>",
        )
        .await
        .1);
        assert_eq!(one.configuration.resolution.width, 640);
        let options: trt::GetVideoEncoderConfigurationOptionsResponse = parse(&call(
            addr,
            &auth(),
            "<trt:GetVideoEncoderConfigurationOptions><trt:ProfileToken>Profile_vs1</trt:ProfileToken></trt:GetVideoEncoderConfigurationOptions>",
        )
        .await
        .1);
        let h264 = options.options.h264.unwrap();
        assert_eq!(h264.resolutions_available[0].width, 640);
        assert_eq!(h264.frame_rate_range.max, 10);
        let sources: trt::GetVideoSourcesResponse =
            parse(&call(addr, &auth(), "<trt:GetVideoSources/>").await.1);
        assert_eq!(sources.video_sources.len(), 2);

        // The burst above was answered from one /status fetch.
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);

        handle.shutdown().await.unwrap();
    }

    /// A two-sensor board whose sensor 0 failed to open: the producer
    /// reports sensor 1 only, and its profile, stream and snapshot all say 1.
    #[tokio::test(flavor = "current_thread")]
    async fn profiles_use_the_sensor_index_when_one_is_missing() {
        let statuses = vec![running(
            1,
            video_proto::StreamInfo {
                codec: "H.264".into(),
                profile: "Main".into(),
                width: 640,
                height: 360,
                fps: 10,
                gop: 20,
                target_kbps: 512,
                kbps: 500.0,
                encoder: "fake".into(),
            },
        )];
        let (producer, _) = crate::producer::fake::serve(statuses).await;
        let (addr, mut handle) = start(av100_settings(producer)).await;
        let auth = || text_header("nvr", "correct horse");

        let (status, body) = call(addr, &auth(), "<trt:GetProfiles/>").await;
        assert_eq!(status, 200, "{body}");
        let profiles: trt::GetProfilesResponse = parse(&body);
        assert_eq!(
            summary(&profiles),
            [(
                "Profile_vs1".to_owned(),
                640,
                360,
                10,
                512,
                20,
                "Main".to_owned()
            )]
        );
        let (status, body) = call(addr, &auth(),
            "<trt:GetSnapshotUri><trt:ProfileToken>Profile_vs1</trt:ProfileToken></trt:GetSnapshotUri>",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let uri: trt::GetSnapshotUriResponse = parse(&body);
        assert_eq!(
            uri.media_uri.uri,
            "http://192.0.2.10:80/api/snapshot.jpg?camera=1"
        );
        let (status, body) = call(addr, &auth(),
            "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>Profile_vs1</trt:ProfileToken></trt:GetStreamUri>",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let uri: trt::GetStreamUriResponse = parse(&body);
        assert_eq!(uri.media_uri.uri, "rtsp://192.0.2.10:554/vs1");

        handle.shutdown().await.unwrap();
    }

    /// PRO-04: with the producer down, the board's streams are described
    /// and the URIs still answer.
    #[tokio::test(flavor = "current_thread")]
    async fn profiles_fall_back_to_the_board() {
        let settings = av100_settings(crate::producer::fake::dead_address());
        let (addr, mut handle) = start(settings).await;
        let auth = || text_header("nvr", "correct horse");

        let (status, body) = call(addr, &auth(), "<trt:GetProfiles/>").await;
        assert_eq!(status, 200, "{body}");
        let profiles: trt::GetProfilesResponse = parse(&body);
        let s = |t: &str| t.to_owned();
        assert_eq!(
            summary(&profiles),
            [
                (s("Profile_vs0"), 1280, 720, 15, 2048, 30, s("Main")),
                (s("Profile_vs1"), 1280, 720, 15, 2048, 30, s("Main")),
            ]
        );
        let (status, body) = call(
            addr,
            &auth(),
            "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>Profile_vs1</trt:ProfileToken></trt:GetStreamUri>",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let uri: trt::GetStreamUriResponse = parse(&body);
        assert_eq!(uri.media_uri.uri, "rtsp://192.0.2.10:554/vs1");

        handle.shutdown().await.unwrap();
    }

    /// PTZ hangs off the first sensor's profile only.
    #[tokio::test(flavor = "current_thread")]
    async fn ptz_is_on_the_first_profile() {
        let (ptz, _clock) = manual_sim();
        let settings = Settings {
            ptz: Some(ptz),
            ..av100_settings(crate::producer::fake::dead_address())
        };
        let (addr, mut handle) = start(settings).await;
        let (status, body) = call(
            addr,
            &text_header("nvr", "correct horse"),
            "<trt:GetProfiles/>",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let profiles: trt::GetProfilesResponse = parse(&body);
        let with_ptz: Vec<_> = profiles
            .profiles
            .iter()
            .map(|p| p.ptz_configuration.is_some())
            .collect();
        assert_eq!(with_ptz, [true, false]);
        handle.shutdown().await.unwrap();
    }

    type Seen = Arc<std::sync::Mutex<Vec<lights_proto::Request>>>;

    /// A lights owner on `path` that keeps the state in memory and records
    /// every request.
    fn fake_lights(path: &std::path::Path) -> Seen {
        use lights_proto::{Request as L, Response as R, State};
        use tokio::io::{AsyncBufReadExt, BufReader};
        let seen = Seen::default();
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path).unwrap();
        let record = Arc::clone(&seen);
        tokio::spawn(async move {
            let mut state = State::default();
            while let Ok((stream, _)) = listener.accept().await {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: L = serde_json::from_str(&line).unwrap();
                    record.lock().unwrap().push(request);
                    if let L::SetMode { mode } = request {
                        state.mode = mode;
                    }
                    let mut text = serde_json::to_string(&R::State { state }).unwrap();
                    text.push('\n');
                    write.write_all(text.as_bytes()).await.unwrap();
                }
            }
        });
        seen
    }

    fn lights_socket(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kamoder-onvif-lights-{}-{tag}.sock",
            std::process::id()
        ))
    }

    fn ir_cut(body: &str) -> tt::IrCutFilterModeType {
        let settings: timg::GetImagingSettingsResponse = parse(body);
        settings
            .imaging_settings
            .ir_cut_filter
            .expect("IrCutFilter")
    }

    fn set_ir_cut(mode: &str) -> String {
        format!("<timg:SetImagingSettings><timg:VideoSourceToken>videoSrc0</timg:VideoSourceToken><timg:ImagingSettings><tt:IrCutFilter>{mode}</tt:IrCutFilter></timg:ImagingSettings></timg:SetImagingSettings>")
    }

    const GET_IMAGING: &str = "<timg:GetImagingSettings><timg:VideoSourceToken>videoSrc0</timg:VideoSourceToken></timg:GetImagingSettings>";

    #[tokio::test(flavor = "current_thread")]
    async fn imaging_ir_cut_filter_through_the_lights_owner() {
        use lights_proto::{Mode, Request as L};
        let path = lights_socket("up");
        let seen = fake_lights(&path);
        let mut s = settings(None);
        s.lights = Some(lights_proto::Client::new(&path));
        let (addr, mut handle) = start(s).await;
        let auth = || text_header("nvr", "correct horse");
        let ok = |(status, body): (u16, String)| {
            assert_eq!(status, 200, "{body}");
            body
        };
        let fault = |(status, body): (u16, String)| {
            assert_eq!(status, 400, "{body}");
            assert!(body.contains("soap:Fault"), "{body}");
            body
        };

        // Advertised while the owner answers.
        let caps: tds::GetCapabilitiesResponse =
            parse(&ok(call(addr, &auth(), "<tds:GetCapabilities/>").await));
        assert_eq!(
            caps.capabilities.imaging.unwrap().x_addr,
            "http://192.0.2.10:8000/onvif/imaging_service"
        );
        let services: tds::GetServicesResponse =
            parse(&ok(call(addr, &auth(), "<tds:GetServices/>").await));
        let namespaces: Vec<_> = services
            .service
            .iter()
            .map(|service| service.namespace.as_str())
            .collect();
        assert_eq!(namespaces, [DEVICE_SERVICE, MEDIA_SERVICE, IMAGING_SERVICE]);

        // Day = filter engaged = ON.
        assert!(matches!(
            ir_cut(&ok(call(addr, &auth(), GET_IMAGING).await)),
            tt::IrCutFilterModeType::On
        ));
        let options: timg::GetOptionsResponse = parse(&ok(call(
            addr,
            &auth(),
            "<timg:GetOptions><timg:VideoSourceToken>videoSrc0</timg:VideoSourceToken></timg:GetOptions>",
        )
        .await));
        let modes = &options.imaging_options.ir_cut_filter_modes;
        assert!(
            matches!(
                modes.as_slice(),
                [tt::IrCutFilterModeType::On, tt::IrCutFilterModeType::Off]
            ),
            "{modes:?}"
        );

        // OFF = filter removed = night; ON goes back to day.
        seen.lock().unwrap().clear();
        let _: timg::SetImagingSettingsResponse =
            parse(&ok(call(addr, &auth(), &set_ir_cut("OFF")).await));
        assert!(matches!(
            ir_cut(&ok(call(addr, &auth(), GET_IMAGING).await)),
            tt::IrCutFilterModeType::Off
        ));
        ok(call(addr, &auth(), &set_ir_cut("ON")).await);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                L::SetMode { mode: Mode::Night },
                L::Get,
                L::SetMode { mode: Mode::Day },
            ]
        );

        // AUTO, other parameters, other sources and bad values are refused
        // without reaching the owner.
        seen.lock().unwrap().clear();
        let body = fault(call(addr, &auth(), &set_ir_cut("AUTO")).await);
        assert!(body.contains("AUTO is not supported"), "{body}");
        fault(call(addr, &auth(), &set_ir_cut("NIGHT")).await);
        let body = fault(call(addr, &auth(), "<timg:SetImagingSettings><timg:VideoSourceToken>videoSrc0</timg:VideoSourceToken><timg:ImagingSettings><tt:Brightness>50</tt:Brightness></timg:ImagingSettings></timg:SetImagingSettings>").await);
        assert!(body.contains("only IrCutFilter"), "{body}");
        fault(call(addr, &auth(), &GET_IMAGING.replace("videoSrc0", "other")).await);
        fault(
            call(
                addr,
                &auth(),
                &set_ir_cut("ON").replace("videoSrc0", "other"),
            )
            .await,
        );
        assert!(seen.lock().unwrap().is_empty());

        // Imaging needs credentials like everything else.
        let (status, _) = call(addr, "", GET_IMAGING).await;
        assert_eq!(status, 401);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn imaging_is_not_advertised_without_the_lights_owner() {
        let mut s = settings(None);
        s.lights = Some(lights_proto::Client::new(lights_socket("none")));
        let (addr, mut handle) = start(s).await;
        let auth = || text_header("nvr", "correct horse");

        let (status, body) = call(addr, &auth(), "<tds:GetCapabilities/>").await;
        assert_eq!(status, 200, "{body}");
        let caps: tds::GetCapabilitiesResponse = parse(&body);
        assert!(caps.capabilities.imaging.is_none());
        let (_, body) = call(addr, &auth(), "<tds:GetServices/>").await;
        let services: tds::GetServicesResponse = parse(&body);
        assert!(!services
            .service
            .iter()
            .any(|service| service.namespace == IMAGING_SERVICE));

        // Registered, but the owner is down: a Receiver fault, not a guess.
        let (status, body) = call(addr, &auth(), GET_IMAGING).await;
        assert_eq!(status, 500, "{body}");
        assert!(body.contains("lights unavailable"), "{body}");

        handle.shutdown().await.unwrap();
    }

    #[test]
    fn missing_credentials_are_refused_by_the_library() {
        // Belt and braces: even if main's check were bypassed, an empty
        // password without --no-auth does not produce an open server.
        let mut s = settings(None);
        s.secret = Some(Secret {
            user: "nvr".into(),
            password: String::new(),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            assert!(build(&s).unwrap().start_on(listener).await.is_err());
        });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_auth_mode_serves_everything() {
        let mut s = settings(None);
        s.secret = None;
        let (addr, mut handle) = start(s).await;
        let (status, body) = call(addr, "", "<tds:GetDeviceInformation/>").await;
        assert_eq!(status, 200, "{body}");
        handle.shutdown().await.unwrap();
    }
}
