//! The ONVIF Media service (`ver10/media/wsdl`): one H.264 profile (`main`)
//! with its video source, encoder and, when the camera has a motor, PTZ
//! configuration, plus where `rtsp_demo` and web serve the stream and the
//! snapshot.
//!
//! Served: GetProfiles, GetStreamUri, GetSnapshotUri, GetVideoSources,
//! GetVideoSourceConfigurations, GetVideoSourceConfiguration,
//! GetVideoEncoderConfigurations, GetVideoEncoderConfiguration,
//! GetVideoEncoderConfigurationOptions,
//! GetCompatibleVideoEncoderConfigurations and GetAudioSources (empty: no
//! audio). Nothing can be changed: the stream parameters are the encoder's,
//! not the client's.
//!
//! Every answer is built from the one [`Media`] description, so the
//! listings always agree with the profile.

use crate::ptz_soap;
use crate::soap::{request, response};
use onvif_device_rs::server::{OnvifActionHandler, OnvifServer};
use onvif_device_rs::types::{resolve_server_ip, OnvifError, RequestInfo};
use onvif_schema::xsd_parser_types::xml::AnyAttributes;
use onvif_schema::{trt, tt};
use std::sync::Arc;

/// Token of the video source, also used for its configuration.
pub const VIDEO_SOURCE_TOKEN: &str = "videoSrc0";
/// Token of the video encoder configuration.
pub const ENCODER_TOKEN: &str = "enc0";

/// `tt:Quality` of the encoder configuration. The encoder has no quality
/// knob (it is rate-controlled); this is the middle of the advertised range.
const QUALITY: f32 = 5.0;
/// `tt:QualityRange` in the encoder options.
const QUALITY_RANGE: (i32, i32) = (1, 10);
/// `tt:SessionTimeout`: how long the RTSP session stays up unrefreshed.
const SESSION_TIMEOUT: &str = "PT60S";
/// The encoder's GOP, in seconds (`rtsp_demo`'s default `gop_len`).
const GOP_SECONDS: i32 = 2;

/// What the Media service reports.
#[derive(Debug, Clone)]
pub struct Media {
    pub profile_token: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Bits per second.
    pub bitrate: u32,
    pub rtsp_port: u16,
    /// With a leading `/`.
    pub rtsp_path: String,
    /// Port of web's snapshot endpoint; 0 means no snapshots.
    pub snapshot_port: u16,
    /// With a leading `/`.
    pub snapshot_path: String,
    /// Fallback address for URIs, used when the request arrived over
    /// loopback or on an unknown address (see [`resolve_server_ip`]).
    pub device_ip: String,
    /// The profile carries the PTZ configuration.
    pub ptz: bool,
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
        respond(&self.media, self.action, body, info)
    }
}

/// Parses the request for `action` and builds the response envelope.
fn respond(
    media: &Media,
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
                profiles: vec![media.profile()],
            };
            response(&profiles, "trt:GetProfilesResponse")
        }
        A::GetStreamUri => {
            let req: trt::GetStreamUri = request(body)?;
            media.check_profile(&req.profile_token)?;
            // Only the one RTSP stream exists; whatever the StreamSetup
            // asks for, this is what the client can have.
            let uri = format!("rtsp://{host}:{}{}", media.rtsp_port, media.rtsp_path);
            let answer = trt::GetStreamUriResponse {
                media_uri: media_uri(uri, false, "PT0S"),
            };
            response(&answer, "trt:GetStreamUriResponse")
        }
        A::GetSnapshotUri => {
            let req: trt::GetSnapshotUri = request(body)?;
            media.check_profile(&req.profile_token)?;
            if media.snapshot_port == 0 {
                return Err(OnvifError::ActionNotSupported(
                    "snapshot not supported".into(),
                ));
            }
            let uri = format!(
                "http://{host}:{}{}",
                media.snapshot_port, media.snapshot_path
            );
            let answer = trt::GetSnapshotUriResponse {
                media_uri: media_uri(uri, true, "PT5S"),
            };
            response(&answer, "trt:GetSnapshotUriResponse")
        }
        A::GetVideoSources => {
            request::<trt::GetVideoSources>(body)?;
            let sources = trt::GetVideoSourcesResponse {
                video_sources: vec![tt::VideoSourceType {
                    token: VIDEO_SOURCE_TOKEN.into(),
                    any_attribute: AnyAttributes::default(),
                    framerate: media.fps as f32,
                    resolution: media.resolution(),
                    imaging: None,
                    extension: None,
                }],
            };
            response(&sources, "trt:GetVideoSourcesResponse")
        }
        A::GetVideoSourceConfigurations => {
            request::<trt::GetVideoSourceConfigurations>(body)?;
            let configurations = trt::GetVideoSourceConfigurationsResponse {
                configurations: vec![media.video_source_configuration()],
            };
            response(&configurations, "trt:GetVideoSourceConfigurationsResponse")
        }
        A::GetVideoSourceConfiguration => {
            let req: trt::GetVideoSourceConfiguration = request(body)?;
            check_configuration(&req.configuration_token, VIDEO_SOURCE_TOKEN)?;
            let configuration = trt::GetVideoSourceConfigurationResponse {
                configuration: media.video_source_configuration(),
            };
            response(&configuration, "trt:GetVideoSourceConfigurationResponse")
        }
        A::GetVideoEncoderConfigurations => {
            request::<trt::GetVideoEncoderConfigurations>(body)?;
            let configurations = trt::GetVideoEncoderConfigurationsResponse {
                configurations: vec![media.video_encoder_configuration()],
            };
            response(&configurations, "trt:GetVideoEncoderConfigurationsResponse")
        }
        A::GetVideoEncoderConfiguration => {
            let req: trt::GetVideoEncoderConfiguration = request(body)?;
            check_configuration(&req.configuration_token, ENCODER_TOKEN)?;
            let configuration = trt::GetVideoEncoderConfigurationResponse {
                configuration: media.video_encoder_configuration(),
            };
            response(&configuration, "trt:GetVideoEncoderConfigurationResponse")
        }
        A::GetVideoEncoderConfigurationOptions => {
            let req: trt::GetVideoEncoderConfigurationOptions = request(body)?;
            if let Some(token) = &req.configuration_token {
                check_configuration(token, ENCODER_TOKEN)?;
            }
            if let Some(token) = &req.profile_token {
                media.check_profile(token)?;
            }
            let options = trt::GetVideoEncoderConfigurationOptionsResponse {
                options: media.video_encoder_options(),
            };
            response(&options, "trt:GetVideoEncoderConfigurationOptionsResponse")
        }
        A::GetCompatibleVideoEncoderConfigurations => {
            // Every encoder configuration fits the one profile.
            let req: trt::GetCompatibleVideoEncoderConfigurations = request(body)?;
            media.check_profile(&req.profile_token)?;
            let configurations = trt::GetCompatibleVideoEncoderConfigurationsResponse {
                configurations: vec![media.video_encoder_configuration()],
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

fn check_configuration(token: &str, want: &str) -> Result<(), OnvifError> {
    if token == want {
        Ok(())
    } else {
        Err(OnvifError::SenderFault(format!(
            "configuration not found: {token}"
        )))
    }
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
    fn check_profile(&self, token: &str) -> Result<(), OnvifError> {
        if token == self.profile_token {
            Ok(())
        } else {
            Err(OnvifError::SenderFault(format!(
                "profile not found: {token}"
            )))
        }
    }

    fn resolution(&self) -> tt::VideoResolutionType {
        tt::VideoResolutionType {
            width: self.width as i32,
            height: self.height as i32,
        }
    }

    /// The one profile.
    fn profile(&self) -> tt::ProfileType {
        tt::ProfileType {
            token: self.profile_token.clone(),
            fixed: Some(true),
            any_attribute: AnyAttributes::default(),
            name: self.profile_token.clone(),
            video_source_configuration: Some(self.video_source_configuration()),
            audio_source_configuration: None,
            video_encoder_configuration: Some(self.video_encoder_configuration()),
            audio_encoder_configuration: None,
            video_analytics_configuration: None,
            ptz_configuration: self.ptz.then(ptz_soap::configuration),
            metadata_configuration: None,
            extension: None,
        }
    }

    /// The whole sensor image, unscaled.
    fn video_source_configuration(&self) -> tt::VideoSourceConfigurationType {
        tt::VideoSourceConfigurationType {
            token: VIDEO_SOURCE_TOKEN.into(),
            view_mode: None,
            any_attribute: AnyAttributes::default(),
            name: "VideoSourceConfig".into(),
            use_count: 1,
            source_token: VIDEO_SOURCE_TOKEN.into(),
            bounds: tt::IntRectangleType {
                x: 0,
                y: 0,
                width: self.width as i32,
                height: self.height as i32,
            },
            any: vec![],
            extension: None,
        }
    }

    /// H.264 Main with the encoder's GOP, unicast.
    fn video_encoder_configuration(&self) -> tt::VideoEncoderConfigurationType {
        tt::VideoEncoderConfigurationType {
            token: ENCODER_TOKEN.into(),
            guaranteed_frame_rate: None,
            any_attribute: AnyAttributes::default(),
            name: "VideoEncoderConfig".into(),
            use_count: 1,
            encoding: tt::VideoEncodingType::H264,
            resolution: self.resolution(),
            quality: QUALITY,
            rate_control: Some(tt::VideoRateControlType {
                frame_rate_limit: self.fps as i32,
                encoding_interval: 1,
                // The schema's unit is kbit/s.
                bitrate_limit: (self.bitrate / 1000) as i32,
            }),
            mpeg_4: None,
            h264: Some(tt::H264ConfigurationType {
                gov_length: GOP_SECONDS * self.fps as i32,
                h264_profile: tt::H264ProfileType::Main,
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

    fn video_encoder_options(&self) -> tt::VideoEncoderConfigurationOptionsType {
        let fps = self.fps.max(1) as i32;
        tt::VideoEncoderConfigurationOptionsType {
            guaranteed_frame_rate_supported: None,
            any_attribute: AnyAttributes::default(),
            quality_range: int_range(QUALITY_RANGE.0, QUALITY_RANGE.1),
            jpeg: None,
            mpeg_4: None,
            h264: Some(tt::H264OptionsType {
                resolutions_available: vec![self.resolution()],
                gov_length_range: int_range(1, GOP_SECONDS * fps),
                frame_rate_range: int_range(1, fps),
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
}
