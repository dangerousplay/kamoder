# Upstream PR draft (not submitted)

Target: <https://github.com/mickeyzzc/onvif-rs>, base `v0.8.0`.

## Title

server: keep inherited xmlns declarations on the SOAP Body child handed to handlers

## Description

### Problem

`parse_soap_request` extracts the first child of the SOAP Body and passes it
verbatim to `OnvifActionHandler::handle` as `body`. Namespace declarations made
on its ancestors (`Envelope`, `Body`) are dropped, so any prefix the client
declared there is unbound in the fragment the handler gets.

Many clients do exactly that. gSOAP-generated ones (ONVIF Device Manager, most
NVR firmwares) declare every namespace on the Envelope. With a namespace-aware
deserializer in the handler (e.g. quick-xml `NsReader`, or serde-based ones), the
request is rejected: `UnexpectedEvent(tptz:ProfileToken)`, unknown prefix, and
so on. Handlers that substring-match the body happen to work, which is why this
did not show up so far.

### Repro

```xml
<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope"
                   xmlns:tt="http://www.onvif.org/ver10/schema"
                   xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl">
  <SOAP-ENV:Body>
    <tptz:ContinuousMove>
      <tptz:ProfileToken>profile_1</tptz:ProfileToken>
      <tptz:Velocity><tt:PanTilt x="0.5" y="0"/></tptz:Velocity>
    </tptz:ContinuousMove>
  </SOAP-ENV:Body>
</SOAP-ENV:Envelope>
```

0.8.0 hands the handler

```xml
<tptz:ContinuousMove><tptz:ProfileToken>profile_1</tptz:ProfileToken>...
```

with `tptz` and `tt` undeclared. With this PR it gets

```xml
<tptz:ContinuousMove xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl">...
```

### Fix

`parse_soap_request` now tracks the namespace bindings of the open elements
with quick-xml's own `NamespaceResolver` (push on `Start`, pop on `End`). When
the action element is written to `body_xml`, every binding in scope from its
ancestors, the default namespace included, is added to it as an `xmlns` /
`xmlns:p` attribute, unless the element redeclares that prefix itself (its own
declaration wins). Descendants are not touched, since they inherit from the
action element. Declarations on `Header` descendants are out of scope for the
Body and are not copied. Malformed declarations are skipped rather than turned
into errors, so no request that parsed before now fails.

This is the same approach as e.g. `soap-server`'s envelope extraction.
`ParsedSoap::body_xml` gets a doc comment stating the guarantee.

### Tests

New unit tests in `server::tests`, each checking the body with a
namespace-aware `NsReader` pass (every element/attribute prefix resolves, the
action is in the expected namespace):

- `test_parse_body_inherits_envelope_namespaces`: prefixes declared on the
  Envelope and Body, used in the body (gSOAP style).
- `test_parse_body_keeps_own_namespace_redeclaration`: the action redeclares a
  prefix the Envelope also binds; only its own binding is kept.
- `test_parse_body_inherits_default_namespace`: default namespace on the
  Envelope with an unprefixed action; and an action with its own default
  namespace keeps exactly one `xmlns`.
- `test_parse_body_ignores_header_namespaces`: Header-scoped declarations do
  not leak into the body.

`cargo test` and `cargo test --all-features` (467 tests incl. integration),
`cargo clippy --all-targets --all-features -- -D warnings` and
`cargo fmt --check` pass.

## Diff against 0.8.0

```diff
diff -ru a/src/server.rs b/src/server.rs
--- a/src/server.rs
+++ b/src/server.rs
@@ -4,7 +4,8 @@
 use std::time::{Duration, Instant};
 
 use async_trait::async_trait;
-use quick_xml::events::Event;
+use quick_xml::events::{BytesStart, Event};
+use quick_xml::name::{NamespaceResolver, PrefixDeclaration};
 use quick_xml::Reader;
 use tokio::io::{AsyncReadExt, AsyncWriteExt};
 use tokio::net::TcpListener;
@@ -190,6 +191,9 @@
 /// Result of parsing an incoming SOAP 1.2 request.
 pub struct ParsedSoap {
     pub action: String,
+    /// The Body's first child, verbatim. Namespace declarations made on its
+    /// ancestors (`Envelope`, `Body`) are copied onto it so the fragment is
+    /// self-contained and can be fed to a namespace-aware parser.
     pub body_xml: String,
     pub username_token: Option<UsernameToken>,
 }
@@ -1343,6 +1347,9 @@
         action: String,
         body_xml: String,
         body_writer: Option<quick_xml::Writer<Vec<u8>>>,
+        // Namespace bindings of the open elements, so the action element can
+        // carry the declarations it inherits from Envelope / Body.
+        ns: NamespaceResolver,
     }
 
     let mut st = ParseState::default();
@@ -1401,7 +1408,8 @@
                                     st.action = local.to_string();
                                     // Write the action element to the body writer
                                     if let Some(ref mut w) = st.body_writer {
-                                        let _ = w.write_event(Event::Start(e.clone()));
+                                        let start = with_inherited_namespaces(&e, &st.ns);
+                                        let _ = w.write_event(Event::Start(start));
                                     }
                                 } else if st.in_body {
                                     if let Some(ref mut w) = st.body_writer {
@@ -1410,6 +1418,10 @@
                                 }
                             }
                         }
+                        // After the action check above, which needs the bindings
+                        // of `e`'s ancestors only. A malformed declaration just
+                        // leaves that binding out.
+                        let _ = st.ns.push(&e);
                     }
 
                     Ok(Event::Empty(e)) => {
@@ -1428,7 +1440,8 @@
                                 if st.in_body && st.action.is_empty() {
                                     st.action = local.to_string();
                                     if let Some(ref mut w) = st.body_writer {
-                                        let _ = w.write_event(Event::Empty(e.clone()));
+                                        let start = with_inherited_namespaces(&e, &st.ns);
+                                        let _ = w.write_event(Event::Empty(start));
                                     }
                                 } else if st.in_body {
                                     if let Some(ref mut w) = st.body_writer {
@@ -1443,6 +1456,7 @@
                         let name_bytes = e.name().as_ref().to_owned();
                         let qname = str::from_utf8(&name_bytes).unwrap_or("");
                         let local = qname.rsplit(':').next().unwrap_or(qname);
+                        st.ns.pop();
 
                         // The Body's own close tag terminates body_xml — do not copy
                         // it in (its opening twin was never written, so appending it
@@ -1523,6 +1537,33 @@
     })
 }
 
+/// Returns a copy of `start` that also declares every namespace binding in
+/// scope in `ns` (its ancestors), except prefixes `start` redeclares itself.
+///
+/// The Body's child is handed to handlers on its own, so declarations made on
+/// `Envelope` / `Body` (as gSOAP-based clients do for `tt`, `tptz`, ...) would
+/// otherwise be lost and its prefixes left unbound.
+fn with_inherited_namespaces<'a>(start: &BytesStart<'a>, ns: &NamespaceResolver) -> BytesStart<'a> {
+    let own: Vec<Vec<u8>> = start
+        .attributes()
+        .with_checks(false)
+        .flatten()
+        .filter(|a| a.key.as_namespace_binding().is_some())
+        .map(|a| a.key.as_ref().to_vec())
+        .collect();
+    let mut out = start.clone();
+    for (prefix, namespace) in ns.bindings() {
+        let key = match prefix {
+            PrefixDeclaration::Default => b"xmlns".to_vec(),
+            PrefixDeclaration::Named(name) => [b"xmlns:".as_slice(), name].concat(),
+        };
+        if !own.contains(&key) {
+            out.push_attribute((key.as_slice(), namespace.0));
+        }
+    }
+    out
+}
+
 // ---------------------------------------------------------------------------
 // Tests
 // ---------------------------------------------------------------------------
@@ -1714,6 +1755,117 @@
         assert_eq!(parsed.action, "GetSystemDateAndTime");
     }
 
+    /// Namespace-aware check: every element and attribute prefix in `body`
+    /// resolves, and the action element is in namespace `action_ns`.
+    fn assert_body_namespaces_resolve(body: &str, action_ns: &str) {
+        let mut reader = quick_xml::NsReader::from_str(body);
+        let mut first = true;
+        loop {
+            match reader.read_resolved_event() {
+                Ok((ns, Event::Start(e))) | Ok((ns, Event::Empty(e))) => {
+                    let quick_xml::name::ResolveResult::Bound(ns) = ns else {
+                        panic!("unbound element {:?} in {body}", e.name());
+                    };
+                    if first {
+                        assert_eq!(ns.as_ref(), action_ns.as_bytes(), "action ns in {body}");
+                        first = false;
+                    }
+                    for a in e.attributes() {
+                        let a = a.unwrap();
+                        let (res, _) = reader.resolver().resolve_attribute(a.key);
+                        assert!(
+                            !matches!(res, quick_xml::name::ResolveResult::Unknown(_)),
+                            "unbound attribute {:?} in {body}",
+                            a.key
+                        );
+                    }
+                }
+                Ok((_, Event::Eof)) => break,
+                Ok(_) => {}
+                Err(e) => panic!("{e} in {body}"),
+            }
+        }
+        assert!(!first, "no element in {body}");
+    }
+
+    #[test]
+    fn test_parse_body_inherits_envelope_namespaces() {
+        // gSOAP-based clients (ONVIF Device Manager, ...) declare every prefix
+        // on the Envelope and use them inside the Body.
+        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
+        <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope"
+                           xmlns:tt="http://www.onvif.org/ver10/schema"
+                           xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl">
+          <SOAP-ENV:Body xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
+            <tptz:ContinuousMove>
+              <tptz:ProfileToken>profile_1</tptz:ProfileToken>
+              <tptz:Velocity>
+                <tt:PanTilt x="0.5" y="0" xsi:type="tt:Vector2D"/>
+              </tptz:Velocity>
+            </tptz:ContinuousMove>
+          </SOAP-ENV:Body>
+        </SOAP-ENV:Envelope>"#;
+
+        let parsed = parse_soap_request(xml).unwrap();
+        assert_eq!(parsed.action, "ContinuousMove");
+        let body = &parsed.body_xml;
+        for decl in [
+            r#"xmlns:tt="http://www.onvif.org/ver10/schema""#,
+            r#"xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl""#,
+            r#"xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance""#,
+            r#"xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope""#,
+        ] {
+            assert!(body.contains(decl), "missing {decl} in {body}");
+        }
+        assert_body_namespaces_resolve(body, "http://www.onvif.org/ver20/ptz/wsdl");
+    }
+
+    #[test]
+    fn test_parse_body_keeps_own_namespace_redeclaration() {
+        // A prefix redeclared on the action element wins over the Envelope's.
+        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
+                       xmlns:tds="urn:outer"><s:Body><tds:GetDeviceInformation xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/></s:Body></s:Envelope>"#;
+
+        let parsed = parse_soap_request(xml).unwrap();
+        assert_eq!(parsed.action, "GetDeviceInformation");
+        let body = &parsed.body_xml;
+        assert!(!body.contains("urn:outer"), "outer binding leaked: {body}");
+        assert_eq!(body.matches("xmlns:tds=").count(), 1, "{body}");
+        assert_body_namespaces_resolve(body, "http://www.onvif.org/ver10/device/wsdl");
+    }
+
+    #[test]
+    fn test_parse_body_inherits_default_namespace() {
+        // Default namespace declared on the Envelope, unprefixed action.
+        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
+                       xmlns="http://www.onvif.org/ver10/device/wsdl"><s:Body><GetHostname/></s:Body></s:Envelope>"#;
+
+        let parsed = parse_soap_request(xml).unwrap();
+        assert_eq!(parsed.action, "GetHostname");
+        let body = &parsed.body_xml;
+        assert!(
+            body.contains(r#"xmlns="http://www.onvif.org/ver10/device/wsdl""#),
+            "missing default namespace in {body}"
+        );
+        assert_body_namespaces_resolve(body, "http://www.onvif.org/ver10/device/wsdl");
+
+        // ...and an action that sets its own default namespace keeps it.
+        let xml = r#"<Envelope xmlns="http://www.w3.org/2003/05/soap-envelope"><Body><GetProfiles xmlns="http://www.onvif.org/ver10/media/wsdl"/></Body></Envelope>"#;
+        let body = parse_soap_request(xml).unwrap().body_xml;
+        assert_eq!(body.matches("xmlns=").count(), 1, "{body}");
+        assert_body_namespaces_resolve(&body, "http://www.onvif.org/ver10/media/wsdl");
+    }
+
+    #[test]
+    fn test_parse_body_ignores_header_namespaces() {
+        // Declarations on Header descendants are out of scope for the Body.
+        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Header><wsse:Security xmlns:wsse="urn:wsse"/></s:Header><s:Body><GetUsers xmlns="http://www.onvif.org/ver10/device/wsdl"/></s:Body></s:Envelope>"#;
+
+        let body = parse_soap_request(xml).unwrap().body_xml;
+        assert!(!body.contains("urn:wsse"), "Header binding leaked: {body}");
+        assert_body_namespaces_resolve(&body, "http://www.onvif.org/ver10/device/wsdl");
+    }
+
     // ------------------------------------------------------------------
     // Auth integration
     // ------------------------------------------------------------------
```
