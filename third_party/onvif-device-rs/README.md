# onvif-rs

**English** | [中文](README.zh-CN.md)

[![CI](https://github.com/mickeyzzc/onvif-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/mickeyzzc/onvif-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Language: Rust](https://img.shields.io/badge/language-Rust-dea584.svg)
![Tests](https://img.shields.io/badge/tests-188%20passing-brightgreen.svg)

ONVIF **Device (server)** library for Rust — expose a camera or media source to ONVIF consumers (NVRs, video management systems) over SOAP + WS-Discovery.

> **Naming decision**: this project is unrelated to [lumeohq/onvif-rs](https://github.com/lumeohq/onvif-rs) (a WSDL-generated ONVIF *client*). The crates.io name `onvif-rs` is held by an abandoned 2018 placeholder, so the crate is published as **`onvif-device-rs`**; this repository keeps its original name.

## Features

- **SOAP HTTP server** with per-action handler registration — Device, Media, Imaging, and (virtual) PTZ services; the Device service covers the full devicemgmt read/write surface: **SetSystemDateAndTime**, mutable **scopes** (Add/Remove/Set; GetScopes answers the WSDL `tds:Scopes`/`tt:ScopeDef`/`tt:ScopeItem` form — built-ins `Fixed`, client-added `Configurable`), **Get/SetHostname**, **Get/SetDiscoveryMode**, a **user directory** (Get/Create/Delete/SetUser — a directory only, the WS-Security layer stays the auth source; seed via `with_users`), honest network statics (GetDNS/GetNTP/GetNetworkInterfaces/GetNetworkDefaultGateway/GetNetworkProtocols), GetServiceCapabilities/GetWsdlUrl/GetEndpointReference, **GetSystemLog/GetSystemSupportInformation**, and the protocol-answer-only family (**SystemReboot**, SetSystemFactoryDefault, UpgradeSystemFirmware, StartSystemRestore) whose real side effects belong to the host via the `DeviceHooks` seam (`with_hooks`) — Set* network variants, certificates, and 802.1X are deliberately unimplemented; GetServices/GetCapabilities enumerate exactly what the host serves — Media/PTZ/Imaging behind `with_media_support`/`with_ptz_support`/`with_imaging_support` flags (default on), Events behind `with_events_support`
- **WS-Discovery responder** — UDP multicast 239.255.255.250:3702 Probe/ProbeMatches with per-request XAddr echo; scopes and EndpointReference UUID are host-configurable; announces itself with **Hello on start and Bye on stop** (same envelope family as ProbeMatches)
- **WS-Security** — UsernameToken verification, PasswordText and PasswordDigest (SHA-1), constant-time comparison, **fail-closed** empty-password handling
- **Security completion** (issue #54) — three additions on top of WS-Security: **HTTP Digest transport auth** (RFC 7616 subset: MD5, `qop="auth"`; opt-in via `OnvifConfig::http_digest` — token-less requests get a 401 `WWW-Authenticate: Digest` challenge, a valid Digest header authenticates without a UsernameToken, both mechanisms coexist and a present UsernameToken takes precedence; server nonces expire after 300 s and enforce per-nonce nc advancement, so a captured `Authorization` header is not replayable); **IP address filter** — `DeviceServiceHandlers::with_ip_filter` + `OnvifServer::with_ip_filter` wired to the *same* shared store: Get/Set/Add/RemoveIPAddressFilter mutate it over SOAP while the server refuses blocked peers with 403 before any HTTP/SOAP processing (IPv4 prefixes, Allow/Deny modes; unparseable/IPv6 peer addresses fail open, logged); **Get/SetAccessPolicy** — an opaque Base64 `PolicyFile` blob stored and returned verbatim (`with_access_policy`; interpreting the policy is host-side). 802.1X is deliberately out of scope (an EAP supplicant is host infrastructure, not SOAP protocol — such actions answer the generic unsupported-action fault)
- **TLS listener** (optional `tls` cargo feature, off by default) — serve ONVIF over HTTPS (Profile T's transport baseline) by pointing `tls_cert_file`/`tls_key_file` at PEM files; both-or-neither, no silent plain-HTTP fallback
- **Events pull-point service** (parity with onvif-go's `SupportEvents`) — `server.enable_events()` routes `/onvif/events_service` (GetServiceCapabilities / GetEventProperties / CreatePullPointSubscription) and `/onvif/events_service/sub/<id>` (PullMessages / Renew / Unsubscribe) on the listener the server already owns; hosts inject events through the returned `EventsService::publish_event` seam (fan-out to every live subscription, Concrete/ConcreteSet topic filters, ISO8601 termination-time clamping, long-poll with PT0S legal); GetCapabilities/GetServices advertise the service only while enabled, and Create* actions stay behind WS-Security while reads are open
- **Events basic notification interface** (`wsnt:Subscribe`) — push subscriptions alongside pull points: `Subscribe` registers a consumer (ConsumerReference `http://` URL only — https consumers are refused; the library carries no TLS client) and one sender task per subscription POSTs every matching published event to the consumer as a `wsnt:Notify` SOAP document (Topic + inner-message writers shared with PullMessages responses, SubscriptionReference named, ProducerReference omitted); delivery is fire-and-forget (5 s connect/IO timeouts) and after three consecutive failures the subscription is auto-unsubscribed (spec-permissible housekeeping); Renew/Unsubscribe share the pull-point lifecycle and the 10-subscription cap; `SetSynchronizationPoint` acks empty on the events endpoint (route-based dispatch, no collision with the media action); GetEventInstances deliberately stays unimplemented (17.06+ feature, zero demand)
- **Imaging focus control** — Move (absolute/relative/continuous focus through the `ImagingParams::focus_move` seam), Stop, GetMoveOptions, GetStatus, GetServiceCapabilities; shared action names (`GetStatus`/`Stop`/`GetServiceCapabilities`) are routed by request shape with the previously registered handler (PTZ) as fallback
- **Namespace-agnostic request parsing** (clients send arbitrary XML prefixes) and explicit-prefix serialization (`tds:`/`trt:`/`timg:`/`tt:`), with XML escaping of all interpolated values
- **Multiple media profiles** (main + substreams) — `OnvifMediaConfig::extra_profiles` advertises additional `MediaProfileConfig` entries (e.g. a 640×360 bandwidth-saving substream) in GetProfiles, always after the primary; GetStreamUri resolves the request's `ProfileToken` against them and fails open to the primary stream for unknown/missing tokens, so single-profile clients see no change
- **Media service completion** — `register_media_actions(server, Arc<RwLock<OnvifMediaConfig>>, keyframe_hook)` registers the whole service in one call: the video encoder configuration family (GetVideoEncoderConfigurations / GetVideoEncoderConfiguration / GetVideoEncoderConfigurationOptions / **SetVideoEncoderConfiguration** — writes land in the shared store every reader, including GetProfiles, immediately reflects), GetGuaranteedNumberOfVideoEncoderInstances, **SetSynchronizationPoint** (fires the host's keyframe/IDR hook), the media GetServiceCapabilities (SnapshotUri follows `snapshot_port`), and the empty audio/OSD sets (no audio hardware, no OSD engine — honest empty answers, not faults); multicast streaming stays unimplemented and is advertised off (`RTPMulticast="false"`)
- **Media2 service** (ver20/media, `tr2` — the Profile-T entry path, parity with onvif-go's `SupportMedia2`) — `server.enable_media2(store, keyframe_hook)` routes `/onvif/media2_service` with its **own dispatch** on the listener the server already owns (the action local names collide with Media1 in the shared action map — the URL decides which face answers): GetProfiles (optional Token filter; `tr2:Profiles`/`tr2:Configurations` wrappers with `tt:` configuration bodies), GetStreamUri / GetSnapshotUri (the plain `tr2:Uri` form), the video-encoder configuration family and options in the tr2 element shapes (child-element resolution list), GetVideoEncoderInstances, **SetSynchronizationPoint** (the same keyframe hook), and the Media2 GetServiceCapabilities (`RTSPStreaming="true"`); reads and writes go through the SAME `SharedMediaConfig` store as Media1 — a SetVideoEncoderConfiguration through either face is immediately visible to both; Set* actions stay behind WS-Security while reads are open; the GetServices advertisement is behind `with_media2_support` (default off — existing bytes unchanged; the legacy GetCapabilities has no Media2 slot)
- **Virtual PTZ** — a pure state machine (`ptz_state`) behind the PTZ service for devices without motors: moves, presets, **home position** (Set/Goto), configuration storage (**SetConfiguration** reflected by GetConfigurations), **GetConfigurationOptions** (coordinate spaces + timeout), **SendAuxiliaryCommand** (ack + echo), **GetPTZServiceCapabilities**
- **Graceful shutdown** for both the SOAP server and the discovery responder

Extracted from the production implementation in [mibee-eye-raspi-rs](https://github.com/Mi-Bee-Studio), whose response XML is **byte-stable against the MiBee NVR** (raw SOAP local-name matching). Element names follow the official WSDL (`GetStreamUriResponse → MediaUri → Uri`).

## Usage

```toml
[dependencies]
onvif-device-rs = "0.3.0"
# git alternative: onvif-device-rs = { git = "https://github.com/mickeyzzc/onvif-rs.git", tag = "v0.3.0" }
```

```rust,no_run
use std::sync::Arc;
use onvif_device_rs::config::DeviceConfig;
use onvif_device_rs::device::{DeviceHandler, DeviceServiceHandlers};
use onvif_device_rs::discovery::DiscoveryServer;
use onvif_device_rs::imaging::{register_imaging_actions, ImagingParamError, ImagingParams};
use onvif_device_rs::media::{
    GetProfilesHandler, GetSnapshotUriHandler, GetStreamUriHandler, OnvifMediaConfig,
};
use onvif_device_rs::server::{OnvifConfig, OnvifServer};

// Implement the Imaging seam over your camera's parameter manager.
struct MyParams;
impl ImagingParams for MyParams {
    fn get_param(&self, name: &str) -> Result<f64, ImagingParamError> { todo!() }
    fn set_param(&self, name: &str, value: f64) -> Result<(), ImagingParamError> { todo!() }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let device_ip = "192.0.2.10".to_string();
    let port = 8080u16;

    // Auth is fail-closed: an empty password without allow_no_auth = true
    // makes start() return an error instead of silently opening the server.
    let config = OnvifConfig {
        port,
        username: "admin".to_string(),
        password: "set-a-real-password".to_string(),
        ..Default::default()
    };
    // TLS (optional `tls` feature): also set tls_cert_file + tls_key_file
    // to PEM file paths and start() serves https instead (both-or-neither).
    let mut server = OnvifServer::new(&config);

    // Device service: identity comes from DeviceConfig (host-supplied).
    // The neutral "unknown" placeholders fail validation — set the real
    // identity explicitly (issue #20).
    let device = Arc::new(DeviceServiceHandlers::new(
        DeviceConfig {
            name: "My Camera".into(),
            manufacturer: "My Company".into(),
            model: "Cam-X".into(),
            firmware: "1.0.0".into(),
            hardware_id: "cam-x".into(),
            serial_number: "SN-001".into(),
        },
        port,
        device_ip.clone(),
    )
    .expect("explicit device identity"));
    for action in ["GetSystemDateAndTime", "GetDeviceInformation",
                   "GetCapabilities", "GetServices", "GetScopes"] {
        // pre-auth action per the ONVIF spec:
        if action == "GetSystemDateAndTime" {
            server.register_anonymous_action(action);
        }
        server.register_handler(action, Box::new(DeviceHandler(Arc::clone(&device))));
    }

    // Media service: tokens, encoding (H264/H265), stream & snapshot URIs
    // are all OnvifMediaConfig fields.
    let media = Arc::new(OnvifMediaConfig::new(1920, 1080, 25, 2_000_000, 8554, device_ip.clone()));
    server.register_handler("GetProfiles", Box::new(GetProfilesHandler::new(Arc::clone(&media))));
    server.register_handler("GetStreamUri", Box::new(GetStreamUriHandler::new(Arc::clone(&media))));
    server.register_handler("GetSnapshotUri", Box::new(GetSnapshotUriHandler::new(media)));

    // Imaging service via the register helper; PTZ likewise (see examples).
    register_imaging_actions(&mut server, Arc::new(MyParams));

    // Start both servers; the returned handles support graceful shutdown.
    let mut soap = server.start().await?;               // 0.0.0.0:port
    let mut discovery = DiscoveryServer::with_identity(&device_ip, port, "My Camera", "Model X")
        .start()
        .await?;                                        // udp/3702 multicast

    // ... run your app ...
    discovery.shutdown().await?;
    soap.shutdown().await?;
    Ok(())
}
```

`DiscoveryServer::with_uuid` pins the EndpointReference UUID so NVRs keying on it do not treat every restart as a new device. The crate logs through the [`log`](https://crates.io/crates/log) facade — initialize a logger in the host to see output.

See [`examples/device_demo.rs`](examples/device_demo.rs) for the complete wiring (it is also the compile-checked reference for README-style code), and the `mibee-eye-raspi-rs` `main.rs` for production wiring.

## Documentation

Topic guides now live in the MiBee documentation hub — the single
source of truth for library manuals, bilingual:

> **https://www.mlsbs.top/docs/mibeelibs**

Manual changes go there by PR (review flow in the hub repo's GOVERNANCE).
[`docs/README.md`](docs/README.md) keeps the redirect.
## Library hygiene (v0.3.0 hardening)

v0.3.0 made the crate safe to embed as a neutral foundation library. The regression tests in [`tests/library_hygiene.rs`](tests/library_hygiene.rs) and [`tests/server_lifecycle.rs`](tests/server_lifecycle.rs) pin each guarantee:

- **Fail-closed auth** — an empty password no longer silently disables authentication; `start()` errors unless `allow_no_auth = true` is set explicitly.
- **Neutral discovery identity** — `DiscoveryServer::new` advertises only the spec profile scope (the origin-hardware `PiCameraV1`/`OV5647` scopes are gone); use `with_identity`/`with_scopes`. Scope values are percent-encoded (space-safe).
- **Persistable UUID** — `with_uuid` keeps the discovery identity across restarts.
- **XML escaping** — client-controlled values (SOAP action names, preset names, Probe MessageIDs) and host-configured strings are escaped in responses.
- **Configurable media surface** — profile/source/encoder tokens, encoding (H.264 **or H.265**), and the GetVideoSources name come from `OnvifMediaConfig`.
- **Configurable imaging modes** — `ImagingParams::exposure_mode` / `white_balance_mode` (defaulted, back-compatible).
- **HTTP hardening** — multi-read header parsing (16 KiB cap), 1 MiB default body cap (413 on overflow, with a drain-before-close), per-connection read timeout (30 s default).
- **Graceful shutdown + listener injection** — `OnvifServerHandle`/`DiscoveryHandle` with `shutdown()`; `OnvifServer::start_on(listener)` accepts a pre-bound listener.
- **`log` facade** — no `println!`/`eprintln!` in library code; lock poisoning is recovered instead of cascading panics.

### Breaking changes, 0.2.x → 0.3.0

- `OnvifConfig` gained `allow_no_auth`, `max_body_bytes`, `read_timeout` (use `..Default::default()` in literals).
- `OnvifServer::start` returns `OnvifServerHandle` (was `()`); `DiscoveryServer::start` returns `DiscoveryHandle`.
- `DiscoveryServer::new` takes `&str` and no longer advertises name/hardware scopes by default.
- `GetVideoSources` name default is `Video Source` (was `Pi Camera`).
- Library output moved from stdout/stderr to the `log` facade.

## Examples

A runnable self-check demo lives in [`examples/`](examples/):

```sh
cargo run --example device_demo [-- --port 8080] [-- --serve]
```

It starts the real SOAP server + WS-Discovery responder, then drives them like a foreign ONVIF client: anonymous `GetSystemDateAndTime`, `GetDeviceInformation` rejected 401 without credentials and accepted with a WS-Security UsernameToken digest (computed by an independent SHA-1 in the demo), `GetCapabilities`, `GetProfiles`/`GetStreamUri`/`GetSnapshotUri`, and a WS-Discovery Probe over UDP. Exits 0 when every check passes — a no-hardware smoke test of the whole stack. `--serve` keeps the servers up for manual poking (curl / ONVIF Device Manager / an NVR).
Two service-focused demos follow the same pattern (real server, foreign-client checks, exit 0 on success):

```sh
cargo run --example ptz_demo      # move/status/preset verbs + simulated motion
cargo run --example imaging_demo  # get/set imaging params + range fault
```

## Byte stability guarantee

Consumers like the MiBee NVR match SOAP responses by local element names on the raw byte stream. The serialization in this crate is load-bearing: **do not change response element names, namespace prefixes, or attribute order** without re-running consumer interop tests. The crate's tests include golden response strings that pin this.

## Development

This project follows strict **TDD** — see [CONTRIBUTING.md](CONTRIBUTING.md). CI enforces `rustfmt`, `clippy -D warnings`, and the full test suite (157 tests incl. golden response strings); `main` is protected (PR-only merges, CI required).

## Status

v0.3.0 — seams (`ImagingParams`, `DeviceConfig`, media registration) are settling but not frozen. Production-tested daily at [Mi-Bee Studio](https://github.com/Mi-Bee-Studio) against the MiBee NVR.

## License

MIT — see [LICENSE](LICENSE). Extracted from Mi-Bee Studio camera projects.
