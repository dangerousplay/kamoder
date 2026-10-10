use std::collections::{HashMap, HashSet};
use std::str;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{NamespaceResolver, PrefixDeclaration};
use quick_xml::Reader;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::auth::verify_username_token;
use crate::events::{
    EventsService, ServiceEndpoint, EVENTS_SERVICE_PATH, SUBSCRIPTION_PATH_PREFIX,
};
use crate::media2::{Media2Service, MEDIA2_SERVICE_PATH};
use crate::types::{serialize_soap_fault, AuthResult, OnvifError, RequestInfo, UsernameToken};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Default cap on a request body (SOAP envelopes are a few KiB).
const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;
/// Default per-connection read timeout.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on the HTTP header section.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Digest challenge realm (issue #54). Fixed on purpose: the realm is
/// a scoping/display label for this single-device service.
const HTTP_DIGEST_REALM: &str = "onvif";
/// Lifetime of an issued HTTP Digest nonce (issue #54): after this the
/// nonce answers `stale=TRUE` and the client re-challenges.
const DIGEST_NONCE_TTL_SECS: u64 = 300;

/// Configuration for the ONVIF SOAP server.
///
/// Auth is **fail-closed**: starting the server with an empty password and
/// `allow_no_auth = false` (the default) is a configuration error. Hosts
/// that deliberately want an open server must set `allow_no_auth = true`.
#[derive(Debug, Clone)]
pub struct OnvifConfig {
    /// Port to bind (`OnvifServer::start`). Ignored by [`OnvifServer::start_on`].
    pub port: u16,
    /// WS-UsernameToken username expected from clients.
    pub username: String,
    /// WS-UsernameToken password expected from clients.
    pub password: String,
    /// Explicitly permit running without authentication when the password is
    /// empty (every action becomes open). Default `false` — fail closed.
    pub allow_no_auth: bool,
    /// Maximum accepted HTTP request body in bytes. Larger bodies are
    /// rejected with 413. Default 1 MiB.
    pub max_body_bytes: usize,
    /// Per-connection read timeout (header + body). Default 30 s.
    pub read_timeout: Duration,
    /// UsernameToken Created freshness window in seconds; replayed nonces
    /// are rejected within it. 0 disables the replay guard. Default 300.
    pub replay_window_secs: u64,
    /// Authentication failures (bad credentials) from one source before a
    /// lockout; negative disables the limiter. Default 5.
    pub auth_failure_limit: i32,
    /// How long a locked-out source is refused, in seconds. Default 60.
    pub auth_lockout_secs: u64,
    /// PEM file holding the server certificate chain for HTTPS serving
    /// (ONVIF Profile T's transport baseline). Empty = plain HTTP.
    /// **Both-or-neither**: exactly one of `tls_cert_file` /
    /// `tls_key_file` is a configuration error. Requires the `tls` cargo
    /// feature; configuring TLS in a build without it fails at `start`.
    pub tls_cert_file: String,
    /// PEM file holding the private key matching `tls_cert_file`.
    /// Read once at `start_on`/`start` — later file changes need a
    /// restart.
    pub tls_key_file: String,
    /// Offer HTTP Digest transport auth (issue #54, RFC 7616 subset:
    /// MD5, qop="auth") alongside WS-Security — the Profile S route for
    /// non-TLS deployments. Default `false`. When enabled, token-less
    /// requests that fail the anonymous check are answered 401 with a
    /// `WWW-Authenticate: Digest` challenge; a request carrying a valid
    /// Digest header authenticates without a UsernameToken (a present
    /// UsernameToken still takes precedence, and both may coexist on
    /// one connection sequence). Pre-auth anonymous actions (e.g.
    /// GetSystemDateAndTime) stay open.
    pub http_digest: bool,
    /// Serve the ONVIF Events pull-point service (parity with onvif-go's
    /// `SupportEvents`): routes `{base}/events_service` and the
    /// per-subscription `{base}/events_service/sub/<id>` subtree on this
    /// server's listener. Default `false` — no routes, no advertisement
    /// change. Grab the host publish seam via
    /// [`OnvifServer::enable_events`] before starting.
    pub support_events: bool,
    /// Serve the ONVIF Media2 service (ver20/media, parity with
    /// onvif-go's `SupportMedia2`): routes `{base}/media2_service` with
    /// its own action dispatch — the Media2 action local names collide
    /// with Media1 in the shared action map, so the URL decides which
    /// face answers. Default `false` — no route, no advertisement
    /// change. Setting the flag alone does not mount anything (the
    /// service needs a media store): call [`OnvifServer::enable_media2`]
    /// before starting, which sets this flag.
    pub support_media2: bool,
}

impl Default for OnvifConfig {
    fn default() -> Self {
        Self {
            port: 8080,
            username: String::new(),
            password: String::new(),
            allow_no_auth: false,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            read_timeout: DEFAULT_READ_TIMEOUT,
            replay_window_secs: 300,
            auth_failure_limit: 5,
            auth_lockout_secs: 60,
            tls_cert_file: String::new(),
            tls_key_file: String::new(),
            http_digest: false,
            support_events: false,
            support_media2: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Server handle
// ---------------------------------------------------------------------------

/// Handle to a running ONVIF SOAP server.
///
/// Await it (`handle.await`) to wait for the accept loop to finish, or call
/// [`OnvifServerHandle::shutdown`] for a graceful stop.
///
/// `#[must_use]`: **dropping the handle stops the server** (Drop sends the
/// shutdown signal). Hosts that spawn `server.start()` inside a task and
/// discard the Ok value have shipped servers that die the instant they
/// start — await the handle in the task, or store it and call shutdown().
#[derive(Debug)]
#[must_use = "dropping the handle stops the server; await it or store it for shutdown()"]
pub struct OnvifServerHandle {
    task: Option<tokio::task::JoinHandle<()>>,
    shutdown: watch::Sender<bool>,
}

impl OnvifServerHandle {
    /// Request a graceful shutdown and wait for the accept loop to finish.
    /// In-flight connections finish their current request first.
    pub async fn shutdown(&mut self) -> Result<(), OnvifError> {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|e| OnvifError::Internal(format!("server task join failed: {e}")))?;
        }
        Ok(())
    }
}

impl std::future::Future for OnvifServerHandle {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.task.as_mut() {
            Some(task) => std::pin::Pin::new(task).poll(cx).map(|_| ()),
            None => std::task::Poll::Ready(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Action handler trait
// ---------------------------------------------------------------------------

/// Handler for a single ONVIF SOAP action.
///
/// `body` is the raw XML content of the SOAP Body's child (the action
/// element).  `request_info` provides caller context and the authentication
/// outcome.
#[async_trait]
pub trait OnvifActionHandler: Send + Sync {
    async fn handle(&self, body: &str, request_info: &RequestInfo) -> Result<String, OnvifError>;
}

// ---------------------------------------------------------------------------
// Parsed SOAP
// ---------------------------------------------------------------------------

/// Result of parsing an incoming SOAP 1.2 request.
pub struct ParsedSoap {
    pub action: String,
    /// The Body's first child, verbatim. Namespace declarations made on its
    /// ancestors (`Envelope`, `Body`) are copied onto it so the fragment is
    /// self-contained and can be fed to a namespace-aware parser.
    pub body_xml: String,
    pub username_token: Option<UsernameToken>,
}

// ---------------------------------------------------------------------------
// OnvifServer
// ---------------------------------------------------------------------------

/// SOAP / ONVIF server that listens on a configured port and dispatches
/// SOAP actions to registered handlers.
pub struct OnvifServer {
    config: OnvifConfig,
    handlers: HashMap<String, Box<dyn OnvifActionHandler>>,
    /// SOAP actions exempt from authentication (pre-auth per ONVIF spec).
    anonymous_actions: HashSet<String>,
    /// Library-neutral observability hooks (issue #18).
    metrics: Arc<dyn crate::metrics::MetricsHooks>,
    /// The events pull-point service, when enabled (`support_events` /
    /// [`OnvifServer::enable_events`]).
    events: Option<Arc<EventsService>>,
    /// IP address filter (issue #54): enforced per connection before
    /// any HTTP/SOAP processing. Hosts wire the SAME state installed on
    /// the device handlers via
    /// [`crate::device::DeviceServiceHandlers::with_ip_filter`].
    ip_filter: Option<crate::device::IpFilterState>,
    /// The Media2 service, when enabled ([`OnvifServer::enable_media2`]).
    media2: Option<Arc<Media2Service>>,
}

impl OnvifServer {
    /// Create a new server with the given configuration.
    pub fn new(config: &OnvifConfig) -> Self {
        Self {
            config: config.clone(),
            handlers: HashMap::new(),
            anonymous_actions: HashSet::new(),
            metrics: Arc::new(crate::metrics::NoopMetrics),
            events: None,
            ip_filter: None,
            media2: None,
        }
    }

    /// Enforce an IP address filter per connection (issue #54): peers
    /// the filter refuses get 403 (SOAP fault) before any HTTP/SOAP
    /// processing. Wire the SAME state you installed via
    /// [`crate::device::DeviceServiceHandlers::with_ip_filter`] so the
    /// Get/Set/Add/RemoveIPAddressFilter SOAP ops and this connection
    /// gate share one store.
    #[must_use]
    pub fn with_ip_filter(mut self, state: crate::device::IpFilterState) -> Self {
        self.ip_filter = Some(state);
        self
    }

    /// Install observability hooks (issue #18): request counts, fault
    /// rate, and authentication outcomes, bridged by the host to any
    /// backend. Defaults to no-ops.
    #[must_use]
    pub fn with_metrics(mut self, hooks: Arc<dyn crate::metrics::MetricsHooks>) -> Self {
        self.metrics = hooks;
        self
    }

    /// Register a handler for a SOAP action (local element name of the
    /// first child of the SOAP Body).
    pub fn register_handler(&mut self, action: &str, handler: Box<dyn OnvifActionHandler>) {
        self.handlers.insert(action.to_string(), handler);
    }

    /// Take the currently registered handler for an action out of the
    /// map (crate-internal): the imaging family uses this to take over
    /// action names it shares with PTZ (`GetStatus`, `Stop`,
    /// `GetServiceCapabilities`), routing by body shape and falling back
    /// to the previous handler (issue #52).
    pub(crate) fn take_handler(&mut self, action: &str) -> Option<Box<dyn OnvifActionHandler>> {
        self.handlers.remove(action)
    }

    /// Mark a SOAP action as accessible without authentication.
    ///
    /// Per the ONVIF spec, `GetSystemDateAndTime` must be reachable before a
    /// client can compute the WS-Security digest, so it is exempt from auth.
    pub fn register_anonymous_action(&mut self, action: &str) {
        self.anonymous_actions.insert(action.to_string());
    }

    /// Enable the events pull-point service (parity with onvif-go's
    /// `SupportEvents`): the server routes `/onvif/events_service`
    /// (GetServiceCapabilities / GetEventProperties /
    /// CreatePullPointSubscription) and `/onvif/events_service/sub/<id>`
    /// (PullMessages / Renew / Unsubscribe) on the listener it already
    /// owns. Returns the shared [`EventsService`] handle — the host-side
    /// publish seam ([`EventsService::publish_event`]); keep it to inject
    /// events. Call before `start`/`start_on`.
    pub fn enable_events(&mut self) -> Arc<EventsService> {
        self.config.support_events = true;
        self.events
            .get_or_insert_with(|| Arc::new(EventsService::new()))
            .clone()
    }

    /// The events service handle, when enabled (via `enable_events` or
    /// `OnvifConfig::support_events`); `None` while disabled. The same
    /// handle the server routes to — publishing through it reaches every
    /// live pull-point subscription.
    #[must_use]
    pub fn events_service(&self) -> Option<Arc<EventsService>> {
        self.events.clone()
    }

    /// Enable the Media2 service (ver20/media, parity with onvif-go's
    /// `SupportMedia2`, issue #53): the server routes
    /// `/onvif/media2_service` (GetProfiles / GetStreamUri /
    /// GetSnapshotUri / the video-encoder configuration family /
    /// GetVideoEncoderInstances / SetSynchronizationPoint /
    /// GetServiceCapabilities) on the listener it already owns, with its
    /// own dispatch — the action local names collide with Media1 in the
    /// shared action map, so the URL decides which face answers.
    ///
    /// The service reads (and SetVideoEncoderConfiguration writes) the
    /// SAME [`SharedMediaConfig`](crate::media::SharedMediaConfig) store
    /// as the Media1 face — pass the store `register_media_actions`
    /// uses so both faces observe one configuration. `keyframe_hook`
    /// fires on every SetSynchronizationPoint request (the host's
    /// "force an IDR frame now" seam); `None` acknowledges only. Call
    /// before `start`/`start_on`, and pair with
    /// [`crate::device::DeviceServiceHandlers::with_media2_support`] so
    /// the GetServices advertisement matches the served route.
    pub fn enable_media2(
        &mut self,
        config: crate::media::SharedMediaConfig,
        keyframe_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Arc<Media2Service> {
        self.config.support_media2 = true;
        self.media2
            .get_or_insert_with(|| Arc::new(Media2Service::new(config, keyframe_hook)))
            .clone()
    }

    /// Validate the configuration. Fail-closed: an empty password without an
    /// explicit `allow_no_auth` is a configuration error, not an open server.
    /// TLS paths are both-or-neither (parity with onvif-go: "TLSCertFile
    /// and TLSKeyFile must be set together").
    fn validate_config(config: &OnvifConfig) -> Result<(), OnvifError> {
        if config.password.is_empty() && !config.allow_no_auth {
            return Err(OnvifError::NotAuthorized(
                "OnvifConfig.password is empty — set credentials, or set \
                 allow_no_auth = true to deliberately run an open server"
                    .to_string(),
            ));
        }
        if config.tls_cert_file.is_empty() != config.tls_key_file.is_empty() {
            return Err(OnvifError::InvalidConfig(
                "tls_cert_file and tls_key_file must be set together \
                 (both empty = plain HTTP)"
                    .to_string(),
            ));
        }
        #[cfg(not(feature = "tls"))]
        if !config.tls_cert_file.is_empty() {
            return Err(OnvifError::InvalidConfig(
                "TLS configured (tls_cert_file/tls_key_file) but this build \
                 does not enable the `tls` cargo feature"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Start listening on `0.0.0.0:{config.port}` and serve.
    ///
    /// Consumes `self` — all handlers must be registered before calling
    /// this. Returns an [`OnvifServerHandle`] for graceful shutdown.
    pub async fn start(self) -> Result<OnvifServerHandle, OnvifError> {
        Self::validate_config(&self.config)?;
        let addr = format!("0.0.0.0:{}", self.config.port);
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| OnvifError::Internal(format!("bind {addr}: {e}")))?;
        self.start_on(listener).await
    }

    /// Serve on an already-bound listener (injection point for tests, custom
    /// bind addresses, or sharing a port with host infrastructure).
    ///
    /// `OnvifConfig::port` is ignored; the listener's own address is used.
    /// With `tls_cert_file`/`tls_key_file` configured (and the `tls`
    /// feature enabled) every accepted connection is served over TLS.
    pub async fn start_on(
        mut self,
        listener: TcpListener,
    ) -> Result<OnvifServerHandle, OnvifError> {
        Self::validate_config(&self.config)?;
        #[cfg(feature = "tls")]
        let tls_acceptor = build_tls_acceptor(&self.config)?;
        let local_addr = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        log::info!("onvif: serving SOAP on {local_addr}");
        #[cfg(feature = "tls")]
        if tls_acceptor.is_some() {
            log::info!("onvif: TLS listener active (https)");
        }

        let handlers = Arc::new(self.handlers);
        let auth_state = Arc::new(AuthState::new(&self.config));
        let cfg = Arc::new(self.config);
        let anonymous = Arc::new(self.anonymous_actions);
        let metrics = self.metrics;
        let ip_filter = self.ip_filter;
        // Events pull-point service (created here when enabled purely by
        // config — hosts that want the publish seam call enable_events
        // before starting and the same instance is reused).
        if cfg.support_events && self.events.is_none() {
            self.events = Some(Arc::new(EventsService::new()));
        }
        let events = self.events;
        // Media2 cannot be created from the flag alone (it needs a media
        // store); a flag without enable_media2 gets a visible warning
        // instead of a silently-absent route.
        if cfg.support_media2 && self.media2.is_none() {
            log::warn!(
                "onvif: support_media2 is set but enable_media2 was never called — \
                 no media2 store, the /onvif/media2_service route stays unmounted"
            );
        }
        let media2 = self.media2;
        // The listener's actual port (an ephemeral one with `start_on`) —
        // the events service builds SubscriptionReference addresses on it.
        let server_port = listener.local_addr().map(|a| a.port()).unwrap_or(cfg.port);
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        let task = tokio::spawn(async move {
            loop {
                let (mut stream, peer_addr) = tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok(conn) => conn,
                        Err(e) => {
                            // Accept errors are usually transient
                            log::warn!("onvif: accept error: {e}");
                            continue;
                        }
                    },
                    _ = shutdown_rx.changed() => {
                        log::info!("onvif: shutdown requested — accept loop stopping");
                        break;
                    }
                };

                let handlers = Arc::clone(&handlers);
                let cfg = Arc::clone(&cfg);
                let anonymous = Arc::clone(&anonymous);
                let auth_state = Arc::clone(&auth_state);
                let metrics = Arc::clone(&metrics);
                let events = events.clone();
                let ip_filter = ip_filter.clone();
                let media2 = media2.clone();
                #[cfg(feature = "tls")]
                let tls_acceptor = tls_acceptor.clone();

                tokio::spawn(async move {
                    let client_ip = peer_addr.ip().to_string();
                    let server_ip = stream
                        .local_addr()
                        .map(|a| a.ip().to_string())
                        .unwrap_or_default();

                    // Feature-gated TLS wrap: with the `tls` feature and a
                    // certificate configured, the plain stream becomes a TLS
                    // session before the HTTP/SOAP stack sees it. Failed
                    // handshakes drop the connection (never served).
                    #[cfg(feature = "tls")]
                    let result = match tls_acceptor {
                        Some(acceptor) => match acceptor.accept(stream).await {
                            Ok(mut tls_stream) => {
                                let result = handle_connection(
                                    &mut tls_stream,
                                    &client_ip,
                                    &server_ip,
                                    server_port,
                                    &handlers,
                                    &cfg,
                                    &anonymous,
                                    &auth_state,
                                    &metrics,
                                    &events,
                                    &ip_filter,
                                    &media2,
                                )
                                .await;
                                // Orderly TLS close (close_notify) — dropping
                                // a rustls stream without it surfaces as
                                // "unexpected EOF" on strict clients.
                                let _ = tls_stream.shutdown().await;
                                result
                            }
                            Err(e) => {
                                log::warn!("onvif: TLS handshake failed from {client_ip}: {e}");
                                Ok(())
                            }
                        },
                        None => {
                            handle_connection(
                                &mut stream,
                                &client_ip,
                                &server_ip,
                                server_port,
                                &handlers,
                                &cfg,
                                &anonymous,
                                &auth_state,
                                &metrics,
                                &events,
                                &ip_filter,
                                &media2,
                            )
                            .await
                        }
                    };
                    #[cfg(not(feature = "tls"))]
                    let result = handle_connection(
                        &mut stream,
                        &client_ip,
                        &server_ip,
                        server_port,
                        &handlers,
                        &cfg,
                        &anonymous,
                        &auth_state,
                        &metrics,
                        &events,
                        &ip_filter,
                        &media2,
                    )
                    .await;

                    if let Err(e) = result {
                        log::error!("onvif: connection error from {client_ip}: {e}");
                    }
                });
            }
        });

        Ok(OnvifServerHandle {
            task: Some(task),
            shutdown: shutdown_tx,
        })
    }
}

// ---------------------------------------------------------------------------
// TLS listener (feature `tls`)
// ---------------------------------------------------------------------------

/// Load the configured certificate/key PEMs into a TLS acceptor.
///
/// Returns `Ok(None)` when no TLS paths are configured (plain HTTP).
/// Failures (missing file, unparseable PEM, unusable key) are
/// `OnvifError::InvalidConfig` — served at startup, never mid-flight.
#[cfg(feature = "tls")]
fn build_tls_acceptor(cfg: &OnvifConfig) -> Result<Option<tokio_rustls::TlsAcceptor>, OnvifError> {
    use std::io::BufReader;

    if cfg.tls_cert_file.is_empty() && cfg.tls_key_file.is_empty() {
        return Ok(None);
    }

    let cert_file = std::fs::File::open(&cfg.tls_cert_file).map_err(|e| {
        OnvifError::InvalidConfig(format!("open tls_cert_file {}: {e}", cfg.tls_cert_file))
    })?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut BufReader::new(cert_file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            OnvifError::InvalidConfig(format!(
                "parse certificates from {}: {e}",
                cfg.tls_cert_file
            ))
        })?;
    if certs.is_empty() {
        return Err(OnvifError::InvalidConfig(format!(
            "no certificates found in {}",
            cfg.tls_cert_file
        )));
    }

    let key_file = std::fs::File::open(&cfg.tls_key_file).map_err(|e| {
        OnvifError::InvalidConfig(format!("open tls_key_file {}: {e}", cfg.tls_key_file))
    })?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
        .map_err(|e| {
            OnvifError::InvalidConfig(format!("parse key from {}: {e}", cfg.tls_key_file))
        })?
        .ok_or_else(|| {
            OnvifError::InvalidConfig(format!("no private key found in {}", cfg.tls_key_file))
        })?;

    let tls = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| OnvifError::InvalidConfig(format!("TLS config: {e}")))?;
    Ok(Some(tokio_rustls::TlsAcceptor::from(Arc::new(tls))))
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

/// Internal server state shared across connections.
type HandlerMap = Arc<HashMap<String, Box<dyn OnvifActionHandler>>>;
type SharedConfig = Arc<OnvifConfig>;
type SharedAnonymous = Arc<HashSet<String>>;

/// Cross-connection auth state: UsernameToken replay guard, the HTTP
/// Digest nonce store (issue #54), plus the per-source failure lockout
/// (issue #16).
#[derive(Default)]
struct AuthState {
    replay: Option<crate::auth::ReplayGuard>,
    digest_nonces: crate::auth::NonceGuard,
    failures: Mutex<HashMap<String, AuthFailEntry>>,
}

#[derive(Default)]
struct AuthFailEntry {
    fails: i32,
    locked_until: Option<Instant>,
}

impl AuthState {
    fn new(cfg: &OnvifConfig) -> Self {
        Self {
            replay: Some(crate::auth::ReplayGuard::new(cfg.replay_window_secs)),
            digest_nonces: crate::auth::NonceGuard::new(DIGEST_NONCE_TTL_SECS),
            failures: Mutex::new(HashMap::new()),
        }
    }

    fn locked(&self, key: &str) -> bool {
        let mut failures = match self.failures.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match failures.get_mut(key) {
            Some(e) => match e.locked_until {
                Some(until) if Instant::now() < until => true,
                Some(_) => {
                    failures.remove(key); // lockout served
                    false
                }
                None => false,
            },
            None => false,
        }
    }

    fn record_failure(&self, key: &str, limit: i32, lockout: Duration) {
        if limit <= 0 {
            return;
        }
        let mut failures = match self.failures.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let e = failures.entry(key.to_string()).or_default();
        e.fails += 1;
        if e.fails >= limit {
            e.locked_until = Some(Instant::now() + lockout);
        }
    }

    fn record_success(&self, key: &str) {
        let mut failures = match self.failures.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        failures.remove(key);
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)] // connection-scoped shared state passed as-is
async fn handle_connection<S>(
    stream: &mut S,
    client_ip: &str,
    server_ip: &str,
    server_port: u16,
    handlers: &HandlerMap,
    cfg: &SharedConfig,
    anonymous: &SharedAnonymous,
    auth_state: &Arc<AuthState>,
    metrics: &Arc<dyn crate::metrics::MetricsHooks>,
    events: &Option<Arc<EventsService>>,
    ip_filter: &Option<crate::device::IpFilterState>,
    media2: &Option<Arc<Media2Service>>,
) -> Result<(), OnvifError>
where
    // Plain TcpStream without the `tls` feature, a TLS session with it.
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // --- Read HTTP request (bounded header, bounded body, read timeout) ---
    let read = read_http_request(stream, cfg);
    let (method, path, body, authorization) =
        match tokio::time::timeout(cfg.read_timeout, read).await {
            Ok(Ok(triple)) => triple,
            Ok(Err(OnvifError::InvalidXml(m))) if m.starts_with("body too large") => {
                let fault = serialize_soap_fault("soap:Sender", &m);
                write_http_response(stream, 413, &fault).await?;
                // Drain the in-flight body before closing so the client sees the
                // 413 instead of a connection reset (RFC 7230 §6.6 politeness).
                drain_before_close(stream).await;
                return Ok(());
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                log::warn!("onvif: read timeout from {client_ip}");
                return Ok(());
            }
        };

    // --- IP address filter (issue #54): refuse blocked peers before
    // any HTTP/SOAP processing. Unparseable/IPv6 peers fail open (see
    // IpFilter::allows_client_ip). ---
    if let Some(state) = ip_filter {
        let allowed = {
            let filter = match state.read() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            filter.allows_client_ip(client_ip)
        };
        if !allowed {
            log::warn!("onvif: refusing connection from {client_ip} (IP filter)");
            let fault = serialize_soap_fault("soap:Sender", "client address refused by IP filter");
            write_http_response(stream, 403, &fault).await?;
            return Ok(());
        }
    }

    if method != "POST" {
        let fault = serialize_soap_fault("soap:Sender", "only POST method is supported");
        write_http_response(stream, 405, &fault).await?;
        return Ok(());
    }

    // --- Parse SOAP ---
    let parsed = match parse_soap_request(&body) {
        Ok(p) => p,
        Err(e) => {
            let fault = serialize_soap_fault("soap:Client", &e.to_string());
            write_http_response(stream, 400, &fault).await?;
            return Ok(());
        }
    };

    // --- Auth ---
    // `auth_disabled` requires BOTH an explicit opt-in flag and an empty
    // password — an empty password alone never silently disables auth.
    let auth_disabled = cfg.allow_no_auth && cfg.password.is_empty();
    if !auth_disabled && auth_state.locked(client_ip) {
        metrics.auth_lockout();
        log::warn!("onvif: auth lockout active for {client_ip}");
        let fault = serialize_soap_fault("soap:Sender", "too many authentication failures");
        if cfg.http_digest {
            let challenge = digest_challenge_value(auth_state, false);
            write_http_response_with_headers(
                stream,
                401,
                &fault,
                &[("WWW-Authenticate", challenge)],
            )
            .await?;
        } else {
            write_http_response(stream, 401, &fault).await?;
        }
        return Ok(());
    }
    let mut token_ok = true;
    // Set when the Digest verdict was Stale (unknown/expired nonce) so
    // the re-challenge carries stale=TRUE (issue #54).
    let mut digest_stale = false;
    let auth_result = if let Some(ref token) = parsed.username_token {
        let mut ok = verify_username_token(token, &cfg.username, &cfg.password);
        if ok {
            // Digest tokens additionally pass the replay guard: fresh
            // Created, nonce never seen before (issue #16).
            if !token.nonce.is_empty() {
                if let Some(guard) = &auth_state.replay {
                    if !guard.check_and_remember(&token.nonce, &token.created) {
                        log::warn!("onvif: UsernameToken replay/stale rejected from {client_ip}");
                        ok = false;
                    }
                }
            }
        }
        token_ok = ok;
        if !ok {
            metrics.auth_fail();
            auth_state.record_failure(
                client_ip,
                cfg.auth_failure_limit,
                Duration::from_secs(cfg.auth_lockout_secs),
            );
        } else {
            auth_state.record_success(client_ip);
        }
        AuthResult {
            username: token.username.clone(),
            authenticated: ok,
        }
    } else if cfg.http_digest && authorization.is_some() {
        // HTTP Digest transport auth (issue #54): token-less requests
        // may authenticate via RFC 7616 instead. A present
        // UsernameToken takes precedence (handled above); the two
        // mechanisms coexist across a connection sequence. Any
        // Authorization header counts as an attempt — a non-Digest
        // scheme (e.g. Basic) fails verification honestly.
        let header = authorization.as_deref().unwrap_or_default();
        let verdict = crate::auth::verify_http_digest(
            header,
            &method,
            &path,
            HTTP_DIGEST_REALM,
            &cfg.username,
            &cfg.password,
            &auth_state.digest_nonces,
        );
        match verdict {
            crate::auth::DigestResult::Ok => {
                auth_state.record_success(client_ip);
                AuthResult {
                    username: cfg.username.clone(),
                    authenticated: true,
                }
            }
            crate::auth::DigestResult::Stale => {
                digest_stale = true;
                AuthResult::default()
            }
            crate::auth::DigestResult::Invalid => {
                log::warn!("onvif: HTTP Digest rejected from {client_ip}");
                metrics.auth_fail();
                auth_state.record_failure(
                    client_ip,
                    cfg.auth_failure_limit,
                    Duration::from_secs(cfg.auth_lockout_secs),
                );
                AuthResult::default()
            }
        }
    } else {
        AuthResult::default()
    };
    let _ = token_ok;

    // --- Action check ---
    if parsed.action.is_empty() {
        let fault = serialize_soap_fault("soap:Client", "no action found in SOAP body");
        write_http_response(stream, 400, &fault).await?;
        return Ok(());
    }

    // --- Dispatch ---
    // Events pull-point paths and the Media2 endpoint are routed by URL
    // (parity with onvif-go's mux: the service endpoints + the
    // per-subscription subtree); every other path keeps the historical
    // action-map dispatch unchanged.
    let route = classify_request_route(&path, events.as_ref(), media2.as_ref());
    if route == RequestRoute::NotFound {
        // No route serves this path — parity with onvif-go's ServeMux 404
        // for the events subtree when the service is disabled.
        write_http_response(stream, 404, "404 page not found\n").await?;
        return Ok(());
    }

    // The auth gate each route applies before invoking its handler.
    let route_requires_auth = match route {
        RequestRoute::Default => !anonymous.contains(&parsed.action),
        // onvif-go's default policy: write-style actions (Set/Remove/
        // Create/Go) protected, reads open — for the events actions that
        // means CreatePullPointSubscription needs credentials, the rest
        // stay open.
        RequestRoute::EventsService | RequestRoute::EventsSubscription => {
            EventsService::action_requires_auth(&parsed.action)
        }
        // The same write-prefix policy on the Media2 endpoint
        // (SetSynchronizationPoint / SetVideoEncoderConfiguration).
        RequestRoute::Media2Service => Media2Service::action_requires_auth(&parsed.action),
        RequestRoute::NotFound => false, // unreachable: handled above
    };

    match route {
        RequestRoute::Default => {
            let handler = match handlers.get(&parsed.action) {
                Some(h) => h,
                None => {
                    let fault = serialize_soap_fault(
                        "soap:Sender",
                        &format!("unsupported action: {}", parsed.action),
                    );
                    write_http_response(stream, 400, &fault).await?;
                    return Ok(());
                }
            };
            if route_requires_auth && !auth_disabled && !auth_result.authenticated {
                write_401_auth_required(stream, cfg, auth_state, digest_stale, &parsed.action)
                    .await?;
                return Ok(());
            }
            let request_info = RequestInfo {
                client_ip: client_ip.to_string(),
                server_ip: server_ip.to_string(),
                auth_result,
            };
            dispatch_handler(
                stream,
                metrics,
                &parsed.action,
                handler.handle(&parsed.body_xml, &request_info),
            )
            .await?;
        }
        RequestRoute::EventsService | RequestRoute::EventsSubscription => {
            let events = match events {
                Some(ev) => Arc::clone(ev),
                // classify_request_route only returns events routes with
                // the service enabled; defensive otherwise.
                None => return Err(OnvifError::Internal("events service unavailable".into())),
            };
            let known_action = match route {
                RequestRoute::EventsService => EventsService::is_service_action(&parsed.action),
                _ => EventsService::is_subscription_action(&parsed.action),
            };
            if !known_action {
                let fault = serialize_soap_fault(
                    "soap:Sender",
                    &format!("unsupported action: {}", parsed.action),
                );
                write_http_response(stream, 400, &fault).await?;
                return Ok(());
            }
            if route_requires_auth && !auth_disabled && !auth_result.authenticated {
                write_401_auth_required(stream, cfg, auth_state, digest_stale, &parsed.action)
                    .await?;
                return Ok(());
            }
            let endpoint = ServiceEndpoint {
                host: if server_ip.is_empty() {
                    "127.0.0.1".to_string()
                } else {
                    server_ip.to_string()
                },
                port: server_port,
            };
            // Handler panics are contained (issue #15), same as the action
            // map: the request gets a 500 and the server keeps serving.
            let future: std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<String, OnvifError>> + Send + '_>,
            > = match route {
                RequestRoute::EventsService => Box::pin(events.handle_service_action(
                    &parsed.action,
                    &parsed.body_xml,
                    &endpoint,
                )),
                _ => Box::pin(events.handle_subscription_action(
                    &path,
                    &parsed.action,
                    &parsed.body_xml,
                    &endpoint,
                )),
            };
            dispatch_handler(stream, metrics, &parsed.action, future).await?;
        }
        RequestRoute::Media2Service => {
            let media2 = match media2 {
                Some(m) => Arc::clone(m),
                // classify_request_route only returns the media2 route
                // with the service enabled; defensive otherwise.
                None => return Err(OnvifError::Internal("media2 service unavailable".into())),
            };
            if !Media2Service::is_service_action(&parsed.action) {
                let fault = serialize_soap_fault(
                    "soap:Sender",
                    &format!("unsupported action: {}", parsed.action),
                );
                write_http_response(stream, 400, &fault).await?;
                return Ok(());
            }
            if route_requires_auth && !auth_disabled && !auth_result.authenticated {
                let fault = serialize_soap_fault(
                    "soap:Sender",
                    &format!("authentication required for action: {}", parsed.action),
                );
                write_http_response(stream, 401, &fault).await?;
                return Ok(());
            }
            // Same panic containment as every other route (issue #15).
            dispatch_handler(
                stream,
                metrics,
                &parsed.action,
                media2.handle_action(&parsed.action, &parsed.body_xml, server_ip),
            )
            .await?;
        }
        RequestRoute::NotFound => unreachable!("handled above"),
    }

    Ok(())
}

/// Invoke an already-authenticated handler future with panic containment
/// (issue #15) and write its outcome: 200 on success, a Sender fault with
/// HTTP 400 for client mistakes ([`OnvifError::SenderFault`], parity with
/// onvif-go's SenderFaultError), a Receiver fault with 500 otherwise.
async fn dispatch_handler<F>(
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    metrics: &Arc<dyn crate::metrics::MetricsHooks>,
    action: &str,
    future: F,
) -> Result<(), OnvifError>
where
    F: std::future::Future<Output = Result<String, OnvifError>>,
{
    metrics.soap_request(action);
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(future)).await;
    match outcome {
        Ok(Ok(response_xml)) => {
            write_http_response(stream, 200, &response_xml).await?;
        }
        Ok(Err(OnvifError::SenderFault(reason))) => {
            metrics.soap_fault(action);
            let fault = serialize_soap_fault("soap:Sender", &reason);
            write_http_response(stream, 400, &fault).await?;
        }
        Ok(Err(e)) => {
            metrics.soap_fault(action);
            let fault = serialize_soap_fault("soap:Receiver", &e.to_string());
            write_http_response(stream, 500, &fault).await?;
        }
        Err(panic_payload) => {
            metrics.soap_fault(action);
            log::error!("onvif: handler panicked for action {action}: {panic_payload:?}");
            let fault = serialize_soap_fault("soap:Receiver", "internal handler error");
            write_http_response(stream, 500, &fault).await?;
        }
    }
    Ok(())
}

/// How an incoming request path is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestRoute {
    /// Historical action-map dispatch — every path outside the events
    /// subtree and the Media2 endpoint, byte-identical to the
    /// pre-events router.
    Default,
    /// `/onvif/events_service` with the events service enabled.
    EventsService,
    /// Under `/onvif/events_service/sub/` with the events service enabled.
    EventsSubscription,
    /// `/onvif/media2_service` with the Media2 service enabled.
    Media2Service,
    /// A service path with no route serving it (the owning service is
    /// disabled) — onvif-go's mux would 404.
    NotFound,
}

fn classify_request_route(
    path: &str,
    events: Option<&Arc<EventsService>>,
    media2: Option<&Arc<Media2Service>>,
) -> RequestRoute {
    if path == EVENTS_SERVICE_PATH {
        match events {
            Some(_) => RequestRoute::EventsService,
            None => RequestRoute::NotFound,
        }
    } else if path.starts_with(SUBSCRIPTION_PATH_PREFIX) {
        match events {
            Some(_) => RequestRoute::EventsSubscription,
            None => RequestRoute::NotFound,
        }
    } else if path == MEDIA2_SERVICE_PATH {
        match media2 {
            Some(_) => RequestRoute::Media2Service,
            None => RequestRoute::NotFound,
        }
    } else {
        RequestRoute::Default
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP request reader
// ---------------------------------------------------------------------------

/// Read one HTTP request: header section (capped at [`MAX_HEADER_BYTES`],
/// may span multiple reads), then exactly `Content-Length` body bytes
/// (capped at `cfg.max_body_bytes`). Returns
/// `(method, path, body, authorization)` — `authorization` is the value
/// of the `Authorization` header when present (issue #54: HTTP Digest).
async fn read_http_request<S>(
    stream: &mut S,
    cfg: &OnvifConfig,
) -> Result<(String, String, String, Option<String>), OnvifError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut data: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];

    // Read until the end of the header section (or the cap).
    let header_end = loop {
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if data.len() > MAX_HEADER_BYTES {
            return Err(OnvifError::InvalidXml("malformed HTTP headers".into()));
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| OnvifError::Internal(format!("read request: {e}")))?;
        if n == 0 {
            return Err(OnvifError::InvalidXml("empty request".into()));
        }
        data.extend_from_slice(&chunk[..n]);
    };

    let header_str = str::from_utf8(&data[..header_end])
        .map_err(|_| OnvifError::InvalidXml("invalid HTTP header encoding".into()))?;

    // Extract method and path (first two tokens of the first line).
    let first_line = header_str.lines().next().unwrap_or("");
    let mut tokens = first_line.split_whitespace();
    let method = tokens.next().unwrap_or("").to_string();
    let path = tokens.next().unwrap_or("/").to_string();

    // Extract Content-Length
    let content_length = header_str
        .lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("content-length:") {
                lower
                    .trim_start_matches("content-length:")
                    .trim()
                    .parse::<usize>()
                    .ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    // Extract Authorization (case-insensitive; first occurrence wins).
    let authorization = header_str.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("authorization:") {
            Some(
                line.split_once(':')
                    .map(|(_, v)| v.trim().to_string())
                    .unwrap_or_default(),
            )
        } else {
            None
        }
    });

    if content_length > cfg.max_body_bytes {
        return Err(OnvifError::InvalidXml(format!(
            "body too large: {content_length} bytes exceeds limit {}",
            cfg.max_body_bytes
        )));
    }

    // Body starts after headers; keep whatever arrived with them (bounded
    // by the declared Content-Length — the connection closes after each
    // response, so pipelined bytes beyond it are ignored).
    let body_start = header_end + 4;
    let mut body: Vec<u8> = data[body_start..].to_vec();
    body.truncate(content_length);

    // Read the remainder of the declared body.
    while body.len() < content_length {
        let m = stream
            .read(&mut chunk)
            .await
            .map_err(|e| OnvifError::Internal(format!("read body: {e}")))?;
        if m == 0 {
            break; // EOF before Content-Length — truncated
        }
        body.extend_from_slice(&chunk[..m]);
    }

    let body_str = String::from_utf8(body)
        .map_err(|_| OnvifError::InvalidXml("request body is not valid UTF-8".into()))?;

    Ok((method, path, body_str, authorization))
}

/// Read and discard pending inbound bytes (bounded) so closing the socket
/// sends FIN instead of RST while the peer is still writing.
async fn drain_before_close<S>(stream: &mut S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut scratch = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    let mut drained: usize = 0;
    loop {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() || drained > 4 * 1024 * 1024 {
            break;
        }
        match tokio::time::timeout(remain, stream.read(&mut scratch)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => drained += n,
        }
    }
}

/// Write an HTTP 1.1 response with `Content-Type: application/soap+xml`.
async fn write_http_response<S>(stream: &mut S, status: u16, body: &str) -> Result<(), OnvifError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    write_http_response_with_headers(stream, status, body, &[]).await
}

/// [`write_http_response`] with optional extra headers (issue #54: the
/// `WWW-Authenticate` challenge on 401s when HTTP Digest is enabled).
/// Zero extra headers is byte-identical to the historical writer.
async fn write_http_response_with_headers<S>(
    stream: &mut S,
    status: u16,
    body: &str,
    extra_headers: &[(&str, String)],
) -> Result<(), OnvifError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let status_line = match status {
        200 => "200 OK",
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        403 => "403 Forbidden",
        404 => "404 Not Found",
        405 => "405 Method Not Allowed",
        413 => "413 Content Too Large",
        500 => "500 Internal Server Error",
        _ => "500 Internal Server Error",
    };

    let mut header = format!(
        "HTTP/1.1 {status_line}\r\n\
         Content-Type: application/soap+xml; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        header.push_str(&format!("{name}: {value}\r\n"));
    }
    header.push_str("\r\n");

    stream
        .write_all(header.as_bytes())
        .await
        .map_err(|e| OnvifError::Internal(format!("write response header: {e}")))?;
    stream
        .write_all(body.as_bytes())
        .await
        .map_err(|e| OnvifError::Internal(format!("write response body: {e}")))?;

    Ok(())
}

/// A fresh `WWW-Authenticate: Digest` challenge value for a 401 (issue
/// #54): server-generated nonce + opaque; `stale=TRUE` re-challenges a
/// client whose nonce is unknown/expired.
fn digest_challenge_value(auth_state: &AuthState, stale: bool) -> String {
    use rand::RngCore;

    let nonce = auth_state.digest_nonces.generate();
    let mut opaque_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut opaque_bytes);
    let opaque = hex::encode(opaque_bytes);
    crate::auth::HttpDigestChallenge::new(HTTP_DIGEST_REALM, &nonce, &opaque, stale)
        .to_header_value()
}

/// 401 for an unauthenticated request: SOAP Sender fault, plus a Digest
/// challenge when HTTP Digest is enabled (issue #54). Byte-identical to
/// the historical 401 when Digest is off.
async fn write_401_auth_required<S>(
    stream: &mut S,
    cfg: &OnvifConfig,
    auth_state: &AuthState,
    digest_stale: bool,
    action: &str,
) -> Result<(), OnvifError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let fault = serialize_soap_fault(
        "soap:Sender",
        &format!("authentication required for action: {action}"),
    );
    if cfg.http_digest {
        let challenge = digest_challenge_value(auth_state, digest_stale);
        write_http_response_with_headers(stream, 401, &fault, &[("WWW-Authenticate", challenge)])
            .await
    } else {
        write_http_response(stream, 401, &fault).await
    }
}

// ---------------------------------------------------------------------------
// SOAP XML parsing
// ---------------------------------------------------------------------------

/// Parse a SOAP 1.2 envelope and extract the action name, body XML, and
/// optional WS-UsernameToken.
///
/// Uses `quick_xml::Reader` as a streaming XML state machine.
/// Parses a raw SOAP 1.2 request envelope into action, body XML, and the
/// WS-Security UsernameToken when present. Public so hosts can pre-inspect
/// requests (and so the parse hot path is benchable).
pub fn parse_soap_request(xml: &str) -> Result<ParsedSoap, OnvifError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();

    // State machine
    #[derive(Default)]
    struct ParseState {
        in_header: bool,
        in_security: bool,
        in_ut: bool,
        in_body: bool,
        // UsernameToken fields being accumulated
        ut_username: String,
        ut_password: String,
        ut_nonce: String,
        ut_created: String,
        current_field: String,
        // Action and body
        action: String,
        body_xml: String,
        body_writer: Option<quick_xml::Writer<Vec<u8>>>,
        // Namespace bindings of the open elements, so the action element can
        // carry the declarations it inherits from Envelope / Body.
        ns: NamespaceResolver,
    }

    let mut st = ParseState::default();
    let mut text_acc = crate::types::TextAccumulator::new();

    loop {
        let event = reader.read_event_into(&mut buf);
        match event {
            Ok(Event::Text(e)) => {
                if st.in_body {
                    if let Some(ref mut w) = st.body_writer {
                        let _ = w.write_event(Event::Text(e.clone()));
                    }
                }
                let _ = text_acc.push_text(&e);
            }
            Ok(Event::GeneralRef(e)) => {
                if st.in_body {
                    if let Some(ref mut w) = st.body_writer {
                        let _ = w.write_event(Event::GeneralRef(e.clone()));
                    }
                }
                let _ = text_acc.push_ref(&e);
            }
            other => {
                if let Some(text) = text_acc.flush() {
                    if st.in_ut && !text.is_empty() {
                        match st.current_field.as_str() {
                            "Username" => st.ut_username = text,
                            "Password" => st.ut_password = text,
                            "Nonce" => st.ut_nonce = text,
                            "Created" => st.ut_created = text,
                            _ => {}
                        }
                    }
                }
                match other {
                    Ok(Event::Start(e)) => {
                        let name_bytes = e.name().as_ref().to_owned();
                        let qname = str::from_utf8(&name_bytes).unwrap_or("");
                        let local = qname.rsplit(':').next().unwrap_or(qname);

                        match local {
                            "Header" => st.in_header = true,
                            "Security" if st.in_header => st.in_security = true,
                            "UsernameToken" if st.in_security => st.in_ut = true,
                            "Body" => {
                                st.in_body = true;
                                st.body_writer = Some(quick_xml::Writer::new(Vec::new()));
                            }
                            _ => {
                                if st.in_ut && st.current_field.is_empty() {
                                    st.current_field = local.to_string();
                                }
                                if st.in_body && st.action.is_empty() {
                                    st.action = local.to_string();
                                    // Write the action element to the body writer
                                    if let Some(ref mut w) = st.body_writer {
                                        let start = with_inherited_namespaces(&e, &st.ns);
                                        let _ = w.write_event(Event::Start(start));
                                    }
                                } else if st.in_body {
                                    if let Some(ref mut w) = st.body_writer {
                                        let _ = w.write_event(Event::Start(e.clone()));
                                    }
                                }
                            }
                        }
                        // After the action check above, which needs the bindings
                        // of `e`'s ancestors only. A malformed declaration just
                        // leaves that binding out.
                        let _ = st.ns.push(&e);
                    }

                    Ok(Event::Empty(e)) => {
                        let name_bytes = e.name().as_ref().to_owned();
                        let qname = str::from_utf8(&name_bytes).unwrap_or("");
                        let local = qname.rsplit(':').next().unwrap_or(qname);

                        // Self-closing structural elements do not change parse state.
                        // They are handled in the _ branch (writing to body_writer if inside Body).
                        match local {
                            "Header" | "Security" | "UsernameToken" | "Body" => {}
                            _ => {
                                if st.in_ut && st.current_field.is_empty() {
                                    st.current_field = local.to_string();
                                }
                                if st.in_body && st.action.is_empty() {
                                    st.action = local.to_string();
                                    if let Some(ref mut w) = st.body_writer {
                                        let start = with_inherited_namespaces(&e, &st.ns);
                                        let _ = w.write_event(Event::Empty(start));
                                    }
                                } else if st.in_body {
                                    if let Some(ref mut w) = st.body_writer {
                                        let _ = w.write_event(Event::Empty(e.clone()));
                                    }
                                }
                            }
                        }
                    }

                    Ok(Event::End(e)) => {
                        let name_bytes = e.name().as_ref().to_owned();
                        let qname = str::from_utf8(&name_bytes).unwrap_or("");
                        let local = qname.rsplit(':').next().unwrap_or(qname);
                        st.ns.pop();

                        // The Body's own close tag terminates body_xml — do not copy
                        // it in (its opening twin was never written, so appending it
                        // would leave a dangling close tag for strict downstream
                        // parsers like imaging's SetImagingSettings).
                        if local == "Body" && st.in_body {
                            st.in_body = false;
                            st.body_xml = st
                                .body_writer
                                .take()
                                .map(|w| String::from_utf8(w.into_inner()).unwrap_or_default())
                                .unwrap_or_default();
                        } else if st.in_body {
                            if let Some(ref mut w) = st.body_writer {
                                let _ = w.write_event(Event::End(e.clone()));
                            }
                        }

                        match local {
                            "Header" => st.in_header = false,
                            "Security" => st.in_security = false,
                            "UsernameToken" => {
                                st.in_ut = false;
                                st.current_field.clear();
                            }
                            _ => {
                                if st.in_ut {
                                    st.current_field.clear();
                                }
                            }
                        }
                    }

                    Ok(Event::CData(e)) => {
                        if st.in_body {
                            if let Some(ref mut w) = st.body_writer {
                                let _ = w.write_event(Event::CData(e.clone()));
                            }
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

    // quick-xml does not validate balanced tags; check our state machine
    // to reject incomplete XML where an element was opened but never closed.
    if st.in_body || st.in_security || st.in_ut {
        return Err(OnvifError::InvalidXml(
            "XML parse error: unexpected end of input, unclosed element".to_string(),
        ));
    }

    let username_token = if !st.ut_username.is_empty() {
        Some(UsernameToken {
            username: st.ut_username,
            password: st.ut_password,
            nonce: st.ut_nonce,
            created: st.ut_created,
        })
    } else {
        None
    };

    Ok(ParsedSoap {
        action: st.action,
        body_xml: st.body_xml,
        username_token,
    })
}

/// Returns a copy of `start` that also declares every namespace binding in
/// scope in `ns` (its ancestors), except prefixes `start` redeclares itself.
///
/// The Body's child is handed to handlers on its own, so declarations made on
/// `Envelope` / `Body` (as gSOAP-based clients do for `tt`, `tptz`, ...) would
/// otherwise be lost and its prefixes left unbound.
fn with_inherited_namespaces<'a>(start: &BytesStart<'a>, ns: &NamespaceResolver) -> BytesStart<'a> {
    let own: Vec<Vec<u8>> = start
        .attributes()
        .with_checks(false)
        .flatten()
        .filter(|a| a.key.as_namespace_binding().is_some())
        .map(|a| a.key.as_ref().to_vec())
        .collect();
    let mut out = start.clone();
    for (prefix, namespace) in ns.bindings() {
        let key = match prefix {
            PrefixDeclaration::Default => b"xmlns".to_vec(),
            PrefixDeclaration::Named(name) => [b"xmlns:".as_slice(), name].concat(),
        };
        if !own.contains(&key) {
            out.push_attribute((key.as_slice(), namespace.0));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod proptests {
    //! Property tests (#17): the quick-xml SOAP request parser faces an
    //! untrusted HTTP body — arbitrary input must surface as an error,
    //! never a panic. Valid requests must round-trip the action and the
    //! UsernameToken fields.
    use super::*;
    use proptest::prelude::*;

    proptest::proptest! {
        #[test]
        fn parse_soap_request_never_panics_on_arbitrary_chars(input in proptest::collection::vec(any::<char>(), 0..1024)) {
            let s: String = input.into_iter().collect();
            let _ = parse_soap_request(&s);
        }

        #[test]
        fn parse_soap_request_never_panics_on_xml_shaped_garbage(
            name in "[A-Za-z][A-Za-z0-9]{0,24}",
            depth in 1usize..8,
        ) {
            let mut s = String::from("<?xml version=\"1.0\"?>");
            for _ in 0..depth {
                s.push_str(&format!("<{name} xmlns=\"http://x\">"));
            }
            for _ in 0..depth {
                s.push_str(&format!("</{name}>"));
            }
            let _ = parse_soap_request(&s);
        }

        #[test]
        fn valid_request_roundtrips_action_and_token(
            action in "[A-Z][A-Za-z0-9]{0,31}",
            user in "[a-z]{1,8}",
        ) {
            let body = format!(
                "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:wsse=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\"><s:Header><wsse:Security><wsse:UsernameToken><wsse:Username>{user}</wsse:Username><wsse:Password Type=\"#Digest\">Zm9v</wsse:Password><wsse:Nonce>bm9uY2U=</wsse:Nonce><wsu:Created xmlns:wsu=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd\">2026-09-08T00:00:00Z</wsu:Created></wsse:UsernameToken></wsse:Security></s:Header><s:Body><{action}/></s:Body></s:Envelope>"
            );
            let parsed = parse_soap_request(&body).expect("well-formed request must parse");
            prop_assert_eq!(parsed.action, action);
            let token = parsed.username_token.as_ref().expect("token must parse");
            prop_assert_eq!(token.username.as_str(), user.as_str());
            prop_assert_eq!(token.nonce.as_str(), "bm9uY2U=");
            prop_assert_eq!(token.created.as_str(), "2026-09-08T00:00:00Z");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namespaces::SOAP_ENVELOPE;
    use crate::types::serialize_soap_response;

    // ------------------------------------------------------------------
    // SOAP parsing
    // ------------------------------------------------------------------

    #[test]
    fn test_parse_simple_envelope() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
        <soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope">
          <soap:Header/>
          <soap:Body>
            <GetDeviceInformation xmlns="http://www.onvif.org/ver10/device/wsdl"/>
          </soap:Body>
        </soap:Envelope>"#;

        let parsed = parse_soap_request(xml).unwrap();
        assert_eq!(parsed.action, "GetDeviceInformation");
        assert!(parsed.body_xml.contains("GetDeviceInformation"));
        assert!(parsed.username_token.is_none());
    }

    #[test]
    fn test_parse_body_xml_is_balanced() {
        // Regression: the writer used to emit the closing `</soap:Body>` (and
        // could not emit its opening twin) into body_xml, leaving a dangling
        // close tag. Substring-searching handlers tolerated it, but strict
        // downstream parsers (imaging's SetImagingSettings) failed end-to-end
        // even though their unit tests — fed balanced fragments — passed.
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
        <soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope">
          <soap:Body>
            <SetImagingSettings xmlns="http://www.onvif.org/ver20/imaging/wsdl/">
              <Settings><Brightness Value="0.8"/></Settings>
            </SetImagingSettings>
          </soap:Body>
        </soap:Envelope>"#;

        let parsed = parse_soap_request(xml).unwrap();
        let body = &parsed.body_xml;
        assert!(
            !body.contains("</soap:Body>") && !body.contains("</s:Body>"),
            "body_xml must not leak the Body close tag: {body}"
        );
        assert!(
            body.trim_end().ends_with("</SetImagingSettings>"),
            "body_xml must end at the action element: {body}"
        );

        // The strict reader used by imaging's parse_settings must accept it.
        let mut reader = quick_xml::Reader::from_str(body);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(quick_xml::events::Event::Eof) | Err(_) => break,
                Ok(_) => {}
            }
            buf.clear();
        }
    }

    #[test]
    fn test_parse_with_security_header() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
        <soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
                       xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"
                       xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">
          <soap:Header>
            <wsse:Security>
              <wsse:UsernameToken>
                <wsse:Username>admin</wsse:Username>
                <wsse:Password>secret</wsse:Password>
                <wsse:Nonce>dGhpcyBpcyBhIG5vbmNl</wsse:Nonce>
                <wsu:Created>2025-01-01T00:00:00Z</wsu:Created>
              </wsse:UsernameToken>
            </wsse:Security>
          </soap:Header>
          <soap:Body>
            <GetProfiles xmlns="http://www.onvif.org/ver10/media/wsdl"/>
          </soap:Body>
        </soap:Envelope>"#;

        let parsed = parse_soap_request(xml).unwrap();
        assert_eq!(parsed.action, "GetProfiles");
        let token = parsed.username_token.expect("should have UsernameToken");
        assert_eq!(token.username, "admin");
        assert_eq!(token.password, "secret");
        assert_eq!(token.nonce, "dGhpcyBpcyBhIG5vbmNl");
        assert_eq!(token.created, "2025-01-01T00:00:00Z");
    }

    #[test]
    fn test_parse_empty_body() {
        let xml = r#"<?xml version="1.0"?>
        <soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope">
          <soap:Header/>
          <soap:Body/>
        </soap:Envelope>"#;

        let parsed = parse_soap_request(xml).unwrap();
        assert!(parsed.action.is_empty());
        assert!(parsed.body_xml.is_empty());
        assert!(parsed.username_token.is_none());
    }

    #[test]
    fn test_parse_malformed_xml() {
        // quick_xml is lenient: non-XML input produces no events but no error
        let result = parse_soap_request("not xml at all");
        if let Ok(parsed) = result {
            assert!(parsed.action.is_empty());
            assert!(parsed.body_xml.is_empty());
            assert!(parsed.username_token.is_none());
        }
        // Actual XML parse errors (e.g. unclosed tags) should still error
        let broken = parse_soap_request("<Envelope><Body><unclosed>");
        assert!(broken.is_err());
    }

    #[test]
    fn test_parse_with_default_namespace() {
        // Some ONVIF clients use default namespace instead of prefixed
        let xml = r#"<?xml version="1.0"?>
        <Envelope xmlns="http://www.w3.org/2003/05/soap-envelope">
          <Header/>
          <Body>
            <GetSystemDateAndTime xmlns="http://www.onvif.org/ver10/device/wsdl"/>
          </Body>
        </Envelope>"#;

        let parsed = parse_soap_request(xml).unwrap();
        assert_eq!(parsed.action, "GetSystemDateAndTime");
    }

    /// Namespace-aware check: every element and attribute prefix in `body`
    /// resolves, and the action element is in namespace `action_ns`.
    fn assert_body_namespaces_resolve(body: &str, action_ns: &str) {
        let mut reader = quick_xml::NsReader::from_str(body);
        let mut first = true;
        loop {
            match reader.read_resolved_event() {
                Ok((ns, Event::Start(e))) | Ok((ns, Event::Empty(e))) => {
                    let quick_xml::name::ResolveResult::Bound(ns) = ns else {
                        panic!("unbound element {:?} in {body}", e.name());
                    };
                    if first {
                        assert_eq!(ns.as_ref(), action_ns.as_bytes(), "action ns in {body}");
                        first = false;
                    }
                    for a in e.attributes() {
                        let a = a.unwrap();
                        let (res, _) = reader.resolver().resolve_attribute(a.key);
                        assert!(
                            !matches!(res, quick_xml::name::ResolveResult::Unknown(_)),
                            "unbound attribute {:?} in {body}",
                            a.key
                        );
                    }
                }
                Ok((_, Event::Eof)) => break,
                Ok(_) => {}
                Err(e) => panic!("{e} in {body}"),
            }
        }
        assert!(!first, "no element in {body}");
    }

    #[test]
    fn test_parse_body_inherits_envelope_namespaces() {
        // gSOAP-based clients (ONVIF Device Manager, ...) declare every prefix
        // on the Envelope and use them inside the Body.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope"
                           xmlns:tt="http://www.onvif.org/ver10/schema"
                           xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl">
          <SOAP-ENV:Body xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
            <tptz:ContinuousMove>
              <tptz:ProfileToken>profile_1</tptz:ProfileToken>
              <tptz:Velocity>
                <tt:PanTilt x="0.5" y="0" xsi:type="tt:Vector2D"/>
              </tptz:Velocity>
            </tptz:ContinuousMove>
          </SOAP-ENV:Body>
        </SOAP-ENV:Envelope>"#;

        let parsed = parse_soap_request(xml).unwrap();
        assert_eq!(parsed.action, "ContinuousMove");
        let body = &parsed.body_xml;
        for decl in [
            r#"xmlns:tt="http://www.onvif.org/ver10/schema""#,
            r#"xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl""#,
            r#"xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance""#,
            r#"xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope""#,
        ] {
            assert!(body.contains(decl), "missing {decl} in {body}");
        }
        assert_body_namespaces_resolve(body, "http://www.onvif.org/ver20/ptz/wsdl");
    }

    #[test]
    fn test_parse_body_keeps_own_namespace_redeclaration() {
        // A prefix redeclared on the action element wins over the Envelope's.
        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
                       xmlns:tds="urn:outer"><s:Body><tds:GetDeviceInformation xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/></s:Body></s:Envelope>"#;

        let parsed = parse_soap_request(xml).unwrap();
        assert_eq!(parsed.action, "GetDeviceInformation");
        let body = &parsed.body_xml;
        assert!(!body.contains("urn:outer"), "outer binding leaked: {body}");
        assert_eq!(body.matches("xmlns:tds=").count(), 1, "{body}");
        assert_body_namespaces_resolve(body, "http://www.onvif.org/ver10/device/wsdl");
    }

    #[test]
    fn test_parse_body_inherits_default_namespace() {
        // Default namespace declared on the Envelope, unprefixed action.
        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
                       xmlns="http://www.onvif.org/ver10/device/wsdl"><s:Body><GetHostname/></s:Body></s:Envelope>"#;

        let parsed = parse_soap_request(xml).unwrap();
        assert_eq!(parsed.action, "GetHostname");
        let body = &parsed.body_xml;
        assert!(
            body.contains(r#"xmlns="http://www.onvif.org/ver10/device/wsdl""#),
            "missing default namespace in {body}"
        );
        assert_body_namespaces_resolve(body, "http://www.onvif.org/ver10/device/wsdl");

        // ...and an action that sets its own default namespace keeps it.
        let xml = r#"<Envelope xmlns="http://www.w3.org/2003/05/soap-envelope"><Body><GetProfiles xmlns="http://www.onvif.org/ver10/media/wsdl"/></Body></Envelope>"#;
        let body = parse_soap_request(xml).unwrap().body_xml;
        assert_eq!(body.matches("xmlns=").count(), 1, "{body}");
        assert_body_namespaces_resolve(&body, "http://www.onvif.org/ver10/media/wsdl");
    }

    #[test]
    fn test_parse_body_ignores_header_namespaces() {
        // Declarations on Header descendants are out of scope for the Body.
        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Header><wsse:Security xmlns:wsse="urn:wsse"/></s:Header><s:Body><GetUsers xmlns="http://www.onvif.org/ver10/device/wsdl"/></s:Body></s:Envelope>"#;

        let body = parse_soap_request(xml).unwrap().body_xml;
        assert!(!body.contains("urn:wsse"), "Header binding leaked: {body}");
        assert_body_namespaces_resolve(&body, "http://www.onvif.org/ver10/device/wsdl");
    }

    // ------------------------------------------------------------------
    // Auth integration
    // ------------------------------------------------------------------

    #[test]
    fn test_parse_and_auth_plaintext() {
        let xml = r#"<?xml version="1.0"?>
        <soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
                       xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd">
          <soap:Header>
            <wsse:Security>
              <wsse:UsernameToken>
                <wsse:Username>admin</wsse:Username>
                <wsse:Password>correct</wsse:Password>
              </wsse:UsernameToken>
            </wsse:Security>
          </soap:Header>
          <soap:Body>
            <GetDeviceInformation/>
          </soap:Body>
        </soap:Envelope>"#;

        let parsed = parse_soap_request(xml).unwrap();
        let token = parsed.username_token.unwrap();

        // Verify correct password
        assert!(verify_username_token(&token, "admin", "correct"));
        // Verify wrong password
        assert!(!verify_username_token(&token, "admin", "wrong"));
    }

    // ------------------------------------------------------------------
    // Fault generation
    // ------------------------------------------------------------------

    #[test]
    fn test_fault_action_not_supported() {
        let fault = serialize_soap_fault("soap:Sender", "unsupported action: GetFoo");
        assert!(fault.contains("soap:Fault"));
        assert!(fault.contains("unsupported action: GetFoo"));
        assert!(fault.contains(SOAP_ENVELOPE));
    }

    #[test]
    fn test_fault_not_authorized() {
        let fault = serialize_soap_fault(
            "soap:Sender",
            "authentication required for action: SetConfiguration",
        );
        assert!(fault.contains("soap:Fault"));
        assert!(fault.contains("authentication required"));
    }

    // ------------------------------------------------------------------
    // Action router
    // ------------------------------------------------------------------

    struct TestHandler {
        response: String,
    }

    #[async_trait]
    impl OnvifActionHandler for TestHandler {
        async fn handle(&self, _body: &str, _info: &RequestInfo) -> Result<String, OnvifError> {
            Ok(self.response.clone())
        }
    }

    #[test]
    fn test_action_router_dispatch() {
        let mut server = OnvifServer::new(&OnvifConfig {
            port: 0, // won't actually listen
            username: "admin".into(),
            password: "pass".into(),
            ..Default::default()
        });

        server.register_handler(
            "GetStatus",
            Box::new(TestHandler {
                response: "<GetStatusResponse/>".into(),
            }),
        );

        // Verify handler was registered
        assert!(server.handlers.contains_key("GetStatus"));
        assert_eq!(server.handlers.len(), 1);
    }

    #[test]
    fn test_action_router_unknown_action() {
        let server = OnvifServer::new(&OnvifConfig {
            port: 0,
            username: "admin".into(),
            password: "pass".into(),
            ..Default::default()
        });

        assert!(!server.handlers.contains_key("GetFoo"));
    }

    #[test]
    fn test_anonymous_action_registration() {
        let mut server = OnvifServer::new(&OnvifConfig {
            port: 0,
            username: "admin".into(),
            password: "pass".into(),
            ..Default::default()
        });

        assert!(!server.anonymous_actions.contains("GetSystemDateAndTime"));
        server.register_anonymous_action("GetSystemDateAndTime");
        assert!(server.anonymous_actions.contains("GetSystemDateAndTime"));
        assert!(!server.anonymous_actions.contains("GetCapabilities"));
    }

    // ------------------------------------------------------------------
    // TLS listener configuration (parity with onvif-go: both-or-neither)
    // ------------------------------------------------------------------

    fn tls_test_config() -> OnvifConfig {
        OnvifConfig {
            port: 0,
            username: "admin".into(),
            password: "pass".into(),
            ..Default::default()
        }
    }

    /// Exactly one of cert/key configured is a configuration error (the
    /// peer in onvif-go's `Start`: "TLSCertFile and TLSKeyFile must be set
    /// together").
    #[tokio::test]
    async fn tls_half_configured_fails_to_start() {
        for (cert, key) in [("cert.pem", ""), ("", "key.pem")] {
            let mut cfg = tls_test_config();
            cfg.tls_cert_file = cert.to_string();
            cfg.tls_key_file = key.to_string();
            let mut server = OnvifServer::new(&cfg);
            server.register_handler(
                "GetProfiles",
                Box::new(TestHandler {
                    response: "<x/>".into(),
                }),
            );
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let err = server
                .start_on(listener)
                .await
                .expect_err("half-configured TLS must fail");
            assert!(
                err.to_string().contains("must be set together"),
                "got: {err}"
            );
        }
    }

    /// With the `tls` cargo feature compiled out, configuring TLS must
    /// fail loudly instead of silently serving plain HTTP.
    #[cfg(not(feature = "tls"))]
    #[tokio::test]
    async fn tls_config_without_feature_fails_to_start() {
        let mut cfg = tls_test_config();
        cfg.tls_cert_file = "cert.pem".to_string();
        cfg.tls_key_file = "key.pem".to_string();
        let mut server = OnvifServer::new(&cfg);
        server.register_handler(
            "GetProfiles",
            Box::new(TestHandler {
                response: "<x/>".into(),
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let err = server
            .start_on(listener)
            .await
            .expect_err("TLS config without the feature must fail");
        assert!(err.to_string().contains("tls"), "got: {err}");
    }
    // ------------------------------------------------------------------
    // HTTP request parsing
    // ------------------------------------------------------------------

    #[test]
    fn test_read_http_request_parses_method() {
        // We test the method extraction logic directly
        let raw = b"POST /onvif/device_service HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
        let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let header_str = str::from_utf8(&raw[..header_end]).unwrap();
        let first_line = header_str.lines().next().unwrap();
        let method = first_line.split_whitespace().next().unwrap();
        assert_eq!(method, "POST");
    }

    #[test]
    fn test_read_http_request_content_length() {
        let raw = b"POST / HTTP/1.1\r\nContent-Length: 11\r\n\r\nHello World";
        let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let header_str = str::from_utf8(&raw[..header_end]).unwrap();
        let cl = header_str
            .lines()
            .find_map(|l| {
                let lower = l.to_ascii_lowercase();
                if lower.starts_with("content-length:") {
                    lower
                        .trim_start_matches("content-length:")
                        .trim()
                        .parse::<usize>()
                        .ok()
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(cl, 11);
    }

    // ------------------------------------------------------------------
    // Response serialization round-trip
    // ------------------------------------------------------------------

    #[test]
    fn test_response_round_trip() {
        let response = serialize_soap_response("<TestResponse>ok</TestResponse>");
        let parsed = parse_soap_request(&response).unwrap();
        assert_eq!(parsed.action, "TestResponse");
    }

    // ------------------------------------------------------------------
    // body_xml reconstruction
    // ------------------------------------------------------------------

    #[test]
    fn test_parse_body_xml_content() {
        let xml = r#"<?xml version="1.0"?>
        <soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope">
          <soap:Body>
            <GetDeviceInformation xmlns="http://www.onvif.org/ver10/device/wsdl">
              <SomeParam>value</SomeParam>
            </GetDeviceInformation>
          </soap:Body>
        </soap:Envelope>"#;

        let parsed = parse_soap_request(xml).unwrap();
        assert_eq!(parsed.action, "GetDeviceInformation");
        assert!(parsed.body_xml.contains("GetDeviceInformation"));
        assert!(parsed.body_xml.contains("SomeParam"));
        assert!(parsed.body_xml.contains("value"));
    }
}
