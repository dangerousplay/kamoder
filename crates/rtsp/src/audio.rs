//! Microphone audio for the PCMA track (AUD-10..13).
//!
//! The `audio` service owns the codec; this side is a client of its socket
//! ([`audio_proto`]). [`AudioSource`] is the seam, like
//! [`VideoSource`](crate::source::VideoSource) for video: [`SocketAudioSource`]
//! reads 20 ms S16 frames from the service, [`FakeAudioSource`] makes a
//! fixed pattern for tests. [`AudioStream`] is the one hub every mount
//! shares — the camera has one microphone, whichever `/vsN` is played.
//!
//! - **On demand (D-A7).** Capture is subscribed only while at least one
//!   session plays the audio track: the first subscriber starts a pump task
//!   with a fresh source, and the pump drops it (closing the capture
//!   connection) once it sees no subscriber left, at most one frame — or one
//!   reconnect backoff — later. The service stops the microphone when its
//!   last subscriber leaves.
//! - **Encode once.** The pump A-law-encodes each frame and splits it into
//!   [`FRAME_SAMPLES`] (20 ms) packets before the `broadcast`, so every
//!   session shares the same `Bytes` and only builds RTP headers.
//! - **Audio down ≠ video down (AUD-13).** A source that cannot reach the
//!   service yields `None`; the pump retries with backoff while sessions
//!   wait. Video runs in other tasks and never waits on audio. Whether the
//!   track is declared at all is decided at start-up from the board profile
//!   (`[audio]`), not from the socket, so a service restart does not change
//!   the SDP under a client.
//!
//! ```text
//!   audio service ──capture──▶ pump ──alaw, 160-sample packets──▶ broadcast
//!                              (only while receivers exist)          │
//!                                             session senders ◀──────┘
//! ```

use audio_proto::{g711, CaptureStream, Client, Frame, FRAME_SAMPLES, RATE};
use bytes::Bytes;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

/// RTP clock of the audio track: G.711 is 8 kHz (RFC 3551 § 4.5.14).
pub const CLOCK_RATE: u32 = RATE;

/// Static RTP payload type of PCMA (RFC 3551, table 4).
pub const PAYLOAD_TYPE: u8 = 8;

/// Samples (and A-law bytes) per RTP packet: 20 ms, RFC 3551's default
/// packetisation time.
pub const PACKET_SAMPLES: usize = FRAME_SAMPLES as usize;

/// Packets held for the slowest session: 320 ms. Audio that late is better
/// skipped than played.
pub const AUDIO_BUFFER: usize = 16;

/// First and longest wait between attempts to reach the audio service.
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// One RTP packet's worth of PCMA.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPacket {
    /// Capture time of the first sample on the 8 kHz clock (`ts_ms × 8`,
    /// plus the packet's offset in its frame); arbitrary zero.
    pub timestamp: u64,
    /// A-law bytes, one per sample.
    pub data: Bytes,
}

/// Live microphone frames, S16 mono at [`RATE`].
///
/// `next_frame` paces itself, like the service does (a frame every 20 ms).
/// `None` means the source lost the audio service, or could not reach it;
/// the caller waits and asks again, and the source reconnects. It never
/// ends for good. Implementations must not block the runtime thread.
pub trait AudioSource: Send + 'static {
    fn next_frame(&mut self) -> impl Future<Output = Option<Frame>> + Send;
}

/// Capture from the audio service's socket ([`audio_proto::DEFAULT_SOCKET`]
/// by default). Connects on the first `next_frame`; dropping it closes the
/// capture connection, which unsubscribes.
pub struct SocketAudioSource {
    client: Client,
    capture: Option<CaptureStream>,
    /// Whether the last attempt failed, so the outage is logged once.
    down: bool,
}

impl SocketAudioSource {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            client: Client::new(socket),
            capture: None,
            down: false,
        }
    }

    fn lost(&mut self, error: &dyn std::fmt::Display) {
        self.capture = None;
        if !self.down {
            tracing::warn!(%error, socket = %self.client.path().display(), "audio capture unavailable; retrying");
            metrics::gauge!("kamoder_rtsp_audio_up").set(0.0);
        } else {
            tracing::debug!(%error, "audio service still unavailable");
        }
        self.down = true;
    }
}

impl AudioSource for SocketAudioSource {
    async fn next_frame(&mut self) -> Option<Frame> {
        if self.capture.is_none() {
            match self.client.capture().await {
                Ok(capture) => {
                    tracing::info!(format = ?capture.format(), "audio capture subscribed");
                    metrics::gauge!("kamoder_rtsp_audio_up").set(1.0);
                    self.capture = Some(capture);
                    self.down = false;
                }
                Err(error) => {
                    self.lost(&error);
                    return None;
                }
            }
        }
        let capture = self.capture.as_mut()?;
        match capture.next_frame().await {
            Ok(Some(frame)) => Some(frame),
            Ok(None) => {
                self.lost(&"the audio service closed the capture stream");
                None
            }
            Err(error) => {
                self.lost(&error);
                None
            }
        }
    }
}

/// A test source: 20 ms frames of a fixed sawtooth ([`FakeAudioSource::sample`]),
/// paced in real (or paused tokio) time, `ts_ms` counting from `start_ms`.
pub struct FakeAudioSource {
    ts_ms: u64,
    ticker: tokio::time::Interval,
}

impl FakeAudioSource {
    pub fn new(start_ms: u64) -> Self {
        let mut ticker = tokio::time::interval(Duration::from_millis(20));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self {
            ts_ms: start_ms,
            ticker,
        }
    }

    /// Sample `n` of every frame: a ramp across the A-law range, so encoded
    /// packets are recognisable byte for byte.
    pub fn sample(n: usize) -> i16 {
        (n as i16 - 80) * 400
    }
}

impl AudioSource for FakeAudioSource {
    async fn next_frame(&mut self) -> Option<Frame> {
        self.ticker.tick().await;
        let frame = Frame {
            ts_ms: self.ts_ms,
            samples: (0..PACKET_SAMPLES).map(Self::sample).collect(),
        };
        self.ts_ms += 20;
        Some(frame)
    }
}

/// Splits `frame` into 20 ms packets and A-law-encodes them.
pub fn packets(frame: &Frame) -> impl Iterator<Item = AudioPacket> + '_ {
    let base = frame.ts_ms.wrapping_mul(u64::from(CLOCK_RATE) / 1000);
    frame
        .samples
        .chunks(PACKET_SAMPLES)
        .enumerate()
        .map(move |(index, chunk)| AudioPacket {
            timestamp: base.wrapping_add((index * PACKET_SAMPLES) as u64),
            data: Bytes::from(g711::alaw_encode_vec(chunk)),
        })
}

type Pump = Box<dyn Fn(Arc<Hub>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The shared microphone feed; cheap to clone.
#[derive(Clone)]
pub struct AudioStream {
    hub: Arc<Hub>,
}

struct Hub {
    packets: broadcast::Sender<AudioPacket>,
    /// Whether a pump task runs. Read and written only under this lock,
    /// together with the receiver count, so a subscriber never misses a
    /// pump that is just deciding to stop.
    running: Mutex<bool>,
    pump: Pump,
}

impl AudioStream {
    /// A feed that opens a source with `open` whenever it goes from no
    /// subscriber to one.
    pub fn new<S, F>(open: F) -> Self
    where
        S: AudioSource,
        F: Fn() -> S + Send + Sync + 'static,
    {
        let (packets, _) = broadcast::channel(AUDIO_BUFFER);
        let open = Arc::new(open);
        let pump: Pump = Box::new(move |hub| {
            let source = open();
            Box::pin(pump(hub, source))
        });
        Self {
            hub: Arc::new(Hub {
                packets,
                running: Mutex::new(false),
                pump,
            }),
        }
    }

    /// The audio service at `socket`.
    pub fn socket(socket: PathBuf) -> Self {
        Self::new(move || SocketAudioSource::new(socket.clone()))
    }

    /// Packets from now on; starts capture if nobody was listening.
    pub fn subscribe(&self) -> broadcast::Receiver<AudioPacket> {
        let mut running = self.hub.lock();
        let receiver = self.hub.packets.subscribe();
        if !*running {
            *running = true;
            tracing::debug!("audio subscribers present; starting capture");
            tokio::spawn((self.hub.pump)(Arc::clone(&self.hub)));
        }
        receiver
    }

    /// Whether a pump (and so a capture subscription) is running.
    pub fn capturing(&self) -> bool {
        *self.hub.lock()
    }
}

impl Hub {
    fn lock(&self) -> std::sync::MutexGuard<'_, bool> {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether anyone still listens; if not, marks the pump stopped, under
    /// the same lock `subscribe` takes.
    fn keep_going(&self) -> bool {
        let mut running = self.lock();
        let listening = self.packets.receiver_count() > 0;
        if !listening {
            *running = false;
        }
        listening
    }
}

async fn pump<S: AudioSource>(hub: Arc<Hub>, mut source: S) {
    let mut backoff = BACKOFF_MIN;
    while hub.keep_going() {
        match source.next_frame().await {
            Some(frame) => {
                backoff = BACKOFF_MIN;
                for packet in packets(&frame) {
                    // Every receiver may have left since the check.
                    let _ = hub.packets.send(packet);
                }
            }
            None => {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
    tracing::debug!("no audio subscribers left; capture stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn frames_split_into_20ms_alaw_packets() {
        let frame = Frame {
            ts_ms: 1000,
            samples: (0..400).map(|n| n as i16 * 50).collect(),
        };
        let packets: Vec<_> = packets(&frame).collect();
        assert_eq!(
            packets.iter().map(|p| p.timestamp).collect::<Vec<_>>(),
            [8000, 8160, 8320]
        );
        assert_eq!(
            packets.iter().map(|p| p.data.len()).collect::<Vec<_>>(),
            [160, 160, 80]
        );
        assert_eq!(packets[1].data[0], g711::alaw_encode(160 * 50));
    }

    /// A source that counts how often it is opened and yields `None` (audio
    /// service down) for its first `misses` frames.
    struct Flaky {
        misses: usize,
        inner: FakeAudioSource,
    }

    impl AudioSource for Flaky {
        async fn next_frame(&mut self) -> Option<Frame> {
            if self.misses > 0 {
                self.misses -= 1;
                return None;
            }
            self.inner.next_frame().await
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn captures_only_while_subscribed_and_retries_when_down() {
        let opened = Arc::new(AtomicUsize::new(0));
        let stream = {
            let opened = Arc::clone(&opened);
            AudioStream::new(move || {
                opened.fetch_add(1, Ordering::SeqCst);
                Flaky {
                    misses: 2,
                    inner: FakeAudioSource::new(500),
                }
            })
        };
        assert!(!stream.capturing());
        assert_eq!(
            opened.load(Ordering::SeqCst),
            0,
            "nothing opened before a subscriber"
        );

        let mut first = stream.subscribe();
        let mut second = stream.subscribe();
        assert_eq!(
            opened.load(Ordering::SeqCst),
            1,
            "one capture for all subscribers"
        );
        // Two misses (0.5 s + 1 s backoff), then frames.
        let packet = first.recv().await.unwrap();
        assert_eq!(packet.timestamp, 500 * 8);
        assert_eq!(
            packet.data[0],
            g711::alaw_encode(FakeAudioSource::sample(0))
        );
        assert_eq!(second.recv().await.unwrap(), packet);
        assert_eq!(first.recv().await.unwrap().timestamp, 520 * 8);

        drop((first, second));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!stream.capturing(), "the pump stops once nobody listens");

        let mut again = stream.subscribe();
        assert_eq!(
            opened.load(Ordering::SeqCst),
            2,
            "a new capture for the next session"
        );
        assert!(again.recv().await.is_ok());
    }
}
