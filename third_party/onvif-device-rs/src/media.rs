// ---------------------------------------------------------------------------
// ONVIF Media Service — handler implementations for GetProfiles,
// GetStreamUri, GetSnapshotUri, and GetVideoSources, plus the completion
// family (issue #48): the video encoder configuration set (list / get /
// options / set), GetGuaranteedNumberOfVideoEncoderInstances,
// SetSynchronizationPoint, the empty audio and OSD sets, and the media
// service capabilities — registered together by
// [`register_media_actions`].
//
// Each handler implements the OnvifActionHandler trait and holds a shared
// reference to OnvifMediaConfig (camera resolution, fps, bitrate, RTSP port,
// device IP). The completion family reads through a shared
// `Arc<RwLock<OnvifMediaConfig>>` store so SetVideoEncoderConfiguration
// mutations are visible to every reader; the four historical handler
// structs keep their immutable `Arc<OnvifMediaConfig>` API.
//
// StartMulticastStreaming / StopMulticastStreaming are deliberately NOT
// implemented: the device serves no RTP multicast and the capabilities
// answer advertises `RTPMulticast="false"` — acknowledging the actions
// anyway would be dishonest.
//
// GetStreamUri uses the per-request server IP (the local interface that
// received the ONVIF connection) so the returned RTSP URL is reachable from
// the caller.  When `server_ip` is empty — e.g. during unit tests or when
// local_addr could not be determined — it falls back to the startup-detected
// `device_ip`.
// ---------------------------------------------------------------------------

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};

use crate::server::{OnvifActionHandler, OnvifServer};
use crate::types::{
    resolve_server_ip, serialize_soap_response, OnvifError, RequestInfo, TextAccumulator,
};

/// Shared, mutable media configuration — the store behind
/// [`register_media_actions`] (issue #48). Readers snapshot through the
/// lock; [`SetVideoEncoderConfigurationHandler`] writes through it.
pub type SharedMediaConfig = Arc<RwLock<OnvifMediaConfig>>;

/// Read-guard the shared config, tolerating lock poisoning (a panicked
/// writer's snapshot is still plain data — mirrors `ptz_state`).
fn read_config(
    store: &RwLock<OnvifMediaConfig>,
) -> std::sync::RwLockReadGuard<'_, OnvifMediaConfig> {
    store
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Write-guard the shared config, tolerating lock poisoning.
fn write_config(
    store: &RwLock<OnvifMediaConfig>,
) -> std::sync::RwLockWriteGuard<'_, OnvifMediaConfig> {
    store
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Video encoder advertised by the Media service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEncoding {
    /// H.264 (`<Encoding>H264</Encoding>`).
    H264,
    /// H.265 / HEVC (`<Encoding>H265</Encoding>`).
    H265,
}

impl VideoEncoding {
    /// ONVIF wire token.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            VideoEncoding::H264 => "H264",
            VideoEncoding::H265 => "H265",
        }
    }
}

/// One additional media profile advertised by GetProfiles — e.g. a
/// low-resolution bandwidth-saving substream next to the primary stream.
///
/// The primary profile stays the flat [`OnvifMediaConfig`] fields;
/// extras are listed in [`OnvifMediaConfig::extra_profiles`] in
/// advertisement order, always after the primary. Profile S consumers
/// that pick "the first profile" therefore keep getting the primary
/// stream.
#[derive(Debug, Clone)]
pub struct MediaProfileConfig {
    /// Profile token (`<Profiles token="...">`); matched against the
    /// `ProfileToken` argument of GetStreamUri.
    pub token: String,
    /// Human-readable profile name (`<Name>`); defaults to the token.
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    /// Advertised video encoder for this profile (default H.264).
    pub encoding: VideoEncoding,
    /// Video encoder configuration token; defaults to `<token>_encoder`,
    /// mirroring the onvif-go twin's derivation.
    pub encoder_token: String,
    /// RTSP URL path returned by GetStreamUri when this profile's token
    /// is requested.
    pub stream_path: String,
}

impl MediaProfileConfig {
    /// Construct with H.264 encoding, `name` = `token`, and the encoder
    /// token derived as `<token>_encoder`.
    #[must_use]
    pub fn new(
        token: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        stream_path: &str,
    ) -> Self {
        Self {
            token: token.to_string(),
            name: token.to_string(),
            width,
            height,
            fps,
            bitrate,
            encoding: VideoEncoding::H264,
            encoder_token: format!("{token}_encoder"),
            stream_path: stream_path.to_string(),
        }
    }
}

/// Camera/media configuration consumed by the ONVIF Media service handlers.
#[derive(Debug, Clone)]
pub struct OnvifMediaConfig {
    pub camera_width: u32,
    pub camera_height: u32,
    pub camera_fps: u32,
    pub camera_bitrate: u32,
    pub rtsp_port: u16,
    pub device_ip: String,
    /// RTSP URL path returned by GetStreamUri (default `/stream`).
    ///
    /// Hosts whose RTSP server serves streams under a different path
    /// (e.g. notebook-cam's `/live/{camera_id}`) set this so the advertised
    /// URI actually resolves.
    pub stream_path: String,
    /// HTTP port of the host's JPEG snapshot endpoint, advertised by
    /// GetSnapshotUri (parameterless form, mirroring onvif-go's
    /// `SnapshotURIParameterless`). `0` disables the feature — GetSnapshotUri
    /// then faults with "snapshot not supported", the twin's
    /// `ErrSnapshotNotSupported` behavior.
    pub snapshot_port: u16,
    /// HTTP path of the snapshot endpoint (default `/snapshot.jpg`).
    pub snapshot_path: String,
    /// Profile token in GetProfiles (default `main`).
    pub profile_token: String,
    /// Video source token (default `videoSrc0`).
    pub video_source_token: String,
    /// Video encoder configuration token (default `enc0`).
    pub encoder_token: String,
    /// Advertised video encoder (default H.264; set H.265 for H.265 hosts).
    pub encoding: VideoEncoding,
    /// Human name of the video source in GetVideoSources (default
    /// `Video Source`). This is host identity, not a product name.
    pub video_source_name: String,
    /// Additional profiles advertised after the primary one (default
    /// empty). Each entry pairs its own geometry and RTSP path with a
    /// token; GetStreamUri resolves the request's `ProfileToken`
    /// against these, failing open to the primary stream for unknown or
    /// missing tokens.
    pub extra_profiles: Vec<MediaProfileConfig>,
}

impl OnvifMediaConfig {
    /// Construct with the historical default tokens (`main` / `videoSrc0` /
    /// `enc0`), H.264 encoding, and stream path `/stream`.
    #[must_use]
    pub fn new(
        camera_width: u32,
        camera_height: u32,
        camera_fps: u32,
        camera_bitrate: u32,
        rtsp_port: u16,
        device_ip: String,
    ) -> Self {
        Self {
            camera_width,
            camera_height,
            camera_fps,
            camera_bitrate,
            rtsp_port,
            device_ip,
            stream_path: "/stream".to_string(),
            snapshot_port: 0,
            snapshot_path: "/snapshot.jpg".to_string(),
            profile_token: "main".to_string(),
            video_source_token: "videoSrc0".to_string(),
            encoder_token: "enc0".to_string(),
            encoding: VideoEncoding::H264,
            video_source_name: "Video Source".to_string(),
            extra_profiles: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// XML building helper
// ---------------------------------------------------------------------------

fn write_text_element(writer: &mut Writer<Vec<u8>>, name: &str, text: &str) {
    let _ = writer.write_event(Event::Start(BytesStart::new(name)));
    let _ = writer.write_event(Event::Text(BytesText::from_escaped(
        crate::types::xml_escape(text),
    )));
    let _ = writer.write_event(Event::End(BytesEnd::new(name)));
}

// ---------------------------------------------------------------------------
// GetProfilesHandler
// ---------------------------------------------------------------------------

/// Geometry + tokens of one profile as written by [`GetProfilesHandler`].
struct ProfileWire<'a> {
    token: &'a str,
    name: &'a str,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    encoding: VideoEncoding,
    encoder_token: &'a str,
}

fn write_profile(
    writer: &mut Writer<Vec<u8>>,
    profile: &ProfileWire<'_>,
    video_source_token: &str,
) {
    // <Profiles token="...">
    let mut profiles = BytesStart::new("Profiles");
    profiles.push_attribute(("token", profile.token));
    writer
        .write_event(Event::Start(profiles))
        .unwrap_or_default();

    write_text_element(writer, "Name", profile.name);

    // <VideoSourceConfiguration token="...">
    let mut vs_cfg = BytesStart::new("VideoSourceConfiguration");
    vs_cfg.push_attribute(("token", video_source_token));
    writer.write_event(Event::Start(vs_cfg)).unwrap_or_default();
    write_text_element(writer, "Name", "VideoSourceConfig");
    write_text_element(writer, "SourceToken", video_source_token);
    write_text_element(writer, "UseCount", "1");
    // <Bounds width="W" height="H"/>
    let mut bounds = BytesStart::new("Bounds");
    let w_str = profile.width.to_string();
    let h_str = profile.height.to_string();
    bounds.push_attribute(("width", w_str.as_str()));
    bounds.push_attribute(("height", h_str.as_str()));
    writer.write_event(Event::Empty(bounds)).unwrap_or_default();
    writer
        .write_event(Event::End(BytesEnd::new("VideoSourceConfiguration")))
        .unwrap_or_default();

    // <VideoEncoderConfiguration token="..."> — the shared encoder block
    // (byte-identical to the standalone GetVideoEncoderConfiguration(s)
    // listings, which use the same helper).
    write_encoder_block(
        writer,
        "VideoEncoderConfiguration",
        &EncoderEntryWire {
            token: profile.encoder_token,
            width: profile.width,
            height: profile.height,
            fps: profile.fps,
            bitrate: profile.bitrate,
            encoding: profile.encoding,
        },
    );

    writer
        .write_event(Event::End(BytesEnd::new("Profiles")))
        .unwrap_or_default();
}

/// One video encoder configuration as a `<{element_name} token="...">`
/// block carrying Name/UseCount/Encoding/Resolution/RateControl — the
/// exact element set [`GetProfilesHandler`] writes for a profile's
/// encoder. The standalone encoder listings reuse it with a different
/// element name (`Configurations` / `Configuration`) so their fields
/// cannot drift from the profile advertisement (issue #48).
fn write_encoder_block(
    writer: &mut Writer<Vec<u8>>,
    element_name: &str,
    entry: &EncoderEntryWire<'_>,
) {
    let mut ve_cfg = BytesStart::new(element_name);
    ve_cfg.push_attribute(("token", entry.token));
    writer.write_event(Event::Start(ve_cfg)).unwrap_or_default();
    write_text_element(writer, "Name", "VideoEncoderConfig");
    write_text_element(writer, "UseCount", "1");
    write_text_element(writer, "Encoding", entry.encoding.as_str());

    // <Resolution>
    writer
        .write_event(Event::Start(BytesStart::new("Resolution")))
        .unwrap_or_default();
    write_text_element(writer, "Width", &entry.width.to_string());
    write_text_element(writer, "Height", &entry.height.to_string());
    writer
        .write_event(Event::End(BytesEnd::new("Resolution")))
        .unwrap_or_default();

    // <RateControl>
    writer
        .write_event(Event::Start(BytesStart::new("RateControl")))
        .unwrap_or_default();
    write_text_element(writer, "FrameRateLimit", &entry.fps.to_string());
    write_text_element(writer, "BitrateLimit", &entry.bitrate.to_string());
    write_text_element(writer, "EncodingInterval", "1");
    writer
        .write_event(Event::End(BytesEnd::new("RateControl")))
        .unwrap_or_default();

    writer
        .write_event(Event::End(BytesEnd::new(element_name)))
        .unwrap_or_default();
}

/// Handler for the ONVIF GetProfiles SOAP action.
///
/// Returns Profile S compatible profiles — the primary one built from the
/// flat [`OnvifMediaConfig`] fields, followed by any
/// [`OnvifMediaConfig::extra_profiles`] (e.g. a low-resolution substream):
/// - VideoSourceConfiguration (bounds from the profile resolution)
/// - VideoEncoderConfiguration (rate control from the profile)
pub struct GetProfilesHandler {
    config: Arc<OnvifMediaConfig>,
}

impl GetProfilesHandler {
    pub fn new(config: Arc<OnvifMediaConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl OnvifActionHandler for GetProfilesHandler {
    async fn handle(&self, _body: &str, _request_info: &RequestInfo) -> Result<String, OnvifError> {
        build_get_profiles_response(&self.config)
    }
}

/// Build the GetProfiles response body for `config` — shared by the
/// standalone [`GetProfilesHandler`] and the store-backed registration
/// path (byte-identical output, issue #48).
fn build_get_profiles_response(config: &OnvifMediaConfig) -> Result<String, OnvifError> {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);

    // <GetProfilesResponse>
    writer
        .write_event(Event::Start(BytesStart::new("GetProfilesResponse")))
        .unwrap_or_default();

    write_profile(
        &mut writer,
        &ProfileWire {
            token: &config.profile_token,
            name: &config.profile_token,
            width: config.camera_width,
            height: config.camera_height,
            fps: config.camera_fps,
            bitrate: config.camera_bitrate,
            encoding: config.encoding,
            encoder_token: &config.encoder_token,
        },
        &config.video_source_token,
    );

    for extra in &config.extra_profiles {
        write_profile(
            &mut writer,
            &ProfileWire {
                token: &extra.token,
                name: &extra.name,
                width: extra.width,
                height: extra.height,
                fps: extra.fps,
                bitrate: extra.bitrate,
                encoding: extra.encoding,
                encoder_token: &extra.encoder_token,
            },
            &config.video_source_token,
        );
    }

    writer
        .write_event(Event::End(BytesEnd::new("GetProfilesResponse")))
        .unwrap_or_default();

    let body = String::from_utf8(writer.into_inner())
        .map_err(|e| OnvifError::Internal(format!("non-UTF-8 output from XML writer: {e}")))?;
    Ok(serialize_soap_response(&body))
}

// ---------------------------------------------------------------------------
// GetStreamUriHandler
// ---------------------------------------------------------------------------

/// Extract the text of the first element whose local name is `local`.
///
/// Tolerant by design: namespace prefixes (`tt:ConfigurationToken`),
/// attributes on the open tag, surrounding whitespace, and any malformed
/// input are all accepted — anything unparseable yields `None`. Never
/// panics on arbitrary input.
fn parse_element_text<'a>(body: &'a str, local_target: &str) -> Option<&'a str> {
    let mut rest = body;
    while let Some(open) = rest.find('<') {
        let after_open = &rest[open + 1..];
        let Some(tag_end) = after_open.find(|c: char| c == '>' || c.is_whitespace()) else {
            break;
        };
        let tag = &after_open[..tag_end];
        // Local name — strip any namespace prefix.
        let local = tag.rsplit(':').next().unwrap_or(tag);
        let Some(greater) = after_open.find('>') else {
            break;
        };
        if after_open[..greater].ends_with('/') {
            // Empty element (`<ProfileToken/>`) — no token text.
            rest = &after_open[greater..];
            continue;
        }
        if local == local_target {
            let content = &after_open[greater + 1..];
            let close = content.find('<')?;
            let token = content[..close].trim();
            return (!token.is_empty()).then_some(token);
        }
        rest = &after_open[tag_end..];
    }
    None
}

/// Extract the `ProfileToken` element text from a GetStreamUri SOAP body
/// (see [`parse_element_text`] for the tolerance contract).
fn parse_profile_token(body: &str) -> Option<&str> {
    parse_element_text(body, "ProfileToken")
}

/// Handler for the ONVIF GetStreamUri SOAP action.
///
/// Returns an RTSP URL built from the device IP and the configured RTSP
/// port. When the request carries a `ProfileToken` matching an entry of
/// [`OnvifMediaConfig::extra_profiles`], that profile's `stream_path` is
/// advertised; missing or unknown tokens fail open to the primary
/// `stream_path` (the historical single-profile behavior).
pub struct GetStreamUriHandler {
    config: Arc<OnvifMediaConfig>,
}

impl GetStreamUriHandler {
    pub fn new(config: Arc<OnvifMediaConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl OnvifActionHandler for GetStreamUriHandler {
    async fn handle(&self, body: &str, request_info: &RequestInfo) -> Result<String, OnvifError> {
        build_get_stream_uri_response(&self.config, body, request_info)
    }
}

/// Build the GetStreamUri response for `config` — shared by the standalone
/// [`GetStreamUriHandler`] and the store-backed registration path
/// (byte-identical output, issue #48).
fn build_get_stream_uri_response(
    config: &OnvifMediaConfig,
    body: &str,
    request_info: &RequestInfo,
) -> Result<String, OnvifError> {
    let ip = resolve_server_ip(&request_info.server_ip, &config.device_ip);
    let stream_path = parse_profile_token(body)
        .and_then(|token| {
            config
                .extra_profiles
                .iter()
                .find(|p| p.token == token)
                .map(|p| p.stream_path.as_str())
        })
        .unwrap_or(&config.stream_path);
    let uri = format!("rtsp://{}:{}{}", ip, config.rtsp_port, stream_path);

    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    writer
        .write_event(Event::Start(BytesStart::new("GetStreamUriResponse")))
        .unwrap_or_default();

    writer
        .write_event(Event::Start(BytesStart::new("MediaUri")))
        .unwrap_or_default();
    write_text_element(&mut writer, "Uri", &uri);
    write_text_element(&mut writer, "InvalidAfterConnect", "false");
    write_text_element(&mut writer, "InvalidAfterReboot", "false");
    write_text_element(&mut writer, "Timeout", "PT0S");
    writer
        .write_event(Event::End(BytesEnd::new("MediaUri")))
        .unwrap_or_default();

    writer
        .write_event(Event::End(BytesEnd::new("GetStreamUriResponse")))
        .unwrap_or_default();

    let body = String::from_utf8(writer.into_inner())
        .map_err(|e| OnvifError::Internal(format!("non-UTF-8 output from XML writer: {e}")))?;
    Ok(serialize_soap_response(&body))
}

// ---------------------------------------------------------------------------
// GetSnapshotUriHandler
// ---------------------------------------------------------------------------

/// Handler for the ONVIF GetSnapshotUri SOAP action.
///
/// Advertises the host's HTTP JPEG snapshot endpoint (parameterless form,
/// mirroring onvif-go's `SnapshotURIParameterless`). Configure
/// [`OnvifMediaConfig::snapshot_port`] with the port of the HTTP server that
/// actually serves the JPEG; `0` (default) keeps the feature off and faults
/// with "snapshot not supported" — the twin's `ErrSnapshotNotSupported`.
pub struct GetSnapshotUriHandler {
    config: Arc<OnvifMediaConfig>,
}

impl GetSnapshotUriHandler {
    pub fn new(config: Arc<OnvifMediaConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl OnvifActionHandler for GetSnapshotUriHandler {
    async fn handle(&self, _body: &str, request_info: &RequestInfo) -> Result<String, OnvifError> {
        build_get_snapshot_uri_response(&self.config, request_info)
    }
}

/// Build the GetSnapshotUri response for `config` — shared by the
/// standalone [`GetSnapshotUriHandler`] and the store-backed
/// registration path (byte-identical output, issue #48).
fn build_get_snapshot_uri_response(
    config: &OnvifMediaConfig,
    request_info: &RequestInfo,
) -> Result<String, OnvifError> {
    if config.snapshot_port == 0 {
        return Err(OnvifError::ActionNotSupported(
            "snapshot not supported (OnvifMediaConfig::snapshot_port is unset)".to_string(),
        ));
    }
    let ip = resolve_server_ip(&request_info.server_ip, &config.device_ip);
    let uri = format!(
        "http://{}:{}{}",
        ip, config.snapshot_port, config.snapshot_path
    );

    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    writer
        .write_event(Event::Start(BytesStart::new("GetSnapshotUriResponse")))
        .unwrap_or_default();

    writer
        .write_event(Event::Start(BytesStart::new("MediaUri")))
        .unwrap_or_default();
    write_text_element(&mut writer, "Uri", &uri);
    write_text_element(&mut writer, "InvalidAfterConnect", "false");
    write_text_element(&mut writer, "InvalidAfterReboot", "true");
    write_text_element(&mut writer, "Timeout", "PT5S");
    writer
        .write_event(Event::End(BytesEnd::new("MediaUri")))
        .unwrap_or_default();

    writer
        .write_event(Event::End(BytesEnd::new("GetSnapshotUriResponse")))
        .unwrap_or_default();

    let body = String::from_utf8(writer.into_inner())
        .map_err(|e| OnvifError::Internal(format!("non-UTF-8 output from XML writer: {e}")))?;
    Ok(serialize_soap_response(&body))
}

// ---------------------------------------------------------------------------
// GetVideoSourcesHandler
// ---------------------------------------------------------------------------

/// Handler for the ONVIF GetVideoSources SOAP action.
///
/// Returns a single physical video source whose token and name come from
/// [`OnvifMediaConfig`]. By design (byte-stability against raw-SOAP NVR
/// matching) the response carries only `token` + `Name` — resolution and
/// frame rate live in the profile (GetProfiles), not here.
pub struct GetVideoSourcesHandler {
    config: Arc<OnvifMediaConfig>,
}

impl GetVideoSourcesHandler {
    pub fn new(config: Arc<OnvifMediaConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl OnvifActionHandler for GetVideoSourcesHandler {
    async fn handle(&self, _body: &str, _request_info: &RequestInfo) -> Result<String, OnvifError> {
        build_get_video_sources_response(&self.config)
    }
}

/// Build the GetVideoSources response for `config` — shared by the
/// standalone [`GetVideoSourcesHandler`] and the store-backed
/// registration path (byte-identical output, issue #48).
fn build_get_video_sources_response(config: &OnvifMediaConfig) -> Result<String, OnvifError> {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    writer
        .write_event(Event::Start(BytesStart::new("GetVideoSourcesResponse")))
        .unwrap_or_default();

    // <VideoSources token="...">
    {
        let mut vs = BytesStart::new("VideoSources");
        vs.push_attribute(("token", config.video_source_token.as_str()));
        writer.write_event(Event::Start(vs)).unwrap_or_default();
    }
    write_text_element(&mut writer, "Name", &config.video_source_name);
    writer
        .write_event(Event::End(BytesEnd::new("VideoSources")))
        .unwrap_or_default();

    writer
        .write_event(Event::End(BytesEnd::new("GetVideoSourcesResponse")))
        .unwrap_or_default();

    let body = String::from_utf8(writer.into_inner())
        .map_err(|e| OnvifError::Internal(format!("non-UTF-8 output from XML writer: {e}")))?;
    Ok(serialize_soap_response(&body))
}

// ---------------------------------------------------------------------------
// Media service completion (issue #48)
// ---------------------------------------------------------------------------

/// One advertised encoder entry — the primary flat config or an extra
/// profile's geometry, as listed by the standalone encoder actions.
struct EncoderEntryWire<'a> {
    token: &'a str,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    encoding: VideoEncoding,
}

/// The encoder configurations GetProfiles advertises — the primary flat
/// config first, then the extra profiles, in advertisement order.
fn encoder_entries(config: &OnvifMediaConfig) -> Vec<EncoderEntryWire<'_>> {
    let mut out = vec![EncoderEntryWire {
        token: &config.encoder_token,
        width: config.camera_width,
        height: config.camera_height,
        fps: config.camera_fps,
        bitrate: config.camera_bitrate,
        encoding: config.encoding,
    }];
    for extra in &config.extra_profiles {
        out.push(EncoderEntryWire {
            token: &extra.encoder_token,
            width: extra.width,
            height: extra.height,
            fps: extra.fps,
            bitrate: extra.bitrate,
            encoding: extra.encoding,
        });
    }
    out
}

/// Handler for GetVideoEncoderConfigurations — lists every encoder
/// configuration the profiles advertise (primary + extras), each as a
/// `<Configurations token="...">` block with the same fields GetProfiles
/// writes for its VideoEncoderConfiguration (issue #48).
pub struct GetVideoEncoderConfigurationsHandler {
    config: SharedMediaConfig,
}

impl GetVideoEncoderConfigurationsHandler {
    #[must_use]
    pub fn new(config: SharedMediaConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl OnvifActionHandler for GetVideoEncoderConfigurationsHandler {
    async fn handle(&self, _body: &str, _request_info: &RequestInfo) -> Result<String, OnvifError> {
        let config = read_config(&self.config);
        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        writer
            .write_event(Event::Start(BytesStart::new(
                "GetVideoEncoderConfigurationsResponse",
            )))
            .unwrap_or_default();
        for entry in encoder_entries(&config) {
            write_encoder_block(&mut writer, "Configurations", &entry);
        }
        writer
            .write_event(Event::End(BytesEnd::new(
                "GetVideoEncoderConfigurationsResponse",
            )))
            .unwrap_or_default();
        let body = String::from_utf8(writer.into_inner())
            .map_err(|e| OnvifError::Internal(format!("non-UTF-8 output from XML writer: {e}")))?;
        Ok(serialize_soap_response(&body))
    }
}

/// Handler for GetVideoEncoderConfiguration — the single configuration
/// named by the request's `ConfigurationToken` (namespace-tolerant
/// parse); unknown or missing tokens are Sender faults (issue #48).
pub struct GetVideoEncoderConfigurationHandler {
    config: SharedMediaConfig,
}

impl GetVideoEncoderConfigurationHandler {
    #[must_use]
    pub fn new(config: SharedMediaConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl OnvifActionHandler for GetVideoEncoderConfigurationHandler {
    async fn handle(&self, body: &str, _request_info: &RequestInfo) -> Result<String, OnvifError> {
        let token = parse_element_text(body, "ConfigurationToken").ok_or_else(|| {
            OnvifError::SenderFault(
                "configuration not found: no ConfigurationToken in request".to_string(),
            )
        })?;
        let config = read_config(&self.config);
        let entry = encoder_entries(&config)
            .into_iter()
            .find(|e| e.token == token)
            .ok_or_else(|| OnvifError::SenderFault(format!("configuration not found: {token}")))?;

        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        writer
            .write_event(Event::Start(BytesStart::new(
                "GetVideoEncoderConfigurationResponse",
            )))
            .unwrap_or_default();
        write_encoder_block(&mut writer, "Configuration", &entry);
        writer
            .write_event(Event::End(BytesEnd::new(
                "GetVideoEncoderConfigurationResponse",
            )))
            .unwrap_or_default();
        let body = String::from_utf8(writer.into_inner())
            .map_err(|e| OnvifError::Internal(format!("non-UTF-8 output from XML writer: {e}")))?;
        Ok(serialize_soap_response(&body))
    }
}

/// `<name><Min>min</Min><Max>max</Max></name>` (tt:IntRange).
fn write_int_range(writer: &mut Writer<Vec<u8>>, name: &str, min: u32, max: u32) {
    writer
        .write_event(Event::Start(BytesStart::new(name)))
        .unwrap_or_default();
    write_text_element(writer, "Min", &min.to_string());
    write_text_element(writer, "Max", &max.to_string());
    writer
        .write_event(Event::End(BytesEnd::new(name)))
        .unwrap_or_default();
}

/// Handler for GetVideoEncoderConfigurationOptions — the quality range
/// plus the codec options for the advertised encoding (issue #48).
///
/// For H.264 that is the WSDL's `tt:H264Options`: every advertised
/// resolution (primary + extras), the gov-length / frame-rate /
/// encoding-interval ranges derived from the primary config's fps. For
/// H.265 the ver10 media WSDL has no codec options element — the honest
/// answer is the quality range only. JPEG options are never advertised:
/// this device encodes no JPEG video.
pub struct GetVideoEncoderConfigurationOptionsHandler {
    config: SharedMediaConfig,
}

impl GetVideoEncoderConfigurationOptionsHandler {
    #[must_use]
    pub fn new(config: SharedMediaConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl OnvifActionHandler for GetVideoEncoderConfigurationOptionsHandler {
    async fn handle(&self, _body: &str, _request_info: &RequestInfo) -> Result<String, OnvifError> {
        let config = read_config(&self.config);
        let fps = config.camera_fps.max(1);
        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        writer
            .write_event(Event::Start(BytesStart::new(
                "GetVideoEncoderConfigurationOptionsResponse",
            )))
            .unwrap_or_default();
        writer
            .write_event(Event::Start(BytesStart::new("Options")))
            .unwrap_or_default();

        write_int_range(&mut writer, "QualityRange", 1, 10);

        if config.encoding == VideoEncoding::H264 {
            writer
                .write_event(Event::Start(BytesStart::new("H264")))
                .unwrap_or_default();
            for entry in encoder_entries(&config) {
                let mut res = BytesStart::new("ResolutionsAvailable");
                let w = entry.width.to_string();
                let h = entry.height.to_string();
                res.push_attribute(("Width", w.as_str()));
                res.push_attribute(("Height", h.as_str()));
                writer.write_event(Event::Empty(res)).unwrap_or_default();
            }
            write_int_range(&mut writer, "GovLengthRange", 1, fps);
            write_int_range(&mut writer, "FrameRateRange", 1, fps);
            write_int_range(&mut writer, "EncodingIntervalRange", 1, 1);
            for profile in ["Baseline", "Main", "High"] {
                write_text_element(&mut writer, "H264ProfilesSupported", profile);
            }
            writer
                .write_event(Event::End(BytesEnd::new("H264")))
                .unwrap_or_default();
        }

        writer
            .write_event(Event::End(BytesEnd::new("Options")))
            .unwrap_or_default();
        writer
            .write_event(Event::End(BytesEnd::new(
                "GetVideoEncoderConfigurationOptionsResponse",
            )))
            .unwrap_or_default();
        let body = String::from_utf8(writer.into_inner())
            .map_err(|e| OnvifError::Internal(format!("non-UTF-8 output from XML writer: {e}")))?;
        Ok(serialize_soap_response(&body))
    }
}

/// Values parsed from a SetVideoEncoderConfiguration request
/// (namespace-agnostic local names; absent fields stay `None` so the
/// stored value is left unchanged).
#[derive(Debug, Default)]
struct ParsedEncoderConfig {
    token: Option<String>,
    encoding: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<u32>,
    bitrate: Option<u32>,
    encoding_interval: Option<u32>,
}

/// Local name of a qualified XML name (`tt:Width` → `Width`).
fn local_name(qname: &[u8]) -> &str {
    let s = std::str::from_utf8(qname).unwrap_or("");
    s.rsplit(':').next().unwrap_or(s)
}

/// Value of the attribute whose (local) name is `attr`, if present.
fn attr_local_value(e: &BytesStart<'_>, attr: &str) -> Option<String> {
    for a in e.attributes().flatten() {
        let key = std::str::from_utf8(a.key.as_ref()).unwrap_or("");
        let key_local = key.rsplit(':').next().unwrap_or(key);
        if key_local == attr {
            return std::str::from_utf8(&a.value).ok().map(|v| v.to_string());
        }
    }
    None
}

/// Parse a positive integer request field; anything else is a Sender
/// fault (the client sent it, the client owns the mistake).
fn parse_u32_field(field: &str, text: &str) -> Result<u32, OnvifError> {
    let v = text
        .parse::<u32>()
        .map_err(|_| OnvifError::SenderFault(format!("invalid {field}: {text}")))?;
    if v == 0 {
        return Err(OnvifError::SenderFault(format!(
            "invalid {field}: {text} (must be > 0)"
        )));
    }
    Ok(v)
}

/// Dispatch one accumulated text region into the parse result by
/// element local name (`Name`/`UseCount` accepted and ignored).
fn assign_text_field(
    result: &mut ParsedEncoderConfig,
    field: &str,
    text: &str,
) -> Result<(), OnvifError> {
    match field {
        "Encoding" => result.encoding = Some(text.to_string()),
        "Width" => result.width = Some(parse_u32_field("Width", text)?),
        "Height" => result.height = Some(parse_u32_field("Height", text)?),
        "FrameRateLimit" => result.frame_rate = Some(parse_u32_field("FrameRateLimit", text)?),
        "BitrateLimit" => result.bitrate = Some(parse_u32_field("BitrateLimit", text)?),
        "EncodingInterval" => {
            result.encoding_interval = Some(parse_u32_field("EncodingInterval", text)?)
        }
        // Name/UseCount are accepted and ignored.
        _ => {}
    }
    Ok(())
}

/// Parse a SetVideoEncoderConfiguration body: the `token` attribute of
/// the `Configuration` element plus its Encoding / Resolution /
/// RateControl children (namespace-agnostic; quick-xml streaming with
/// entity-aware text accumulation). Malformed input is a Sender fault,
/// never a panic.
fn parse_encoder_configuration(body: &str) -> Result<ParsedEncoderConfig, OnvifError> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut result = ParsedEncoderConfig::default();
    let mut in_configuration = false;
    let mut current_field = String::new();
    let mut text_acc = TextAccumulator::new();

    loop {
        let event = reader.read_event_into(&mut buf);
        match event {
            Ok(Event::Text(e)) => {
                text_acc
                    .push_text(&e)
                    .map_err(|e| OnvifError::SenderFault(format!("XML text error: {e}")))?;
            }
            Ok(Event::GeneralRef(e)) => {
                text_acc
                    .push_ref(&e)
                    .map_err(|e| OnvifError::SenderFault(format!("XML entity error: {e}")))?;
            }
            other => {
                match text_acc.flush() {
                    Some(text) => {
                        if !text.is_empty() && in_configuration && !current_field.is_empty() {
                            assign_text_field(&mut result, &current_field, &text)?;
                        }
                    }
                    None => {
                        return Err(OnvifError::SenderFault(
                            "unresolvable XML entity in request".to_string(),
                        ))
                    }
                }
                match other {
                    Ok(Event::Start(e)) => {
                        let name = e.name();
                        let local = local_name(name.as_ref());
                        if local == "Configuration" {
                            in_configuration = true;
                            if result.token.is_none() {
                                result.token = attr_local_value(&e, "token");
                            }
                        } else if in_configuration {
                            current_field = local.to_string();
                        }
                    }
                    Ok(Event::Empty(e)) => {
                        let name = e.name();
                        let local = local_name(name.as_ref());
                        if local == "Configuration" && result.token.is_none() {
                            result.token = attr_local_value(&e, "token");
                        }
                    }
                    Ok(Event::End(e)) => {
                        let name = e.name();
                        let local = local_name(name.as_ref());
                        if local == "Configuration" {
                            in_configuration = false;
                        }
                        current_field.clear();
                    }
                    Ok(Event::Eof) => break,
                    Err(e) => return Err(OnvifError::SenderFault(format!("XML parse error: {e}"))),
                    _ => {}
                }
            }
        }
        buf.clear();
    }
    Ok(result)
}

/// Handler for SetVideoEncoderConfiguration — applies the client's
/// encoder changes (resolution, rate control, encoding) to the stored
/// config behind the shared [`SharedMediaConfig`] store, so every
/// reader (GetProfiles, the encoder listings) reflects them (issue #48).
///
/// Absent fields are left unchanged. The `Configuration` token must name
/// the primary or an extra profile's encoder configuration. JPEG is a
/// valid ONVIF token but unsupported here (this device encodes
/// H.264/H.265 only); unknown encodings and encoding intervals other
/// than 1 (the encoder writes every frame) are rejected as Sender
/// faults instead of being silently ignored.
pub struct SetVideoEncoderConfigurationHandler {
    config: SharedMediaConfig,
}

impl SetVideoEncoderConfigurationHandler {
    #[must_use]
    pub fn new(config: SharedMediaConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl OnvifActionHandler for SetVideoEncoderConfigurationHandler {
    async fn handle(&self, body: &str, _request_info: &RequestInfo) -> Result<String, OnvifError> {
        let parsed = parse_encoder_configuration(body)?;
        let token = parsed.token.ok_or_else(|| {
            OnvifError::SenderFault(
                "configuration not found: no Configuration/token in request".to_string(),
            )
        })?;
        let encoding = match parsed.encoding.as_deref() {
            None => None,
            Some("H264") => Some(VideoEncoding::H264),
            Some("H265") => Some(VideoEncoding::H265),
            Some("JPEG") => {
                return Err(OnvifError::SenderFault(
                    "encoding JPEG not supported by this device (H264/H265 only)".to_string(),
                ))
            }
            Some(other) => {
                return Err(OnvifError::SenderFault(format!(
                    "invalid encoding: {other}"
                )))
            }
        };
        if parsed.encoding_interval.is_some_and(|v| v != 1) {
            return Err(OnvifError::SenderFault(
                "encoding interval other than 1 not supported (every frame is encoded)".to_string(),
            ));
        }

        let mut config = write_config(&self.config);
        if config.encoder_token == token {
            let cfg = &mut *config;
            cfg.camera_width = parsed.width.unwrap_or(cfg.camera_width);
            cfg.camera_height = parsed.height.unwrap_or(cfg.camera_height);
            cfg.camera_fps = parsed.frame_rate.unwrap_or(cfg.camera_fps);
            cfg.camera_bitrate = parsed.bitrate.unwrap_or(cfg.camera_bitrate);
            if let Some(enc) = encoding {
                cfg.encoding = enc;
            }
        } else {
            let target = config
                .extra_profiles
                .iter_mut()
                .find(|p| p.encoder_token == token)
                .ok_or_else(|| {
                    OnvifError::SenderFault(format!("configuration not found: {token}"))
                })?;
            target.width = parsed.width.unwrap_or(target.width);
            target.height = parsed.height.unwrap_or(target.height);
            target.fps = parsed.frame_rate.unwrap_or(target.fps);
            target.bitrate = parsed.bitrate.unwrap_or(target.bitrate);
            if let Some(enc) = encoding {
                target.encoding = enc;
            }
        }

        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        writer
            .write_event(Event::Empty(BytesStart::new(
                "SetVideoEncoderConfigurationResponse",
            )))
            .unwrap_or_default();
        let body = String::from_utf8(writer.into_inner())
            .map_err(|e| OnvifError::Internal(format!("non-UTF-8 output from XML writer: {e}")))?;
        Ok(serialize_soap_response(&body))
    }
}

/// Handler for GetGuaranteedNumberOfVideoEncoderInstances. The ver10
/// media WSDL names the response element `TotalNumber`; the device
/// dedicates one encoder instance per source configuration regardless
/// of the requested token, so the answer is static (issue #48).
pub struct GetGuaranteedNumberOfVideoEncoderInstancesHandler;

#[async_trait]
impl OnvifActionHandler for GetGuaranteedNumberOfVideoEncoderInstancesHandler {
    async fn handle(&self, _body: &str, _request_info: &RequestInfo) -> Result<String, OnvifError> {
        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        writer
            .write_event(Event::Start(BytesStart::new(
                "GetGuaranteedNumberOfVideoEncoderInstancesResponse",
            )))
            .unwrap_or_default();
        write_text_element(&mut writer, "TotalNumber", "1");
        writer
            .write_event(Event::End(BytesEnd::new(
                "GetGuaranteedNumberOfVideoEncoderInstancesResponse",
            )))
            .unwrap_or_default();
        let body = String::from_utf8(writer.into_inner())
            .map_err(|e| OnvifError::Internal(format!("non-UTF-8 output from XML writer: {e}")))?;
        Ok(serialize_soap_response(&body))
    }
}

/// Handler for SetSynchronizationPoint — the "make the next frame a
/// keyframe" request. The host hooks keyframe forcing through the
/// optional `hook` closure (fired synchronously per request); without
/// one the action is still acknowledged and the stream keeps its normal
/// GOP cadence (issue #48).
pub struct SetSynchronizationPointHandler {
    hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl SetSynchronizationPointHandler {
    #[must_use]
    pub fn new(hook: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
        Self { hook }
    }
}

#[async_trait]
impl OnvifActionHandler for SetSynchronizationPointHandler {
    async fn handle(&self, _body: &str, _request_info: &RequestInfo) -> Result<String, OnvifError> {
        if let Some(hook) = &self.hook {
            hook();
        }
        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        writer
            .write_event(Event::Empty(BytesStart::new(
                "SetSynchronizationPointResponse",
            )))
            .unwrap_or_default();
        let body = String::from_utf8(writer.into_inner())
            .map_err(|e| OnvifError::Internal(format!("non-UTF-8 output from XML writer: {e}")))?;
        Ok(serialize_soap_response(&body))
    }
}

/// Answers a "list the Xs" action with the valid empty set — the audio
/// family and the OSDs, of which this device has none. An empty set is
/// a real answer (not a fault); the capabilities advertise the features
/// off (issue #48).
struct EmptyMediaSetHandler {
    action: &'static str,
}

#[async_trait]
impl OnvifActionHandler for EmptyMediaSetHandler {
    async fn handle(&self, _body: &str, _request_info: &RequestInfo) -> Result<String, OnvifError> {
        let root = format!("{}Response", self.action);
        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        writer
            .write_event(Event::Empty(BytesStart::new(root.as_str())))
            .unwrap_or_default();
        let body = String::from_utf8(writer.into_inner())
            .map_err(|e| OnvifError::Internal(format!("non-UTF-8 output from XML writer: {e}")))?;
        Ok(serialize_soap_response(&body))
    }
}

/// Handler for the media GetServiceCapabilities. Attribute and child
/// names follow the ver10 media WSDL (`Capabilities` with the
/// `SnapshotUri`/`Rotation`/… attributes and the `ProfileCapabilities`
/// / `StreamingCapabilities` children); elements are written
/// unprefixed to match this module's wire style. `SnapshotUri` mirrors
/// [`OnvifMediaConfig::snapshot_port`]; the profile count is what
/// GetProfiles actually advertises (issue #48).
pub struct GetMediaServiceCapabilitiesHandler {
    config: SharedMediaConfig,
}

impl GetMediaServiceCapabilitiesHandler {
    #[must_use]
    pub fn new(config: SharedMediaConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl OnvifActionHandler for GetMediaServiceCapabilitiesHandler {
    async fn handle(&self, _body: &str, _request_info: &RequestInfo) -> Result<String, OnvifError> {
        let config = read_config(&self.config);
        let snapshot_uri = config.snapshot_port != 0;
        let max_profiles = (1 + config.extra_profiles.len()).to_string();

        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        writer
            .write_event(Event::Start(BytesStart::new(
                "GetServiceCapabilitiesResponse",
            )))
            .unwrap_or_default();

        let mut caps = BytesStart::new("Capabilities");
        caps.push_attribute(("SnapshotUri", if snapshot_uri { "true" } else { "false" }));
        caps.push_attribute(("Rotation", "false"));
        caps.push_attribute(("VideoSourceMode", "false"));
        caps.push_attribute(("OSD", "false"));
        caps.push_attribute(("TemporaryOSDText", "false"));
        caps.push_attribute(("EXICompression", "false"));
        writer.write_event(Event::Start(caps)).unwrap_or_default();

        let mut profile_caps = BytesStart::new("ProfileCapabilities");
        profile_caps.push_attribute(("MaximumNumberOfProfiles", max_profiles.as_str()));
        writer
            .write_event(Event::Empty(profile_caps))
            .unwrap_or_default();

        let mut streaming = BytesStart::new("StreamingCapabilities");
        streaming.push_attribute(("RTPMulticast", "false"));
        streaming.push_attribute(("RTP_TCP", "true"));
        streaming.push_attribute(("RTP_RTSP_TCP", "true"));
        streaming.push_attribute(("NonAggregateControl", "false"));
        streaming.push_attribute(("NoRTSPStreaming", "false"));
        writer
            .write_event(Event::Empty(streaming))
            .unwrap_or_default();

        writer
            .write_event(Event::End(BytesEnd::new("Capabilities")))
            .unwrap_or_default();
        writer
            .write_event(Event::End(BytesEnd::new("GetServiceCapabilitiesResponse")))
            .unwrap_or_default();
        let body = String::from_utf8(writer.into_inner())
            .map_err(|e| OnvifError::Internal(format!("non-UTF-8 output from XML writer: {e}")))?;
        Ok(serialize_soap_response(&body))
    }
}

// ---------------------------------------------------------------------------
// Aggregate registration (issue #48)
// ---------------------------------------------------------------------------

/// Store-backed twins of the four historical handlers — same response
/// builders, but reading a snapshot through the shared
/// [`SharedMediaConfig`] lock so SetVideoEncoderConfiguration mutations
/// are visible. Registered by [`register_media_actions`]; hosts
/// preferring the immutable historical API keep registering the
/// standalone structs directly.
struct StoreGetProfilesHandler {
    config: SharedMediaConfig,
}

struct StoreGetStreamUriHandler {
    config: SharedMediaConfig,
}

struct StoreGetSnapshotUriHandler {
    config: SharedMediaConfig,
}

struct StoreGetVideoSourcesHandler {
    config: SharedMediaConfig,
}

#[async_trait]
impl OnvifActionHandler for StoreGetProfilesHandler {
    async fn handle(&self, _body: &str, _request_info: &RequestInfo) -> Result<String, OnvifError> {
        build_get_profiles_response(&read_config(&self.config))
    }
}

#[async_trait]
impl OnvifActionHandler for StoreGetStreamUriHandler {
    async fn handle(&self, body: &str, request_info: &RequestInfo) -> Result<String, OnvifError> {
        build_get_stream_uri_response(&read_config(&self.config), body, request_info)
    }
}

#[async_trait]
impl OnvifActionHandler for StoreGetSnapshotUriHandler {
    async fn handle(&self, _body: &str, request_info: &RequestInfo) -> Result<String, OnvifError> {
        build_get_snapshot_uri_response(&read_config(&self.config), request_info)
    }
}

#[async_trait]
impl OnvifActionHandler for StoreGetVideoSourcesHandler {
    async fn handle(&self, _body: &str, _request_info: &RequestInfo) -> Result<String, OnvifError> {
        build_get_video_sources_response(&read_config(&self.config))
    }
}

/// Register the complete Media service on `server` (issue #48): the four
/// historical actions plus the encoder configuration family
/// (list / get / options / set), the guaranteed-instance count,
/// SetSynchronizationPoint, the empty audio and OSD sets, and the media
/// service capabilities.
///
/// Every handler reads through `config` — a shared
/// [`RwLock<OnvifMediaConfig>`](std::sync::RwLock) store — so
/// [`SetVideoEncoderConfigurationHandler`] mutations are reflected by
/// every reader. Hosts wanting the immutable historical behavior keep
/// registering the standalone handler structs directly.
///
/// `keyframe_hook` fires on every SetSynchronizationPoint request (the
/// host's "force an IDR frame now" seam); `None` acknowledges only.
///
/// Deliberately NOT registered: StartMulticastStreaming /
/// StopMulticastStreaming — the device serves no RTP multicast and the
/// capabilities answer advertises `RTPMulticast="false"`; acknowledging
/// the actions anyway would be dishonest.
///
/// NOTE: `GetServiceCapabilities` is a contested action name (the PTZ
/// and imaging families register their own answers under it; imaging
/// additionally routes by request shape). Registration order decides
/// which family answers plain requests — register media last (or after
/// PTZ) to have the media capabilities answer win.
pub fn register_media_actions(
    server: &mut OnvifServer,
    config: SharedMediaConfig,
    keyframe_hook: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    server.register_handler(
        "GetProfiles",
        Box::new(StoreGetProfilesHandler {
            config: Arc::clone(&config),
        }),
    );
    server.register_handler(
        "GetStreamUri",
        Box::new(StoreGetStreamUriHandler {
            config: Arc::clone(&config),
        }),
    );
    server.register_handler(
        "GetSnapshotUri",
        Box::new(StoreGetSnapshotUriHandler {
            config: Arc::clone(&config),
        }),
    );
    server.register_handler(
        "GetVideoSources",
        Box::new(StoreGetVideoSourcesHandler {
            config: Arc::clone(&config),
        }),
    );
    server.register_handler(
        "GetVideoEncoderConfigurations",
        Box::new(GetVideoEncoderConfigurationsHandler::new(Arc::clone(
            &config,
        ))),
    );
    server.register_handler(
        "GetVideoEncoderConfiguration",
        Box::new(GetVideoEncoderConfigurationHandler::new(Arc::clone(
            &config,
        ))),
    );
    server.register_handler(
        "GetVideoEncoderConfigurationOptions",
        Box::new(GetVideoEncoderConfigurationOptionsHandler::new(Arc::clone(
            &config,
        ))),
    );
    server.register_handler(
        "SetVideoEncoderConfiguration",
        Box::new(SetVideoEncoderConfigurationHandler::new(Arc::clone(
            &config,
        ))),
    );
    server.register_handler(
        "GetGuaranteedNumberOfVideoEncoderInstances",
        Box::new(GetGuaranteedNumberOfVideoEncoderInstancesHandler),
    );
    server.register_handler(
        "SetSynchronizationPoint",
        Box::new(SetSynchronizationPointHandler::new(keyframe_hook)),
    );
    for action in [
        "GetAudioSources",
        "GetAudioSourceConfigurations",
        "GetAudioEncoderConfigurations",
        "GetAudioOutputs",
        "GetAudioDecoderConfigurations",
        "GetOSDs",
    ] {
        server.register_handler(action, Box::new(EmptyMediaSetHandler { action }));
    }
    server.register_handler(
        "GetServiceCapabilities",
        Box::new(GetMediaServiceCapabilitiesHandler::new(config)),
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AuthResult;

    fn test_config() -> Arc<OnvifMediaConfig> {
        Arc::new(OnvifMediaConfig {
            stream_path: "/stream".to_string(),
            snapshot_port: 0,
            snapshot_path: "/snapshot.jpg".to_string(),
            profile_token: "main".to_string(),
            video_source_token: "videoSrc0".to_string(),
            encoder_token: "enc0".to_string(),
            encoding: VideoEncoding::H264,
            video_source_name: "Video Source".to_string(),
            camera_width: 1920,
            camera_height: 1080,
            camera_fps: 30,
            camera_bitrate: 4_000_000,
            rtsp_port: 8554,
            device_ip: "192.168.1.100".to_string(),
            extra_profiles: Vec::new(),
        })
    }

    fn req_info(server_ip: &str) -> RequestInfo {
        RequestInfo {
            client_ip: "10.0.0.1".to_string(),
            server_ip: server_ip.to_string(),
            auth_result: AuthResult {
                username: "admin".to_string(),
                authenticated: true,
            },
        }
    }

    // ------------------------------------------------------------------
    // GetProfiles
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_profiles_response() {
        let handler = GetProfilesHandler::new(test_config());
        let result = handler.handle("", &req_info("10.0.0.1")).await.unwrap();

        // SOAP envelope wrapping
        assert!(
            result.contains("soap:Envelope"),
            "should have SOAP envelope"
        );
        assert!(result.contains("soap:Body"), "should have SOAP body");

        // Response element
        assert!(result.contains("GetProfilesResponse"));

        // Profile attributes
        assert!(result.contains("Profiles"), "should have Profiles element");
        assert!(
            result.contains(r#"token="main""#),
            "should have profile token"
        );

        // Video source configuration
        assert!(
            result.contains("VideoSourceConfiguration"),
            "should have VideoSourceConfiguration"
        );
        assert!(
            result.contains(r#"token="videoSrc0""#),
            "should have source token"
        );

        // Bounds with resolution from config
        assert!(
            result.contains(r#"width="1920""#),
            "should have correct width"
        );
        assert!(
            result.contains(r#"height="1080""#),
            "should have correct height"
        );

        // Video encoder configuration
        assert!(
            result.contains("VideoEncoderConfiguration"),
            "should have VideoEncoderConfiguration"
        );
        assert!(result.contains("H264"), "should be H.264 encoding");

        // Resolution elements
        assert!(
            result.contains("<Width>1920</Width>"),
            "should have Width element"
        );
        assert!(
            result.contains("<Height>1080</Height>"),
            "should have Height element"
        );

        // Rate control
        assert!(result.contains("<FrameRateLimit>30</FrameRateLimit>"));
        assert!(result.contains("<BitrateLimit>4000000</BitrateLimit>"));
        assert!(result.contains("<EncodingInterval>1</EncodingInterval>"));
    }

    // ------------------------------------------------------------------
    // GetStreamUri
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_stream_uri_honors_custom_stream_path() {
        let config = Arc::new(OnvifMediaConfig {
            stream_path: "/live/cam-42".to_string(),
            snapshot_port: 0,
            snapshot_path: "/snapshot.jpg".to_string(),
            profile_token: "main".to_string(),
            video_source_token: "videoSrc0".to_string(),
            encoder_token: "enc0".to_string(),
            encoding: VideoEncoding::H264,
            video_source_name: "Video Source".to_string(),
            camera_width: 1280,
            camera_height: 720,
            camera_fps: 25,
            camera_bitrate: 2_500_000,
            rtsp_port: 8554,
            device_ip: "192.168.1.10".to_string(),
            extra_profiles: Vec::new(),
        });
        let handler = GetStreamUriHandler::new(config);
        let result = handler.handle("", &req_info("192.168.1.10")).await.unwrap();
        assert!(
            result.contains("rtsp://192.168.1.10:8554/live/cam-42"),
            "custom stream path must be advertised, got: {result}"
        );
    }

    #[tokio::test]
    async fn test_get_stream_uri_dynamic_ip() {
        let handler = GetStreamUriHandler::new(test_config());
        let result = handler.handle("", &req_info("10.0.0.5")).await.unwrap();

        // Must use the per-request client IP
        assert!(
            result.contains("rtsp://10.0.0.5:8554/stream"),
            "should use dynamic client IP"
        );
        assert!(result.contains("GetStreamUriResponse"));
        assert!(result.contains("MediaUri"));
        assert!(result.contains("InvalidAfterConnect"));
        assert!(result.contains("InvalidAfterReboot"));
        assert!(result.contains("Timeout"));
    }

    #[tokio::test]
    async fn test_get_stream_uri_fallback_ip() {
        let handler = GetStreamUriHandler::new(test_config());

        // Empty client_ip -> fallback to device_ip from config
        let result = handler.handle("", &req_info("")).await.unwrap();

        assert!(
            result.contains("rtsp://192.168.1.100:8554/stream"),
            "should fall back to device IP when client IP is empty"
        );
    }

    #[tokio::test]
    async fn test_get_stream_uri_different_port() {
        let mut cfg = (*test_config()).clone();
        cfg.rtsp_port = 554;
        let config = Arc::new(cfg);
        let handler = GetStreamUriHandler::new(config);

        let result = handler.handle("", &req_info("10.0.0.1")).await.unwrap();
        assert!(
            result.contains("rtsp://10.0.0.1:554/stream"),
            "should use configured RTSP port"
        );
    }

    // ------------------------------------------------------------------
    // GetSnapshotUri
    // ------------------------------------------------------------------

    fn snapshot_config() -> OnvifMediaConfig {
        let mut cfg = (*test_config()).clone();
        cfg.snapshot_port = 8088;
        cfg
    }

    #[tokio::test]
    async fn test_get_snapshot_uri_advertises_http_snapshot() {
        let handler = GetSnapshotUriHandler::new(Arc::new(snapshot_config()));
        let result = handler.handle("", &req_info("10.0.0.5")).await.unwrap();

        assert!(result.contains("GetSnapshotUriResponse"));
        assert!(result.contains("MediaUri"));
        assert!(
            result.contains("http://10.0.0.5:8088/snapshot.jpg"),
            "should advertise the host snapshot endpoint, got: {result}"
        );
        // MediaUri semantics mirror the onvif-go twin (snapshot: fresh fetch,
        // invalid after reboot, 5 s validity).
        assert!(result.contains("<InvalidAfterConnect>false</InvalidAfterConnect>"));
        assert!(result.contains("<InvalidAfterReboot>true</InvalidAfterReboot>"));
        assert!(result.contains("<Timeout>PT5S</Timeout>"));
    }

    #[tokio::test]
    async fn test_get_snapshot_uri_honors_custom_path_and_fallback_ip() {
        let mut cfg = snapshot_config();
        cfg.snapshot_path = "/cgi-bin/snap.jpeg".to_string();
        let handler = GetSnapshotUriHandler::new(Arc::new(cfg));

        // Empty per-request IP → fall back to the configured device IP.
        let result = handler.handle("", &req_info("")).await.unwrap();
        assert!(
            result.contains("http://192.168.1.100:8088/cgi-bin/snap.jpeg"),
            "custom path + fallback IP, got: {result}"
        );
    }

    #[tokio::test]
    async fn test_get_snapshot_uri_disabled_faults() {
        // snapshot_port = 0 (the default) → the handler must fault instead of
        // advertising a URI nothing serves (onvif-go's ErrSnapshotNotSupported).
        let handler = GetSnapshotUriHandler::new(test_config());
        let err = handler.handle("", &req_info("10.0.0.1")).await.unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("not supported"),
            "expected a not-supported fault, got: {err}"
        );
    }

    // ------------------------------------------------------------------
    // GetVideoSources
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_video_sources_response() {
        let handler = GetVideoSourcesHandler::new(test_config());
        let result = handler.handle("", &req_info("")).await.unwrap();

        assert!(result.contains("GetVideoSourcesResponse"));
        assert!(result.contains(r#"token="videoSrc0""#));
        assert!(result.contains("Video Source"), "neutral default name");
        assert!(!result.contains("Pi Camera"), "no origin-hardware branding");
        assert!(result.contains("soap:Envelope"));
    }

    // ------------------------------------------------------------------
    // Multiple profiles (main + substreams) — GetProfiles advertises the
    // primary profile first, extras after; GetStreamUri routes by the
    // request's ProfileToken.
    // ------------------------------------------------------------------

    fn multi_profile_config() -> Arc<OnvifMediaConfig> {
        let mut cfg = (*test_config()).clone();
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
        Arc::new(cfg)
    }

    #[tokio::test]
    async fn test_get_profiles_extra_profiles_advertised_after_primary() {
        let handler = GetProfilesHandler::new(multi_profile_config());
        let result = handler.handle("", &req_info("10.0.0.1")).await.unwrap();

        assert!(
            result.contains(r#"token="main""#) && result.contains(r#"token="sub""#),
            "both profiles advertised, got: {result}"
        );
        let main_at = result.find(r#"token="main""#).unwrap_or_default();
        let sub_at = result.find(r#"token="sub""#).unwrap_or_default();
        assert!(
            main_at < sub_at,
            "primary profile must come first (Profile S clients pick the first), got: {result}"
        );

        // The sub profile block carries its own encoder geometry.
        assert!(
            result.contains("<Width>640</Width>"),
            "sub width, got: {result}"
        );
        assert!(result.contains("<Height>360</Height>"));
        assert!(result.contains("<FrameRateLimit>15</FrameRateLimit>"));
        assert!(result.contains("<BitrateLimit>400000</BitrateLimit>"));
        assert!(result.contains(r#"token="sub_encoder""#));

        // Exactly two Profiles elements — no duplication of the primary.
        assert_eq!(
            result.matches("<Profiles ").count(),
            2,
            "expected exactly two Profiles elements, got: {result}"
        );
    }

    #[tokio::test]
    async fn test_get_profiles_single_profile_shape_unchanged() {
        // No extra_profiles → exactly one Profiles element, primary geometry.
        let handler = GetProfilesHandler::new(test_config());
        let result = handler.handle("", &req_info("10.0.0.1")).await.unwrap();
        assert_eq!(result.matches("<Profiles ").count(), 1);
        assert!(!result.contains(r#"token="sub""#));
    }

    fn stream_uri_body(token: &str) -> String {
        format!(
            "<GetStreamUri xmlns=\"http://www.onvif.org/ver10/media/wsdl\">\
             <ProfileToken>{token}</ProfileToken></GetStreamUri>"
        )
    }

    #[tokio::test]
    async fn test_get_stream_uri_routes_extra_profile_token() {
        let handler = GetStreamUriHandler::new(multi_profile_config());

        let sub = handler
            .handle(&stream_uri_body("sub"), &req_info("10.0.0.1"))
            .await
            .unwrap();
        assert!(
            sub.contains("rtsp://10.0.0.1:8554/sub"),
            "sub token must map to the sub stream path, got: {sub}"
        );

        let main = handler
            .handle(&stream_uri_body("main"), &req_info("10.0.0.1"))
            .await
            .unwrap();
        assert!(main.contains("rtsp://10.0.0.1:8554/stream"));

        // Unknown and missing tokens fail open to the primary stream —
        // legacy clients that never send a token keep the historical URI.
        let unknown = handler
            .handle(&stream_uri_body("profile_7"), &req_info("10.0.0.1"))
            .await
            .unwrap();
        assert!(unknown.contains("rtsp://10.0.0.1:8554/stream"));

        let missing = handler.handle("", &req_info("10.0.0.1")).await.unwrap();
        assert!(missing.contains("rtsp://10.0.0.1:8554/stream"));
    }

    #[tokio::test]
    async fn test_get_stream_uri_token_namespaced_and_attributed() {
        let handler = GetStreamUriHandler::new(multi_profile_config());

        let namespaced = handler
            .handle(
                "<GetStreamUri><tt:ProfileToken>sub</tt:ProfileToken></GetStreamUri>",
                &req_info("10.0.0.1"),
            )
            .await
            .unwrap();
        assert!(
            namespaced.contains("rtsp://10.0.0.1:8554/sub"),
            "namespace prefixes must not defeat token routing, got: {namespaced}"
        );

        let attributed = handler
            .handle(
                "<GetStreamUri><ProfileToken xmlns=\"http://www.onvif.org/ver10/media/wsdl\">sub</ProfileToken></GetStreamUri>",
                &req_info("10.0.0.1"),
            )
            .await
            .unwrap();
        assert!(attributed.contains("rtsp://10.0.0.1:8554/sub"));
    }

    #[test]
    fn test_parse_profile_token_forms() {
        // Plain element.
        assert_eq!(
            parse_profile_token("<GetStreamUri><ProfileToken>sub</ProfileToken></GetStreamUri>"),
            Some("sub")
        );
        // Namespaced element.
        assert_eq!(
            parse_profile_token("<tt:ProfileToken>sub</tt:ProfileToken>"),
            Some("sub")
        );
        // Attributes on the open tag.
        assert_eq!(
            parse_profile_token(r#"<ProfileToken xmlns="x">sub</ProfileToken>"#),
            Some("sub")
        );
        // Whitespace around the token text is trimmed.
        assert_eq!(
            parse_profile_token("<ProfileToken>\n  sub  </ProfileToken>"),
            Some("sub")
        );
        // Empty element / empty content / absent / garbage → no token.
        assert_eq!(parse_profile_token("<ProfileToken/>"), None);
        assert_eq!(parse_profile_token("<ProfileToken></ProfileToken>"), None);
        assert_eq!(parse_profile_token("<GetStreamUri/>"), None);
        assert_eq!(parse_profile_token("not xml at all"), None);
        assert_eq!(parse_profile_token("<<<<"), None);
        assert_eq!(parse_profile_token(""), None);
    }

    // ------------------------------------------------------------------
    // Malformed request handling — handlers must not panic on arbitrary
    // body input (they ignore the body, so any content is acceptable).
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_handler_ignores_malformed_body() {
        let cfg = test_config();
        let handlers: Vec<Box<dyn OnvifActionHandler>> = vec![
            Box::new(GetProfilesHandler::new(Arc::clone(&cfg))),
            Box::new(GetStreamUriHandler::new(Arc::clone(&cfg))),
            Box::new(GetVideoSourcesHandler::new(Arc::clone(&cfg))),
        ];

        let garbage_bodies = vec!["", "not xml", "<broken>", "{{{{{"];

        for handler in &handlers {
            for garbage in &garbage_bodies {
                let result = handler.handle(garbage, &req_info("10.0.0.1")).await;
                assert!(result.is_ok(), "handler should not fail on garbage input");
                let xml = result.unwrap();
                assert!(
                    xml.contains("soap:Envelope"),
                    "should still produce valid SOAP envelope"
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // Media service completion (issue #48): encoder configuration
    // family, synchronization point, guaranteed instances, service
    // capabilities, and the empty audio/OSD sets — all reading through
    // the shared RwLock store so SetVideoEncoderConfiguration mutations
    // are visible to every reader.
    // ------------------------------------------------------------------

    use std::sync::atomic::{AtomicU32, Ordering};

    fn store_config() -> Arc<RwLock<OnvifMediaConfig>> {
        Arc::new(RwLock::new((*test_config()).clone()))
    }

    fn multi_store() -> Arc<RwLock<OnvifMediaConfig>> {
        Arc::new(RwLock::new((*multi_profile_config()).clone()))
    }

    fn assert_well_formed(xml: &str, ctx: &str) {
        let mut reader = quick_xml::Reader::from_str(xml);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(quick_xml::events::Event::Eof) => break,
                Err(e) => panic!("{ctx}: XML parse error: {e}"),
                Ok(_) => {}
            }
            buf.clear();
        }
    }

    fn get_encoder_body(token: &str) -> String {
        format!(
            "<GetVideoEncoderConfiguration><ConfigurationToken>{token}\
             </ConfigurationToken></GetVideoEncoderConfiguration>"
        )
    }

    // -- GetVideoEncoderConfigurations (list) ---------------------------

    #[tokio::test]
    async fn test_get_video_encoder_configurations_lists_primary_and_extras() {
        let handler = GetVideoEncoderConfigurationsHandler::new(multi_store());
        let result = handler.handle("", &req_info("")).await.unwrap();
        assert_well_formed(&result, "GetVideoEncoderConfigurations");

        assert!(result.contains("GetVideoEncoderConfigurationsResponse"));
        assert_eq!(
            result.matches("<Configurations ").count(),
            2,
            "primary + extra encoder configs, got: {result}"
        );
        // Primary first, extras after — same order GetProfiles advertises.
        let enc0 = result.find(r#"token="enc0""#).unwrap_or_default();
        let sub = result.find(r#"token="sub_encoder""#).unwrap_or_default();
        assert!(enc0 < sub, "primary encoder config must come first");

        // Field mirror of GetProfiles' VideoEncoderConfiguration block.
        assert!(result.contains("<Name>VideoEncoderConfig</Name>"));
        assert!(result.contains("<UseCount>1</UseCount>"));
        assert!(result.contains("<Encoding>H264</Encoding>"));
        assert!(result.contains("<Width>1920</Width>"));
        assert!(result.contains("<Height>1080</Height>"));
        assert!(result.contains("<FrameRateLimit>30</FrameRateLimit>"));
        assert!(result.contains("<BitrateLimit>4000000</BitrateLimit>"));
        assert!(result.contains("<EncodingInterval>1</EncodingInterval>"));
        assert!(result.contains("<Width>640</Width>"));
        // No profile wrapper leaks into the standalone listing.
        assert!(!result.contains("<Profiles "));
    }

    #[tokio::test]
    async fn test_get_video_encoder_configurations_single_profile() {
        let handler = GetVideoEncoderConfigurationsHandler::new(store_config());
        let result = handler.handle("", &req_info("")).await.unwrap();
        assert_eq!(result.matches("<Configurations ").count(), 1);
        assert!(result.contains(r#"token="enc0""#));
    }

    // -- GetVideoEncoderConfiguration (single) --------------------------

    #[tokio::test]
    async fn test_get_video_encoder_configuration_known_token() {
        let handler = GetVideoEncoderConfigurationHandler::new(multi_store());
        let result = handler
            .handle(&get_encoder_body("enc0"), &req_info(""))
            .await
            .unwrap();
        assert_well_formed(&result, "GetVideoEncoderConfiguration");
        assert!(result.contains("GetVideoEncoderConfigurationResponse"));
        assert!(result.contains(r#"<Configuration token="enc0">"#));
        assert_eq!(result.matches("<Configuration ").count(), 1);
        assert!(result.contains("<Width>1920</Width>"));
        assert!(result.contains("<FrameRateLimit>30</FrameRateLimit>"));
    }

    #[tokio::test]
    async fn test_get_video_encoder_configuration_namespaced_token() {
        let handler = GetVideoEncoderConfigurationHandler::new(multi_store());
        let body = "<GetVideoEncoderConfiguration><tt:ConfigurationToken>sub_encoder\
                    </tt:ConfigurationToken></GetVideoEncoderConfiguration>";
        let result = handler.handle(body, &req_info("")).await.unwrap();
        assert!(
            result.contains(r#"<Configuration token="sub_encoder">"#),
            "namespace prefix must not defeat token lookup, got: {result}"
        );
        assert!(result.contains("<Width>640</Width>"));
    }

    #[tokio::test]
    async fn test_get_video_encoder_configuration_unknown_token_faults() {
        let handler = GetVideoEncoderConfigurationHandler::new(multi_store());
        let err = handler
            .handle(&get_encoder_body("nope"), &req_info(""))
            .await
            .unwrap_err();
        assert!(
            matches!(err, OnvifError::SenderFault(ref m) if m.contains("configuration not found")),
            "unknown token must be a Sender fault, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_get_video_encoder_configuration_missing_token_faults() {
        let handler = GetVideoEncoderConfigurationHandler::new(multi_store());
        let err = handler
            .handle("<GetVideoEncoderConfiguration/>", &req_info(""))
            .await
            .unwrap_err();
        assert!(
            matches!(err, OnvifError::SenderFault(ref m) if m.contains("configuration not found")),
            "missing token must be a Sender fault, got: {err}"
        );
    }

    // -- GetVideoEncoderConfigurationOptions -----------------------------

    #[tokio::test]
    async fn test_get_video_encoder_configuration_options_h264() {
        let handler = GetVideoEncoderConfigurationOptionsHandler::new(multi_store());
        let result = handler.handle("", &req_info("")).await.unwrap();
        assert_well_formed(&result, "GetVideoEncoderConfigurationOptions");

        assert!(result.contains("GetVideoEncoderConfigurationOptionsResponse"));
        assert!(result.contains("<Options>"));
        assert!(result.contains("<QualityRange>"));
        assert!(result.contains("<Min>1</Min>"));
        assert!(result.contains("<Max>10</Max>"));
        // H264 options: every advertised resolution (primary + extras),
        // ranges derived from the primary config's fps.
        assert!(result.contains("<H264>"));
        assert!(result.contains(r#"<ResolutionsAvailable Width="1920" Height="1080"/>"#));
        assert!(result.contains(r#"<ResolutionsAvailable Width="640" Height="360"/>"#));
        assert!(result.contains("<GovLengthRange>"));
        assert!(result.contains("<FrameRateRange>"));
        assert!(result.contains("<EncodingIntervalRange>"));
        assert!(result.contains("<H264ProfilesSupported>Baseline</H264ProfilesSupported>"));
        assert!(result.contains("<H264ProfilesSupported>Main</H264ProfilesSupported>"));
        assert!(result.contains("<H264ProfilesSupported>High</H264ProfilesSupported>"));
        // No JPEG block: this device does not encode JPEG video.
        assert!(!result.contains("<JPEG>"));
    }

    #[tokio::test]
    async fn test_get_video_encoder_configuration_options_h265_has_no_codec_block() {
        // ver10 has no options element for H.265 — the honest answer is
        // the quality range only, no codec block at all.
        let mut cfg = (*test_config()).clone();
        cfg.encoding = VideoEncoding::H265;
        let handler = GetVideoEncoderConfigurationOptionsHandler::new(Arc::new(RwLock::new(cfg)));
        let result = handler.handle("", &req_info("")).await.unwrap();
        assert_well_formed(&result, "GetVideoEncoderConfigurationOptions/H265");
        assert!(result.contains("<QualityRange>"));
        assert!(!result.contains("<H264>"));
        assert!(!result.contains("<JPEG>"));
    }

    // -- SetVideoEncoderConfiguration ------------------------------------

    fn set_encoder_body(token: &str, fields: &str) -> String {
        format!(
            "<SetVideoEncoderConfiguration>\
             <Configuration token=\"{token}\">{fields}</Configuration>\
             <ForcePersistence>true</ForcePersistence>\
             </SetVideoEncoderConfiguration>"
        )
    }

    #[tokio::test]
    async fn test_set_video_encoder_configuration_mutates_store() {
        let store = multi_store();
        let set = SetVideoEncoderConfigurationHandler::new(Arc::clone(&store));
        let get = GetVideoEncoderConfigurationsHandler::new(Arc::clone(&store));

        let body = set_encoder_body(
            "enc0",
            "<Name>renamed</Name><UseCount>3</UseCount><Encoding>H264</Encoding>\
             <Resolution><Width>1280</Width><Height>720</Height></Resolution>\
             <RateControl><FrameRateLimit>15</FrameRateLimit>\
             <BitrateLimit>1000000</BitrateLimit>\
             <EncodingInterval>1</EncodingInterval></RateControl>",
        );
        let ack = set.handle(&body, &req_info("")).await.unwrap();
        assert!(ack.contains("<SetVideoEncoderConfigurationResponse/>"));
        assert_well_formed(&ack, "SetVideoEncoderConfiguration");

        let after = get.handle("", &req_info("")).await.unwrap();
        assert!(after.contains("<Width>1280</Width>"));
        assert!(after.contains("<Height>720</Height>"));
        assert!(after.contains("<FrameRateLimit>15</FrameRateLimit>"));
        assert!(after.contains("<BitrateLimit>1000000</BitrateLimit>"));
        assert!(!after.contains("<Width>1920</Width>"), "old width gone");
    }

    #[tokio::test]
    async fn test_set_video_encoder_configuration_absent_fields_unchanged() {
        let store = multi_store();
        let set = SetVideoEncoderConfigurationHandler::new(Arc::clone(&store));
        let get = GetVideoEncoderConfigurationsHandler::new(Arc::clone(&store));

        // Only the resolution is sent — fps/bitrate/encoding stay put.
        let body = set_encoder_body(
            "enc0",
            "<Resolution><Width>640</Width><Height>360</Height></Resolution>",
        );
        set.handle(&body, &req_info("")).await.unwrap();
        let after = get.handle("", &req_info("")).await.unwrap();
        assert!(after.contains("<Width>640</Width>"));
        assert!(after.contains("<Height>360</Height>"));
        assert!(after.contains("<FrameRateLimit>30</FrameRateLimit>"));
        assert!(after.contains("<BitrateLimit>4000000</BitrateLimit>"));
        assert!(after.contains("<Encoding>H264</Encoding>"));
    }

    #[tokio::test]
    async fn test_set_video_encoder_configuration_namespaced_fields_parse() {
        let store = multi_store();
        let set = SetVideoEncoderConfigurationHandler::new(Arc::clone(&store));
        let get = GetVideoEncoderConfigurationHandler::new(Arc::clone(&store));

        let body = "<SetVideoEncoderConfiguration>\
                    <tt:Configuration token=\"enc0\" xmlns:tt=\"http://www.onvif.org/ver10/schema\">\
                    <tt:Resolution><tt:Width>320</tt:Width><tt:Height>240</tt:Height></tt:Resolution>\
                    <tt:RateControl><tt:FrameRateLimit>10</tt:FrameRateLimit>\
                    <tt:EncodingInterval>1</tt:EncodingInterval></tt:RateControl>\
                    </tt:Configuration></SetVideoEncoderConfiguration>";
        set.handle(body, &req_info("")).await.unwrap();
        let after = get
            .handle(&get_encoder_body("enc0"), &req_info(""))
            .await
            .unwrap();
        assert!(after.contains("<Width>320</Width>"));
        assert!(after.contains("<Height>240</Height>"));
        assert!(after.contains("<FrameRateLimit>10</FrameRateLimit>"));
    }

    #[tokio::test]
    async fn test_set_video_encoder_configuration_extra_profile_token() {
        let store = multi_store();
        let set = SetVideoEncoderConfigurationHandler::new(Arc::clone(&store));
        let get = GetVideoEncoderConfigurationHandler::new(Arc::clone(&store));

        let body = set_encoder_body(
            "sub_encoder",
            "<Resolution><Width>320</Width><Height>180</Height></Resolution>",
        );
        set.handle(&body, &req_info("")).await.unwrap();

        let sub = get
            .handle(&get_encoder_body("sub_encoder"), &req_info(""))
            .await
            .unwrap();
        assert!(sub.contains("<Width>320</Width>"));
        // Primary untouched.
        let main = get
            .handle(&get_encoder_body("enc0"), &req_info(""))
            .await
            .unwrap();
        assert!(main.contains("<Width>1920</Width>"));
    }

    #[tokio::test]
    async fn test_set_video_encoder_configuration_unknown_token_faults() {
        let store = multi_store();
        let set = SetVideoEncoderConfigurationHandler::new(store);
        let err = set
            .handle(&set_encoder_body("nope", ""), &req_info(""))
            .await
            .unwrap_err();
        assert!(
            matches!(err, OnvifError::SenderFault(ref m) if m.contains("configuration not found")),
            "unknown token must be a Sender fault, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_set_video_encoder_configuration_missing_configuration_faults() {
        let set = SetVideoEncoderConfigurationHandler::new(store_config());
        let err = set
            .handle("<SetVideoEncoderConfiguration/>", &req_info(""))
            .await
            .unwrap_err();
        assert!(
            matches!(err, OnvifError::SenderFault(ref m) if m.contains("configuration not found")),
            "missing Configuration/token must be a Sender fault, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_set_video_encoder_configuration_invalid_encoding_faults() {
        let set = SetVideoEncoderConfigurationHandler::new(store_config());
        let err = set
            .handle(
                &set_encoder_body("enc0", "<Encoding>MPG4</Encoding>"),
                &req_info(""),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, OnvifError::SenderFault(ref m) if m.contains("encoding")),
            "unknown encoding token must be a Sender fault, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_set_video_encoder_configuration_jpeg_faults_as_unsupported() {
        // JPEG is a valid ONVIF encoding token, but this device encodes
        // H.264/H.265 only — acknowledged support would be a lie.
        let set = SetVideoEncoderConfigurationHandler::new(store_config());
        let err = set
            .handle(
                &set_encoder_body("enc0", "<Encoding>JPEG</Encoding>"),
                &req_info(""),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, OnvifError::SenderFault(ref m) if m.contains("JPEG")),
            "JPEG must fault as unsupported, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_set_video_encoder_configuration_interval_other_than_one_faults() {
        // The encoder writes every frame (interval fixed at 1); promising
        // a different interval without applying it would be dishonest.
        let set = SetVideoEncoderConfigurationHandler::new(store_config());
        let err = set
            .handle(
                &set_encoder_body(
                    "enc0",
                    "<RateControl><EncodingInterval>2</EncodingInterval></RateControl>",
                ),
                &req_info(""),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, OnvifError::SenderFault(ref m) if m.contains("interval")),
            "interval != 1 must fault, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_set_video_encoder_configuration_invalid_number_faults() {
        let set = SetVideoEncoderConfigurationHandler::new(store_config());
        let err = set
            .handle(
                &set_encoder_body(
                    "enc0",
                    "<Resolution><Width>abc</Width><Height>720</Height></Resolution>",
                ),
                &req_info(""),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, OnvifError::SenderFault(ref m) if m.contains("Width")),
            "non-numeric Width must fault, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_set_encoder_reflected_in_profiles_via_shared_store() {
        // The register_media_actions story: Set mutates the store, every
        // reader (including GetProfiles) sees it through the same lock.
        let store = multi_store();
        let set = SetVideoEncoderConfigurationHandler::new(Arc::clone(&store));
        let profiles = StoreGetProfilesHandler {
            config: Arc::clone(&store),
        };
        let body = set_encoder_body(
            "enc0",
            "<Resolution><Width>800</Width><Height>600</Height></Resolution>",
        );
        set.handle(&body, &req_info("")).await.unwrap();
        let p = profiles.handle("", &req_info("")).await.unwrap();
        assert!(p.contains("<Width>800</Width>"), "GetProfiles must see it");
    }

    #[tokio::test]
    async fn test_store_backed_old_handlers_match_standalone_bytes() {
        // Byte stability through the aggregate registration path: the
        // store-backed wrappers must produce bytes identical to the
        // standalone handler structs for the same config.
        let store = multi_store();
        let info = req_info("10.0.0.9");

        let standalone = GetProfilesHandler::new(multi_profile_config())
            .handle("", &info)
            .await
            .unwrap();
        let via_store = StoreGetProfilesHandler {
            config: Arc::clone(&store),
        }
        .handle("", &info)
        .await
        .unwrap();
        assert_eq!(standalone, via_store);

        let body = stream_uri_body("sub");
        let standalone = GetStreamUriHandler::new(multi_profile_config())
            .handle(&body, &info)
            .await
            .unwrap();
        let via_store = StoreGetStreamUriHandler {
            config: Arc::clone(&store),
        }
        .handle(&body, &info)
        .await
        .unwrap();
        assert_eq!(standalone, via_store);
    }

    // -- GetGuaranteedNumberOfVideoEncoderInstances ----------------------

    #[tokio::test]
    async fn test_get_guaranteed_number_of_video_encoder_instances() {
        // WSDL name check: the child element is TotalNumber.
        let handler = GetGuaranteedNumberOfVideoEncoderInstancesHandler;
        for body in [
            "",
            "<GetGuaranteedNumberOfVideoEncoderInstances>\
             <ConfigurationToken>videoSrc0</ConfigurationToken>\
             </GetGuaranteedNumberOfVideoEncoderInstances>",
            "garbage",
        ] {
            let result = handler.handle(body, &req_info("")).await.unwrap();
            assert_well_formed(&result, "GetGuaranteedNumberOfVideoEncoderInstances");
            assert!(
                result.contains("GetGuaranteedNumberOfVideoEncoderInstancesResponse"),
                "got: {result}"
            );
            assert!(
                result.contains("<TotalNumber>1</TotalNumber>"),
                "got: {result}"
            );
        }
    }

    // -- SetSynchronizationPoint ------------------------------------------

    #[tokio::test]
    async fn test_set_synchronization_point_fires_hook_and_acks() {
        let counter = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&counter);
        let handler = SetSynchronizationPointHandler::new(Some(Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        })));
        let result = handler
            .handle(
                "<SetSynchronizationPoint><ProfileToken>main</ProfileToken></SetSynchronizationPoint>",
                &req_info(""),
            )
            .await
            .unwrap();
        assert_well_formed(&result, "SetSynchronizationPoint");
        assert!(result.contains("<SetSynchronizationPointResponse/>"));
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Without a hook the action still acks — keyframe signaling is
        // host-optional.
        let bare = SetSynchronizationPointHandler::new(None);
        let r2 = bare.handle("", &req_info("")).await.unwrap();
        assert!(r2.contains("<SetSynchronizationPointResponse/>"));
        assert_eq!(counter.load(Ordering::SeqCst), 1, "no hook, no fire");
    }

    // -- Empty-set families (audio + OSD) ----------------------------------

    #[tokio::test]
    async fn test_audio_and_osd_families_answer_empty_sets() {
        for action in [
            "GetAudioSources",
            "GetAudioSourceConfigurations",
            "GetAudioEncoderConfigurations",
            "GetAudioOutputs",
            "GetAudioDecoderConfigurations",
            "GetOSDs",
        ] {
            let handler = EmptyMediaSetHandler { action };
            let result = handler.handle("", &req_info("")).await.unwrap();
            assert_well_formed(&result, action);
            assert!(
                result.contains("soap:Envelope"),
                "{action}: SOAP envelope, got: {result}"
            );
            let root = format!("<{action}Response/>");
            assert!(
                result.contains(&root),
                "{action}: expected the bare empty root {root}, got: {result}"
            );
        }
    }

    // -- GetServiceCapabilities (media) ------------------------------------

    #[tokio::test]
    async fn test_media_service_capabilities_snapshot_on() {
        let mut cfg = (*test_config()).clone();
        cfg.snapshot_port = 8088;
        let handler = GetMediaServiceCapabilitiesHandler::new(Arc::new(RwLock::new(cfg)));
        let result = handler.handle("", &req_info("")).await.unwrap();
        assert_well_formed(&result, "GetServiceCapabilities");

        assert!(result.contains("GetServiceCapabilitiesResponse"));
        assert!(result.contains("<Capabilities"));
        assert!(result.contains(r#"SnapshotUri="true""#));
        // Honest capability flags: no rotation/OSD/EXI, no multicast;
        // RTP over TCP (RTSP interleaved) is served.
        assert!(result.contains(r#"Rotation="false""#));
        assert!(result.contains(r#"OSD="false""#));
        assert!(result.contains("<ProfileCapabilities"));
        assert!(result.contains(r#"MaximumNumberOfProfiles="1""#));
        assert!(result.contains("<StreamingCapabilities"));
        assert!(result.contains(r#"RTPMulticast="false""#));
        assert!(result.contains(r#"RTP_RTSP_TCP="true""#));
        assert!(result.contains(r#"NoRTSPStreaming="false""#));
        // media.rs wire style: unprefixed children.
        assert!(!result.contains("trt:"));
    }

    #[tokio::test]
    async fn test_media_service_capabilities_snapshot_off_and_profile_count() {
        let handler = GetMediaServiceCapabilitiesHandler::new(multi_store());
        let result = handler.handle("", &req_info("")).await.unwrap();
        // snapshot_port = 0 (the test_config default) → advertised off.
        assert!(result.contains(r#"SnapshotUri="false""#), "got: {result}");
        // 1 primary + 1 extra profile.
        assert!(
            result.contains(r#"MaximumNumberOfProfiles="2""#),
            "got: {result}"
        );
    }

    // -- Tolerant element-text parsing (generalized token scan) ------------

    #[test]
    fn test_parse_element_text_forms() {
        assert_eq!(
            parse_element_text(
                "<GetVideoEncoderConfiguration><ConfigurationToken>enc0</ConfigurationToken></GetVideoEncoderConfiguration>",
                "ConfigurationToken"
            ),
            Some("enc0")
        );
        assert_eq!(
            parse_element_text(
                "<tt:ConfigurationToken>sub</tt:ConfigurationToken>",
                "ConfigurationToken"
            ),
            Some("sub")
        );
        assert_eq!(
            parse_element_text(
                r#"<ConfigurationToken xmlns="x">t</ConfigurationToken>"#,
                "ConfigurationToken"
            ),
            Some("t")
        );
        assert_eq!(
            parse_element_text("<Width> 42 </Width>", "Width"),
            Some("42")
        );
        assert_eq!(
            parse_element_text("<ConfigurationToken/>", "ConfigurationToken"),
            None
        );
        assert_eq!(
            parse_element_text(
                "<ConfigurationToken></ConfigurationToken>",
                "ConfigurationToken"
            ),
            None
        );
        assert_eq!(parse_element_text("", "ConfigurationToken"), None);
        assert_eq!(
            parse_element_text("not xml at all", "ConfigurationToken"),
            None
        );
        assert_eq!(parse_element_text("<<<<", "ConfigurationToken"), None);
        // The existing ProfileToken scan delegates unchanged.
        assert_eq!(
            parse_profile_token("<ProfileToken>sub</ProfileToken>"),
            Some("sub")
        );
    }

    // -- Garbage bodies: no panics on the new handlers ----------------------

    #[tokio::test]
    async fn test_new_handlers_do_not_panic_on_garbage() {
        let store = multi_store();
        let read_handlers: Vec<Box<dyn OnvifActionHandler>> = vec![
            Box::new(GetVideoEncoderConfigurationsHandler::new(Arc::clone(
                &store,
            ))),
            Box::new(GetVideoEncoderConfigurationOptionsHandler::new(Arc::clone(
                &store,
            ))),
            Box::new(GetGuaranteedNumberOfVideoEncoderInstancesHandler),
            Box::new(SetSynchronizationPointHandler::new(None)),
            Box::new(GetMediaServiceCapabilitiesHandler::new(Arc::clone(&store))),
            Box::new(EmptyMediaSetHandler {
                action: "GetAudioSources",
            }),
        ];
        let faulting_handlers: Vec<Box<dyn OnvifActionHandler>> = vec![
            Box::new(GetVideoEncoderConfigurationHandler::new(Arc::clone(&store))),
            Box::new(SetVideoEncoderConfigurationHandler::new(Arc::clone(&store))),
        ];

        for garbage in ["", "not xml", "<broken>", "{{{{{"] {
            for h in &read_handlers {
                let xml = h.handle(garbage, &req_info("10.0.0.1")).await.unwrap();
                assert!(xml.contains("soap:Envelope"));
            }
            for h in &faulting_handlers {
                // A fault is fine; a panic is not (it would fail the test).
                let _ = h.handle(garbage, &req_info("10.0.0.1")).await;
            }
        }
    }

    // -- register_media_actions wiring over the wire -------------------------

    async fn post_soap_media(port: u16, soap_body: &str) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let envelope = format!(
            "<?xml version=\"1.0\"?>\
             <soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\">\
             <soap:Body>{soap_body}</soap:Body></soap:Envelope>"
        );
        let req = format!(
            "POST /onvif/media_service HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             Content-Type: application/soap+xml; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{envelope}",
            envelope.len()
        );
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).to_string();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or_default();
        (status, text)
    }

    #[tokio::test]
    async fn test_register_media_actions_dispatches_over_the_wire() {
        use crate::server::OnvifConfig;

        // Empty password + the explicit opt-in flag → open test server.
        let cfg = OnvifConfig {
            allow_no_auth: true,
            ..Default::default()
        };
        let mut server = OnvifServer::new(&cfg);

        let counter = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&counter);
        register_media_actions(
            &mut server,
            store_config(),
            Some(Arc::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            })),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut handle = server.start_on(listener).await.unwrap();

        // Old four, registered by the aggregate: GetProfiles answers.
        let (status, body) = post_soap_media(
            port,
            "<GetProfiles xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("GetProfilesResponse"));

        // New encoder family dispatches.
        let (status, body) = post_soap_media(
            port,
            "<GetVideoEncoderConfigurations xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(r#"token="enc0""#));

        // Sync point acks and fires the keyframe hook.
        let (status, body) = post_soap_media(
            port,
            "<SetSynchronizationPoint xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("SetSynchronizationPointResponse"));
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Multicast streaming is deliberately NOT registered — the
        // capabilities answer declares RTPMulticast=false, so the action
        // must not silently ack either.
        let (status, body) = post_soap_media(
            port,
            "<StartMulticastStreaming xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>",
        )
        .await;
        assert_eq!(status, 400, "multicast actions stay unregistered: {body}");
        assert!(body.contains("unsupported action"));

        handle.shutdown().await.unwrap();
    }
}
