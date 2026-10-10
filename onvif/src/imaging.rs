//! The ONVIF Imaging service (`ver20/imaging/wsdl`), IR-cut filter only
//! (IND-07), as a client of the lights owner (`lights-proto`).
//!
//! The `lights` service owns the IR-cut and IR LED GPIOs; this module only
//! translates. ONVIF's `IrCutFilter` names the *filter*, not the night mode:
//!
//! | ONVIF `IrCutFilter` | lights `mode` | what happens |
//! |---|---|---|
//! | `ON` (filter engaged) | `day` | filter in, IR LEDs off |
//! | `OFF` (filter removed) | `night` | filter out, IR LEDs on |
//! | `AUTO` | — | Sender fault: no light measure yet |
//!
//! The owner switches the filter and the LEDs in the safe order; nothing here
//! sequences GPIOs. GetOptions lists only `ON` and `OFF` until an automatic
//! day/night mode exists.
//!
//! Served: GetImagingSettings, SetImagingSettings, GetOptions, all for the one
//! video source ([`VIDEO_SOURCE_TOKEN`]). Every other imaging parameter
//! (brightness, exposure, focus, …) is absent from the settings and options,
//! and a SetImagingSettings that carries one is refused. Move, Stop, GetStatus
//! and GetServiceCapabilities are not registered: the library routes by
//! action name only, and PTZ, Device and Media already answer those names.
//!
//! Requests and responses are the generated `timg`/`tt` types (see
//! `soap.rs`).

use crate::media::VIDEO_SOURCE_TOKEN;
use crate::soap::{request, response};
use lights_proto::{Client, ErrorCode, Mode, Response, State};
use onvif_device_rs::server::{OnvifActionHandler, OnvifServer};
use onvif_device_rs::types::{OnvifError, RequestInfo};
use onvif_schema::xsd_parser_types::xml::AnyAttributes;
use onvif_schema::{timg, tt};
use std::time::Duration;

/// How long GetCapabilities/GetServices wait for the owner before leaving
/// Imaging out. Short: an NVR's discovery call must not hang on it.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// The served actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    GetImagingSettings,
    SetImagingSettings,
    GetOptions,
}

const ACTIONS: &[(&str, Action)] = &[
    ("GetImagingSettings", Action::GetImagingSettings),
    ("SetImagingSettings", Action::SetImagingSettings),
    ("GetOptions", Action::GetOptions),
];

/// Registers every served Imaging action on `server`.
pub fn register(server: &mut OnvifServer, lights: &Client) {
    for (name, action) in ACTIONS {
        server.register_handler(
            name,
            Box::new(ImagingAction {
                lights: lights.clone(),
                action: *action,
            }),
        );
    }
}

/// Whether the lights owner answers right now. GetCapabilities and
/// GetServices advertise Imaging only then, checked on every call so the
/// answer follows the owner's restarts and the start order does not matter.
pub async fn available(lights: &Client) -> bool {
    matches!(
        lights.clone().with_timeout(PROBE_TIMEOUT).get().await,
        Ok(Response::State { .. })
    )
}

struct ImagingAction {
    lights: Client,
    action: Action,
}

#[async_trait::async_trait]
impl OnvifActionHandler for ImagingAction {
    async fn handle(&self, body: &str, _info: &RequestInfo) -> Result<String, OnvifError> {
        respond(&self.lights, self.action, body).await
    }
}

/// Parses the request for `action`, carries it out and builds the response
/// envelope.
async fn respond(lights: &Client, action: Action, body: &str) -> Result<String, OnvifError> {
    match action {
        Action::GetImagingSettings => {
            let req: timg::GetImagingSettings = request(body)?;
            expect_source(&req.video_source_token)?;
            let state = ask(lights.get()).await?;
            let settings = timg::GetImagingSettingsResponse {
                imaging_settings: settings(ir_cut_filter(state.mode)),
            };
            response(&settings, "timg:GetImagingSettingsResponse")
        }
        Action::SetImagingSettings => {
            let req: timg::SetImagingSettings = request(body)?;
            expect_source(&req.video_source_token)?;
            // ForcePersistence is moot: the owner always persists.
            if let Some(mode) = requested_mode(&req.imaging_settings)? {
                ask(lights.set_mode(mode)).await?;
            }
            response(
                &timg::SetImagingSettingsResponse {},
                "timg:SetImagingSettingsResponse",
            )
        }
        Action::GetOptions => {
            let req: timg::GetOptions = request(body)?;
            expect_source(&req.video_source_token)?;
            response(
                &timg::GetOptionsResponse {
                    imaging_options: options(),
                },
                "timg:GetOptionsResponse",
            )
        }
    }
}

/// `ON` = IR-cut filter engaged = day; `OFF` = removed = night.
fn ir_cut_filter(mode: Mode) -> tt::IrCutFilterModeType {
    match mode {
        Mode::Day => tt::IrCutFilterModeType::On,
        Mode::Night => tt::IrCutFilterModeType::Off,
    }
}

/// The mode a SetImagingSettings asks for, `None` if it leaves the filter
/// alone. `AUTO` and any parameter other than the filter are refused.
fn requested_mode(settings: &tt::ImagingSettings20Type) -> Result<Option<Mode>, OnvifError> {
    let tt::ImagingSettings20Type {
        any_attribute: _,
        backlight_compensation,
        brightness,
        color_saturation,
        contrast,
        exposure,
        focus,
        ir_cut_filter,
        sharpness,
        wide_dynamic_range,
        white_balance,
        extension,
    } = settings;
    let others = backlight_compensation.is_some()
        || brightness.is_some()
        || color_saturation.is_some()
        || contrast.is_some()
        || exposure.is_some()
        || focus.is_some()
        || sharpness.is_some()
        || wide_dynamic_range.is_some()
        || white_balance.is_some()
        || extension.is_some();
    if others {
        return Err(OnvifError::SenderFault(
            "only IrCutFilter can be set on this camera".into(),
        ));
    }
    match ir_cut_filter {
        None => Ok(None),
        Some(tt::IrCutFilterModeType::On) => Ok(Some(Mode::Day)),
        Some(tt::IrCutFilterModeType::Off) => Ok(Some(Mode::Night)),
        Some(tt::IrCutFilterModeType::Auto) => Err(OnvifError::SenderFault(
            "IrCutFilter AUTO is not supported (see GetOptions)".into(),
        )),
    }
}

fn expect_source(token: &str) -> Result<(), OnvifError> {
    if token == VIDEO_SOURCE_TOKEN {
        Ok(())
    } else {
        Err(OnvifError::SenderFault(format!(
            "unknown video source {token:?}"
        )))
    }
}

/// One round trip to the owner. An unreachable owner is the camera's
/// problem (Receiver fault); a value it refuses is the client's.
async fn ask(
    call: impl std::future::Future<Output = Result<Response, lights_proto::ClientError>>,
) -> Result<State, OnvifError> {
    match call.await {
        Ok(Response::State { state }) => Ok(state),
        Ok(Response::Error { code, message }) => Err(match code {
            ErrorCode::BadRequest | ErrorCode::TooLong | ErrorCode::InvalidArgument => {
                OnvifError::SenderFault(message)
            }
            ErrorCode::Gpio => OnvifError::Internal(format!("lights: {message}")),
        }),
        Err(error) => {
            tracing::debug!(%error, "lights owner unreachable");
            Err(OnvifError::Internal(format!("lights unavailable: {error}")))
        }
    }
}

fn settings(ir_cut_filter: tt::IrCutFilterModeType) -> tt::ImagingSettings20Type {
    tt::ImagingSettings20Type {
        any_attribute: AnyAttributes::default(),
        backlight_compensation: None,
        brightness: None,
        color_saturation: None,
        contrast: None,
        exposure: None,
        focus: None,
        ir_cut_filter: Some(ir_cut_filter),
        sharpness: None,
        wide_dynamic_range: None,
        white_balance: None,
        extension: None,
    }
}

fn options() -> tt::ImagingOptions20Type {
    tt::ImagingOptions20Type {
        any_attribute: AnyAttributes::default(),
        backlight_compensation: None,
        brightness: None,
        color_saturation: None,
        contrast: None,
        exposure: None,
        focus: None,
        // No AUTO until there is a light measure (IND-02).
        ir_cut_filter_modes: vec![tt::IrCutFilterModeType::On, tt::IrCutFilterModeType::Off],
        sharpness: None,
        wide_dynamic_range: None,
        white_balance: None,
        extension: None,
    }
}
