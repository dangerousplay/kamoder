//! Library-neutral observability hooks (issue #18): the host bridges
//! events to Prometheus (or any backend) without this crate taking a
//! metrics dependency. All methods default to no-ops — implement only
//! what you need:
//!
//! ```rust,ignore
//! struct PrometheusBridge { /* your registry */ }
//! impl onvif_device_rs::metrics::MetricsHooks for PrometheusBridge {
//!     fn soap_request(&self, action: &str) { /* counter.with(&[action]).inc() */ }
//!     // …
//! }
//! let server = OnvifServer::new(&config)
//!     .with_metrics(std::sync::Arc::new(PrometheusBridge));
//! ```
//!
//! Hooks must be cheap (they fire on every request); never block inside
//! them.

/// Observation hooks fired by the SOAP server and the WS-Discovery
/// responder. Default implementations are no-ops.
pub trait MetricsHooks: Send + Sync + 'static {
    /// A SOAP request passed auth and was dispatched to its handler,
    /// with the action's local name (e.g. "GetStreamUri").
    fn soap_request(&self, _action: &str) {}
    /// A dispatched request completed with a SOAP Fault response
    /// (handler error or contained panic — the fault-rate numerator).
    fn soap_fault(&self, _action: &str) {}
    /// A UsernameToken failed verification (bad credentials or replay).
    fn auth_fail(&self) {}
    /// A source was refused while locked out after repeated auth
    /// failures.
    fn auth_lockout(&self) {}
    /// A WS-Discovery Probe was answered with ProbeMatches.
    fn discovery_probe_answered(&self) {}
}

/// The default no-op hooks used when none are configured.
pub struct NoopMetrics;

impl MetricsHooks for NoopMetrics {}
