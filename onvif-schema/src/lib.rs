//! ONVIF message types, generated from the official WSDL/XSD.
//!
//! Each ONVIF namespace is a module named after its conventional prefix:
//! [`tt`] (`http://www.onvif.org/ver10/schema`, the shared types), [`tds`]
//! (Device), [`trt`] (Media), [`tptz`] (PTZ) and [`timg`] (Imaging). A
//! request element such as `tptz:ContinuousMove` is `tptz::ContinuousMove`,
//! its response `tptz::ContinuousMoveResponse`.
//!
//! [`to_xml`] and [`from_xml`] convert between a value and the XML of one
//! SOAP Body child. The SOAP envelope, faults and authentication are the
//! transport's job (onvif-device-rs), not this crate's.
//!
//! ```
//! use onvif_schema::{from_xml, tptz};
//!
//! let body = r#"<tptz:Stop xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl">
//!     <tptz:ProfileToken>Profile_1</tptz:ProfileToken></tptz:Stop>"#;
//! let stop: tptz::Stop = from_xml(body).unwrap();
//! assert_eq!(stop.profile_token, "Profile_1");
//! ```
//!
//! # Namespaces are checked, prefixes are not
//!
//! Parsing matches elements by namespace URI, so `tptz:`, a default `xmlns`
//! and zeep's `ns0:` all work, and an element in the wrong namespace is an
//! error. Serializing writes the pinned prefixes (`tt:`, `trt:`, `tptz:`,
//! `tds:`, `timg:`) and declares every namespace the value uses on the root
//! element.
//! The body must carry its own `xmlns` declarations: a prefix declared only
//! on `<s:Envelope>` is unknown here.
//!
//! # Where the code comes from
//!
//! `src/generated` is the output of `./x gen-onvif` (xsd-parser, in xtask)
//! over the schemas vendored in `schema/` — see `schema/README.md` for their
//! sources, versions and checksums. It is committed, not built by a build
//! script, so the generator never runs in a camera build and changes to the
//! types are reviewable diffs. `./x gen-onvif --check` (part of `./x ci`)
//! fails if it no longer matches the generator. Only the operations listed
//! in `SERVICES` in `xtask/src/gen_onvif.rs` and the types they reference are
//! generated; to add one, extend that list and regenerate. Never edit the
//! generated files by hand.

// The generated code is not written to pass this workspace's lints. It is
// one deserializer state machine per type, emitted from a template, so every
// type gets the same scaffolding whether it needs it or not: an unconstructed
// `Phantom__` variant, `helper`/`state` parameters that go unused, `mut` on
// bindings that are never mutated (rustc `unused`, ~370 hits), plus clippy's
// style and complexity lints. Fixing that would mean patching xsd-parser's
// output, which `./x gen-onvif --check` forbids. The allowance stops at this
// module; the helpers below are linted normally.
#[allow(clippy::all, unused)]
mod generated;

pub use generated::*;
/// The runtime crate the generated types are built on, for the types that
/// appear in their fields (`AnyAttributes`, `AnyElement`) and its error type.
pub use xsd_parser_types;

use xsd_parser_types::quick_xml::{
    DeserializeSync, Error, SerializeSync, SliceReader, WithDeserializer, WithSerializer, Writer,
    XmlReader,
};

/// Serializes `value` as the element `root`, e.g. `"tptz:GetStatusResponse"`.
///
/// `root` should use the generated prefix of the element's namespace (`tt`,
/// `tds`, `trt`, `tptz`, `timg`): the serializer declares the namespaces
/// under those prefixes, so any other prefix would come out undeclared.
pub fn to_xml<T: WithSerializer>(value: &T, root: &str) -> Result<String, Error> {
    let mut buffer = Vec::new();
    value.serialize(root, &mut Writer::new(&mut buffer))?;
    // quick-xml writes only what it was given, and every input is a `&str`.
    Ok(String::from_utf8(buffer).expect("the serializer writes UTF-8"))
}

/// Parses one element, typically the child of a SOAP `<Body>`, as `T`.
///
/// Fails if the element's namespace or content does not match the schema.
pub fn from_xml<T: WithDeserializer>(xml: &str) -> Result<T, Error> {
    let mut reader = SliceReader::new(xml).with_error_info();
    T::deserialize(&mut reader)
}
