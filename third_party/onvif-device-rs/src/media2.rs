//! ONVIF Media2 service (ver20/media, `tr2`) — the Profile-T entry path
//! (issue #53).
//!
//! Mirrors onvif-go's `SupportMedia2` + `server/media2.go` (the golden
//! wire source): with the service enabled the SOAP server routes
//! `/onvif/media2_service` on the listener it already owns, with its own
//! action dispatch — the Media2 action local names (`GetProfiles`,
//! `GetStreamUri`, `SetSynchronizationPoint`, …) collide with Media1 in
//! the shared action map, so the URL decides which face answers, exactly
//! like the events service's dedicated endpoint.
//!
//! Both faces read the SAME [`SharedMediaConfig`] store: a
//! SetVideoEncoderConfiguration through Media2 is immediately visible to
//! the Media1 readers and vice versa.
//!
//! ## Wire rules (the media2.wsdl ground truth, mirrored by onvif-go's
//! nsMedia2* decoder test)
//!
//! - `GetProfilesResponse`/`Profiles` and the `Configurations` wrapper
//!   with its children (`VideoSource`, `VideoEncoder`) are tr2-local
//!   elements;
//! - the configuration bodies (`Name`, `UseCount`, `SourceToken`,
//!   `Encoding`, `Resolution`, `RateControl`) resolve to ver10/schema
//!   (`tt:`);
//! - `GetStreamUriResponse`/`GetSnapshotUriResponse` carry a plain
//!   `tr2:Uri` (no MediaUri wrapper);
//! - `SetSynchronizationPointResponse` and
//!   `SetVideoEncoderConfigurationResponse` are empty acks;
//! - `GetVideoEncoderInstancesResponse`'s `Info`/`Codec`/`Encoding`/
//!   `Number`/`Total` are tr2-local (locally declared in the media2
//!   WSDL, `elementFormDefault="qualified"`).
//!
//! Deliberately NOT implemented: profile mutation (CreateProfile /
//! AddConfiguration / RemoveConfiguration / DeleteProfile — the store's
//! profile set is host-owned) and multicast streaming (the device serves
//! none; `RTPMulticast="false"` is advertised). Unknown actions on the
//! path are Sender faults.

use std::sync::{Arc, RwLock};

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};

use crate::media::{OnvifMediaConfig, SharedMediaConfig, VideoEncoding};
use crate::namespaces::SCHEMAS;
use crate::types::{resolve_server_ip, serialize_soap_response, OnvifError, TextAccumulator};

/// ONVIF Media2 Service WSDL namespace (`tr2`).
pub const MEDIA2_SERVICE: &str = "http://www.onvif.org/ver20/media/wsdl";

/// HTTP path of the Media2 service endpoint on the SOAP server's
/// listener (parity with onvif-go's `media2_service` route).
pub const MEDIA2_SERVICE_PATH: &str = "/onvif/media2_service";

// ---------------------------------------------------------------------------
// Small quick-xml helpers (crate writer style; see media.rs / events.rs)
// ---------------------------------------------------------------------------

/// Read-guard the shared config, tolerating lock poisoning (the
/// `media.rs` / `ptz_state` pattern).
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

/// Write `<name>escaped text</name>` at the current indent.
fn write_text(w: &mut Writer<Vec<u8>>, name: &str, text: &str) {
    w.write_event(Event::Start(BytesStart::new(name)))
        .unwrap_or_default();
    w.write_event(Event::Text(BytesText::from_escaped(
        crate::types::xml_escape(text),
    )))
    .unwrap_or_default();
    w.write_event(Event::End(BytesEnd::new(name)))
        .unwrap_or_default();
}

/// Open an element, call `f` to write children, close it.
fn open_close<F>(w: &mut Writer<Vec<u8>>, name: &str, f: F)
where
    F: FnOnce(&mut Writer<Vec<u8>>),
{
    w.write_event(Event::Start(BytesStart::new(name)))
        .unwrap_or_default();
    f(w);
    w.write_event(Event::End(BytesEnd::new(name)))
        .unwrap_or_default();
}

/// The response root with the `tr2` (and, when tt children follow,
/// `tt`) namespace declarations. `name` is always a `'static` literal
/// (the builder call sites), matching quick-xml's `Cow<'static>`
/// element-name convention used across the crate.
fn tr2_root(name: &'static str, with_tt: bool) -> BytesStart<'static> {
    let mut root = BytesStart::new(name);
    root.push_attribute(("xmlns:tr2", MEDIA2_SERVICE));
    if with_tt {
        root.push_attribute(("xmlns:tt", SCHEMAS));
    }
    root
}

// ---------------------------------------------------------------------------
// Advertised entries (snapshots of the shared store)
// ---------------------------------------------------------------------------

/// Geometry + tokens of one encoder configuration in the tr2 wire form.
struct EncoderEntry<'a> {
    token: &'a str,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    encoding: VideoEncoding,
}

/// The encoder configurations the profiles advertise — the primary flat
/// config first, then the extra profiles, in advertisement order (the
/// same order the Media1 face lists).
fn encoder_entries(config: &OnvifMediaConfig) -> Vec<EncoderEntry<'_>> {
    let mut out = vec![EncoderEntry {
        token: &config.encoder_token,
        width: config.camera_width,
        height: config.camera_height,
        fps: config.camera_fps,
        bitrate: config.camera_bitrate,
        encoding: config.encoding,
    }];
    for extra in &config.extra_profiles {
        out.push(EncoderEntry {
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

/// One advertised profile: its token/name plus its encoder entry.
struct ProfileEntry<'a> {
    token: &'a str,
    name: &'a str,
    encoder: EncoderEntry<'a>,
}

/// The profiles GetProfiles advertises — primary first (Profile S / T
/// clients that pick "the first profile" keep getting the primary
/// stream), extras after.
fn profile_entries(config: &OnvifMediaConfig) -> Vec<ProfileEntry<'_>> {
    let mut out = vec![ProfileEntry {
        token: &config.profile_token,
        name: &config.profile_token,
        encoder: EncoderEntry {
            token: &config.encoder_token,
            width: config.camera_width,
            height: config.camera_height,
            fps: config.camera_fps,
            bitrate: config.camera_bitrate,
            encoding: config.encoding,
        },
    }];
    for extra in &config.extra_profiles {
        out.push(ProfileEntry {
            token: &extra.token,
            name: &extra.name,
            encoder: EncoderEntry {
                token: &extra.encoder_token,
                width: extra.width,
                height: extra.height,
                fps: extra.fps,
                bitrate: extra.bitrate,
                encoding: extra.encoding,
            },
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Response builders (pure — byte-golden test targets)
// ---------------------------------------------------------------------------

/// The tr2 video-encoder configuration block: `<{element} token="…">`
/// with the tt configuration body. Shared by GetProfiles (as
/// `tr2:VideoEncoder`) and the standalone encoder listings (as
/// `tr2:Configurations` / `tr2:Configuration`), so their fields cannot
/// drift (the media.rs `write_encoder_block` discipline).
///
/// The `GovLength`/`AnchorFrameDistance` attributes of the WSDL's
/// `tt:VideoEncoder2Configuration` are optional and omitted — the store
/// carries no GOP geometry to honestly report.
fn write_tr2_encoder_block(w: &mut Writer<Vec<u8>>, element_name: &str, entry: &EncoderEntry<'_>) {
    let mut el = BytesStart::new(element_name);
    el.push_attribute(("token", entry.token));
    w.write_event(Event::Start(el)).unwrap_or_default();
    write_text(w, "tt:Name", "VideoEncoderConfig");
    write_text(w, "tt:UseCount", "1");
    write_text(w, "tt:Encoding", entry.encoding.as_str());
    open_close(w, "tt:Resolution", |w| {
        write_text(w, "tt:Width", &entry.width.to_string());
        write_text(w, "tt:Height", &entry.height.to_string());
    });
    open_close(w, "tt:RateControl", |w| {
        write_text(w, "tt:FrameRateLimit", &entry.fps.to_string());
        write_text(w, "tt:BitrateLimit", &entry.bitrate.to_string());
    });
    w.write_event(Event::End(BytesEnd::new(element_name)))
        .unwrap_or_default();
}

/// `<tr2:GetProfilesResponse>` for the given entries.
fn build_get_profiles(
    profiles: &[ProfileEntry<'_>],
    video_source_token: &str,
    video_source_name: &str,
) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    w.write_event(Event::Start(tr2_root("tr2:GetProfilesResponse", true)))
        .unwrap_or_default();

    for p in profiles {
        let mut profiles_el = BytesStart::new("tr2:Profiles");
        profiles_el.push_attribute(("token", p.token));
        profiles_el.push_attribute(("fixed", "true"));
        w.write_event(Event::Start(profiles_el)).unwrap_or_default();

        write_text(&mut w, "tt:Name", p.name);

        open_close(&mut w, "tr2:Configurations", |w| {
            let mut vs = BytesStart::new("tr2:VideoSource");
            vs.push_attribute(("token", video_source_token));
            w.write_event(Event::Start(vs)).unwrap_or_default();
            write_text(w, "tt:Name", video_source_name);
            write_text(w, "tt:UseCount", "1");
            write_text(w, "tt:SourceToken", video_source_token);
            w.write_event(Event::End(BytesEnd::new("tr2:VideoSource")))
                .unwrap_or_default();

            write_tr2_encoder_block(w, "tr2:VideoEncoder", &p.encoder);
        });

        w.write_event(Event::End(BytesEnd::new("tr2:Profiles")))
            .unwrap_or_default();
    }

    w.write_event(Event::End(BytesEnd::new("tr2:GetProfilesResponse")))
        .unwrap_or_default();
    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `<tr2:GetStreamUriResponse>` / `<tr2:GetSnapshotUriResponse>` — the
/// plain `tr2:Uri` form (no MediaUri wrapper).
fn build_uri_response(root_name: &'static str, uri: &str) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    w.write_event(Event::Start(tr2_root(root_name, false)))
        .unwrap_or_default();
    write_text(&mut w, "tr2:Uri", uri);
    w.write_event(Event::End(BytesEnd::new(root_name)))
        .unwrap_or_default();
    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `<tr2:GetVideoEncoderConfigurationsResponse>` — one
/// `tr2:Configurations` block per advertised entry.
fn build_encoder_list(entries: &[EncoderEntry<'_>]) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    w.write_event(Event::Start(tr2_root(
        "tr2:GetVideoEncoderConfigurationsResponse",
        true,
    )))
    .unwrap_or_default();
    for entry in entries {
        write_tr2_encoder_block(&mut w, "tr2:Configurations", entry);
    }
    w.write_event(Event::End(BytesEnd::new(
        "tr2:GetVideoEncoderConfigurationsResponse",
    )))
    .unwrap_or_default();
    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `<tr2:GetVideoEncoderConfigurationResponse>` — the single
/// `tr2:Configuration` block.
fn build_encoder_single(entry: &EncoderEntry<'_>) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    w.write_event(Event::Start(tr2_root(
        "tr2:GetVideoEncoderConfigurationResponse",
        true,
    )))
    .unwrap_or_default();
    write_tr2_encoder_block(&mut w, "tr2:Configuration", entry);
    w.write_event(Event::End(BytesEnd::new(
        "tr2:GetVideoEncoderConfigurationResponse",
    )))
    .unwrap_or_default();
    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `<tr2:GetVideoEncoderConfigurationOptionsResponse>` — the minimal
/// honest options set: every advertised resolution (child-element
/// `tt:VideoResolution` form, unlike Media1's attribute form) plus the
/// frame-rate range derived from the primary config's fps.
fn build_encoder_options(entries: &[EncoderEntry<'_>], primary_fps: u32) -> String {
    let fps = primary_fps.max(1);
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    w.write_event(Event::Start(tr2_root(
        "tr2:GetVideoEncoderConfigurationOptionsResponse",
        true,
    )))
    .unwrap_or_default();
    open_close(&mut w, "tr2:Options", |w| {
        for entry in entries {
            open_close(w, "tt:ResolutionsAvailable", |w| {
                write_text(w, "tt:Width", &entry.width.to_string());
                write_text(w, "tt:Height", &entry.height.to_string());
            });
        }
        open_close(w, "tt:FrameRateRange", |w| {
            write_text(w, "tt:Min", "1");
            write_text(w, "tt:Max", &fps.to_string());
        });
    });
    w.write_event(Event::End(BytesEnd::new(
        "tr2:GetVideoEncoderConfigurationOptionsResponse",
    )))
    .unwrap_or_default();
    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `<tr2:GetVideoEncoderInstancesResponse>` — the WSDL's
/// `Info{Codec[]{Encoding,Number}, Total}` with one codec entry (the
/// primary encoding, one guaranteed instance) and Total 1.
fn build_encoder_instances(encoding: VideoEncoding) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    w.write_event(Event::Start(tr2_root(
        "tr2:GetVideoEncoderInstancesResponse",
        false,
    )))
    .unwrap_or_default();
    open_close(&mut w, "tr2:Info", |w| {
        open_close(w, "tr2:Codec", |w| {
            write_text(w, "tr2:Encoding", encoding.as_str());
            write_text(w, "tr2:Number", "1");
        });
        write_text(w, "tr2:Total", "1");
    });
    w.write_event(Event::End(BytesEnd::new(
        "tr2:GetVideoEncoderInstancesResponse",
    )))
    .unwrap_or_default();
    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `<tr2:{root_name}/>` — the empty ack shape shared by
/// SetVideoEncoderConfiguration and SetSynchronizationPoint.
fn build_empty_ack(root_name: &'static str) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    w.write_event(Event::Empty(tr2_root(root_name, false)))
        .unwrap_or_default();
    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// The Media2 GetServiceCapabilities: `tr2:Capabilities` with the
/// `SnapshotUri` attribute following `snapshot_port`, the advertised
/// profile count, and honest streaming flags (RTSP unicast + RTP over
/// RTSP/TCP served; no multicast, no non-aggregate control).
fn build_capabilities(snapshot_uri: bool, max_profiles: usize) -> String {
    let max_profiles = max_profiles.to_string();
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    w.write_event(Event::Start(tr2_root(
        "tr2:GetServiceCapabilitiesResponse",
        false,
    )))
    .unwrap_or_default();

    let mut caps = BytesStart::new("tr2:Capabilities");
    caps.push_attribute(("SnapshotUri", if snapshot_uri { "true" } else { "false" }));
    caps.push_attribute(("Rotation", "false"));
    caps.push_attribute(("VideoSourceMode", "false"));
    caps.push_attribute(("OSD", "false"));
    caps.push_attribute(("TemporaryOSDText", "false"));
    w.write_event(Event::Start(caps)).unwrap_or_default();

    let mut profile_caps = BytesStart::new("tr2:ProfileCapabilities");
    profile_caps.push_attribute(("MaximumNumberOfProfiles", max_profiles.as_str()));
    w.write_event(Event::Empty(profile_caps))
        .unwrap_or_default();

    let mut streaming = BytesStart::new("tr2:StreamingCapabilities");
    streaming.push_attribute(("RTSPStreaming", "true"));
    streaming.push_attribute(("RTPMulticast", "false"));
    streaming.push_attribute(("RTP_RTSP_TCP", "true"));
    streaming.push_attribute(("NonAggregateControl", "false"));
    w.write_event(Event::Empty(streaming)).unwrap_or_default();

    w.write_event(Event::End(BytesEnd::new("tr2:Capabilities")))
        .unwrap_or_default();
    w.write_event(Event::End(BytesEnd::new(
        "tr2:GetServiceCapabilitiesResponse",
    )))
    .unwrap_or_default();
    String::from_utf8(w.into_inner()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tolerant request parsing (namespace-agnostic, never panics)
// ---------------------------------------------------------------------------

/// Extract the text of the first element whose local name is `local`
/// (the media.rs tolerance contract: prefixes, attributes on the open
/// tag, surrounding whitespace, and malformed input are all accepted —
/// anything unparseable yields `None`).
fn parse_element_text<'a>(body: &'a str, local_target: &str) -> Option<&'a str> {
    let mut rest = body;
    while let Some(open) = rest.find('<') {
        let after_open = &rest[open + 1..];
        let Some(tag_end) = after_open.find(|c: char| c == '>' || c.is_whitespace()) else {
            break;
        };
        let tag = &after_open[..tag_end];
        let local = tag.rsplit(':').next().unwrap_or(tag);
        let Some(greater) = after_open.find('>') else {
            break;
        };
        if after_open[..greater].ends_with('/') {
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

/// Values parsed from a Media2 SetVideoEncoderConfiguration request
/// (namespace-agnostic local names; absent fields stay `None` so the
/// stored value is left unchanged — the Media1 partial-update
/// semantics). The tr2 form carries no EncodingInterval.
#[derive(Debug, Default)]
struct ParsedEncoderConfig {
    token: Option<String>,
    encoding: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<u32>,
    bitrate: Option<u32>,
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
        _ => {}
    }
    Ok(())
}

/// Parse a Media2 SetVideoEncoderConfiguration body: the `token`
/// attribute of the `Configuration` element plus its tt children
/// (Encoding / Resolution / RateControl). The GovLength /
/// AnchorFrameDistance attributes are accepted and ignored (this device
/// reports none). Malformed input is a Sender fault, never a panic.
fn parse_tr2_encoder_configuration(body: &str) -> Result<ParsedEncoderConfig, OnvifError> {
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
                        let name_bytes = e.name().as_ref().to_owned();
                        let local = local_name(name_bytes.as_slice());
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
                        if local_name(e.name().as_ref()) == "Configuration"
                            && result.token.is_none()
                        {
                            result.token = attr_local_value(&e, "token");
                        }
                    }
                    Ok(Event::End(e)) => {
                        if local_name(e.name().as_ref()) == "Configuration" {
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

/// Whether the requested streaming protocol is one this device can
/// serve — the tr2:TransportProtocol family is rtsp-flavored, matched
/// tolerantly (case-insensitive substring); an absent Protocol assumes
/// RTSP. Anything else is a Sender fault instead of a silently-wrong
/// URI.
fn protocol_supported(protocol: Option<&str>) -> Result<(), OnvifError> {
    match protocol {
        None | Some("") => Ok(()),
        Some(p) if p.to_ascii_lowercase().contains("rtsp") => Ok(()),
        Some(p) => Err(OnvifError::SenderFault(format!(
            "unsupported streaming protocol: {p} (device serves RTSP only)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Media2Service — the routed service
// ---------------------------------------------------------------------------

/// The Media2 service: the action dispatch behind
/// [`crate::server::OnvifServer::enable_media2`]. Reads (and the one
/// write) go through the shared [`SharedMediaConfig`] store, so both
/// media faces observe the same configuration.
pub struct Media2Service {
    config: SharedMediaConfig,
    /// Host keyframe seam — fired on every SetSynchronizationPoint (the
    /// same hook type [`crate::media::register_media_actions`] takes).
    keyframe_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Media2Service {
    /// A fresh service over `config` with an optional keyframe hook.
    #[must_use]
    pub fn new(
        config: SharedMediaConfig,
        keyframe_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self {
            config,
            keyframe_hook,
        }
    }

    /// Whether `action` belongs to the Media2 endpoint.
    #[must_use]
    pub(crate) fn is_service_action(action: &str) -> bool {
        matches!(
            action,
            "GetProfiles"
                | "GetStreamUri"
                | "GetSnapshotUri"
                | "GetVideoEncoderConfigurations"
                | "GetVideoEncoderConfiguration"
                | "GetVideoEncoderConfigurationOptions"
                | "SetVideoEncoderConfiguration"
                | "GetVideoEncoderInstances"
                | "SetSynchronizationPoint"
                | "GetServiceCapabilities"
        )
    }

    /// Auth policy for Media2 actions (onvif-go's `DefaultAuthPolicy`:
    /// write-style prefixes are protected, reads stay open):
    /// SetSynchronizationPoint / SetVideoEncoderConfiguration /
    /// CreateProfile-style actions require WS-Security credentials.
    #[must_use]
    pub(crate) fn action_requires_auth(action: &str) -> bool {
        ["Set", "Remove", "Create", "Go", "Delete"]
            .iter()
            .any(|p| action.starts_with(p))
    }

    /// Dispatch a Media2 endpoint action (the router pre-checked
    /// membership). `server_ip` is the local address that received the
    /// connection — the URIs are built against it so they are reachable
    /// from the caller.
    pub(crate) async fn handle_action(
        &self,
        action: &str,
        body: &str,
        server_ip: &str,
    ) -> Result<String, OnvifError> {
        let fragment = match action {
            "GetProfiles" => {
                let config = read_config(&self.config);
                let token_filter = parse_element_text(body, "Token");
                let profiles: Vec<ProfileEntry<'_>> = profile_entries(&config)
                    .into_iter()
                    .filter(|p| token_filter.map_or(true, |t| t == p.token))
                    .collect();
                build_get_profiles(
                    &profiles,
                    &config.video_source_token,
                    &config.video_source_name,
                )
            }
            "GetStreamUri" => {
                protocol_supported(parse_element_text(body, "Protocol"))?;
                let config = read_config(&self.config);
                let uri = stream_uri(&config, body, server_ip);
                build_uri_response("tr2:GetStreamUriResponse", &uri)
            }
            "GetSnapshotUri" => {
                let config = read_config(&self.config);
                let uri = snapshot_uri(&config, server_ip)?;
                build_uri_response("tr2:GetSnapshotUriResponse", &uri)
            }
            "GetVideoEncoderConfigurations" => {
                let config = read_config(&self.config);
                let token_filter = parse_element_text(body, "ConfigurationToken");
                let entries: Vec<EncoderEntry<'_>> = encoder_entries(&config)
                    .into_iter()
                    .filter(|e| token_filter.map_or(true, |t| t == e.token))
                    .collect();
                build_encoder_list(&entries)
            }
            "GetVideoEncoderConfiguration" => {
                let token = parse_element_text(body, "ConfigurationToken").ok_or_else(|| {
                    OnvifError::SenderFault(
                        "configuration not found: no ConfigurationToken in request".to_string(),
                    )
                })?;
                let config = read_config(&self.config);
                let entry = encoder_entries(&config)
                    .into_iter()
                    .find(|e| e.token == token)
                    .ok_or_else(|| {
                        OnvifError::SenderFault(format!("configuration not found: {token}"))
                    })?;
                build_encoder_single(&entry)
            }
            "GetVideoEncoderConfigurationOptions" => {
                let config = read_config(&self.config);
                build_encoder_options(&encoder_entries(&config), config.camera_fps)
            }
            "SetVideoEncoderConfiguration" => {
                self.set_video_encoder_configuration(body)?;
                build_empty_ack("tr2:SetVideoEncoderConfigurationResponse")
            }
            "GetVideoEncoderInstances" => {
                let config = read_config(&self.config);
                build_encoder_instances(config.encoding)
            }
            "SetSynchronizationPoint" => {
                if let Some(hook) = &self.keyframe_hook {
                    hook();
                }
                build_empty_ack("tr2:SetSynchronizationPointResponse")
            }
            "GetServiceCapabilities" => {
                let config = read_config(&self.config);
                build_capabilities(config.snapshot_port != 0, 1 + config.extra_profiles.len())
            }
            _ => {
                return Err(OnvifError::SenderFault(format!(
                    "unsupported action: {action}"
                )))
            }
        };
        Ok(serialize_soap_response(&fragment))
    }

    /// SetVideoEncoderConfiguration: apply the client's partial changes
    /// to the shared store (absent fields unchanged; the token must name
    /// the primary or an extra profile's encoder configuration; JPEG
    /// and unknown encodings fault instead of being silently ignored —
    /// the Media1 semantics).
    fn set_video_encoder_configuration(&self, body: &str) -> Result<(), OnvifError> {
        let parsed = parse_tr2_encoder_configuration(body)?;
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
        Ok(())
    }
}

/// The RTSP URI for the request's ProfileToken: a matching extra
/// profile advertises its own stream path; unknown or missing tokens
/// fail open to the primary stream (the Media1 semantics — legacy
/// clients that never send a token keep the historical URI).
fn stream_uri(config: &OnvifMediaConfig, body: &str, server_ip: &str) -> String {
    let ip = resolve_server_ip(server_ip, &config.device_ip);
    let stream_path = parse_element_text(body, "ProfileToken")
        .and_then(|token| {
            config
                .extra_profiles
                .iter()
                .find(|p| p.token == token)
                .map(|p| p.stream_path.as_str())
        })
        .unwrap_or(&config.stream_path);
    format!("rtsp://{}:{}{}", ip, config.rtsp_port, stream_path)
}

/// The HTTP snapshot URI, honoring `snapshot_port` (0 = the feature is
/// off → the Media1 `ErrSnapshotNotSupported` fault).
fn snapshot_uri(config: &OnvifMediaConfig, server_ip: &str) -> Result<String, OnvifError> {
    if config.snapshot_port == 0 {
        return Err(OnvifError::SenderFault(
            "snapshot not supported (OnvifMediaConfig::snapshot_port is unset)".to_string(),
        ));
    }
    let ip = resolve_server_ip(server_ip, &config.device_ip);
    Ok(format!(
        "http://{}:{}{}",
        ip, config.snapshot_port, config.snapshot_path
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn multi_store() -> SharedMediaConfig {
        let mut cfg = OnvifMediaConfig::new(1920, 1080, 30, 4_000_000, 8554, "10.1.1.100".into());
        cfg.snapshot_port = 8088;
        cfg.extra_profiles = vec![crate::media::MediaProfileConfig {
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

    fn plain_store() -> SharedMediaConfig {
        Arc::new(RwLock::new(OnvifMediaConfig::new(
            1280,
            720,
            25,
            2_000_000,
            8554,
            "10.1.1.100".into(),
        )))
    }

    fn req_ip() -> String {
        "10.1.1.5".to_string()
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

    // -- action tables ----------------------------------------------------

    #[test]
    fn service_action_table() {
        for action in [
            "GetProfiles",
            "GetStreamUri",
            "GetSnapshotUri",
            "GetVideoEncoderConfigurations",
            "GetVideoEncoderConfiguration",
            "GetVideoEncoderConfigurationOptions",
            "SetVideoEncoderConfiguration",
            "GetVideoEncoderInstances",
            "SetSynchronizationPoint",
            "GetServiceCapabilities",
        ] {
            assert!(Media2Service::is_service_action(action), "{action}");
        }
        // Media1-only and unknown actions do not belong to the path.
        for action in [
            "GetVideoSources",
            "GetAudioSources",
            "CreateProfile",
            "DeleteProfile",
            "StartMulticastStreaming",
            "",
        ] {
            assert!(!Media2Service::is_service_action(action), "{action}");
        }
    }

    #[test]
    fn auth_policy_table() {
        assert!(Media2Service::action_requires_auth(
            "SetSynchronizationPoint"
        ));
        assert!(Media2Service::action_requires_auth(
            "SetVideoEncoderConfiguration"
        ));
        assert!(!Media2Service::action_requires_auth("GetProfiles"));
        assert!(!Media2Service::action_requires_auth(
            "GetVideoEncoderConfiguration"
        ));
    }

    // -- byte goldens (fragment builders) ---------------------------------

    #[test]
    fn golden_get_profiles_fragment() {
        let store = multi_store();
        let config = read_config(&store);
        let fragment = build_get_profiles(
            &profile_entries(&config),
            &config.video_source_token,
            &config.video_source_name,
        );
        assert_eq!(
            fragment,
            "<tr2:GetProfilesResponse xmlns:tr2=\"http://www.onvif.org/ver20/media/wsdl\" xmlns:tt=\"http://www.onvif.org/ver10/schema\">\n  \
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
             </tr2:GetProfilesResponse>"
        );
    }

    #[test]
    fn golden_uri_fragments() {
        assert_eq!(
            build_uri_response("tr2:GetStreamUriResponse", "rtsp://10.1.1.5:8554/stream"),
            "<tr2:GetStreamUriResponse xmlns:tr2=\"http://www.onvif.org/ver20/media/wsdl\">\n  \
             <tr2:Uri>rtsp://10.1.1.5:8554/stream</tr2:Uri>\n\
             </tr2:GetStreamUriResponse>"
        );
        assert_eq!(
            build_uri_response(
                "tr2:GetSnapshotUriResponse",
                "http://10.1.1.5:8088/snapshot.jpg"
            ),
            "<tr2:GetSnapshotUriResponse xmlns:tr2=\"http://www.onvif.org/ver20/media/wsdl\">\n  \
             <tr2:Uri>http://10.1.1.5:8088/snapshot.jpg</tr2:Uri>\n\
             </tr2:GetSnapshotUriResponse>"
        );
    }

    #[test]
    fn golden_encoder_list_single_and_options() {
        let store = multi_store();
        let config = read_config(&store);
        let entries = encoder_entries(&config);

        let list = build_encoder_list(&entries);
        assert_well_formed(&list, "encoder list");
        assert_eq!(list.matches("<tr2:Configurations ").count(), 2);
        assert!(list.contains(r#"<tr2:Configurations token="enc0">"#));
        assert!(list.contains(r#"<tr2:Configurations token="sub_encoder">"#));
        assert!(list.contains("<tt:Encoding>H264</tt:Encoding>"));
        assert!(!list.contains("EncodingInterval"));
        assert!(!list.contains("GovLength"));

        let single = build_encoder_single(&entries[1]);
        assert_well_formed(&single, "encoder single");
        assert!(single.contains("<tr2:GetVideoEncoderConfigurationResponse"));
        assert_eq!(single.matches("<tr2:Configuration ").count(), 1);
        assert!(single.contains(r#"<tr2:Configuration token="sub_encoder">"#));

        let options = build_encoder_options(&entries, config.camera_fps);
        assert_well_formed(&options, "encoder options");
        assert_eq!(
            options,
            "<tr2:GetVideoEncoderConfigurationOptionsResponse xmlns:tr2=\"http://www.onvif.org/ver20/media/wsdl\" xmlns:tt=\"http://www.onvif.org/ver10/schema\">\n  \
             <tr2:Options>\n    \
             <tt:ResolutionsAvailable>\n      \
             <tt:Width>1920</tt:Width>\n      \
             <tt:Height>1080</tt:Height>\n    \
             </tt:ResolutionsAvailable>\n    \
             <tt:ResolutionsAvailable>\n      \
             <tt:Width>640</tt:Width>\n      \
             <tt:Height>360</tt:Height>\n    \
             </tt:ResolutionsAvailable>\n    \
             <tt:FrameRateRange>\n      \
             <tt:Min>1</tt:Min>\n      \
             <tt:Max>30</tt:Max>\n    \
             </tt:FrameRateRange>\n  \
             </tr2:Options>\n\
             </tr2:GetVideoEncoderConfigurationOptionsResponse>"
        );
    }

    #[test]
    fn golden_instances_caps_and_acks() {
        assert_eq!(
            build_encoder_instances(VideoEncoding::H265),
            "<tr2:GetVideoEncoderInstancesResponse xmlns:tr2=\"http://www.onvif.org/ver20/media/wsdl\">\n  \
             <tr2:Info>\n    \
             <tr2:Codec>\n      \
             <tr2:Encoding>H265</tr2:Encoding>\n      \
             <tr2:Number>1</tr2:Number>\n    \
             </tr2:Codec>\n    \
             <tr2:Total>1</tr2:Total>\n  \
             </tr2:Info>\n\
             </tr2:GetVideoEncoderInstancesResponse>"
        );

        assert_eq!(
            build_capabilities(true, 2),
            "<tr2:GetServiceCapabilitiesResponse xmlns:tr2=\"http://www.onvif.org/ver20/media/wsdl\">\n  \
             <tr2:Capabilities SnapshotUri=\"true\" Rotation=\"false\" VideoSourceMode=\"false\" OSD=\"false\" TemporaryOSDText=\"false\">\n    \
             <tr2:ProfileCapabilities MaximumNumberOfProfiles=\"2\"/>\n    \
             <tr2:StreamingCapabilities RTSPStreaming=\"true\" RTPMulticast=\"false\" RTP_RTSP_TCP=\"true\" NonAggregateControl=\"false\"/>\n  \
             </tr2:Capabilities>\n\
             </tr2:GetServiceCapabilitiesResponse>"
        );

        assert_eq!(
            build_empty_ack("tr2:SetSynchronizationPointResponse"),
            "<tr2:SetSynchronizationPointResponse xmlns:tr2=\"http://www.onvif.org/ver20/media/wsdl\"/>"
        );
    }

    // -- handler semantics -------------------------------------------------

    #[tokio::test]
    async fn get_profiles_token_filter_semantics() {
        let svc = Media2Service::new(multi_store(), None);
        let by_sub = "<GetProfiles><Token>sub</Token></GetProfiles>";
        let out = svc.handle_action("GetProfiles", by_sub, &req_ip()).await;
        assert!(out.is_ok());
        let xml = out.unwrap_or_default();
        assert_eq!(xml.matches("<tr2:Profiles ").count(), 1);
        assert!(xml.contains(r#"token="sub""#));

        let unknown = "<GetProfiles><Token>nope</Token></GetProfiles>";
        let xml = svc
            .handle_action("GetProfiles", unknown, &req_ip())
            .await
            .unwrap_or_default();
        assert_eq!(xml.matches("<tr2:Profiles ").count(), 0);
        assert!(xml.contains("GetProfilesResponse"));
    }

    #[tokio::test]
    async fn stream_uri_and_protocol_tolerances() {
        let svc = Media2Service::new(multi_store(), None);
        let ip = req_ip();

        let body = "<GetStreamUri><Protocol>RTSP</Protocol>\
                    <ProfileToken>sub</ProfileToken></GetStreamUri>";
        let xml = svc
            .handle_action("GetStreamUri", body, &ip)
            .await
            .unwrap_or_default();
        assert!(xml.contains("rtsp://10.1.1.5:8554/sub"), "{xml}");

        // Per-request loopback IP falls back to the configured device IP.
        let xml = svc
            .handle_action("GetStreamUri", body, "127.0.0.1")
            .await
            .unwrap_or_default();
        assert!(xml.contains("rtsp://10.1.1.100:8554/sub"), "{xml}");

        // rtsp-family spellings tolerated; missing protocol assumed rtsp.
        for protocol in ["RtspUnicast", "RtspMulticast", "rtsps_unicast", "RTSP"] {
            let body = format!(
                "<GetStreamUri><Protocol>{protocol}</Protocol>\
                 <ProfileToken>main</ProfileToken></GetStreamUri>"
            );
            let result = svc.handle_action("GetStreamUri", &body, &ip).await;
            assert!(result.is_ok(), "protocol {protocol} must be accepted");
        }
        let xml = svc
            .handle_action(
                "GetStreamUri",
                "<GetStreamUri><ProfileToken>main</ProfileToken></GetStreamUri>",
                &ip,
            )
            .await
            .unwrap_or_default();
        assert!(xml.contains("rtsp://10.1.1.5:8554/stream"));

        // Non-rtsp protocol → Sender fault naming the protocol.
        let err = svc
            .handle_action(
                "GetStreamUri",
                "<GetStreamUri><Protocol>http</Protocol></GetStreamUri>",
                &ip,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, OnvifError::SenderFault(ref m) if m.contains("protocol")),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn snapshot_uri_disabled_faults() {
        let store = plain_store(); // snapshot_port = 0 by default
        let svc = Media2Service::new(Arc::clone(&store), None);
        let err = svc
            .handle_action("GetSnapshotUri", "", &req_ip())
            .await
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("not supported"),
            "got: {err}"
        );

        store
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .snapshot_port = 8088;
        let xml = svc
            .handle_action("GetSnapshotUri", "", &req_ip())
            .await
            .unwrap_or_default();
        assert!(xml.contains("http://10.1.1.5:8088/snapshot.jpg"), "{xml}");
    }

    #[tokio::test]
    async fn set_video_encoder_configuration_partial_and_faults() {
        let store = multi_store();
        let svc = Media2Service::new(Arc::clone(&store), None);

        // Partial update: only the resolution; the rest unchanged.
        let body = "<SetVideoEncoderConfiguration>\
                    <Configuration token=\"sub_encoder\" GovLength=\"30\">\
                    <tt:Resolution><tt:Width>320</tt:Width><tt:Height>180</tt:Height></tt:Resolution>\
                    </Configuration></SetVideoEncoderConfiguration>";
        let ack = svc
            .handle_action("SetVideoEncoderConfiguration", body, &req_ip())
            .await
            .unwrap_or_default();
        assert!(
            ack.contains("<tr2:SetVideoEncoderConfigurationResponse"),
            "{ack}"
        );

        let after = svc
            .handle_action(
                "GetVideoEncoderConfiguration",
                "<GetVideoEncoderConfiguration><ConfigurationToken>sub_encoder\
                 </ConfigurationToken></GetVideoEncoderConfiguration>",
                &req_ip(),
            )
            .await
            .unwrap_or_default();
        assert!(after.contains("<tt:Width>320</tt:Width>"), "{after}");
        assert!(
            after.contains("<tt:FrameRateLimit>15</tt:FrameRateLimit>"),
            "{after}"
        );
        // Primary untouched.
        let primary = svc
            .handle_action(
                "GetVideoEncoderConfiguration",
                "<GetVideoEncoderConfiguration><ConfigurationToken>enc0\
                 </ConfigurationToken></GetVideoEncoderConfiguration>",
                &req_ip(),
            )
            .await
            .unwrap_or_default();
        assert!(primary.contains("<tt:Width>1920</tt:Width>"), "{primary}");

        // Faults: missing token, unknown token, JPEG, bogus encoding,
        // non-numeric width.
        for (name, body) in [
            ("missing token", "<SetVideoEncoderConfiguration/>"),
            (
                "unknown token",
                "<SetVideoEncoderConfiguration><Configuration token=\"nope\"/>\
                 </SetVideoEncoderConfiguration>",
            ),
            (
                "jpeg",
                "<SetVideoEncoderConfiguration><Configuration token=\"enc0\">\
                 <tt:Encoding>JPEG</tt:Encoding></Configuration></SetVideoEncoderConfiguration>",
            ),
            (
                "bogus encoding",
                "<SetVideoEncoderConfiguration><Configuration token=\"enc0\">\
                 <tt:Encoding>MPG4</tt:Encoding></Configuration></SetVideoEncoderConfiguration>",
            ),
            (
                "bad width",
                "<SetVideoEncoderConfiguration><Configuration token=\"enc0\">\
                 <tt:Resolution><tt:Width>abc</tt:Width></tt:Resolution>\
                 </Configuration></SetVideoEncoderConfiguration>",
            ),
        ] {
            let err = svc
                .handle_action("SetVideoEncoderConfiguration", body, &req_ip())
                .await
                .unwrap_err();
            assert!(!err.to_string().is_empty(), "{name}");
        }
    }

    #[tokio::test]
    async fn encoder_configurations_filter_and_get_faults() {
        let svc = Media2Service::new(multi_store(), None);

        let xml = svc
            .handle_action(
                "GetVideoEncoderConfigurations",
                "<GetVideoEncoderConfigurations><ConfigurationToken>enc0\
                 </ConfigurationToken></GetVideoEncoderConfigurations>",
                &req_ip(),
            )
            .await
            .unwrap_or_default();
        assert_eq!(xml.matches("<tr2:Configurations ").count(), 1, "{xml}");

        // Unknown filter token → the valid empty set.
        let xml = svc
            .handle_action(
                "GetVideoEncoderConfigurations",
                "<GetVideoEncoderConfigurations><ConfigurationToken>zzz\
                 </ConfigurationToken></GetVideoEncoderConfigurations>",
                &req_ip(),
            )
            .await
            .unwrap_or_default();
        assert_eq!(xml.matches("<tr2:Configurations ").count(), 0, "{xml}");

        // Single get: missing / unknown tokens fault.
        for body in [
            "<GetVideoEncoderConfiguration/>",
            "<GetVideoEncoderConfiguration><ConfigurationToken>zzz\
             </ConfigurationToken></GetVideoEncoderConfiguration>",
        ] {
            let err = svc
                .handle_action("GetVideoEncoderConfiguration", body, &req_ip())
                .await
                .unwrap_err();
            assert!(
                matches!(err, OnvifError::SenderFault(ref m) if m.contains("configuration not found")),
                "got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn sync_point_fires_hook_and_capabilities_follow_store() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let counter = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&counter);
        let svc = Media2Service::new(
            multi_store(),
            Some(Arc::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            })),
        );

        let xml = svc
            .handle_action("SetSynchronizationPoint", "", &req_ip())
            .await
            .unwrap_or_default();
        assert!(
            xml.contains("<tr2:SetSynchronizationPointResponse"),
            "{xml}"
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let caps = svc
            .handle_action("GetServiceCapabilities", "", &req_ip())
            .await
            .unwrap_or_default();
        assert!(caps.contains(r#"SnapshotUri="true""#), "{caps}");
        assert!(caps.contains(r#"MaximumNumberOfProfiles="2""#), "{caps}");
        assert!(caps.contains(r#"RTSPStreaming="true""#), "{caps}");
    }

    #[tokio::test]
    async fn read_actions_never_panic_on_garbage() {
        let svc = Media2Service::new(multi_store(), None);
        for garbage in ["", "not xml", "<broken>", "{{{{{"] {
            for action in [
                "GetProfiles",
                "GetStreamUri",
                "GetVideoEncoderConfigurations",
                "GetVideoEncoderConfiguration",
                "GetVideoEncoderConfigurationOptions",
                "GetVideoEncoderInstances",
                "SetSynchronizationPoint",
                "GetServiceCapabilities",
            ] {
                let result = svc.handle_action(action, garbage, &req_ip()).await;
                if let Ok(xml) = result {
                    assert!(xml.contains("soap:Envelope"), "{action}/{garbage}");
                }
                // A fault is fine (token-requiring actions); a panic
                // would have failed the test.
            }
        }
    }

    #[test]
    fn parse_element_text_forms() {
        assert_eq!(
            parse_element_text("<Token>sub</Token>", "Token"),
            Some("sub")
        );
        assert_eq!(
            parse_element_text("<tt:Token>sub</tt:Token>", "Token"),
            Some("sub")
        );
        assert_eq!(parse_element_text("<Token/>", "Token"), None);
        assert_eq!(parse_element_text("", "Token"), None);
        assert_eq!(parse_element_text("<<<<", "Token"), None);
        // Token is matched exactly — ProfileToken must not satisfy it.
        assert_eq!(
            parse_element_text("<ProfileToken>sub</ProfileToken>", "Token"),
            None
        );
    }

    #[test]
    fn protocol_tolerance_table() {
        assert!(protocol_supported(None).is_ok());
        assert!(protocol_supported(Some("")).is_ok());
        for ok in [
            "RTSP",
            "rtsp",
            "RtspUnicast",
            "RtspMulticast",
            "RTSP_OverHttp",
        ] {
            assert!(protocol_supported(Some(ok)).is_ok(), "{ok}");
        }
        for bad in ["http", "ftp", "udp"] {
            assert!(protocol_supported(Some(bad)).is_err(), "{bad}");
        }
    }
}
