//! The two conversions every handler makes: the SOAP Body child that
//! `onvif-device-rs` hands over into a generated request type, and a
//! generated response value into the envelope it sends back.
//!
//! The types come from `onvif-schema` (the official WSDL/XSD), so requests
//! are matched by namespace whatever prefixes the client picked, and
//! responses are qualified (`tds:`/`trt:`/`tptz:` messages, `tt:` types).

use onvif_device_rs::types::{serialize_soap_response, OnvifError};
use onvif_schema::xsd_parser_types::quick_xml::{WithDeserializer, WithSerializer};

/// Parses a request body as `T`. A body the schema does not accept (wrong
/// namespace, missing required element, bad number) is the client's
/// mistake: a Sender fault, HTTP 400.
pub fn request<T: WithDeserializer>(body: &str) -> Result<T, OnvifError> {
    onvif_schema::from_xml(body)
        .map_err(|error| OnvifError::SenderFault(format!("invalid request: {error}")))
}

/// Serializes `value` as the element `root` (e.g. `"tptz:StopResponse"`)
/// inside a SOAP 1.2 response envelope.
pub fn response<T: WithSerializer>(value: &T, root: &str) -> Result<String, OnvifError> {
    let body = onvif_schema::to_xml(value, root)
        .map_err(|error| OnvifError::Internal(format!("{root}: {error}")))?;
    Ok(serialize_soap_response(&body))
}
