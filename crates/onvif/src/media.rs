//! The ONVIF Media service (`ver10/media/wsdl`): one H.264 profile per
//! image sensor, each with its own video source and encoder configuration
//! (and, on the first sensor when the camera has a motor, the PTZ
//! configuration), plus where `rtsp` and web serve its stream and snapshot.
//!
//! Served: GetProfiles, GetStreamUri, GetSnapshotUri, GetVideoSources,
//! GetVideoSourceConfigurations, GetVideoSourceConfiguration,
//! GetVideoEncoderConfigurations, GetVideoEncoderConfiguration,
//! GetVideoEncoderConfigurationOptions,
//! GetCompatibleVideoEncoderConfigurations and GetAudioSources (empty: no
//! audio). Nothing can be changed: the stream parameters are the encoder's,
//! not the client's.
//!
//! Sensor N is, in every answer:
//!
//! ```text
//! profile          Profile_vsN      rtsp://<host>:<rtsp port>/vsN
//! video source     videoSrcN        (also its configuration's token)
//! encoder config   encN             http://<host>:<web port>/api/snapshot.jpg?camera=N
//! ```
//!
//! What each stream is (size, frame rate, bitrate, GOP, H.264 profile) comes
//! from the producer's `/status` (PRO-02, see `producer.rs`), so the answer
//! follows whatever the camera service actually encodes. While the producer
//! is unreachable, or reports no encoder for any sensor, the board profile's
//! `video.sensors` and `video.stream_for(N)` stand in, so NVRs still see the
//! same profiles and URLs and simply get no video until it is back (PRO-04).
//! A sensor the producer lists without stream info falls back to the board
//! values for that sensor only. Profiles are not dropped for a `failed`
//! sensor: a profile list that changes under an NVR makes it re-add the
//! camera.

use crate::producer::Producer;
use crate::ptz_soap;
use crate::soap::{request, response};
use onvif_device_rs::server::{OnvifActionHandler, OnvifServer};
use onvif_device_rs::types::{resolve_server_ip, OnvifError, RequestInfo};
use onvif_schema::xsd_parser_types::xml::AnyAttributes;
use onvif_schema::{trt, tt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use video_proto::{stream_path, Status, StreamInfo};

/// Token of sensor 0's video source, the one Imaging (IR-cut filter)
/// applies to.
pub const VIDEO_SOURCE_TOKEN: &str = "videoSrc0";

/// `tt:Quality` of the encoder configuration. The encoder has no quality
/// knob (it is rate-controlled); this is the middle of the advertised range.
const QUALITY: f32 = 5.0;
/// `tt:QualityRange` in the encoder options.
const QUALITY_RANGE: (i32, i32) = (1, 10);
/// `tt:SessionTimeout`: how long the RTSP session stays up unrefreshed.
const SESSION_TIMEOUT: &str = "PT60S";

/// Media profile token of sensor `index`: `Profile_vsN`.
pub fn profile_token(index: u32) -> String {
    format!("Profile_{}", stream_path(index))
}

fn source_token(index: u32) -> String {
    format!("videoSrc{index}")
}

fn encoder_token(index: u32) -> String {
    format!("enc{index}")
}

/// One sensor's H.264 stream, as the Media service describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoStream {
    /// Sensor index (`camera=N`, `vsN`).
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Target bitrate, kbit/s.
    pub kbps: u32,
    /// Frames between IDR pictures.
    pub gop: u32,
    /// H.264 profile, `baseline`/`main`/`high` in any case.
    pub profile: String,
}

impl VideoStream {
    /// Sensor `index` as the board profile configures it.
    pub fn from_board(index: u32, stream: &board::Stream) -> Self {
        Self {
            index,
            width: stream.width,
            height: stream.height,
            fps: stream.fps,
            kbps: stream.kbps,
            gop: stream.gop,
            profile: stream.profile.clone(),
        }
    }

    /// Sensor `index` as the producer reports it encoding; `None` when the
    /// report has no usable size.
    pub fn from_info(index: u32, info: &StreamInfo) -> Option<Self> {
        (info.width > 0 && info.height > 0).then(|| Self {
            index,
            width: info.width,
            height: info.height,
            fps: info.fps,
            kbps: info.target_kbps,
            gop: info.gop,
            profile: info.profile.clone(),
        })
    }

    fn h264_profile(&self) -> tt::H264ProfileType {
        match self.profile.to_ascii_lowercase().as_str() {
            "baseline" => tt::H264ProfileType::Baseline,
            "high" => tt::H264ProfileType::High,
            _ => tt::H264ProfileType::Main,
        }
    }

    fn resolution(&self) -> tt::VideoResolutionType {
        tt::VideoResolutionType {
            width: self.width as i32,
            height: self.height as i32,
        }
    }
}

/// The streams to describe: the producer's when it reported any, else the
/// board's. Sorted by sensor index.
pub fn select(statuses: Option<&[Status]>, fallback: &[VideoStream]) -> Vec<VideoStream> {
    let mut streams: Vec<VideoStream> = statuses
        .unwrap_or_default()
        .iter()
        .filter_map(|status| {
            status
                .stream
                .as_ref()
                .and_then(|info| VideoStream::from_info(status.index, info))
                .or_else(|| fallback.iter().find(|s| s.index == status.index).cloned())
        })
        .collect();
    if streams.is_empty() {
        streams = fallback.to_vec();
    }
    streams.sort_by_key(|s| s.index);
    streams.dedup_by_key(|s| s.index);
    streams
}

/// What the Media service reports.
#[derive(Debug)]
pub struct Media {
    /// Where stream parameters come from; `None` = the board only.
    pub producer: Option<Producer>,
    /// The board's streams, used while the producer has none. Never empty.
    pub fallback: Vec<VideoStream>,
    pub rtsp_port: u16,
    /// Port of web's snapshot endpoint; 0 means no snapshots.
    pub snapshot_port: u16,
    /// With a leading `/`.
    pub snapshot_path: String,
    /// Fallback address for URIs, used when the request arrived over
    /// loopback or on an unknown address (see [`resolve_server_ip`]).
    pub device_ip: String,
    /// Every profile carries the PTZ configuration (see `ptz_soap`).
    pub ptz: bool,
    /// The last answer came from the board, not the producer (logged on
    /// change only).
    pub on_fallback: AtomicBool,
}

/// The served actions, named as in the WSDL (all Media reads are `Get…`).
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    GetProfiles,
    GetStreamUri,
    GetSnapshotUri,
    GetVideoSources,
    GetVideoSourceConfigurations,
    GetVideoSourceConfiguration,
    GetVideoEncoderConfigurations,
    GetVideoEncoderConfiguration,
    GetVideoEncoderConfigurationOptions,
    GetCompatibleVideoEncoderConfigurations,
    GetAudioSources,
}

const ACTIONS: &[(&str, Action)] = &[
    ("GetProfiles", Action::GetProfiles),
    ("GetStreamUri", Action::GetStreamUri),
    ("GetSnapshotUri", Action::GetSnapshotUri),
    ("GetVideoSources", Action::GetVideoSources),
    (
        "GetVideoSourceConfigurations",
        Action::GetVideoSourceConfigurations,
    ),
    (
        "GetVideoSourceConfiguration",
        Action::GetVideoSourceConfiguration,
    ),
    (
        "GetVideoEncoderConfigurations",
        Action::GetVideoEncoderConfigurations,
    ),
    (
        "GetVideoEncoderConfiguration",
        Action::GetVideoEncoderConfiguration,
    ),
    (
        "GetVideoEncoderConfigurationOptions",
        Action::GetVideoEncoderConfigurationOptions,
    ),
    (
        "GetCompatibleVideoEncoderConfigurations",
        Action::GetCompatibleVideoEncoderConfigurations,
    ),
    ("GetAudioSources", Action::GetAudioSources),
];

/// Registers every served Media action on `server`.
pub fn register(server: &mut OnvifServer, media: Media) {
    let media = Arc::new(media);
    for (name, action) in ACTIONS {
        server.register_handler(
            name,
            Box::new(MediaAction {
                media: Arc::clone(&media),
                action: *action,
            }),
        );
    }
}

struct MediaAction {
    media: Arc<Media>,
    action: Action,
}

#[async_trait::async_trait]
impl OnvifActionHandler for MediaAction {
    async fn handle(&self, body: &str, info: &RequestInfo) -> Result<String, OnvifError> {
        let streams = self.media.streams().await;
        respond(&self.media, &streams, self.action, body, info)
    }
}

/// Parses the request for `action` and builds the response envelope.
fn respond(
    media: &Media,
    streams: &[VideoStream],
    action: Action,
    body: &str,
    info: &RequestInfo,
) -> Result<String, OnvifError> {
    use Action as A;
    // The address the client reached us on, so the URIs work from there.
    let host = resolve_server_ip(&info.server_ip, &media.device_ip);
    match action {
        A::GetProfiles => {
            request::<trt::GetProfiles>(body)?;
            let profiles = trt::GetProfilesResponse {
                profiles: streams.iter().map(|s| media.profile(s)).collect(),
            };
            response(&profiles, "trt:GetProfilesResponse")
        }
        A::GetStreamUri => {
            let req: trt::GetStreamUri = request(body)?;
            let stream = by_profile(streams, &req.profile_token)?;
            // One RTSP stream per sensor; whatever the StreamSetup asks
            // for, this is what the client can have.
            let uri = format!(
                "rtsp://{host}:{}/{}",
                media.rtsp_port,
                stream_path(stream.index)
            );
            let answer = trt::GetStreamUriResponse {
                media_uri: media_uri(uri, false, "PT0S"),
            };
            response(&answer, "trt:GetStreamUriResponse")
        }
        A::GetSnapshotUri => {
            let req: trt::GetSnapshotUri = request(body)?;
            let stream = by_profile(streams, &req.profile_token)?;
            if media.snapshot_port == 0 {
                return Err(OnvifError::ActionNotSupported(
                    "snapshot not supported".into(),
                ));
            }
            let separator = if media.snapshot_path.contains('?') {
                '&'
            } else {
                '?'
            };
            let uri = format!(
                "http://{host}:{}{}{separator}camera={}",
                media.snapshot_port, media.snapshot_path, stream.index
            );
            let answer = trt::GetSnapshotUriResponse {
                media_uri: media_uri(uri, true, "PT5S"),
            };
            response(&answer, "trt:GetSnapshotUriResponse")
        }
        A::GetVideoSources => {
            request::<trt::GetVideoSources>(body)?;
            let sources = trt::GetVideoSourcesResponse {
                video_sources: streams
                    .iter()
                    .map(|s| tt::VideoSourceType {
                        token: source_token(s.index),
                        any_attribute: AnyAttributes::default(),
                        framerate: s.fps as f32,
                        resolution: s.resolution(),
                        imaging: None,
                        extension: None,
                    })
                    .collect(),
            };
            response(&sources, "trt:GetVideoSourcesResponse")
        }
        A::GetVideoSourceConfigurations => {
            request::<trt::GetVideoSourceConfigurations>(body)?;
            let configurations = trt::GetVideoSourceConfigurationsResponse {
                configurations: streams.iter().map(video_source_configuration).collect(),
            };
            response(&configurations, "trt:GetVideoSourceConfigurationsResponse")
        }
        A::GetVideoSourceConfiguration => {
            let req: trt::GetVideoSourceConfiguration = request(body)?;
            let stream = by_configuration(streams, &req.configuration_token, source_token)?;
            let configuration = trt::GetVideoSourceConfigurationResponse {
                configuration: video_source_configuration(stream),
            };
            response(&configuration, "trt:GetVideoSourceConfigurationResponse")
        }
        A::GetVideoEncoderConfigurations => {
            request::<trt::GetVideoEncoderConfigurations>(body)?;
            let configurations = trt::GetVideoEncoderConfigurationsResponse {
                configurations: streams.iter().map(video_encoder_configuration).collect(),
            };
            response(&configurations, "trt:GetVideoEncoderConfigurationsResponse")
        }
        A::GetVideoEncoderConfiguration => {
            let req: trt::GetVideoEncoderConfiguration = request(body)?;
            let stream = by_configuration(streams, &req.configuration_token, encoder_token)?;
            let configuration = trt::GetVideoEncoderConfigurationResponse {
                configuration: video_encoder_configuration(stream),
            };
            response(&configuration, "trt:GetVideoEncoderConfigurationResponse")
        }
        A::GetVideoEncoderConfigurationOptions => {
            let req: trt::GetVideoEncoderConfigurationOptions = request(body)?;
            // The configuration named, else the profile's, else the first.
            let mut stream = streams.first();
            if let Some(token) = &req.profile_token {
                stream = Some(by_profile(streams, token)?);
            }
            if let Some(token) = &req.configuration_token {
                stream = Some(by_configuration(streams, token, encoder_token)?);
            }
            let stream = stream.ok_or_else(|| OnvifError::SenderFault("no video stream".into()))?;
            let options = trt::GetVideoEncoderConfigurationOptionsResponse {
                options: video_encoder_options(stream),
            };
            response(&options, "trt:GetVideoEncoderConfigurationOptionsResponse")
        }
        A::GetCompatibleVideoEncoderConfigurations => {
            // A sensor's encoder configuration fits only its own profile.
            let req: trt::GetCompatibleVideoEncoderConfigurations = request(body)?;
            let stream = by_profile(streams, &req.profile_token)?;
            let configurations = trt::GetCompatibleVideoEncoderConfigurationsResponse {
                configurations: vec![video_encoder_configuration(stream)],
            };
            response(
                &configurations,
                "trt:GetCompatibleVideoEncoderConfigurationsResponse",
            )
        }
        A::GetAudioSources => {
            request::<trt::GetAudioSources>(body)?;
            let sources = trt::GetAudioSourcesResponse {
                audio_sources: vec![],
            };
            response(&sources, "trt:GetAudioSourcesResponse")
        }
    }
}

fn by_profile<'a>(streams: &'a [VideoStream], token: &str) -> Result<&'a VideoStream, OnvifError> {
    streams
        .iter()
        .find(|s| profile_token(s.index) == token)
        .ok_or_else(|| OnvifError::SenderFault(format!("profile not found: {token}")))
}

fn by_configuration<'a>(
    streams: &'a [VideoStream],
    token: &str,
    token_of: fn(u32) -> String,
) -> Result<&'a VideoStream, OnvifError> {
    streams
        .iter()
        .find(|s| token_of(s.index) == token)
        .ok_or_else(|| OnvifError::SenderFault(format!("configuration not found: {token}")))
}

fn media_uri(uri: String, invalid_after_reboot: bool, timeout: &str) -> tt::MediaUriType {
    tt::MediaUriType {
        any_attribute: AnyAttributes::default(),
        uri,
        invalid_after_connect: false,
        invalid_after_reboot,
        timeout: timeout.into(),
        any: vec![],
    }
}

/// An inclusive `tt:IntRange`.
fn int_range(min: i32, max: i32) -> tt::IntRangeType {
    tt::IntRangeType { min, max }
}

impl Media {
    /// The streams right now: the producer's (cached), else the board's.
    async fn streams(&self) -> Vec<VideoStream> {
        let statuses = match &self.producer {
            Some(producer) => producer.statuses().await,
            None => None,
        };
        let live = statuses
            .as_deref()
            .is_some_and(|s| s.iter().any(|s| s.stream.is_some()));
        let was = self.on_fallback.swap(!live, Ordering::Relaxed);
        if self.producer.is_some() && was != !live {
            if live {
                tracing::info!("stream parameters from the producer");
            } else {
                tracing::warn!("producer has no stream info; profiles from the board");
            }
        }
        select(statuses.as_deref(), &self.fallback)
    }

    /// Sensor `stream`'s profile.
    fn profile(&self, stream: &VideoStream) -> tt::ProfileType {
        let token = profile_token(stream.index);
        tt::ProfileType {
            token: token.clone(),
            fixed: Some(true),
            any_attribute: AnyAttributes::default(),
            name: token,
            video_source_configuration: Some(video_source_configuration(stream)),
            audio_source_configuration: None,
            video_encoder_configuration: Some(video_encoder_configuration(stream)),
            audio_encoder_configuration: None,
            video_analytics_configuration: None,
            ptz_configuration: self.ptz.then(ptz_soap::configuration),
            metadata_configuration: None,
            extension: None,
        }
    }
}

/// The whole sensor image, unscaled.
fn video_source_configuration(stream: &VideoStream) -> tt::VideoSourceConfigurationType {
    tt::VideoSourceConfigurationType {
        token: source_token(stream.index),
        view_mode: None,
        any_attribute: AnyAttributes::default(),
        name: format!("VideoSourceConfig{}", stream.index),
        use_count: 1,
        source_token: source_token(stream.index),
        bounds: tt::IntRectangleType {
            x: 0,
            y: 0,
            width: stream.width as i32,
            height: stream.height as i32,
        },
        any: vec![],
        extension: None,
    }
}

/// H.264 with the encoder's GOP, unicast.
fn video_encoder_configuration(stream: &VideoStream) -> tt::VideoEncoderConfigurationType {
    tt::VideoEncoderConfigurationType {
        token: encoder_token(stream.index),
        guaranteed_frame_rate: None,
        any_attribute: AnyAttributes::default(),
        name: format!("VideoEncoderConfig{}", stream.index),
        use_count: 1,
        encoding: tt::VideoEncodingType::H264,
        resolution: stream.resolution(),
        quality: QUALITY,
        rate_control: Some(tt::VideoRateControlType {
            frame_rate_limit: stream.fps as i32,
            encoding_interval: 1,
            // The schema's unit is kbit/s.
            bitrate_limit: stream.kbps as i32,
        }),
        mpeg_4: None,
        h264: Some(tt::H264ConfigurationType {
            gov_length: stream.gop as i32,
            h264_profile: stream.h264_profile(),
        }),
        // Required by the schema; no multicast is served.
        multicast: tt::MulticastConfigurationType {
            any_attribute: AnyAttributes::default(),
            address: tt::IpAddressType {
                type_: tt::IpType::IPv4,
                i_pv_4_address: Some("0.0.0.0".into()),
                i_pv_6_address: None,
            },
            port: 0,
            ttl: 0,
            auto_start: false,
            any: vec![],
        },
        session_timeout: SESSION_TIMEOUT.into(),
        any: vec![],
    }
}

fn video_encoder_options(stream: &VideoStream) -> tt::VideoEncoderConfigurationOptionsType {
    tt::VideoEncoderConfigurationOptionsType {
        guaranteed_frame_rate_supported: None,
        any_attribute: AnyAttributes::default(),
        quality_range: int_range(QUALITY_RANGE.0, QUALITY_RANGE.1),
        jpeg: None,
        mpeg_4: None,
        h264: Some(tt::H264OptionsType {
            resolutions_available: vec![stream.resolution()],
            gov_length_range: int_range(1, stream.gop.max(1) as i32),
            frame_rate_range: int_range(1, stream.fps.max(1) as i32),
            encoding_interval_range: int_range(1, 1),
            h264_profiles_supported: vec![
                tt::H264ProfileType::Baseline,
                tt::H264ProfileType::Main,
                tt::H264ProfileType::High,
            ],
        }),
        extension: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(index: u32, width: u32) -> VideoStream {
        VideoStream {
            index,
            width,
            height: width * 9 / 16,
            fps: 15,
            kbps: 2048,
            gop: 30,
            profile: "main".into(),
        }
    }

    fn status(index: u32, info: Option<StreamInfo>) -> Status {
        Status {
            index,
            state: "running".into(),
            stream: info,
            ..Status::default()
        }
    }

    fn info(width: u32, height: u32) -> StreamInfo {
        StreamInfo {
            codec: "H.264".into(),
            profile: "High".into(),
            width,
            height,
            fps: 25,
            gop: 50,
            target_kbps: 4096,
            ..StreamInfo::default()
        }
    }

    #[test]
    fn tokens() {
        assert_eq!(profile_token(0), "Profile_vs0");
        assert_eq!(profile_token(1), "Profile_vs1");
        assert_eq!(source_token(0), VIDEO_SOURCE_TOKEN);
        assert_eq!(encoder_token(1), "enc1");
    }

    #[test]
    fn producer_streams_win() {
        let fallback = [stream(0, 1280), stream(1, 1280)];
        let statuses = [
            status(1, Some(info(640, 360))),
            status(0, Some(info(1920, 1080))),
        ];
        let streams = select(Some(&statuses), &fallback);
        assert_eq!(
            streams
                .iter()
                .map(|s| (s.index, s.width))
                .collect::<Vec<_>>(),
            [(0, 1920), (1, 640)]
        );
        assert_eq!(
            (streams[0].fps, streams[0].kbps, streams[0].gop),
            (25, 4096, 50)
        );
        assert!(matches!(
            streams[0].h264_profile(),
            tt::H264ProfileType::High
        ));
    }

    #[test]
    fn board_fills_the_gaps() {
        let fallback = [stream(0, 1280), stream(1, 1280)];
        // Producer down.
        assert_eq!(select(None, &fallback), fallback);
        // Producer up, but no encoder anywhere.
        assert_eq!(
            select(Some(&[status(0, None), status(1, None)]), &fallback),
            fallback
        );
        assert_eq!(select(Some(&[]), &fallback), fallback);
        // One sensor without stream info: the board's values for it.
        let streams = select(
            Some(&[status(0, Some(info(640, 360))), status(1, None)]),
            &fallback,
        );
        let mut want = fallback.to_vec();
        want[0] = VideoStream::from_info(0, &info(640, 360)).unwrap();
        assert_eq!(streams, want);
        // A zero-sized report is not usable.
        assert_eq!(
            select(Some(&[status(0, Some(info(0, 0)))]), &fallback[..1]),
            fallback[..1]
        );
    }

    /// Statuses are matched to sensors by `index`, not by position: sparse
    /// (sensor 0 missing) and out of order.
    #[test]
    fn statuses_match_by_index_not_position() {
        let fallback = [stream(0, 1280), stream(1, 1280), stream(2, 1280)];
        let streams = select(
            Some(&[status(2, Some(info(640, 360))), status(1, None)]),
            &fallback,
        );
        assert_eq!(
            streams
                .iter()
                .map(|s| (s.index, s.width))
                .collect::<Vec<_>>(),
            [(1, 1280), (2, 640)]
        );
        // A `sensors = [1]` board: the producer's sensor 1 is profile 1.
        let streams = select(Some(&[status(1, Some(info(640, 360)))]), &fallback[1..2]);
        assert_eq!(
            streams
                .iter()
                .map(|s| (s.index, s.width))
                .collect::<Vec<_>>(),
            [(1, 640)]
        );
    }

    #[test]
    fn h264_profile_names() {
        let mut s = stream(0, 1280);
        for (name, want) in [
            ("baseline", tt::H264ProfileType::Baseline),
            ("Main", tt::H264ProfileType::Main),
            ("HIGH", tt::H264ProfileType::High),
            ("", tt::H264ProfileType::Main),
        ] {
            s.profile = name.into();
            assert_eq!(
                std::mem::discriminant(&s.h264_profile()),
                std::mem::discriminant(&want),
                "{name}"
            );
        }
    }
}
