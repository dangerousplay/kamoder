use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::namespaces::SOAP_ENVELOPE;

/// Escape a string for interpolation as XML text content.
///
/// Client-controlled data (SOAP action names, preset names, host identity
/// strings) reaches response text through this helper — without escaping, a
/// `<` or `&` in any of them produces malformed XML that strict NVR parsers
/// reject.
pub(crate) fn xml_escape(text: &str) -> std::borrow::Cow<'_, str> {
    quick_xml::escape::escape(text)
}

/// Accumulates the fragments of one contiguous text region.
///
/// quick-xml 0.41 delivers text content as alternating `Event::Text` (literal
/// chunks) and `Event::GeneralRef` (entity references) events; an element's
/// text value is the concatenation with entities resolved — what the pre-0.41
/// `BytesText::unescape` produced for the whole region. Feed both event kinds,
/// then [`TextAccumulator::flush`] on the next non-text event. A region that
/// contained an unresolvable entity flushes to `None` (poisoned).
#[derive(Default)]
pub(crate) struct TextAccumulator {
    buf: String,
    poisoned: bool,
}

impl TextAccumulator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Append a literal chunk (`Event::Text`).
    pub(crate) fn push_text(&mut self, e: &BytesText<'_>) -> Result<(), quick_xml::Error> {
        if self.poisoned {
            return Ok(());
        }
        self.buf
            .push_str(&e.decode().map_err(quick_xml::Error::from)?);
        Ok(())
    }

    /// Resolve and append one entity reference (`Event::GeneralRef`).
    pub(crate) fn push_ref(
        &mut self,
        r: &quick_xml::events::BytesRef<'_>,
    ) -> Result<(), quick_xml::Error> {
        if self.poisoned {
            return Ok(());
        }
        if let Some(ch) = r.resolve_char_ref()? {
            self.buf.push(ch);
            return Ok(());
        }
        match quick_xml::escape::resolve_predefined_entity(&r.decode()?) {
            Some(text) => self.buf.push_str(text),
            None => {
                self.poisoned = true;
                self.buf.clear();
            }
        }
        Ok(())
    }

    /// Consume the region: `Some(text)` when every fragment resolved,
    /// `None` when an entity in the region was unresolvable.
    pub(crate) fn flush(&mut self) -> Option<String> {
        let poisoned = self.poisoned;
        self.poisoned = false;
        if poisoned {
            None
        } else {
            Some(std::mem::take(&mut self.buf))
        }
    }
}

// ---------------------------------------------------------------------------
// SOAP data types
// ---------------------------------------------------------------------------

/// WS-UsernameToken credentials carried in the SOAP Security header.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsernameToken {
    pub username: String,
    pub password: String,
    pub nonce: String,
    pub created: String,
}

/// Outcome of WS-UsernameToken authentication.
#[derive(Debug, Clone, Default)]
pub struct AuthResult {
    /// The username that was authenticated (empty if none).
    pub username: String,
    /// Whether authentication succeeded.
    pub authenticated: bool,
}

/// Top-level errors that can occur during ONVIF request processing.
#[derive(Debug, Clone)]
pub enum OnvifError {
    /// The requested SOAP action is not supported by this server.
    ActionNotSupported(String),
    /// Authentication failed.
    NotAuthorized(String),
    /// The incoming XML could not be parsed.
    InvalidXml(String),
    /// The server/handler configuration is invalid (fail-fast at build).
    InvalidConfig(String),
    /// Internal server error.
    Internal(String),
    /// A client (sender) mistake — serialized as a SOAP **Sender** fault
    /// with HTTP 400 instead of the default Receiver/500 (parity with
    /// onvif-go's `SenderFaultError`): unknown subscriptions, invalid
    /// durations, exceeded limits, and other request-side problems.
    SenderFault(String),
}

impl std::fmt::Display for OnvifError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OnvifError::ActionNotSupported(a) => write!(f, "Action not supported: {a}"),
            OnvifError::NotAuthorized(m) => write!(f, "Not authorized: {m}"),
            OnvifError::InvalidXml(m) => write!(f, "Invalid XML: {m}"),
            OnvifError::InvalidConfig(m) => write!(f, "Invalid configuration: {m}"),
            OnvifError::Internal(m) => write!(f, "Internal error: {m}"),
            OnvifError::SenderFault(m) => write!(f, "Sender fault: {m}"),
        }
    }
}

impl std::error::Error for OnvifError {}

/// SOAP 1.2 Fault representation used for serialising error responses.
#[derive(Debug, Clone)]
pub struct SoapFault {
    pub code: String,
    pub reason: String,
}

/// Contextual information about an incoming ONVIF request.
#[derive(Debug, Clone)]
pub struct RequestInfo {
    /// The remote (peer) IP of the ONVIF client — used for logging.
    pub client_ip: String,
    /// The local IP that received the connection — used to build URIs
    /// (RTSP stream URI, ONVIF service endpoints) so they are reachable
    /// from the caller. Falls back to the startup-detected `device_ip`
    /// when empty (e.g. UDP discovery has no TCP local address).
    pub server_ip: String,
    pub auth_result: AuthResult,
}

/// Resolve the server IP to use in ONVIF URI responses.
///
/// Uses the per-request `server_ip` (the local interface that received the
/// TCP connection) when it is a real routable address. Falls back to the
/// startup-detected `device_ip` when `server_ip` is empty or a loopback
/// address — this covers requests arriving through a host-side localhost
/// reverse proxy (e.g. a web UI on :8088 proxying to the ONVIF port).
///
/// NOTE for multi-homed hosts: this rewrites loopback-sourced requests to
/// the startup `device_ip`. Hosts that genuinely serve ONVIF on loopback to
/// external callers should pass the loopback address as `device_ip`.
pub fn resolve_server_ip<'a>(server_ip: &'a str, device_ip: &'a str) -> &'a str {
    if server_ip.is_empty() || server_ip.starts_with("127.") || server_ip == "::1" {
        device_ip
    } else {
        server_ip
    }
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

/// Build a complete SOAP 1.2 Envelope containing the given `body_xml`
/// (typically a service response).
pub fn serialize_soap_response(body_xml: &str) -> String {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);

    write_declaration(&mut writer);
    let mut envelope = BytesStart::new("soap:Envelope");
    envelope.push_attribute(("xmlns:soap", SOAP_ENVELOPE));
    writer
        .write_event(Event::Start(envelope))
        .unwrap_or_default();

    // Empty header
    write_empty_tag(&mut writer, "soap:Header");
    // Body with user content
    write_tag_with_raw(&mut writer, "soap:Body", body_xml);

    writer
        .write_event(Event::End(BytesEnd::new("soap:Envelope")))
        .unwrap_or_default();

    String::from_utf8(writer.into_inner()).unwrap_or_default()
}

/// Build a SOAP 1.2 Fault Envelope.
///
/// `fault_code` should be something like `"soap:Sender"` and `reason` a
/// human-readable description.
pub fn serialize_soap_fault(fault_code: &str, reason: &str) -> String {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);

    write_declaration(&mut writer);
    let mut envelope = BytesStart::new("soap:Envelope");
    envelope.push_attribute(("xmlns:soap", SOAP_ENVELOPE));
    writer
        .write_event(Event::Start(envelope))
        .unwrap_or_default();

    write_empty_tag(&mut writer, "soap:Header");

    // ---- Body / Fault ----
    writer
        .write_event(Event::Start(BytesStart::new("soap:Body")))
        .unwrap_or_default();

    writer
        .write_event(Event::Start(BytesStart::new("soap:Fault")))
        .unwrap_or_default();

    // Code
    writer
        .write_event(Event::Start(BytesStart::new("soap:Code")))
        .unwrap_or_default();
    write_text_tag(&mut writer, "soap:Value", fault_code);
    writer
        .write_event(Event::End(BytesEnd::new("soap:Code")))
        .unwrap_or_default();

    // Reason
    writer
        .write_event(Event::Start(BytesStart::new("soap:Reason")))
        .unwrap_or_default();
    write_text_tag(&mut writer, "soap:Text", reason);
    writer
        .write_event(Event::End(BytesEnd::new("soap:Reason")))
        .unwrap_or_default();

    writer
        .write_event(Event::End(BytesEnd::new("soap:Fault")))
        .unwrap_or_default();
    writer
        .write_event(Event::End(BytesEnd::new("soap:Body")))
        .unwrap_or_default();

    writer
        .write_event(Event::End(BytesEnd::new("soap:Envelope")))
        .unwrap_or_default();

    String::from_utf8(writer.into_inner()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Internal writer helpers
// ---------------------------------------------------------------------------

fn write_declaration(writer: &mut Writer<Vec<u8>>) {
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("utf-8"), None)))
        .unwrap_or_default();
}

fn write_empty_tag(writer: &mut Writer<Vec<u8>>, name: &str) {
    writer
        .write_event(Event::Start(BytesStart::new(name)))
        .unwrap_or_default();
    writer
        .write_event(Event::End(BytesEnd::new(name)))
        .unwrap_or_default();
}

fn write_text_tag(writer: &mut Writer<Vec<u8>>, name: &str, text: &str) {
    writer
        .write_event(Event::Start(BytesStart::new(name)))
        .unwrap_or_default();
    writer
        .write_event(Event::Text(BytesText::from_escaped(xml_escape(text))))
        .unwrap_or_default();
    writer
        .write_event(Event::End(BytesEnd::new(name)))
        .unwrap_or_default();
}

fn write_tag_with_raw(writer: &mut Writer<Vec<u8>>, name: &str, raw: &str) {
    use std::io::Write;
    writer
        .write_event(Event::Start(BytesStart::new(name)))
        .unwrap_or_default();
    // Write raw content WITHOUT XML escaping
    writer
        .get_mut()
        .write_all(raw.as_bytes())
        .unwrap_or_default();
    writer
        .write_event(Event::End(BytesEnd::new(name)))
        .unwrap_or_default();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_accumulator_reassembles_split_text_regions() {
        use quick_xml::events::Event;
        use quick_xml::Reader;
        let read_text = |xml: &str| -> Option<String> {
            let mut reader = Reader::from_str(xml);
            let mut buf = Vec::new();
            let mut acc = TextAccumulator::new();
            loop {
                match reader.read_event_into(&mut buf) {
                    Ok(Event::Text(e)) => {
                        let _ = acc.push_text(&e);
                    }
                    Ok(Event::GeneralRef(e)) => {
                        let _ = acc.push_ref(&e);
                    }
                    Ok(Event::End(_)) | Ok(Event::Eof) => return acc.flush(),
                    Err(_) => return None,
                    _ => {}
                }
                buf.clear();
            }
        };
        assert_eq!(
            read_text("<x>a&gt;b</x>").as_deref(),
            Some("a>b"),
            "split Text/GeneralRef chunks must reassemble with entities resolved"
        );
        assert_eq!(
            read_text("<x>&amp;x</x>").as_deref(),
            Some("&x"),
            "predefined entity alone must resolve"
        );
        assert_eq!(
            read_text("<x>a&#38;b</x>").as_deref(),
            Some("a&b"),
            "numeric character references must resolve"
        );
        assert_eq!(
            read_text("<x></x>").as_deref(),
            Some(""),
            "empty region must stay valid"
        );
        assert!(
            read_text("<x>&some;</x>").is_none(),
            "undefined entity must poison the region"
        );
    }

    #[test]
    fn test_serialize_soap_response_includes_envelope() {
        let xml = serialize_soap_response("<test>hello</test>");
        assert!(xml.contains("soap:Envelope"), "should have Envelope");
        assert!(xml.contains("soap:Body"), "should have Body");
        assert!(xml.contains("<test>hello</test>"), "should embed body XML");
        assert!(xml.contains(SOAP_ENVELOPE), "should declare SOAP namespace");
    }

    #[test]
    fn test_serialize_soap_fault_includes_code_reason() {
        let xml = serialize_soap_fault("soap:Sender", "test fault");
        assert!(xml.contains("soap:Envelope"));
        assert!(xml.contains("soap:Fault"));
        assert!(xml.contains("soap:Value"));
        assert!(xml.contains("soap:Text"));
        assert!(xml.contains("soap:Sender"));
        assert!(xml.contains("test fault"));
    }

    #[test]
    fn test_onvif_error_display() {
        let e = OnvifError::ActionNotSupported("GetFoo".into());
        assert!(e.to_string().contains("GetFoo"));

        let e = OnvifError::NotAuthorized("bad pass".into());
        assert!(e.to_string().contains("bad pass"));

        let e = OnvifError::InvalidXml("malformed".into());
        assert!(e.to_string().contains("malformed"));

        let e = OnvifError::Internal("oops".into());
        assert!(e.to_string().contains("oops"));
    }

    #[test]
    fn test_serialize_fault_with_body_fragment() {
        // Verify that a fault can be parsed back by quick_xml to catch
        // well-formedness issues.
        let xml = serialize_soap_fault("soap:Receiver", "something broke");
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut count = 0;
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(quick_xml::events::Event::Eof) => break,
                Err(e) => panic!("fault XML parse error: {e}"),
                _ => count += 1,
            }
            buf.clear();
        }
        assert!(count > 5, "fault should have multiple XML events");
    }

    #[test]
    fn test_serialize_response_well_formed() {
        let xml = serialize_soap_response("<GetDeviceInformationResponse/>");
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(quick_xml::events::Event::Eof) => break,
                Err(e) => panic!("response XML parse error: {e}"),
                _ => {}
            }
            buf.clear();
        }
    }
}
