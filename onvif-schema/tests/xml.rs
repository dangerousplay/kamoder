//! What the ONVIF server relies on from the generated types: responses come
//! out namespace-qualified (`trt:`/`tptz:`/`timg:`/`tt:`), and requests parse by
//! namespace whatever prefixes the client chose.

use onvif_schema::xsd_parser_types::xml::AnyAttributes;
use onvif_schema::{from_xml, timg, to_xml, tptz, trt, tt};

const TT: &str = "http://www.onvif.org/ver10/schema";
const TRT: &str = "http://www.onvif.org/ver10/media/wsdl";
const TPTZ: &str = "http://www.onvif.org/ver20/ptz/wsdl";
const TIMG: &str = "http://www.onvif.org/ver20/imaging/wsdl";

/// A main-stream H.264 profile, the shape the camera reports.
fn profile() -> tt::ProfileType {
    tt::ProfileType {
        token: "Profile_1".into(),
        fixed: Some(true),
        any_attribute: AnyAttributes::default(),
        name: "mainStream".into(),
        video_source_configuration: Some(tt::VideoSourceConfigurationType {
            token: "VideoSource_1".into(),
            view_mode: None,
            any_attribute: AnyAttributes::default(),
            name: "VideoSourceConfig".into(),
            use_count: 1,
            source_token: "VideoSource_1".into(),
            bounds: tt::IntRectangleType {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            any: vec![],
            extension: None,
        }),
        audio_source_configuration: None,
        video_encoder_configuration: Some(tt::VideoEncoderConfigurationType {
            token: "VideoEncoder_1".into(),
            guaranteed_frame_rate: None,
            any_attribute: AnyAttributes::default(),
            // `&` checks that text is escaped on the way out and back.
            name: "H264 & main".into(),
            use_count: 1,
            encoding: tt::VideoEncodingType::H264,
            resolution: tt::VideoResolutionType {
                width: 1920,
                height: 1080,
            },
            quality: 4.0,
            rate_control: Some(tt::VideoRateControlType {
                frame_rate_limit: 15,
                encoding_interval: 1,
                bitrate_limit: 2048,
            }),
            mpeg_4: None,
            h264: Some(tt::H264ConfigurationType {
                gov_length: 30,
                h264_profile: tt::H264ProfileType::Main,
            }),
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
            session_timeout: "PT60S".into(),
            any: vec![],
        }),
        audio_encoder_configuration: None,
        video_analytics_configuration: None,
        ptz_configuration: None,
        metadata_configuration: None,
        extension: None,
    }
}

#[test]
fn get_profiles_response_is_qualified_and_round_trips() {
    let response = trt::GetProfilesResponse {
        profiles: vec![profile()],
    };
    let xml = to_xml(&response, "trt:GetProfilesResponse").unwrap();

    assert!(xml.starts_with("<trt:GetProfilesResponse"), "{xml}");
    assert!(xml.contains(&format!(r#"xmlns:trt="{TRT}""#)), "{xml}");
    assert!(xml.contains(&format!(r#"xmlns:tt="{TT}""#)), "{xml}");
    assert!(
        xml.contains(r#"<trt:Profiles token="Profile_1" fixed="true">"#),
        "{xml}"
    );
    assert!(xml.contains("<tt:Name>mainStream</tt:Name>"), "{xml}");
    assert!(xml.contains("<tt:Encoding>H264</tt:Encoding>"), "{xml}");
    assert!(xml.contains("H264 &amp; main"), "{xml}");
    assert!(
        xml.contains("<tt:SessionTimeout>PT60S</tt:SessionTimeout>"),
        "{xml}"
    );

    let back: trt::GetProfilesResponse = from_xml(&xml).unwrap();
    let encoder = back.profiles[0].video_encoder_configuration.as_ref();
    assert_eq!(encoder.unwrap().name, "H264 & main");
    assert_eq!(to_xml(&back, "trt:GetProfilesResponse").unwrap(), xml);
}

#[test]
fn ptz_get_status_response_is_qualified_and_round_trips() {
    let response = tptz::GetStatusResponse {
        ptz_status: tt::PtzStatusType {
            any_attribute: AnyAttributes::default(),
            position: Some(tt::PtzVectorType {
                pan_tilt: Some(tt::Vector2DType {
                    x: 0.25,
                    y: -0.5,
                    space: Some(
                        "http://www.onvif.org/ver10/tptz/PanTiltSpaces/PositionGenericSpace".into(),
                    ),
                }),
                zoom: None,
            }),
            move_status: Some(tt::PtzMoveStatusType {
                pan_tilt: Some(tt::MoveStatusType::Idle),
                zoom: None,
            }),
            error: None,
            utc_time: "2026-10-10T00:00:00Z".into(),
            field_of_view: None,
            any: vec![],
        },
    };
    let xml = to_xml(&response, "tptz:GetStatusResponse").unwrap();

    assert!(xml.starts_with("<tptz:GetStatusResponse"), "{xml}");
    assert!(xml.contains(&format!(r#"xmlns:tptz="{TPTZ}""#)), "{xml}");
    assert!(xml.contains("<tptz:PTZStatus>"), "{xml}");
    assert!(
        xml.contains(r#"<tt:PanTilt x="0.25" y="-0.5" space="#),
        "{xml}"
    );
    // Unset optionals are omitted, not written as empty elements.
    assert!(!xml.contains("Zoom"), "{xml}");
    assert!(
        xml.contains("<tt:MoveStatus><tt:PanTilt>IDLE</tt:PanTilt></tt:MoveStatus>"),
        "{xml}"
    );
    assert!(
        xml.contains("<tt:UtcTime>2026-10-10T00:00:00Z</tt:UtcTime>"),
        "{xml}"
    );

    let back: tptz::GetStatusResponse = from_xml(&xml).unwrap();
    assert_eq!(to_xml(&back, "tptz:GetStatusResponse").unwrap(), xml);
}

/// Imaging settings that carry only the IR-cut filter, as the camera reports.
fn ir_cut_only(mode: tt::IrCutFilterModeType) -> tt::ImagingSettings20Type {
    tt::ImagingSettings20Type {
        any_attribute: AnyAttributes::default(),
        backlight_compensation: None,
        brightness: None,
        color_saturation: None,
        contrast: None,
        exposure: None,
        focus: None,
        ir_cut_filter: Some(mode),
        sharpness: None,
        wide_dynamic_range: None,
        white_balance: None,
        extension: None,
    }
}

#[test]
fn imaging_settings_response_is_qualified_and_round_trips() {
    let response = timg::GetImagingSettingsResponse {
        imaging_settings: ir_cut_only(tt::IrCutFilterModeType::On),
    };
    let xml = to_xml(&response, "timg:GetImagingSettingsResponse").unwrap();
    assert!(xml.starts_with("<timg:GetImagingSettingsResponse"), "{xml}");
    assert!(xml.contains(&format!(r#"xmlns:timg="{TIMG}""#)), "{xml}");
    assert!(
        xml.contains(
            "<timg:ImagingSettings><tt:IrCutFilter>ON</tt:IrCutFilter></timg:ImagingSettings>"
        ),
        "{xml}"
    );
    let back: timg::GetImagingSettingsResponse = from_xml(&xml).unwrap();
    assert!(matches!(
        back.imaging_settings.ir_cut_filter,
        Some(tt::IrCutFilterModeType::On)
    ));
    assert_eq!(
        to_xml(&back, "timg:GetImagingSettingsResponse").unwrap(),
        xml
    );

    let options = timg::GetOptionsResponse {
        imaging_options: tt::ImagingOptions20Type {
            any_attribute: AnyAttributes::default(),
            backlight_compensation: None,
            brightness: None,
            color_saturation: None,
            contrast: None,
            exposure: None,
            focus: None,
            ir_cut_filter_modes: vec![tt::IrCutFilterModeType::On, tt::IrCutFilterModeType::Off],
            sharpness: None,
            wide_dynamic_range: None,
            white_balance: None,
            extension: None,
        },
    };
    let xml = to_xml(&options, "timg:GetOptionsResponse").unwrap();
    assert!(
        xml.contains("<tt:IrCutFilterModes>ON</tt:IrCutFilterModes><tt:IrCutFilterModes>OFF</tt:IrCutFilterModes>"),
        "{xml}"
    );
    let back: timg::GetOptionsResponse = from_xml(&xml).unwrap();
    assert_eq!(back.imaging_options.ir_cut_filter_modes.len(), 2);
    assert_eq!(to_xml(&back, "timg:GetOptionsResponse").unwrap(), xml);
}

#[test]
fn set_imaging_settings_parses_with_any_prefix() {
    let xml = r#"<ns0:SetImagingSettings xmlns:ns0="http://www.onvif.org/ver20/imaging/wsdl"><ns0:VideoSourceToken>videoSrc0</ns0:VideoSourceToken><ns0:ImagingSettings><ns1:IrCutFilter xmlns:ns1="http://www.onvif.org/ver10/schema">AUTO</ns1:IrCutFilter></ns0:ImagingSettings><ns0:ForcePersistence>true</ns0:ForcePersistence></ns0:SetImagingSettings>"#;
    let request: timg::SetImagingSettings = from_xml(xml).unwrap();
    assert_eq!(request.video_source_token, "videoSrc0");
    assert!(matches!(
        request.imaging_settings.ir_cut_filter,
        Some(tt::IrCutFilterModeType::Auto)
    ));
    assert_eq!(request.force_persistence, Some(true));
    let bad = xml.replace("AUTO", "NIGHT");
    assert!(from_xml::<timg::SetImagingSettings>(&bad).is_err());
}

/// Every spelling below carries the same request.
fn assert_continuous_move(xml: &str) {
    let request: tptz::ContinuousMove =
        from_xml(xml).unwrap_or_else(|error| panic!("{error:?}\n{xml}"));
    assert_eq!(request.profile_token, "Profile_1");
    let pan_tilt = request.velocity.pan_tilt.as_ref().unwrap();
    assert_eq!((pan_tilt.x, pan_tilt.y), (0.5, -0.25));
    assert_eq!(request.velocity.zoom.as_ref().unwrap().x, 0.0);
    assert_eq!(request.timeout.as_deref(), Some("PT5S"));
}

#[test]
fn continuous_move_parses_with_tptz_and_tt_prefixes() {
    assert_continuous_move(
        r#"<tptz:ContinuousMove xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
  <tptz:ProfileToken>Profile_1</tptz:ProfileToken>
  <tptz:Velocity><tt:PanTilt x="0.5" y="-0.25"/><tt:Zoom x="0"/></tptz:Velocity>
  <tptz:Timeout>PT5S</tptz:Timeout></tptz:ContinuousMove>"#,
    );
}

#[test]
fn continuous_move_parses_with_default_namespaces() {
    assert_continuous_move(
        r#"<ContinuousMove xmlns="http://www.onvif.org/ver20/ptz/wsdl"><ProfileToken>Profile_1</ProfileToken>
  <Velocity><PanTilt xmlns="http://www.onvif.org/ver10/schema" x="0.5" y="-0.25"/><Zoom xmlns="http://www.onvif.org/ver10/schema" x="0"/></Velocity>
  <Timeout>PT5S</Timeout></ContinuousMove>"#,
    );
}

#[test]
fn continuous_move_parses_with_zeep_ns0_ns1_prefixes() {
    assert_continuous_move(
        r#"<ns0:ContinuousMove xmlns:ns0="http://www.onvif.org/ver20/ptz/wsdl"><ns0:ProfileToken>Profile_1</ns0:ProfileToken><ns0:Velocity><ns1:PanTilt xmlns:ns1="http://www.onvif.org/ver10/schema" x="0.5" y="-0.25"/><ns1:Zoom xmlns:ns1="http://www.onvif.org/ver10/schema" x="0"/></ns0:Velocity><ns0:Timeout>PT5S</ns0:Timeout></ns0:ContinuousMove>"#,
    );
}

#[test]
fn continuous_move_in_the_wrong_namespace_is_rejected() {
    let xml = r#"<x:ContinuousMove xmlns:x="urn:wrong"><x:ProfileToken>P</x:ProfileToken></x:ContinuousMove>"#;
    assert!(from_xml::<tptz::ContinuousMove>(xml).is_err());
}

/// onvif-device-rs 0.8 hands over the Body child without the `xmlns` that a
/// client declared on `<s:Envelope>`. Such a body cannot be parsed, which is
/// why the transport has to re-emit the in-scope declarations (design T15).
#[test]
fn undeclared_prefixes_are_rejected() {
    let xml = r#"<tptz:ContinuousMove><tptz:ProfileToken>Profile_1</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x="0.5" y="-0.25"/></tptz:Velocity></tptz:ContinuousMove>"#;
    assert!(from_xml::<tptz::ContinuousMove>(xml).is_err());
}
