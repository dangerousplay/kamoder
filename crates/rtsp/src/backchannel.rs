//! The ONVIF audio backchannel: client talks, the camera's speaker plays
//! (AUD-29, AUD-30).
//!
//! ONVIF Streaming Specification (Ver. 26.06), § 5.3 "Back channel
//! connection":
//!
//! - **Feature tag.** A client that wants to talk sends
//!   `Require: www.onvif.org/ver20/backchannel` ([`FEATURE`]) with DESCRIBE
//!   (§ 5.3.2), and its examples repeat it on SETUP, PLAY and TEARDOWN. A
//!   server that does not support it answers `551 Option not supported`
//!   with `Unsupported: <tag>` (§ 5.3.2.1), and so does this one on a mount
//!   without a speaker, or for any other tag (RFC 2326 § 12.32). SETUP/PLAY
//!   of the backchannel track *without* the header are accepted anyway: the
//!   track URL is only learnt from a DESCRIBE that carried it, and some
//!   clients drop the header after DESCRIBE.
//! - **Direction attributes.** § 5.3.2: "the server shall include the
//!   a=sendonly in each media section representing media being sent from
//!   the client to the server and a=recvonly attributes in each media
//!   section representing media being sent from the server to the client"
//!   — i.e. the *client's* point of view, the opposite of RFC 3264 offer
//!   semantics. The § 5.3.2.2 example marks the backchannel `a=sendonly`
//!   and the camera's video and audio `a=recvonly`; the SDP for a
//!   backchannel DESCRIBE does the same (`server::session_description`).
//!   Clients that did not ask never see the extra media or the attributes.
//! - **Codec.** One media section, `m=audio 0 RTP/AVP 8`, PCMA/8000 — the
//!   audio service's 8 kHz mono needs no resampling. PCMU (payload type 0)
//!   is decoded too, for clients that ignore the offer; anything else is
//!   dropped and counted.
//! - **When.** The client may send only after the PLAY response (§ 5.3.2.2),
//!   so the receiver task starts at PLAY, over the transport chosen at
//!   SETUP (TCP interleaved, default channels 4-5, or UDP).
//!
//! # Speaker ownership (AUD-04)
//!
//! The audio service lets one talker hold the speaker; a second `play` is
//! refused `busy`. The ONVIF spec defines no RTSP error for an occupied
//! audio output, and failing PLAY (453/503) would also stop the client's
//! video and microphone tracks, so:
//!
//! - PLAY always answers 200. The speaker is taken **lazily, on the first
//!   audio packet**, and released after [`IDLE_RELEASE`] without one. An NVR
//!   that sets up the backchannel but stays silent therefore does not lock
//!   out push-to-talk in the web UI; "first talker" means first to talk.
//! - **Busy:** the packets are dropped (`kamoder_rtsp_backchannel_packets_dropped_total`,
//!   `kamoder_rtsp_backchannel_busy_total`, one log line per refusal
//!   streak) and the speaker is asked again every [`RETRY`] while audio
//!   keeps coming, so the client is heard once the other talker stops.
//! - The audio service being down is handled the same way.
//!
//! # Jitter
//!
//! `msf-rtp`'s transceiver already reorders by sequence number (64-packet
//! buffer) and drops duplicates; [`Depacketizer`] additionally drops
//! anything at or before the last packet played. Lost packets are not
//! concealed: the service plays what arrives and inserts silence when the
//! stream stalls. Decoded samples are re-chunked into 20 ms
//! ([`FRAME_SAMPLES`]) frames, the size the service plays.

use crate::session::Sender;
use audio_proto::{g711, Client, ClientError, Frame, PlaySink, Talker, FRAME_SAMPLES};
use futures::StreamExt;
use msf_rtp::RtpPacket;
use msf_rtsp::server::RtpTransport;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::Instant;

/// The ONVIF feature tag for the backchannel (Streaming Spec § 5.3.1).
pub const FEATURE: &str = "www.onvif.org/ver20/backchannel";

/// Static payload type of PCMU (RFC 3551, table 4).
pub const PCMU: u8 = 0;

/// Static payload type of PCMA, the one offered.
pub const PCMA: u8 = crate::audio::PAYLOAD_TYPE;

/// Silence after which the speaker is released for other talkers.
pub const IDLE_RELEASE: Duration = Duration::from_secs(1);

/// Wait before asking for the speaker again after `busy` or a failure.
pub const RETRY: Duration = Duration::from_secs(1);

/// Handshake and frame-write timeout towards the audio service: the
/// receiver reads RTP in the same task, so it must not wait long.
const SERVICE_TIMEOUT: Duration = Duration::from_secs(1);

/// Samples per frame sent to the service: 20 ms.
const CHUNK: usize = FRAME_SAMPLES as usize;

/// The camera's speaker, as the audio service at a socket offers it.
/// Cheap to clone; mounts share one.
#[derive(Clone, Debug)]
pub struct Speaker {
    client: Client,
}

impl Speaker {
    /// The audio service at `socket`.
    pub fn socket(socket: impl Into<PathBuf>) -> Self {
        Self {
            client: Client::new(socket).with_timeout(SERVICE_TIMEOUT),
        }
    }
}

/// Why an RTP packet was not played.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Not PCMA or PCMU.
    PayloadType(u8),
    /// At or before the last packet already taken.
    Late,
}

/// G.711 RTP payloads in, 20 ms S16 frames out.
#[derive(Debug, Default)]
pub struct Depacketizer {
    /// Extended sequence number of the last packet taken.
    last: Option<u64>,
    /// RTP time of the first packet, the zero of the frames' `ts_ms`.
    origin: Option<u32>,
    /// Decoded samples not yet sent, and the RTP time of the first.
    pending: Vec<i16>,
    pending_rtp: u32,
}

impl Depacketizer {
    /// Decodes one packet (`index` is its extended sequence number) and
    /// queues its samples.
    pub fn push(
        &mut self,
        index: u64,
        payload_type: u8,
        timestamp: u32,
        payload: &[u8],
    ) -> Result<(), Rejected> {
        let decode: fn(u8) -> i16 = match payload_type {
            PCMA => g711::alaw_decode,
            PCMU => g711::ulaw_decode,
            other => return Err(Rejected::PayloadType(other)),
        };
        if self.last.is_some_and(|last| index <= last) {
            return Err(Rejected::Late);
        }
        self.last = Some(index);
        self.origin.get_or_insert(timestamp);
        if self.pending.is_empty() {
            self.pending_rtp = timestamp;
        }
        self.pending
            .extend(payload.iter().map(|&code| decode(code)));
        Ok(())
    }

    /// The next whole 20 ms frame, if enough samples are queued.
    pub fn frame(&mut self) -> Option<Frame> {
        (self.pending.len() >= CHUNK).then(|| self.take(CHUNK))
    }

    /// Whatever is left, as a short frame (end of a talkspurt).
    pub fn flush(&mut self) -> Option<Frame> {
        (!self.pending.is_empty()).then(|| self.take(self.pending.len()))
    }

    fn take(&mut self, samples: usize) -> Frame {
        let origin = self.origin.unwrap_or(self.pending_rtp);
        let elapsed = self.pending_rtp.wrapping_sub(origin);
        let frame = Frame {
            ts_ms: u64::from(elapsed) * 1000 / u64::from(crate::audio::CLOCK_RATE),
            samples: self.pending.drain(..samples).collect(),
        };
        self.pending_rtp = self.pending_rtp.wrapping_add(samples as u32);
        frame
    }
}

/// Starts receiving the backchannel on `transport` into `speaker`.
/// `finished` runs if the task ends on its own (transport closed), as for
/// [`crate::session::spawn`]; dropping the [`Sender`] stops it and releases
/// the speaker.
pub fn spawn(
    transport: RtpTransport,
    speaker: Speaker,
    finished: impl FnOnce() + Send + 'static,
) -> Sender {
    Sender::from_task(tokio::spawn(async move {
        if let Err(error) = run(transport, speaker).await {
            tracing::info!(%error, "RTSP backchannel stopped");
        }
        finished();
    }))
}

/// Counts the running receivers in `kamoder_rtsp_backchannel_sessions`.
struct Active;

impl Active {
    fn new() -> Self {
        metrics::gauge!("kamoder_rtsp_backchannel_sessions").increment(1.0);
        Active
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        metrics::gauge!("kamoder_rtsp_backchannel_sessions").decrement(1.0);
    }
}

/// The speaker connection, taken on demand.
struct Talk {
    speaker: Speaker,
    sink: Option<PlaySink>,
    /// No new attempt before this, after a refusal or failure.
    retry_at: Option<Instant>,
    /// Whether the current refusal streak was logged.
    refused: bool,
}

impl Talk {
    /// Plays `frame`, taking the speaker first if needed; false if the
    /// frame was dropped.
    async fn play(&mut self, frame: &Frame) -> bool {
        if self.sink.is_none() && !self.acquire().await {
            return false;
        }
        let Some(sink) = self.sink.as_mut() else {
            return false;
        };
        match sink.send(frame).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "backchannel lost the speaker");
                self.sink = None;
                self.retry_at = Some(Instant::now() + RETRY);
                false
            }
        }
    }

    async fn acquire(&mut self) -> bool {
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return false;
        }
        match self.speaker.client.play(Talker::Onvif).await {
            Ok(sink) => {
                tracing::info!("RTSP backchannel took the speaker");
                self.sink = Some(sink);
                self.retry_at = None;
                self.refused = false;
                true
            }
            Err(error) => {
                if matches!(error, ClientError::Busy { .. }) {
                    metrics::counter!("kamoder_rtsp_backchannel_busy_total").increment(1);
                }
                if !self.refused {
                    tracing::warn!(%error, "RTSP backchannel cannot play; dropping its audio");
                } else {
                    tracing::debug!(%error, "speaker still unavailable to the backchannel");
                }
                self.refused = true;
                self.retry_at = Some(Instant::now() + RETRY);
                false
            }
        }
    }

    /// Hands the speaker back (end of a talkspurt).
    async fn release(&mut self) {
        if let Some(sink) = self.sink.take() {
            tracing::info!("RTSP backchannel released the speaker");
            if let Err(error) = sink.finish().await {
                tracing::debug!(%error, "closing the speaker connection");
            }
        }
    }
}

async fn run(mut transport: RtpTransport, speaker: Speaker) -> Result<(), msf_rtsp::Error> {
    let _active = Active::new();
    let received = metrics::counter!("kamoder_rtsp_backchannel_packets_total");
    let dropped = metrics::counter!("kamoder_rtsp_backchannel_packets_dropped_total");
    let mut talk = Talk {
        speaker,
        sink: None,
        retry_at: None,
        refused: false,
    };
    let mut depacketizer = Depacketizer::default();
    let mut last_packet = Instant::now();
    loop {
        let idle = tokio::time::sleep_until(last_packet + IDLE_RELEASE);
        tokio::select! {
            incoming = transport.next() => {
                let packet = match incoming {
                    Some(Ok(packet)) => packet,
                    Some(Err(error)) => return Err(error),
                    None => return Err(msf_rtsp::Error::from_static_msg("transport closed")),
                };
                received.increment(1);
                let rtp: &RtpPacket = packet.as_ref();
                let payload = rtp.stripped_payload();
                if let Err(reason) = depacketizer.push(packet.index(), rtp.payload_type(), rtp.timestamp(), &payload) {
                    tracing::debug!(?reason, "backchannel packet dropped");
                    dropped.increment(1);
                    continue;
                }
                last_packet = Instant::now();
                let mut played = true;
                while let Some(frame) = depacketizer.frame() {
                    played &= talk.play(&frame).await;
                }
                if !played {
                    dropped.increment(1);
                }
            }
            () = idle, if talk.sink.is_some() || !depacketizer.pending.is_empty() => {
                if let Some(frame) = depacketizer.flush() {
                    talk.play(&frame).await;
                }
                talk.release().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alaw(samples: impl IntoIterator<Item = i16>) -> Vec<u8> {
        samples.into_iter().map(g711::alaw_encode).collect()
    }

    #[test]
    fn pcma_and_pcmu_are_decoded_and_rechunked_to_20ms() {
        let mut d = Depacketizer::default();
        // 100 + 100 samples: one 160-sample frame, 40 left over.
        let first: Vec<i16> = (0..100).map(|n| n * 100).collect();
        d.push(10, PCMA, 8000, &alaw(first.iter().copied()))
            .unwrap();
        assert!(d.frame().is_none());
        let ulaw: Vec<u8> = (0..100).map(|n| g711::ulaw_encode(-n * 50)).collect();
        d.push(11, PCMU, 8100, &ulaw).unwrap();
        let frame = d.frame().unwrap();
        assert_eq!(frame.ts_ms, 0);
        assert_eq!(frame.samples.len(), 160);
        assert_eq!(frame.samples[5], g711::alaw_decode(g711::alaw_encode(500)));
        assert_eq!(frame.samples[100], g711::ulaw_decode(g711::ulaw_encode(0)));
        assert!(d.frame().is_none());
        let rest = d.flush().unwrap();
        assert_eq!(rest.samples.len(), 40);
        assert_eq!(rest.ts_ms, 20, "160 samples after the origin");
        assert!(d.flush().is_none());
    }

    #[test]
    fn late_duplicates_and_other_codecs_are_rejected() {
        let mut d = Depacketizer::default();
        let payload = alaw([0; 160]);
        d.push(5, PCMA, 0, &payload).unwrap();
        assert_eq!(d.frame().unwrap().ts_ms, 0);
        assert_eq!(d.push(5, PCMA, 0, &payload), Err(Rejected::Late));
        assert_eq!(d.push(4, PCMA, 0, &payload), Err(Rejected::Late));
        assert_eq!(d.push(6, 96, 160, &payload), Err(Rejected::PayloadType(96)));
        d.push(7, PCMA, 320, &payload).unwrap();
        assert_eq!(d.frame().unwrap().ts_ms, 40, "the RTP gap carries");
    }
}
