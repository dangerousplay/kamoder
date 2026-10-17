//! RTSP 1.0 server for the camera's H.264 stream (PRO-05).
//!
//! Built on the `msf` family (`msf-rtsp`, `msf-rtp`, `msf-sdp`) and
//! `ttpkit-auth`; see `.specs/features/pure-rust-onvif-rtsp/design.md`,
//! "RTSP crate evaluation (T5)", for why those and what is ours:
//!
//! - [`audio`] — the [`audio::AudioSource`] seam and the on-demand
//!   microphone feed shared by every mount (the PCMA track).
//! - [`backchannel`] — the ONVIF audio backchannel: RTP from the client,
//!   G.711 decoded, played on the speaker through the audio service.
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

pub mod audio;
pub mod backchannel;
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
    describe_counter!(
        "kamoder_rtsp_audio_packets_sent_total",
        "PCMA RTP packets (20 ms each) sent, summed over sessions"
    );
    describe_counter!(
        "kamoder_rtsp_audio_packets_dropped_total",
        "PCMA packets skipped because a client fell behind"
    );
    describe_counter!(
        "kamoder_rtsp_backchannel_packets_total",
        "RTP packets received on ONVIF backchannels"
    );
    describe_counter!(
        "kamoder_rtsp_backchannel_packets_dropped_total",
        "Backchannel packets not played: wrong payload type, late, or speaker busy/unavailable"
    );
    describe_counter!(
        "kamoder_rtsp_backchannel_busy_total",
        "Times the speaker was refused to a backchannel because another talker held it"
    );
    describe_gauge!(
        "kamoder_rtsp_backchannel_sessions",
        "Backchannel receivers running (sessions playing trackID=2)"
    );
    describe_gauge!(
        "kamoder_rtsp_audio_up",
        "1 while capture from the audio service works, 0 while it is retried"
    );
}
