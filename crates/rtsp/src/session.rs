//! The per-session sender: frames in, RTP out.
//!
//! Every playing session runs one task with its own `H264Packetizer` (single
//! NAL units, FU-A above the MTU), SSRC, sequence numbers and timestamps, so
//! sessions never share mutable state. It starts at the next IDR, which the
//! stream task has already given SPS/PPS. If the client cannot keep up —
//! TCP backpressure, or a slow scheduler — the broadcast channel overruns,
//! and the sender drops everything up to the next IDR instead of buffering:
//! a gap in the picture beats ever-growing latency and memory.

use crate::h264::Frame;
use futures::{SinkExt, StreamExt};
use msf_rtp::h264::{AccessUnit, H264Packetizer};
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
            // Incoming RTP is not expected; polling also drives RTCP.
            incoming = transport.next() => match incoming {
                Some(Ok(_)) => {}
                // Connection lost, or ICMP port unreachable on UDP.
                Some(Err(error)) => return Err(error),
                None => return Err(msf_rtsp::Error::from_static_msg("transport closed")),
            },
        }
    }
}
