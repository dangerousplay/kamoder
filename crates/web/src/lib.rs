//! The `web` server as a library, so the router can be driven by tests
//! without a socket. The binary (`main.rs`) adds the CLI, logging, signals
//! and the listener; see its docs for the design constraints.
//!
//! Module map:
//!
//! - [`app`] — builds the router; features plug in their `routes()` here.
//! - [`state`] — configuration, session key, cached users table, throttles.
//! - [`auth`] — `AuthUser` / `AdminUser` / `StreamUser` extractors.
//! - [`session`] — login, `/me`, stream tokens.
//! - [`error`] — the `{error, code}` response and `ApiJson`.
//! - [`services`] — supervisor status/control; [`logs`] — list, tail, SSE.
//! - [`config`] — `camera.conf`; [`system`] — Wi-Fi status, reboot.
//! - [`users`] — web users; [`onvif`] — the ONVIF account.
//! - [`video`] — snapshot proxy and MJPEG; [`ptz`] — pan/tilt via the PTZ socket.
//! - [`lights`] — day/night, white, red/blue and mute via the lights socket.
//! - [`follow`] — follow-movement status and on/off via the follow socket.
//! - [`audio`] — microphone/speaker list and gain, talk and listen
//!   WebSockets via the audio socket.
//! - [`update`] — firmware update status, check/install/upload, rollback
//!   and settings via the update socket.
//! - [`tls`] — the self-signed certificate and the HTTPS listener.
//! - [`rate_limit`], [`stream_token`], [`spa`].

// Linked for its side effect: `os` supplies the `pthread_setname_np`
// shim that the crosstool's uClibc 0.9.33 does not export but Rust's thread
// startup references. Nothing here calls into it, so it has to be spelled out
// or it is dropped before linking.
extern crate os;

#[cfg(test)]
mod api_tests;
pub mod app;
pub mod audio;
pub mod auth;
pub mod cameras;
pub mod config;
pub mod error;
pub mod follow;
pub mod hardware;
pub mod lights;
pub mod logs;
pub mod onvif;
pub mod ptz;
pub mod rate_limit;
pub mod services;
pub mod session;
pub mod setup;
pub mod spa;
pub mod ssh;
pub mod state;
pub mod stats;
pub mod stream_token;
pub mod system;
#[cfg(test)]
mod tests;
pub mod tls;
pub mod update;
pub mod users;
pub mod video;
