//! The service's engine: capture fan-out, the single talker, the speaker
//! amp and device health.
//!
//! ```text
//!  capture thread ──read period──► decimate ──► 20 ms Frames ──► broadcast (25)
//!     ▲ started on the first subscriber,                         │  │
//!     │ stopped 5 s after the last one (D-A7)        Subscription ┘  └ Subscription
//!
//!  Talk (one at a time, AUD-04) ──samples──► speaker task ──► writer thread
//!                                            amp on/off,        interpolate, write
//!                                            1 s tail (AUD-20)  periods; silence
//!                                                               when starved
//! ```
//!
//! Blocking driver calls (`read`, `write`) only ever run on the two
//! dedicated threads; the tokio side only opens nodes and switches the amp.
//!
//! - **Capture** runs while at least one [`Subscription`] exists. Frames go
//!   out on a `broadcast` channel of [`BROADCAST_CAPACITY`] frames (500 ms);
//!   a subscriber that falls further behind loses the oldest frames (counted
//!   in `kamoder_audio_capture_dropped_frames_total`) and never slows the
//!   others or the capture thread.
//! - **Timestamps** are `CLOCK_MONOTONIC` ms (the clock V4L2 stamps video
//!   frames with, so `rtsp` can line up audio and video, AUD-12), advanced
//!   by sample count for a jitter-free RTP clock and re-anchored when the
//!   count falls more than [`RESYNC_MS`] behind the wall (lost periods).
//! - **Playback**: [`Runtime::talk`] takes the speaker or answers busy. The
//!   speaker task opens the node, turns the amp on, forwards samples to the
//!   writer thread, and turns the amp off [`Settings::speaker_tail`] after
//!   the last frame or the end of the talk, closing the node when nobody
//!   talks. The writer pads with silence whenever the talker is late, so
//!   the driver never starves mid-word and a stalled client blocks nothing.
//! - **Errors** (open, ioctl, read, write) are logged once per distinct
//!   message, counted in `kamoder_audio_errors_total`, shown as
//!   [`DeviceState::Error`] and retried with exponential backoff
//!   ([`Settings::backoff_min`]..[`Settings::backoff_max`]); the process
//!   never exits for them (AUD-05).
//! - **Gains** come from `etc/audio.json`, else the board profile, and are
//!   applied right after each open and whenever [`Runtime::set`] changes one
//!   (AUD-25, AUD-26).

use crate::amp::Amp;
use crate::backend::{self, Backend, Capture, Playback, StreamParams};
use crate::resample::{self, Decimator, Interpolator};
use crate::store::{self, Saved};
use audio_proto::{Device, DeviceKind, DeviceState, ErrorCode, Frame, Talker};
use metrics::{counter, gauge};
use snafu::{ensure, ResultExt, Snafu};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant;

/// Frames the broadcast keeps for slow subscribers: 500 ms.
pub const BROADCAST_CAPACITY: usize = 25;

/// Driver period asked for by default, bytes: 256 samples, 32 ms at 8 kHz.
/// The ak_pcm capture minimum (512 B) and live-tested on the AV100: each
/// read returns one period, so this is the capture latency floor.
pub const DEFAULT_PERIOD_BYTES: u32 = 512;

/// A sample-count timestamp this far behind the wall clock is re-anchored.
pub const RESYNC_MS: u64 = 100;

/// Talker chunks queued for the writer thread (each ≥ 20 ms): beyond this
/// the talker is far ahead of real time and chunks are dropped.
const PLAY_QUEUE: usize = 25;

/// Commands queued for the speaker task.
const SPEAKER_COMMANDS: usize = 64;

/// Timings; [`Settings::default`] is what the camera uses.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Driver period, bytes (even).
    pub period_bytes: u32,
    /// Capture keeps running this long after the last subscriber (D-A7).
    pub capture_idle: Duration,
    /// The amp stays on this long after the last frame (AUD-20).
    pub speaker_tail: Duration,
    /// First retry delay after a device error.
    pub backoff_min: Duration,
    /// Longest retry delay.
    pub backoff_max: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            period_bytes: DEFAULT_PERIOD_BYTES,
            capture_idle: Duration::from_secs(5),
            speaker_tail: Duration::from_secs(1),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
        }
    }
}

/// Configuration the service cannot run with.
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("{source}"))]
    Rate { source: resample::Error },

    #[snafu(display("period of {bytes} bytes: must be even and at least 2"))]
    Period { bytes: u32 },
}

/// A request the runtime refused; maps onto the socket's error reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub code: ErrorCode,
    pub message: String,
}

impl Refused {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// `CLOCK_MONOTONIC` in ms: the clock of video frame timestamps.
pub fn monotonic_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the call;
    // CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

/// What `status` reports about a device, plus the log-once memory.
#[derive(Debug)]
struct Health {
    state: DeviceState,
    error: Option<String>,
}

/// One board device and its live settings.
struct Entry {
    spec: board::AudioDevice,
    kind: DeviceKind,
    gain: Mutex<Option<u32>>,
    /// Bumped on every gain change, so the device threads re-apply it.
    generation: AtomicU32,
    health: Mutex<Health>,
}

impl Entry {
    fn health(&self) -> MutexGuard<'_, Health> {
        lock(&self.health)
    }

    /// Applies the current gain through `apply`; returns the generation
    /// applied.
    fn apply_gain(&self, apply: impl FnOnce(u32) -> backend::Result<()>) -> backend::Result<u32> {
        let generation = self.generation.load(Ordering::Acquire);
        if let Some(gain) = *lock(&self.gain) {
            apply(gain)?;
        }
        Ok(generation)
    }

    /// Records a failure: counted always, logged when the reason changed.
    fn failed(&self, message: &str) {
        counter!("kamoder_audio_errors_total", "device" => self.spec.id.clone()).increment(1);
        let mut health = self.health();
        if health.error.as_deref() != Some(message) {
            tracing::warn!(device = %self.spec.id, node = %self.spec.device.display(), error = message, "audio device failed; retrying with backoff");
        } else {
            tracing::debug!(device = %self.spec.id, error = message, "audio device still failing");
        }
        health.state = DeviceState::Error;
        health.error = Some(message.to_owned());
    }

    /// Records a working state (`Active` or `Idle`).
    fn ok(&self, state: DeviceState) {
        let mut health = self.health();
        if health.error.take().is_some() && state == DeviceState::Active {
            tracing::info!(device = %self.spec.id, "audio device recovered");
        }
        health.state = state;
    }

    /// `Active` → `Idle`; an error stays visible until the next success.
    fn idle(&self) {
        let mut health = self.health();
        if health.state == DeviceState::Active {
            health.state = DeviceState::Idle;
        }
    }
}

struct Inner {
    settings: Settings,
    params: StreamParams,
    backend: Arc<dyn Backend>,
    devices: Vec<Entry>,
    /// Index into `devices` of the microphone captured (the first input).
    input: Option<usize>,
    /// Index into `devices` of the speaker played to (the first output).
    output: Option<usize>,
    frames: broadcast::Sender<Arc<Frame>>,
    subscribers: watch::Sender<usize>,
    talker: Mutex<Option<Talker>>,
    speaker: mpsc::Sender<SpeakerCmd>,
    store: Option<PathBuf>,
    /// What the store file holds; locked across a save.
    saved: Mutex<Saved>,
    shutdown: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// The running service; cheap to clone.
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
}

impl Runtime {
    /// Builds the runtime for `audio` and starts its tasks; must be called
    /// inside a tokio runtime. `store` holds saved gains (`None`: memory
    /// only).
    pub fn start(
        audio: &board::Audio,
        backend: Arc<dyn Backend>,
        amp: Amp,
        settings: Settings,
        store: Option<PathBuf>,
    ) -> Result<Self, Error> {
        resample::factor(audio.rate).context(RateSnafu)?;
        let bytes = settings.period_bytes;
        ensure!(bytes >= 2 && bytes.is_multiple_of(2), PeriodSnafu { bytes });

        let saved = store.as_deref().map(store::load).unwrap_or_default();
        let devices: Vec<Entry> = audio
            .inputs
            .iter()
            .map(|d| (DeviceKind::Input, d))
            .chain(audio.outputs.iter().map(|d| (DeviceKind::Output, d)))
            .map(|(kind, spec)| Entry {
                gain: Mutex::new(initial_gain(spec, &saved)),
                spec: spec.clone(),
                kind,
                generation: AtomicU32::new(0),
                health: Mutex::new(Health {
                    state: DeviceState::Idle,
                    error: None,
                }),
            })
            .collect();
        let input = (!audio.inputs.is_empty()).then_some(0);
        let output = (!audio.outputs.is_empty()).then_some(audio.inputs.len());
        for (kind, list) in [("input", &audio.inputs), ("output", &audio.outputs)] {
            if list.len() > 1 {
                tracing::warn!(
                    kind,
                    used = %list[0].id,
                    "the board lists several audio {kind}s; only the first is driven"
                );
            }
        }

        let (speaker, speaker_rx) = mpsc::channel(SPEAKER_COMMANDS);
        let inner = Arc::new(Inner {
            params: StreamParams {
                rate: audio.rate,
                period_bytes: settings.period_bytes,
            },
            settings,
            backend,
            devices,
            input,
            output,
            frames: broadcast::Sender::new(BROADCAST_CAPACITY),
            subscribers: watch::Sender::new(0),
            talker: Mutex::new(None),
            speaker,
            store,
            saved: Mutex::new(saved),
            shutdown: AtomicBool::new(false),
        });
        gauge!("kamoder_audio_capture_subscribers").set(0.0);
        gauge!("kamoder_audio_capture_active").set(0.0);
        gauge!("kamoder_audio_speaker_on").set(0.0);
        if let Some(input) = input {
            tokio::spawn(capture_task(Arc::clone(&inner), input));
        }
        if let Some(output) = output {
            let speaker = Speaker::new(Arc::clone(&inner), output, amp);
            tokio::spawn(speaker.run(speaker_rx));
        }
        Ok(Self { inner })
    }

    /// Every device, as `status` reports it.
    pub fn status(&self) -> Vec<Device> {
        self.inner
            .devices
            .iter()
            .map(|e| {
                let health = e.health();
                Device {
                    id: e.spec.id.clone(),
                    kind: e.kind,
                    name: e.spec.name.clone(),
                    state: health.state,
                    rate: audio_proto::RATE,
                    gain: *lock(&e.gain),
                    gain_range: e.spec.gain_range,
                    error: health.error.clone(),
                }
            })
            .collect()
    }

    /// Sets and persists a gain or volume; applied live if the device is
    /// open. A failed save changes nothing.
    pub fn set(&self, device: &str, gain: u32) -> Result<(), Refused> {
        let entry = self
            .inner
            .devices
            .iter()
            .find(|e| e.spec.id == device)
            .ok_or_else(|| {
                Refused::new(
                    ErrorCode::UnknownDevice,
                    format!("no audio device {device:?}"),
                )
            })?;
        let [min, max] = entry.spec.gain_range;
        if !(min..=max).contains(&gain) {
            return Err(Refused::new(
                ErrorCode::OutOfRange,
                format!("gain {gain} outside {min}..={max}"),
            ));
        }
        let mut saved = lock(&self.inner.saved);
        if let Some(path) = &self.inner.store {
            let mut next = saved.clone();
            next.gains.insert(device.to_owned(), gain);
            if let Err(error) = store::save(path, &next) {
                tracing::warn!(path = %path.display(), %error, "cannot save the audio state");
                return Err(Refused::new(
                    ErrorCode::Unavailable,
                    format!("cannot save {}: {error}", path.display()),
                ));
            }
            *saved = next;
        }
        *lock(&entry.gain) = Some(gain);
        entry.generation.fetch_add(1, Ordering::Release);
        tracing::info!(device, gain, "audio gain set");
        Ok(())
    }

    /// Subscribes to the microphone; capture starts if it is not running.
    pub fn subscribe(&self) -> Result<Subscription, Refused> {
        if self.inner.input.is_none() {
            return Err(Refused::new(
                ErrorCode::Unavailable,
                "the board has no microphone",
            ));
        }
        // Receiver first, so the frames of a capture this starts are seen.
        let rx = self.inner.frames.subscribe();
        self.inner.subscribers.send_modify(|n| *n += 1);
        gauge!("kamoder_audio_capture_subscribers").increment(1.0);
        Ok(Subscription {
            rx,
            inner: Arc::clone(&self.inner),
        })
    }

    /// Takes the speaker for `talker`: [`ErrorCode::Busy`] while another
    /// talk holds it, [`ErrorCode::Unavailable`] when the node cannot be
    /// opened. The amp is on when this returns.
    pub async fn talk(&self, talker: Talker) -> Result<Talk, Refused> {
        if self.inner.output.is_none() {
            return Err(Refused::new(
                ErrorCode::Unavailable,
                "the board has no speaker",
            ));
        }
        {
            let mut held = lock(&self.inner.talker);
            if let Some(holder) = *held {
                counter!("kamoder_audio_talk_busy_total").increment(1);
                return Err(Refused::new(
                    ErrorCode::Busy,
                    format!("speaker held by {holder}"),
                ));
            }
            *held = Some(talker);
        }
        // From here on, dropping `talk` releases the speaker.
        let talk = Talk {
            inner: Arc::clone(&self.inner),
            talker,
        };
        let (reply, answer) = oneshot::channel();
        let stopping = || Refused::new(ErrorCode::Unavailable, "the audio service is stopping");
        self.inner
            .speaker
            .send(SpeakerCmd::Start(reply))
            .await
            .map_err(|_| stopping())?;
        match answer.await {
            Ok(Ok(())) => {
                tracing::info!(%talker, "talk started");
                counter!("kamoder_audio_talks_total", "client" => talker.as_str()).increment(1);
                Ok(talk)
            }
            Ok(Err(message)) => Err(Refused::new(ErrorCode::Unavailable, message)),
            Err(_) => Err(stopping()),
        }
    }

    /// Turns the amp off and stops both devices.
    pub async fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Release);
        let (reply, done) = oneshot::channel();
        if self
            .inner
            .speaker
            .send(SpeakerCmd::Shutdown(reply))
            .await
            .is_ok()
        {
            let _ = tokio::time::timeout(Duration::from_secs(2), done).await;
        }
    }
}

/// Saved gain if valid for `spec`, else the profile's.
fn initial_gain(spec: &board::AudioDevice, saved: &Saved) -> Option<u32> {
    let [min, max] = spec.gain_range;
    match saved.gains.get(&spec.id) {
        Some(&gain) if (min..=max).contains(&gain) => Some(gain),
        Some(&gain) => {
            tracing::warn!(device = %spec.id, gain, min, max, "saved gain outside the device's range; using the board's");
            spec.gain
        }
        None => spec.gain,
    }
}

/// A microphone subscriber; dropping it unsubscribes.
pub struct Subscription {
    rx: broadcast::Receiver<Arc<Frame>>,
    inner: Arc<Inner>,
}

impl Subscription {
    /// The next frame; `None` once the service stops. Frames this
    /// subscriber was too slow for are skipped and counted.
    pub async fn recv(&mut self) -> Option<Arc<Frame>> {
        loop {
            match self.rx.recv().await {
                Ok(frame) => return Some(frame),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    counter!("kamoder_audio_capture_dropped_frames_total").increment(n);
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.inner
            .subscribers
            .send_modify(|n| *n = n.saturating_sub(1));
        gauge!("kamoder_audio_capture_subscribers").decrement(1.0);
    }
}

/// Holds the speaker; dropping it ends the talk (the amp follows after the
/// tail).
pub struct Talk {
    inner: Arc<Inner>,
    talker: Talker,
}

impl Talk {
    /// Queues 8 kHz mono samples; dropped (and counted) when the speaker is
    /// far behind.
    pub fn play(&self, samples: Vec<i16>) {
        counter!("kamoder_audio_playback_frames_total").increment(1);
        if self
            .inner
            .speaker
            .try_send(SpeakerCmd::Samples(samples))
            .is_err()
        {
            counter!("kamoder_audio_playback_dropped_frames_total").increment(1);
        }
    }

    /// Who talks.
    pub fn talker(&self) -> Talker {
        self.talker
    }
}

impl Drop for Talk {
    fn drop(&mut self) {
        *lock(&self.inner.talker) = None;
        tracing::info!(talker = %self.talker, "talk ended");
        if let Err(mpsc::error::TrySendError::Full(cmd)) =
            self.inner.speaker.try_send(SpeakerCmd::Stop)
        {
            // Must not be lost, or the amp would wait for the next talk.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let speaker = self.inner.speaker.clone();
                handle.spawn(async move {
                    let _ = speaker.send(cmd).await;
                });
            }
        }
    }
}

// ---------------------------------------------------------------- capture

/// Runs capture whenever there are subscribers; see the module docs.
async fn capture_task(inner: Arc<Inner>, input: usize) {
    let mut subscribers = inner.subscribers.subscribe();
    let mut backoff = inner.settings.backoff_min;
    loop {
        if subscribers.wait_for(|&n| n > 0).await.is_err() {
            return;
        }
        if inner.shutdown.load(Ordering::Acquire) {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let opened = Arc::new(AtomicBool::new(false));
        let (done_tx, mut done) = oneshot::channel();
        let spawned = {
            let (inner, stop, opened) =
                (Arc::clone(&inner), Arc::clone(&stop), Arc::clone(&opened));
            std::thread::Builder::new()
                .name("audio-capture".into())
                .spawn(move || {
                    let result = capture_thread(&inner, input, &stop, &opened);
                    let _ = done_tx.send(result);
                })
        };
        if let Err(error) = spawned {
            tracing::error!(%error, "cannot start the capture thread");
            tokio::time::sleep(inner.settings.backoff_max).await;
            continue;
        }

        let result = supervise_capture(&inner, &mut subscribers, &stop, &mut done).await;
        gauge!("kamoder_audio_capture_active").set(0.0);
        let entry = &inner.devices[input];
        match result {
            Ok(()) => {
                entry.idle();
                backoff = inner.settings.backoff_min;
                tracing::info!(device = %entry.spec.id, "capture stopped");
            }
            Err(message) => {
                entry.failed(&message);
                if opened.load(Ordering::Acquire) {
                    // It worked for a while: a fresh failure, not a streak.
                    backoff = inner.settings.backoff_min;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(inner.settings.backoff_max);
            }
        }
    }
}

/// Waits for the capture thread to fail, or stops it after the idle time.
async fn supervise_capture(
    inner: &Inner,
    subscribers: &mut watch::Receiver<usize>,
    stop: &AtomicBool,
    done: &mut oneshot::Receiver<backend::Result<()>>,
) -> Result<(), String> {
    let finished = |r: Result<backend::Result<()>, oneshot::error::RecvError>| match r {
        Ok(result) => result.map_err(|e| e.message),
        Err(_) => Err("the capture thread panicked".to_owned()),
    };
    loop {
        tokio::select! {
            r = &mut *done => return finished(r),
            _ = until(subscribers, |n| n == 0) => {}
        }
        // Nobody listens: keep the device warm for a while (D-A7).
        tokio::select! {
            r = &mut *done => return finished(r),
            _ = tokio::time::sleep(inner.settings.capture_idle) => {
                stop.store(true, Ordering::Release);
                return finished((&mut *done).await);
            }
            _ = until(subscribers, |n| n > 0) => {}
        }
    }
}

/// Waits until the subscriber count satisfies `f` (without holding the
/// watch's read guard across an await, which would make the task `!Send`).
async fn until(subscribers: &mut watch::Receiver<usize>, f: impl Fn(usize) -> bool) {
    let _ = subscribers.wait_for(|&n| f(n)).await.map(drop);
}

/// The capture thread: open, then read → decimate → frames until `stop`.
fn capture_thread(
    inner: &Inner,
    input: usize,
    stop: &AtomicBool,
    opened: &AtomicBool,
) -> backend::Result<()> {
    let entry = &inner.devices[input];
    let mut dev: Box<dyn Capture> = inner
        .backend
        .open_capture(&entry.spec.device, inner.params)?;
    let mut generation = entry.apply_gain(|g| dev.set_gain(g))?;
    opened.store(true, Ordering::Release);
    entry.ok(DeviceState::Active);
    gauge!("kamoder_audio_capture_active").set(1.0);
    tracing::info!(device = %entry.spec.id, rate = inner.params.rate, period_bytes = inner.params.period_bytes, "capture started");

    let mut decimator =
        Decimator::new(inner.params.rate).map_err(|e| backend::Error::new(e.to_string()))?;
    let frame_samples = audio_proto::FRAME_SAMPLES as usize;
    let frame_ms = u64::from(audio_proto::FRAME_SAMPLES) * 1000 / u64::from(audio_proto::RATE);
    let mut period = Vec::with_capacity(inner.params.period_samples());
    let mut pending: Vec<i16> = Vec::with_capacity(inner.params.period_samples() + frame_samples);
    let mut next_ts: Option<u64> = None;
    while !stop.load(Ordering::Acquire) && !inner.shutdown.load(Ordering::Acquire) {
        if entry.generation.load(Ordering::Acquire) != generation {
            generation = entry.apply_gain(|g| dev.set_gain(g))?;
        }
        period.clear();
        dev.read(&mut period)?;
        let now = monotonic_ms();
        decimator.process(&period, &mut pending);
        // When pending[0] was captured, by the wall clock.
        let wall = now.saturating_sub(pending.len() as u64 * 1000 / u64::from(audio_proto::RATE));
        let mut ts = match next_ts {
            Some(ts) if wall <= ts + RESYNC_MS => ts,
            Some(ts) => {
                tracing::debug!(behind_ms = wall - ts, "capture timestamps re-anchored");
                counter!("kamoder_audio_capture_resyncs_total").increment(1);
                wall
            }
            None => wall,
        };
        let mut chunks = pending.chunks_exact(frame_samples);
        let mut sent = 0;
        for chunk in &mut chunks {
            let frame = Frame {
                ts_ms: ts,
                samples: chunk.to_vec(),
            };
            ts += frame_ms;
            sent += 1;
            // Err only means nobody is subscribed right now.
            let _ = inner.frames.send(Arc::new(frame));
        }
        counter!("kamoder_audio_capture_frames_total").increment(sent);
        pending.drain(..sent as usize * frame_samples);
        next_ts = Some(ts);
    }
    Ok(())
}

// ---------------------------------------------------------------- speaker

enum SpeakerCmd {
    /// A talk begins: open the node and turn the amp on.
    Start(oneshot::Sender<Result<(), String>>),
    /// 8 kHz samples to play.
    Samples(Vec<i16>),
    /// The talk ended: amp off and close after the tail.
    Stop,
    /// The service stops: amp off and close now.
    Shutdown(oneshot::Sender<()>),
}

struct Writer {
    tx: std_mpsc::SyncSender<Vec<i16>>,
    thread: JoinHandle<()>,
}

/// The speaker task's state.
struct Speaker {
    inner: Arc<Inner>,
    output: usize,
    amp: Amp,
    writer: Option<Writer>,
    /// A writer told to stop, joined before the node is opened again.
    closing: Option<JoinHandle<()>>,
    amp_on: bool,
    talking: bool,
    /// When the amp goes off (and, without a talk, the node closes).
    deadline: Option<Instant>,
    /// No playback open before this, after an error.
    retry_at: Option<Instant>,
    backoff: Duration,
}

impl Speaker {
    fn new(inner: Arc<Inner>, output: usize, amp: Amp) -> Self {
        let backoff = inner.settings.backoff_min;
        Self {
            inner,
            output,
            amp,
            writer: None,
            closing: None,
            amp_on: false,
            talking: false,
            deadline: None,
            retry_at: None,
            backoff,
        }
    }

    fn entry(&self) -> &Entry {
        &self.inner.devices[self.output]
    }

    async fn run(mut self, mut commands: mpsc::Receiver<SpeakerCmd>) {
        tracing::debug!(amp = self.amp.name(), "speaker ready");
        loop {
            let deadline = self.deadline;
            tokio::select! {
                command = commands.recv() => match command {
                    None => {
                        self.close().await;
                        return;
                    }
                    Some(SpeakerCmd::Start(reply)) => {
                        let result = self.open();
                        if result.is_ok() {
                            self.talking = true;
                            self.deadline = None;
                            self.switch_amp(true).await;
                        }
                        let _ = reply.send(result);
                    }
                    Some(SpeakerCmd::Samples(samples)) => self.samples(samples).await,
                    Some(SpeakerCmd::Stop) => {
                        self.talking = false;
                        self.deadline = Some(Instant::now() + self.inner.settings.speaker_tail);
                    }
                    Some(SpeakerCmd::Shutdown(reply)) => {
                        self.close().await;
                        let _ = reply.send(());
                        return;
                    }
                },
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    self.deadline = None;
                    if self.talking {
                        // A stalled talker: silence the amp, keep the node.
                        self.switch_amp(false).await;
                    } else {
                        self.close().await;
                    }
                }
            }
        }
    }

    /// Drops a writer whose thread ended (a write failed; it logged and
    /// counted that) and schedules the retry. `failed`: the caller saw it
    /// disconnect.
    fn reap(&mut self, failed: bool) {
        let finished = self
            .writer
            .as_ref()
            .is_some_and(|w| failed || w.thread.is_finished());
        if let Some(writer) = finished.then(|| self.writer.take()).flatten() {
            let _ = writer.thread.join();
            self.retry_at = Some(Instant::now() + self.backoff);
            self.backoff = (self.backoff * 2).min(self.inner.settings.backoff_max);
        }
    }

    /// Opens the node and starts the writer, unless already open or still
    /// backing off after an error.
    fn open(&mut self) -> Result<(), String> {
        self.reap(false);
        if self.writer.is_some() {
            return Ok(());
        }
        if let Some(closing) = self.closing.take() {
            // Its sender is gone, so it is at most one period write away
            // from releasing the node.
            let _ = closing.join();
        }
        if let Some(at) = self.retry_at {
            let now = Instant::now();
            if now < at {
                let error = self.entry().health().error.clone();
                return Err(format!(
                    "{} (retrying in {} ms)",
                    error.as_deref().unwrap_or("the speaker failed"),
                    (at - now).as_millis()
                ));
            }
        }
        let entry = &self.inner.devices[self.output];
        let opened = self
            .inner
            .backend
            .open_playback(&entry.spec.device, self.inner.params)
            .and_then(|mut dev| {
                let generation = entry.apply_gain(|g| dev.set_gain(g))?;
                Ok((dev, generation))
            });
        let (dev, generation) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                entry.failed(&error.message);
                self.retry_at = Some(Instant::now() + self.backoff);
                self.backoff = (self.backoff * 2).min(self.inner.settings.backoff_max);
                return Err(error.message);
            }
        };
        let (tx, rx) = std_mpsc::sync_channel(PLAY_QUEUE);
        let inner = Arc::clone(&self.inner);
        let output = self.output;
        let thread = std::thread::Builder::new()
            .name("audio-playback".into())
            .spawn(move || {
                let entry = &inner.devices[output];
                if let Err(error) = writer_thread(&inner, entry, dev, generation, &rx) {
                    entry.failed(&error.message);
                }
            })
            .map_err(|e| format!("cannot start the playback thread: {e}"))?;
        self.writer = Some(Writer { tx, thread });
        self.retry_at = None;
        self.backoff = self.inner.settings.backoff_min;
        entry.ok(DeviceState::Active);
        tracing::debug!(device = %entry.spec.id, "playback opened");
        Ok(())
    }

    async fn samples(&mut self, samples: Vec<i16>) {
        if !self.talking {
            // Late frames of a finished talk.
            return;
        }
        if self.open().is_err() {
            counter!("kamoder_audio_playback_dropped_frames_total").increment(1);
            return;
        }
        let Some(writer) = &self.writer else {
            return;
        };
        match writer.tx.try_send(samples) {
            Ok(()) => {}
            Err(std_mpsc::TrySendError::Full(_)) => {
                counter!("kamoder_audio_playback_dropped_frames_total").increment(1);
            }
            Err(std_mpsc::TrySendError::Disconnected(_)) => {
                // The writer failed; the next frame retries after backoff.
                counter!("kamoder_audio_playback_dropped_frames_total").increment(1);
                self.reap(true);
                self.switch_amp(false).await;
                return;
            }
        }
        self.deadline = Some(Instant::now() + self.inner.settings.speaker_tail);
        self.switch_amp(true).await;
    }

    /// Closes the node and turns the amp off. With [`Amp::Driver`] the amp
    /// goes first (its ioctl needs the node open). Otherwise it goes last:
    /// releasing an `ak_pcm` playback node drives the AV100 amp line low,
    /// which is *on* for that board, so muting before the close would be
    /// undone by the close.
    async fn close(&mut self) {
        let amp_first = matches!(self.amp, Amp::Driver);
        if amp_first {
            self.switch_amp(false).await;
        }
        self.deadline = None;
        if let Some(writer) = self.writer.take() {
            // The thread drains its queue, then drops the node.
            drop(writer.tx);
            if amp_first {
                self.closing = Some(writer.thread);
            } else {
                // At most the queued frames plus one period write away.
                let _ = tokio::task::spawn_blocking(move || writer.thread.join()).await;
            }
            tracing::debug!(device = %self.entry().spec.id, "playback closed");
        }
        if !amp_first {
            self.switch_amp(false).await;
        }
        self.entry().idle();
    }

    async fn switch_amp(&mut self, on: bool) {
        if self.amp_on == on {
            return;
        }
        // Recorded even on failure, so a broken amp does not retry per frame.
        self.amp_on = on;
        gauge!("kamoder_audio_speaker_on").set(f64::from(u8::from(on)));
        match self.amp.set(&*self.inner.backend, on).await {
            Ok(()) => tracing::debug!(on, amp = self.amp.name(), "speaker amp switched"),
            Err(error) => {
                counter!("kamoder_audio_errors_total", "device" => "amp").increment(1);
                tracing::warn!(on, amp = self.amp.name(), %error, "cannot switch the speaker amp");
            }
        }
    }
}

/// The writer thread: interpolate, write whole periods, fill gaps with
/// silence; ends when the speaker task drops the sender.
fn writer_thread(
    inner: &Inner,
    entry: &Entry,
    mut dev: Box<dyn Playback>,
    mut generation: u32,
    rx: &std_mpsc::Receiver<Vec<i16>>,
) -> backend::Result<()> {
    let period = inner.params.period_samples();
    let wait = inner.params.period();
    let mut interpolator =
        Interpolator::new(inner.params.rate).map_err(|e| backend::Error::new(e.to_string()))?;
    let mut pending: Vec<i16> = Vec::with_capacity(period * 2);
    loop {
        match rx.recv_timeout(wait) {
            Ok(samples) => interpolator.process(&samples, &mut pending),
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                // Starved for a whole period: play what there is, padded,
                // so the driver never underruns (spec: stalled talker).
                pending.resize(pending.len().div_ceil(period).max(1) * period, 0);
                counter!("kamoder_audio_playback_silence_total").increment(1);
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if entry.generation.load(Ordering::Acquire) != generation {
            generation = entry.apply_gain(|g| dev.set_gain(g))?;
        }
        let whole = pending.len() / period * period;
        for chunk in pending[..whole].chunks_exact(period) {
            dev.write(chunk)?;
        }
        pending.drain(..whole);
    }
}

#[cfg(test)]
pub(crate) mod tests;
