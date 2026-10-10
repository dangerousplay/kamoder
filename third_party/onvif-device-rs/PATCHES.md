# Local patches to onvif-device-rs 0.8.0

This directory is the crates.io source of `onvif-device-rs` 0.8.0
(MIT, <https://github.com/mickeyzzc/onvif-rs>), copied verbatim from the cargo
registry and wired into the workspace through `[patch.crates-io]` in the root
`Cargo.toml`. Everything here except the changes listed below is upstream code.

**This fork is temporary.** Drop it (delete this directory and the
`[patch.crates-io]` entry, bump the `onvif-device-rs` version in
`onvif/Cargo.toml`) as soon as upstream releases the fix. The proposed upstream
PR, with the full diff against 0.8.0, is in `UPSTREAM-PR.md`.

## 1. Keep inherited namespace declarations on the extracted Body child

File: `src/server.rs` (`parse_soap_request`, new `with_inherited_namespaces`,
tests in `mod tests`).

**Problem.** `parse_soap_request` copies the SOAP Body's first child (the action
element) into `ParsedSoap::body_xml`, which is what
`OnvifActionHandler::handle` receives. Namespace declarations made on its
ancestors (`<s:Envelope>`, `<s:Body>`) were not copied. gSOAP-based clients
(ONVIF Device Manager and friends) declare every prefix on the Envelope
(`xmlns:tt`, `xmlns:tptz`, ...), so handlers got a fragment with unbound
prefixes and any namespace-aware deserializer rejected it (for example
`UnexpectedEvent(tptz:ProfileToken)` for `ContinuousMove`).

**Fix.** The parser now keeps a `quick_xml::name::NamespaceResolver` of the open
elements (push on `Start`, pop on `End`). When the action element is written,
every binding in scope from its ancestors, the default namespace included, is
added to it as an `xmlns`/`xmlns:p` attribute, unless the element declares that
prefix itself. Descendants are untouched; they already inherit from the action
element. Header declarations are out of scope for the Body and are not copied.

**Tests added.** `test_parse_body_inherits_envelope_namespaces`,
`test_parse_body_keeps_own_namespace_redeclaration`,
`test_parse_body_inherits_default_namespace`,
`test_parse_body_ignores_header_namespaces`.

No other files differ from 0.8.0 (the registry's `.cargo-ok` marker was not
copied).
