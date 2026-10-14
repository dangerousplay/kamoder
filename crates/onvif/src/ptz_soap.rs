//! The ONVIF PTZ service (`ver20/ptz/wsdl`) on top of [`Ptz`].
//!
//! One node (`ptz_node`) with one configuration (`ptz_config`) attached to
//! the first sensor's media profile (one head, however many sensors). Coordinates use ONVIF's generic spaces:
//! positions and translations in `[-1, 1]` over the backend's limits,
//! velocities in `[-1, 1]` (the backend's own unit), speed ignored. No zoom.
//!
//! Requests and responses are the generated `tptz`/`tt` types (see
//! `soap.rs`): each action parses its `tptz::X`, acts, and answers with a
//! `tptz::XResponse` value.
//!
//! Served: GetNodes, GetNode, GetConfigurations, GetConfiguration,
//! GetConfigurationOptions, GetStatus, ContinuousMove, Stop, AbsoluteMove,
//! RelativeMove, GetPresets, SetPreset, GotoPreset, RemovePreset,
//! GotoHomePosition. Anything else in the PTZ WSDL (SetConfiguration,
//! SetHomePosition, preset tours, auxiliary commands, …) is not registered
//! and gets the library's "unsupported action" fault. GetServiceCapabilities
//! is not registered either: the library routes by action name only, and the
//! Device and Media services share that name.

use crate::ptz::Ptz;
use crate::soap::{request, response};
use onvif_device_rs::server::{OnvifActionHandler, OnvifServer};
use onvif_device_rs::types::{OnvifError, RequestInfo};
use onvif_schema::xsd_parser_types::xml::AnyAttributes;
use onvif_schema::{tptz, tt};
use std::sync::Arc;
use std::time::Duration;

pub const NODE_TOKEN: &str = "ptz_node";
pub const CONFIG_TOKEN: &str = "ptz_config";

const POSITION_SPACE: &str = "http://www.onvif.org/ver10/tptz/PanTiltSpaces/PositionGenericSpace";
const TRANSLATION_SPACE: &str =
    "http://www.onvif.org/ver10/tptz/PanTiltSpaces/TranslationGenericSpace";
const VELOCITY_SPACE: &str = "http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace";
const SPEED_SPACE: &str = "http://www.onvif.org/ver10/tptz/PanTiltSpaces/GenericSpeedSpace";

/// ContinuousMove without a `Timeout` stops after this (the configuration's
/// `DefaultPTZTimeout`): an NVR that loses its Stop must not leave the head
/// running into the end stop forever.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest `Timeout` honoured; longer requests are clamped.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60);
/// Shortest `Timeout` honoured; shorter (non-zero) requests are raised.
const MIN_TIMEOUT: Duration = Duration::from_millis(100);

/// Values up to this far outside `[-1, 1]` are clamped rather than refused
/// (clients compute positions in floating point too).
const RANGE_SLACK: f32 = 1e-3;

/// The served actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    GetNodes,
    GetNode,
    GetConfigurations,
    GetConfiguration,
    GetConfigurationOptions,
    GetStatus,
    ContinuousMove,
    Stop,
    AbsoluteMove,
    RelativeMove,
    GetPresets,
    SetPreset,
    GotoPreset,
    RemovePreset,
    GotoHomePosition,
}

const ACTIONS: &[(&str, Action)] = &[
    ("GetNodes", Action::GetNodes),
    ("GetNode", Action::GetNode),
    ("GetConfigurations", Action::GetConfigurations),
    ("GetConfiguration", Action::GetConfiguration),
    ("GetConfigurationOptions", Action::GetConfigurationOptions),
    ("GetStatus", Action::GetStatus),
    ("ContinuousMove", Action::ContinuousMove),
    ("Stop", Action::Stop),
    ("AbsoluteMove", Action::AbsoluteMove),
    ("RelativeMove", Action::RelativeMove),
    ("GetPresets", Action::GetPresets),
    ("SetPreset", Action::SetPreset),
    ("GotoPreset", Action::GotoPreset),
    ("RemovePreset", Action::RemovePreset),
    ("GotoHomePosition", Action::GotoHomePosition),
];

/// Registers every served PTZ action on `server`.
pub fn register(server: &mut OnvifServer, ptz: &Arc<Ptz>, profile_token: &str) {
    let profile: Arc<str> = Arc::from(profile_token);
    for (name, action) in ACTIONS {
        server.register_handler(
            name,
            Box::new(PtzAction {
                ptz: Arc::clone(ptz),
                profile: Arc::clone(&profile),
                action: *action,
            }),
        );
    }
}

struct PtzAction {
    ptz: Arc<Ptz>,
    profile: Arc<str>,
    action: Action,
}

#[async_trait::async_trait]
impl OnvifActionHandler for PtzAction {
    async fn handle(&self, body: &str, _info: &RequestInfo) -> Result<String, OnvifError> {
        respond(&self.ptz, &self.profile, self.action, body)
    }
}

/// Parses the request for `action`, carries it out and builds the response
/// envelope.
fn respond(ptz: &Ptz, profile: &str, action: Action, body: &str) -> Result<String, OnvifError> {
    use Action as A;
    match action {
        A::GetNodes => {
            request::<tptz::GetNodes>(body)?;
            let nodes = tptz::GetNodesResponse {
                ptz_node: vec![node()],
            };
            response(&nodes, "tptz:GetNodesResponse")
        }
        A::GetNode => {
            let req: tptz::GetNode = request(body)?;
            expect_token(&req.node_token, NODE_TOKEN, "PTZ node")?;
            let node = tptz::GetNodeResponse { ptz_node: node() };
            response(&node, "tptz:GetNodeResponse")
        }
        A::GetConfigurations => {
            request::<tptz::GetConfigurations>(body)?;
            let configurations = tptz::GetConfigurationsResponse {
                ptz_configuration: vec![configuration()],
            };
            response(&configurations, "tptz:GetConfigurationsResponse")
        }
        A::GetConfiguration => {
            let req: tptz::GetConfiguration = request(body)?;
            expect_token(
                &req.ptz_configuration_token,
                CONFIG_TOKEN,
                "PTZ configuration",
            )?;
            let configuration = tptz::GetConfigurationResponse {
                ptz_configuration: configuration(),
            };
            response(&configuration, "tptz:GetConfigurationResponse")
        }
        A::GetConfigurationOptions => {
            let req: tptz::GetConfigurationOptions = request(body)?;
            expect_token(&req.configuration_token, CONFIG_TOKEN, "PTZ configuration")?;
            let options = tptz::GetConfigurationOptionsResponse {
                ptz_configuration_options: configuration_options(),
            };
            response(&options, "tptz:GetConfigurationOptionsResponse")
        }
        A::GetStatus => {
            let req: tptz::GetStatus = request(body)?;
            check_profile(&req.profile_token, profile)?;
            let status = tptz::GetStatusResponse {
                ptz_status: status(ptz),
            };
            response(&status, "tptz:GetStatusResponse")
        }
        A::ContinuousMove => {
            let req: tptz::ContinuousMove = request(body)?;
            check_profile(&req.profile_token, profile)?;
            let velocity = &req.velocity;
            let (x, y) = pan_tilt(
                velocity.pan_tilt.as_ref(),
                velocity.zoom.is_some(),
                "Velocity",
                VELOCITY_SPACE,
            )?;
            let timeout = match req.timeout.as_deref() {
                Some(text) => parse_duration(text)
                    .ok_or_else(|| sender(format!("invalid Timeout {text:?}")))?,
                None => Duration::ZERO,
            };
            let timeout = if timeout.is_zero() {
                DEFAULT_TIMEOUT
            } else {
                timeout.clamp(MIN_TIMEOUT, MAX_TIMEOUT)
            };
            ptz.continuous_move(x, y, Some(timeout))
                .map_err(backend_fault)?;
            response(
                &tptz::ContinuousMoveResponse {},
                "tptz:ContinuousMoveResponse",
            )
        }
        A::Stop => {
            let req: tptz::Stop = request(body)?;
            check_profile(&req.profile_token, profile)?;
            // `PanTilt=false` asks to stop only zoom, which does not exist.
            if req.pan_tilt.unwrap_or(true) {
                ptz.stop().map_err(backend_fault)?;
            }
            response(&tptz::StopResponse {}, "tptz:StopResponse")
        }
        A::AbsoluteMove => {
            let req: tptz::AbsoluteMove = request(body)?;
            check_profile(&req.profile_token, profile)?;
            let position = &req.position;
            let (x, y) = pan_tilt(
                position.pan_tilt.as_ref(),
                position.zoom.is_some(),
                "Position",
                POSITION_SPACE,
            )?;
            let (x, y) = (in_range(x, "x")?, in_range(y, "y")?);
            let (pan, tilt) = to_degrees(&ptz.limits(), x, y);
            ptz.goto(pan, tilt).map_err(backend_fault)?;
            response(&tptz::AbsoluteMoveResponse {}, "tptz:AbsoluteMoveResponse")
        }
        A::RelativeMove => {
            let req: tptz::RelativeMove = request(body)?;
            check_profile(&req.profile_token, profile)?;
            let translation = &req.translation;
            let (dx, dy) = pan_tilt(
                translation.pan_tilt.as_ref(),
                translation.zoom.is_some(),
                "Translation",
                TRANSLATION_SPACE,
            )?;
            let limits = ptz.limits();
            let here = ptz.position();
            let pan = here.pan_deg + dx * (limits.pan_max_deg - limits.pan_min_deg) / 2.0;
            let tilt = here.tilt_deg + dy * (limits.tilt_max_deg - limits.tilt_min_deg) / 2.0;
            ptz.goto(pan, tilt).map_err(backend_fault)?;
            response(&tptz::RelativeMoveResponse {}, "tptz:RelativeMoveResponse")
        }
        A::GetPresets => {
            let req: tptz::GetPresets = request(body)?;
            check_profile(&req.profile_token, profile)?;
            let limits = ptz.limits();
            let preset = ptz
                .presets()
                .into_iter()
                .map(|preset| {
                    let (x, y) = to_normalised(&limits, preset.pan_deg, preset.tilt_deg);
                    tt::PtzPresetType {
                        token: Some(preset.token),
                        any_attribute: AnyAttributes::default(),
                        name: Some(preset.name),
                        ptz_position: Some(position_vector(x, y)),
                    }
                })
                .collect();
            response(
                &tptz::GetPresetsResponse { preset },
                "tptz:GetPresetsResponse",
            )
        }
        A::SetPreset => {
            let req: tptz::SetPreset = request(body)?;
            check_profile(&req.profile_token, profile)?;
            let name = req.preset_name.as_deref().unwrap_or("");
            let token = req.preset_token.as_deref().filter(|t| !t.is_empty());
            let preset = ptz.set_preset(name, token).map_err(backend_fault)?;
            response(
                &tptz::SetPresetResponse {
                    preset_token: preset.token,
                },
                "tptz:SetPresetResponse",
            )
        }
        A::GotoPreset => {
            let req: tptz::GotoPreset = request(body)?;
            check_profile(&req.profile_token, profile)?;
            let token = required(&req.preset_token, "PresetToken")?;
            ptz.goto_preset(token).map_err(backend_fault)?;
            response(&tptz::GotoPresetResponse {}, "tptz:GotoPresetResponse")
        }
        A::RemovePreset => {
            let req: tptz::RemovePreset = request(body)?;
            check_profile(&req.profile_token, profile)?;
            let token = required(&req.preset_token, "PresetToken")?;
            ptz.remove_preset(token).map_err(backend_fault)?;
            response(&tptz::RemovePresetResponse {}, "tptz:RemovePresetResponse")
        }
        A::GotoHomePosition => {
            let req: tptz::GotoHomePosition = request(body)?;
            check_profile(&req.profile_token, profile)?;
            ptz.home().map_err(backend_fault)?;
            response(
                &tptz::GotoHomePositionResponse {},
                "tptz:GotoHomePositionResponse",
            )
        }
    }
}

// ------------------------------------------------------------ responses ----

/// `[min, max]` as a `tt:FloatRange`.
fn range(min: f32, max: f32) -> tt::FloatRangeType {
    tt::FloatRangeType { min, max }
}

/// A 2D space over `[-1, 1]` on both axes.
fn space2d(uri: &str) -> tt::Space2DDescriptionType {
    tt::Space2DDescriptionType {
        uri: uri.into(),
        x_range: range(-1.0, 1.0),
        y_range: range(-1.0, 1.0),
    }
}

/// `tt:PTZSpaces`: what the node and the configuration options list.
fn spaces() -> tt::PtzSpacesType {
    tt::PtzSpacesType {
        any_attribute: AnyAttributes::default(),
        absolute_pan_tilt_position_space: vec![space2d(POSITION_SPACE)],
        absolute_zoom_position_space: vec![],
        relative_pan_tilt_translation_space: vec![space2d(TRANSLATION_SPACE)],
        relative_zoom_translation_space: vec![],
        continuous_pan_tilt_velocity_space: vec![space2d(VELOCITY_SPACE)],
        continuous_zoom_velocity_space: vec![],
        pan_tilt_speed_space: vec![tt::Space1DDescriptionType {
            uri: SPEED_SPACE.into(),
            x_range: range(0.0, 1.0),
        }],
        zoom_speed_space: vec![],
        extension: None,
    }
}

/// The one `tt:PTZNode`.
fn node() -> tt::PtzNodeType {
    tt::PtzNodeType {
        token: NODE_TOKEN.into(),
        fixed_home_position: Some(true),
        geo_move: None,
        any_attribute: AnyAttributes::default(),
        name: Some("Pan/Tilt".into()),
        supported_ptz_spaces: spaces(),
        maximum_number_of_presets: anyka_ptz::MAX_PRESETS as i32,
        home_supported: true,
        auxiliary_commands: vec![],
        extension: None,
    }
}

/// The one `tt:PTZConfiguration`, also embedded in the first sensor's media
/// profile.
/// (`default_absolute_pant_tilt_position_space` follows the schema's own
/// spelling, `DefaultAbsolutePantTiltPositionSpace`.)
pub fn configuration() -> tt::PtzConfigurationType {
    tt::PtzConfigurationType {
        token: CONFIG_TOKEN.into(),
        move_ramp: None,
        preset_ramp: None,
        preset_tour_ramp: None,
        any_attribute: AnyAttributes::default(),
        name: "PTZ".into(),
        use_count: 1,
        node_token: NODE_TOKEN.into(),
        default_absolute_pant_tilt_position_space: Some(POSITION_SPACE.into()),
        default_absolute_zoom_position_space: None,
        default_relative_pan_tilt_translation_space: Some(TRANSLATION_SPACE.into()),
        default_relative_zoom_translation_space: None,
        default_continuous_pan_tilt_velocity_space: Some(VELOCITY_SPACE.into()),
        default_continuous_zoom_velocity_space: None,
        default_ptz_speed: Some(tt::PtzSpeedType {
            pan_tilt: Some(tt::Vector2DType {
                x: 1.0,
                y: 1.0,
                space: Some(SPEED_SPACE.into()),
            }),
            zoom: None,
        }),
        default_ptz_timeout: Some(format!("PT{}S", DEFAULT_TIMEOUT.as_secs())),
        pan_tilt_limits: Some(tt::PanTiltLimitsType {
            range: space2d(POSITION_SPACE),
        }),
        zoom_limits: None,
        extension: None,
    }
}

fn configuration_options() -> tt::PtzConfigurationOptionsType {
    tt::PtzConfigurationOptionsType {
        ptz_ramps: None,
        any_attribute: AnyAttributes::default(),
        spaces: spaces(),
        ptz_timeout: tt::DurationRangeType {
            min: format!("PT{}S", MIN_TIMEOUT.as_secs_f32()),
            max: format!("PT{}S", MAX_TIMEOUT.as_secs()),
        },
        any: vec![],
        pt_control_direction: None,
        extension: None,
    }
}

/// A pan/tilt position in the generic position space.
fn position_vector(x: f32, y: f32) -> tt::PtzVectorType {
    tt::PtzVectorType {
        pan_tilt: Some(tt::Vector2DType {
            x,
            y,
            space: Some(POSITION_SPACE.into()),
        }),
        zoom: None,
    }
}

fn status(ptz: &Ptz) -> tt::PtzStatusType {
    let here = ptz.position();
    let (x, y) = to_normalised(&ptz.limits(), here.pan_deg, here.tilt_deg);
    tt::PtzStatusType {
        any_attribute: AnyAttributes::default(),
        position: Some(position_vector(x, y)),
        move_status: Some(tt::PtzMoveStatusType {
            pan_tilt: Some(if here.moving {
                tt::MoveStatusType::Moving
            } else {
                tt::MoveStatusType::Idle
            }),
            zoom: None,
        }),
        error: None,
        utc_time: utc_now(),
        field_of_view: None,
        any: vec![],
    }
}

// ---------------------------------------------------------- coordinates ----

/// Degrees → ONVIF generic position space.
pub fn to_normalised(limits: &anyka_ptz::Limits, pan_deg: f32, tilt_deg: f32) -> (f32, f32) {
    let norm = |v: f32, lo: f32, hi: f32| ((v - lo) / (hi - lo) * 2.0 - 1.0).clamp(-1.0, 1.0);
    (
        norm(pan_deg, limits.pan_min_deg, limits.pan_max_deg),
        norm(tilt_deg, limits.tilt_min_deg, limits.tilt_max_deg),
    )
}

/// ONVIF generic position space → degrees.
pub fn to_degrees(limits: &anyka_ptz::Limits, x: f32, y: f32) -> (f32, f32) {
    let deg = |v: f32, lo: f32, hi: f32| lo + (v.clamp(-1.0, 1.0) + 1.0) / 2.0 * (hi - lo);
    (
        deg(x, limits.pan_min_deg, limits.pan_max_deg),
        deg(y, limits.tilt_min_deg, limits.tilt_max_deg),
    )
}

fn in_range(v: f32, axis: &str) -> Result<f32, OnvifError> {
    if v.abs() <= 1.0 + RANGE_SLACK {
        Ok(v.clamp(-1.0, 1.0))
    } else {
        Err(sender(format!("{axis} = {v} is outside [-1, 1]")))
    }
}

// ------------------------------------------------------------- requests ----

fn sender(message: impl Into<String>) -> OnvifError {
    OnvifError::SenderFault(message.into())
}

/// Client mistakes are Sender faults (400); the rest are Receiver faults.
fn backend_fault(error: anyka_ptz::Error) -> OnvifError {
    use anyka_ptz::Error as E;
    match error {
        E::InvalidArgument { .. }
        | E::UnknownPreset { .. }
        | E::TooManyPresets { .. }
        | E::Unsupported { .. } => sender(error.to_string()),
        other => OnvifError::Internal(other.to_string()),
    }
}

fn check_profile(token: &str, profile: &str) -> Result<(), OnvifError> {
    if token == profile {
        Ok(())
    } else {
        Err(sender(format!("no profile {token:?}")))
    }
}

fn expect_token(token: &str, want: &str, what: &str) -> Result<(), OnvifError> {
    if token == want {
        Ok(())
    } else {
        Err(sender(format!("no {what} {token:?}")))
    }
}

fn required<'a>(value: &'a str, field: &str) -> Result<&'a str, OnvifError> {
    Some(value.trim())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| sender(format!("missing {field}")))
}

/// The pan/tilt part of a `Velocity`/`Position`/`Translation` (`parent`),
/// which must be in `space` when it names one. `zoom` says whether the
/// request moved zoom instead, for a clearer fault.
fn pan_tilt(
    pan_tilt: Option<&tt::Vector2DType>,
    zoom: bool,
    parent: &str,
    space: &str,
) -> Result<(f32, f32), OnvifError> {
    let Some(vector) = pan_tilt else {
        return Err(if zoom {
            sender("this camera has no zoom")
        } else {
            sender(format!("missing {parent}/PanTilt"))
        });
    };
    if let Some(given) = vector.space.as_deref() {
        if given != space {
            return Err(sender(format!("unsupported space {given:?}")));
        }
    }
    // xs:float admits INF and NaN; neither is a coordinate.
    for (axis, value) in [("x", vector.x), ("y", vector.y)] {
        if !value.is_finite() {
            return Err(sender(format!("invalid {parent} {axis} {value}")));
        }
    }
    Ok((vector.x, vector.y))
}

/// An `xs:duration` without years or months (`PT1S`, `PT0.5S`, `P0DT1M`).
pub fn parse_duration(text: &str) -> Option<Duration> {
    let rest = text.trim().strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (rest, None),
    };
    let mut secs = 0f64;
    let mut any = false;
    let mut take = |part: &str, units: &[(char, f64)]| -> Option<()> {
        let mut number = String::new();
        let mut allowed = units.iter();
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                number.push(c);
                continue;
            }
            // Designators must appear in order, each at most once.
            let (_, scale) = allowed.find(|(unit, _)| *unit == c)?;
            secs += number.parse::<f64>().ok()? * scale;
            number.clear();
            any = true;
        }
        number.is_empty().then_some(())
    };
    take(date, &[('D', 86_400.0)])?;
    if let Some(time) = time {
        if time.is_empty() {
            return None;
        }
        take(time, &[('H', 3_600.0), ('M', 60.0), ('S', 1.0)])?;
    }
    (any && secs.is_finite()).then(|| Duration::from_secs_f64(secs.min(86_400.0)))
}

/// Current UTC time as `xs:dateTime`.
fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyka_ptz::Limits;

    #[test]
    fn durations() {
        let d = |s| parse_duration(s);
        assert_eq!(d("PT1S"), Some(Duration::from_secs(1)));
        assert_eq!(d("PT0.5S"), Some(Duration::from_millis(500)));
        assert_eq!(d("PT1M30S"), Some(Duration::from_secs(90)));
        assert_eq!(d("P0DT0H0M2S"), Some(Duration::from_secs(2)));
        assert_eq!(d("PT0S"), Some(Duration::ZERO));
        for bad in [
            "", "P", "PT", "1S", "PT1", "PTS1", "PT1S1M", "P1Y", "-PT1S", "PT1X",
        ] {
            assert_eq!(d(bad), None, "{bad}");
        }
    }

    #[test]
    fn coordinates_round_trip() {
        let limits = Limits::EV200;
        assert_eq!(to_normalised(&limits, 0.0, 120.0), (-1.0, 1.0));
        assert_eq!(to_normalised(&limits, 175.0, 60.0), (0.0, 0.0));
        assert_eq!(to_degrees(&limits, -1.0, 1.0), (0.0, 120.0));
        assert_eq!(to_degrees(&limits, 0.0, 0.0), (175.0, 60.0));
        assert_eq!(to_degrees(&limits, 0.5, -0.5), (262.5, 30.0));
        assert!(in_range(1.0005, "x").is_ok());
        assert!(in_range(1.1, "x").is_err());
    }
}
