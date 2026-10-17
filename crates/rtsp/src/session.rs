//! The per-session sender: frames in, RTP out.
//!
//! Every playing session runs one task with its own `H264Packetizer` (single
//! NAL units, FU-A above the MTU), SSRC, sequence numbers and timestamps, so
//! sessions never share mutable state. It starts at the next IDR, which the
//! stream task has already given SPS/PPS. If the client cannot keep up —
//! TCP backpressure, or a slow scheduler — the broadcast channel overruns,
//! and the sender drops everything up to the next IDR instead of buffering:
//! a gap in the picture beats ever-growing latency and memory.
//!
//! The audio track (AUD-11, AUD-12) gets its own task, transport and SSRC
//! per session: one PCMA packet per 20 ms [`AudioPacket`], RTP time on the
//! 8 kHz clock from the capture timestamp, so A/V alignment follows the two
//! capture clocks rather than send times. A lagging audio session just
//! skips the packets it missed; the timestamps carry the gap.

use crate::audio::AudioPacket;
use crate::h264::Frame;
use futures::{SinkExt, StreamExt};
use msf_rtp::h264::{AccessUnit, H264Packetizer};
use msf_rtp::pcm::{AudioFrame, PCMPacketizer};
use msf_rtp::Packetizer;
use msf_rtsp::server::RtpTransport;
use tokio::sync::broadcast::{self, error::RecvError};

/// Where a session's RTP numbering starts. Random, per RFC 3550 § 5.1, and
/// announced in `RTP-Info` so the client can map timestamps to play time.
#[derive(Clone, Copy, Debug)]
pub struct Start {
    pub ssrc: u32,
    pub seq: u16,
    pub timestamp: u32,
}

impl Start {
    pub fn random() -> Self {
        Self {
            ssrc: rand::random(),
            seq: rand::random(),
            timestamp: rand::random(),
        }
    }
}

/// The running sender; dropping it stops the task.
pub struct Sender(tokio::task::JoinHandle<()>);

impl Sender {
    /// Wraps a spawned per-track task (the backchannel receiver).
    pub(crate) fn from_task(task: tokio::task::JoinHandle<()>) -> Self {
        Self(task)
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts sending `frames` on `transport`. `finished` runs if the task ends
/// on its own (the client went away or the stream ended), so the session is
/// removed; it does not run when the [`Sender`] is dropped.
pub fn spawn(
    frames: broadcast::Receiver<Frame>,
    transport: RtpTransport,
    start: Start,
    finished: impl FnOnce() + Send + 'static,
) -> Sender {
    Sender(tokio::spawn(async move {
        if let Err(error) = run(frames, transport, start).await {
            tracing::info!(%error, "RTSP sender stopped");
        }
        finished();
    }))
}

/// Starts sending the audio `packets` on `transport`; see [`spawn`].
pub fn spawn_audio(
    packets: broadcast::Receiver<AudioPacket>,
    transport: RtpTransport,
    start: Start,
    finished: impl FnOnce() + Send + 'static,
) -> Sender {
    Sender(tokio::spawn(async move {
        if let Err(error) = run_audio(packets, transport, start).await {
            tracing::info!(%error, "RTSP audio sender stopped");
        }
        finished();
    }))
}

/// Maps capture timestamps (8 kHz samples, arbitrary zero) to RTP time from
/// `start`. Gaps (lost or skipped packets) stay gaps; a capture clock that
/// goes backwards — the audio service restarted — continues right after the
/// last packet instead, so RTP time never runs back.
#[derive(Debug)]
struct AudioClock {
    start: u32,
    /// Capture timestamp, length and RTP time of the last packet.
    last: Option<(u64, u32, u32)>,
}

impl AudioClock {
    fn new(start: u32) -> Self {
        Self { start, last: None }
    }

    fn rtp_time(&mut self, timestamp: u64, samples: usize) -> u32 {
        let rtp = match self.last {
            None => self.start,
            Some((last, _, last_rtp)) if timestamp > last => {
                last_rtp.wrapping_add((timestamp - last) as u32)
            }
            Some((_, length, last_rtp)) => last_rtp.wrapping_add(length),
        };
        self.last = Some((timestamp, samples as u32, rtp));
        rtp
    }
}

async fn run_audio(
    mut packets: broadcast::Receiver<AudioPacket>,
    mut transport: RtpTransport,
    start: Start,
) -> Result<(), msf_rtsp::Error> {
    let sent = metrics::counter!("kamoder_rtsp_audio_packets_sent_total");
    let dropped = metrics::counter!("kamoder_rtsp_audio_packets_dropped_total");
    let mut packetizer = PCMPacketizer::new(crate::audio::PAYLOAD_TYPE, start.ssrc);
    let mut clock = AudioClock::new(start.timestamp);
    let mut first = true;
    loop {
        tokio::select! {
            packet = packets.recv() => match packet {
                Ok(packet) => {
                    let timestamp = clock.rtp_time(packet.timestamp, packet.data.len());
                    let Ok(()) = packetizer.push(AudioFrame::new(timestamp, packet.data));
                    while let Ok(Some(rtp)) = packetizer.take() {
                        let seq = start.seq.wrapping_add(rtp.sequence_number());
                        // msf marks every packet; RFC 3551 § 4.1 marks only
                        // the first of a talkspurt — for us, of the stream.
                        let rtp = rtp.with_sequence_number(seq).with_marker(first);
                        first = false;
                        transport.feed(rtp).await?;
                    }
                    transport.flush().await?;
                    sent.increment(1);
                }
                Err(RecvError::Lagged(missed)) => {
                    tracing::debug!(missed, "RTSP client fell behind on audio; skipping");
                    dropped.increment(missed);
                }
                Err(RecvError::Closed) => return Ok(()),
            },
            incoming = transport.next() => receive(incoming)?,
        }
    }
}

/// Largest sequence-number step [`SequenceGuard`] passes unchanged: half of
/// `msf-rtp`'s 64-packet reordering window.
const MAX_SEQUENCE_STEP: i16 = 32;

/// Keeps incoming RTP sequence numbers within reach of `msf-rtp`'s
/// reordering buffer, which every [`RtpTransport`] runs on what the client
/// sends.
///
/// `msf-rtp` 0.4.2 spins forever (`DefaultRtpTransceiver` retries
/// `ReorderingMultiBuffer::push` while it reports `BufferFull`, and `take`
/// cannot make progress on an empty buffer) when a packet arrives 64 or
/// more sequence numbers ahead of an empty window — one forged packet from
/// any client would hang this single-threaded server. Every incoming RTP
/// channel therefore passes through this guard first: small steps (and
/// reordering, and duplicates, which msf handles) keep their numbers; a
/// jump of [`MAX_SEQUENCE_STEP`] or more either way — a sender restart, a
/// long loss, or an attack — is renumbered to follow the last packet, so
/// msf's window never sees a gap it cannot absorb.
#[derive(Debug, Default)]
pub struct SequenceGuard {
    /// Last sequence number passed on (after renumbering).
    last: Option<u16>,
    /// Added to every incoming sequence number.
    offset: u16,
}

impl SequenceGuard {
    /// The packet to pass on, renumbered if needed. Anything too short to
    /// be RTP goes through untouched (msf drops it as undecodable).
    pub fn admit(&mut self, packet: bytes::Bytes) -> bytes::Bytes {
        if packet.len() < 12 {
            return packet;
        }
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        let mut out = seq.wrapping_add(self.offset);
        if let Some(last) = self.last {
            let step = out.wrapping_sub(last) as i16;
            if (-MAX_SEQUENCE_STEP..=0).contains(&step) {
                // Late or duplicate: msf sorts it out; `last` stays.
                return renumber(packet, out);
            }
            if !(1..MAX_SEQUENCE_STEP).contains(&step) {
                out = last.wrapping_add(1);
                self.offset = out.wrapping_sub(seq);
            }
        }
        self.last = Some(out);
        renumber(packet, out)
    }
}

fn renumber(packet: bytes::Bytes, seq: u16) -> bytes::Bytes {
    if packet[2..4] == seq.to_be_bytes() {
        return packet;
    }
    let mut packet = bytes::BytesMut::from(&packet[..]);
    packet[2..4].copy_from_slice(&seq.to_be_bytes());
    packet.freeze()
}

/// Incoming RTP is not expected; polling the transport also drives RTCP.
/// An error or the end of the transport ends the sender.
fn receive<T>(incoming: Option<Result<T, msf_rtsp::Error>>) -> Result<(), msf_rtsp::Error> {
    match incoming {
        Some(Ok(_)) => Ok(()),
        // Connection lost, or ICMP port unreachable on UDP.
        Some(Err(error)) => Err(error),
        None => Err(msf_rtsp::Error::from_static_msg("transport closed")),
    }
}

async fn run(
    mut frames: broadcast::Receiver<Frame>,
    mut transport: RtpTransport,
    start: Start,
) -> Result<(), msf_rtsp::Error> {
    let sent = metrics::counter!("kamoder_rtsp_frames_sent_total");
    let dropped = metrics::counter!("kamoder_rtsp_frames_dropped_total");
    let mut packetizer = H264Packetizer::new(crate::server::PAYLOAD_TYPE, start.ssrc);
    // The stream timestamp of the first frame sent; RTP time counts from it.
    let mut origin: Option<u64> = None;
    // Until the first IDR, and again after an overrun.
    let mut want_keyframe = true;
    let mut resyncing = false;
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Ok(frame) => {
                    if want_keyframe && !frame.keyframe {
                        if resyncing {
                            dropped.increment(1);
                        }
                        continue;
                    }
                    want_keyframe = false;
                    resyncing = false;
                    let origin = *origin.get_or_insert(frame.pts);
                    let timestamp = start
                        .timestamp
                        .wrapping_add(frame.pts.wrapping_sub(origin) as u32);
                    packetizer
                        .push(AccessUnit::new(timestamp.into(), timestamp.into(), frame.data))
                        .map_err(|error| msf_rtsp::Error::from_msg(format!("packetizer: {error}")))?;
                    while let Some(packet) = packetizer
                        .take()
                        .map_err(|error| msf_rtsp::Error::from_msg(format!("packetizer: {error}")))?
                    {
                        // The packetizer counts from 0; shift to our random start.
                        let seq = start.seq.wrapping_add(packet.sequence_number());
                        transport.feed(packet.with_sequence_number(seq)).await?;
                    }
                    transport.flush().await?;
                    sent.increment(1);
                }
                Err(RecvError::Lagged(missed)) => {
                    tracing::debug!(missed, "RTSP client fell behind; skipping to the next IDR");
                    dropped.increment(missed);
                    want_keyframe = true;
                    resyncing = true;
                }
                Err(RecvError::Closed) => return Ok(()),
            },
            incoming = transport.next() => receive(incoming)?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(seq: u16) -> bytes::Bytes {
        let mut p = vec![0x80, 8];
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&[0; 8]);
        bytes::Bytes::from(p)
    }

    fn seq(p: &bytes::Bytes) -> u16 {
        u16::from_be_bytes([p[2], p[3]])
    }

    #[test]
    fn sequence_guard_keeps_small_steps_and_renumbers_jumps() {
        let mut guard = SequenceGuard::default();
        let mut pass = |s| seq(&guard.admit(packet(s)));
        assert_eq!(pass(65530), 65530);
        assert_eq!(pass(65535), 65535);
        assert_eq!(pass(2), 2, "wrap is a small step");
        assert_eq!(pass(1), 1, "reordered: unchanged");
        assert_eq!(pass(2), 2, "duplicate: unchanged");
        assert_eq!(pass(3), 3);
        // A jump far ahead continues right after the last packet...
        assert_eq!(pass(3000), 4);
        assert_eq!(pass(3001), 5, "...and later packets follow the new offset");
        assert_eq!(pass(2999), 3, "reordering still works after a jump");
        // Far behind is a jump too.
        assert_eq!(pass(100), 6);
        assert_eq!(pass(101), 7);
        let short = bytes::Bytes::from_static(&[1, 2, 3]);
        assert_eq!(guard.admit(short.clone()), short);
    }

    #[test]
    fn audio_rtp_time_follows_capture_time_and_never_runs_back() {
        let mut clock = AudioClock::new(u32::MAX - 100);
        assert_eq!(clock.rtp_time(8000, 160), u32::MAX - 100);
        assert_eq!(clock.rtp_time(8160, 160), 59, "wraps");
        // Two packets lost: the gap stays.
        assert_eq!(clock.rtp_time(8640, 160), 539);
        // The service restarted with a new zero: continue after the last.
        assert_eq!(clock.rtp_time(0, 160), 699);
        assert_eq!(clock.rtp_time(160, 160), 859);
    }
}
