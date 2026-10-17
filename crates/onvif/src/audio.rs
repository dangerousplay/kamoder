//! The Media service's audio (AUD-14..17, AUD-27, AUD-28, AUD-31): the
//! board's microphones as audio sources with one G.711 encoder
//! configuration, its speakers as audio outputs with one G.711 decoder
//! configuration (the ONVIF backchannel), and the speaker volume as
//! `AudioOutputConfiguration.OutputLevel`.
//!
//! Tokens come from the board profile's device ids (`[[audio.input]]`,
//! `[[audio.output]]`), so they stay the same across restarts:
//!
//! ```text
//! audio source          <input id>                     e.g. mic0
//! source configuration  AudioSourceConfig_<input id>   AudioSourceConfig_mic0
//! encoder configuration AudioEncoderConfig_0           G711, 64 kbit/s, 8 kHz
//! audio output          <output id>                    e.g. spk0
//! output configuration  AudioOutputConfig_<output id>  AudioOutputConfig_spk0
//! decoder configuration AudioDecoderConfig_0           G711, 64 kbit/s, 8 kHz
//! ```
//!
//! Every profile carries the first source configuration and the encoder
//! configuration (the PCMA track is part of `/vsN`, so `GetStreamUri` does
//! not change), and in its `Extension` the first output and the decoder
//! configuration. A board without `[audio]` has none of them: the lists are
//! empty and single-configuration requests fault, as before audio existed.
//!
//! Nothing about the encoding can change: the `audio` service speaks
//! 8 kHz G.711 only (A-law on the RTSP track). `Set…Configuration` and
//! `Add…`/`Remove…` are accepted when they ask for exactly that — the
//! profiles are fixed, so Add and Remove leave them as they are — and fault
//! `ter:InvalidArgVal/ter:ConfigModify` otherwise. The one thing that does
//! change is the speaker volume: `OutputLevel` is the output's gain in the
//! driver's own units, its range the board's `gain_range`, read and written
//! through the audio socket (`audio-proto` `status`/`set`, shared with the web
//! UI, AUD-25). While the audio service does not answer, the board's default
//! volume is reported and setting it is a Receiver fault.
//!
//! SPEC_DEVIATION (AUD-17): `onvif-device-rs` cannot emit fault subcodes;
//! faults are SOAP Sender faults (HTTP 400) whose reason starts with the
//! subcode path, e.g. `ter:InvalidArgVal/ter:ConfigModify: …`.

use crate::media::{self, Media};
use crate::soap::{request, response};
use audio_proto::{Client, ClientError, ErrorCode};
use onvif_device_rs::server::{OnvifActionHandler, OnvifServer};
use onvif_device_rs::types::{OnvifError, RequestInfo};
use onvif_schema::xsd_parser_types::xml::AnyAttributes;
use onvif_schema::{trt, tt};
use std::sync::Arc;
use std::time::Duration;

/// The one encoder configuration (every microphone shares it).
pub const ENCODER_TOKEN: &str = "AudioEncoderConfig_0";
/// The one decoder configuration (every speaker shares it).
pub const DECODER_TOKEN: &str = "AudioDecoderConfig_0";
/// G.711 at 8 kHz, one byte per sample: kbit/s, ONVIF's `Bitrate` unit.
pub const G711_KBPS: i32 = 64;
/// ONVIF's `SampleRate` unit is kHz.
pub const SAMPLE_RATE_KHZ: i32 = 8;
/// Mono, as the audio socket is ([`audio_proto::CHANNELS`]).
const CHANNELS: i32 = audio_proto::CHANNELS as i32;
/// How long a read of the current volume waits for the audio service. Short:
/// GetProfiles must not hang on it.
const STATUS_TIMEOUT: Duration = Duration::from_millis(500);

/// Token of the source configuration of microphone `id`.
pub fn source_config_token(id: &str) -> String {
    format!("AudioSourceConfig_{id}")
}

/// Token of the output configuration of speaker `id`.
pub fn output_config_token(id: &str) -> String {
    format!("AudioOutputConfig_{id}")
}

/// The board's audio devices and where their gain lives.
#[derive(Debug, Clone, Default)]
pub struct Audio {
    /// Microphones; empty = no audio sources.
    pub inputs: Vec<board::AudioDevice>,
    /// Speakers; empty = no audio outputs.
    pub outputs: Vec<board::AudioDevice>,
    /// The audio service; `None` (`--audio-socket ""`) = board defaults only,
    /// the volume cannot be set.
    pub client: Option<Client>,
}

/// What the audio configurations of a profile are built from: the
/// generated types are not `Clone`, so each profile gets its own copy.
#[derive(Debug, Clone, Default)]
pub struct ProfileAudio {
    /// The number of profiles (every configuration's `UseCount`).
    pub uses: usize,
    /// The speakers' volumes, in `outputs` order (see [`Audio::levels`]).
    pub levels: Vec<i32>,
}

impl Audio {
    /// The board's `[audio]`, if any, served through `client`.
    pub fn new(board: Option<&board::Audio>, client: Option<Client>) -> Self {
        match board {
            Some(audio) => Self {
                inputs: audio.inputs.clone(),
                outputs: audio.outputs.clone(),
                client,
            },
            None => Self::default(),
        }
    }

    /// The volume of each output, in `outputs` order: the audio service's,
    /// else the board's default, else the bottom of the range.
    pub async fn levels(&self) -> Vec<i32> {
        let live = match (&self.client, self.outputs.is_empty()) {
            (Some(client), false) => {
                match client.clone().with_timeout(STATUS_TIMEOUT).status().await {
                    Ok(devices) => devices,
                    Err(error) => {
                        tracing::debug!(%error, "audio service unreachable; board volumes");
                        vec![]
                    }
                }
            }
            _ => vec![],
        };
        self.outputs
            .iter()
            .map(|output| {
                let current = live
                    .iter()
                    .find(|d| d.id == output.id && d.kind == audio_proto::DeviceKind::Output)
                    .and_then(|d| d.gain);
                current.or(output.gain).unwrap_or(output.gain_range[0]) as i32
            })
            .collect()
    }

    /// The volumes for the profiles, `uses` being their number. No socket
    /// round trip without a speaker.
    pub async fn for_profiles(&self, uses: usize) -> ProfileAudio {
        ProfileAudio {
            uses,
            levels: self.levels().await,
        }
    }

    /// The first microphone's source configuration, for a profile.
    pub fn profile_source(&self, at: &ProfileAudio) -> Option<tt::AudioSourceConfigurationType> {
        self.source_configurations(at.uses).into_iter().next()
    }

    /// The encoder configuration, for a profile.
    pub fn profile_encoder(&self, at: &ProfileAudio) -> Option<tt::AudioEncoderConfigurationType> {
        self.encoder_configuration(at.uses)
    }

    /// The first speaker's output and the decoder configuration, for a
    /// profile's `Extension`; `None` without a speaker.
    pub fn profile_extension(&self, at: &ProfileAudio) -> Option<tt::ProfileExtensionType> {
        let output = self
            .output_configurations(at.uses, &at.levels)
            .into_iter()
            .next()?;
        Some(tt::ProfileExtensionType {
            any_attribute: AnyAttributes::default(),
            any: vec![],
            audio_output_configuration: Some(output),
            audio_decoder_configuration: self.decoder_configuration(at.uses),
            extension: None,
        })
    }

    fn source_configurations(&self, uses: usize) -> Vec<tt::AudioSourceConfigurationType> {
        self.inputs
            .iter()
            .map(|input| tt::AudioSourceConfigurationType {
                token: source_config_token(&input.id),
                any_attribute: AnyAttributes::default(),
                name: input.name.clone(),
                use_count: use_count(uses),
                source_token: input.id.clone(),
                any: vec![],
            })
            .collect()
    }

    fn encoder_configuration(&self, uses: usize) -> Option<tt::AudioEncoderConfigurationType> {
        (!self.inputs.is_empty()).then(|| tt::AudioEncoderConfigurationType {
            token: ENCODER_TOKEN.into(),
            any_attribute: AnyAttributes::default(),
            name: "G711".into(),
            use_count: use_count(uses),
            encoding: tt::AudioEncodingType::G711,
            bitrate: G711_KBPS,
            sample_rate: SAMPLE_RATE_KHZ,
            multicast: media::unicast_only(),
            session_timeout: media::SESSION_TIMEOUT.into(),
            any: vec![],
        })
    }

    fn output_configurations(
        &self,
        uses: usize,
        levels: &[i32],
    ) -> Vec<tt::AudioOutputConfigurationType> {
        self.outputs
            .iter()
            .zip(levels)
            .map(|(output, &level)| tt::AudioOutputConfigurationType {
                token: output_config_token(&output.id),
                any_attribute: AnyAttributes::default(),
                name: output.name.clone(),
                use_count: use_count(uses),
                output_token: output.id.clone(),
                // Talk-back takes the speaker whatever the microphone does.
                send_primacy: None,
                output_level: level,
                any: vec![],
            })
            .collect()
    }

    fn decoder_configuration(&self, uses: usize) -> Option<tt::AudioDecoderConfigurationType> {
        (!self.outputs.is_empty()).then(|| tt::AudioDecoderConfigurationType {
            token: DECODER_TOKEN.into(),
            any_attribute: AnyAttributes::default(),
            name: "G711".into(),
            use_count: use_count(uses),
            any: vec![],
        })
    }

    fn input_of(&self, config_token: &str) -> Result<&board::AudioDevice, OnvifError> {
        self.inputs
            .iter()
            .find(|d| source_config_token(&d.id) == config_token)
            .ok_or_else(|| no_config(config_token))
    }

    fn output_of(&self, config_token: &str) -> Result<&board::AudioDevice, OnvifError> {
        self.outputs
            .iter()
            .find(|d| output_config_token(&d.id) == config_token)
            .ok_or_else(|| no_config(config_token))
    }

    fn expect_encoder(&self, token: &str) -> Result<(), OnvifError> {
        if !self.inputs.is_empty() && token == ENCODER_TOKEN {
            Ok(())
        } else {
            Err(no_config(token))
        }
    }

    fn expect_decoder(&self, token: &str) -> Result<(), OnvifError> {
        if !self.outputs.is_empty() && token == DECODER_TOKEN {
            Ok(())
        } else {
            Err(no_config(token))
        }
    }

    /// Sets speaker `output`'s volume through the audio service.
    async fn set_level(&self, output: &board::AudioDevice, level: i32) -> Result<(), OnvifError> {
        let [min, max] = output.gain_range;
        let level = u32::try_from(level)
            .ok()
            .filter(|l| (min..=max).contains(l))
            .ok_or_else(|| {
                config_modify(format!(
                    "OutputLevel {level} is outside [{min}, {max}] (see GetAudioOutputConfigurationOptions)"
                ))
            })?;
        let client = self.client.as_ref().ok_or_else(|| {
            OnvifError::Internal("the audio service is not configured (--audio-socket)".into())
        })?;
        match client.set(output.id.as_str(), level).await {
            Ok(()) => {
                tracing::info!(device = %output.id, level, "speaker volume set over ONVIF");
                Ok(())
            }
            Err(ClientError::Rejected {
                code:
                    code @ (ErrorCode::OutOfRange | ErrorCode::UnknownDevice | ErrorCode::BadRequest),
                message,
            }) => Err(config_modify(format!(
                "audio service refused ({code}): {message}"
            ))),
            Err(error) => {
                tracing::warn!(%error, "cannot set the speaker volume");
                Err(OnvifError::Internal(format!(
                    "audio service unavailable: {error}"
                )))
            }
        }
    }
}

fn use_count(uses: usize) -> i32 {
    i32::try_from(uses).unwrap_or(i32::MAX)
}

fn no_config(token: &str) -> OnvifError {
    OnvifError::SenderFault(format!(
        "ter:InvalidArgVal/ter:NoConfig: configuration not found: {token}"
    ))
}

fn no_profile(token: &str) -> OnvifError {
    OnvifError::SenderFault(format!(
        "ter:InvalidArgVal/ter:NoProfile: profile not found: {token}"
    ))
}

fn config_modify(message: impl std::fmt::Display) -> OnvifError {
    OnvifError::SenderFault(format!("ter:InvalidArgVal/ter:ConfigModify: {message}"))
}

/// The served actions, named as in the WSDL.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    GetAudioSources,
    GetAudioSourceConfigurations,
    GetAudioSourceConfiguration,
    GetCompatibleAudioSourceConfigurations,
    GetAudioSourceConfigurationOptions,
    SetAudioSourceConfiguration,
    AddAudioSourceConfiguration,
    RemoveAudioSourceConfiguration,
    GetAudioEncoderConfigurations,
    GetAudioEncoderConfiguration,
    GetCompatibleAudioEncoderConfigurations,
    GetAudioEncoderConfigurationOptions,
    SetAudioEncoderConfiguration,
    AddAudioEncoderConfiguration,
    RemoveAudioEncoderConfiguration,
    GetAudioOutputs,
    GetAudioOutputConfigurations,
    GetAudioOutputConfiguration,
    GetCompatibleAudioOutputConfigurations,
    GetAudioOutputConfigurationOptions,
    SetAudioOutputConfiguration,
    AddAudioOutputConfiguration,
    RemoveAudioOutputConfiguration,
    GetAudioDecoderConfigurations,
    GetAudioDecoderConfiguration,
    GetCompatibleAudioDecoderConfigurations,
    GetAudioDecoderConfigurationOptions,
    SetAudioDecoderConfiguration,
    AddAudioDecoderConfiguration,
    RemoveAudioDecoderConfiguration,
}

const ACTIONS: &[(&str, Action)] = {
    use Action as A;
    &[
        ("GetAudioSources", A::GetAudioSources),
        (
            "GetAudioSourceConfigurations",
            A::GetAudioSourceConfigurations,
        ),
        (
            "GetAudioSourceConfiguration",
            A::GetAudioSourceConfiguration,
        ),
        (
            "GetCompatibleAudioSourceConfigurations",
            A::GetCompatibleAudioSourceConfigurations,
        ),
        (
            "GetAudioSourceConfigurationOptions",
            A::GetAudioSourceConfigurationOptions,
        ),
        (
            "SetAudioSourceConfiguration",
            A::SetAudioSourceConfiguration,
        ),
        (
            "AddAudioSourceConfiguration",
            A::AddAudioSourceConfiguration,
        ),
        (
            "RemoveAudioSourceConfiguration",
            A::RemoveAudioSourceConfiguration,
        ),
        (
            "GetAudioEncoderConfigurations",
            A::GetAudioEncoderConfigurations,
        ),
        (
            "GetAudioEncoderConfiguration",
            A::GetAudioEncoderConfiguration,
        ),
        (
            "GetCompatibleAudioEncoderConfigurations",
            A::GetCompatibleAudioEncoderConfigurations,
        ),
        (
            "GetAudioEncoderConfigurationOptions",
            A::GetAudioEncoderConfigurationOptions,
        ),
        (
            "SetAudioEncoderConfiguration",
            A::SetAudioEncoderConfiguration,
        ),
        (
            "AddAudioEncoderConfiguration",
            A::AddAudioEncoderConfiguration,
        ),
        (
            "RemoveAudioEncoderConfiguration",
            A::RemoveAudioEncoderConfiguration,
        ),
        ("GetAudioOutputs", A::GetAudioOutputs),
        (
            "GetAudioOutputConfigurations",
            A::GetAudioOutputConfigurations,
        ),
        (
            "GetAudioOutputConfiguration",
            A::GetAudioOutputConfiguration,
        ),
        (
            "GetCompatibleAudioOutputConfigurations",
            A::GetCompatibleAudioOutputConfigurations,
        ),
        (
            "GetAudioOutputConfigurationOptions",
            A::GetAudioOutputConfigurationOptions,
        ),
        (
            "SetAudioOutputConfiguration",
            A::SetAudioOutputConfiguration,
        ),
        (
            "AddAudioOutputConfiguration",
            A::AddAudioOutputConfiguration,
        ),
        (
            "RemoveAudioOutputConfiguration",
            A::RemoveAudioOutputConfiguration,
        ),
        (
            "GetAudioDecoderConfigurations",
            A::GetAudioDecoderConfigurations,
        ),
        (
            "GetAudioDecoderConfiguration",
            A::GetAudioDecoderConfiguration,
        ),
        (
            "GetCompatibleAudioDecoderConfigurations",
            A::GetCompatibleAudioDecoderConfigurations,
        ),
        (
            "GetAudioDecoderConfigurationOptions",
            A::GetAudioDecoderConfigurationOptions,
        ),
        (
            "SetAudioDecoderConfiguration",
            A::SetAudioDecoderConfiguration,
        ),
        (
            "AddAudioDecoderConfiguration",
            A::AddAudioDecoderConfiguration,
        ),
        (
            "RemoveAudioDecoderConfiguration",
            A::RemoveAudioDecoderConfiguration,
        ),
    ]
};

/// Registers every audio action on `server`. Registered whether or not the
/// board has audio, so a client gets empty lists rather than "unsupported
/// action".
pub fn register(server: &mut OnvifServer, media: &Arc<Media>) {
    for (name, action) in ACTIONS {
        server.register_handler(
            name,
            Box::new(AudioAction {
                media: Arc::clone(media),
                action: *action,
            }),
        );
    }
}

struct AudioAction {
    media: Arc<Media>,
    action: Action,
}

#[async_trait::async_trait]
impl OnvifActionHandler for AudioAction {
    async fn handle(&self, body: &str, _info: &RequestInfo) -> Result<String, OnvifError> {
        let profiles: Vec<String> = self
            .media
            .streams()
            .await
            .iter()
            .map(|s| media::profile_token(s.index))
            .collect();
        respond(&self.media.audio, &profiles, self.action, body).await
    }
}

fn check_profile(profiles: &[String], token: &str) -> Result<(), OnvifError> {
    if profiles.iter().any(|p| p == token) {
        Ok(())
    } else {
        Err(no_profile(token))
    }
}

/// The configuration and profile tokens of a `Get…ConfigurationOptions`,
/// both optional, checked.
fn check_options_tokens(
    profiles: &[String],
    profile: Option<&str>,
    configuration: Option<&str>,
    known: impl Fn(&str) -> bool,
) -> Result<(), OnvifError> {
    if let Some(token) = profile {
        check_profile(profiles, token)?;
    }
    match configuration {
        Some(token) if !known(token) => Err(no_config(token)),
        _ => Ok(()),
    }
}

/// `value` is `want`, also when a client sent it in the base unit (bit/s,
/// Hz) instead of ONVIF's (kbit/s, kHz).
fn is_or_base_unit(value: i32, want: i32) -> bool {
    value == want || value == want * 1000
}

/// Parses the request for `action`, carries it out and builds the response
/// envelope.
async fn respond(
    audio: &Audio,
    profiles: &[String],
    action: Action,
    body: &str,
) -> Result<String, OnvifError> {
    use Action as A;
    let uses = profiles.len();
    match action {
        // ---------------------------------------------------- sources ----
        A::GetAudioSources => {
            request::<trt::GetAudioSources>(body)?;
            let sources = trt::GetAudioSourcesResponse {
                audio_sources: audio
                    .inputs
                    .iter()
                    .map(|input| tt::AudioSourceType {
                        token: input.id.clone(),
                        any_attribute: AnyAttributes::default(),
                        channels: CHANNELS,
                        any: vec![],
                    })
                    .collect(),
            };
            response(&sources, "trt:GetAudioSourcesResponse")
        }
        A::GetAudioSourceConfigurations => {
            request::<trt::GetAudioSourceConfigurations>(body)?;
            let answer = trt::GetAudioSourceConfigurationsResponse {
                configurations: audio.source_configurations(uses),
            };
            response(&answer, "trt:GetAudioSourceConfigurationsResponse")
        }
        A::GetAudioSourceConfiguration => {
            let req: trt::GetAudioSourceConfiguration = request(body)?;
            audio.input_of(&req.configuration_token)?;
            let configuration = audio
                .source_configurations(uses)
                .into_iter()
                .find(|c| c.token == req.configuration_token)
                .ok_or_else(|| no_config(&req.configuration_token))?;
            let answer = trt::GetAudioSourceConfigurationResponse { configuration };
            response(&answer, "trt:GetAudioSourceConfigurationResponse")
        }
        A::GetCompatibleAudioSourceConfigurations => {
            let req: trt::GetCompatibleAudioSourceConfigurations = request(body)?;
            check_profile(profiles, &req.profile_token)?;
            let answer = trt::GetCompatibleAudioSourceConfigurationsResponse {
                configurations: audio.source_configurations(uses),
            };
            response(
                &answer,
                "trt:GetCompatibleAudioSourceConfigurationsResponse",
            )
        }
        A::GetAudioSourceConfigurationOptions => {
            let req: trt::GetAudioSourceConfigurationOptions = request(body)?;
            check_options_tokens(
                profiles,
                req.profile_token.as_deref(),
                req.configuration_token.as_deref(),
                |token| audio.input_of(token).is_ok(),
            )?;
            // A configuration is bound to its own microphone.
            let inputs = match &req.configuration_token {
                Some(token) => vec![audio.input_of(token)?.id.clone()],
                None => audio.inputs.iter().map(|d| d.id.clone()).collect(),
            };
            if inputs.is_empty() {
                return Err(no_config("no audio source"));
            }
            let answer = trt::GetAudioSourceConfigurationOptionsResponse {
                options: tt::AudioSourceConfigurationOptionsType {
                    any_attribute: AnyAttributes::default(),
                    input_tokens_available: inputs,
                    extension: None,
                },
            };
            response(&answer, "trt:GetAudioSourceConfigurationOptionsResponse")
        }
        A::SetAudioSourceConfiguration => {
            let req: trt::SetAudioSourceConfiguration = request(body)?;
            let input = audio.input_of(&req.configuration.token)?;
            if req.configuration.source_token != input.id {
                return Err(config_modify(format!(
                    "{} is bound to source {}",
                    req.configuration.token, input.id
                )));
            }
            response(
                &trt::SetAudioSourceConfigurationResponse {},
                "trt:SetAudioSourceConfigurationResponse",
            )
        }
        A::AddAudioSourceConfiguration => {
            let req: trt::AddAudioSourceConfiguration = request(body)?;
            check_profile(profiles, &req.profile_token)?;
            audio.input_of(&req.configuration_token)?;
            response(
                &trt::AddAudioSourceConfigurationResponse {},
                "trt:AddAudioSourceConfigurationResponse",
            )
        }
        A::RemoveAudioSourceConfiguration => {
            let req: trt::RemoveAudioSourceConfiguration = request(body)?;
            check_profile(profiles, &req.profile_token)?;
            response(
                &trt::RemoveAudioSourceConfigurationResponse {},
                "trt:RemoveAudioSourceConfigurationResponse",
            )
        }

        // --------------------------------------------------- encoders ----
        A::GetAudioEncoderConfigurations => {
            request::<trt::GetAudioEncoderConfigurations>(body)?;
            let answer = trt::GetAudioEncoderConfigurationsResponse {
                configurations: audio.encoder_configuration(uses).into_iter().collect(),
            };
            response(&answer, "trt:GetAudioEncoderConfigurationsResponse")
        }
        A::GetAudioEncoderConfiguration => {
            let req: trt::GetAudioEncoderConfiguration = request(body)?;
            audio.expect_encoder(&req.configuration_token)?;
            let configuration = audio
                .encoder_configuration(uses)
                .ok_or_else(|| no_config(&req.configuration_token))?;
            let answer = trt::GetAudioEncoderConfigurationResponse { configuration };
            response(&answer, "trt:GetAudioEncoderConfigurationResponse")
        }
        A::GetCompatibleAudioEncoderConfigurations => {
            let req: trt::GetCompatibleAudioEncoderConfigurations = request(body)?;
            check_profile(profiles, &req.profile_token)?;
            let answer = trt::GetCompatibleAudioEncoderConfigurationsResponse {
                configurations: audio.encoder_configuration(uses).into_iter().collect(),
            };
            response(
                &answer,
                "trt:GetCompatibleAudioEncoderConfigurationsResponse",
            )
        }
        A::GetAudioEncoderConfigurationOptions => {
            let req: trt::GetAudioEncoderConfigurationOptions = request(body)?;
            check_options_tokens(
                profiles,
                req.profile_token.as_deref(),
                req.configuration_token.as_deref(),
                |token| audio.expect_encoder(token).is_ok(),
            )?;
            if audio.inputs.is_empty() {
                return Err(no_config("no audio source"));
            }
            let answer = trt::GetAudioEncoderConfigurationOptionsResponse {
                options: tt::AudioEncoderConfigurationOptionsType {
                    any_attribute: AnyAttributes::default(),
                    options: vec![tt::AudioEncoderConfigurationOptionType {
                        any_attribute: AnyAttributes::default(),
                        encoding: tt::AudioEncodingType::G711,
                        bitrate_list: tt::IntItemsType {
                            items: vec![G711_KBPS],
                        },
                        sample_rate_list: tt::IntItemsType {
                            items: vec![SAMPLE_RATE_KHZ],
                        },
                        any: vec![],
                    }],
                },
            };
            response(&answer, "trt:GetAudioEncoderConfigurationOptionsResponse")
        }
        A::SetAudioEncoderConfiguration => {
            let req: trt::SetAudioEncoderConfiguration = request(body)?;
            let c = &req.configuration;
            audio.expect_encoder(&c.token)?;
            if !matches!(c.encoding, tt::AudioEncodingType::G711) {
                return Err(config_modify(format!(
                    "encoding {:?} is not supported, only G711",
                    c.encoding
                )));
            }
            if !is_or_base_unit(c.bitrate, G711_KBPS) {
                return Err(config_modify(format!(
                    "bitrate {} is not supported, only {G711_KBPS} kbit/s",
                    c.bitrate
                )));
            }
            if !is_or_base_unit(c.sample_rate, SAMPLE_RATE_KHZ) {
                return Err(config_modify(format!(
                    "sample rate {} is not supported, only {SAMPLE_RATE_KHZ} kHz",
                    c.sample_rate
                )));
            }
            // Name, multicast and session timeout are not ours to change;
            // there is nothing else to apply.
            response(
                &trt::SetAudioEncoderConfigurationResponse {},
                "trt:SetAudioEncoderConfigurationResponse",
            )
        }
        A::AddAudioEncoderConfiguration => {
            let req: trt::AddAudioEncoderConfiguration = request(body)?;
            check_profile(profiles, &req.profile_token)?;
            audio.expect_encoder(&req.configuration_token)?;
            response(
                &trt::AddAudioEncoderConfigurationResponse {},
                "trt:AddAudioEncoderConfigurationResponse",
            )
        }
        A::RemoveAudioEncoderConfiguration => {
            let req: trt::RemoveAudioEncoderConfiguration = request(body)?;
            check_profile(profiles, &req.profile_token)?;
            response(
                &trt::RemoveAudioEncoderConfigurationResponse {},
                "trt:RemoveAudioEncoderConfigurationResponse",
            )
        }

        // ---------------------------------------------------- outputs ----
        A::GetAudioOutputs => {
            request::<trt::GetAudioOutputs>(body)?;
            let answer = trt::GetAudioOutputsResponse {
                audio_outputs: audio
                    .outputs
                    .iter()
                    .map(|output| tt::AudioOutputType {
                        token: output.id.clone(),
                        any_attribute: AnyAttributes::default(),
                        any: vec![],
                    })
                    .collect(),
            };
            response(&answer, "trt:GetAudioOutputsResponse")
        }
        A::GetAudioOutputConfigurations => {
            request::<trt::GetAudioOutputConfigurations>(body)?;
            let levels = audio.levels().await;
            let answer = trt::GetAudioOutputConfigurationsResponse {
                configurations: audio.output_configurations(uses, &levels),
            };
            response(&answer, "trt:GetAudioOutputConfigurationsResponse")
        }
        A::GetAudioOutputConfiguration => {
            let req: trt::GetAudioOutputConfiguration = request(body)?;
            audio.output_of(&req.configuration_token)?;
            let levels = audio.levels().await;
            let configuration = audio
                .output_configurations(uses, &levels)
                .into_iter()
                .find(|c| c.token == req.configuration_token)
                .ok_or_else(|| no_config(&req.configuration_token))?;
            let answer = trt::GetAudioOutputConfigurationResponse { configuration };
            response(&answer, "trt:GetAudioOutputConfigurationResponse")
        }
        A::GetCompatibleAudioOutputConfigurations => {
            let req: trt::GetCompatibleAudioOutputConfigurations = request(body)?;
            check_profile(profiles, &req.profile_token)?;
            let levels = audio.levels().await;
            let answer = trt::GetCompatibleAudioOutputConfigurationsResponse {
                configurations: audio.output_configurations(uses, &levels),
            };
            response(
                &answer,
                "trt:GetCompatibleAudioOutputConfigurationsResponse",
            )
        }
        A::GetAudioOutputConfigurationOptions => {
            let req: trt::GetAudioOutputConfigurationOptions = request(body)?;
            check_options_tokens(
                profiles,
                req.profile_token.as_deref(),
                req.configuration_token.as_deref(),
                |token| audio.output_of(token).is_ok(),
            )?;
            // The named configuration's speaker, else the first.
            let output = match &req.configuration_token {
                Some(token) => audio.output_of(token)?,
                None => audio
                    .outputs
                    .first()
                    .ok_or_else(|| no_config("no audio output"))?,
            };
            let [min, max] = output.gain_range;
            let answer = trt::GetAudioOutputConfigurationOptionsResponse {
                options: tt::AudioOutputConfigurationOptionsType {
                    any_attribute: AnyAttributes::default(),
                    output_tokens_available: vec![output.id.clone()],
                    send_primacy_options: vec![],
                    // The driver's own units (AUD-31); the web UI shows the
                    // same numbers.
                    output_level_range: tt::IntRangeType {
                        min: min as i32,
                        max: max as i32,
                    },
                    eq_preset_schedule_support: None,
                    eq_presets: vec![],
                    any: vec![],
                },
            };
            response(&answer, "trt:GetAudioOutputConfigurationOptionsResponse")
        }
        A::SetAudioOutputConfiguration => {
            let req: trt::SetAudioOutputConfiguration = request(body)?;
            let c = &req.configuration;
            let output = audio.output_of(&c.token)?;
            if c.output_token != output.id {
                return Err(config_modify(format!(
                    "{} is bound to output {}",
                    c.token, output.id
                )));
            }
            if c.send_primacy.is_some() {
                return Err(config_modify("SendPrimacy is not supported"));
            }
            // Always persisted by the audio service, whatever
            // ForcePersistence says.
            audio.set_level(output, c.output_level).await?;
            response(
                &trt::SetAudioOutputConfigurationResponse {},
                "trt:SetAudioOutputConfigurationResponse",
            )
        }
        A::AddAudioOutputConfiguration => {
            let req: trt::AddAudioOutputConfiguration = request(body)?;
            check_profile(profiles, &req.profile_token)?;
            audio.output_of(&req.configuration_token)?;
            response(
                &trt::AddAudioOutputConfigurationResponse {},
                "trt:AddAudioOutputConfigurationResponse",
            )
        }
        A::RemoveAudioOutputConfiguration => {
            let req: trt::RemoveAudioOutputConfiguration = request(body)?;
            check_profile(profiles, &req.profile_token)?;
            response(
                &trt::RemoveAudioOutputConfigurationResponse {},
                "trt:RemoveAudioOutputConfigurationResponse",
            )
        }

        // --------------------------------------------------- decoders ----
        A::GetAudioDecoderConfigurations => {
            request::<trt::GetAudioDecoderConfigurations>(body)?;
            let answer = trt::GetAudioDecoderConfigurationsResponse {
                configurations: audio.decoder_configuration(uses).into_iter().collect(),
            };
            response(&answer, "trt:GetAudioDecoderConfigurationsResponse")
        }
        A::GetAudioDecoderConfiguration => {
            let req: trt::GetAudioDecoderConfiguration = request(body)?;
            audio.expect_decoder(&req.configuration_token)?;
            let configuration = audio
                .decoder_configuration(uses)
                .ok_or_else(|| no_config(&req.configuration_token))?;
            let answer = trt::GetAudioDecoderConfigurationResponse { configuration };
            response(&answer, "trt:GetAudioDecoderConfigurationResponse")
        }
        A::GetCompatibleAudioDecoderConfigurations => {
            let req: trt::GetCompatibleAudioDecoderConfigurations = request(body)?;
            check_profile(profiles, &req.profile_token)?;
            let answer = trt::GetCompatibleAudioDecoderConfigurationsResponse {
                configurations: audio.decoder_configuration(uses).into_iter().collect(),
            };
            response(
                &answer,
                "trt:GetCompatibleAudioDecoderConfigurationsResponse",
            )
        }
        A::GetAudioDecoderConfigurationOptions => {
            let req: trt::GetAudioDecoderConfigurationOptions = request(body)?;
            check_options_tokens(
                profiles,
                req.profile_token.as_deref(),
                req.configuration_token.as_deref(),
                |token| audio.expect_decoder(token).is_ok(),
            )?;
            if audio.outputs.is_empty() {
                return Err(no_config("no audio output"));
            }
            let answer = trt::GetAudioDecoderConfigurationOptionsResponse {
                options: tt::AudioDecoderConfigurationOptionsType {
                    any_attribute: AnyAttributes::default(),
                    aac_dec_options: None,
                    g711_dec_options: Some(tt::G711DecOptionsType {
                        any_attribute: AnyAttributes::default(),
                        bitrate: tt::IntItemsType {
                            items: vec![G711_KBPS],
                        },
                        sample_rate_range: tt::IntItemsType {
                            items: vec![SAMPLE_RATE_KHZ],
                        },
                        any: vec![],
                    }),
                    g726_dec_options: None,
                    extension: None,
                },
            };
            response(&answer, "trt:GetAudioDecoderConfigurationOptionsResponse")
        }
        A::SetAudioDecoderConfiguration => {
            // A decoder configuration has nothing but its name to set.
            let req: trt::SetAudioDecoderConfiguration = request(body)?;
            audio.expect_decoder(&req.configuration.token)?;
            response(
                &trt::SetAudioDecoderConfigurationResponse {},
                "trt:SetAudioDecoderConfigurationResponse",
            )
        }
        A::AddAudioDecoderConfiguration => {
            let req: trt::AddAudioDecoderConfiguration = request(body)?;
            check_profile(profiles, &req.profile_token)?;
            audio.expect_decoder(&req.configuration_token)?;
            response(
                &trt::AddAudioDecoderConfigurationResponse {},
                "trt:AddAudioDecoderConfigurationResponse",
            )
        }
        A::RemoveAudioDecoderConfiguration => {
            let req: trt::RemoveAudioDecoderConfiguration = request(body)?;
            check_profile(profiles, &req.profile_token)?;
            response(
                &trt::RemoveAudioDecoderConfigurationResponse {},
                "trt:RemoveAudioDecoderConfigurationResponse",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, gain: Option<u32>, range: [u32; 2]) -> board::AudioDevice {
        board::AudioDevice {
            id: id.into(),
            name: id.into(),
            device: id.into(),
            gain,
            gain_range: range,
        }
    }

    #[test]
    fn tokens() {
        assert_eq!(source_config_token("mic0"), "AudioSourceConfig_mic0");
        assert_eq!(output_config_token("spk0"), "AudioOutputConfig_spk0");
    }

    #[test]
    fn base_units_are_tolerated() {
        assert!(is_or_base_unit(64, G711_KBPS));
        assert!(is_or_base_unit(64_000, G711_KBPS));
        assert!(is_or_base_unit(8_000, SAMPLE_RATE_KHZ));
        assert!(!is_or_base_unit(32, G711_KBPS));
        assert!(!is_or_base_unit(16, SAMPLE_RATE_KHZ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn levels_fall_back_to_the_board() {
        let audio = Audio {
            inputs: vec![],
            outputs: vec![device("a", Some(4), [0, 6]), device("b", None, [2, 9])],
            // Nothing listens there.
            client: Some(Client::new(std::env::temp_dir().join(format!(
                "kamoder-onvif-audio-{}-none.sock",
                std::process::id()
            )))),
        };
        assert_eq!(audio.levels().await, [4, 2]);
        let none = Audio::new(None, None);
        assert!(none.levels().await.is_empty());
        let parts = none.for_profiles(2).await;
        assert!(none.profile_source(&parts).is_none());
        assert!(none.profile_encoder(&parts).is_none());
        assert!(none.profile_extension(&parts).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn out_of_range_level_is_config_modify() {
        let audio = Audio {
            inputs: vec![],
            outputs: vec![device("spk", Some(1), [0, 6])],
            client: None,
        };
        for level in [-1, 7] {
            let error = audio.set_level(&audio.outputs[0], level).await.unwrap_err();
            assert!(
                matches!(&error, OnvifError::SenderFault(m) if m.contains("ter:ConfigModify")),
                "{error}"
            );
        }
        // In range, but nobody to tell: the camera's problem.
        let error = audio.set_level(&audio.outputs[0], 3).await.unwrap_err();
        assert!(matches!(error, OnvifError::Internal(_)), "{error}");
    }
}
