// ---------------------------------------------------------------------------
// ONVIF Device Service Handlers
// ---------------------------------------------------------------------------
//
// Implements the Device service (devicemgmt) action set as
// OnvifActionHandler trait objects: the reads (GetSystemDateAndTime,
// GetDeviceInformation, GetCapabilities, GetServices, GetScopes, …), the
// mutable stores (scopes, hostname, discovery mode, users), and the
// "protocol answer only" writes (SetSystemDateAndTime, factory default,
// firmware upgrade, restore, SystemReboot) that fire [`DeviceHooks`] and
// answer on the wire (issue #49).
//
// Deliberately **not** implemented (unknown action → ActionNotSupported
// fault; issue #49 scope decision): the Set* network variants
// (SetDNS/SetNTP/SetNetworkInterfaces/SetNetworkProtocols/
// SetNetworkDefaultGateway) — a library server has no business mutating
// the host's network stack — and the certificate / 802.1X families
// (LoadCertificates, GetCertificates, GetDot1XCapabilities, …) — there is
// no TLS client-cert surface here; that lands with future Profile T /
// TLS-server work. User management is a *directory* (see
// [`DeviceServiceHandlers::with_users`]) — the WS-Security layer stays
// the authentication source. Write actions are authenticated by default:
// they are only reachable pre-auth if the host explicitly registers them
// as anonymous actions.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Reader;
use quick_xml::Writer;

use crate::config::DeviceConfig;
use crate::namespaces::{
    DEVICE_SERVICE, EVENTS_SERVICE, IMAGING_SERVICE, MEDIA_SERVICE, PTZ_SERVICE, SCHEMAS,
};
use crate::server::OnvifActionHandler;
use crate::types::{resolve_server_ip, serialize_soap_response, OnvifError, RequestInfo};

// ---------------------------------------------------------------------------
// DeviceHooks — host-side effects for Device service write operations
// ---------------------------------------------------------------------------

/// Host-side effects for Device service write operations. Every method
/// has a default no-op — the protocol answer happens regardless; hosts
/// override to perform real side effects (issue #49).
///
/// The library itself never reboots, wipes, or re-clocks anything: it
/// parses the request, updates its protocol-visible state (where one
/// exists), fires the hook, and answers. Hosts that want a real effect
/// implement it here.
pub trait DeviceHooks: Send + Sync {
    /// SetSystemDateAndTime applied (UTC fields + timezone string).
    fn set_date_time(&self, _utc: (i32, i32, i32, i32, i32, i32), _tz: &str) {}
    /// SystemReboot confirmed by the host.
    fn reboot(&self) {}
    /// Factory default requested (hard = wipe config).
    fn factory_default(&self, _hard: bool) {}
    /// System log text for GetSystemLog.
    fn system_log(&self) -> String {
        String::new()
    }
    /// Support info text for GetSystemSupportInformation.
    fn support_info(&self) -> String {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// DeviceServiceHandlers — config holder with XML body builders
// ---------------------------------------------------------------------------

/// Holds device-level configuration needed by all device service handlers.
/// One scope in the mutable store (issue #65): the WSDL `tt:Scope` pair
/// — the item text plus whether the client may remove it (`Configurable`
/// scopes come from AddScopes/SetScopes; the built-ins report `Fixed`).
#[derive(Debug, Clone)]
struct ScopeEntry {
    item: String,
    configurable: bool,
}

impl ScopeEntry {
    fn fixed(item: &str) -> Self {
        Self {
            item: item.to_string(),
            configurable: false,
        }
    }
    fn configurable(item: &str) -> Self {
        Self {
            item: item.to_string(),
            configurable: true,
        }
    }
    fn scope_def(&self) -> &'static str {
        if self.configurable {
            "Configurable"
        } else {
            "Fixed"
        }
    }
}

pub struct DeviceServiceHandlers {
    device_config: DeviceConfig,
    onvif_port: u16,
    device_ip: String,
    /// Advertise the Media service in GetCapabilities/GetServices. Default
    /// `true` (the historical advertisement); hosts that do not register the
    /// media handlers turn it off so the advertisement matches the routes
    /// actually served (issue #47).
    support_media: bool,
    /// Advertise the PTZ service (see [`Self::with_media_support`]).
    support_ptz: bool,
    /// Advertise the Imaging service (see [`Self::with_media_support`]).
    support_imaging: bool,
    /// Advertise the Events service in GetCapabilities/GetServices (the
    /// host must also serve the routes — `OnvifServer::enable_events` on the
    /// SOAP side; parity with onvif-go's SupportEvents flagging both).
    support_events: bool,
    /// IP address filter seam (issue #54): the shared store behind the
    /// Get/Set/Add/RemoveIPAddressFilter ops. `None` (default) — Get
    /// answers a disabled filter, mutations are refused. Wire the same
    /// state into [`crate::server::OnvifServer::with_ip_filter`] for
    /// per-connection enforcement.
    ip_filter: Option<IpFilterState>,
    /// AccessPolicy seam (issue #54): opaque Base64-backed policy blob
    /// (Get/SetAccessPolicy). `None` (default) — Get answers an empty
    /// policy, Set is refused.
    access_policy: Option<AccessPolicyState>,
    /// Advertise the Media2 service (ver20/media) in GetServices only —
    /// the legacy GetCapabilities enumeration has no Media2 slot. Set
    /// together with `OnvifServer::enable_media2` so the advertisement
    /// and the served `/onvif/media2_service` route agree. Default
    /// `false` — the pre-Media2 GetServices bytes are unchanged.
    support_media2: bool,
    /// Host-side effects for write operations (issue #49); `None` = every
    /// hook is a no-op.
    host: Option<Arc<dyn DeviceHooks>>,
    /// Mutable scope directory (AddScopes/RemoveScopes/SetScopes; issue
    /// #49). Initialized with the three historical GetScopes items
    /// (`Fixed`); client-written entries report `Configurable` (issue
    /// #65: GetScopes answers the WSDL tt:Scope form).
    scopes: RwLock<Vec<ScopeEntry>>,
    /// Hostname set via SetHostname; `None` = report the startup-detected
    /// `device_ip` as the name (issue #49).
    hostname: RwLock<Option<String>>,
    /// Discovery mode ("Discoverable" | "NonDiscoverable"; issue #49).
    discovery_mode: RwLock<String>,
    /// User directory for GetUsers/CreateUsers/DeleteUsers/SetUser (issue
    /// #49): (username, UserLevel) pairs. **Not** an authentication
    /// source — the WS-Security layer owns real credentials; hosts that
    /// want ONVIF-side user administration to drive real auth map the
    /// store changes themselves (the directory never echoes passwords).
    users: RwLock<Vec<(String, String)>>,
}

impl DeviceServiceHandlers {
    /// Fail-fast on an unvalidated identity (issue #20): a config left at
    /// the neutral placeholders is a configuration error, not a silent
    /// "unknown" device on the network.
    pub fn new(
        device_config: DeviceConfig,
        onvif_port: u16,
        device_ip: String,
    ) -> Result<Self, OnvifError> {
        device_config
            .validate()
            .map_err(OnvifError::InvalidConfig)?;
        let scopes = vec![
            ScopeEntry::fixed("onvif://www.onvif.org/type/video_encoder"),
            ScopeEntry::fixed(&format!(
                "onvif://www.onvif.org/name/{}",
                device_config.name
            )),
            ScopeEntry::fixed(&format!(
                "onvif://www.onvif.org/hardware/{}",
                device_config.hardware_id
            )),
        ];
        Ok(Self {
            device_config,
            onvif_port,
            device_ip,
            support_media: true,
            support_ptz: true,
            support_imaging: true,
            support_events: false,
            ip_filter: None,
            access_policy: None,
            support_media2: false,
            host: None,
            scopes: RwLock::new(scopes),
            hostname: RwLock::new(None),
            discovery_mode: RwLock::new("Discoverable".to_string()),
            users: RwLock::new(Vec::new()),
        })
    }

    /// Advertise (or omit) the Media service in GetCapabilities /
    /// GetServices. Set to `false` when the host does not register the
    /// media action handlers, so clients discover only what is really
    /// served. Default `true` — the historical advertisement.
    #[must_use]
    pub fn with_media_support(mut self, support: bool) -> Self {
        self.support_media = support;
        self
    }

    /// Advertise (or omit) the PTZ service — see
    /// [`Self::with_media_support`]. Default `true`.
    #[must_use]
    pub fn with_ptz_support(mut self, support: bool) -> Self {
        self.support_ptz = support;
        self
    }

    /// Advertise (or omit) the Imaging service — see
    /// [`Self::with_media_support`]. Default `true`.
    #[must_use]
    pub fn with_imaging_support(mut self, support: bool) -> Self {
        self.support_imaging = support;
        self
    }

    /// Advertise (or omit) the Events service in GetCapabilities /
    /// GetServices. Set together with `OnvifServer::enable_events` so the
    /// advertisement and the served routes agree — with
    /// `WSPullPointSupport = true`, because the pull-point service really
    /// serves Create/Pull/Renew/Unsubscribe on the advertised XAddr.
    #[must_use]
    pub fn with_events_support(mut self, support: bool) -> Self {
        self.support_events = support;
        self
    }

    /// Advertise the Media2 service (ver20/media) in GetServices — the
    /// entry a Media2-discovering client probes for (GetCapabilities has
    /// no Media2 slot in the legacy enumeration, so this flag touches
    /// GetServices only). Set together with `OnvifServer::enable_media2`
    /// so the advertisement and the served `/onvif/media2_service` route
    /// agree. Default `false` — existing deployments' bytes unchanged.
    #[must_use]
    pub fn with_media2_support(mut self, support: bool) -> Self {
        self.support_media2 = support;
        self
    }

    /// Install host-side effects for Device service write operations
    /// (issue #49): clock sets, reboots, factory defaults, and the
    /// GetSystemLog / GetSystemSupportInformation texts. Without a hook
    /// every effect is a no-op and the two text actions answer empty —
    /// the protocol answers happen regardless.
    #[must_use]
    pub fn with_hooks(mut self, hooks: Arc<dyn DeviceHooks>) -> Self {
        self.host = Some(hooks);
        self
    }

    /// Seed the user directory `(username, UserLevel)` pairs exposed via
    /// GetUsers (issue #49). Fails on an empty username, a UserLevel
    /// outside {Administrator, Operator, User, Anonymous}, or duplicate
    /// usernames — the same invariants CreateUsers enforces. The default
    /// directory is empty: `DeviceConfig` carries no username, and the
    /// ONVIF WS-Security layer (the server's real auth source) is
    /// configured separately in `OnvifConfig`.
    pub fn with_users(mut self, users: Vec<(String, String)>) -> Result<Self, OnvifError> {
        validate_user_directory(&users).map_err(OnvifError::InvalidConfig)?;
        self.users = RwLock::new(users);
        Ok(self)
    }

    fn base_url(&self, server_ip: &str) -> String {
        let ip = resolve_server_ip(server_ip, &self.device_ip);
        format!("http://{ip}:{}/onvif", self.onvif_port)
    }
}

// ---------------------------------------------------------------------------
// DeviceHandler — dispatches to the right body builder and wraps in SOAP
// ---------------------------------------------------------------------------

/// Wraps `Arc<DeviceServiceHandlers>` and dispatches `handle()` to the
/// appropriate response builder based on the action element in `body`.
pub struct DeviceHandler(pub Arc<DeviceServiceHandlers>);

#[async_trait]
impl OnvifActionHandler for DeviceHandler {
    async fn handle(&self, body: &str, info: &RequestInfo) -> Result<String, OnvifError> {
        let svc = &self.0;
        // SUBSTRING DISPATCH ORDERING CONSTRAINT (issue #49):
        //
        // 1. Arms match on FULL action-name tokens only — no bare
        //    "Scopes"/"User"/"Network" prefixes — and among the arms below
        //    no token is a substring of another. Verify any new arm against
        //    every existing one (e.g. "GetServiceCapabilities" must NOT be
        //    added as "Capabilities"; "GetNetworkProtocols" as "Network").
        //
        // 2. Within a family, WRITE verbs come before the matching read:
        //    request *text* is client-controlled, so a SetScopes body whose
        //    scope item literally reads "GetScopes", or a CreateUsers body
        //    with username "GetUsers", must still route to the write verb.
        //    (The reverse — a Get body mentioning a write token — cannot
        //    occur: read bodies carry no such text.)
        //
        // 3. Otherwise longest/most-specific name first.
        //
        // `test_all_device_actions_route_to_own_response` pins this table;
        // extend it whenever an arm is added.
        let fragment = if body.contains("SetSystemDateAndTime") {
            svc.handle_set_system_date_time(body)?
        } else if body.contains("GetSystemDateAndTime") {
            svc.build_system_date_time()
        } else if body.contains("GetDeviceInformation") {
            svc.build_device_information()
        } else if body.contains("GetServiceCapabilities") {
            // The Device service's own capabilities query (the WSDL action
            // is GetServiceCapabilities; the response carries the
            // DeviceServiceCapabilities type). Reached via the imaging/PTZ
            // shared-name fallback when those handlers own the action name
            // (issue #52 routing): a body without VideoSourceToken /
            // timg: / PTZ markers lands here.
            svc.build_device_service_capabilities()
        } else if body.contains("GetCapabilities") {
            svc.build_capabilities(&info.server_ip)
        } else if body.contains("GetServices") {
            svc.build_services(&info.server_ip)
        } else if body.contains("SetScopes") {
            svc.handle_set_scopes(body)?
        } else if body.contains("AddScopes") {
            svc.handle_add_scopes(body)?
        } else if body.contains("RemoveScopes") {
            svc.handle_remove_scopes(body)?
        } else if body.contains("GetScopes") {
            svc.build_scopes()
        } else if body.contains("SetHostname") {
            svc.handle_set_hostname(body)?
        } else if body.contains("GetHostname") {
            svc.build_hostname()
        } else if body.contains("GetNetworkDefaultGateway") {
            svc.build_network_default_gateway()
        } else if body.contains("GetNetworkInterfaces") {
            svc.build_network_interfaces()
        } else if body.contains("GetNetworkProtocols") {
            svc.build_network_protocols()
        } else if body.contains("GetDNS") {
            svc.build_dns()
        } else if body.contains("GetNTP") {
            svc.build_ntp()
        } else if body.contains("SetDiscoveryMode") {
            svc.handle_set_discovery_mode(body)?
        } else if body.contains("GetDiscoveryMode") {
            svc.build_discovery_mode()
        } else if body.contains("CreateUsers") {
            svc.handle_create_users(body)?
        } else if body.contains("DeleteUsers") {
            svc.handle_delete_users(body)?
        } else if body.contains("SetUser") {
            svc.handle_set_user(body)?
        } else if body.contains("GetUsers") {
            svc.build_users()
        } else if body.contains("GetWsdlUrl") {
            svc.build_wsdl_url(&info.server_ip)
        } else if body.contains("GetEndpointReference") {
            svc.build_endpoint_reference()
        } else if body.contains("GetSystemSupportInformation") {
            svc.build_support_information()
        } else if body.contains("GetSystemLog") {
            svc.build_system_log()
        } else if body.contains("SetSystemFactoryDefault") {
            svc.handle_set_system_factory_default(body)?
        } else if body.contains("UpgradeSystemFirmware") {
            svc.build_upgrade_system_firmware_ack()
        } else if body.contains("StartSystemRestore") {
            svc.build_start_system_restore(&info.server_ip)
        } else if body.contains("SystemReboot") {
            svc.handle_system_reboot()
        } else if body.contains("GetIPAddressFilter") {
            svc.sec_build_get_ip_address_filter()
        } else if body.contains("SetIPAddressFilter") {
            svc.sec_apply_set_ip_address_filter(body)?
        } else if body.contains("AddIPAddressFilter") {
            svc.sec_apply_add_ip_address_filter(body)?
        } else if body.contains("RemoveIPAddressFilter") {
            svc.sec_apply_remove_ip_address_filter(body)?
        } else if body.contains("GetAccessPolicy") {
            svc.sec_build_get_access_policy()
        } else if body.contains("SetAccessPolicy") {
            svc.sec_apply_set_access_policy(body)?
        } else {
            return Err(OnvifError::ActionNotSupported(
                "unknown device action".into(),
            ));
        };
        Ok(serialize_soap_response(&fragment))
    }
}

// ---------------------------------------------------------------------------
// XML response body builders (return just the <tds:GetXxxResponse> fragment)
// ---------------------------------------------------------------------------

impl DeviceServiceHandlers {
    /// Build `<tds:GetSystemDateAndTimeResponse>` body.
    fn build_system_date_time(&self) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let (year, month, day, hour, minute, second) = secs_to_utc(secs);

        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

        let mut root = BytesStart::new("tds:GetSystemDateAndTimeResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();

        w.write_event(Event::Start(BytesStart::new("tds:SystemDateAndTime")))
            .unwrap_or_default();
        write_text(&mut w, "tt:DateTimeType", "Manual");
        write_text(&mut w, "tt:DaylightSavings", "false");
        open_close(&mut w, "tt:TimeZone", |w| {
            write_text(w, "tt:TZ", "UTC");
        });
        open_close(&mut w, "tt:UTCDateTime", |w| {
            open_close(w, "tt:Time", |w| {
                write_int(w, "tt:Hour", hour);
                write_int(w, "tt:Minute", minute);
                write_int(w, "tt:Second", second);
            });
            open_close(w, "tt:Date", |w| {
                write_int(w, "tt:Year", year);
                write_int(w, "tt:Month", month);
                write_int(w, "tt:Day", day);
            });
        });
        w.write_event(Event::End(BytesEnd::new("tds:SystemDateAndTime")))
            .unwrap_or_default();
        w.write_event(Event::End(BytesEnd::new(
            "tds:GetSystemDateAndTimeResponse",
        )))
        .unwrap_or_default();

        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// Build `<tds:GetDeviceInformationResponse>` body.
    fn build_device_information(&self) -> String {
        let cfg = &self.device_config;
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

        let mut root = BytesStart::new("tds:GetDeviceInformationResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        w.write_event(Event::Start(root)).unwrap_or_default();

        write_text(&mut w, "tds:Manufacturer", &cfg.manufacturer);
        write_text(&mut w, "tds:Model", &cfg.model);
        write_text(&mut w, "tds:FirmwareVersion", &cfg.firmware);
        write_text(&mut w, "tds:SerialNumber", &cfg.serial_number);
        write_text(&mut w, "tds:HardwareId", &cfg.hardware_id);

        w.write_event(Event::End(BytesEnd::new(
            "tds:GetDeviceInformationResponse",
        )))
        .unwrap_or_default();

        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// Build `<tds:GetCapabilitiesResponse>` body.
    fn build_capabilities(&self, server_ip: &str) -> String {
        let base = self.base_url(server_ip);
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

        let mut root = BytesStart::new("tds:GetCapabilitiesResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();

        w.write_event(Event::Start(BytesStart::new("tds:Capabilities")))
            .unwrap_or_default();
        capability_xaddr(&mut w, "tt:Device", &base, "/device_service");
        if self.support_media {
            capability_xaddr(&mut w, "tt:Media", &base, "/media_service");
        }
        if self.support_ptz {
            capability_xaddr(&mut w, "tt:PTZ", &base, "/ptz_service");
        }
        if self.support_imaging {
            capability_xaddr(&mut w, "tt:Imaging", &base, "/device_service");
        }
        if self.support_events {
            // WSPullPointSupport=true: the events service really serves
            // CreatePullPointSubscription/PullMessages/Renew/Unsubscribe on
            // the advertised XAddr (parity with onvif-go, including its
            // spec-divergent WSPausableSubscriptionManagerInterfaceSupport
            // attribute name — twin wire parity wins over the spec text).
            let mut events = BytesStart::new("tt:Events");
            events.push_attribute(("WSSubscriptionPolicySupport", "false"));
            events.push_attribute(("WSPullPointSupport", "true"));
            events.push_attribute(("WSPausableSubscriptionManagerInterfaceSupport", "false"));
            w.write_event(Event::Start(events)).unwrap_or_default();
            write_text(&mut w, "tt:XAddr", &format!("{base}/events_service"));
            w.write_event(Event::End(BytesEnd::new("tt:Events")))
                .unwrap_or_default();
        }
        w.write_event(Event::End(BytesEnd::new("tds:Capabilities")))
            .unwrap_or_default();

        w.write_event(Event::End(BytesEnd::new("tds:GetCapabilitiesResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// Build `<tds:GetServicesResponse>` body.
    ///
    /// Enumerates the services this host actually serves (issue #47):
    /// Device always; Media/PTZ/Imaging per their support flags; Events
    /// only after [`Self::with_events_support`]. XAddr paths match the
    /// GetCapabilities advertisement.
    fn build_services(&self, server_ip: &str) -> String {
        let base = self.base_url(server_ip);
        let mut services: Vec<(&str, &str)> = vec![(DEVICE_SERVICE, "/device_service")];
        if self.support_media {
            services.push((MEDIA_SERVICE, "/media_service"));
        }
        if self.support_ptz {
            services.push((PTZ_SERVICE, "/ptz_service"));
        }
        if self.support_imaging {
            services.push((IMAGING_SERVICE, "/device_service"));
        }
        if self.support_events {
            services.push((EVENTS_SERVICE, "/events_service"));
        }
        // Media2 (ver20/media): GetServices-only advertisement — the
        // legacy GetCapabilities enumeration has no slot for it. The
        // entry rides the same writer loop as every other service, so
        // the Version block matches the existing entries' shape.
        if self.support_media2 {
            services.push((crate::media2::MEDIA2_SERVICE, "/media2_service"));
        }

        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

        let mut root = BytesStart::new("tds:GetServicesResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();

        for (ns, path) in services {
            w.write_event(Event::Start(BytesStart::new("tds:Service")))
                .unwrap_or_default();
            write_text(&mut w, "tds:Namespace", ns);
            write_text(&mut w, "tds:XAddr", &format!("{}{}", base, path));
            open_close(&mut w, "tds:Version", |w| {
                write_int(w, "tt:Major", 1);
                write_int(w, "tt:Minor", 0);
            });
            w.write_event(Event::End(BytesEnd::new("tds:Service")))
                .unwrap_or_default();
        }

        w.write_event(Event::End(BytesEnd::new("tds:GetServicesResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// Build `<tds:GetScopesResponse>` body.
    ///
    /// Reads the mutable scope store (issue #49): until a write happens,
    /// the store holds exactly the three historical items, so the bytes
    /// are identical to the pre-store implementation.
    fn build_scopes(&self) -> String {
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

        let mut root = BytesStart::new("tds:GetScopesResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();

        // `write_text` escapes via BytesText::new — client-added scope
        // texts cannot break the XML. WSDL form (issue #65): each scope
        // is a tds:Scopes element of type tt:Scope (ScopeDef + ScopeItem).
        for entry in store_read(&self.scopes).iter() {
            w.write_event(Event::Start(BytesStart::new("tds:Scopes")))
                .unwrap_or_default();
            write_text(&mut w, "tt:ScopeDef", entry.scope_def());
            write_text(&mut w, "tt:ScopeItem", &entry.item);
            w.write_event(Event::End(BytesEnd::new("tds:Scopes")))
                .unwrap_or_default();
        }

        w.write_event(Event::End(BytesEnd::new("tds:GetScopesResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// Build `<tds:SystemRebootResponse>` body.
    ///
    /// **Protocol answer only** — this library never performs the reboot
    /// side effect. The ONVIF Device service defines SystemReboot as
    /// "reboot the device"; here it is a byte-stable wire answer (WSDL
    /// `SystemRebootResponse/Message`, parity with onvif-go's
    /// `HandleSystemReboot` returning "Device rebooting"). Hosts that want
    /// a real reboot implement [`DeviceHooks::reboot`] (issue #49) or
    /// observe the action in their handler wrapping layer; keep this
    /// action authenticated (it is not in the anonymous set by default),
    /// matching onvif-go which treats SystemReboot as a write-style
    /// credential-protected action.
    fn build_system_reboot(&self) -> String {
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

        let mut root = BytesStart::new("tds:SystemRebootResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        w.write_event(Event::Start(root)).unwrap_or_default();

        write_text(&mut w, "tds:Message", "Device rebooting");

        w.write_event(Event::End(BytesEnd::new("tds:SystemRebootResponse")))
            .unwrap_or_default();

        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// SystemReboot entry point: fire [`DeviceHooks::reboot`] (no-op
    /// without a hook), then answer with the byte-stable body (issue #49).
    fn handle_system_reboot(&self) -> String {
        if let Some(hooks) = &self.host {
            hooks.reboot();
        }
        self.build_system_reboot()
    }

    // -------------------------------------------------------------------
    // Issue #49 — clock, scopes, hostname, network, discovery, users,
    // capabilities, system info, factory default / firmware / restore
    // -------------------------------------------------------------------

    /// SetSystemDateAndTime (issue #49): parse, fire
    /// [`DeviceHooks::set_date_time`], ack. The library does not touch the
    /// system clock — applying the time is the host's effect.
    fn handle_set_system_date_time(&self, body: &str) -> Result<String, OnvifError> {
        let parsed = parse_set_system_date_time(body)
            .map_err(|m| OnvifError::SenderFault(format!("SetSystemDateAndTime: {m}")))?;
        if let Some(hooks) = &self.host {
            hooks.set_date_time(parsed.utc, &parsed.tz);
        }
        Ok(build_empty_response("tds:SetSystemDateAndTimeResponse"))
    }

    /// SetScopes (issue #49): replace the whole scope list. The WSDL
    /// requires at least one scope (minOccurs=1) — an empty list is a
    /// sender fault.
    fn handle_set_scopes(&self, body: &str) -> Result<String, OnvifError> {
        let items = parse_scope_items(body)
            .map_err(|m| OnvifError::SenderFault(format!("SetScopes: {m}")))?;
        if items.is_empty() {
            return Err(OnvifError::SenderFault(
                "SetScopes requires at least one scope".into(),
            ));
        }
        *store_write(&self.scopes) = items.iter().map(|i| ScopeEntry::configurable(i)).collect();
        Ok(build_empty_response("tds:SetScopesResponse"))
    }

    /// AddScopes (issue #49): append new scope items, skipping ones
    /// already present.
    fn handle_add_scopes(&self, body: &str) -> Result<String, OnvifError> {
        let items = parse_scope_items(body)
            .map_err(|m| OnvifError::SenderFault(format!("AddScopes: {m}")))?;
        if items.is_empty() {
            return Err(OnvifError::SenderFault(
                "AddScopes requires at least one ScopeItem".into(),
            ));
        }
        let mut store = store_write(&self.scopes);
        for item in items {
            if !store.iter().any(|e| e.item == item) {
                store.push(ScopeEntry::configurable(&item));
            }
        }
        Ok(build_empty_response("tds:AddScopesResponse"))
    }

    /// RemoveScopes (issue #49): remove the matching items. Items not in
    /// the store are ignored (the WSDL's ScopeItem echo in the response is
    /// documented as deprecated; minOccurs=0 makes the empty ack valid).
    /// Unlike the spec's fixed/configurable split, every listed scope —
    /// including the built-ins — is removable: the library has no
    /// hardware-derived scope it must protect.
    fn handle_remove_scopes(&self, body: &str) -> Result<String, OnvifError> {
        let items = parse_scope_items(body)
            .map_err(|m| OnvifError::SenderFault(format!("RemoveScopes: {m}")))?;
        if items.is_empty() {
            return Err(OnvifError::SenderFault(
                "RemoveScopes requires at least one ScopeItem".into(),
            ));
        }
        store_write(&self.scopes).retain(|e| !items.contains(&e.item));
        Ok(build_empty_response("tds:RemoveScopesResponse"))
    }

    /// GetHostname (issue #49): WSDL `HostnameInformation` — FromDHCP
    /// always false (the library never obtains a name from DHCP), Name
    /// from SetHostname or the startup-detected device IP.
    fn build_hostname(&self) -> String {
        let name = store_read(&self.hostname)
            .clone()
            .unwrap_or_else(|| self.device_ip.clone());
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:GetHostnameResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();
        open_close(&mut w, "tds:HostnameInformation", |w| {
            write_text(w, "tt:FromDHCP", "false");
            write_text(w, "tt:Name", &name);
        });
        w.write_event(Event::End(BytesEnd::new("tds:GetHostnameResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// SetHostname (issue #49): store the name for GetHostname. The
    /// library does not change the host's actual hostname — hosts that
    /// want that effect wrap it themselves (no hook: the wire state is
    /// the only library-visible effect).
    fn handle_set_hostname(&self, body: &str) -> Result<String, OnvifError> {
        let name = element_text(body, &["Name"])
            .map_err(|m| OnvifError::SenderFault(format!("SetHostname: {m}")))?;
        let Some(name) = name else {
            return Err(OnvifError::SenderFault(
                "SetHostname requires a Name element".into(),
            ));
        };
        if name.is_empty() {
            return Err(OnvifError::SenderFault(
                "SetHostname Name must not be empty".into(),
            ));
        }
        *store_write(&self.hostname) = Some(name);
        Ok(build_empty_response("tds:SetHostnameResponse"))
    }

    /// GetNetworkInterfaces (issue #49): empty response. A library
    /// context has no configurable network-interface abstraction to
    /// expose (the WSDL's minOccurs=1 NetworkInterfaces entry is relaxed
    /// deliberately; clients iterate zero interfaces).
    fn build_network_interfaces(&self) -> String {
        build_empty_response("tds:GetNetworkInterfacesResponse")
    }

    /// GetNetworkDefaultGateway (issue #49): honest static — the default
    /// route belongs to the host OS, not this library.
    fn build_network_default_gateway(&self) -> String {
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:GetNetworkDefaultGatewayResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();
        open_close(&mut w, "tds:NetworkGateway", |w| {
            write_text(w, "tt:IPv4Address", "0.0.0.0");
        });
        w.write_event(Event::End(BytesEnd::new(
            "tds:GetNetworkDefaultGatewayResponse",
        )))
        .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// GetDNS (issue #49): honest static — no DHCP-derived or manual DNS
    /// list in a library context.
    fn build_dns(&self) -> String {
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:GetDNSResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();
        open_close(&mut w, "tds:DNSInformation", |w| {
            write_text(w, "tt:FromDHCP", "false");
        });
        w.write_event(Event::End(BytesEnd::new("tds:GetDNSResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// GetNTP (issue #49): honest static — see [`Self::build_dns`].
    fn build_ntp(&self) -> String {
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:GetNTPResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();
        open_close(&mut w, "tds:NTPInformation", |w| {
            write_text(w, "tt:FromDHCP", "false");
        });
        w.write_event(Event::End(BytesEnd::new("tds:GetNTPResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// GetNetworkProtocols (issue #49): the one protocol this server
    /// really serves — HTTP(S) on the ONVIF port. RTSP is served by the
    /// host's media stack, not this SOAP server, so it is not listed.
    fn build_network_protocols(&self) -> String {
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:GetNetworkProtocolsResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();
        open_close(&mut w, "tds:NetworkProtocols", |w| {
            write_text(w, "tt:Name", "HTTP");
            write_text(w, "tt:Enabled", "true");
            write_int(w, "tt:Port", i32::from(self.onvif_port));
        });
        w.write_event(Event::End(BytesEnd::new("tds:GetNetworkProtocolsResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// GetDiscoveryMode (issue #49): reflects the SetDiscoveryMode store.
    fn build_discovery_mode(&self) -> String {
        let mode = store_read(&self.discovery_mode).clone();
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:GetDiscoveryModeResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        w.write_event(Event::Start(root)).unwrap_or_default();
        write_text(&mut w, "tds:DiscoveryMode", &mode);
        w.write_event(Event::End(BytesEnd::new("tds:GetDiscoveryModeResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// SetDiscoveryMode (issue #49): store the mode. The WS-Discovery
    /// responder keeps running regardless — halting discovery is a
    /// host-level effect.
    fn handle_set_discovery_mode(&self, body: &str) -> Result<String, OnvifError> {
        let mode = element_text(body, &["DiscoveryMode"])
            .map_err(|m| OnvifError::SenderFault(format!("SetDiscoveryMode: {m}")))?;
        match mode.as_deref() {
            Some(m @ ("Discoverable" | "NonDiscoverable")) => {
                *store_write(&self.discovery_mode) = m.to_string();
            }
            _ => {
                return Err(OnvifError::SenderFault(
                    "SetDiscoveryMode requires DiscoveryMode to be Discoverable or NonDiscoverable"
                        .into(),
                ));
            }
        }
        Ok(build_empty_response("tds:SetDiscoveryModeResponse"))
    }

    /// GetUsers (issue #49): the user directory. Passwords are never
    /// serialized — the WS-Security layer (OnvifConfig credentials) is
    /// the real authentication source; this directory only describes the
    /// users the Device service administers.
    fn build_users(&self) -> String {
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:GetUsersResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();
        for (username, level) in store_read(&self.users).iter() {
            open_close(&mut w, "tds:User", |w| {
                write_text(w, "tt:Username", username);
                write_text(w, "tt:UserLevel", level);
            });
        }
        w.write_event(Event::End(BytesEnd::new("tds:GetUsersResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// CreateUsers (issue #49): validate every entry, then append —
    /// all-or-nothing per the WSDL. Passwords are validated (present,
    /// non-empty — the WSDL's "Too weak password" fault for a missing
    /// password) but not stored: the directory is not the auth source.
    fn handle_create_users(&self, body: &str) -> Result<String, OnvifError> {
        let entries = parse_user_entries(body)
            .map_err(|m| OnvifError::SenderFault(format!("CreateUsers: {m}")))?;
        if entries.is_empty() {
            return Err(OnvifError::SenderFault(
                "CreateUsers requires at least one User".into(),
            ));
        }
        let mut new_pairs = Vec::new();
        for entry in &entries {
            let pair = validate_new_user(entry)
                .map_err(|m| OnvifError::SenderFault(format!("CreateUsers: {m}")))?;
            if new_pairs
                .iter()
                .any(|(u, _): &(String, String)| *u == pair.0)
            {
                return Err(OnvifError::SenderFault(format!(
                    "CreateUsers: duplicate username in request: {}",
                    pair.0
                )));
            }
            new_pairs.push(pair);
        }
        // All-or-nothing (WSDL): every entry validated before the store
        // gains anything — duplicates against the store included.
        let mut store = store_write(&self.users);
        for (username, _) in &new_pairs {
            if store.iter().any(|(u, _)| u == username) {
                return Err(OnvifError::SenderFault(format!(
                    "CreateUsers: username already exists: {username}"
                )));
            }
        }
        store.extend(new_pairs);
        Ok(build_empty_response("tds:CreateUsersResponse"))
    }

    /// DeleteUsers (issue #49): remove by username; a missing name is a
    /// sender fault (all-or-nothing per the WSDL).
    fn handle_delete_users(&self, body: &str) -> Result<String, OnvifError> {
        let names = parse_username_list(body)
            .map_err(|m| OnvifError::SenderFault(format!("DeleteUsers: {m}")))?;
        if names.is_empty() {
            return Err(OnvifError::SenderFault(
                "DeleteUsers requires at least one Username".into(),
            ));
        }
        let mut store = store_write(&self.users);
        for name in &names {
            if !store.iter().any(|(u, _)| u == name) {
                return Err(OnvifError::SenderFault(format!(
                    "DeleteUsers: no such user: {name}"
                )));
            }
        }
        store.retain(|(u, _)| !names.contains(u));
        Ok(build_empty_response("tds:DeleteUsersResponse"))
    }

    /// SetUser (issue #49): update the UserLevel of an existing user
    /// (username required; the password is accepted but not stored —
    /// real credentials live in the WS-Security layer). A missing user is
    /// a sender fault.
    fn handle_set_user(&self, body: &str) -> Result<String, OnvifError> {
        let entries = parse_user_entries(body)
            .map_err(|m| OnvifError::SenderFault(format!("SetUser: {m}")))?;
        if entries.is_empty() {
            return Err(OnvifError::SenderFault(
                "SetUser requires at least one User".into(),
            ));
        }
        // All-or-nothing (WSDL): validate every entry (level, user
        // exists) before applying the first update.
        let mut store = store_write(&self.users);
        let mut updates = Vec::new();
        for entry in &entries {
            let Some(username) = &entry.username else {
                return Err(OnvifError::SenderFault(
                    "SetUser: User requires a Username".into(),
                ));
            };
            let Some(level) = &entry.user_level else {
                return Err(OnvifError::SenderFault(format!(
                    "SetUser: nothing to update for {username} — UserLevel is required"
                )));
            };
            validate_user_level(level)
                .map_err(|m| OnvifError::SenderFault(format!("SetUser: {m}")))?;
            if !store.iter().any(|(u, _)| u == username) {
                return Err(OnvifError::SenderFault(format!(
                    "SetUser: no such user: {username}"
                )));
            }
            updates.push((username.clone(), level.clone()));
        }
        for (username, level) in updates {
            if let Some(slot) = store.iter_mut().find(|(u, _)| *u == username) {
                slot.1 = level;
            }
        }
        Ok(build_empty_response("tds:SetUserResponse"))
    }

    /// GetServiceCapabilities (Device service, issue #49): WSDL
    /// `DeviceServiceCapabilities` — the required Network/Security/System
    /// elements with every flag at its default (false/absent): the honest
    /// minimal answer for a library that supports no IP filtering, no
    /// Dot1X, no DHCP hostname, no NTP configuration surface.
    fn build_device_service_capabilities(&self) -> String {
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:GetServiceCapabilitiesResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        w.write_event(Event::Start(root)).unwrap_or_default();
        open_close(&mut w, "tds:Capabilities", |w| {
            w.write_event(Event::Empty(BytesStart::new("tds:Network")))
                .unwrap_or_default();
            w.write_event(Event::Empty(BytesStart::new("tds:Security")))
                .unwrap_or_default();
            w.write_event(Event::Empty(BytesStart::new("tds:System")))
                .unwrap_or_default();
        });
        w.write_event(Event::End(BytesEnd::new(
            "tds:GetServiceCapabilitiesResponse",
        )))
        .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// GetWsdlUrl (issue #49): the conventional WSDL URL on this server.
    /// The library serves no actual WSDL document — the URL shape is what
    /// ONVIF clients expect to probe.
    fn build_wsdl_url(&self, server_ip: &str) -> String {
        let ip = resolve_server_ip(server_ip, &self.device_ip);
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:GetWsdlUrlResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        w.write_event(Event::Start(root)).unwrap_or_default();
        write_text(
            &mut w,
            "tds:WsdlUrl",
            &format!("http://{ip}:{}/onvif/device_service?wsdl", self.onvif_port),
        );
        w.write_event(Event::End(BytesEnd::new("tds:GetWsdlUrlResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// GetEndpointReference (issue #49): static placeholder GUID — the
    /// library has no per-instance persistent identity to derive one
    /// from. Hosts needing a real endpoint reference intercept the action
    /// in their wrapping layer (the zero UUID is the documented library
    /// answer).
    fn build_endpoint_reference(&self) -> String {
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:GetEndpointReferenceResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        w.write_event(Event::Start(root)).unwrap_or_default();
        write_text(
            &mut w,
            "tds:GUID",
            "urn:uuid:00000000-0000-0000-0000-000000000000",
        );
        w.write_event(Event::End(BytesEnd::new(
            "tds:GetEndpointReferenceResponse",
        )))
        .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// GetSystemLog (issue #49): WSDL `SystemLog` with the `tt:String`
    /// text from [`DeviceHooks::system_log`] (the `tt:Binary` variant is
    /// not used — hosts provide text logs). The LogType request element
    /// is tolerated but not distinguished.
    fn build_system_log(&self) -> String {
        let text = self
            .host
            .as_ref()
            .map_or_else(String::new, |h| h.system_log());
        self.build_text_report("tds:GetSystemLogResponse", "tds:SystemLog", &text)
    }

    /// GetSystemSupportInformation (issue #49): same shape as
    /// [`Self::build_system_log`] with the
    /// [`DeviceHooks::support_info`] text.
    fn build_support_information(&self) -> String {
        let text = self
            .host
            .as_ref()
            .map_or_else(String::new, |h| h.support_info());
        self.build_text_report(
            "tds:GetSystemSupportInformationResponse",
            "tds:SupportInformation",
            &text,
        )
    }

    /// Shared shape of GetSystemLog/GetSystemSupportInformation:
    /// `<root><wrapper><tt:String>text</tt:String></wrapper></root>`.
    fn build_text_report(&self, root_name: &str, wrapper: &str, text: &str) -> String {
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new(root_name);
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();
        open_close(&mut w, wrapper, |w| {
            write_text(w, "tt:String", text);
        });
        w.write_event(Event::End(BytesEnd::new(root_name)))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// SetSystemFactoryDefault (issue #49): parse Hard|Soft, fire
    /// [`DeviceHooks::factory_default`], ack. The library wipes nothing.
    fn handle_set_system_factory_default(&self, body: &str) -> Result<String, OnvifError> {
        let kind = element_text(body, &["FactoryDefault"])
            .map_err(|m| OnvifError::SenderFault(format!("SetSystemFactoryDefault: {m}")))?;
        let hard = match kind.as_deref() {
            Some("Hard") => true,
            Some("Soft") => false,
            _ => {
                return Err(OnvifError::SenderFault(
                    "SetSystemFactoryDefault requires FactoryDefaultType to be Hard or Soft".into(),
                ));
            }
        };
        if let Some(hooks) = &self.host {
            hooks.factory_default(hard);
        }
        Ok(build_empty_response("tds:SetSystemFactoryDefaultResponse"))
    }

    /// UpgradeSystemFirmware (issue #49): **protocol answer only** — the
    /// firmware payload is parsed past, no hook exists, nothing is
    /// flashed (the SystemReboot philosophy: the wire answer is stable,
    /// the side effect is the host's decision).
    fn build_upgrade_system_firmware_ack(&self) -> String {
        // (WSDL: UpgradeSystemFirmwareResponse/Message is optional — an
        // empty response is schema-valid.)
        build_empty_response("tds:UpgradeSystemFirmwareResponse")
    }

    /// StartSystemRestore (issue #49): **protocol answer only** — the
    /// WSDL-required UploadUri/ExpectedDownTime pair is answered with the
    /// device service URL and zero downtime, but no restore-upload
    /// endpoint exists in this library; hosts do not act on it.
    fn build_start_system_restore(&self, server_ip: &str) -> String {
        let ip = resolve_server_ip(server_ip, &self.device_ip);
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:StartSystemRestoreResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        w.write_event(Event::Start(root)).unwrap_or_default();
        write_text(
            &mut w,
            "tds:UploadUri",
            &format!("http://{ip}:{}/onvif/device_service", self.onvif_port),
        );
        write_text(&mut w, "tds:ExpectedDownTime", "PT0S");
        w.write_event(Event::End(BytesEnd::new("tds:StartSystemRestoreResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn write_text(w: &mut Writer<Vec<u8>>, name: &str, text: &str) {
    w.write_event(Event::Start(BytesStart::new(name)))
        .unwrap_or_default();
    w.write_event(Event::Text(BytesText::new(text)))
        .unwrap_or_default();
    w.write_event(Event::End(BytesEnd::new(name)))
        .unwrap_or_default();
}

fn write_int(w: &mut Writer<Vec<u8>>, name: &str, value: i32) {
    write_text(w, name, &value.to_string());
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

/// Write `<name xmlns:tds="…"/>` — the empty (ack-only) response shape
/// shared by the Device write actions (issue #49).
fn build_empty_response(name: &str) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    let mut root = BytesStart::new(name);
    root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
    w.write_event(Event::Empty(root)).unwrap_or_default();
    String::from_utf8(w.into_inner()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Poisoning-tolerant store locks (the ptz_state.rs pattern, #15): the
// stores are plain data with no invariants, so a panicked peer thread's
// guard is still consistent to read/write.
// ---------------------------------------------------------------------------

fn store_write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn store_read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

// ---------------------------------------------------------------------------
// User directory validation (issue #49)
// ---------------------------------------------------------------------------

/// UserLevel values accepted by the Device service (the tt:UserLevel
/// enumeration minus `Extended`, which has no meaning for this library's
/// directory).
const USER_LEVELS: [&str; 4] = ["Administrator", "Operator", "User", "Anonymous"];

fn validate_user_level(level: &str) -> Result<(), String> {
    if USER_LEVELS.contains(&level) {
        Ok(())
    } else {
        Err(format!(
            "invalid UserLevel {level:?} — must be one of {USER_LEVELS:?}"
        ))
    }
}

fn validate_user_directory(users: &[(String, String)]) -> Result<(), String> {
    for (username, level) in users {
        if username.is_empty() {
            return Err("user directory entry with empty username".into());
        }
        validate_user_level(level)?;
    }
    for i in 0..users.len() {
        if users[i + 1..].iter().any(|(u, _)| u == &users[i].0) {
            return Err(format!(
                "duplicate username in user directory: {}",
                users[i].0
            ));
        }
    }
    Ok(())
}

/// One parsed `User` entry (any field may be missing — the caller
/// validates per-action semantics).
#[derive(Default)]
struct ParsedUserEntry {
    username: Option<String>,
    password: Option<String>,
    user_level: Option<String>,
}

/// Validate a CreateUsers entry: username non-empty, password present
/// (the WSDL's "Too weak password" fault for a missing one), level known.
fn validate_new_user(entry: &ParsedUserEntry) -> Result<(String, String), String> {
    let Some(username) = &entry.username else {
        return Err("User requires a Username".into());
    };
    if username.is_empty() {
        return Err("Username must not be empty".into());
    }
    if entry.password.as_deref().unwrap_or("").is_empty() {
        return Err(format!(
            "Too weak password — a non-empty Password is required for {username}"
        ));
    }
    let Some(level) = &entry.user_level else {
        return Err(format!("User {username} requires a UserLevel"));
    };
    validate_user_level(level)?;
    Ok((username.clone(), level.clone()))
}

// ---------------------------------------------------------------------------
// Tolerant request parsing (issue #49) — local-name matching, entity
// resolution, never panics on arbitrary input
// ---------------------------------------------------------------------------

/// Walk every contiguous text region of `body` and invoke `visit` with
/// the stack of open element local names (outermost first) and the
/// region's resolved text. Namespace-prefix tolerant; malformed XML
/// yields `Err`. The media.rs `parse_profile_token` philosophy applied to
/// structured bodies.
fn walk_element_texts<F>(body: &str, mut visit: F) -> Result<(), String>
where
    F: FnMut(&[String], &str),
{
    fn flush<F: FnMut(&[String], &str)>(
        acc: &mut crate::types::TextAccumulator,
        stack: &[String],
        visit: &mut F,
    ) {
        if let Some(text) = acc.flush() {
            if !text.is_empty() {
                visit(stack, &text);
            }
        }
    }

    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut acc = crate::types::TextAccumulator::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                flush(&mut acc, &stack, &mut visit);
                let local = local_name(e.name().as_ref()).to_string();
                stack.push(local);
            }
            Ok(Event::End(e)) => {
                flush(&mut acc, &stack, &mut visit);
                let local = local_name(e.name().as_ref()).to_string();
                // Defensive: only pop what the matching Start pushed
                // (tolerates unbalanced input).
                if stack.last().map(|s| *s == local).unwrap_or(false) {
                    stack.pop();
                }
            }
            Ok(Event::Empty(_)) | Ok(Event::CData(_)) => {
                // Self-closing elements and CDATA also terminate a text
                // region (CDATA content itself is not merged).
                flush(&mut acc, &stack, &mut visit);
            }
            Ok(Event::Text(t)) => {
                acc.push_text(&t)
                    .map_err(|e| format!("XML unescape error: {e}"))?;
            }
            Ok(Event::GeneralRef(r)) => {
                acc.push_ref(&r)
                    .map_err(|e| format!("XML unescape error: {e}"))?;
            }
            Ok(Event::Eof) => {
                flush(&mut acc, &stack, &mut visit);
                break;
            }
            Ok(_) => {
                flush(&mut acc, &stack, &mut visit);
            }
            Err(e) => return Err(format!("XML parse error: {e}")),
        }
        buf.clear();
    }
    Ok(())
}

/// Collect the resolved text of every element whose local name is in
/// `names`, in document order.
fn collect_texts(body: &str, names: &[&str]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    walk_element_texts(body, |stack, text| {
        if let Some(innermost) = stack.last() {
            if names.contains(&innermost.as_str()) {
                out.push(text.to_string());
            }
        }
    })?;
    Ok(out)
}

/// First resolved text of an element whose local name is in `names`.
fn element_text(body: &str, names: &[&str]) -> Result<Option<String>, String> {
    Ok(collect_texts(body, names)?.into_iter().next())
}

/// Parsed SetSystemDateAndTime fields.
struct ParsedSetDateTime {
    utc: (i32, i32, i32, i32, i32, i32),
    tz: String,
}

/// Parse a SetSystemDateAndTime body. Tolerant of prefixes, attributes,
/// and ordering; requires a complete UTCDateTime (all six fields as
/// integers) — the hook contract carries the UTC fields, and NVR clients
/// send them for both Manual and NTP modes (the WSDL makes UTCDateTime
/// optional for NTP; this library still requires it so the hook always
/// receives a full time).
fn parse_set_system_date_time(body: &str) -> Result<ParsedSetDateTime, String> {
    let mut date_type: Option<String> = None;
    let mut tz: Option<String> = None;
    let mut hour: Option<i32> = None;
    let mut minute: Option<i32> = None;
    let mut second: Option<i32> = None;
    let mut year: Option<i32> = None;
    let mut month: Option<i32> = None;
    let mut day: Option<i32> = None;

    walk_element_texts(body, |stack, text| {
        let innermost = stack.last().map(String::as_str).unwrap_or_default();
        let in_utc = stack.iter().any(|s| s == "UTCDateTime");
        match innermost {
            "DateTimeType" => date_type = Some(text.to_string()),
            "TZ" => tz = Some(text.to_string()),
            "Hour" if in_utc => hour = text.parse::<i32>().ok(),
            "Minute" if in_utc => minute = text.parse::<i32>().ok(),
            "Second" if in_utc => second = text.parse::<i32>().ok(),
            "Year" if in_utc => year = text.parse::<i32>().ok(),
            "Month" if in_utc => month = text.parse::<i32>().ok(),
            "Day" if in_utc => day = text.parse::<i32>().ok(),
            _ => {}
        }
    })?;

    match date_type.as_deref() {
        Some("Manual") | Some("NTP") => {}
        _ => return Err("DateTimeType must be Manual or NTP".into()),
    }
    let utc = (
        year.ok_or("UTCDateTime requires a numeric Year")?,
        month.ok_or("UTCDateTime requires a numeric Month")?,
        day.ok_or("UTCDateTime requires a numeric Day")?,
        hour.ok_or("UTCDateTime requires a numeric Hour")?,
        minute.ok_or("UTCDateTime requires a numeric Minute")?,
        second.ok_or("UTCDateTime requires a numeric Second")?,
    );
    Ok(ParsedSetDateTime {
        utc,
        tz: tz.unwrap_or_default(),
    })
}

/// Parse scope items from an Add/Remove/SetScopes body: the texts of
/// `ScopeItem` elements (Add/Remove spelling per the WSDL) and `Scopes`
/// elements (the SetScopes spelling) — both accepted everywhere for
/// client tolerance.
fn parse_scope_items(body: &str) -> Result<Vec<String>, String> {
    let mut items = collect_texts(body, &["ScopeItem", "Scopes"])?;
    items.retain(|s| !s.is_empty());
    Ok(items)
}

/// Parse `User` entries (CreateUsers/SetUser): each `User` element's
/// Username/Password/UserLevel texts, grouped per User. Dedicated reader
/// loop (the imaging.rs `parse_settings` pattern): text-only visitors
/// cannot see element opens, and entries must open with their `User`.
fn parse_user_entries(body: &str) -> Result<Vec<ParsedUserEntry>, String> {
    fn assign(users: &mut [ParsedUserEntry], field: &Option<String>, text: &str) {
        let Some(field) = field else { return };
        let Some(current) = users.last_mut() else {
            return;
        };
        match field.as_str() {
            "Username" => current.username = Some(text.to_string()),
            "Password" => current.password = Some(text.to_string()),
            "UserLevel" => current.user_level = Some(text.to_string()),
            _ => {}
        }
    }
    fn take_region(acc: &mut crate::types::TextAccumulator) -> Option<String> {
        acc.flush().filter(|s| !s.is_empty())
    }

    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut users: Vec<ParsedUserEntry> = Vec::new();
    let mut in_user = false;
    let mut field: Option<String> = None;
    let mut acc = crate::types::TextAccumulator::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Text(t)) => {
                acc.push_text(&t)
                    .map_err(|e| format!("XML unescape error: {e}"))?;
            }
            Ok(Event::GeneralRef(r)) => {
                acc.push_ref(&r)
                    .map_err(|e| format!("XML unescape error: {e}"))?;
            }
            Ok(Event::Eof) => {
                if let Some(text) = take_region(&mut acc) {
                    assign(&mut users, &field, &text);
                }
                break;
            }
            Ok(Event::Start(e)) => {
                if let Some(text) = take_region(&mut acc) {
                    assign(&mut users, &field, &text);
                }
                let local = local_name(e.name().as_ref()).to_string();
                if local == "User" {
                    users.push(ParsedUserEntry::default());
                    in_user = true;
                    field = None;
                } else if in_user {
                    field = Some(local);
                }
            }
            Ok(Event::End(e)) => {
                if let Some(text) = take_region(&mut acc) {
                    assign(&mut users, &field, &text);
                }
                if local_name(e.name().as_ref()) == "User" {
                    in_user = false;
                }
                field = None;
            }
            Ok(_) => {
                if let Some(text) = take_region(&mut acc) {
                    assign(&mut users, &field, &text);
                }
            }
            Err(e) => return Err(format!("XML parse error: {e}")),
        }
        buf.clear();
    }
    Ok(users)
}

/// Parse the `Username` element texts (DeleteUsers).
fn parse_username_list(body: &str) -> Result<Vec<String>, String> {
    let mut names = collect_texts(body, &["Username"])?;
    names.retain(|s| !s.is_empty());
    Ok(names)
}

/// Extract the local name from a qualified XML name (e.g. `tt:TZ` ->
/// `TZ`, `Name` -> `Name`).
fn local_name(name: &[u8]) -> &str {
    let qname = std::str::from_utf8(name).unwrap_or("");
    qname.rsplit(':').next().unwrap_or(qname)
}

/// Write `<tt:XXX><tt:XAddr>base/path</tt:XAddr></tt:XXX>`.
fn capability_xaddr(w: &mut Writer<Vec<u8>>, tag: &str, base: &str, path: &str) {
    open_close(w, tag, |w| {
        write_text(w, "tt:XAddr", &format!("{}{}", base, path));
    });
}

/// Convert seconds since UNIX_EPOCH to UTC date/time fields.
/// Valid for years 1970–2100. Crate-visible: the events service reuses it
/// for RFC3339 timestamps.
pub(crate) fn secs_to_utc(secs: u64) -> (i32, i32, i32, i32, i32, i32) {
    let days = secs / 86400;
    let rem = secs % 86400;
    let hour = (rem / 3600) as i32;
    let minute = ((rem % 3600) / 60) as i32;
    let second = (rem % 60) as i32;

    let mut y: i64 = 1970;
    let mut d = days as i64;
    loop {
        let diy = if is_leap(y) { 366 } else { 365 };
        if d < diy {
            break;
        }
        d -= diy;
        y += 1;
    }

    let mdays = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1i32;
    for &md in &mdays {
        if d < md as i64 {
            break;
        }
        d -= md as i64;
        month += 1;
    }
    (y as i32, month, (d + 1) as i32, hour, minute, second)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ---------------------------------------------------------------------------
// Security ops (issue #54): IP address filter + AccessPolicy
//
// Self-contained section — `sec_`-prefixed helpers only, nothing shared
// with the response builders above, so the section stays merge-friendly
// against independent changes to the rest of this file.
//
// 802.1X (GetDot1XConfiguration/SetDot1XConfiguration &c.) is
// deliberately NOT implemented: an EAP supplicant is host
// infrastructure, not SOAP device protocol — those actions stay
// unregistered and answer the generic unsupported-action fault.
// ---------------------------------------------------------------------------

/// Mode of the ONVIF IP address filter (`tt:IPAddressFilter/Type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpFilterMode {
    Allow,
    Deny,
}

impl IpFilterMode {
    fn as_wire(&self) -> &'static str {
        match self {
            Self::Allow => "Allow",
            Self::Deny => "Deny",
        }
    }

    fn from_wire(s: &str) -> Option<Self> {
        match s.trim() {
            "Allow" => Some(Self::Allow),
            "Deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// One IPv4 filter entry: dotted-quad network address + prefix length
/// 0–32 (`tt:PrefixedIPv4Address` on the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpEntry {
    pub ipv4: String,
    pub prefix_len: u8,
}

/// The ONVIF IP address filter state — host-mutable config seam shared
/// between the SOAP ops and [`crate::server::OnvifServer::with_ip_filter`].
/// `enabled = false` filters nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpFilter {
    pub enabled: bool,
    pub mode: IpFilterMode,
    pub entries: Vec<IpEntry>,
}

impl Default for IpFilter {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: IpFilterMode::Allow,
            entries: Vec::new(),
        }
    }
}

impl IpFilter {
    /// A filter that admits everyone (`enabled = false`).
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Whether a client IP passes the filter.
    ///
    /// Unparseable or IPv6 client addresses fail OPEN (allowed): a
    /// malformed peer address must not brick the SOAP listener — the
    /// caller logs a warning. Entries whose address does not parse
    /// never match. Enforcement notes: prefix 0 matches everything,
    /// 32 matches one host; Allow mode admits only matching peers,
    /// Deny mode refuses only matching peers.
    pub fn allows_client_ip(&self, client_ip: &str) -> bool {
        if !self.enabled {
            return true;
        }
        let Some(ip) = client_ip.trim().parse::<std::net::Ipv4Addr>().ok() else {
            // IPv6 peer or unparseable address: fail open (documented) —
            // a malformed peer address must not brick the listener.
            log::warn!("onvif: IP filter cannot parse client address {client_ip:?} — allowing");
            return true;
        };
        let client = u32::from(ip);
        let matched = self
            .entries
            .iter()
            .any(|e| match e.ipv4.parse::<std::net::Ipv4Addr>() {
                Ok(net) => sec_ipv4_prefix_matches(client, u32::from(net), e.prefix_len),
                Err(_) => false, // malformed stored entry never matches
            });
        match self.mode {
            IpFilterMode::Allow => matched,
            IpFilterMode::Deny => !matched,
        }
    }
}

/// Shared IP filter state (install via
/// [`DeviceServiceHandlers::with_ip_filter`]).
pub type IpFilterState = Arc<std::sync::RwLock<IpFilter>>;
/// Shared AccessPolicy blob state (install via
/// [`DeviceServiceHandlers::with_access_policy`]). The library stores
/// and returns the bytes verbatim — interpreting (enforcing) the policy
/// is host-side.
pub type AccessPolicyState = Arc<std::sync::RwLock<Vec<u8>>>;

/// Poison-tolerant read guard (the established crate pattern, kept
/// local to this section under a `sec_` name).
fn sec_read<T>(lock: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Poison-tolerant write guard.
fn sec_write<T>(lock: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Whether two IPv4 addresses (as `u32`) share the first `prefix` bits.
fn sec_ipv4_prefix_matches(a: u32, b: u32, prefix: u8) -> bool {
    if prefix > 32 {
        return false;
    }
    let mask: u32 = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    (a & mask) == (b & mask)
}

impl DeviceServiceHandlers {
    /// Install the IP filter config seam (issue #54). Wire the SAME
    /// state into [`crate::server::OnvifServer::with_ip_filter`] so the
    /// SOAP Get/Set/Add/RemoveIPAddressFilter ops and the
    /// per-connection gate share one store.
    #[must_use]
    pub fn with_ip_filter(mut self, state: IpFilterState) -> Self {
        self.ip_filter = Some(state);
        self
    }

    /// The installed IP filter state, when present (hand this to
    /// [`crate::server::OnvifServer::with_ip_filter`]).
    #[must_use]
    pub fn ip_filter_state(&self) -> Option<IpFilterState> {
        self.ip_filter.clone()
    }

    /// Install the AccessPolicy config seam (issue #54): Get/Set carry
    /// an opaque Base64 blob; this library does not interpret it.
    #[must_use]
    pub fn with_access_policy(mut self, state: AccessPolicyState) -> Self {
        self.access_policy = Some(state);
        self
    }

    /// The installed AccessPolicy state, when present.
    #[must_use]
    pub fn access_policy_state(&self) -> Option<AccessPolicyState> {
        self.access_policy.clone()
    }

    /// `<tds:GetIPAddressFilterResponse>` — the current filter; without
    /// an installed state, a disabled (allow-all) filter.
    fn sec_build_get_ip_address_filter(&self) -> String {
        let filter = match &self.ip_filter {
            Some(state) => sec_read(state).clone(),
            None => IpFilter::disabled(),
        };
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:GetIPAddressFilterResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();
        w.write_event(Event::Start(BytesStart::new("tds:IPAddressFilter")))
            .unwrap_or_default();
        sec_write_text(&mut w, "tt:Type", filter.mode.as_wire());
        for e in &filter.entries {
            sec_write_entry(&mut w, e);
        }
        w.write_event(Event::End(BytesEnd::new("tds:IPAddressFilter")))
            .unwrap_or_default();
        w.write_event(Event::End(BytesEnd::new("tds:GetIPAddressFilterResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// `SetIPAddressFilter` — replace mode + entries wholesale.
    fn sec_apply_set_ip_address_filter(&self, body: &str) -> Result<String, OnvifError> {
        let Some(state) = &self.ip_filter else {
            return Err(OnvifError::SenderFault(
                "IP address filter is not configured on this device".into(),
            ));
        };
        let parsed = sec_parse_ip_filter(body)?;
        let mut f = sec_write(state);
        f.enabled = true;
        f.mode = parsed.mode;
        f.entries = parsed.entries;
        Ok(sec_empty_ack("tds:SetIPAddressFilterResponse"))
    }

    /// `AddIPAddressFilter` — set the mode and append entries not
    /// already present.
    fn sec_apply_add_ip_address_filter(&self, body: &str) -> Result<String, OnvifError> {
        let Some(state) = &self.ip_filter else {
            return Err(OnvifError::SenderFault(
                "IP address filter is not configured on this device".into(),
            ));
        };
        let parsed = sec_parse_ip_filter(body)?;
        let mut f = sec_write(state);
        f.enabled = true;
        f.mode = parsed.mode;
        for e in parsed.entries {
            if !f.entries.contains(&e) {
                f.entries.push(e);
            }
        }
        Ok(sec_empty_ack("tds:AddIPAddressFilterResponse"))
    }

    /// `RemoveIPAddressFilter` — remove entries matching the request
    /// (address + prefix); the mode is kept. Idempotent: removing an
    /// absent entry still acks.
    fn sec_apply_remove_ip_address_filter(&self, body: &str) -> Result<String, OnvifError> {
        let Some(state) = &self.ip_filter else {
            return Err(OnvifError::SenderFault(
                "IP address filter is not configured on this device".into(),
            ));
        };
        let parsed = sec_parse_ip_filter(body)?;
        let mut f = sec_write(state);
        f.entries.retain(|e| !parsed.entries.contains(e));
        Ok(sec_empty_ack("tds:RemoveIPAddressFilterResponse"))
    }

    /// `<tds:GetAccessPolicyResponse>` — the stored policy blob
    /// (Base64), empty when none installed.
    fn sec_build_get_access_policy(&self) -> String {
        use base64::Engine as _;

        let blob = match &self.access_policy {
            Some(state) => sec_read(state).clone(),
            None => Vec::new(),
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&blob);
        let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
        let mut root = BytesStart::new("tds:GetAccessPolicyResponse");
        root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
        root.push_attribute(("xmlns:tt", SCHEMAS));
        w.write_event(Event::Start(root)).unwrap_or_default();
        w.write_event(Event::Start(BytesStart::new("tds:PolicyFile")))
            .unwrap_or_default();
        sec_write_text(&mut w, "tt:Data", &b64);
        w.write_event(Event::End(BytesEnd::new("tds:PolicyFile")))
            .unwrap_or_default();
        w.write_event(Event::End(BytesEnd::new("tds:GetAccessPolicyResponse")))
            .unwrap_or_default();
        String::from_utf8(w.into_inner()).unwrap_or_default()
    }

    /// `SetAccessPolicy` — decode the Base64 PolicyFile/Data into the
    /// store.
    fn sec_apply_set_access_policy(&self, body: &str) -> Result<String, OnvifError> {
        use base64::Engine as _;

        let Some(state) = &self.access_policy else {
            return Err(OnvifError::SenderFault(
                "access policy is not configured on this device".into(),
            ));
        };
        let b64 = sec_extract_policy_data(body).ok_or_else(|| {
            OnvifError::SenderFault("SetAccessPolicy requires a PolicyFile/Data payload".into())
        })?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|_| OnvifError::SenderFault("PolicyFile Data is not valid base64".into()))?;
        *sec_write(state) = bytes;
        Ok(sec_empty_ack("tds:SetAccessPolicyResponse"))
    }
}

/// Serialize one `<tt:Address>`/`<tt:PrefixLength>` pair inside an open
/// `tt:IPv4Address` element.
fn sec_write_entry(w: &mut Writer<Vec<u8>>, entry: &IpEntry) {
    w.write_event(Event::Start(BytesStart::new("tt:IPv4Address")))
        .unwrap_or_default();
    sec_write_text(w, "tt:Address", &entry.ipv4);
    sec_write_text(w, "tt:PrefixLength", &entry.prefix_len.to_string());
    w.write_event(Event::End(BytesEnd::new("tt:IPv4Address")))
        .unwrap_or_default();
}

/// Text-element writer for this section (independent of `write_text`).
fn sec_write_text(w: &mut Writer<Vec<u8>>, name: &str, text: &str) {
    w.write_event(Event::Start(BytesStart::new(name)))
        .unwrap_or_default();
    w.write_event(Event::Text(BytesText::new(text)))
        .unwrap_or_default();
    w.write_event(Event::End(BytesEnd::new(name)))
        .unwrap_or_default();
}

/// An empty (ack-only) `<tds:XxxResponse>` fragment.
fn sec_empty_ack(response_local_name: &str) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    let mut root = BytesStart::new(response_local_name);
    root.push_attribute(("xmlns:tds", DEVICE_SERVICE));
    w.write_event(Event::Start(root)).unwrap_or_default();
    w.write_event(Event::End(BytesEnd::new(response_local_name)))
        .unwrap_or_default();
    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// Local name of a QName (`tt:Address` → `Address`).
fn sec_local_name(name: quick_xml::name::QName<'_>) -> String {
    let qname = std::str::from_utf8(name.as_ref()).unwrap_or("");
    qname.rsplit(':').next().unwrap_or(qname).to_string()
}

/// Parse the `IPAddressFilter` payload of Set/Add/Remove (tolerant of
/// `tt:`-prefixed or default-namespace forms). Requires a valid Type
/// and complete IPv4 entries; IPv6 entries are refused (IPv4-only
/// state, honestly reported rather than silently dropped).
fn sec_parse_ip_filter(body: &str) -> Result<IpFilter, OnvifError> {
    let mut reader = quick_xml::Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut mode: Option<IpFilterMode> = None;
    let mut entries: Vec<IpEntry> = Vec::new();
    let mut saw_ipv6 = false;
    // State inside one <IPv4Address> element.
    let mut in_v4 = false;
    let mut addr: Option<String> = None;
    let mut plen: Option<u8> = None;
    let mut field = String::new();
    let mut text_acc = crate::types::TextAccumulator::new();

    loop {
        let event = reader.read_event_into(&mut buf);
        match event {
            Ok(Event::Text(e)) => {
                let _ = text_acc.push_text(&e);
            }
            Ok(Event::GeneralRef(e)) => {
                let _ = text_acc.push_ref(&e);
            }
            other => {
                if let Some(text) = text_acc.flush() {
                    if !text.is_empty() {
                        match field.as_str() {
                            "Type" => {
                                if mode.is_none() {
                                    mode = IpFilterMode::from_wire(&text);
                                }
                            }
                            "Address" if in_v4 => addr = Some(text),
                            "PrefixLength" if in_v4 => {
                                plen = text.parse::<u8>().ok().filter(|p| *p <= 32)
                            }
                            _ => {}
                        }
                    }
                }
                match other {
                    Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                        match sec_local_name(e.name()).as_str() {
                            "Type" => field = "Type".into(),
                            "IPv4Address" => {
                                in_v4 = true;
                                addr = None;
                                plen = None;
                                field.clear();
                            }
                            "IPv6Address" => saw_ipv6 = true,
                            "Address" if in_v4 => field = "Address".into(),
                            "PrefixLength" if in_v4 => field = "PrefixLength".into(),
                            _ => field.clear(),
                        }
                    }
                    Ok(Event::End(e)) => {
                        let local = sec_local_name(e.name());
                        if local == "IPv4Address" && in_v4 {
                            let a = addr
                                .as_deref()
                                .and_then(|a| a.parse::<std::net::Ipv4Addr>().ok())
                                .map(|ip| ip.to_string());
                            match (a, plen) {
                                (Some(address), Some(prefix_len)) => {
                                    entries.push(IpEntry {
                                        ipv4: address,
                                        prefix_len,
                                    });
                                }
                                (None, _) => {
                                    return Err(OnvifError::SenderFault(
                                        "IPv4 filter entry requires a valid dotted-quad Address"
                                            .into(),
                                    ));
                                }
                                (_, None) => {
                                    return Err(OnvifError::SenderFault(
                                        "IPv4 filter entry requires a PrefixLength (0-32)".into(),
                                    ));
                                }
                            }
                            in_v4 = false;
                        }
                        if local == "Type" || local == "Address" || local == "PrefixLength" {
                            field.clear();
                        }
                    }
                    Ok(Event::Eof) => break,
                    Err(e) => {
                        return Err(OnvifError::InvalidXml(format!("XML parse error: {e}")));
                    }
                    _ => {}
                }
            }
        }
        buf.clear();
    }

    if saw_ipv6 {
        return Err(OnvifError::SenderFault(
            "IPv6 filter entries are not supported (IPv4 only)".into(),
        ));
    }
    let Some(mode) = mode else {
        return Err(OnvifError::SenderFault(
            "IPAddressFilter requires a Type of Allow or Deny".into(),
        ));
    };
    Ok(IpFilter {
        enabled: true,
        mode,
        entries,
    })
}

/// Extract the Base64 text of `PolicyFile/Data` from a SetAccessPolicy
/// body (tolerant of prefixes). `None` when no Data element is found.
fn sec_extract_policy_data(body: &str) -> Option<String> {
    let mut reader = quick_xml::Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut in_data = false;
    let mut out: Option<String> = None;
    let mut text_acc = crate::types::TextAccumulator::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Text(e)) => {
                let _ = text_acc.push_text(&e);
            }
            Ok(Event::GeneralRef(e)) => {
                let _ = text_acc.push_ref(&e);
            }
            Ok(Event::Start(e)) => {
                let _ = text_acc.flush();
                if sec_local_name(e.name()) == "Data" {
                    in_data = true;
                }
            }
            Ok(Event::Empty(_)) => {
                let _ = text_acc.flush();
            }
            Ok(Event::End(e)) => {
                let flushed = text_acc.flush();
                let local = sec_local_name(e.name());
                if in_data && local == "Data" {
                    in_data = false;
                    out = flushed; // schema has exactly one Data
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AuthResult;

    fn test_handlers() -> DeviceServiceHandlers {
        DeviceServiceHandlers::new(test_device_config(), 8080, "192.168.1.100".to_string()).unwrap()
    }

    /// Explicit, valid identity — the neutral defaults would fail the
    /// constructor's fail-fast validation (issue #20).
    fn test_device_config() -> DeviceConfig {
        DeviceConfig {
            name: "Test Cam".into(),
            manufacturer: "MiBee".into(),
            model: "IMX219".into(),
            firmware: "1.0.0".into(),
            hardware_id: "HW-1".into(),
            serial_number: "SN-1".into(),
        }
    }

    fn test_info(server_ip: &str) -> RequestInfo {
        RequestInfo {
            client_ip: "10.0.0.1".to_string(),
            server_ip: server_ip.to_string(),
            auth_result: AuthResult {
                username: "admin".into(),
                authenticated: true,
            },
        }
    }

    // --------------------------------------------------------------
    // XML body fragment tests (no SOAP envelope)
    // --------------------------------------------------------------

    #[test]
    fn test_get_system_date_time_contains_expected_fields() {
        let h = test_handlers();
        let xml = h.build_system_date_time();

        assert!(xml.contains("GetSystemDateAndTimeResponse"));
        assert!(xml.contains("DateTimeType"));
        assert!(xml.contains("Manual"));
        assert!(xml.contains("DaylightSavings"));
        assert!(xml.contains("TimeZone"));
        assert!(xml.contains("<tt:TZ>UTC</tt:TZ>"));
        assert!(xml.contains("UTCDateTime"));
        assert!(xml.contains("tt:Hour"));
        assert!(xml.contains("tt:Minute"));
        assert!(xml.contains("tt:Second"));
        assert!(xml.contains("tt:Year"));
        assert!(xml.contains("tt:Month"));
        assert!(xml.contains("tt:Day"));

        // Verify well-formed XML
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Err(e) => panic!("XML parse error in system_date_time: {e}"),
                _ => {}
            }
            buf.clear();
        }
    }

    #[test]
    fn test_get_device_information_contains_expected_fields() {
        let h = DeviceServiceHandlers::new(
            DeviceConfig {
                manufacturer: "Raspberry Pi".into(),
                model: "OV5647".into(),
                firmware: "1.0.0".into(),
                serial_number: "SN-001".into(),
                hardware_id: "OV5647".into(),
                name: "Pi Camera V1".into(),
            },
            8080,
            "192.168.1.100".to_string(),
        )
        .unwrap();
        let xml = h.build_device_information();

        assert!(xml.contains("GetDeviceInformationResponse"));
        assert!(xml.contains("Raspberry Pi"));
        assert!(xml.contains("OV5647"));
        assert!(xml.contains("1.0.0"));
        assert!(xml.contains("SN-001"));
        assert!(xml.contains("tds:Manufacturer"));
        assert!(xml.contains("tds:Model"));
        assert!(xml.contains("tds:FirmwareVersion"));
        assert!(xml.contains("tds:SerialNumber"));
        assert!(xml.contains("tds:HardwareId"));
    }

    #[test]
    fn test_get_capabilities_contains_service_endpoints() {
        let h = test_handlers();
        let xml = h.build_capabilities("10.0.0.5");

        assert!(xml.contains("GetCapabilitiesResponse"));
        assert!(xml.contains("http://10.0.0.5:8080/onvif/device_service"));
        assert!(xml.contains("http://10.0.0.5:8080/onvif/media_service"));
        assert!(xml.contains("http://10.0.0.5:8080/onvif/ptz_service"));
        assert!(xml.contains("tt:Device"));
        assert!(xml.contains("tt:Media"));
        assert!(xml.contains("tt:PTZ"));
        assert!(xml.contains("tt:Imaging"));
    }

    /// WSDL (devicemgmt.xsd): GetServicesResponse carries `Service`
    /// elements as DIRECT children — no wrapper. Strict clients (issue
    /// #64: the onvif-go client) parse zero services from a wrapped list.
    #[test]
    fn test_get_services_direct_children_no_wrapper() {
        let h = test_handlers();
        let xml = h.build_services("10.0.0.5");
        assert!(
            !xml.contains("tds:Services"),
            "no tds:Services wrapper may exist: {xml}"
        );
        // Direct children: every Service element sits exactly one indent
        // level under the response root.
        assert!(xml.contains(">\n  <tds:Service>"));
        assert!(xml.contains("\n  </tds:Service>\n</tds:GetServicesResponse>"));
    }

    #[test]
    fn test_get_services_contains_service_entries() {
        let h = test_handlers();
        let xml = h.build_services("10.0.0.5");

        assert!(xml.contains("GetServicesResponse"));
        assert!(xml.contains("tds:Service"));
        assert!(xml.contains("tds:Namespace"));
        assert!(xml.contains("tds:XAddr"));
        assert!(xml.contains("tds:Version"));
        assert!(xml.contains("<tt:Major>1</tt:Major>"));
        assert!(xml.contains("<tt:Minor>0</tt:Minor>"));
        assert!(xml.contains("http://10.0.0.5:8080/onvif/device_service"));
        assert!(xml.contains("http://10.0.0.5:8080/onvif/media_service"));
        assert!(xml.contains("http://10.0.0.5:8080/onvif/ptz_service"));
        assert!(xml.contains(DEVICE_SERVICE));
        assert!(xml.contains(MEDIA_SERVICE));
        assert!(xml.contains(PTZ_SERVICE));
        assert!(xml.contains(IMAGING_SERVICE));
    }

    /// WSDL: each scope is a `tds:Scopes` element of type tt:Scope with
    /// `tt:ScopeDef` (Fixed|Configurable) + `tt:ScopeItem` children (issue
    /// #65) — bare tt:ScopeItem strings parse as zero scopes on strict
    /// clients.
    #[test]
    fn test_get_scopes_wsd_scope_shape() {
        let h = test_handlers();
        let xml = h.build_scopes();
        assert!(xml.contains("<tds:Scopes>"));
        assert!(xml.contains("<tt:ScopeDef>Fixed</tt:ScopeDef>"));
        assert!(!xml.contains("<tt:ScopeDef>Configurable</tt:ScopeDef>"));
        assert_eq!(xml.matches("<tds:Scopes>").count(), 3);
        assert_eq!(xml.matches("<tt:ScopeDef>Fixed</tt:ScopeDef>").count(), 3);
    }

    #[test]
    fn test_get_scopes_contains_scope_items() {
        let h = DeviceServiceHandlers::new(
            DeviceConfig {
                name: "Pi Camera V1".into(),
                hardware_id: "OV5647".into(),
                manufacturer: "TestVendor".into(),
                model: "TestModel".into(),
                ..DeviceConfig::default()
            },
            8080,
            "192.168.1.100".to_string(),
        )
        .unwrap();
        let xml = h.build_scopes();

        assert!(xml.contains("GetScopesResponse"));
        assert!(xml.contains("tt:ScopeItem"));
        assert!(xml.contains("onvif://www.onvif.org/type/video_encoder"));
        assert!(xml.contains("onvif://www.onvif.org/name/Pi Camera V1"));
        assert!(xml.contains("onvif://www.onvif.org/hardware/OV5647"));
    }

    // --------------------------------------------------------------
    // SystemReboot (parity with onvif-go HandleSystemReboot)
    // --------------------------------------------------------------

    #[test]
    fn test_build_system_reboot_contains_message() {
        let h = test_handlers();
        let xml = h.build_system_reboot();

        assert!(xml.contains("tds:SystemRebootResponse"));
        assert!(xml.contains("tds:Message"));
        assert!(xml.contains("Device rebooting"));
        // WSDL namespace declaration on the response root.
        assert!(xml.contains("xmlns:tds"));
        assert!(xml.contains(DEVICE_SERVICE));

        // Verify well-formed XML
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Err(e) => panic!("XML parse error in system_reboot: {e}"),
                _ => {}
            }
            buf.clear();
        }
    }

    #[tokio::test]
    async fn test_handler_dispatches_system_reboot() {
        let svc = Arc::new(test_handlers());
        let handler = DeviceHandler(svc);
        let ri = test_info("10.0.0.1");

        let body = r#"<SystemReboot xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#;
        let resp = handler.handle(body, &ri).await.unwrap();

        assert!(resp.contains("soap:Envelope"));
        assert!(resp.contains("SystemRebootResponse"));
        assert!(resp.contains("Device rebooting"));
    }

    /// A SOAP body mentioning SystemReboot inside a *different* action
    /// element must not be misrouted by the substring dispatch chain.
    #[tokio::test]
    async fn test_handler_routes_get_actions_over_system_reboot_substring() {
        let svc = Arc::new(test_handlers());
        let handler = DeviceHandler(svc);
        let ri = test_info("10.0.0.1");

        // GetServices contains "Service", GetSystemDateAndTime contains
        // "System"… but none of the existing actions contains the exact
        // "SystemReboot" token; this pins the reverse direction — a body
        // that IS SystemReboot never matches the earlier arms.
        let body = r#"<GetDeviceInformation xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#;
        let resp = handler.handle(body, &ri).await.unwrap();
        assert!(resp.contains("GetDeviceInformationResponse"));
        assert!(!resp.contains("SystemRebootResponse"));
    }

    // --------------------------------------------------------------
    // Per-request client IP tests
    // --------------------------------------------------------------

    #[test]
    fn test_get_capabilities_uses_per_request_client_ip() {
        let h = test_handlers();
        let a = h.build_capabilities("192.168.1.99");
        let b = h.build_capabilities("10.0.0.42");
        assert!(a.contains("192.168.1.99"));
        assert!(b.contains("10.0.0.42"));
        assert!(!a.contains("10.0.0.42"));
        assert!(!b.contains("192.168.1.99"));
    }

    #[test]
    fn test_get_services_uses_per_request_client_ip() {
        let h = test_handlers();
        let xml = h.build_services("172.16.0.8");
        assert!(xml.contains("172.16.0.8"));
    }

    // --------------------------------------------------------------
    // Per-service advertisement flags (issue #47) — GetServices and
    // GetCapabilities must agree with what the host actually serves.
    // --------------------------------------------------------------

    #[test]
    fn test_get_services_omits_disabled_services() {
        let h = test_handlers()
            .with_media_support(false)
            .with_ptz_support(false)
            .with_imaging_support(false)
            .with_events_support(false);
        let xml = h.build_services("10.0.0.5");

        assert!(
            xml.contains(DEVICE_SERVICE),
            "Device is always advertised, got: {xml}"
        );
        assert!(!xml.contains(MEDIA_SERVICE), "media disabled: {xml}");
        assert!(!xml.contains(PTZ_SERVICE), "ptz disabled: {xml}");
        assert!(!xml.contains(IMAGING_SERVICE), "imaging disabled: {xml}");
        assert!(!xml.contains(EVENTS_SERVICE), "events disabled: {xml}");
        // Exactly one tds:Service entry.
        assert_eq!(xml.matches("<tds:Service>").count(), 1, "{xml}");
    }

    #[test]
    fn test_get_services_selectively_disables_services() {
        let h = test_handlers().with_ptz_support(false);
        let xml = h.build_services("10.0.0.5");
        assert!(xml.contains(DEVICE_SERVICE));
        assert!(xml.contains(MEDIA_SERVICE));
        assert!(!xml.contains(PTZ_SERVICE));
        assert!(xml.contains(IMAGING_SERVICE));
        assert_eq!(xml.matches("<tds:Service>").count(), 3, "{xml}");
    }

    #[test]
    fn test_get_capabilities_omits_disabled_services() {
        let h = test_handlers()
            .with_media_support(false)
            .with_ptz_support(false)
            .with_imaging_support(false);
        let xml = h.build_capabilities("10.0.0.5");

        assert!(xml.contains("tt:Device"), "device caps always present");
        assert!(!xml.contains("tt:Media"), "media disabled: {xml}");
        assert!(!xml.contains("tt:PTZ"), "ptz disabled: {xml}");
        assert!(!xml.contains("tt:Imaging"), "imaging disabled: {xml}");
        assert!(
            !xml.contains("/media_service"),
            "no media XAddr when disabled: {xml}"
        );
    }

    #[test]
    fn test_service_flags_default_advertises_everything() {
        // Defaults keep the historical (pre-flag) advertisement: every
        // service listed — existing hosts' wire bytes must not change.
        let h = test_handlers();
        let services = h.build_services("10.0.0.5");
        for ns in [DEVICE_SERVICE, MEDIA_SERVICE, PTZ_SERVICE, IMAGING_SERVICE] {
            assert!(services.contains(ns), "default must advertise {ns}");
        }
        assert_eq!(services.matches("<tds:Service>").count(), 4);
    }

    // --------------------------------------------------------------
    // DeviceHandler dispatch integration
    // --------------------------------------------------------------

    #[tokio::test]
    async fn test_handler_dispatches_get_device_information() {
        let svc = Arc::new(test_handlers());
        let handler = DeviceHandler(svc);
        let ri = test_info("10.0.0.1");

        let body = r#"<GetDeviceInformation xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#;
        let resp = handler.handle(body, &ri).await.unwrap();

        assert!(resp.contains("soap:Envelope"));
        assert!(resp.contains("GetDeviceInformationResponse"));
        assert!(resp.contains("MiBee"));
    }

    #[tokio::test]
    async fn test_handler_dispatches_get_capabilities_with_client_ip() {
        let svc = Arc::new(test_handlers());
        let handler = DeviceHandler(svc);
        let ri = test_info("192.168.1.50");

        let body = r#"<GetCapabilities xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#;
        let resp = handler.handle(body, &ri).await.unwrap();

        assert!(resp.contains("soap:Envelope"));
        assert!(resp.contains("192.168.1.50"));
        assert!(resp.contains("/onvif/device_service"));
    }

    #[tokio::test]
    async fn test_handler_returns_error_for_unknown_action() {
        let svc = Arc::new(test_handlers());
        let handler = DeviceHandler(svc);
        let ri = test_info("10.0.0.1");

        let body = r#"<GetFoo xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#;
        let result = handler.handle(body, &ri).await;
        assert!(result.is_err());
        match result {
            Err(OnvifError::ActionNotSupported(msg)) => {
                assert!(msg.contains("unknown device action"));
            }
            _ => panic!("expected ActionNotSupported"),
        }
    }

    #[test]
    fn test_soap_response_well_formed() {
        let svc = test_handlers();
        let xml = svc.build_device_information();
        let wrapped = serialize_soap_response(&xml);

        let mut reader = quick_xml::Reader::from_str(&wrapped);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Err(e) => panic!("SOAP response XML parse error: {e}"),
                _ => {}
            }
            buf.clear();
        }
    }

    // ==============================================================
    // Device service completion (issue #49)
    // ==============================================================

    /// Well-formedness helper shared by the issue #49 response tests.
    fn assert_well_formed(xml: &str, ctx: &str) {
        let mut reader = quick_xml::Reader::from_str(xml);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Err(e) => panic!("XML parse error in {ctx}: {e}"),
                _ => {}
            }
            buf.clear();
        }
    }

    /// Records every DeviceHooks call for assertions.
    #[derive(Default)]
    struct RecordingHooks {
        calls: std::sync::Mutex<Vec<String>>,
        log_text: &'static str,
    }

    impl RecordingHooks {
        fn snapshot(&self) -> Vec<String> {
            self.calls.lock().unwrap_or_else(|p| p.into_inner()).clone()
        }
    }

    impl DeviceHooks for RecordingHooks {
        fn set_date_time(&self, utc: (i32, i32, i32, i32, i32, i32), tz: &str) {
            self.calls
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(format!("set_date_time {utc:?} tz={tz}"));
        }
        fn reboot(&self) {
            self.calls
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push("reboot".into());
        }
        fn factory_default(&self, hard: bool) {
            self.calls
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(format!("factory_default hard={hard}"));
        }
        fn system_log(&self) -> String {
            self.log_text.to_string()
        }
        fn support_info(&self) -> String {
            "support & <info>".into()
        }
    }

    fn hooked_handlers() -> (DeviceServiceHandlers, Arc<RecordingHooks>) {
        let hooks = Arc::new(RecordingHooks {
            calls: std::sync::Mutex::new(Vec::new()),
            log_text: "line-1 & <line-2>",
        });
        let h = test_handlers().with_hooks(hooks.clone());
        (h, hooks)
    }

    async fn roundtrip(handler: &DeviceHandler, body: &str) -> Result<String, OnvifError> {
        handler.handle(body, &test_info("10.0.0.1")).await
    }

    fn sender_fault(result: &Result<String, OnvifError>) -> String {
        match result {
            Err(OnvifError::SenderFault(m)) => m.clone(),
            other => panic!("expected SenderFault, got {other:?}"),
        }
    }

    // --------------------------------------------------------------
    // SetSystemDateAndTime
    // --------------------------------------------------------------

    const SET_DATE_TIME_BODY_TT: &str = r#"<tds:SetSystemDateAndTime xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
  <tt:DateTimeType>Manual</tt:DateTimeType>
  <tt:DaylightSavings>false</tt:DaylightSavings>
  <tt:TimeZone>
    <tt:TZ>EST5EDT</tt:TZ>
  </tt:TimeZone>
  <tt:UTCDateTime>
    <tt:Time>
      <tt:Hour>12</tt:Hour>
      <tt:Minute>34</tt:Minute>
      <tt:Second>56</tt:Second>
    </tt:Time>
    <tt:Date>
      <tt:Year>2026</tt:Year>
      <tt:Month>9</tt:Month>
      <tt:Day>29</tt:Day>
    </tt:Date>
  </tt:UTCDateTime>
</tds:SetSystemDateAndTime>"#;

    #[tokio::test]
    async fn test_set_system_date_time_applies_and_acks() {
        let (svc, hooks) = hooked_handlers();
        let handler = DeviceHandler(Arc::new(svc));
        let resp = roundtrip(&handler, SET_DATE_TIME_BODY_TT).await.unwrap();

        assert!(resp.contains("soap:Envelope"));
        assert!(resp.contains("SetSystemDateAndTimeResponse"));
        // Empty response: no data elements echo back.
        assert!(!resp.contains("tt:"));
        assert_well_formed(&resp, "set_date_time ack");
        let calls = hooks.snapshot();
        assert_eq!(
            calls,
            vec!["set_date_time (2026, 9, 29, 12, 34, 56) tz=EST5EDT".to_string()],
            "hook must receive UTC fields + timezone"
        );
    }

    #[tokio::test]
    async fn test_set_system_date_time_tolerates_unprefixed_elements() {
        let (svc, hooks) = hooked_handlers();
        let handler = DeviceHandler(Arc::new(svc));
        let body = r#"<SetSystemDateAndTime xmlns="http://www.onvif.org/ver10/device/wsdl">
  <DateTimeType>NTP</DateTimeType>
  <DaylightSavings>true</DaylightSavings>
  <TimeZone><TZ>UTC</TZ></TimeZone>
  <UTCDateTime>
    <Time><Hour>1</Hour><Minute>2</Minute><Second>3</Second></Time>
    <Date><Year>2025</Year><Month>12</Month><Day>31</Day></Date>
  </UTCDateTime>
</SetSystemDateAndTime>"#;
        let resp = roundtrip(&handler, body).await.unwrap();
        assert!(resp.contains("SetSystemDateAndTimeResponse"));
        assert_eq!(
            hooks.snapshot(),
            vec!["set_date_time (2025, 12, 31, 1, 2, 3) tz=UTC".to_string()]
        );
    }

    #[tokio::test]
    async fn test_set_system_date_time_missing_utcdatetime_fauls() {
        let (svc, hooks) = hooked_handlers();
        let handler = DeviceHandler(Arc::new(svc));
        let body = r#"<SetSystemDateAndTime xmlns="http://www.onvif.org/ver10/device/wsdl">
  <DateTimeType>Manual</DateTimeType><DaylightSavings>false</DaylightSavings>
</SetSystemDateAndTime>"#;
        let result = roundtrip(&handler, body).await;
        let msg = sender_fault(&result);
        assert!(msg.contains("UTCDateTime"), "got: {msg}");
        assert!(hooks.snapshot().is_empty(), "no hook on fault");
    }

    #[tokio::test]
    async fn test_set_system_date_time_malformed_hour_fauls() {
        let (svc, _) = hooked_handlers();
        let handler = DeviceHandler(Arc::new(svc));
        let body = r#"<SetSystemDateAndTime xmlns="http://www.onvif.org/ver10/device/wsdl">
  <DateTimeType>Manual</DateTimeType><DaylightSavings>false</DaylightSavings>
  <UTCDateTime><Time><Hour>noon</Hour><Minute>0</Minute><Second>0</Second></Time>
  <Date><Year>2026</Year><Month>1</Month><Day>1</Day></Date></UTCDateTime>
</SetSystemDateAndTime>"#;
        let result = roundtrip(&handler, body).await;
        assert!(sender_fault(&result).contains("Hour"));
    }

    #[tokio::test]
    async fn test_set_system_date_time_incomplete_utcdatetime_fauls() {
        let (svc, _) = hooked_handlers();
        let handler = DeviceHandler(Arc::new(svc));
        let body = r#"<SetSystemDateAndTime xmlns="http://www.onvif.org/ver10/device/wsdl">
  <DateTimeType>Manual</DateTimeType><DaylightSavings>false</DaylightSavings>
  <UTCDateTime><Time><Hour>1</Hour><Minute>2</Minute><Second>3</Second></Time></UTCDateTime>
</SetSystemDateAndTime>"#;
        let result = roundtrip(&handler, body).await;
        assert!(sender_fault(&result).contains("Year"));
    }

    #[tokio::test]
    async fn test_set_system_date_time_invalid_type_fauls() {
        let (svc, _) = hooked_handlers();
        let handler = DeviceHandler(Arc::new(svc));
        let body = r#"<SetSystemDateAndTime xmlns="http://www.onvif.org/ver10/device/wsdl">
  <DateTimeType>Telepathic</DateTimeType><DaylightSavings>false</DaylightSavings>
  <UTCDateTime><Time><Hour>1</Hour><Minute>2</Minute><Second>3</Second></Time>
  <Date><Year>2026</Year><Month>1</Month><Day>1</Day></Date></UTCDateTime>
</SetSystemDateAndTime>"#;
        let result = roundtrip(&handler, body).await;
        assert!(sender_fault(&result).contains("DateTimeType"));
    }

    #[tokio::test]
    async fn test_set_system_date_time_without_hooks_still_acks() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(&handler, SET_DATE_TIME_BODY_TT).await.unwrap();
        assert!(resp.contains("SetSystemDateAndTimeResponse"));
    }

    /// SetSystemDateAndTime must not fall into the GetSystemDateAndTime
    /// arm even though both contain the token "SystemDateAndTime".
    #[tokio::test]
    async fn test_set_over_get_system_date_time_dispatch() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(&handler, SET_DATE_TIME_BODY_TT).await.unwrap();
        assert!(resp.contains("SetSystemDateAndTimeResponse"));
        assert!(!resp.contains("GetSystemDateAndTimeResponse"));

        let get_resp = roundtrip(
            &handler,
            r#"<GetSystemDateAndTime xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(get_resp.contains("GetSystemDateAndTimeResponse"));
        assert!(!get_resp.contains("SetSystemDateAndTimeResponse"));
    }

    // --------------------------------------------------------------
    // Scopes: mutable store, byte-stable initial output
    // --------------------------------------------------------------

    /// The initial GetScopes bytes are a contract (NVR raw matching) —
    /// pin them exactly, not just by containment.
    #[test]
    fn test_get_scopes_wsd_golden_before_any_write() {
        let h = test_handlers();
        assert_eq!(
            h.build_scopes(),
            concat!(
                "<tds:GetScopesResponse ",
                "xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\" ",
                "xmlns:tt=\"http://www.onvif.org/ver10/schema\">\n",
                "  <tds:Scopes>\n",
                "    <tt:ScopeDef>Fixed</tt:ScopeDef>\n",
                "    <tt:ScopeItem>onvif://www.onvif.org/type/video_encoder</tt:ScopeItem>\n",
                "  </tds:Scopes>\n",
                "  <tds:Scopes>\n",
                "    <tt:ScopeDef>Fixed</tt:ScopeDef>\n",
                "    <tt:ScopeItem>onvif://www.onvif.org/name/Test Cam</tt:ScopeItem>\n",
                "  </tds:Scopes>\n",
                "  <tds:Scopes>\n",
                "    <tt:ScopeDef>Fixed</tt:ScopeDef>\n",
                "    <tt:ScopeItem>onvif://www.onvif.org/hardware/HW-1</tt:ScopeItem>\n",
                "  </tds:Scopes>\n",
                "</tds:GetScopesResponse>"
            )
        );
    }

    #[tokio::test]
    async fn test_set_scopes_replaces_store() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let set = roundtrip(
            &handler,
            r#"<SetScopes xmlns="http://www.onvif.org/ver10/device/wsdl">
  <Scopes>onvif://www.onvif.org/location/city/Berlin</Scopes>
  <Scopes>onvif://www.onvif.org/type/ptz</Scopes>
</SetScopes>"#,
        )
        .await
        .unwrap();
        assert!(set.contains("SetScopesResponse"));

        let get = roundtrip(
            &handler,
            r#"<GetScopes xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(get.contains("onvif://www.onvif.org/location/city/Berlin"));
        assert!(get.contains("onvif://www.onvif.org/type/ptz"));
        assert!(!get.contains("name/Test Cam"), "replaced, not appended");
        assert_eq!(get.matches("<tt:ScopeItem>").count(), 2);
        assert_well_formed(&get, "get_scopes after set");
    }

    /// Some clients send `ScopeItem` children for SetScopes (the element
    /// name AddScopes/RemoveScopes use) — accept both spellings.
    #[tokio::test]
    async fn test_set_scopes_accepts_scopeitem_elements() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        roundtrip(
            &handler,
            r#"<SetScopes xmlns="http://www.onvif.org/ver10/device/wsdl">
  <tt:ScopeItem>onvif://www.onvif.org/location/city/Oslo</tt:ScopeItem>
</SetScopes>"#,
        )
        .await
        .unwrap();
        let get = roundtrip(
            &handler,
            r#"<GetScopes xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(get.contains("onvif://www.onvif.org/location/city/Oslo"));
    }

    /// The WSDL requires minOccurs=1 — an empty SetScopes is a client
    /// mistake.
    #[tokio::test]
    async fn test_set_scopes_empty_list_fauls() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let result = roundtrip(
            &handler,
            r#"<SetScopes xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await;
        assert!(sender_fault(&result).contains("scope"));
    }

    #[tokio::test]
    async fn test_add_scopes_appends_and_dedupes() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<AddScopes xmlns="http://www.onvif.org/ver10/device/wsdl">
  <tt:ScopeItem>onvif://www.onvif.org/location/city/Tokyo</tt:ScopeItem>
  <tt:ScopeItem>onvif://www.onvif.org/type/video_encoder</tt:ScopeItem>
</AddScopes>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("AddScopesResponse"));

        let get = roundtrip(
            &handler,
            r#"<GetScopes xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(get.contains("location/city/Tokyo"));
        assert_eq!(
            get.matches("<tt:ScopeItem>").count(),
            4,
            "built-ins + one new (duplicate not re-added): {get}"
        );
        // WSDL tt:Scope (issue #65): client-added scopes are Configurable,
        // the built-ins stay Fixed.
        assert_eq!(
            get.matches("<tt:ScopeDef>Configurable</tt:ScopeDef>")
                .count(),
            1
        );
        assert_eq!(get.matches("<tt:ScopeDef>Fixed</tt:ScopeDef>").count(), 3);
    }

    #[tokio::test]
    async fn test_add_scopes_empty_fauls() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let result = roundtrip(
            &handler,
            r#"<AddScopes xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await;
        assert!(sender_fault(&result).contains("ScopeItem"));
    }

    #[tokio::test]
    async fn test_remove_scopes_removes_matching() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<RemoveScopes xmlns="http://www.onvif.org/ver10/device/wsdl">
  <ScopeItem>onvif://www.onvif.org/name/Test Cam</ScopeItem>
</RemoveScopes>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("RemoveScopesResponse"));

        let get = roundtrip(
            &handler,
            r#"<GetScopes xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(!get.contains("name/Test Cam"));
        assert_eq!(get.matches("<tt:ScopeItem>").count(), 2);
    }

    /// Client-controlled scope text must be escaped on output.
    #[tokio::test]
    async fn test_scope_texts_escaped_on_output() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        roundtrip(
            &handler,
            r#"<AddScopes xmlns="http://www.onvif.org/ver10/device/wsdl">
  <ScopeItem>onvif://example/&lt;&amp;&gt;</ScopeItem>
</AddScopes>"#,
        )
        .await
        .unwrap();
        let get = roundtrip(
            &handler,
            r#"<GetScopes xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(get.contains("onvif://example/&lt;&amp;&gt;"), "got: {get}");
        assert_well_formed(&get, "escaped scopes");
    }

    /// Echo adversarial bodies: write bodies carrying a read action's
    /// token as *text* must still route to the write verb.
    #[tokio::test]
    async fn test_scopes_family_dispatch_survives_echoed_tokens() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<SetScopes xmlns="http://www.onvif.org/ver10/device/wsdl">
  <Scopes>onvif://www.onvif.org/GetScopes</Scopes>
</SetScopes>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("SetScopesResponse"), "got: {resp}");

        let resp = roundtrip(
            &handler,
            r#"<AddScopes xmlns="http://www.onvif.org/ver10/device/wsdl">
  <ScopeItem>onvif://x/RemoveScopes</ScopeItem>
</AddScopes>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("AddScopesResponse"), "got: {resp}");
    }

    // --------------------------------------------------------------
    // Hostname
    // --------------------------------------------------------------

    #[tokio::test]
    async fn test_get_hostname_defaults_to_device_ip() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<GetHostname xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetHostnameResponse"));
        assert!(resp.contains("HostnameInformation"));
        assert!(resp.contains("<tt:FromDHCP>false</tt:FromDHCP>"));
        assert!(resp.contains("<tt:Name>192.168.1.100</tt:Name>"));
        assert_well_formed(&resp, "get_hostname");
    }

    #[tokio::test]
    async fn test_set_hostname_roundtrip() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<SetHostname xmlns="http://www.onvif.org/ver10/device/wsdl">
  <tt:Name>front-door-cam</tt:Name>
</SetHostname>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("SetHostnameResponse"));
        assert_well_formed(&resp, "set_hostname");

        let get = roundtrip(
            &handler,
            r#"<GetHostname xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(get.contains("<tt:Name>front-door-cam</tt:Name>"));
        assert!(!get.contains("192.168.1.100"));
    }

    #[tokio::test]
    async fn test_set_hostname_missing_name_fauls() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let result = roundtrip(
            &handler,
            r#"<SetHostname xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await;
        assert!(sender_fault(&result).contains("Name"));
    }

    #[tokio::test]
    async fn test_set_hostname_echoed_gethostname_routes_right() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<SetHostname xmlns="http://www.onvif.org/ver10/device/wsdl">
  <Name>GetHostname</Name>
</SetHostname>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("SetHostnameResponse"), "got: {resp}");
    }

    // --------------------------------------------------------------
    // Network statics
    // --------------------------------------------------------------

    #[tokio::test]
    async fn test_get_dns_static() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<GetDNS xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetDNSResponse"));
        assert!(resp.contains("<tds:DNSInformation>"));
        assert!(resp.contains("<tt:FromDHCP>false</tt:FromDHCP>"));
        assert_well_formed(&resp, "get_dns");
    }

    #[tokio::test]
    async fn test_get_ntp_static() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<GetNTP xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetNTPResponse"));
        assert!(resp.contains("<tds:NTPInformation>"));
        assert!(resp.contains("<tt:FromDHCP>false</tt:FromDHCP>"));
        assert_well_formed(&resp, "get_ntp");
    }

    #[tokio::test]
    async fn test_get_network_default_gateway_static() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<GetNetworkDefaultGateway xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetNetworkDefaultGatewayResponse"));
        assert!(resp.contains("<tds:NetworkGateway>"));
        assert!(resp.contains("<tt:IPv4Address>0.0.0.0</tt:IPv4Address>"));
        assert_well_formed(&resp, "get_gateway");
    }

    #[tokio::test]
    async fn test_get_network_protocols_http_port() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<GetNetworkProtocols xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetNetworkProtocolsResponse"));
        assert!(resp.contains("<tds:NetworkProtocols>"));
        assert!(resp.contains("<tt:Name>HTTP</tt:Name>"));
        assert!(resp.contains("<tt:Enabled>true</tt:Enabled>"));
        // Port comes from the configured ONVIF port (8080 in tests).
        assert!(resp.contains("<tt:Port>8080</tt:Port>"));
        assert_well_formed(&resp, "get_network_protocols");
    }

    #[tokio::test]
    async fn test_get_network_interfaces_empty() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<GetNetworkInterfaces xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetNetworkInterfacesResponse"));
        assert!(!resp.contains("NetworkInterfaces token"), "no entry");
        assert_well_formed(&resp, "get_network_interfaces");
    }

    /// The Set* network variants are deliberately unimplemented — they
    /// must fault (unknown action), never silently ack.
    #[tokio::test]
    async fn test_set_network_variants_not_implemented() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        for body in [
            r#"<SetDNS xmlns="http://www.onvif.org/ver10/device/wsdl"><FromDHCP>false</FromDHCP></SetDNS>"#,
            r#"<SetNTP xmlns="http://www.onvif.org/ver10/device/wsdl"><FromDHCP>false</FromDHCP></SetNTP>"#,
            r#"<SetNetworkInterfaces xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
            r#"<SetNetworkProtocols xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
            r#"<SetNetworkDefaultGateway xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        ] {
            let result = roundtrip(&handler, body).await;
            assert!(result.is_err(), "Set* must fault: {body}");
        }
    }

    // --------------------------------------------------------------
    // Discovery mode
    // --------------------------------------------------------------

    #[tokio::test]
    async fn test_get_discovery_mode_defaults_discoverable() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<GetDiscoveryMode xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetDiscoveryModeResponse"));
        assert!(resp.contains("<tds:DiscoveryMode>Discoverable</tds:DiscoveryMode>"));
        assert_well_formed(&resp, "get_discovery_mode");
    }

    #[tokio::test]
    async fn test_set_discovery_mode_roundtrip() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<SetDiscoveryMode xmlns="http://www.onvif.org/ver10/device/wsdl">
  <tt:DiscoveryMode>NonDiscoverable</tt:DiscoveryMode>
</SetDiscoveryMode>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("SetDiscoveryModeResponse"));

        let get = roundtrip(
            &handler,
            r#"<GetDiscoveryMode xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(get.contains("<tds:DiscoveryMode>NonDiscoverable</tds:DiscoveryMode>"));
    }

    #[tokio::test]
    async fn test_set_discovery_mode_invalid_fauls() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        for body in [
            r#"<SetDiscoveryMode xmlns="http://www.onvif.org/ver10/device/wsdl"><DiscoveryMode>Stealth</DiscoveryMode></SetDiscoveryMode>"#,
            r#"<SetDiscoveryMode xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        ] {
            let result = roundtrip(&handler, body).await;
            assert!(sender_fault(&result).contains("DiscoveryMode"));
        }
    }

    // --------------------------------------------------------------
    // Users: directory store (not the auth source — WS-Security is)
    // --------------------------------------------------------------

    #[tokio::test]
    async fn test_get_users_empty_by_default() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<GetUsers xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetUsersResponse"));
        assert!(!resp.contains("<tds:User>"));
        assert_well_formed(&resp, "get_users empty");
    }

    #[tokio::test]
    async fn test_with_users_seeds_directory() {
        let h = test_handlers()
            .with_users(vec![
                ("admin".into(), "Administrator".into()),
                ("viewer".into(), "User".into()),
            ])
            .unwrap();
        let handler = DeviceHandler(Arc::new(h));
        let resp = roundtrip(
            &handler,
            r#"<GetUsers xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("<tds:User>"));
        assert!(resp.contains("<tt:Username>admin</tt:Username>"));
        assert!(resp.contains("<tt:UserLevel>Administrator</tt:UserLevel>"));
        assert!(resp.contains("<tt:Username>viewer</tt:Username>"));
        assert_eq!(resp.matches("<tds:User>").count(), 2);
        assert_well_formed(&resp, "get_users seeded");
    }

    #[test]
    fn test_with_users_rejects_invalid_level() {
        let result = test_handlers().with_users(vec![("a".into(), "Root".into())]);
        assert!(result.is_err(), "invalid UserLevel must be rejected");
    }

    #[test]
    fn test_with_users_rejects_duplicates() {
        let result = test_handlers().with_users(vec![
            ("a".into(), "Administrator".into()),
            ("a".into(), "User".into()),
        ]);
        assert!(result.is_err(), "duplicate usernames must be rejected");
    }

    const CREATE_USERS_BODY: &str = r#"<CreateUsers xmlns="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
  <tt:User>
    <tt:Username>alice</tt:Username>
    <tt:Password>s3cret</tt:Password>
    <tt:UserLevel>Administrator</tt:UserLevel>
  </tt:User>
  <tt:User>
    <tt:Username>bob</tt:Username>
    <tt:Password>hunter2</tt:Password>
    <tt:UserLevel>Operator</tt:UserLevel>
  </tt:User>
</CreateUsers>"#;

    #[tokio::test]
    async fn test_create_users_appends() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(&handler, CREATE_USERS_BODY).await.unwrap();
        assert!(resp.contains("CreateUsersResponse"));
        assert_well_formed(&resp, "create_users");

        let get = roundtrip(
            &handler,
            r#"<GetUsers xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert_eq!(get.matches("<tds:User>").count(), 2);
        assert!(get.contains("<tt:Username>alice</tt:Username>"));
        assert!(get.contains("<tt:UserLevel>Operator</tt:UserLevel>"));
    }

    /// All-or-nothing per WSDL: a fault in any entry must leave the
    /// store untouched.
    #[tokio::test]
    async fn test_create_users_invalid_level_rejected_atomically() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let body = r#"<CreateUsers xmlns="http://www.onvif.org/ver10/device/wsdl">
  <User><Username>alice</Username><Password>p1</Password><UserLevel>Administrator</UserLevel></User>
  <User><Username>bob</Username><Password>p2</Password><UserLevel>Superuser</UserLevel></User>
</CreateUsers>"#;
        let result = roundtrip(&handler, body).await;
        assert!(sender_fault(&result).contains("UserLevel"));

        let get = roundtrip(
            &handler,
            r#"<GetUsers xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(!get.contains("alice"), "store must stay untouched");
    }

    #[tokio::test]
    async fn test_create_users_duplicate_username_fauls() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        roundtrip(&handler, CREATE_USERS_BODY).await.unwrap();

        let dup_vs_store = r#"<CreateUsers xmlns="http://www.onvif.org/ver10/device/wsdl">
  <User><Username>alice</Username><Password>p</Password><UserLevel>User</UserLevel></User>
</CreateUsers>"#;
        let result = roundtrip(&handler, dup_vs_store).await;
        assert!(sender_fault(&result).contains("alice"));

        let dup_in_request = r#"<CreateUsers xmlns="http://www.onvif.org/ver10/device/wsdl">
  <User><Username>carol</Username><Password>p</Password><UserLevel>User</UserLevel></User>
  <User><Username>carol</Username><Password>q</Password><UserLevel>Operator</UserLevel></User>
</CreateUsers>"#;
        let result = roundtrip(&handler, dup_in_request).await;
        assert!(sender_fault(&result).contains("carol"));
    }

    /// WSDL annotation: "If password is missing, then fault message Too
    /// weak password is returned."
    #[tokio::test]
    async fn test_create_users_missing_password_fauls() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let body = r#"<CreateUsers xmlns="http://www.onvif.org/ver10/device/wsdl">
  <User><Username>alice</Username><UserLevel>User</UserLevel></User>
</CreateUsers>"#;
        let result = roundtrip(&handler, body).await;
        assert!(sender_fault(&result).contains("password"));
    }

    #[tokio::test]
    async fn test_create_users_missing_username_fauls() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let body = r#"<CreateUsers xmlns="http://www.onvif.org/ver10/device/wsdl">
  <User><Password>p</Password><UserLevel>User</UserLevel></User>
</CreateUsers>"#;
        let result = roundtrip(&handler, body).await;
        assert!(sender_fault(&result).contains("Username"));
    }

    #[tokio::test]
    async fn test_delete_users_removes() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        roundtrip(&handler, CREATE_USERS_BODY).await.unwrap();
        let resp = roundtrip(
            &handler,
            r#"<DeleteUsers xmlns="http://www.onvif.org/ver10/device/wsdl">
  <tt:Username>bob</tt:Username>
</DeleteUsers>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("DeleteUsersResponse"));

        let get = roundtrip(
            &handler,
            r#"<GetUsers xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(!get.contains("bob"));
        assert!(get.contains("alice"));
    }

    #[tokio::test]
    async fn test_delete_users_missing_fauls() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let result = roundtrip(
            &handler,
            r#"<DeleteUsers xmlns="http://www.onvif.org/ver10/device/wsdl">
  <Username>ghost</Username>
</DeleteUsers>"#,
        )
        .await;
        assert!(sender_fault(&result).contains("ghost"));
    }

    #[tokio::test]
    async fn test_set_user_updates_level() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        roundtrip(&handler, CREATE_USERS_BODY).await.unwrap();
        let resp = roundtrip(
            &handler,
            r#"<SetUser xmlns="http://www.onvif.org/ver10/device/wsdl">
  <tt:User>
    <tt:Username>bob</tt:Username>
    <tt:Password>new-pass</tt:Password>
    <tt:UserLevel>User</tt:UserLevel>
  </tt:User>
</SetUser>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("SetUserResponse"));

        let get = roundtrip(
            &handler,
            r#"<GetUsers xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(get.contains("<tt:UserLevel>User</tt:UserLevel>"));
        assert!(!get.contains("Operator"));
    }

    #[tokio::test]
    async fn test_set_user_missing_user_fauls() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let result = roundtrip(
            &handler,
            r#"<SetUser xmlns="http://www.onvif.org/ver10/device/wsdl">
  <User><Username>ghost</Username><UserLevel>User</UserLevel></User>
</SetUser>"#,
        )
        .await;
        assert!(sender_fault(&result).contains("ghost"));
    }

    /// The directory never echoes passwords — the WS-Security layer is
    /// the real credential store.
    #[tokio::test]
    async fn test_get_users_never_exposes_passwords() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        roundtrip(&handler, CREATE_USERS_BODY).await.unwrap();
        let get = roundtrip(
            &handler,
            r#"<GetUsers xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(!get.contains("s3cret"));
        assert!(!get.contains("hunter2"));
        assert!(!get.contains("Password"));
    }

    #[tokio::test]
    async fn test_username_escaped_in_get_users() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        roundtrip(
            &handler,
            r#"<CreateUsers xmlns="http://www.onvif.org/ver10/device/wsdl">
  <User><Username>a&lt;&amp;&gt;b</Username><Password>p</Password><UserLevel>User</UserLevel></User>
</CreateUsers>"#,
        )
        .await
        .unwrap();
        let get = roundtrip(
            &handler,
            r#"<GetUsers xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(get.contains("a&lt;&amp;&gt;b"), "got: {get}");
        assert_well_formed(&get, "escaped usernames");
    }

    #[tokio::test]
    async fn test_users_family_dispatch_survives_echoed_tokens() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<CreateUsers xmlns="http://www.onvif.org/ver10/device/wsdl">
  <User><Username>GetUsers</Username><Password>p</Password><UserLevel>User</UserLevel></User>
</CreateUsers>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("CreateUsersResponse"), "got: {resp}");

        let resp = roundtrip(
            &handler,
            r#"<DeleteUsers xmlns="http://www.onvif.org/ver10/device/wsdl">
  <Username>SetUser</Username>
</DeleteUsers>"#,
        )
        .await;
        // SetUser is checked after DeleteUsers — the echoed token in the
        // username must not steal the routing.
        assert!(sender_fault(&resp).contains("SetUser"));
    }

    // --------------------------------------------------------------
    // Service capabilities / WSDL URL / endpoint reference
    // --------------------------------------------------------------

    #[tokio::test]
    async fn test_get_device_service_capabilities_shape() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        // The Device service's capabilities action is GetServiceCapabilities
        // (the response carries the DeviceServiceCapabilities type).
        let resp = roundtrip(
            &handler,
            r#"<GetServiceCapabilities xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetServiceCapabilitiesResponse"));
        assert!(resp.contains("<tds:Capabilities>"));
        // WSDL DeviceServiceCapabilities: required Network/Security/System
        // child elements (all flags default false = honest minimal).
        assert!(resp.contains("<tds:Network/>"));
        assert!(resp.contains("<tds:Security/>"));
        assert!(resp.contains("<tds:System/>"));
        assert_well_formed(&resp, "device service capabilities");
    }

    #[tokio::test]
    async fn test_get_wsdl_url_uses_request_ip() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = handler
            .handle(
                r#"<GetWsdlUrl xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
                &test_info("172.16.0.9"),
            )
            .await
            .unwrap();
        assert!(resp.contains("GetWsdlUrlResponse"));
        assert!(resp.contains("http://172.16.0.9:8080/onvif/device_service?wsdl"));
        // Loopback falls back to the startup device_ip.
        let resp = handler
            .handle(
                r#"<GetWsdlUrl xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
                &test_info("127.0.0.1"),
            )
            .await
            .unwrap();
        assert!(resp.contains("http://192.168.1.100:8080/onvif/device_service?wsdl"));
    }

    #[tokio::test]
    async fn test_get_endpoint_reference_static_guid() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<GetEndpointReference xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetEndpointReferenceResponse"));
        assert!(resp.contains("<tds:GUID>urn:uuid:00000000-0000-0000-0000-000000000000</tds:GUID>"));
        assert_well_formed(&resp, "endpoint reference");
    }

    // --------------------------------------------------------------
    // System log / support information (hook-sourced text)
    // --------------------------------------------------------------

    #[tokio::test]
    async fn test_get_system_log_from_hook() {
        let (svc, _) = hooked_handlers();
        let handler = DeviceHandler(Arc::new(svc));
        let resp = roundtrip(
            &handler,
            r#"<GetSystemLog xmlns="http://www.onvif.org/ver10/device/wsdl">
  <LogType>System</LogType>
</GetSystemLog>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetSystemLogResponse"));
        assert!(resp.contains("<tds:SystemLog>"));
        assert!(resp.contains("<tt:String>line-1 &amp; &lt;line-2&gt;</tt:String>"));
        assert_well_formed(&resp, "system log");
    }

    #[tokio::test]
    async fn test_get_system_log_without_hook() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = roundtrip(
            &handler,
            r#"<GetSystemLog xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetSystemLogResponse"));
        assert!(resp.contains("<tds:SystemLog>"));
        assert_well_formed(&resp, "system log no hook");
    }

    #[tokio::test]
    async fn test_get_system_support_information_from_hook() {
        let (svc, _) = hooked_handlers();
        let handler = DeviceHandler(Arc::new(svc));
        let resp = roundtrip(
            &handler,
            r#"<GetSystemSupportInformation xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("GetSystemSupportInformationResponse"));
        assert!(resp.contains("<tds:SupportInformation>"));
        assert!(resp.contains("support &amp; &lt;info&gt;"));
        assert_well_formed(&resp, "support info");
    }

    // --------------------------------------------------------------
    // Factory default / firmware / restore — protocol-answer family
    // --------------------------------------------------------------

    #[tokio::test]
    async fn test_set_system_factory_default_hard_soft_hooks() {
        let (svc, hooks) = hooked_handlers();
        let handler = DeviceHandler(Arc::new(svc));
        let resp = roundtrip(
            &handler,
            r#"<SetSystemFactoryDefault xmlns="http://www.onvif.org/ver10/device/wsdl">
  <tt:FactoryDefault>Hard</tt:FactoryDefault>
</SetSystemFactoryDefault>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("SetSystemFactoryDefaultResponse"));

        roundtrip(
            &handler,
            r#"<SetSystemFactoryDefault xmlns="http://www.onvif.org/ver10/device/wsdl">
  <FactoryDefault>Soft</FactoryDefault>
</SetSystemFactoryDefault>"#,
        )
        .await
        .unwrap();
        assert_eq!(
            hooks.snapshot(),
            vec![
                "factory_default hard=true".to_string(),
                "factory_default hard=false".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn test_set_system_factory_default_invalid_fauls() {
        let (svc, hooks) = hooked_handlers();
        let handler = DeviceHandler(Arc::new(svc));
        for body in [
            r#"<SetSystemFactoryDefault xmlns="http://www.onvif.org/ver10/device/wsdl"><FactoryDefault>FactoryReset</FactoryDefault></SetSystemFactoryDefault>"#,
            r#"<SetSystemFactoryDefault xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
        ] {
            let result = roundtrip(&handler, body).await;
            assert!(sender_fault(&result).contains("FactoryDefault"));
        }
        assert!(hooks.snapshot().is_empty());
    }

    #[tokio::test]
    async fn test_upgrade_system_firmware_acks() {
        let (svc, hooks) = hooked_handlers();
        let handler = DeviceHandler(Arc::new(svc));
        let resp = roundtrip(
            &handler,
            r#"<UpgradeSystemFirmware xmlns="http://www.onvif.org/ver10/device/wsdl">
  <Firmware><tt:ContentType>application/octet-stream</tt:ContentType></Firmware>
</UpgradeSystemFirmware>"#,
        )
        .await
        .unwrap();
        assert!(resp.contains("UpgradeSystemFirmwareResponse"));
        assert_well_formed(&resp, "upgrade firmware");
        assert!(hooks.snapshot().is_empty(), "no hook for firmware");
    }

    #[tokio::test]
    async fn test_start_system_restore_shape() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        let resp = handler
            .handle(
                r#"<StartSystemRestore xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#,
                &test_info("127.0.0.1"),
            )
            .await
            .unwrap();
        assert!(resp.contains("StartSystemRestoreResponse"));
        // WSDL-required elements.
        assert!(resp.contains(
            "<tds:UploadUri>http://192.168.1.100:8080/onvif/device_service</tds:UploadUri>"
        ));
        assert!(resp.contains("<tds:ExpectedDownTime>PT0S</tds:ExpectedDownTime>"));
        assert_well_formed(&resp, "start system restore");
    }

    /// SystemReboot keeps its byte-stable answer and now also fires the
    /// host hook.
    #[tokio::test]
    async fn test_system_reboot_fires_hook() {
        let (svc, hooks) = hooked_handlers();
        let handler = DeviceHandler(Arc::new(svc));
        let body = r#"<SystemReboot xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#;
        let resp = roundtrip(&handler, body).await.unwrap();
        assert!(resp.contains("SystemRebootResponse"));
        assert!(resp.contains("Device rebooting"));
        assert_eq!(hooks.snapshot(), vec!["reboot".to_string()]);
    }

    // --------------------------------------------------------------
    // Exhaustive dispatch: every action routes to its own response
    // --------------------------------------------------------------

    /// Bodies of every Device action must route to their own response —
    /// pins the substring-dispatch ordering against regressions when a
    /// new arm is inserted (issue #49).
    #[tokio::test]
    async fn test_all_device_actions_route_to_own_response() {
        let handler = DeviceHandler(Arc::new(test_handlers()));
        // (action, inner body content)
        let read_actions = [
            "GetSystemDateAndTime",
            "GetDeviceInformation",
            "GetCapabilities",
            "GetServices",
            "GetServiceCapabilities",
            "GetScopes",
            "SystemReboot",
            "GetHostname",
            "GetDNS",
            "GetNTP",
            "GetNetworkInterfaces",
            "GetNetworkDefaultGateway",
            "GetNetworkProtocols",
            "GetDiscoveryMode",
            "GetUsers",
            "GetWsdlUrl",
            "GetEndpointReference",
            "GetSystemLog",
            "GetSystemSupportInformation",
            "StartSystemRestore",
        ];
        for action in read_actions {
            let body = format!(r#"<{action} xmlns="{DEVICE_SERVICE}"/>"#);
            let resp = roundtrip(&handler, &body)
                .await
                .unwrap_or_else(|e| panic!("{action} must answer, got error {e:?}"));
            assert!(
                resp.contains(&format!("{action}Response")),
                "{action} misrouted: {resp}"
            );
        }

        let write_actions_with_bodies: &[(&str, &str)] = &[
            ("SetSystemDateAndTime", SET_DATE_TIME_BODY_TT),
            (
                "SetScopes",
                r#"<Scopes>onvif://www.onvif.org/type/audio_encoder</Scopes>"#,
            ),
            ("AddScopes", r#"<ScopeItem>onvif://x/added</ScopeItem>"#),
            (
                "RemoveScopes",
                r#"<ScopeItem>onvif://x/removed</ScopeItem>"#,
            ),
            ("SetHostname", r#"<Name>cam</Name>"#),
            (
                "SetDiscoveryMode",
                r#"<DiscoveryMode>Discoverable</DiscoveryMode>"#,
            ),
            (
                "CreateUsers",
                r#"<User><Username>u</Username><Password>p</Password><UserLevel>User</UserLevel></User>"#,
            ),
            (
                "SetUser",
                r#"<User><Username>u</Username><UserLevel>Operator</UserLevel></User>"#,
            ),
            ("DeleteUsers", r#"<Username>u</Username>"#),
            (
                "SetSystemFactoryDefault",
                r#"<FactoryDefault>Soft</FactoryDefault>"#,
            ),
            (
                "UpgradeSystemFirmware",
                r#"<Firmware><ContentType>application/octet-stream</ContentType></Firmware>"#,
            ),
        ];
        // Seed the user directory so DeleteUsers/SetUser find their user.
        roundtrip(&handler, CREATE_USERS_BODY).await.unwrap();
        for (action, inner) in write_actions_with_bodies {
            let body = format!(r#"<{action} xmlns="{DEVICE_SERVICE}">{inner}</{action}>"#);
            let resp = roundtrip(&handler, &body)
                .await
                .unwrap_or_else(|e| panic!("{action} must answer, got {e:?}"));
            assert!(
                resp.contains(&format!("{action}Response")),
                "{action} misrouted: {resp}"
            );
        }
    }

    // --------------------------------------------------------------
    // Tolerant element-text collection (parser unit tests)
    // --------------------------------------------------------------

    #[test]
    fn test_collect_texts_namespaced_and_entities() {
        let body = r#"<Root xmlns="urn:x" xmlns:tt="urn:y">
  <tt:Item>a&amp;b</tt:Item>
  <Item attr="1"> plain </Item>
  <Other>skip</Other>
  <Item/>
</Root>"#;
        let texts = collect_texts(body, &["Item"]).unwrap();
        assert_eq!(texts, vec!["a&b".to_string(), "plain".to_string()]);
    }

    #[test]
    fn test_collect_texts_malformed_xml_errors() {
        assert!(collect_texts("<<<<", &["Item"]).is_err());
        assert!(collect_texts("", &["Item"]).unwrap().is_empty());
    }
}

// ---------------------------------------------------------------------------
// Security ops tests (issue #54)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod security_ops_tests {
    use super::*;
    use crate::types::AuthResult;

    fn handlers() -> DeviceServiceHandlers {
        DeviceServiceHandlers::new(
            DeviceConfig {
                name: "Sec Cam".into(),
                manufacturer: "Vendor".into(),
                model: "Model".into(),
                firmware: "1.0.0".into(),
                hardware_id: "HW".into(),
                serial_number: "SN".into(),
            },
            8080,
            "10.1.1.1".to_string(),
        )
        .unwrap()
    }

    fn info() -> RequestInfo {
        RequestInfo {
            client_ip: "10.0.0.1".to_string(),
            server_ip: "10.0.0.5".to_string(),
            auth_result: AuthResult {
                username: "admin".into(),
                authenticated: true,
            },
        }
    }

    fn filter_state(f: IpFilter) -> IpFilterState {
        Arc::new(std::sync::RwLock::new(f))
    }

    fn entry(ip: &str, plen: u8) -> IpEntry {
        IpEntry {
            ipv4: ip.to_string(),
            prefix_len: plen,
        }
    }

    fn assert_well_formed(xml: &str) {
        let mut reader = quick_xml::Reader::from_str(xml);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Err(e) => panic!("XML parse error: {e} in\n{xml}"),
                _ => {}
            }
            buf.clear();
        }
    }

    // ------------------------------------------------------------------
    // Filter matching
    // ------------------------------------------------------------------

    #[test]
    fn ip_filter_disabled_allows_everything() {
        assert!(IpFilter::disabled().allows_client_ip("10.0.0.1"));
        // Entries present but disabled: still no enforcement.
        let f = IpFilter {
            enabled: false,
            mode: IpFilterMode::Deny,
            entries: vec![entry("10.0.0.1", 32)],
        };
        assert!(f.allows_client_ip("10.0.0.1"));
    }

    #[test]
    fn ip_filter_allow_mode_admits_only_matches() {
        let f = IpFilter {
            enabled: true,
            mode: IpFilterMode::Allow,
            entries: vec![entry("192.168.1.0", 24)],
        };
        assert!(f.allows_client_ip("192.168.1.77"));
        assert!(!f.allows_client_ip("192.168.2.1"));
        assert!(!f.allows_client_ip("10.0.0.1"));
    }

    #[test]
    fn ip_filter_deny_mode_refuses_only_matches() {
        let f = IpFilter {
            enabled: true,
            mode: IpFilterMode::Deny,
            entries: vec![entry("10.9.9.5", 32)],
        };
        assert!(!f.allows_client_ip("10.9.9.5"));
        assert!(f.allows_client_ip("10.9.9.6"));
    }

    #[test]
    fn ip_filter_prefix_bounds() {
        let all = IpFilter {
            enabled: true,
            mode: IpFilterMode::Allow,
            entries: vec![entry("0.0.0.0", 0)],
        };
        assert!(all.allows_client_ip("203.0.113.9"));

        let host = IpFilter {
            enabled: true,
            mode: IpFilterMode::Allow,
            entries: vec![entry("198.51.100.7", 32)],
        };
        assert!(host.allows_client_ip("198.51.100.7"));
        assert!(!host.allows_client_ip("198.51.100.8"));
    }

    /// IPv6 / unparseable peer addresses fail OPEN (documented): a
    /// malformed address must not brick the listener.
    #[test]
    fn ip_filter_ipv6_and_garbage_fail_open() {
        let deny_all_v4 = IpFilter {
            enabled: true,
            mode: IpFilterMode::Deny,
            entries: vec![entry("0.0.0.0", 0)],
        };
        assert!(!deny_all_v4.allows_client_ip("10.0.0.1"), "IPv4 is denied");
        assert!(deny_all_v4.allows_client_ip("::1"));
        assert!(deny_all_v4.allows_client_ip("not-an-ip"));
    }

    #[test]
    fn ip_filter_invalid_entries_never_match() {
        let f = IpFilter {
            enabled: true,
            mode: IpFilterMode::Allow,
            entries: vec![entry("garbage", 24), entry("10.0.0.0", 40)],
        };
        assert!(!f.allows_client_ip("192.168.0.1"));
        assert!(!f.allows_client_ip("10.0.0.1"));
    }

    // ------------------------------------------------------------------
    // GetIPAddressFilter wire shape
    // ------------------------------------------------------------------

    #[test]
    fn get_ip_filter_without_state_reports_disabled() {
        let xml = handlers().sec_build_get_ip_address_filter();
        assert!(xml.contains("tds:GetIPAddressFilterResponse"), "{xml}");
        assert!(xml.contains("<tds:IPAddressFilter>"), "{xml}");
        assert!(xml.contains("<tt:Type>Allow</tt:Type>"), "{xml}");
        assert!(!xml.contains("tt:IPv4Address"), "{xml}");
        assert_well_formed(&xml);
    }

    #[test]
    fn get_ip_filter_serializes_entries() {
        let state = filter_state(IpFilter {
            enabled: true,
            mode: IpFilterMode::Deny,
            entries: vec![entry("192.168.0.0", 16), entry("10.1.2.3", 32)],
        });
        let xml = handlers()
            .with_ip_filter(state)
            .sec_build_get_ip_address_filter();
        assert!(xml.contains("<tt:Type>Deny</tt:Type>"), "{xml}");
        assert!(
            xml.contains("<tt:Address>192.168.0.0</tt:Address>"),
            "{xml}"
        );
        assert!(
            xml.contains("<tt:PrefixLength>16</tt:PrefixLength>"),
            "{xml}"
        );
        assert!(xml.contains("<tt:Address>10.1.2.3</tt:Address>"), "{xml}");
        assert!(
            xml.contains("<tt:PrefixLength>32</tt:PrefixLength>"),
            "{xml}"
        );
        assert_eq!(xml.matches("tt:IPv4Address").count(), 4, "{xml}"); // open+close per entry
        assert_well_formed(&xml);
    }

    // ------------------------------------------------------------------
    // Request parsing
    // ------------------------------------------------------------------

    fn set_filter_body(mode: &str, entries: &[(&str, &str)], prefixed: bool) -> String {
        let (pt, pv, pe, pp) = if prefixed {
            ("tt:Type", "tt:IPv4Address", "tt:Address", "tt:PrefixLength")
        } else {
            ("Type", "IPv4Address", "Address", "PrefixLength")
        };
        let mut x = format!(
            "<SetIPAddressFilter xmlns=\"http://www.onvif.org/ver10/device/wsdl\">\
             <IPAddressFilter><{pt}>{mode}</{pt}>"
        );
        for (addr, plen) in entries {
            x.push_str(&format!(
                "<{pv}><{pe}>{addr}</{pe}><{pp}>{plen}</{pp}></{pv}>"
            ));
        }
        x.push_str("</IPAddressFilter></SetIPAddressFilter>");
        x
    }

    #[test]
    fn parse_ip_filter_tolerates_tt_prefixes() {
        let f = sec_parse_ip_filter(&set_filter_body("Deny", &[("192.168.0.0", "16")], true))
            .expect("prefixed body must parse");
        assert_eq!(f.mode, IpFilterMode::Deny);
        assert_eq!(f.entries, vec![entry("192.168.0.0", 16)]);
    }

    #[test]
    fn parse_ip_filter_tolerates_default_namespace() {
        let f = sec_parse_ip_filter(&set_filter_body("Allow", &[("10.0.0.0", "8")], false))
            .expect("unprefixed body must parse");
        assert_eq!(f.mode, IpFilterMode::Allow);
        assert_eq!(f.entries, vec![entry("10.0.0.0", 8)]);
    }

    #[test]
    fn parse_ip_filter_rejects_bad_type() {
        let err = sec_parse_ip_filter(&set_filter_body("Maybe", &[], false)).unwrap_err();
        assert!(err.to_string().contains("Type"), "{err}");
    }

    #[test]
    fn parse_ip_filter_rejects_ipv6_entries() {
        let body = "<SetIPAddressFilter><IPAddressFilter><Type>Allow</Type>\
                    <IPv6Address><Address>fe80::1</Address><PrefixLength>10</PrefixLength></IPv6Address>\
                    </IPAddressFilter></SetIPAddressFilter>";
        let err = sec_parse_ip_filter(body).unwrap_err();
        assert!(err.to_string().contains("IPv6"), "{err}");
    }

    #[test]
    fn parse_ip_filter_rejects_incomplete_entry() {
        let body = "<SetIPAddressFilter><IPAddressFilter><Type>Allow</Type>\
                    <IPv4Address><Address>10.0.0.0</Address></IPv4Address>\
                    </IPAddressFilter></SetIPAddressFilter>";
        let err = sec_parse_ip_filter(body).unwrap_err();
        assert!(err.to_string().contains("PrefixLength"), "{err}");
    }

    #[test]
    fn parse_ip_filter_rejects_bad_address() {
        let err = sec_parse_ip_filter(&set_filter_body("Allow", &[("300.1.2.3", "24")], false))
            .unwrap_err();
        assert!(err.to_string().contains("Address"), "{err}");
    }

    #[test]
    fn parse_ip_filter_rejects_prefix_over_32() {
        let err = sec_parse_ip_filter(&set_filter_body("Allow", &[("10.0.0.0", "33")], false))
            .unwrap_err();
        assert!(err.to_string().contains("PrefixLength"), "{err}");
    }

    // ------------------------------------------------------------------
    // Handler dispatch round-trips (Set/Add/Remove/Get over the store)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn set_then_get_ip_filter_roundtrip() {
        let state = filter_state(IpFilter::disabled());
        let svc = Arc::new(handlers().with_ip_filter(state.clone()));
        let handler = DeviceHandler(svc);
        let ri = info();

        let resp = handler
            .handle(
                &set_filter_body("Allow", &[("172.16.0.0", "12")], false),
                &ri,
            )
            .await
            .unwrap();
        assert!(resp.contains("tds:SetIPAddressFilterResponse"), "{resp}");

        // Mutations mark the filter enabled in the shared state.
        assert!(state.read().unwrap().enabled);

        let get = handler
            .handle(
                "<GetIPAddressFilter xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>",
                &ri,
            )
            .await
            .unwrap();
        assert!(get.contains("<tt:Type>Allow</tt:Type>"), "{get}");
        assert!(get.contains("<tt:Address>172.16.0.0</tt:Address>"), "{get}");
        assert!(
            get.contains("<tt:PrefixLength>12</tt:PrefixLength>"),
            "{get}"
        );
    }

    #[tokio::test]
    async fn add_appends_then_set_replaces() {
        let state = filter_state(IpFilter::disabled());
        let svc = Arc::new(handlers().with_ip_filter(state));
        let handler = DeviceHandler(svc);
        let ri = info();

        handler
            .handle(&set_filter_body("Allow", &[("10.0.0.0", "8")], false), &ri)
            .await
            .unwrap();
        let resp = handler
            .handle(
                "<AddIPAddressFilter xmlns=\"http://www.onvif.org/ver10/device/wsdl\">\
                 <IPAddressFilter><Type>Allow</Type>\
                 <IPv4Address><Address>192.168.0.0</Address><PrefixLength>16</PrefixLength></IPv4Address>\
                 </IPAddressFilter></AddIPAddressFilter>",
                &ri,
            )
            .await
            .unwrap();
        assert!(resp.contains("tds:AddIPAddressFilterResponse"), "{resp}");

        let get = handler.handle("<GetIPAddressFilter/>", &ri).await.unwrap();
        assert!(
            get.contains("10.0.0.0") && get.contains("192.168.0.0"),
            "{get}"
        );

        // Set replaces wholesale.
        handler
            .handle(&set_filter_body("Deny", &[("10.9.0.0", "16")], false), &ri)
            .await
            .unwrap();
        let get = handler.handle("<GetIPAddressFilter/>", &ri).await.unwrap();
        assert!(get.contains("<tt:Type>Deny</tt:Type>"), "{get}");
        assert!(get.contains("10.9.0.0"), "{get}");
        assert!(!get.contains("192.168.0.0"), "{get}");
    }

    #[tokio::test]
    async fn remove_removes_matching_entry() {
        let state = filter_state(IpFilter::disabled());
        let svc = Arc::new(handlers().with_ip_filter(state));
        let handler = DeviceHandler(svc);
        let ri = info();

        handler
            .handle(
                &set_filter_body("Deny", &[("10.1.0.0", "16"), ("10.2.0.0", "16")], false),
                &ri,
            )
            .await
            .unwrap();
        let resp = handler
            .handle(
                "<RemoveIPAddressFilter xmlns=\"http://www.onvif.org/ver10/device/wsdl\">\
                 <IPAddressFilter><Type>Deny</Type>\
                 <IPv4Address><Address>10.1.0.0</Address><PrefixLength>16</PrefixLength></IPv4Address>\
                 </IPAddressFilter></RemoveIPAddressFilter>",
                &ri,
            )
            .await
            .unwrap();
        assert!(resp.contains("tds:RemoveIPAddressFilterResponse"), "{resp}");

        let get = handler.handle("<GetIPAddressFilter/>", &ri).await.unwrap();
        assert!(get.contains("10.2.0.0"), "{get}");
        assert!(!get.contains("10.1.0.0"), "{get}");
        // Mode is kept on remove.
        assert!(get.contains("<tt:Type>Deny</tt:Type>"), "{get}");
    }

    #[tokio::test]
    async fn set_ip_filter_without_state_refused() {
        let handler = DeviceHandler(Arc::new(handlers()));
        let result = handler
            .handle(
                &set_filter_body("Allow", &[("10.0.0.0", "8")], false),
                &info(),
            )
            .await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not configured"), "{err}");
    }

    // ------------------------------------------------------------------
    // AccessPolicy
    // ------------------------------------------------------------------

    #[test]
    fn get_access_policy_default_empty() {
        let xml = handlers().sec_build_get_access_policy();
        assert!(xml.contains("tds:GetAccessPolicyResponse"), "{xml}");
        assert!(xml.contains("tds:PolicyFile"), "{xml}");
        assert!(xml.contains("tt:Data"), "{xml}");
        assert_well_formed(&xml);
    }

    #[tokio::test]
    async fn access_policy_roundtrip() {
        use base64::Engine as _;

        let state: AccessPolicyState = Arc::new(std::sync::RwLock::new(Vec::new()));
        let svc = Arc::new(handlers().with_access_policy(state));
        let handler = DeviceHandler(svc);
        let ri = info();

        let blob = b"<policy>allow admin</policy>".as_slice();
        let b64 = base64::engine::general_purpose::STANDARD.encode(blob);
        let set_body = format!(
            "<SetAccessPolicy xmlns=\"http://www.onvif.org/ver10/device/wsdl\">\
             <PolicyFile><Data>{b64}</Data></PolicyFile></SetAccessPolicy>"
        );
        let resp = handler.handle(&set_body, &ri).await.unwrap();
        assert!(resp.contains("tds:SetAccessPolicyResponse"), "{resp}");

        let get = handler
            .handle(
                "<GetAccessPolicy xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>",
                &ri,
            )
            .await
            .unwrap();
        assert!(get.contains(&format!("<tt:Data>{b64}</tt:Data>")), "{get}");
    }

    #[tokio::test]
    async fn set_access_policy_invalid_base64_refused() {
        let state: AccessPolicyState = Arc::new(std::sync::RwLock::new(Vec::new()));
        let handler = DeviceHandler(Arc::new(handlers().with_access_policy(state)));
        let body = "<SetAccessPolicy><PolicyFile><Data>!!not-base64!!</Data></PolicyFile></SetAccessPolicy>";
        let err = handler.handle(body, &info()).await.unwrap_err();
        assert!(err.to_string().contains("base64"), "{err}");
    }

    #[tokio::test]
    async fn set_access_policy_without_state_refused() {
        let handler = DeviceHandler(Arc::new(handlers()));
        let body = "<SetAccessPolicy><PolicyFile><Data>AAAA</Data></PolicyFile></SetAccessPolicy>";
        let err = handler.handle(body, &info()).await.unwrap_err();
        assert!(err.to_string().contains("not configured"), "{err}");
    }

    // ------------------------------------------------------------------
    // Builder seam
    // ------------------------------------------------------------------

    #[test]
    fn with_ip_filter_exposes_the_same_state() {
        let state = filter_state(IpFilter::disabled());
        let h = handlers().with_ip_filter(state.clone());
        let exposed = h.ip_filter_state().expect("state must be exposed");
        assert!(Arc::ptr_eq(&exposed, &state));
        assert!(handlers().ip_filter_state().is_none());
    }

    #[test]
    fn with_access_policy_exposes_the_same_state() {
        let state: AccessPolicyState = Arc::new(std::sync::RwLock::new(Vec::new()));
        let h = handlers().with_access_policy(state.clone());
        let exposed = h.access_policy_state().expect("state must be exposed");
        assert!(Arc::ptr_eq(&exposed, &state));
        assert!(handlers().access_policy_state().is_none());
    }
}
