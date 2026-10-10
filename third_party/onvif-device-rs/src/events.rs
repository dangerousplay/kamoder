//! ONVIF Events service — pull-point and basic-notification subscriptions
//! (WS-BaseNotification).
//!
//! Mirrors onvif-go's `server/events.go` (the golden wire source) for the
//! pull-point half: with the service enabled the SOAP server routes
//! `{base}/events_service` (GetServiceCapabilities / GetEventProperties /
//! CreatePullPointSubscription / Subscribe / SetSynchronizationPoint)
//! and the per-subscription `{base}/events_service/sub/<id>` subtree
//! (PullMessages / Renew / Unsubscribe) to this module on the listener the
//! server already owns. The basic-notification half (wsnt:Subscribe + push
//! Notify, issue #50) goes beyond the Go twin.
//!
//! Notifications use the canonical WS-BaseNotification + ONVIF double-layer
//! shape: outer `wsnt:NotificationMessage` > `wsnt:Topic` +
//! `wsnt:Message` > inner `tt:Message` with PropertyOperation/UtcTime
//! attributes and Source/Key/Data `tt:SimpleItem` groups.
//!
//! ## Basic notification interface (wsnt:Subscribe)
//!
//! `Subscribe` registers a *push* subscription: the host
//! [`EventsService::publish_event`] seam fans out exactly as for pull
//! points (same per-subscription topic filter, same lossy bounded queue),
//! and one dedicated sender task per subscription POSTs each queued
//! notification as a `wsnt:Notify` SOAP document to the consumer's
//! ConsumerReference address — Topic + inner message bytes come from the
//! same writers the pull-point response uses, so push and pull payloads
//! stay consistent. Delivery is best-effort fire-and-forget over a
//! hand-rolled HTTP/1.1 POST (the library carries no HTTP-client
//! dependency): `https://` consumers are unsupported and refused at
//! Subscribe time; after three consecutive delivery failures the
//! subscription is auto-unsubscribed (spec-permissible housekeeping).
//! Renew and Unsubscribe operate on the returned SubscriptionReference
//! like any pull point; PullMessages on a push subscription is a Sender
//! fault (it is not a pull point).
//!
//! ## Decisions (issue #50)
//!
//! - **Pull-point cap**: [`DEFAULT_MAX_PULL_POINTS`] = 10 is shared between
//!   pull and basic subscriptions — the spec allows a device-defined
//!   limit, and 10 covers realistic NVR fan-out.
//! - **GetEventInstances**: deliberately NOT implemented (ONVIF 17.06+,
//!   zero demand) — the unknown-action fault answers it.
//! - **SetSynchronizationPoint**: served on the events endpoint only
//!   (dispatch is route-based, so the action name shared with the media
//!   service cannot collide); ack-only, because host-injected events
//!   carry no device-stored state to re-send.
//! - **TopicExpressionDialect**: GetEventProperties already advertises
//!   both mandatory dialects (Concrete + ConcreteSet), and both are
//!   honored — no byte change was needed for dialect completeness.
//! - **Capabilities bytes**: GetServiceCapabilities keeps its
//!   pull-point-shaped advertisement (`WSPullPointSupport`) — adding a
//!   `WSSubscriptionPolicySupport` attribute would change pinned response
//!   bytes, which this crate treats as a contract.
//!
//! ## Host seam
//!
//! [`EventsService::publish_event`] is the host-facing injection point
//! (parity with onvif-go's `Server.PublishEvent`): fan-out to every live
//! subscription, filtered per-subscription by its topic expression, safe
//! no-op with no subscribers, queues lossy at the head.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event as XmlEvent};
use quick_xml::{Reader, Writer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

use crate::namespaces::{EVENTS_SERVICE, SCHEMAS, WS_ADDRESSING, WS_NOTIFICATION, WS_TOPICS};
use crate::types::{serialize_soap_response, OnvifError};

// ---------------------------------------------------------------------------
// Tuning (parity with onvif-go server/events.go)
// ---------------------------------------------------------------------------

/// Advertised cap on concurrent pull-point subscriptions
/// (GetServiceCapabilities / MaxPullPoints).
pub const DEFAULT_MAX_PULL_POINTS: usize = 10;
/// Termination granted when CreatePullPointSubscription omits
/// InitialTerminationTime (1 h).
pub const DEFAULT_TERMINATION: Duration = Duration::from_secs(60 * 60);
/// Upper clamp for requested and renewed terminations (ONVIF-typical 24 h).
pub const MAX_TERMINATION: Duration = Duration::from_secs(24 * 60 * 60);
/// Server-side clamp on a PullMessages long poll — a hostile Timeout cannot
/// pin connections.
pub const MAX_PULL_WAIT: Duration = Duration::from_secs(60);
/// Per-subscription notification queue bound; queues are lossy at the head
/// (newest wins) like real device notification buffers. The same bound
/// backs pull points and basic-notification push queues.
pub const MAX_EVENT_QUEUE: usize = 100;
/// Consecutive Notify-delivery failures before a basic-notification
/// subscription is auto-unsubscribed (spec-permissible housekeeping: a
/// dead consumer must not pin queue and task forever).
const MAX_PUSH_FAILURES: u32 = 3;
/// Connect timeout for one Notify POST (fire-and-forget delivery must
/// not pin the sender task on an unreachable consumer).
const NOTIFY_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Write/read timeout for one Notify POST (Connection: close exchange —
/// consumers that never close read down to this bound).
const NOTIFY_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// HTTP path of the events service endpoint on the SOAP server's listener
/// (the crate's base path is `/onvif`, parity with onvif-go's BasePath).
pub const EVENTS_SERVICE_PATH: &str = "/onvif/events_service";
/// HTTP path prefix of the per-subscription endpoints returned in
/// SubscriptionReference addresses.
pub const SUBSCRIPTION_PATH_PREFIX: &str = "/onvif/events_service/sub/";

/// Canonical topic-namespace location advertised in GetEventProperties.
const TOPIC_NAMESPACE_LOCATION: &str = "http://www.onvif.org/ver10/tev/topicns.xml";
/// The two mandatory ONVIF topic-expression dialects (both honored — see
/// [`TopicFilter`]).
const DIALECT_CONCRETE: &str = "http://docs.oasis-open.org/wsn/t-1/TopicExpression/Concrete";
const DIALECT_CONCRETE_SET: &str = "http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet";
/// Message-content filtering is not applied — the spec-blessed single empty
/// MessageContentFilterDialect is advertised.
const MESSAGE_SCHEMA_LOCATION: &str = "http://www.onvif.org/ver10/schema/onvif.xsd";

// ---------------------------------------------------------------------------
// Host-facing event types
// ---------------------------------------------------------------------------

/// One `tt:SimpleItem` name/value pair of an event's Source/Key/Data group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleItem {
    pub name: String,
    pub value: String,
}

impl SimpleItem {
    /// Convenience constructor.
    #[must_use]
    pub fn new(name: &str, value: &str) -> Self {
        Self {
            name: name.to_string(),
            value: value.to_string(),
        }
    }
}

/// One property-event notification handed to the server by the host (the
/// [`EventsService::publish_event`] seam): an AI motion alarm, a tamper
/// switch, a signal-loss detector, … `UtcTime` and the default
/// `PropertyOperation` are stamped at publish time.
#[derive(Debug, Clone, Default)]
pub struct Event {
    /// Topic expression, e.g. `"tns1:VideoSource/MotionAlarm"`.
    pub topic: String,
    /// Property operation; empty defaults to `"Changed"`.
    pub property_operation: String,
    /// Source/Key/Data `SimpleItem` groups of the inner `tt:Message`.
    pub source: Vec<SimpleItem>,
    pub key: Vec<SimpleItem>,
    pub data: Vec<SimpleItem>,
}

impl Event {
    /// An event with just a topic (PropertyOperation defaults to `"Changed"`).
    #[must_use]
    pub fn new(topic: &str) -> Self {
        Self {
            topic: topic.to_string(),
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// ISO 8601 duration parsing
// ---------------------------------------------------------------------------

/// Parse the ISO 8601 duration subset ONVIF uses on the wire (`PnDTnHnMnS`,
/// `PT0S` included). Date-only forms are rejected — a lifetime without a
/// time component is not meaningful for subscription terminations and pull
/// timeouts (parity with onvif-go's `parseISO8601Duration`; arithmetic is
/// checked so an absurd duration is rejected instead of overflowing).
pub(crate) fn parse_iso8601_duration(s: &str) -> Option<Duration> {
    const HOUR_SECS: u64 = 3600;
    const MINUTE_SECS: u64 = 60;
    const DAY_SECS: u64 = 24 * 3600;

    let mut rest = s.strip_prefix('P')?;

    let mut days: u64 = 0;
    while !rest.is_empty() && !rest.starts_with('T') {
        let (num, remainder) = scan_duration_number(rest)?;
        rest = remainder;
        if !rest.starts_with('D') {
            return None;
        }
        days += num;
        rest = &rest[1..];
    }

    if rest.is_empty() || rest.len() == 1 {
        // No 'T' section: date-only ("P1D") or a bare "PT".
        return None;
    }
    rest = &rest[1..]; // consume 'T'

    let mut total = days.checked_mul(DAY_SECS)?;
    while !rest.is_empty() {
        let (num, remainder) = scan_duration_number(rest)?;
        rest = remainder;
        let unit_secs = match rest.as_bytes().first() {
            Some(b'H') => HOUR_SECS,
            Some(b'M') => MINUTE_SECS,
            Some(b'S') => 1,
            _ => return None,
        };
        total = total.checked_add(num.checked_mul(unit_secs)?)?;
        rest = &rest[1..];
    }

    Some(Duration::from_secs(total))
}

/// Read a run of digits off the front of `s`, returning it with the
/// remainder (parity with onvif-go's `scanDurationNumber`).
fn scan_duration_number(s: &str) -> Option<(u64, &str)> {
    let digits = s
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(s.len());
    if digits == 0 {
        return None;
    }
    let num: u64 = s[..digits].parse().ok()?;
    Some((num, &s[digits..]))
}

// ---------------------------------------------------------------------------
// Topic-expression filter
// ---------------------------------------------------------------------------

/// One subscription's topic-expression filter: the Concrete dialect (exact
/// local topic path) or the ConcreteSet dialect (`'|'` alternatives with
/// per-segment `'*'` wildcards) — the two dialects GetEventProperties
/// advertises.
#[derive(Debug, Clone)]
struct TopicFilter {
    /// Kept for parity with onvif-go's topicFilter (and future content
    /// filters); matching itself is dialect-independent.
    #[allow(dead_code)]
    dialect: String,
    expression: String,
}

impl TopicFilter {
    /// Whether a notification topic satisfies the filter. Prefixes are
    /// namespace bindings, not identity: matching compares the `/`-separated
    /// local path segments (`"tns1:VideoSource/MotionAlarm"` matches
    /// `"VideoSource/MotionAlarm"` and any equivalent binding).
    fn matches(&self, topic: &str) -> bool {
        let topic_segs = topic_path_segments(topic);
        self.expression
            .split('|')
            .any(|alternative| match_topic_path(&topic_path_segments(alternative), &topic_segs))
    }
}

/// Split a topic expression into local-name segments, stripping any
/// namespace prefix from each segment.
fn topic_path_segments(expr: &str) -> Vec<String> {
    expr.trim()
        .split('/')
        .map(|seg| match seg.rsplit_once(':') {
            Some((_, local)) => local,
            None => seg,
        })
        .map(str::to_string)
        .collect()
}

/// Compare a filter path against a topic path; a `"*"` filter segment
/// matches any single topic segment.
fn match_topic_path(filter: &[String], topic: &[String]) -> bool {
    !filter.is_empty()
        && filter.len() == topic.len()
        && filter.iter().zip(topic).all(|(f, t)| f == "*" || f == t)
}

/// Validate the CreatePullPointSubscription filter. An absent filter means
/// every topic is delivered; an empty dialect defaults to Concrete (the
/// WS-BaseNotification default); unsupported dialects and empty expressions
/// are Sender faults instead of being silently ignored.
fn parse_topic_filter(
    expr: Option<&TopicExpressionRequest>,
) -> Result<Option<TopicFilter>, OnvifError> {
    let Some(expr) = expr else {
        return Ok(None);
    };

    let value = expr.value.trim().to_string();
    let dialect = if expr.dialect.is_empty() {
        DIALECT_CONCRETE.to_string()
    } else {
        expr.dialect.clone()
    };

    if dialect != DIALECT_CONCRETE && dialect != DIALECT_CONCRETE_SET {
        return Err(OnvifError::SenderFault(format!(
            "Unsupported TopicExpression dialect: got {dialect:?}, device supports the \
             mandatory Concrete and ConcreteSet dialects"
        )));
    }
    if value.is_empty() {
        return Err(OnvifError::SenderFault(
            "Invalid TopicExpression: filter topic expression must not be empty".to_string(),
        ));
    }

    Ok(Some(TopicFilter {
        dialect,
        expression: value,
    }))
}

// ---------------------------------------------------------------------------
// Subscription registry
// ---------------------------------------------------------------------------

/// One queued event, stamped and buffered for one subscriber.
#[derive(Debug, Clone)]
struct QueuedNotification {
    topic: String,
    message: QueuedMessage,
}

/// The inner `tt:Message` payload (attributes + SimpleItem groups).
#[derive(Debug, Clone)]
struct QueuedMessage {
    property_operation: String,
    utc_time: String,
    source: Vec<SimpleItem>,
    key: Vec<SimpleItem>,
    data: Vec<SimpleItem>,
}

/// One live subscription. `consumer: None` → a classic pull point (the
/// client PullMessages); `Some(address)` → a basic-notification (push)
/// subscription whose Notify messages are POSTed to the consumer address
/// by a dedicated sender task sharing this same queue.
struct PullPoint {
    /// Termination instant — precise expiry comparisons (the Go twin keeps
    /// nanosecond `time.Time`; `termination_unix` below is only the
    /// second-truncated RFC3339 wire form).
    termination: Instant,
    termination_unix: u64,
    queue: VecDeque<QueuedNotification>,
    /// Cap-1-style wakeup for long-polling PullMessages.
    notify: Arc<Notify>,
    /// `None` → every topic is delivered.
    filter: Option<TopicFilter>,
    /// The ConsumerReference address when this is a basic-notification
    /// (push) subscription; `None` for a pull point.
    consumer: Option<String>,
}

/// The push target of one basic-notification subscription, parsed off the
/// ConsumerReference address — `http://` only (the library carries no TLS
/// client dependency; https consumers are a documented non-goal).
#[derive(Debug, Clone)]
struct HttpTarget {
    /// Host or IPv6 literal, brackets stripped.
    host: String,
    port: u16,
    /// Request path, including any query string.
    path: String,
}

impl HttpTarget {
    /// Host as it appears in the Host header and URLs (bracketed IPv6).
    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }
}

impl std::fmt::Display for HttpTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "http://{}:{}{}",
            self.host_header(),
            self.port,
            self.path
        )
    }
}

/// Parse a ConsumerReference address into an [`HttpTarget`]. Non-http
/// schemes and https are Sender faults (no HTTP-client or TLS dependency
/// is carried for Notify delivery).
fn parse_http_url(url: &str) -> Result<HttpTarget, OnvifError> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        OnvifError::SenderFault(format!(
            "Unsupported ConsumerReference address: got {url:?}, device supports \
             http:// consumers only"
        ))
    })?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = split_host_port(authority)?;
    if host.is_empty() {
        return Err(OnvifError::SenderFault(format!(
            "Invalid ConsumerReference address: no host in {url:?}"
        )));
    }
    Ok(HttpTarget { host, port, path })
}

/// Split a URL authority (`host`, `host:port`, `[v6]`, `[v6]:port`) into
/// the host (brackets stripped) and port (defaulting to 80).
fn split_host_port(authority: &str) -> Result<(String, u16), OnvifError> {
    let invalid = || {
        OnvifError::SenderFault(format!(
            "Invalid ConsumerReference address: cannot read host and port from {authority:?}"
        ))
    };
    if let Some(rest) = authority.strip_prefix('[') {
        // RFC 3986: IPv6 literals are bracketed.
        let Some((host, tail)) = rest.split_once(']') else {
            return Err(invalid());
        };
        let port = if tail.is_empty() {
            80
        } else {
            tail.strip_prefix(':')
                .and_then(|p| p.parse::<u16>().ok())
                .ok_or_else(invalid)?
        };
        Ok((host.to_string(), port))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => Ok((
                host.to_string(),
                port.parse::<u16>().map_err(|_| invalid())?,
            )),
            None => Ok((authority.to_string(), 80)),
        }
    }
}

/// Address book for building SubscriptionReference / ProducerReference
/// addresses on the listener the server already owns.
#[derive(Debug, Clone)]
pub(crate) struct ServiceEndpoint {
    /// Local IP that received the client's connection.
    pub host: String,
    /// The listener's actual port (an ephemeral port with `start_on`).
    pub port: u16,
}

impl ServiceEndpoint {
    /// `http://host:port/onvif/events_service/sub/<id>`
    fn subscription_address(&self, id: &str) -> String {
        format!(
            "http://{}:{}{SUBSCRIPTION_PATH_PREFIX}{id}",
            self.host, self.port
        )
    }

    /// The producer reference: the advertised device service endpoint.
    fn device_service_address(&self) -> String {
        format!("http://{}:{}/onvif/device_service", self.host, self.port)
    }
}

/// The events service: the subscription registry plus the host publish
/// seam. Shared between the SOAP server (routing), the host
/// ([`EventsService::publish_event`]), and the per-subscription push
/// sender tasks via `Arc`.
pub struct EventsService {
    subs: Arc<Mutex<HashMap<String, PullPoint>>>,
}

impl Default for EventsService {
    fn default() -> Self {
        Self::new()
    }
}

impl EventsService {
    /// A fresh service with an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            subs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Fan an event out to every live subscription, pull points and
    /// basic-notification (push) subscriptions alike (the host seam,
    /// parity with onvif-go's `Server.PublishEvent`). With no subscribers
    /// it is a safe no-op. Queues are lossy at the head: a slow subscriber
    /// beyond [`MAX_EVENT_QUEUE`] pending messages loses the oldest first.
    /// Push delivery itself never blocks here — the sender tasks own the
    /// network side.
    pub fn publish_event(&self, ev: Event) {
        let operation = if ev.property_operation.is_empty() {
            "Changed".to_string()
        } else {
            ev.property_operation
        };
        let qn = QueuedNotification {
            topic: ev.topic,
            message: QueuedMessage {
                property_operation: operation,
                utc_time: rfc3339(unix_now()),
                source: ev.source,
                key: ev.key,
                data: ev.data,
            },
        };

        let mut subs = lock_subs(&self.subs);
        prune_expired(&mut subs);

        for pp in subs.values_mut() {
            if let Some(filter) = &pp.filter {
                if !filter.matches(&qn.topic) {
                    continue;
                }
            }
            if pp.queue.len() >= MAX_EVENT_QUEUE {
                pp.queue.pop_front();
            }
            pp.queue.push_back(qn.clone());
            pp.notify.notify_one();
        }
    }

    /// Whether `action` belongs to the events service endpoint.
    #[must_use]
    pub(crate) fn is_service_action(action: &str) -> bool {
        matches!(
            action,
            "GetServiceCapabilities"
                | "GetEventProperties"
                | "CreatePullPointSubscription"
                | "Subscribe"
                | "SetSynchronizationPoint"
        )
    }

    /// Whether `action` belongs to the per-subscription endpoint.
    #[must_use]
    pub(crate) fn is_subscription_action(action: &str) -> bool {
        matches!(action, "PullMessages" | "Renew" | "Unsubscribe")
    }

    /// Auth policy for events actions, mirroring onvif-go's
    /// `DefaultAuthPolicy` (write-style prefixes `Set`/`Remove`/`Create`/
    /// `Go` are protected, reads stay open):
    /// `CreatePullPointSubscription` and `SetSynchronizationPoint` (Set*
    /// prefix) require WS-Security credentials;
    /// GetServiceCapabilities/GetEventProperties/PullMessages/Renew/
    /// Unsubscribe/Subscribe stay open — Subscribe is deliberately open
    /// for prefix-policy parity with the Go twin.
    #[must_use]
    pub(crate) fn action_requires_auth(action: &str) -> bool {
        ["Set", "Remove", "Create", "Go"]
            .iter()
            .any(|p| action.starts_with(p))
    }

    /// Dispatch a service-endpoint action (router pre-checked membership).
    pub(crate) async fn handle_service_action(
        &self,
        action: &str,
        body: &str,
        endpoint: &ServiceEndpoint,
    ) -> Result<String, OnvifError> {
        match action {
            "GetServiceCapabilities" => {
                Ok(serialize_soap_response(&build_get_service_capabilities()))
            }
            "GetEventProperties" => Ok(serialize_soap_response(&build_get_event_properties())),
            "CreatePullPointSubscription" => self
                .create_subscription(body, endpoint)
                .await
                .map(|fragment| serialize_soap_response(&fragment)),
            // Basic notification interface (issue #50): a push
            // subscription whose Notify messages the sender task POSTs to
            // the consumer address.
            "Subscribe" => self
                .subscribe(body, endpoint)
                .await
                .map(|fragment| serialize_soap_response(&fragment)),
            // Shared action name with the media service — route-based
            // dispatch means the events endpoint answers it independently
            // (no collision); ack-only (see the builder's doc).
            "SetSynchronizationPoint" => Ok(serialize_soap_response(
                &build_set_synchronization_point_response(),
            )),
            _ => Err(OnvifError::ActionNotSupported(format!(
                "unsupported action: {action}"
            ))),
        }
    }

    /// Dispatch a subscription-endpoint action. `path` is the raw request
    /// URL path (the subscription id is addressed by the URL, not the body).
    pub(crate) async fn handle_subscription_action(
        &self,
        path: &str,
        action: &str,
        body: &str,
        endpoint: &ServiceEndpoint,
    ) -> Result<String, OnvifError> {
        match action {
            "PullMessages" => self.pull_messages(path, body, endpoint).await,
            "Renew" => self.renew(path, body).await,
            "Unsubscribe" => self.unsubscribe(path).await,
            _ => Err(OnvifError::ActionNotSupported(format!(
                "unsupported action: {action}"
            ))),
        }
    }

    /// CreatePullPointSubscription: parse the filter and termination time,
    /// mint an opaque subscription id, and answer the SubscriptionReference
    /// address plus the granted termination window.
    async fn create_subscription(
        &self,
        body: &str,
        endpoint: &ServiceEndpoint,
    ) -> Result<String, OnvifError> {
        let req = parse_create_request(body)?;
        let filter = parse_topic_filter(req.topic_expression.as_ref())?;

        // Go twin semantics: an absent OR empty InitialTerminationTime
        // grants the default window; anything else must parse positive and
        // is clamped to MAX_TERMINATION (no lower clamp).
        let mut termination = DEFAULT_TERMINATION;
        match req.initial_termination_time.as_deref() {
            None | Some("") => {}
            Some(raw) => {
                let parsed = parse_iso8601_duration(raw)
                    .filter(|d| !d.is_zero())
                    .ok_or_else(|| {
                        OnvifError::SenderFault(format!(
                            "Invalid InitialTerminationTime: got {raw:?}, want a positive \
                             ISO 8601 duration"
                        ))
                    })?;
                termination = parsed.min(MAX_TERMINATION);
            }
        }

        let id = random_subscription_id();
        let now = unix_now();

        {
            let mut subs = lock_subs(&self.subs);
            prune_expired(&mut subs);
            if subs.len() >= DEFAULT_MAX_PULL_POINTS {
                return Err(OnvifError::SenderFault(format!(
                    "Too many active pull point subscriptions: device supports at most \
                     {DEFAULT_MAX_PULL_POINTS} concurrent pull points"
                )));
            }
            subs.insert(
                id.clone(),
                PullPoint {
                    termination: Instant::now() + termination,
                    termination_unix: now + termination.as_secs(),
                    queue: VecDeque::new(),
                    notify: Arc::new(Notify::new()),
                    filter,
                    consumer: None,
                },
            );
        }

        Ok(build_create_subscription_response(
            &endpoint.subscription_address(&id),
            now,
            now + termination.as_secs(),
        ))
    }

    /// wsnt:Subscribe (basic notification interface, issue #50): register
    /// a push subscription. The registry entry is the same shape a pull
    /// point uses — same topic filter, same queue, same termination
    /// semantics — plus the consumer address; a dedicated sender task
    /// (see [`spawn_push_sender`]) drains the queue and POSTs each
    /// notification to the consumer. Renew/Unsubscribe work on the
    /// returned SubscriptionReference address; PullMessages does not.
    /// SubscriptionPolicy is ignored when present; only `http://`
    /// consumers are supported.
    async fn subscribe(
        &self,
        body: &str,
        endpoint: &ServiceEndpoint,
    ) -> Result<String, OnvifError> {
        let req = parse_subscribe_request(body)?;
        let consumer = req
            .consumer_address
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                OnvifError::SenderFault(
                    "Invalid Subscribe: a ConsumerReference/Address is required".to_string(),
                )
            })?;
        let target = parse_http_url(&consumer)?;
        let filter = parse_topic_filter(req.topic_expression.as_ref())?;

        // Same termination semantics as CreatePullPointSubscription: an
        // absent or empty TerminationTime grants the default window;
        // anything else must parse positive and is clamped to
        // MAX_TERMINATION (no lower clamp).
        let mut termination = DEFAULT_TERMINATION;
        match req.termination_time.as_deref() {
            None | Some("") => {}
            Some(raw) => {
                let parsed = parse_iso8601_duration(raw)
                    .filter(|d| !d.is_zero())
                    .ok_or_else(|| {
                        OnvifError::SenderFault(format!(
                            "Invalid TerminationTime: got {raw:?}, want a positive ISO 8601 \
                             duration"
                        ))
                    })?;
                termination = parsed.min(MAX_TERMINATION);
            }
        }

        let id = random_subscription_id();
        let now = unix_now();
        let notify = Arc::new(Notify::new());
        let term_instant = Instant::now() + termination;

        {
            let mut subs = lock_subs(&self.subs);
            prune_expired(&mut subs);
            if subs.len() >= DEFAULT_MAX_PULL_POINTS {
                return Err(OnvifError::SenderFault(format!(
                    "Too many active subscriptions: device supports at most \
                     {DEFAULT_MAX_PULL_POINTS} concurrent subscriptions (pull points and basic \
                     notification share the cap)"
                )));
            }
            subs.insert(
                id.clone(),
                PullPoint {
                    termination: term_instant,
                    termination_unix: now + termination.as_secs(),
                    queue: VecDeque::new(),
                    notify: Arc::clone(&notify),
                    filter,
                    consumer: Some(consumer),
                },
            );
        }

        let address = endpoint.subscription_address(&id);
        spawn_push_sender(
            Arc::clone(&self.subs),
            id,
            address.clone(),
            target,
            term_instant,
            notify,
        );

        Ok(serialize_soap_response(&build_subscribe_response(
            &address,
            now,
            now + termination.as_secs(),
        )))
    }

    /// PullMessages: long-poll the subscription addressed by `path`; answer
    /// as soon as a notification is queued, when the requested Timeout
    /// (clamped to [`MAX_PULL_WAIT`]) elapses, or the client goes away.
    /// `PT0S` is legal — an immediate, non-blocking poll.
    async fn pull_messages(
        &self,
        path: &str,
        body: &str,
        endpoint: &ServiceEndpoint,
    ) -> Result<String, OnvifError> {
        let req = parse_pull_request(body)?;

        if req.message_limit <= 0 {
            return Err(OnvifError::SenderFault(format!(
                "Invalid MessageLimit: got {}, want a positive message limit",
                req.message_limit
            )));
        }
        // PT0S is legal: an immediate, non-blocking poll. Only unparsable
        // timeouts fault.
        let wait = parse_iso8601_duration(&req.timeout).ok_or_else(|| {
            OnvifError::SenderFault(format!(
                "Invalid Timeout: got {:?}, want an ISO 8601 duration",
                req.timeout
            ))
        })?;
        let deadline = tokio::time::Instant::now() + wait.min(MAX_PULL_WAIT);
        let limit = usize::try_from(req.message_limit).unwrap_or(usize::MAX);

        loop {
            // Register the wakeup BEFORE draining: a publish between the
            // drain and the await still wakes this poller (Notify keeps a
            // pending permit when no waiter is registered yet).
            let (notify, drained, termination) = {
                let mut subs = lock_subs(&self.subs);
                let pp = live_pull_point(&mut subs, path)?;
                if pp.consumer.is_some() {
                    return Err(OnvifError::SenderFault(format!(
                        "PullMessages is a pull-point operation, and the subscription at {path} \
                         is not a pull point (basic notification subscriptions serve Renew \
                         and Unsubscribe only)"
                    )));
                }
                let notify = Arc::clone(&pp.notify);
                let count = limit.min(pp.queue.len());
                let drained: Vec<QueuedNotification> = pp.queue.drain(..count).collect();
                (notify, drained, pp.termination_unix)
            };
            let notified = notify.notified();

            if !drained.is_empty() || tokio::time::Instant::now() >= deadline {
                return Ok(serialize_soap_response(&build_pull_messages_response(
                    unix_now(),
                    termination,
                    &drained,
                    endpoint,
                )));
            }

            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        }
    }

    /// Renew: extend the addressed subscription's termination time.
    async fn renew(&self, path: &str, body: &str) -> Result<String, OnvifError> {
        let req = parse_renew_request(body)?;
        let duration = parse_iso8601_duration(&req.termination_time)
            .filter(|d| !d.is_zero())
            .ok_or_else(|| {
                OnvifError::SenderFault(format!(
                    "Invalid TerminationTime: got {:?}, want a positive ISO 8601 duration",
                    req.termination_time
                ))
            })?;

        let now = unix_now();
        let granted = duration.min(MAX_TERMINATION);
        let mut subs = lock_subs(&self.subs);
        let pp = live_pull_point(&mut subs, path)?;
        pp.termination = Instant::now() + granted;
        pp.termination_unix = now + granted.as_secs();
        Ok(serialize_soap_response(&build_renew_response(
            now,
            pp.termination_unix,
        )))
    }

    /// Unsubscribe: remove the addressed subscription.
    async fn unsubscribe(&self, path: &str) -> Result<String, OnvifError> {
        let id = subscription_id_from_path(path)?;
        let mut subs = lock_subs(&self.subs);

        let live = subs
            .get(&id)
            .is_some_and(|pp| Instant::now() <= pp.termination);
        // Expired pull points are pruned and reported like unknown ones —
        // both are gone from the client's perspective.
        subs.remove(&id);
        if !live {
            return Err(unknown_subscription_fault(&id));
        }
        Ok(serialize_soap_response(&build_unsubscribe_response()))
    }
}

// ---------------------------------------------------------------------------
// Basic notification push delivery
// ---------------------------------------------------------------------------

/// One basic-notification sender task: wakes on the subscription's
/// notifier, drains the shared queue, and POSTs each notification as a
/// wsnt:Notify to the consumer address. Delivery is fire-and-forget — the
/// publish path only queues; this task owns the network, so the host seam
/// never blocks and the queue bounds memory (lossy at the head like every
/// other subscription queue). The task exits when the subscription is
/// unsubscribed or pruned elsewhere, at its termination deadline, or —
/// spec-permissible housekeeping — after [`MAX_PUSH_FAILURES`]
/// consecutive delivery failures, removing the subscription so a dead
/// consumer cannot pin resources forever.
fn spawn_push_sender(
    subs: Arc<Mutex<HashMap<String, PullPoint>>>,
    id: String,
    subscription_address: String,
    target: HttpTarget,
    termination: Instant,
    notify: Arc<Notify>,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::from_std(termination);
        let mut consecutive_failures: u32 = 0;
        loop {
            // Register the wakeup BEFORE draining (see pull_messages).
            let notified = notify.notified();
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep_until(deadline) => break,
            }

            let drained: Vec<QueuedNotification> = {
                let mut guard = lock_subs(&subs);
                match guard.get_mut(&id) {
                    // Unsubscribed or pruned elsewhere — nothing left to do.
                    None => break,
                    Some(pp) => pp.queue.drain(..).collect(),
                }
            };

            for qn in drained {
                let envelope = build_notify_envelope(&subscription_address, &qn);
                if let Err(e) = post_notify(&target, &envelope).await {
                    consecutive_failures += 1;
                    log::warn!(
                        "onvif events: Notify delivery to consumer {target} failed ({e}); \
                         consecutive failures: {consecutive_failures}"
                    );
                    if consecutive_failures >= MAX_PUSH_FAILURES {
                        lock_subs(&subs).remove(&id);
                        log::warn!(
                            "onvif events: auto-unsubscribed basic notification subscription \
                             {id} after {MAX_PUSH_FAILURES} consecutive delivery failures"
                        );
                        return;
                    }
                } else {
                    consecutive_failures = 0;
                }
            }
        }
    });
}

/// Fire-and-forget HTTP/1.1 POST of one Notify envelope
/// (application/soap+xml) to the consumer — hand-rolled over `TcpStream`
/// because the library carries no HTTP-client dependency. Redirects,
/// keep-alive, and chunked responses are out of scope (Connection: close,
/// single request per connection). The write half is shut down after
/// sending so even a close-delimited consumer can answer. `Ok(())` means
/// the consumer answered 2xx.
async fn post_notify(target: &HttpTarget, envelope: &str) -> Result<(), String> {
    let connect = tokio::net::TcpStream::connect((target.host.as_str(), target.port));
    let mut stream = tokio::time::timeout(NOTIFY_CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| format!("connect timeout after {NOTIFY_CONNECT_TIMEOUT:?}"))?
        .map_err(|e| format!("connect: {e}"))?;

    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\n\
         Content-Type: application/soap+xml; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{envelope}",
        target.path,
        target.host_header(),
        target.port,
        envelope.len()
    );
    tokio::time::timeout(NOTIFY_IO_TIMEOUT, stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| "write timeout".to_string())?
        .map_err(|e| format!("write: {e}"))?;
    // Half-close the write side: the request is Content-Length delimited,
    // so compliant servers already have it, and naive close-delimited
    // consumers see request end without us dropping the response.
    let _ = stream.shutdown().await;

    let mut response = Vec::new();
    tokio::time::timeout(NOTIFY_IO_TIMEOUT, stream.read_to_end(&mut response))
        .await
        .map_err(|_| "read timeout".to_string())?
        .map_err(|e| format!("read: {e}"))?;

    let status = http_status(&response)?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!("consumer answered HTTP {status}"))
    }
}

/// The numeric status off a raw HTTP response, or an error when the
/// consumer answered something that is not an HTTP status line.
fn http_status(response: &[u8]) -> Result<u16, String> {
    let head = std::str::from_utf8(response).map_err(|_| "non-UTF-8 response".to_string())?;
    head.lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| "malformed HTTP status line".to_string())
}

// ---------------------------------------------------------------------------
// Registry helpers
// ---------------------------------------------------------------------------

/// Lock the subscription registry (poison-tolerant, like `AuthState`).
fn lock_subs(
    subs: &Mutex<HashMap<String, PullPoint>>,
) -> std::sync::MutexGuard<'_, HashMap<String, PullPoint>> {
    match subs.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Drop pull points past their termination time (callers hold the lock).
fn prune_expired(subs: &mut HashMap<String, PullPoint>) {
    let now = Instant::now();
    subs.retain(|_, pp| now <= pp.termination);
}

/// The Sender fault for gone, unknown, or expired pull points.
fn unknown_subscription_fault(id: &str) -> OnvifError {
    OnvifError::SenderFault(format!("Unknown subscription: no pull point for id {id}"))
}

/// Resolve the live pull point addressed by a request URL path. Callers
/// hold the lock; expired pull points are pruned and reported like unknown
/// ones (both are gone from the client's perspective).
fn live_pull_point<'a>(
    subs: &'a mut HashMap<String, PullPoint>,
    path: &str,
) -> Result<&'a mut PullPoint, OnvifError> {
    let id = subscription_id_from_path(path)?;
    let expired = subs
        .get(&id)
        .is_some_and(|pp| Instant::now() > pp.termination);
    if expired {
        subs.remove(&id);
    }
    subs.get_mut(&id)
        .ok_or_else(|| unknown_subscription_fault(&id))
}

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

/// A parsed TopicExpression (Dialect attribute + chardata expression).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TopicExpressionRequest {
    dialect: String,
    value: String,
}

/// Parsed CreatePullPointSubscription body.
#[derive(Debug, Default)]
struct CreateRequest {
    initial_termination_time: Option<String>,
    topic_expression: Option<TopicExpressionRequest>,
}

/// Parsed PullMessages body. Absent fields keep the zero values the Go twin
/// faults on (MessageLimit 0, empty Timeout).
#[derive(Debug, Default)]
struct PullRequest {
    timeout: String,
    message_limit: i64,
}

/// Parsed Renew body.
#[derive(Debug, Default)]
struct RenewRequest {
    termination_time: String,
}

fn parse_create_request(body: &str) -> Result<CreateRequest, OnvifError> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut result = CreateRequest::default();
    #[derive(Default)]
    struct State {
        in_filter: bool,
        in_topic_expression: bool,
        in_initial_termination_time: bool,
    }
    let mut st = State::default();
    let mut text_acc = crate::types::TextAccumulator::new();

    loop {
        let event = reader.read_event_into(&mut buf);
        match event {
            Ok(XmlEvent::Text(e)) => {
                let _ = text_acc.push_text(&e);
            }
            Ok(XmlEvent::GeneralRef(e)) => {
                let _ = text_acc.push_ref(&e);
            }
            other => {
                let text = text_acc.flush();
                match other {
                    Ok(XmlEvent::Start(e)) => match local_name(e.name().as_ref()) {
                        "Filter" => st.in_filter = true,
                        "TopicExpression" if st.in_filter => {
                            st.in_topic_expression = true;
                            result.topic_expression = Some(TopicExpressionRequest {
                                dialect: attribute_by_local_name(&e, "Dialect"),
                                value: String::new(),
                            });
                        }
                        "InitialTerminationTime" => {
                            st.in_initial_termination_time = true;
                            result.initial_termination_time = Some(String::new());
                        }
                        _ => {}
                    },
                    Ok(XmlEvent::Empty(e)) => match local_name(e.name().as_ref()) {
                        "TopicExpression" if st.in_filter => {
                            result.topic_expression = Some(TopicExpressionRequest {
                                dialect: attribute_by_local_name(&e, "Dialect"),
                                value: String::new(),
                            });
                        }
                        "InitialTerminationTime" => {
                            result.initial_termination_time = Some(String::new());
                        }
                        _ => {}
                    },
                    Ok(XmlEvent::End(e)) => match local_name(e.name().as_ref()) {
                        "Filter" => st.in_filter = false,
                        "TopicExpression" if st.in_topic_expression => {
                            st.in_topic_expression = false;
                            if let Some(expr) = result.topic_expression.as_mut() {
                                expr.value = text.clone().unwrap_or_default();
                            }
                        }
                        "InitialTerminationTime" if st.in_initial_termination_time => {
                            st.in_initial_termination_time = false;
                            if let Some(itt) = result.initial_termination_time.as_mut() {
                                *itt = text.clone().unwrap_or_default();
                            }
                        }
                        _ => {}
                    },
                    Ok(XmlEvent::Eof) => break,
                    Err(e) => return Err(OnvifError::InvalidXml(format!("XML parse error: {e}"))),
                    _ => {}
                }
            }
        }
        buf.clear();
    }
    Ok(result)
}

/// Parsed wsnt:Subscribe body (basic notification interface).
#[derive(Debug, Default)]
struct SubscribeRequest {
    /// ConsumerReference/Address — the URL Notify messages are POSTed to.
    consumer_address: Option<String>,
    /// TerminationTime (absent → the default window).
    termination_time: Option<String>,
    /// Filter/TopicExpression — the same shape CreatePullPointSubscription
    /// carries, validated by the same `parse_topic_filter`.
    topic_expression: Option<TopicExpressionRequest>,
}

/// Parse a wsnt:Subscribe body: ConsumerReference/Address, Filter/
/// TopicExpression (same shape as the pull-point create), TerminationTime.
/// SubscriptionPolicy content is deliberately ignored when present.
fn parse_subscribe_request(body: &str) -> Result<SubscribeRequest, OnvifError> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut result = SubscribeRequest::default();
    #[derive(Default)]
    struct State {
        in_filter: bool,
        in_topic_expression: bool,
        in_consumer: bool,
        in_address: bool,
        in_termination_time: bool,
    }
    let mut st = State::default();
    let mut text_acc = crate::types::TextAccumulator::new();

    loop {
        let event = reader.read_event_into(&mut buf);
        match event {
            Ok(XmlEvent::Text(e)) => {
                let _ = text_acc.push_text(&e);
            }
            Ok(XmlEvent::GeneralRef(e)) => {
                let _ = text_acc.push_ref(&e);
            }
            other => {
                let text = text_acc.flush();
                match other {
                    Ok(XmlEvent::Start(e)) => match local_name(e.name().as_ref()) {
                        "Filter" => st.in_filter = true,
                        "ConsumerReference" => st.in_consumer = true,
                        "Address" if st.in_consumer => {
                            st.in_address = true;
                            result.consumer_address = Some(String::new());
                        }
                        "TopicExpression" if st.in_filter => {
                            st.in_topic_expression = true;
                            result.topic_expression = Some(TopicExpressionRequest {
                                dialect: attribute_by_local_name(&e, "Dialect"),
                                value: String::new(),
                            });
                        }
                        "TerminationTime" => {
                            st.in_termination_time = true;
                            result.termination_time = Some(String::new());
                        }
                        _ => {}
                    },
                    Ok(XmlEvent::Empty(e)) => match local_name(e.name().as_ref()) {
                        "TopicExpression" if st.in_filter => {
                            result.topic_expression = Some(TopicExpressionRequest {
                                dialect: attribute_by_local_name(&e, "Dialect"),
                                value: String::new(),
                            });
                        }
                        "TerminationTime" => {
                            result.termination_time = Some(String::new());
                        }
                        _ => {}
                    },
                    Ok(XmlEvent::End(e)) => match local_name(e.name().as_ref()) {
                        "Filter" => st.in_filter = false,
                        "ConsumerReference" => st.in_consumer = false,
                        "Address" if st.in_address => {
                            st.in_address = false;
                            if let Some(addr) = result.consumer_address.as_mut() {
                                *addr = text.clone().unwrap_or_default();
                            }
                        }
                        "TopicExpression" if st.in_topic_expression => {
                            st.in_topic_expression = false;
                            if let Some(expr) = result.topic_expression.as_mut() {
                                expr.value = text.clone().unwrap_or_default();
                            }
                        }
                        "TerminationTime" if st.in_termination_time => {
                            st.in_termination_time = false;
                            if let Some(tt) = result.termination_time.as_mut() {
                                *tt = text.clone().unwrap_or_default();
                            }
                        }
                        _ => {}
                    },
                    Ok(XmlEvent::Eof) => break,
                    Err(e) => return Err(OnvifError::InvalidXml(format!("XML parse error: {e}"))),
                    _ => {}
                }
            }
        }
        buf.clear();
    }
    Ok(result)
}

fn parse_pull_request(body: &str) -> Result<PullRequest, OnvifError> {
    let children = direct_child_texts(body)?;
    let field = |name: &str| {
        children
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    let message_limit = match field("MessageLimit") {
        Some(raw) => raw
            .parse::<i64>()
            .map_err(|_| OnvifError::InvalidXml(format!("invalid MessageLimit {raw:?}")))?,
        None => 0,
    };
    Ok(PullRequest {
        timeout: field("Timeout").unwrap_or_default(),
        message_limit,
    })
}

fn parse_renew_request(body: &str) -> Result<RenewRequest, OnvifError> {
    let children = direct_child_texts(body)?;
    let termination_time = children
        .iter()
        .find(|(n, _)| n == "TerminationTime")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    Ok(RenewRequest { termination_time })
}

/// Collect `(local name, text)` for each direct child element of the body's
/// root (the action element) — the shape of the PullMessages / Renew
/// request bodies. Namespace-agnostic, entity-resolving.
fn direct_child_texts(body: &str) -> Result<Vec<(String, String)>, OnvifError> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut depth: usize = 0;
    let mut current: Option<String> = None;
    let mut text_acc = crate::types::TextAccumulator::new();

    loop {
        let event = reader.read_event_into(&mut buf);
        match event {
            Ok(XmlEvent::Text(e)) => {
                if depth == 2 {
                    let _ = text_acc.push_text(&e);
                }
            }
            Ok(XmlEvent::GeneralRef(e)) => {
                if depth == 2 {
                    let _ = text_acc.push_ref(&e);
                }
            }
            other => {
                let text = text_acc.flush().unwrap_or_default();
                match other {
                    Ok(XmlEvent::Start(e)) => {
                        depth += 1;
                        if depth == 2 {
                            current = Some(local_name(e.name().as_ref()).to_string());
                        }
                    }
                    Ok(XmlEvent::Empty(e)) => {
                        if depth == 1 {
                            out.push((local_name(e.name().as_ref()).to_string(), String::new()));
                        }
                    }
                    Ok(XmlEvent::End(_e)) => {
                        depth = depth.saturating_sub(1);
                        if depth == 1 {
                            if let Some(current) = current.take() {
                                // Named by the opening tag (balanced XML).
                                out.push((current, text));
                            }
                        }
                    }
                    Ok(XmlEvent::Eof) => break,
                    Err(e) => return Err(OnvifError::InvalidXml(format!("XML parse error: {e}"))),
                    _ => {}
                }
            }
        }
        buf.clear();
    }
    Ok(out)
}

/// First attribute whose local name matches, decoded as UTF-8 (crate
/// convention — see imaging's `attr_value_f64`).
fn attribute_by_local_name(e: &BytesStart<'_>, want: &str) -> String {
    for attr in e.attributes().flatten() {
        if local_name(attr.key.as_ref()) == want {
            return String::from_utf8_lossy(&attr.value).into_owned();
        }
    }
    String::new()
}

/// Extract the opaque subscription id from a per-subscription request path,
/// mirroring onvif-go's `subscriptionIDFromRequest`: the remainder after
/// [`SUBSCRIPTION_PATH_PREFIX`] must be non-empty and slash-free.
fn subscription_id_from_path(path: &str) -> Result<String, OnvifError> {
    match path.strip_prefix(SUBSCRIPTION_PATH_PREFIX) {
        Some(id) if !id.is_empty() && !id.contains('/') => Ok(id.to_string()),
        _ => Err(OnvifError::SenderFault(format!(
            "Unknown subscription: not a subscription endpoint: {path}"
        ))),
    }
}

/// Mint an opaque subscription token (16 random bytes, hex — parity with
/// onvif-go's `randomSubscriptionID`).
fn random_subscription_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Unix seconds now (the single time base for terminations and stamps).
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// RFC3339 UTC, second resolution (parity with Go's
/// `time.Format(time.RFC3339)` on truncated times).
fn rfc3339(unix_secs: u64) -> String {
    let (y, mo, d, h, mi, s) = crate::device::secs_to_utc(unix_secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

// ---------------------------------------------------------------------------
// Response builders (pure — byte-golden test targets)
// ---------------------------------------------------------------------------

/// `<tev:GetServiceCapabilitiesResponse>` — no time fields, fully
/// deterministic.
fn build_get_service_capabilities() -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

    let mut root = BytesStart::new("tev:GetServiceCapabilitiesResponse");
    root.push_attribute(("xmlns:tev", EVENTS_SERVICE));
    w.write_event(XmlEvent::Start(root)).unwrap_or_default();

    let mut caps = BytesStart::new("tev:Capabilities");
    caps.push_attribute(("WSPullPointSupport", "true"));
    let max_pull_points = DEFAULT_MAX_PULL_POINTS.to_string();
    caps.push_attribute(("MaxPullPoints", max_pull_points.as_str()));
    w.write_event(XmlEvent::Start(caps)).unwrap_or_default();
    // Empty Text event keeps the pair inline — the twin's byte form
    // `<Capabilities ...></Capabilities>` rather than a split close tag.
    w.write_event(XmlEvent::Text(BytesText::new("")))
        .unwrap_or_default();
    w.write_event(XmlEvent::End(BytesEnd::new("tev:Capabilities")))
        .unwrap_or_default();

    w.write_event(XmlEvent::End(BytesEnd::new(
        "tev:GetServiceCapabilitiesResponse",
    )))
    .unwrap_or_default();

    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `<tev:GetEventPropertiesResponse>` — the spec-complete answer: fixed
/// empty topic set, the two mandatory topic-expression dialects (both
/// honored by the pull-point filter), the spec-blessed single empty
/// message-content filter dialect, and the ONVIF namespace/schema
/// locations.
fn build_get_event_properties() -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

    let mut root = BytesStart::new("tev:GetEventPropertiesResponse");
    root.push_attribute(("xmlns:tev", EVENTS_SERVICE));
    root.push_attribute(("xmlns:wsnt", WS_NOTIFICATION));
    root.push_attribute(("xmlns:wstop", WS_TOPICS));
    w.write_event(XmlEvent::Start(root)).unwrap_or_default();

    write_text(
        &mut w,
        "tev:TopicNamespaceLocation",
        TOPIC_NAMESPACE_LOCATION,
    );
    write_text(&mut w, "wsnt:FixedTopicSet", "true");
    write_text(&mut w, "wstop:TopicSet", "");
    write_text(&mut w, "wsnt:TopicExpressionDialect", DIALECT_CONCRETE);
    write_text(&mut w, "wsnt:TopicExpressionDialect", DIALECT_CONCRETE_SET);
    // One empty message-content filter dialect: content filters are not
    // applied, and the spec prescribes exactly this no-filtering form.
    write_text(&mut w, "wsnt:MessageContentFilterDialect", "");
    write_text(
        &mut w,
        "tev:MessageContentSchemaLocation",
        MESSAGE_SCHEMA_LOCATION,
    );

    w.write_event(XmlEvent::End(BytesEnd::new(
        "tev:GetEventPropertiesResponse",
    )))
    .unwrap_or_default();

    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `<tev:CreatePullPointSubscriptionResponse>` — the SubscriptionReference
/// endpoint plus the granted termination window (RFC3339).
fn build_create_subscription_response(address: &str, current: u64, termination: u64) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

    let mut root = BytesStart::new("tev:CreatePullPointSubscriptionResponse");
    root.push_attribute(("xmlns:tev", EVENTS_SERVICE));
    root.push_attribute(("xmlns:wsnt", WS_NOTIFICATION));
    root.push_attribute(("xmlns:wsa", WS_ADDRESSING));
    w.write_event(XmlEvent::Start(root)).unwrap_or_default();

    open_close(&mut w, "tev:SubscriptionReference", |w| {
        write_text(w, "wsa:Address", address);
    });
    write_text(&mut w, "wsnt:CurrentTime", &rfc3339(current));
    write_text(&mut w, "wsnt:TerminationTime", &rfc3339(termination));

    w.write_event(XmlEvent::End(BytesEnd::new(
        "tev:CreatePullPointSubscriptionResponse",
    )))
    .unwrap_or_default();

    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `<tev:PullMessagesResponse>` — times plus the canonical double-layer
/// notification payloads. Note the twin's namespace split: CurrentTime /
/// TerminationTime are `tev` here but `wsnt` in the create/renew responses.
fn build_pull_messages_response(
    current: u64,
    termination: u64,
    drained: &[QueuedNotification],
    endpoint: &ServiceEndpoint,
) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

    let mut root = BytesStart::new("tev:PullMessagesResponse");
    root.push_attribute(("xmlns:tev", EVENTS_SERVICE));
    root.push_attribute(("xmlns:wsnt", WS_NOTIFICATION));
    if !drained.is_empty() {
        root.push_attribute(("xmlns:wsa", WS_ADDRESSING));
        root.push_attribute(("xmlns:tt", SCHEMAS));
    }
    w.write_event(XmlEvent::Start(root)).unwrap_or_default();

    write_text(&mut w, "tev:CurrentTime", &rfc3339(current));
    write_text(&mut w, "tev:TerminationTime", &rfc3339(termination));

    for qn in drained {
        w.write_event(XmlEvent::Start(BytesStart::new("wsnt:NotificationMessage")))
            .unwrap_or_default();
        write_notification_topic(&mut w, qn);
        open_close(&mut w, "wsnt:ProducerReference", |w| {
            write_text(w, "wsa:Address", &endpoint.device_service_address());
        });
        write_notification_message(&mut w, qn);
        w.write_event(XmlEvent::End(BytesEnd::new("wsnt:NotificationMessage")))
            .unwrap_or_default();
    }

    w.write_event(XmlEvent::End(BytesEnd::new("tev:PullMessagesResponse")))
        .unwrap_or_default();

    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `wsnt:Topic` of one notification — the shared payload writer behind
/// both PullMessages responses and push Notify bodies (issue #50: push
/// and pull bytes stay consistent by construction).
fn write_notification_topic(w: &mut Writer<Vec<u8>>, qn: &QueuedNotification) {
    write_text(w, "wsnt:Topic", &qn.topic);
}

/// `wsnt:Message` wrapping the inner `tt:Message` payload — the shared
/// payload writer behind both PullMessages responses and push Notify
/// bodies.
fn write_notification_message(w: &mut Writer<Vec<u8>>, qn: &QueuedNotification) {
    open_close(w, "wsnt:Message", |w| {
        let mut msg = BytesStart::new("tt:Message");
        msg.push_attribute(("PropertyOperation", qn.message.property_operation.as_str()));
        msg.push_attribute(("UtcTime", qn.message.utc_time.as_str()));
        w.write_event(XmlEvent::Start(msg)).unwrap_or_default();

        // Source/Key/Data groups are always present (possibly empty) —
        // the twin's struct-marshal shape.
        write_simple_items(w, "tt:Source", &qn.message.source);
        write_simple_items(w, "tt:Key", &qn.message.key);
        write_simple_items(w, "tt:Data", &qn.message.data);

        w.write_event(XmlEvent::End(BytesEnd::new("tt:Message")))
            .unwrap_or_default();
    });
}

/// One `tt:Source`/`tt:Key`/`tt:Data` SimpleItem group (always emitted,
/// empty when the group has no items — inline `<tt:Key></tt:Key>`, the
/// twin's byte form).
fn write_simple_items(w: &mut Writer<Vec<u8>>, group: &str, items: &[SimpleItem]) {
    w.write_event(XmlEvent::Start(BytesStart::new(group)))
        .unwrap_or_default();
    if items.is_empty() {
        w.write_event(XmlEvent::Text(BytesText::new("")))
            .unwrap_or_default();
    }
    for item in items {
        let mut si = BytesStart::new("tt:SimpleItem");
        si.push_attribute(("Name", item.name.as_str()));
        si.push_attribute(("Value", item.value.as_str()));
        w.write_event(XmlEvent::Empty(si)).unwrap_or_default();
    }
    w.write_event(XmlEvent::End(BytesEnd::new(group)))
        .unwrap_or_default();
}

/// `<wsnt:RenewResponse>` — the extended termination window.
fn build_renew_response(current: u64, termination: u64) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

    let mut root = BytesStart::new("wsnt:RenewResponse");
    root.push_attribute(("xmlns:wsnt", WS_NOTIFICATION));
    w.write_event(XmlEvent::Start(root)).unwrap_or_default();

    write_text(&mut w, "wsnt:CurrentTime", &rfc3339(current));
    write_text(&mut w, "wsnt:TerminationTime", &rfc3339(termination));

    w.write_event(XmlEvent::End(BytesEnd::new("wsnt:RenewResponse")))
        .unwrap_or_default();

    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `<wsnt:UnsubscribeResponse>` — empty acknowledgment.
fn build_unsubscribe_response() -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    let mut root = BytesStart::new("wsnt:UnsubscribeResponse");
    root.push_attribute(("xmlns:wsnt", WS_NOTIFICATION));
    w.write_event(XmlEvent::Empty(root)).unwrap_or_default();
    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `<wsnt:SubscribeResponse>` — the basic-notification ack: the
/// SubscriptionReference (a pull-style address, so Renew/Unsubscribe
/// operate on it through the shared registry) plus the granted termination
/// window. Same shape as the create-pull-point response under the `wsnt`
/// root — Subscribe is a WS-BaseNotification action, not a tev one.
fn build_subscribe_response(address: &str, current: u64, termination: u64) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

    let mut root = BytesStart::new("wsnt:SubscribeResponse");
    root.push_attribute(("xmlns:wsnt", WS_NOTIFICATION));
    root.push_attribute(("xmlns:wsa", WS_ADDRESSING));
    w.write_event(XmlEvent::Start(root)).unwrap_or_default();

    open_close(&mut w, "wsnt:SubscriptionReference", |w| {
        write_text(w, "wsa:Address", address);
    });
    write_text(&mut w, "wsnt:CurrentTime", &rfc3339(current));
    write_text(&mut w, "wsnt:TerminationTime", &rfc3339(termination));

    w.write_event(XmlEvent::End(BytesEnd::new("wsnt:SubscribeResponse")))
        .unwrap_or_default();

    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// `<tev:SetSynchronizationPointResponse>` — empty acknowledgment. A sync
/// point asks the device to re-send current property states to its
/// subscribers; this device's events are host-injected stateless
/// notifications, so there is no device-stored state to replay and the
/// ack is the honest answer. (The action name is shared with the media
/// service — dispatch is route-based, no collision.)
fn build_set_synchronization_point_response() -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    let mut root = BytesStart::new("tev:SetSynchronizationPointResponse");
    root.push_attribute(("xmlns:tev", EVENTS_SERVICE));
    w.write_event(XmlEvent::Empty(root)).unwrap_or_default();
    String::from_utf8(w.into_inner()).unwrap_or_default()
}

/// The complete SOAP document POSTed to a basic-notification consumer:
/// the crate's standard response envelope (same declaration, prefixes,
/// and spacing as every other emitted body) wrapping one wsnt:Notify.
fn build_notify_envelope(subscription_address: &str, qn: &QueuedNotification) -> String {
    serialize_soap_response(&build_notify_fragment(subscription_address, qn))
}

/// The `wsnt:Notify` fragment: one NotificationMessage naming this
/// subscription (`SubscriptionReference` — `ProducerReference` is
/// omitted, WS-BaseNotification makes it optional) and carrying the same
/// Topic + inner-message payload writers the pull-point response uses.
fn build_notify_fragment(subscription_address: &str, qn: &QueuedNotification) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);

    let mut root = BytesStart::new("wsnt:Notify");
    root.push_attribute(("xmlns:wsnt", WS_NOTIFICATION));
    root.push_attribute(("xmlns:wsa", WS_ADDRESSING));
    root.push_attribute(("xmlns:tt", SCHEMAS));
    w.write_event(XmlEvent::Start(root)).unwrap_or_default();

    w.write_event(XmlEvent::Start(BytesStart::new("wsnt:NotificationMessage")))
        .unwrap_or_default();
    open_close(&mut w, "wsnt:SubscriptionReference", |w| {
        write_text(w, "wsa:Address", subscription_address);
    });
    write_notification_topic(&mut w, qn);
    write_notification_message(&mut w, qn);
    w.write_event(XmlEvent::End(BytesEnd::new("wsnt:NotificationMessage")))
        .unwrap_or_default();

    w.write_event(XmlEvent::End(BytesEnd::new("wsnt:Notify")))
        .unwrap_or_default();

    String::from_utf8(w.into_inner()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Small quick-xml helpers (crate writer style)
// ---------------------------------------------------------------------------

fn write_text(w: &mut Writer<Vec<u8>>, name: &str, text: &str) {
    w.write_event(XmlEvent::Start(BytesStart::new(name)))
        .unwrap_or_default();
    w.write_event(XmlEvent::Text(BytesText::from_escaped(
        crate::types::xml_escape(text),
    )))
    .unwrap_or_default();
    w.write_event(XmlEvent::End(BytesEnd::new(name)))
        .unwrap_or_default();
}

/// Open an element, call `f` to write children, close it.
fn open_close<F>(w: &mut Writer<Vec<u8>>, name: &str, f: F)
where
    F: FnOnce(&mut Writer<Vec<u8>>),
{
    w.write_event(XmlEvent::Start(BytesStart::new(name)))
        .unwrap_or_default();
    f(w);
    w.write_event(XmlEvent::End(BytesEnd::new(name)))
        .unwrap_or_default();
}

/// Local name of a (possibly prefixed) XML element name — matches the
/// namespace-agnostic parsing convention used across the crate.
fn local_name(qname: &[u8]) -> &str {
    let qname = std::str::from_utf8(qname).unwrap_or("");
    qname.rsplit(':').next().unwrap_or(qname)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> ServiceEndpoint {
        ServiceEndpoint {
            host: "192.0.2.10".to_string(),
            port: 8080,
        }
    }

    // 2026-09-19T12:00:00Z / 13:00:00Z (deterministic golden anchors).
    const T0: u64 = 1_789_819_200;
    const T1: u64 = 1_789_822_800;

    // ------------------------------------------------------------------
    // Byte goldens — the wire contract (parity with onvif-go events.go)
    // ------------------------------------------------------------------

    #[test]
    fn golden_get_service_capabilities() {
        assert_eq!(
            build_get_service_capabilities(),
            "<tev:GetServiceCapabilitiesResponse xmlns:tev=\"http://www.onvif.org/ver10/events/wsdl\">\n  \
             <tev:Capabilities WSPullPointSupport=\"true\" MaxPullPoints=\"10\"></tev:Capabilities>\n\
             </tev:GetServiceCapabilitiesResponse>"
        );
    }

    #[test]
    fn golden_get_event_properties() {
        assert_eq!(
            build_get_event_properties(),
            "<tev:GetEventPropertiesResponse xmlns:tev=\"http://www.onvif.org/ver10/events/wsdl\" xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\" xmlns:wstop=\"http://docs.oasis-open.org/wsn/t-1\">\n  \
             <tev:TopicNamespaceLocation>http://www.onvif.org/ver10/tev/topicns.xml</tev:TopicNamespaceLocation>\n  \
             <wsnt:FixedTopicSet>true</wsnt:FixedTopicSet>\n  \
             <wstop:TopicSet></wstop:TopicSet>\n  \
             <wsnt:TopicExpressionDialect>http://docs.oasis-open.org/wsn/t-1/TopicExpression/Concrete</wsnt:TopicExpressionDialect>\n  \
             <wsnt:TopicExpressionDialect>http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet</wsnt:TopicExpressionDialect>\n  \
             <wsnt:MessageContentFilterDialect></wsnt:MessageContentFilterDialect>\n  \
             <tev:MessageContentSchemaLocation>http://www.onvif.org/ver10/schema/onvif.xsd</tev:MessageContentSchemaLocation>\n\
             </tev:GetEventPropertiesResponse>"
        );
    }

    #[test]
    fn golden_create_subscription_response() {
        let address =
            "http://192.0.2.10:8080/onvif/events_service/sub/0123456789abcdef0123456789abcdef";
        assert_eq!(
            build_create_subscription_response(address, T0, T1),
            "<tev:CreatePullPointSubscriptionResponse xmlns:tev=\"http://www.onvif.org/ver10/events/wsdl\" xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\" xmlns:wsa=\"http://www.w3.org/2005/08/addressing\">\n  \
             <tev:SubscriptionReference>\n    \
             <wsa:Address>http://192.0.2.10:8080/onvif/events_service/sub/0123456789abcdef0123456789abcdef</wsa:Address>\n  \
             </tev:SubscriptionReference>\n  \
             <wsnt:CurrentTime>2026-09-19T12:00:00Z</wsnt:CurrentTime>\n  \
             <wsnt:TerminationTime>2026-09-19T13:00:00Z</wsnt:TerminationTime>\n\
             </tev:CreatePullPointSubscriptionResponse>"
        );
    }

    #[test]
    fn golden_pull_messages_response_with_simple_item_event() {
        let drained = vec![QueuedNotification {
            topic: "tns1:VideoSource/MotionAlarm".to_string(),
            message: QueuedMessage {
                property_operation: "Changed".to_string(),
                utc_time: "2026-09-19T12:00:00Z".to_string(),
                source: vec![SimpleItem::new("Source", "CSI")],
                key: vec![],
                data: vec![
                    SimpleItem::new("State", "true"),
                    SimpleItem::new("Score", "87"),
                ],
            },
        }];
        assert_eq!(
            build_pull_messages_response(T0, T1, &drained, &endpoint()),
            "<tev:PullMessagesResponse xmlns:tev=\"http://www.onvif.org/ver10/events/wsdl\" xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\" xmlns:wsa=\"http://www.w3.org/2005/08/addressing\" xmlns:tt=\"http://www.onvif.org/ver10/schema\">\n  \
             <tev:CurrentTime>2026-09-19T12:00:00Z</tev:CurrentTime>\n  \
             <tev:TerminationTime>2026-09-19T13:00:00Z</tev:TerminationTime>\n  \
             <wsnt:NotificationMessage>\n    \
             <wsnt:Topic>tns1:VideoSource/MotionAlarm</wsnt:Topic>\n    \
             <wsnt:ProducerReference>\n      \
             <wsa:Address>http://192.0.2.10:8080/onvif/device_service</wsa:Address>\n    \
             </wsnt:ProducerReference>\n    \
             <wsnt:Message>\n      \
             <tt:Message PropertyOperation=\"Changed\" UtcTime=\"2026-09-19T12:00:00Z\">\n        \
             <tt:Source>\n          \
             <tt:SimpleItem Name=\"Source\" Value=\"CSI\"/>\n        \
             </tt:Source>\n        \
             <tt:Key></tt:Key>\n        \
             <tt:Data>\n          \
             <tt:SimpleItem Name=\"State\" Value=\"true\"/>\n          \
             <tt:SimpleItem Name=\"Score\" Value=\"87\"/>\n        \
             </tt:Data>\n      \
             </tt:Message>\n    \
             </wsnt:Message>\n  \
             </wsnt:NotificationMessage>\n\
             </tev:PullMessagesResponse>"
        );
    }

    #[test]
    fn golden_pull_messages_empty() {
        assert_eq!(
            build_pull_messages_response(T0, T1, &[], &endpoint()),
            "<tev:PullMessagesResponse xmlns:tev=\"http://www.onvif.org/ver10/events/wsdl\" xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\">\n  \
             <tev:CurrentTime>2026-09-19T12:00:00Z</tev:CurrentTime>\n  \
             <tev:TerminationTime>2026-09-19T13:00:00Z</tev:TerminationTime>\n\
             </tev:PullMessagesResponse>"
        );
    }

    #[test]
    fn golden_renew_and_unsubscribe() {
        assert_eq!(
            build_renew_response(T0, T1),
            "<wsnt:RenewResponse xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\">\n  \
             <wsnt:CurrentTime>2026-09-19T12:00:00Z</wsnt:CurrentTime>\n  \
             <wsnt:TerminationTime>2026-09-19T13:00:00Z</wsnt:TerminationTime>\n\
             </wsnt:RenewResponse>"
        );
        assert_eq!(
            build_unsubscribe_response(),
            "<wsnt:UnsubscribeResponse xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\"/>"
        );
    }

    #[test]
    fn rfc3339_formatting() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(T0), "2026-09-19T12:00:00Z");
        assert_eq!(rfc3339(951_827_696), "2000-02-29T12:34:56Z"); // leap day
    }

    // ------------------------------------------------------------------
    // ISO 8601 durations (grammar ported from onvif-go, table included)
    // ------------------------------------------------------------------

    #[test]
    fn parse_iso8601_duration_table() {
        let cases: &[(&str, Option<u64>)] = &[
            ("PT30S", Some(30)),
            ("PT5M", Some(300)),
            ("PT1M30S", Some(90)),
            ("PT1H", Some(3600)),
            ("PT1H5M10S", Some(3910)),
            ("P1DT2H", Some(93_600)),
            ("PT0S", Some(0)),
            ("", None),
            ("5M", None),
            ("PTxS", None),
            ("PT", None),
            ("P1D", None), // date-only carries no time component
        ];
        for (input, want) in cases {
            let got = parse_iso8601_duration(input);
            assert_eq!(
                got.map(|d| d.as_secs()),
                *want,
                "parse_iso8601_duration({input:?})"
            );
        }
    }

    // ------------------------------------------------------------------
    // Topic filters
    // ------------------------------------------------------------------

    fn concrete(expr: &str) -> TopicFilter {
        TopicFilter {
            dialect: DIALECT_CONCRETE.to_string(),
            expression: expr.to_string(),
        }
    }

    fn concrete_set(expr: &str) -> TopicFilter {
        TopicFilter {
            dialect: DIALECT_CONCRETE_SET.to_string(),
            expression: expr.to_string(),
        }
    }

    #[test]
    fn concrete_filter_matches_exact_local_path_only() {
        let f = concrete("tns1:VideoSource/MotionAlarm");
        assert!(f.matches("tns1:VideoSource/MotionAlarm"));
        assert!(f.matches("VideoSource/MotionAlarm")); // prefix is a binding
        assert!(f.matches("x:VideoSource/MotionAlarm"));
        assert!(!f.matches("tns1:VideoSource/SignalLoss"));
        assert!(!f.matches("tns1:VideoSource"));
        assert!(!f.matches("tns1:Device/HardwareFailure/StorageFailure"));
    }

    #[test]
    fn concrete_set_filter_matches_alternatives_and_wildcards() {
        let f = concrete_set("tns1:Device/*/*|tns1:VideoSource/*");
        assert!(f.matches("tns1:Device/HardwareFailure/StorageFailure"));
        assert!(f.matches("tns1:VideoSource/MotionAlarm"));
        assert!(!f.matches("tns1:VideoAnalytics/LineDetector/Crossed"));
        assert!(!f.matches("tns1:Device/HardwareFailure")); // depth matters
    }

    #[test]
    fn topic_path_segments_strips_prefixes() {
        assert_eq!(
            topic_path_segments("tns1:VideoSource/MotionAlarm"),
            ["VideoSource", "MotionAlarm"]
        );
        assert_eq!(topic_path_segments(" VideoSource "), ["VideoSource"]);
    }

    #[test]
    fn match_topic_path_requires_equal_length() {
        let f: Vec<String> = vec!["a".into(), "*".into()];
        let yes: Vec<String> = vec!["a".into(), "anything".into()];
        let no: Vec<String> = vec!["a".into()];
        let deep: Vec<String> = vec!["a".into(), "x".into(), "y".into()];
        assert!(match_topic_path(&f, &yes));
        assert!(!match_topic_path(&f, &no));
        assert!(!match_topic_path(&f, &deep));
        assert!(!match_topic_path(&[], &[]));
    }

    #[test]
    fn parse_topic_filter_rules() {
        // Absent filter → no filtering.
        assert!(parse_topic_filter(None).unwrap().is_none());

        // Filter without TopicExpression → no filtering.
        let no_expr = parse_topic_filter(None).unwrap();
        assert!(no_expr.is_none());

        // Empty dialect defaults to Concrete.
        let expr = TopicExpressionRequest {
            dialect: String::new(),
            value: "VideoSource/MotionAlarm".into(),
        };
        let f = parse_topic_filter(Some(&expr)).unwrap().unwrap();
        assert_eq!(f.dialect, DIALECT_CONCRETE);

        // Unsupported dialect → Sender fault, never silently ignored.
        let xpath = TopicExpressionRequest {
            dialect: "http://www.w3.org/TR/1999/REC-xpath-19991116".into(),
            value: "tns1:VideoSource".into(),
        };
        match parse_topic_filter(Some(&xpath)) {
            Err(OnvifError::SenderFault(m)) => {
                assert!(m.contains("Unsupported TopicExpression dialect"), "{m}")
            }
            other => panic!("want SenderFault, got {other:?}"),
        }

        // Empty expression → Sender fault.
        let empty = TopicExpressionRequest {
            dialect: DIALECT_CONCRETE.into(),
            value: "  ".into(),
        };
        match parse_topic_filter(Some(&empty)) {
            Err(OnvifError::SenderFault(m)) => {
                assert!(m.contains("Invalid TopicExpression"), "{m}")
            }
            other => panic!("want SenderFault, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Subscription id from path (parity with subscriptionIDFromRequest)
    // ------------------------------------------------------------------

    #[test]
    fn subscription_id_from_path_rules() {
        assert_eq!(
            subscription_id_from_path("/onvif/events_service/sub/abc123").unwrap(),
            "abc123"
        );
        for bad in [
            "/onvif/events_service/sub/",
            "/onvif/events_service/sub/a/b",
            "/onvif/other",
        ] {
            match subscription_id_from_path(bad) {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("not a subscription endpoint"), "{m}")
                }
                other => panic!("path {bad:?}: want SenderFault, got {other:?}"),
            }
        }
    }

    #[test]
    fn random_subscription_id_is_opaque_hex() {
        let id = random_subscription_id();
        assert_eq!(id.len(), 32, "16 bytes hex-encoded");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(id, random_subscription_id());
    }

    // ------------------------------------------------------------------
    // Request body parsing
    // ------------------------------------------------------------------

    #[test]
    fn parse_create_request_fields() {
        let body = r#"<CreatePullPointSubscription xmlns="http://www.onvif.org/ver10/events/wsdl">
            <InitialTerminationTime>PT5M</InitialTerminationTime>
            <Filter><wsnt:TopicExpression xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">tns1:Device/*</wsnt:TopicExpression></Filter>
        </CreatePullPointSubscription>"#;
        let req = parse_create_request(body).unwrap();
        assert_eq!(req.initial_termination_time.as_deref(), Some("PT5M"));
        let expr = req.topic_expression.expect("topic expression");
        assert_eq!(expr.dialect, DIALECT_CONCRETE_SET);
        assert_eq!(expr.value, "tns1:Device/*");
    }

    #[test]
    fn parse_create_request_defaults() {
        let req = parse_create_request(
            r#"<CreatePullPointSubscription xmlns="http://www.onvif.org/ver10/events/wsdl"/>"#,
        )
        .unwrap();
        assert!(req.initial_termination_time.is_none());
        assert!(req.topic_expression.is_none());
    }

    #[test]
    fn parse_pull_request_fields() {
        let req = parse_pull_request(
            r#"<PullMessages xmlns="http://www.onvif.org/ver10/events/wsdl"><Timeout>PT2S</Timeout><MessageLimit>5</MessageLimit></PullMessages>"#,
        )
        .unwrap();
        assert_eq!(req.timeout, "PT2S");
        assert_eq!(req.message_limit, 5);
    }

    #[test]
    fn parse_pull_request_zero_defaults() {
        let req =
            parse_pull_request(r#"<PullMessages xmlns="http://www.onvif.org/ver10/events/wsdl"/>"#)
                .unwrap();
        assert_eq!(req.timeout, "");
        assert_eq!(req.message_limit, 0);
    }

    #[test]
    fn parse_renew_request_fields() {
        let req = parse_renew_request(
            r#"<Renew xmlns="http://docs.oasis-open.org/wsn/b-2"><TerminationTime>PT10M</TerminationTime></Renew>"#,
        )
        .unwrap();
        assert_eq!(req.termination_time, "PT10M");
    }

    // ------------------------------------------------------------------
    // Registry semantics (handler level, no HTTP)
    // ------------------------------------------------------------------

    fn tokio_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    }

    #[test]
    fn create_pull_point_wire_and_defaults() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();

            // Default termination (~1h) when InitialTerminationTime absent.
            let fragment = svc
                .create_subscription(
                    r#"<CreatePullPointSubscription xmlns="http://www.onvif.org/ver10/events/wsdl"/>"#,
                    &ep,
                )
                .await
                .unwrap();
            assert!(fragment.contains(&format!(
                "http://192.0.2.10:8080{SUBSCRIPTION_PATH_PREFIX}"
            )));
            assert!(fragment.contains("CreatePullPointSubscriptionResponse"));

            // Requested PT5M is honored (5-minute window).
            let before = unix_now();
            let fragment = svc
                .create_subscription(
                    r#"<CreatePullPointSubscription xmlns="http://www.onvif.org/ver10/events/wsdl"><InitialTerminationTime>PT5M</InitialTerminationTime></CreatePullPointSubscription>"#,
                    &ep,
                )
                .await
                .unwrap();
            let term = fragment
                .split("<wsnt:TerminationTime>")
                .nth(1)
                .and_then(|rest| rest.split('<').next())
                .unwrap_or_default()
                .to_string();
            let term_secs = parse_rfc3339_to_unix(&term);
            let remaining = term_secs.saturating_sub(before);
            assert!(
                (240..=360).contains(&remaining),
                "PT5M termination remaining {remaining}s"
            );

            // Unparseable InitialTerminationTime → Sender fault.
            match svc
                .create_subscription(
                    r#"<CreatePullPointSubscription xmlns="http://www.onvif.org/ver10/events/wsdl"><InitialTerminationTime>whenever</InitialTerminationTime></CreatePullPointSubscription>"#,
                    &ep,
                )
                .await
            {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("Invalid InitialTerminationTime"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }
        });
    }

    /// Parse an RFC3339 second-resolution timestamp back to unix seconds
    /// (test helper — the inverse of `rfc3339` for rough time assertions).
    fn parse_rfc3339_to_unix(ts: &str) -> u64 {
        let bytes = ts.as_bytes();
        if bytes.len() != 20 {
            return 0;
        }
        let num = |from: usize, to: usize| -> u64 { ts[from..to].parse::<u64>().unwrap_or(0) };
        let (y, mo, d) = (num(0, 4), num(5, 7), num(8, 10));
        let (h, mi, s) = (num(11, 13), num(14, 16), num(17, 19));
        // days-from-civil (Howard Hinnant), u64-only: RFC3339 years here
        // are always positive, so the negative-era arms drop out.
        let y = if mo <= 2 { y - 1 } else { y };
        let era = y / 400;
        let yoe = y - era * 400;
        let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        days * 86_400 + h * 3600 + mi * 60 + s
    }

    #[test]
    fn pull_messages_drains_published_events_in_order() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();
            let sub_path = create(&svc, &ep, "PT10M").await;

            svc.publish_event(Event {
                topic: "tns1:VideoSource/MotionAlarm".into(),
                source: vec![SimpleItem::new("Source", "CSI")],
                data: vec![SimpleItem::new("State", "true")],
                ..Event::new("tns1:VideoSource/MotionAlarm")
            });
            svc.publish_event(Event::new("tns1:VideoSource/SignalLoss"));

            // MessageLimit=1 → only the first event; the rest stay queued.
            let fragment = pull(&svc, &sub_path, "PT0S", 1).await.unwrap();
            assert_eq!(fragment.matches("<wsnt:NotificationMessage>").count(), 1);
            assert!(fragment.contains("MotionAlarm"));

            // Second pull drains the still-queued event.
            let fragment = pull(&svc, &sub_path, "PT0S", 10).await.unwrap();
            assert_eq!(fragment.matches("<wsnt:NotificationMessage>").count(), 1);
            assert!(fragment.contains("SignalLoss"));

            // Third pull is empty.
            let fragment = pull(&svc, &sub_path, "PT0S", 10).await.unwrap();
            assert_eq!(fragment.matches("<wsnt:NotificationMessage>").count(), 0);
        });
    }

    #[test]
    fn pull_messages_validates_request() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();
            let sub_path = create(&svc, &ep, "PT10M").await;

            // MessageLimit <= 0 → Sender fault.
            match pull(&svc, &sub_path, "PT0S", 0).await {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("Invalid MessageLimit"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }

            // Unparsable Timeout → Sender fault; PT0S is legal.
            match pull(&svc, &sub_path, "soon", 5).await {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("Invalid Timeout"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }
            assert!(pull(&svc, &sub_path, "PT0S", 5).await.is_ok());
        });
    }

    #[test]
    fn unknown_and_expired_subscriptions_fault() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();

            // Never existed.
            match pull(&svc, "/onvif/events_service/sub/deadbeef", "PT0S", 5).await {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("Unknown subscription"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }

            // Expired subscriptions are pruned lazily and fault as unknown.
            let sub_path = create(&svc, &ep, "PT1S").await;
            tokio::time::sleep(Duration::from_millis(1100)).await;
            match pull(&svc, &sub_path, "PT0S", 5).await {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("Unknown subscription"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }
        });
    }

    #[test]
    fn renew_and_unsubscribe_semantics() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();
            let sub_path = create(&svc, &ep, "PT2M").await;

            let fragment = svc
                .renew(
                    &sub_path,
                    r#"<Renew xmlns="http://docs.oasis-open.org/wsn/b-2"><TerminationTime>PT10M</TerminationTime></Renew>"#,
                )
                .await
                .unwrap();
            assert!(fragment.contains("RenewResponse"));

            // Invalid TerminationTime → Sender fault.
            match svc
                .renew(
                    &sub_path,
                    r#"<Renew xmlns="http://docs.oasis-open.org/wsn/b-2"><TerminationTime>later</TerminationTime></Renew>"#,
                )
                .await
            {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("Invalid TerminationTime"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }

            svc.unsubscribe(&sub_path).await.unwrap();
            assert!(matches!(
                pull(&svc, &sub_path, "PT0S", 5).await,
                Err(OnvifError::SenderFault(_))
            ));
        });
    }

    #[test]
    fn publish_fans_out_and_filters_per_subscription() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();

            let plain = create(&svc, &ep, "PT10M").await;
            let filtered =
                create_with_filter(&svc, &ep, DIALECT_CONCRETE, "tns1:VideoSource/MotionAlarm")
                    .await;

            svc.publish_event(Event::new("tns1:VideoSource/MotionAlarm"));
            svc.publish_event(Event::new("tns1:Device/HardwareFailure"));

            let got = pull(&svc, &plain, "PT0S", 10).await.unwrap();
            assert_eq!(got.matches("<wsnt:NotificationMessage>").count(), 2);
            let got = pull(&svc, &filtered, "PT0S", 10).await.unwrap();
            assert_eq!(got.matches("<wsnt:NotificationMessage>").count(), 1);
            assert!(got.contains("MotionAlarm"));
        });
    }

    #[test]
    fn queue_is_lossy_at_the_head_beyond_the_bound() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();
            let sub_path = create(&svc, &ep, "PT10M").await;

            for i in 0..=(MAX_EVENT_QUEUE + 5) {
                svc.publish_event(Event::new(&format!("tns1:Counter/Tick{i}")));
            }

            // The oldest events were dropped; the newest MAX_EVENT_QUEUE stay.
            let fragment = pull(
                &svc,
                &sub_path,
                "PT0S",
                i64::try_from(MAX_EVENT_QUEUE + 10).unwrap_or(200),
            )
            .await
            .unwrap();
            let count = fragment.matches("<wsnt:NotificationMessage>").count();
            assert_eq!(count, MAX_EVENT_QUEUE);
            // Head of the queue = the event after the dropped ones.
            let first_tick = (MAX_EVENT_QUEUE + 5) - MAX_EVENT_QUEUE + 1;
            let head = fragment
                .split("<wsnt:Topic>")
                .nth(1)
                .and_then(|rest| rest.split('<').next())
                .unwrap_or_default();
            assert_eq!(head, format!("tns1:Counter/Tick{first_tick}"));
        });
    }

    #[test]
    fn long_poll_waits_for_the_timeout_then_answers_empty() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();
            let sub_path = create(&svc, &ep, "PT10M").await;

            let started = std::time::Instant::now();
            let fragment = pull(&svc, &sub_path, "PT1S", 5).await.unwrap();
            assert!(started.elapsed() >= Duration::from_millis(900));
            assert_eq!(fragment.matches("<wsnt:NotificationMessage>").count(), 0);
        });
    }

    #[test]
    fn long_poll_wakes_on_publish() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = Arc::new(EventsService::new());
            let ep = endpoint();
            let sub_path = create(&svc, &ep, "PT10M").await;

            let svc2 = Arc::clone(&svc);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                svc2.publish_event(Event::new("tns1:VideoSource/MotionAlarm"));
            });

            let started = std::time::Instant::now();
            let fragment = pull(&svc, &sub_path, "PT10S", 5).await.unwrap();
            assert!(started.elapsed() < Duration::from_secs(5));
            assert!(fragment.contains("MotionAlarm"));
        });
    }

    #[test]
    fn max_pull_points_enforced() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();
            for _ in 0..DEFAULT_MAX_PULL_POINTS {
                create(&svc, &ep, "PT10M").await;
            }
            match svc
                .create_subscription(
                    r#"<CreatePullPointSubscription xmlns="http://www.onvif.org/ver10/events/wsdl"/>"#,
                    &ep,
                )
                .await
            {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("Too many active pull point subscriptions"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }
        });
    }

    #[test]
    fn property_operation_and_utc_time_stamped_at_publish() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();
            let sub_path = create(&svc, &ep, "PT10M").await;

            let before = unix_now();
            svc.publish_event(Event {
                property_operation: "Initialized".into(),
                ..Event::new("tns1:Device/Time")
            });
            svc.publish_event(Event::new("tns1:Device/Time")); // → Changed

            let fragment = pull(&svc, &sub_path, "PT0S", 10).await.unwrap();
            assert!(fragment.contains("PropertyOperation=\"Initialized\""));
            assert!(fragment.contains("PropertyOperation=\"Changed\""));
            assert!(fragment.contains("UtcTime=\""));
            let stamped = fragment
                .split("UtcTime=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_default();
            let stamped_secs = parse_rfc3339_to_unix(stamped);
            assert!(stamped_secs >= before);
        });
    }

    // ------------------------------------------------------------------
    // Basic notification interface — wsnt:Subscribe + push Notify
    // ------------------------------------------------------------------

    #[test]
    fn golden_subscribe_response() {
        let address =
            "http://192.0.2.10:8080/onvif/events_service/sub/0123456789abcdef0123456789abcdef";
        assert_eq!(
            build_subscribe_response(address, T0, T1),
            "<wsnt:SubscribeResponse xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\" xmlns:wsa=\"http://www.w3.org/2005/08/addressing\">\n  \
             <wsnt:SubscriptionReference>\n    \
             <wsa:Address>http://192.0.2.10:8080/onvif/events_service/sub/0123456789abcdef0123456789abcdef</wsa:Address>\n  \
             </wsnt:SubscriptionReference>\n  \
             <wsnt:CurrentTime>2026-09-19T12:00:00Z</wsnt:CurrentTime>\n  \
             <wsnt:TerminationTime>2026-09-19T13:00:00Z</wsnt:TerminationTime>\n\
             </wsnt:SubscribeResponse>"
        );
    }

    #[test]
    fn golden_set_synchronization_point_response() {
        assert_eq!(
            build_set_synchronization_point_response(),
            "<tev:SetSynchronizationPointResponse xmlns:tev=\"http://www.onvif.org/ver10/events/wsdl\"/>"
        );
    }

    #[test]
    fn golden_notify_fragment() {
        let address =
            "http://192.0.2.10:8080/onvif/events_service/sub/0123456789abcdef0123456789abcdef";
        let qn = QueuedNotification {
            topic: "tns1:VideoSource/MotionAlarm".to_string(),
            message: QueuedMessage {
                property_operation: "Changed".to_string(),
                utc_time: "2026-09-19T12:00:00Z".to_string(),
                source: vec![SimpleItem::new("Source", "CSI")],
                key: vec![],
                data: vec![SimpleItem::new("State", "true")],
            },
        };
        assert_eq!(
            build_notify_fragment(address, &qn),
            "<wsnt:Notify xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\" xmlns:wsa=\"http://www.w3.org/2005/08/addressing\" xmlns:tt=\"http://www.onvif.org/ver10/schema\">\n  \
             <wsnt:NotificationMessage>\n    \
             <wsnt:SubscriptionReference>\n      \
             <wsa:Address>http://192.0.2.10:8080/onvif/events_service/sub/0123456789abcdef0123456789abcdef</wsa:Address>\n    \
             </wsnt:SubscriptionReference>\n    \
             <wsnt:Topic>tns1:VideoSource/MotionAlarm</wsnt:Topic>\n    \
             <wsnt:Message>\n      \
             <tt:Message PropertyOperation=\"Changed\" UtcTime=\"2026-09-19T12:00:00Z\">\n        \
             <tt:Source>\n          \
             <tt:SimpleItem Name=\"Source\" Value=\"CSI\"/>\n        \
             </tt:Source>\n        \
             <tt:Key></tt:Key>\n        \
             <tt:Data>\n          \
             <tt:SimpleItem Name=\"State\" Value=\"true\"/>\n        \
             </tt:Data>\n      \
             </tt:Message>\n    \
             </wsnt:Message>\n  \
             </wsnt:NotificationMessage>\n\
             </wsnt:Notify>"
        );
    }

    #[test]
    fn notify_envelope_reuses_the_standard_response_envelope() {
        let address = "http://192.0.2.10:8080/onvif/events_service/sub/abc";
        let qn = QueuedNotification {
            topic: "tns1:VideoSource/MotionAlarm".to_string(),
            message: QueuedMessage {
                property_operation: "Changed".to_string(),
                utc_time: "2026-09-19T12:00:00Z".to_string(),
                source: vec![],
                key: vec![],
                data: vec![],
            },
        };
        let envelope = build_notify_envelope(address, &qn);
        assert!(
            envelope.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"),
            "Notify is a standalone SOAP document: {envelope}"
        );
        assert!(envelope.contains("<soap:Envelope"));
        assert!(
            envelope.contains(&build_notify_fragment(address, &qn)),
            "body must embed the Notify fragment verbatim"
        );
        // Push Notify carries the subscription reference, never a
        // producer reference (WS-BaseNotification makes it optional).
        assert!(!envelope.contains("ProducerReference"));
        assert_eq!(envelope.matches("<wsnt:NotificationMessage>").count(), 1);
    }

    #[test]
    fn parse_subscribe_request_fields() {
        let body = r#"<Subscribe xmlns="http://docs.oasis-open.org/wsn/b-2">
            <ConsumerReference><wsa:Address xmlns:wsa="http://www.w3.org/2005/08/addressing">http://127.0.0.1:9999/notify</wsa:Address></ConsumerReference>
            <Filter><wsnt:TopicExpression xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">tns1:VideoSource/*</wsnt:TopicExpression></Filter>
            <TerminationTime>PT5M</TerminationTime>
            <SubscriptionPolicy><Anything>ignored</Anything></SubscriptionPolicy>
        </Subscribe>"#;
        let req = parse_subscribe_request(body).unwrap();
        assert_eq!(
            req.consumer_address.as_deref(),
            Some("http://127.0.0.1:9999/notify")
        );
        assert_eq!(req.termination_time.as_deref(), Some("PT5M"));
        let expr = req.topic_expression.expect("topic expression");
        assert_eq!(expr.dialect, DIALECT_CONCRETE_SET);
        assert_eq!(expr.value, "tns1:VideoSource/*");
    }

    #[test]
    fn parse_subscribe_request_defaults() {
        let req =
            parse_subscribe_request(r#"<Subscribe xmlns="http://docs.oasis-open.org/wsn/b-2"/>"#)
                .unwrap();
        assert!(req.consumer_address.is_none());
        assert!(req.termination_time.is_none());
        assert!(req.topic_expression.is_none());
    }

    #[test]
    fn parse_http_url_rules() {
        let t = parse_http_url("http://127.0.0.1:9999/notify").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str()),
            ("127.0.0.1", 9999, "/notify")
        );

        let t = parse_http_url("http://consumer.example").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str()),
            ("consumer.example", 80, "/")
        );

        let t = parse_http_url("http://consumer.example/a/b?x=1").unwrap();
        assert_eq!(t.path, "/a/b?x=1");

        // IPv6 literals keep their bracketed display form.
        let t = parse_http_url("http://[::1]:9000/n").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("::1", 9000));
        assert_eq!(t.host_header(), "[::1]");
        assert_eq!(t.to_string(), "http://[::1]:9000/n");

        for bad in [
            "https://consumer.example/notify",
            "ftp://consumer.example",
            "consumer.example",
            "http://",
            "http://host:notaport/",
            "http://:8080/",
        ] {
            match parse_http_url(bad) {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("ConsumerReference"), "{bad}: {m}")
                }
                other => panic!("url {bad:?}: want SenderFault, got {other:?}"),
            }
        }
    }

    #[test]
    fn subscribe_response_and_registry_entry() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();

            let fragment = svc
                .subscribe(
                    r#"<Subscribe xmlns="http://docs.oasis-open.org/wsn/b-2"><ConsumerReference><wsa:Address xmlns:wsa="http://www.w3.org/2005/08/addressing">http://127.0.0.1:59999/notify</wsa:Address></ConsumerReference><TerminationTime>PT5M</TerminationTime></Subscribe>"#,
                    &ep,
                )
                .await
                .unwrap();
            assert!(fragment.contains("SubscribeResponse"));
            let address = fragment
                .split("<wsa:Address>")
                .nth(1)
                .and_then(|rest| rest.split('<').next())
                .unwrap_or_default();
            assert!(address.starts_with("http://192.0.2.10:8080/onvif/events_service/sub/"));

            // The registry entry is a basic-notification (push) subscription.
            let id = address.rsplit('/').next().unwrap_or_default();
            let subs = lock_subs(&svc.subs);
            let pp = subs.get(id).expect("registry entry");
            assert_eq!(
                pp.consumer.as_deref(),
                Some("http://127.0.0.1:59999/notify")
            );
        });
    }

    #[test]
    fn subscribe_validation_faults() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();

            // Missing ConsumerReference/Address.
            match svc
                .subscribe(
                    r#"<Subscribe xmlns="http://docs.oasis-open.org/wsn/b-2"><TerminationTime>PT5M</TerminationTime></Subscribe>"#,
                    &ep,
                )
                .await
            {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("ConsumerReference"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }

            // https consumers are documented-unsupported.
            match svc
                .subscribe(
                    r#"<Subscribe xmlns="http://docs.oasis-open.org/wsn/b-2"><ConsumerReference><wsa:Address>https://consumer.example/n</wsa:Address></ConsumerReference></Subscribe>"#,
                    &ep,
                )
                .await
            {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("http://"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }

            // Unparseable TerminationTime mirrors the pull-point rules.
            match svc
                .subscribe(
                    r#"<Subscribe xmlns="http://docs.oasis-open.org/wsn/b-2"><ConsumerReference><wsa:Address>http://127.0.0.1:59999/notify</wsa:Address></ConsumerReference><TerminationTime>whenever</TerminationTime></Subscribe>"#,
                    &ep,
                )
                .await
            {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("Invalid TerminationTime"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }

            // PT0S is not a positive lifetime.
            match svc
                .subscribe(
                    r#"<Subscribe xmlns="http://docs.oasis-open.org/wsn/b-2"><ConsumerReference><wsa:Address>http://127.0.0.1:59999/notify</wsa:Address></ConsumerReference><TerminationTime>PT0S</TerminationTime></Subscribe>"#,
                    &ep,
                )
                .await
            {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("Invalid TerminationTime"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }
        });
    }

    #[test]
    fn subscribe_termination_window_rules() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();
            let consumer = r#"<ConsumerReference><wsa:Address>http://127.0.0.1:59999/notify</wsa:Address></ConsumerReference>"#;

            // Absent TerminationTime grants the 1 h default.
            let fragment = svc
                .subscribe(
                    &format!(r#"<Subscribe xmlns="http://docs.oasis-open.org/wsn/b-2">{consumer}</Subscribe>"#),
                    &ep,
                )
                .await
                .unwrap();
            let before = unix_now();
            let term = fragment
                .split("<wsnt:TerminationTime>")
                .nth(1)
                .and_then(|rest| rest.split('<').next())
                .unwrap_or_default()
                .to_string();
            let remaining = parse_rfc3339_to_unix(&term).saturating_sub(before);
            assert!(
                (3540..=3600).contains(&remaining),
                "default termination remaining {remaining}s"
            );

            // Requested windows are clamped to MAX_TERMINATION (24 h).
            let fragment = svc
                .subscribe(
                    &format!(r#"<Subscribe xmlns="http://docs.oasis-open.org/wsn/b-2">{consumer}<TerminationTime>PT1000H</TerminationTime></Subscribe>"#),
                    &ep,
                )
                .await
                .unwrap();
            let term = fragment
                .split("<wsnt:TerminationTime>")
                .nth(1)
                .and_then(|rest| rest.split('<').next())
                .unwrap_or_default()
                .to_string();
            let remaining = parse_rfc3339_to_unix(&term).saturating_sub(before);
            assert!(
                remaining <= MAX_TERMINATION.as_secs() + 60,
                "PT1000H must clamp to 24 h, remaining {remaining}s"
            );
        });
    }

    #[test]
    fn basic_subscriptions_share_the_pull_point_cap() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();
            for _ in 0..DEFAULT_MAX_PULL_POINTS {
                create(&svc, &ep, "PT10M").await;
            }
            match svc
                .subscribe(
                    r#"<Subscribe xmlns="http://docs.oasis-open.org/wsn/b-2"><ConsumerReference><wsa:Address>http://127.0.0.1:59999/notify</wsa:Address></ConsumerReference></Subscribe>"#,
                    &ep,
                )
                .await
            {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("Too many active subscriptions"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }
        });
    }

    #[test]
    fn pull_messages_on_basic_subscription_faults() {
        let rt = tokio_rt();
        rt.block_on(async {
            let svc = EventsService::new();
            let ep = endpoint();
            let sub_path = subscribe_path(&svc, &ep, "PT10M").await;

            match pull(&svc, &sub_path, "PT0S", 5).await {
                Err(OnvifError::SenderFault(m)) => {
                    assert!(m.contains("not a pull point"), "{m}")
                }
                other => panic!("want SenderFault, got {other:?}"),
            }

            // Renew and Unsubscribe still work on the basic subscription.
            let fragment = svc
                .renew(
                    &sub_path,
                    r#"<Renew xmlns="http://docs.oasis-open.org/wsn/b-2"><TerminationTime>PT10M</TerminationTime></Renew>"#,
                )
                .await
                .unwrap();
            assert!(fragment.contains("RenewResponse"));
            svc.unsubscribe(&sub_path).await.unwrap();
            assert!(matches!(
                svc.renew(
                    &sub_path,
                    r#"<Renew xmlns="http://docs.oasis-open.org/wsn/b-2"><TerminationTime>PT10M</TerminationTime></Renew>"#,
                )
                .await,
                Err(OnvifError::SenderFault(_))
            ));
        });
    }

    /// Subscribe over the handler seam and return the subscription path.
    async fn subscribe_path(
        svc: &EventsService,
        ep: &ServiceEndpoint,
        termination: &str,
    ) -> String {
        let body = format!(
            r#"<Subscribe xmlns="http://docs.oasis-open.org/wsn/b-2"><ConsumerReference><wsa:Address>http://127.0.0.1:59999/notify</wsa:Address></ConsumerReference><TerminationTime>{termination}</TerminationTime></Subscribe>"#
        );
        let fragment = svc.subscribe(&body, ep).await.unwrap();
        let address = fragment
            .split("<wsa:Address>")
            .nth(1)
            .and_then(|rest| rest.split('<').next())
            .unwrap_or_default();
        format!(
            "{SUBSCRIPTION_PATH_PREFIX}{}",
            address.rsplit('/').next().unwrap_or("")
        )
    }

    // -- shared test helpers ------------------------------------------------

    async fn create(svc: &EventsService, ep: &ServiceEndpoint, termination: &str) -> String {
        let body = format!(
            r#"<CreatePullPointSubscription xmlns="http://www.onvif.org/ver10/events/wsdl"><InitialTerminationTime>{termination}</InitialTerminationTime></CreatePullPointSubscription>"#
        );
        let fragment = svc.create_subscription(&body, ep).await.unwrap();
        let address = fragment
            .split("<wsa:Address>")
            .nth(1)
            .and_then(|rest| rest.split('<').next())
            .unwrap_or_default();
        format!(
            "{SUBSCRIPTION_PATH_PREFIX}{}",
            address.rsplit('/').next().unwrap_or("")
        )
    }

    async fn create_with_filter(
        svc: &EventsService,
        ep: &ServiceEndpoint,
        dialect: &str,
        expression: &str,
    ) -> String {
        let body = format!(
            r#"<CreatePullPointSubscription xmlns="http://www.onvif.org/ver10/events/wsdl"><Filter><wsnt:TopicExpression xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" Dialect="{dialect}">{expression}</wsnt:TopicExpression></Filter><InitialTerminationTime>PT10M</InitialTerminationTime></CreatePullPointSubscription>"#
        );
        let fragment = svc.create_subscription(&body, ep).await.unwrap();
        let address = fragment
            .split("<wsa:Address>")
            .nth(1)
            .and_then(|rest| rest.split('<').next())
            .unwrap_or_default();
        format!(
            "{SUBSCRIPTION_PATH_PREFIX}{}",
            address.rsplit('/').next().unwrap_or("")
        )
    }

    async fn pull(
        svc: &EventsService,
        sub_path: &str,
        timeout: &str,
        limit: i64,
    ) -> Result<String, OnvifError> {
        svc.pull_messages(
            sub_path,
            &format!(
                r#"<PullMessages xmlns="http://www.onvif.org/ver10/events/wsdl"><Timeout>{timeout}</Timeout><MessageLimit>{limit}</MessageLimit></PullMessages>"#
            ),
            &endpoint(),
        )
        .await
    }
}
