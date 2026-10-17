//! The `audio` service: the only owner of the board's audio codec (D-A1).
//!
//! It captures the microphone and fans it out as 8 kHz S16 frames, plays
//! one talker's frames on the speaker and drives the amplifier, over the
//! socket protocol in [`audio_proto`]. `rtsp`, `web` and `onvif` are
//! clients; none of them opens a codec node. It is a service of its own,
//! not part of `camerad`, so an audio failure never touches video (the AV100
//! encoder must not be restarted).
//!
//! - [`backend`]: the codec seam (AUD-06); [`fake`] for tests and
//!   codec-less boards, `ak` (feature `ak-pcm`) for Anyka's `ak_pcm.ko`.
//! - [`resample`]: integer FIR between the codec rate and 8 kHz (D-A5).
//! - [`amp`]: who switches the speaker amplifier (D-A6).
//! - [`runtime`]: capture on demand, broadcast, single talker, amp tail,
//!   error backoff (AUD-01..05, AUD-20).
//! - [`store`]: gains across restarts (`etc/audio.json`, AUD-26).
//! - [`server`]: the socket.

#[cfg(feature = "ak-pcm")]
pub mod ak;
pub mod amp;
pub mod backend;
pub mod fake;
pub mod resample;
pub mod runtime;
pub mod server;
pub mod store;

#[cfg(test)]
pub(crate) mod test_support;

pub use amp::Amp;
pub use backend::Backend;
pub use runtime::{Runtime, Settings};

/// Registers the descriptions of every metric this crate emits.
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge};
    describe_counter!(
        "kamoder_audio_errors_total",
        "Device failures (open, ioctl, read, write; label device: a board device id, or amp)"
    );
    describe_gauge!(
        "kamoder_audio_capture_active",
        "1 while the microphone is being captured"
    );
    describe_gauge!(
        "kamoder_audio_capture_subscribers",
        "Clients subscribed to the microphone"
    );
    describe_counter!(
        "kamoder_audio_capture_frames_total",
        "20 ms frames captured and published"
    );
    describe_counter!(
        "kamoder_audio_capture_dropped_frames_total",
        "Frames a slow subscriber missed (it fell more than 500 ms behind)"
    );
    describe_counter!(
        "kamoder_audio_capture_resyncs_total",
        "Capture timestamps re-anchored to the clock after lost periods"
    );
    describe_gauge!(
        "kamoder_audio_speaker_on",
        "1 while the speaker amplifier is switched on"
    );
    describe_counter!(
        "kamoder_audio_talks_total",
        "Talks started (label client: web, onvif, other)"
    );
    describe_counter!(
        "kamoder_audio_talk_busy_total",
        "Talks refused because another talker held the speaker"
    );
    describe_counter!(
        "kamoder_audio_talk_bad_frames_total",
        "Talk streams closed for an impossible frame header"
    );
    describe_counter!(
        "kamoder_audio_playback_frames_total",
        "Frames received from talkers"
    );
    describe_counter!(
        "kamoder_audio_playback_dropped_frames_total",
        "Talker frames dropped (speaker queue full or device failing)"
    );
    describe_counter!(
        "kamoder_audio_playback_silence_total",
        "Periods padded with silence because the talker was late"
    );
    describe_counter!(
        "kamoder_audio_socket_requests_total",
        "Requests on the audio socket, by op and result"
    );
    describe_counter!(
        "kamoder_audio_socket_refused_total",
        "Audio socket connections refused because all slots were busy"
    );
}
