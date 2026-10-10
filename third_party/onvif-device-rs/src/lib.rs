//! ONVIF Device (server) library for Rust.
//!
//! Hand-written SOAP implementation of the ONVIF Device role: Device, Media,
//! Media2 (ver20/media), Imaging, (virtual) PTZ, and Events pull-point
//! services over HTTP, plus a WS-Discovery UDP responder and WS-Security
//! UsernameToken verification (PasswordText and PasswordDigest).
//!
//! Extracted verbatim from the production implementation in
//! `mibee-eye-raspi-rs`, whose response XML is byte-stable against the
//! MiBee NVR (raw SOAP local-name matching) — consumers with the same
//! constraint inherit that property unchanged.
//!
//! ## Host integration seams
//!
//! - [`config::DeviceConfig`] — device identity for GetDeviceInformation
//! - [`imaging::ImagingParams`] — camera parameter source for the Imaging
//!   service (implement over your capture pipeline's parameter manager)
//! - [`events::EventsService`] — the Events pull-point publish seam (inject
//!   property events; fan-out to live subscriptions)
//! - [`ptz_state`] — pure virtual-PTZ state machine used by the PTZ service
//! - Media service handlers take URIs/profiles as plain data at registration
//!
//! ## Byte stability
//!
//! Response element names and namespace prefixes follow the official WSDL
//! (`GetStreamUriResponse → MediaUri → Uri`, lowercase `ri` action URIs).
//! Do not change serialization without re-running consumer interop tests.
//!
//! ## Logging
//!
//! The crate logs through the [`log`](https://docs.rs/log) facade — hosts
//! initialize a logger to see output. Nothing is written to stdout/stderr
//! directly.
//!
//! # Example
//!
//! Minimal server wiring (compile-checked by `cargo test --doc`):
//!
//! ```
//! use onvif_device_rs::server::{OnvifConfig, OnvifServer};
//!
//! # fn main() -> Result<(), onvif_device_rs::types::OnvifError> {
//! let mut config = OnvifConfig::default();
//! config.port = 8080;
//! config.username = "admin".to_string();
//! config.password = "set-a-real-password".to_string(); // empty = start() errs
//!
//! let mut server = OnvifServer::new(&config);
//! // server.register_handler("GetProfiles", Box::new(my_handler));
//! // In a tokio runtime:
//! // let mut handle = server.start().await?;   // binds 0.0.0.0:8080
//! // handle.shutdown().await?;                 // graceful stop
//! # let _ = &mut server;
//! # Ok(())
//! # }
//! ```

pub mod auth;
pub mod config;
pub mod device;
pub mod discovery;
pub mod events;
pub mod imaging;
pub mod media;
pub mod media2;
pub mod metrics;
pub mod namespaces;
pub mod ptz;
pub mod ptz_state;
pub mod server;
pub mod types;

pub use config::DeviceConfig;
pub use device::DeviceHooks;
pub use discovery::DiscoveryServer;
pub use events::{Event, EventsService, SimpleItem};
pub use imaging::ImagingParams;
pub use ptz_state::{Position, PtzState, Velocity};
pub use server::{OnvifConfig, OnvifServer, OnvifServerHandle};

/// Compile-check every ```rust block in README.md (`cargo test --doc`) —
/// the quick-start once referenced APIs that did not exist; this guard
/// keeps the documented wiring compiling forever.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
