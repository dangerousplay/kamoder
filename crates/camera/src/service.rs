//! The camera service: one capture thread for every image sensor, which
//! encodes each picture to H.264 (the board's [`Encoder`]) and keeps the
//! latest one for snapshots; a small HTTP server on loopback; and the video socket the
//! `rtsp` service reads H.264 from.
//!
//! ```text
//! GET /snapshot.jpg[?camera=N]   image/jpeg, 503 while that sensor has no picture
//! GET /status                    JSON: per sensor, state, fps, stream, last error
//! unix /tmp/kamoder-video.sock   "N\n" → H.264 access units of sensor N
//! ```
//!
//! `N` is the board's sensor number ([`Camera::index`], `Status.index`), not
//! a position in the list served: a board with `sensors = [1]`, or one whose
//! sensor 0 is missing, serves camera `1` only, and asking for `0` gets a
//! 404 or a closed socket (see `video_proto`).
//!
//! Capture runs continuously (the ISP's auto exposure needs a running
//! pipeline to converge), but a frame is only copied out of the driver's
//! buffer while someone has asked for one recently, and only encoded output
//! that a client is subscribed to leaves the capture thread.

use crate::backend::{Backend, BoxError, Encoder, Frame, FrameRef, Session};
use crate::jpeg;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
pub use video_proto::{Status, StreamInfo};

/// A picture older than this is not served.
pub const MAX_AGE: Duration = Duration::from_secs(3);
/// Frames are copied out while a request came in this recently.
pub const DEMAND_WINDOW: Duration = Duration::from_secs(10);
/// How long a request waits for a fresh picture.
const WAIT: Duration = Duration::from_millis(1500);
/// Snapshots wider than this are halved (until they are not), by default.
pub const SNAPSHOT_MAX_WIDTH: u32 = 640;

/// One encoded access unit on its way to the video socket.
#[derive(Debug)]
pub struct VideoPacket {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Capture time, ms.
    pub timestamp_ms: u64,
}

/// Access units queued per subscriber before it is considered behind.
pub const VIDEO_BUFFER: usize = 16;

#[derive(Default)]
struct Slot {
    status: Status,
    /// Start of the current FPS window, frames and encoded bytes in it.
    window: Option<(Instant, u32, usize)>,
    latest: Option<(Instant, Arc<Frame>)>,
    demand: Option<Instant>,
}

/// Shared state of one sensor.
#[derive(Clone)]
pub struct Camera {
    /// Board sensor number, fixed at creation: what clients ask for.
    index: u32,
    slot: Arc<Mutex<Slot>>,
    video: tokio::sync::broadcast::Sender<Arc<VideoPacket>>,
    /// A new subscriber asked for an IDR.
    idr: Arc<AtomicBool>,
}

impl Default for Camera {
    /// Sensor 0.
    fn default() -> Self {
        Camera::new(0)
    }
}

impl Camera {
    /// The camera of board sensor `index`; its status reports that index
    /// from the start, before capture has brought anything up.
    pub fn new(index: u32) -> Self {
        let slot = Slot {
            status: Status {
                index,
                ..Status::default()
            },
            ..Slot::default()
        };
        Camera {
            index,
            slot: Arc::new(Mutex::new(slot)),
            video: tokio::sync::broadcast::channel(VIDEO_BUFFER).0,
            idr: Arc::default(),
        }
    }

    /// Board sensor number (`?camera=N`, the subscribe line, `vsN`).
    pub fn index(&self) -> u32 {
        self.index
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Encoded access units from now on; the next one is made an IDR.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<VideoPacket>> {
        self.idr.store(true, Ordering::Relaxed);
        self.video.subscribe()
    }

    pub fn status(&self) -> Status {
        self.lock().status.clone()
    }

    fn wanted(&self) -> bool {
        self.lock()
            .demand
            .is_some_and(|t| t.elapsed() < DEMAND_WINDOW)
    }

    /// Latest picture no older than [`MAX_AGE`]; records the demand.
    pub fn latest(&self) -> Option<Arc<Frame>> {
        let mut slot = self.lock();
        slot.demand = Some(Instant::now());
        slot.latest
            .as_ref()
            .filter(|(at, _)| at.elapsed() < MAX_AGE)
            .map(|(_, f)| Arc::clone(f))
    }

    pub fn publish(&self, frame: Frame) {
        let mut slot = self.lock();
        slot.latest = Some((Instant::now(), Arc::new(frame)));
    }

    /// Counts one captured frame (and its encoded size) and refreshes the
    /// FPS and bitrate once a second.
    fn count_frame(&self, encoded: usize) {
        let mut slot = self.lock();
        slot.status.frames += 1;
        let now = Instant::now();
        let (start, n, bytes) = slot.window.get_or_insert((now, 0, 0));
        *n += 1;
        *bytes += encoded;
        let elapsed = now.duration_since(*start);
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f32();
            let fps = (*n - 1) as f32 / secs;
            let kbps = *bytes as f32 * 8.0 / secs / 1000.0;
            slot.window = Some((now, 1, 0));
            slot.status.fps = (fps * 10.0).round() / 10.0;
            if let Some(stream) = slot.status.stream.as_mut() {
                stream.kbps = kbps.round();
            }
        }
    }

    fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.lock().status);
    }
}

/// Halves an NV12 picture by dropping every other pixel and line: cheap,
/// and plenty for a preview JPEG of a 720p stream.
pub fn half(f: &FrameRef<'_>) -> Frame {
    let (w, h) = (f.width as usize, f.height as usize);
    let (hw, hh) = (w / 2, h / 2);
    let mut data = Vec::with_capacity(hw * hh * 3 / 2);
    for y in 0..hh {
        let row = &f.data[2 * y * w..][..w];
        data.extend(row.iter().step_by(2));
    }
    let uv = &f.data[w * h..];
    for y in 0..hh / 2 {
        let row = &uv[2 * y * w..][..w];
        for pair in row.as_chunks::<4>().0 {
            data.extend_from_slice(&pair[..2]);
        }
    }
    Frame {
        width: hw as u32,
        height: hh as u32,
        data,
        timestamp_ms: f.timestamp_ms,
    }
}

/// The snapshot copy of a picture: halved while wider than `max_width`.
pub fn snapshot_of(f: &FrameRef<'_>, max_width: u32) -> Frame {
    if f.width <= max_width || f.width < 2 {
        return Frame::copy_of(f);
    }
    let mut out = half(f);
    while out.width > max_width && out.width >= 2 {
        out = half(&out.view());
    }
    out
}

/// The camera of board sensor `index`, wherever it sits in `cameras`.
pub fn find(cameras: &[Camera], index: u32) -> Option<&Camera> {
    cameras.iter().find(|c| c.index == index)
}

/// Tunables of [`capture`]; production values by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureOptions {
    /// Snapshots are halved while wider than this.
    pub snapshot_max_width: u32,
    /// A sensor that delivers nothing this long counts as failed.
    pub frame_timeout: Duration,
    /// First pause after a failure; doubled per failed bring-up up to
    /// `retry_max`, reset by any picture.
    pub retry_min: Duration,
    pub retry_max: Duration,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        CaptureOptions {
            snapshot_max_width: SNAPSHOT_MAX_WIDTH,
            frame_timeout: Duration::from_secs(2),
            retry_min: Duration::from_secs(2),
            retry_max: Duration::from_secs(60),
        }
    }
}

/// An encoder failure, tagged with its sensor.
#[derive(Debug)]
struct SensorError {
    index: u32,
    source: BoxError,
}

impl std::fmt::Display for SensorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sensor {}: {}", self.index, self.source)
    }
}

impl std::error::Error for SensorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

fn tag(index: u32) -> impl FnOnce(BoxError) -> BoxError {
    move |source| Box::new(SensorError { index, source })
}

/// Capture loop for every sensor; returns when `stop` is set.
///
/// The backend brings its sensors up and starts them together, then they are
/// read in turn (a dual-sensor board cannot retry them one by one). With
/// `encoders` (one per sensor slot, created once at start-up and kept across
/// capture retries), every picture is H.264-encoded straight from the
/// capture buffer and offered to the video socket's subscribers.
/// Any failure tears everything down and retries with a growing pause, and
/// is reported in each camera's status.
///
/// `cameras` pair with the backend's sensors by position; each must have
/// been created for that sensor's index ([`Camera::new`]).
pub fn capture<B: Backend>(
    mut backend: B,
    cameras: Vec<Camera>,
    mut encoders: Vec<Option<Box<dyn Encoder>>>,
    stop: Arc<AtomicBool>,
    opts: CaptureOptions,
) {
    let expected = backend.sensors();
    encoders.resize_with(expected.len(), || None);
    let mut pause = opts.retry_min;
    for (info, camera) in expected.iter().zip(&cameras) {
        if camera.index != info.index {
            tracing::warn!(
                camera = camera.index,
                sensor = info.index,
                "camera created for another sensor than the backend's slot"
            );
        }
        camera.update(|s| {
            s.state = "starting".into();
            s.format = "NV12".into();
            s.encoding = "JPEG snapshots (software)".into();
            s.tuning = info.tuning.clone();
        });
    }
    while !stop.load(Ordering::Relaxed) {
        let result = run(
            &mut backend,
            &cameras,
            &mut encoders,
            &stop,
            &opts,
            &mut pause,
        );
        if let Err(e) = result {
            tracing::warn!(error = %e, retry_in = ?pause, "capture failed");
            for camera in &cameras {
                camera.update(|s| {
                    s.state = "failed".into();
                    s.fps = 0.0;
                    s.error = Some(e.to_string());
                });
            }
            let until = Instant::now() + pause;
            while !stop.load(Ordering::Relaxed) {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                std::thread::sleep(left.min(Duration::from_millis(200)));
            }
            pause = (pause * 2).min(opts.retry_max);
        }
    }
}

/// One bring-up and the capture until `stop` or the first failure.
fn run<B: Backend>(
    backend: &mut B,
    cameras: &[Camera],
    encoders: &mut [Option<Box<dyn Encoder>>],
    stop: &AtomicBool,
    opts: &CaptureOptions,
    pause: &mut Duration,
) -> Result<(), BoxError> {
    let mut session = backend.start()?;
    let sensors = session.sensors();
    for ((info, camera), enc) in sensors.iter().zip(cameras).zip(encoders.iter()) {
        camera.update(|s| {
            s.state = "running".into();
            (s.sensor_width, s.sensor_height) = info.active.unwrap_or_default();
            s.sensor_id = info.id.clone();
            s.width = info.width;
            s.height = info.height;
            s.error = None;
            s.stream = enc.as_ref().map(|e| e.info());
        });
    }
    tracing::info!(
        sensors = sensors.len(),
        encoding = encoders.iter().any(Option::is_some),
        "capturing"
    );
    while !stop.load(Ordering::Relaxed) {
        for (slot, ((info, camera), enc)) in sensors
            .iter()
            .zip(cameras)
            .zip(encoders.iter_mut())
            .enumerate()
        {
            let wanted = camera.wanted();
            let index = info.index;
            let encoded = session.with_frame(slot, opts.frame_timeout, &mut |f| {
                if wanted {
                    camera.publish(snapshot_of(f, opts.snapshot_max_width));
                }
                let Some(enc) = enc.as_mut() else {
                    return Ok(0);
                };
                if camera.idr.swap(false, Ordering::Relaxed) {
                    enc.request_idr().map_err(tag(index))?;
                }
                let subscribed = camera.video.receiver_count() > 0;
                enc.encode(f, &mut |pkt| {
                    if subscribed {
                        let _ = camera.video.send(Arc::new(VideoPacket {
                            data: pkt.data.to_vec(),
                            keyframe: pkt.keyframe,
                            timestamp_ms: f.timestamp_ms,
                        }));
                    }
                })
                .map_err(tag(index))
            })?;
            // Only a picture proves the pipeline works; a bring-up that
            // never delivers one keeps backing off.
            *pause = opts.retry_min;
            camera.count_frame(encoded);
            metrics::counter!("kamoder_camera_frames_total", "camera" => index.to_string())
                .increment(1);
        }
    }
    Ok(())
}

/// Serves the video socket (`video_proto`): each client names a sensor (its
/// board index, not a position) on one line and then receives its access
/// units, starting at the next IDR, each as a [`video_proto::PacketHeader`]
/// followed by Annex-B data. A client that falls behind skips to the next
/// IDR.
pub async fn serve_video(listener: tokio::net::UnixListener, cameras: Arc<Vec<Camera>>) {
    loop {
        let Ok((conn, _)) = listener.accept().await else {
            continue;
        };
        let cameras = Arc::clone(&cameras);
        tokio::spawn(async move {
            if let Err(e) = video_client(conn, &cameras).await {
                tracing::debug!(error = %e, "video client gone");
            }
        });
    }
}

async fn video_client(conn: tokio::net::UnixStream, cameras: &[Camera]) -> std::io::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::broadcast::error::RecvError;
    let (read, mut write) = conn.into_split();
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let Some(camera) = video_proto::parse_subscribe(&line).and_then(|i| find(cameras, i)) else {
        return Err(std::io::Error::other(format!(
            "no camera {:?}",
            line.trim()
        )));
    };
    tracing::info!(camera = line.trim(), "video client subscribed");
    let mut rx = camera.subscribe();
    let mut synced = false;
    loop {
        let packet = match rx.recv().await {
            Ok(p) => p,
            Err(RecvError::Lagged(n)) => {
                tracing::debug!(missed = n, "video client behind; waiting for an IDR");
                camera.idr.store(true, Ordering::Relaxed);
                synced = false;
                continue;
            }
            Err(RecvError::Closed) => return Ok(()),
        };
        if !synced && !packet.keyframe {
            continue;
        }
        synced = true;
        let head = video_proto::PacketHeader {
            len: packet.data.len() as u32,
            keyframe: packet.keyframe,
            timestamp_ms: packet.timestamp_ms,
        };
        write.write_all(&head.encode()).await?;
        write.write_all(&packet.data).await?;
    }
}

#[derive(Clone)]
pub struct AppState {
    pub cameras: Arc<Vec<Camera>>,
    pub quality: u8,
}

#[derive(Debug, Deserialize)]
pub struct CameraQuery {
    /// Board sensor index; absent = the first camera served.
    camera: Option<u32>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/snapshot.jpg", get(snapshot))
        .route("/status", get(status))
        .with_state(state)
}

fn unavailable(message: String) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, message).into_response()
}

async fn snapshot(State(state): State<AppState>, Query(q): Query<CameraQuery>) -> Response {
    let camera = match q.camera {
        Some(index) => find(&state.cameras, index),
        None => state.cameras.first(),
    };
    let Some(camera) = camera.cloned() else {
        let which = q.camera.map_or_else(|| "served".into(), |i| i.to_string());
        return (StatusCode::NOT_FOUND, format!("no camera {which}")).into_response();
    };
    let index = camera.index;
    // The first request after a quiet spell only switches copying on; wait
    // for the next picture.
    let deadline = tokio::time::Instant::now() + WAIT;
    let frame = loop {
        if let Some(f) = camera.latest() {
            break f;
        }
        if tokio::time::Instant::now() >= deadline {
            let st = camera.status();
            return unavailable(match st.error {
                Some(e) => format!("camera {index} {}: {e}", st.state),
                None => format!("camera {index} has no picture yet ({})", st.state),
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let quality = state.quality;
    match tokio::task::spawn_blocking(move || jpeg::encode(&frame, quality)).await {
        Ok(Ok(jpeg)) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            jpeg,
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("JPEG encode failed: {e}"),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn status(State(state): State<AppState>) -> Json<Vec<Status>> {
    Json(state.cameras.iter().map(Camera::status).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn frame() -> Frame {
        Frame {
            width: 32,
            height: 16,
            data: vec![100; 32 * 16 * 3 / 2],
            timestamp_ms: 5,
        }
    }

    #[tokio::test]
    async fn serves_the_latest_picture_per_camera() {
        let cams = vec![Camera::new(0), Camera::new(1)];
        cams[1].publish(frame());
        let app = router(AppState {
            cameras: Arc::new(cams),
            quality: 80,
        });
        let ok = app
            .clone()
            .oneshot(
                Request::get("/snapshot.jpg?camera=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(ok.headers()[header::CONTENT_TYPE], "image/jpeg");
        let missing = app
            .clone()
            .oneshot(
                Request::get("/snapshot.jpg?camera=7")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    async fn get(app: &Router, uri: &str) -> StatusCode {
        app.clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// `?camera=N` is the board sensor index, not a position: a producer
    /// serving sensor 1 only (a `sensors = [1]` board, or sensor 0 missing)
    /// answers `camera=1` and has no camera 0.
    #[tokio::test]
    async fn camera_is_the_sensor_index_not_a_position() {
        let cams = vec![Camera::new(1)];
        cams[0].publish(frame());
        let app = router(AppState {
            cameras: Arc::new(cams),
            quality: 80,
        });
        assert_eq!(get(&app, "/snapshot.jpg?camera=1").await, StatusCode::OK);
        assert_eq!(
            get(&app, "/snapshot.jpg?camera=0").await,
            StatusCode::NOT_FOUND
        );
        // No `camera`: the first one served.
        assert_eq!(get(&app, "/snapshot.jpg").await, StatusCode::OK);
        let r = app
            .clone()
            .oneshot(Request::get("/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.starts_with(r#"[{"index":1,"#), "{body}");
        assert_eq!(body.matches(r#""index""#).count(), 1, "{body}");
    }

    /// Sparse and out of order: each camera is found by its own index.
    #[tokio::test(start_paused = true)]
    async fn sparse_cameras_are_found_by_index() {
        let cams = vec![Camera::new(2), Camera::new(0)];
        cams[1].publish(frame());
        let app = router(AppState {
            cameras: Arc::new(cams),
            quality: 80,
        });
        assert_eq!(get(&app, "/snapshot.jpg?camera=0").await, StatusCode::OK);
        assert_eq!(
            get(&app, "/snapshot.jpg?camera=1").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(&app, "/snapshot.jpg?camera=2").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_picture_is_503() {
        let app = router(AppState {
            cameras: Arc::new(vec![Camera::default()]),
            quality: 80,
        });
        let r = app
            .oneshot(Request::get("/snapshot.jpg").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

#[cfg(test)]
mod half_tests {
    use super::*;

    #[test]
    fn halves_nv12() {
        let (w, h) = (8usize, 4usize);
        let mut data: Vec<u8> = (0..w * h).map(|i| i as u8).collect();
        // Chroma rows: (U, V) pairs numbered by position.
        data.extend((0..w * h / 2).map(|i| 100 + i as u8));
        let f = FrameRef {
            width: w as u32,
            height: h as u32,
            format: crate::backend::PixelFormat::Nv12,
            data: &data,
            phys: None,
            timestamp_ms: 7,
        };
        let half = half(&f);
        assert_eq!((half.width, half.height, half.timestamp_ms), (4, 2, 7));
        assert_eq!(&half.data[..8], &[0, 2, 4, 6, 16, 18, 20, 22]);
        // One chroma row of 4 bytes: pairs 0 and 2 of chroma row 0.
        assert_eq!(&half.data[8..], &[100, 101, 104, 105]);
    }

    #[test]
    fn snapshot_halves_while_wider_than_max() {
        let big = Frame {
            width: 128,
            height: 64,
            data: vec![9; 128 * 64 * 3 / 2],
            timestamp_ms: 3,
        };
        let s = snapshot_of(&big.view(), 40);
        assert_eq!((s.width, s.height, s.data.len()), (32, 16, 32 * 16 * 3 / 2));
        let s = snapshot_of(&big.view(), 64);
        assert_eq!((s.width, s.height), (64, 32));
        let s = snapshot_of(&big.view(), 640);
        assert_eq!((s.width, s.height, s.data.len()), (128, 64, big.data.len()));
    }
}

/// CAM-05: the capture loop, the snapshot path and the video socket driven
/// by `fake` instead of hardware.
#[cfg(test)]
mod capture_tests {
    use super::*;
    use crate::fake::{FakeBackend, FakeEncoder, FakeEncoderStats, FAKE_PARAMETER_SETS};
    use std::sync::atomic::AtomicUsize;

    /// Short pauses so failure tests take milliseconds, not seconds.
    fn quick() -> CaptureOptions {
        CaptureOptions {
            retry_min: Duration::from_millis(100),
            retry_max: Duration::from_millis(400),
            ..CaptureOptions::default()
        }
    }

    /// A capture thread, stopped and joined on drop.
    struct Running {
        cameras: Vec<Camera>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                t.join().unwrap();
            }
        }
    }

    fn run(
        backend: FakeBackend,
        encoders: Vec<Option<Box<dyn Encoder>>>,
        opts: CaptureOptions,
    ) -> Running {
        let cameras: Vec<Camera> = backend.indices.iter().map(|&i| Camera::new(i)).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (cameras, stop) = (cameras.clone(), Arc::clone(&stop));
            std::thread::spawn(move || capture(backend, cameras, encoders, stop, opts))
        };
        Running {
            cameras,
            stop,
            thread: Some(thread),
        }
    }

    fn encoder(w: u32, h: u32, gop: u64) -> (Option<Box<dyn Encoder>>, Arc<FakeEncoderStats>) {
        let e = FakeEncoder::new(w, h, gop);
        let stats = Arc::clone(&e.stats);
        (Some(Box::new(e)), stats)
    }

    /// Polls `cond` for up to 5 s.
    fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn frames(c: &Camera) -> u64 {
        c.status().frames
    }

    #[test]
    fn status_reports_sensor_and_stream() {
        let (enc, _) = encoder(64, 32, 30);
        let r = run(FakeBackend::new(1, 64, 32), vec![enc], quick());
        wait_for("frames", || frames(&r.cameras[0]) >= 3);
        let st = r.cameras[0].status();
        assert_eq!(st.state, "running");
        assert_eq!((st.width, st.height), (64, 32));
        assert_eq!((st.sensor_width, st.sensor_height), (128, 64));
        assert_eq!(st.sensor_id.as_deref(), Some("0xfa00"));
        assert_eq!(st.tuning.as_deref(), Some("fake0.conf"));
        assert_eq!((st.format.as_str(), st.error), ("NV12", None));
        let stream = st.stream.unwrap();
        assert_eq!(
            (stream.codec.as_str(), stream.encoder.as_str()),
            ("H.264", "fake")
        );
    }

    #[test]
    fn snapshot_is_copied_only_on_demand_and_downscaled() {
        let r = run(
            FakeBackend::new(1, 128, 64),
            Vec::new(),
            CaptureOptions {
                snapshot_max_width: 40,
                ..quick()
            },
        );
        let cam = &r.cameras[0];
        wait_for("frames", || frames(cam) >= 10);
        assert!(cam.lock().latest.is_none(), "no copy without a request");
        // The first request only switches copying on.
        let mut got = cam.latest();
        wait_for("a snapshot", || {
            got = cam.latest();
            got.is_some()
        });
        let f = got.unwrap();
        // 128 → 64 → 32, the first width not above 40.
        assert_eq!((f.width, f.height, f.data.len()), (32, 16, 32 * 16 * 3 / 2));
        // Frames run with or without encoders; a sensor without one has no
        // stream in its status.
        assert!(cam.status().stream.is_none());
    }

    #[test]
    fn packets_fan_out_to_every_subscriber() {
        let (enc0, _) = encoder(32, 16, 30);
        let (enc1, _) = encoder(32, 16, 30);
        let backend = FakeBackend {
            interval: Duration::from_millis(5),
            ..FakeBackend::new(2, 32, 16)
        };
        // Subscribed before the first picture: both see the same units.
        let r = {
            let cameras = vec![Camera::new(0), Camera::new(1)];
            let mut a = cameras[0].subscribe();
            let mut b = cameras[0].subscribe();
            let mut other = cameras[1].subscribe();
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (cameras, stop) = (cameras.clone(), Arc::clone(&stop));
                std::thread::spawn(move || {
                    capture(backend, cameras, vec![enc0, enc1], stop, quick())
                })
            };
            for n in 0..5 {
                let (pa, pb) = (a.blocking_recv().unwrap(), b.blocking_recv().unwrap());
                assert!(Arc::ptr_eq(&pa, &pb), "same unit to both");
                assert_eq!(pa.keyframe, n == 0);
                assert!(other.blocking_recv().is_ok());
            }
            Running {
                cameras,
                stop,
                thread: Some(thread),
            }
        };
        // Counted after the units are sent, so it may trail by a picture.
        wait_for("frames", || frames(&r.cameras[1]) >= 5);
    }

    #[test]
    fn subscribing_requests_an_idr() {
        let (enc, stats) = encoder(32, 16, 1_000_000);
        let r = run(FakeBackend::new(1, 32, 16), vec![enc], quick());
        let cam = &r.cameras[0];
        wait_for("frames", || stats.encoded.load(Ordering::Relaxed) >= 5);
        assert_eq!(stats.keyframes.load(Ordering::Relaxed), 1, "only the first");
        assert_eq!(stats.idr_requests.load(Ordering::Relaxed), 0);
        let mut rx = cam.subscribe();
        // A picture already being encoded may come first; the IDR follows.
        let key = (0..3).any(|_| rx.blocking_recv().unwrap().keyframe);
        assert!(key, "a keyframe right after subscribing");
        assert_eq!(stats.idr_requests.load(Ordering::Relaxed), 1);
        assert!(stats.keyframes.load(Ordering::Relaxed) >= 2);
    }

    #[test]
    fn lagging_subscriber_gets_a_new_idr() {
        let (enc, stats) = encoder(32, 16, 1_000_000);
        let backend = FakeBackend {
            interval: Duration::from_millis(1),
            ..FakeBackend::new(1, 32, 16)
        };
        let r = run(backend, vec![enc], quick());
        let cam = &r.cameras[0];
        let mut rx = cam.subscribe();
        wait_for("lag", || {
            stats.encoded.load(Ordering::Relaxed) > VIDEO_BUFFER * 2
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let lagged = rt.block_on(rx.recv());
        assert!(matches!(
            lagged,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
        ));
        // What `video_client` does on lag.
        cam.idr.store(true, Ordering::Relaxed);
        wait_for("IDR", || stats.idr_requests.load(Ordering::Relaxed) >= 2);
    }

    #[test]
    fn start_failure_is_reported_then_recovers() {
        let backend = FakeBackend {
            fail_starts: 2,
            ..FakeBackend::new(2, 32, 16)
        };
        let stats = Arc::clone(&backend.stats);
        let r = run(backend, Vec::new(), quick());
        wait_for("failed", || r.cameras[0].status().state == "failed");
        for cam in &r.cameras {
            let st = cam.status();
            assert_eq!(st.state, "failed");
            assert!(
                st.error.as_deref().unwrap().contains("start failed"),
                "{:?}",
                st.error
            );
        }
        wait_for("recovery", || {
            r.cameras.iter().all(|c| {
                let st = c.status();
                st.state == "running" && st.error.is_none() && st.frames > 0
            })
        });
        assert_eq!(stats.starts.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn frame_failure_restarts_every_sensor_with_encoders_kept() {
        let (enc, estats) = encoder(32, 16, 1_000_000);
        let backend = FakeBackend {
            fail_frame: Some(4),
            ..FakeBackend::new(2, 32, 16)
        };
        let stats = Arc::clone(&backend.stats);
        let r = run(backend, vec![enc], quick());
        wait_for("failed", || r.cameras[1].status().state == "failed");
        assert_eq!(
            r.cameras[1].status().error.as_deref(),
            Some("fake: sensor 0: no frame")
        );
        wait_for("recovery", || {
            r.cameras[0].status().state == "running" && stats.starts.load(Ordering::Relaxed) == 2
        });
        // Same encoder before and after: it was not re-created.
        let before = estats.encoded.load(Ordering::Relaxed);
        wait_for("encoding", || {
            estats.encoded.load(Ordering::Relaxed) > before
        });
    }

    #[test]
    fn encoder_errors_name_the_sensor() {
        // Encoder expects a different size than the sensor delivers.
        let (enc, _) = encoder(64, 32, 30);
        let r = run(FakeBackend::new(1, 32, 16), vec![enc], quick());
        wait_for("failed", || r.cameras[0].status().state == "failed");
        let e = r.cameras[0].status().error.unwrap();
        assert!(e.starts_with("sensor 0: fake encoder: frame is"), "{e}");
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "camera-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn video_socket_end_to_end() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use video_proto::{subscribe_line, PacketHeader, HEADER_LEN};

        let (enc, _) = encoder(32, 16, 1_000_000);
        let r = run(FakeBackend::new(1, 32, 16), vec![enc], quick());
        let dir = TempDir::new();
        let path = dir.0.join("video.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(serve_video(listener, Arc::new(r.cameras.clone())));

        let test = async {
            // An unknown sensor is refused: the server closes the connection.
            let mut bad = tokio::net::UnixStream::connect(&path).await.unwrap();
            bad.write_all(subscribe_line(7).as_bytes()).await.unwrap();
            let mut rest = Vec::new();
            bad.read_to_end(&mut rest).await.unwrap();
            assert!(rest.is_empty());

            let mut conn = tokio::net::UnixStream::connect(&path).await.unwrap();
            conn.write_all(subscribe_line(0).as_bytes()).await.unwrap();
            let mut last_ts = None;
            for n in 0..4 {
                let mut head = [0u8; HEADER_LEN];
                conn.read_exact(&mut head).await.unwrap();
                let h = PacketHeader::decode(&head);
                let mut data = vec![0; h.len as usize];
                conn.read_exact(&mut data).await.unwrap();
                assert_eq!(&data[..4], &[0, 0, 0, 1], "Annex-B");
                if n == 0 {
                    assert!(h.keyframe, "stream starts at an IDR");
                    assert!(data.starts_with(FAKE_PARAMETER_SETS));
                }
                if let Some(t) = last_ts {
                    assert!(h.timestamp_ms > t);
                }
                last_ts = Some(h.timestamp_ms);
            }
        };
        tokio::time::timeout(Duration::from_secs(10), test)
            .await
            .expect("video socket test timed out");
    }

    /// A board listing sensors `[0, 1]` whose sensor 0 failed to open (or a
    /// `sensors = [1]` board): the producer serves sensor 1 only, and the
    /// subscribe line names the sensor index, not the position.
    #[tokio::test(flavor = "current_thread")]
    async fn video_socket_subscribes_by_sensor_index() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use video_proto::{subscribe_line, PacketHeader, HEADER_LEN};

        let (enc, _) = encoder(32, 16, 1_000_000);
        let r = run(
            FakeBackend::with_indices(vec![1], 32, 16),
            vec![enc],
            quick(),
        );
        assert_eq!(r.cameras[0].index(), 1);
        let dir = TempDir::new();
        let path = dir.0.join("video.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(serve_video(listener, Arc::new(r.cameras.clone())));

        let test = async {
            // Position 0 is not sensor 0: refused.
            let mut bad = tokio::net::UnixStream::connect(&path).await.unwrap();
            bad.write_all(subscribe_line(0).as_bytes()).await.unwrap();
            let mut rest = Vec::new();
            bad.read_to_end(&mut rest).await.unwrap();
            assert!(rest.is_empty());

            let mut conn = tokio::net::UnixStream::connect(&path).await.unwrap();
            conn.write_all(subscribe_line(1).as_bytes()).await.unwrap();
            let mut head = [0u8; HEADER_LEN];
            conn.read_exact(&mut head).await.unwrap();
            let h = PacketHeader::decode(&head);
            assert!(h.keyframe, "stream starts at an IDR");
            let mut data = vec![0; h.len as usize];
            conn.read_exact(&mut data).await.unwrap();
            assert!(data.starts_with(FAKE_PARAMETER_SETS));
        };
        tokio::time::timeout(Duration::from_secs(10), test)
            .await
            .expect("video socket test timed out");
        let st = r.cameras[0].status();
        assert_eq!((st.index, st.tuning.as_deref()), (1, Some("fake1.conf")));
    }
}
