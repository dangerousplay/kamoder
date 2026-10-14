//! RTSP 1.0 server for the camera's H.264 stream (PRO-05).
//!
//! Built on the `msf` family (`msf-rtsp`, `msf-rtp`, `msf-sdp`) and
//! `ttpkit-auth`; see `.specs/features/pure-rust-onvif-rtsp/design.md`,
//! "RTSP crate evaluation (T5)", for why those and what is ours:
//!
//! - [`source`] — the [`source::VideoSource`] seam, and the looped Annex-B
//!   [`source::FileSource`] used until the hardware encoder lands (Phase C).
//! - [`stream`] — one pump task, `broadcast` fan-out, cached SPS/PPS.
//! - [`server`] — the method handlers, session table, transports.
//! - [`session`] — the per-session packetizer and sender.
//! - [`digest`] — RTSP Digest against the ONVIF account.
//! - [`h264`] — Annex-B framing.
//!
//! The library exists so the integration tests can run a server in-process;
//! the `rtsp` binary is a thin CLI over it. No vendor library is linked and
//! nothing needs 64-bit atomics, which ARMv5TE lacks.

pub mod digest;
pub mod h264;
pub mod server;
pub mod session;
pub mod source;
pub mod stream;

use metrics::{describe_counter, describe_gauge};

/// Registers the help text of every metric this crate records.
pub fn describe_metrics() {
    describe_gauge!(
        "kamoder_rtsp_sessions_active",
        "RTSP sessions currently set up or playing"
    );
    describe_counter!(
        "kamoder_rtsp_sessions_total",
        "RTSP sessions created by SETUP"
    );
    describe_counter!(
        "kamoder_rtsp_auth_failures_total",
        "RTSP requests with wrong or stale Digest credentials"
    );
    describe_counter!(
        "kamoder_rtsp_frames_sent_total",
        "H.264 access units sent, summed over sessions"
    );
    describe_counter!(
        "kamoder_rtsp_frames_dropped_total",
        "H.264 access units skipped because a client fell behind"
    );
}
