//! The camera service: one capture thread for every image sensor, which
//! hardware-encodes each picture to H.264 and keeps the latest one for
//! snapshots; a small HTTP server on loopback; and the video socket the
//! `rtsp` service reads H.264 from.
//!
//! ```text
//! GET /snapshot.jpg[?camera=N]   image/jpeg, 503 while that sensor has no picture
//! GET /status                    JSON: per sensor, state, fps, stream, last error
//! unix /tmp/kamoder-video.sock   "N\n" → H.264 access units of sensor N
//! ```
//!
//! Capture runs continuously (the ISP's auto exposure needs a running
//! pipeline to converge), but a frame is only copied out of the driver's
//! buffer while someone has asked for one recently, and only encoded output
//! that a client is subscribed to leaves the capture thread.

use crate::device::{ChannelSpec, Frame, FrameRef, Options, Sensor};
use crate::jpeg;
use crate::venc::{self, Encoder, Settings};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A picture older than this is not served.
pub const MAX_AGE: Duration = Duration::from_secs(3);
/// Frames are copied out while a request came in this recently.
pub const DEMAND_WINDOW: Duration = Duration::from_secs(10);
/// How long a request waits for a fresh picture.
const WAIT: Duration = Duration::from_millis(1500);

/// The H.264 stream of one sensor.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct StreamInfo {
    pub codec: &'static str,
    pub profile: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub gop: u32,
    pub target_kbps: u32,
    /// Measured over the last second or so.
    pub kbps: f32,
    pub encoder: String,
}

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

#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub index: u32,
    /// `starting`, `running`, `failed`.
    pub state: &'static str,
    /// Preview (sub channel) size.
    pub width: u32,
    pub height: u32,
    /// Pixel format of the captured pictures.
    pub format: &'static str,
    /// How snapshots are encoded.
    pub encoding: &'static str,
    pub frames: u64,
    /// Measured over the last second or so.
    pub fps: f32,
    /// Sensor active area and chip id, once brought up.
    pub sensor_width: u32,
    pub sensor_height: u32,
    pub sensor_id: Option<String>,
    /// ISP tuning file in use.
    pub tuning: Option<String>,
    /// Hardware H.264, when encoding.
    pub stream: Option<StreamInfo>,
    pub error: Option<String>,
}

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
    slot: Arc<Mutex<Slot>>,
    video: tokio::sync::broadcast::Sender<Arc<VideoPacket>>,
    /// A new subscriber asked for an IDR.
    idr: Arc<AtomicBool>,
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            slot: Arc::default(),
            video: tokio::sync::broadcast::channel(VIDEO_BUFFER).0,
            idr: Arc::default(),
        }
    }
}

impl Camera {
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

/// Capture loop for every sensor; returns when `stop` is set.
///
/// The sensors are brought up and started together, then read in turn. A
/// dual-sensor board runs its ISPs in dual mode, where neither delivers a
/// picture until both are streaming, so they cannot be retried one by one.
/// With encoder `channels` (one per sensor, created once at start-up and
/// kept across capture retries, see [`open_channels`]), every picture is
/// H.264-encoded in hardware straight from the capture buffer and offered to
/// the video socket's subscribers.
/// Any failure tears everything down and retries with a growing pause, and
/// is reported in each camera's status.
pub fn capture(
    sensors: Vec<(Options, usize)>,
    cameras: Vec<Camera>,
    encoder: Option<(Arc<Encoder>, Settings)>,
    mut channels: Vec<Option<venc::Channel>>,
    stop: Arc<AtomicBool>,
) {
    channels.resize_with(sensors.len(), || None);
    let mut pause = Duration::from_secs(2);
    for ((opts, _), camera) in sensors.iter().zip(&cameras) {
        camera.update(|s| {
            s.index = opts.index;
            s.state = "starting";
            s.format = "NV12";
            s.encoding = "JPEG snapshots (software)";
            s.tuning = opts
                .conf
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
        });
    }
    while !stop.load(Ordering::Relaxed) {
        let result = (|| {
            let mut open = Vec::new();
            for (opts, _) in &sensors {
                open.push(Sensor::open(opts)?);
            }
            for sensor in &mut open {
                sensor.start()?;
            }
            for (((opts, preview), camera), sensor) in sensors.iter().zip(&cameras).zip(&open) {
                let spec = opts.channels[*preview];
                camera.update(|s| {
                    s.state = "running";
                    s.sensor_width = sensor.width;
                    s.sensor_height = sensor.height;
                    s.sensor_id = Some(format!("{:#06x}", sensor.sensor_id));
                    s.width = spec.width;
                    s.height = spec.height;
                    s.error = None;
                    s.stream = encoder.as_ref().map(|(e, st)| StreamInfo {
                        codec: "H.264",
                        profile: match st.profile {
                            venc::PROFILE_BASELINE => "Baseline",
                            venc::PROFILE_HIGH => "High",
                            _ => "Main",
                        },
                        width: st.width,
                        height: st.height,
                        fps: st.fps,
                        gop: st.gop,
                        target_kbps: st.kbps,
                        kbps: 0.0,
                        encoder: e.version.clone(),
                    });
                });
            }
            tracing::info!(
                sensors = open.len(),
                encoding = encoder.is_some(),
                "capturing"
            );
            while !stop.load(Ordering::Relaxed) {
                for (((sensor, (opts, preview)), camera), chan) in open
                    .iter_mut()
                    .zip(&sensors)
                    .zip(&cameras)
                    .zip(channels.iter_mut())
                {
                    let wanted = camera.wanted();
                    let encoded = sensor.with_frame(*preview, Duration::from_secs(2), |f| {
                        if wanted {
                            camera.publish(if f.width > 640 {
                                half(&f)
                            } else {
                                Frame {
                                    width: f.width,
                                    height: f.height,
                                    data: f.data.to_vec(),
                                    timestamp_ms: f.timestamp_ms,
                                }
                            });
                        }
                        let Some(chan) = chan.as_mut() else {
                            return Ok::<usize, venc::Error>(0);
                        };
                        if camera.idr.swap(false, Ordering::Relaxed) {
                            chan.request_idr()?;
                        }
                        let subscribed = camera.video.receiver_count() > 0;
                        chan.encode(&f, |pkt| {
                            if subscribed {
                                let _ = camera.video.send(Arc::new(VideoPacket {
                                    data: pkt.data.to_vec(),
                                    keyframe: pkt.keyframe,
                                    timestamp_ms: f.timestamp_ms,
                                }));
                            }
                            pkt.data.len()
                        })
                    })?;
                    let encoded = encoded.map_err(|e| crate::device::Error::Encoder {
                        index: opts.index,
                        source: e,
                    })?;
                    // Only a picture proves the pipeline works; a bring-up
                    // that never delivers one keeps backing off.
                    pause = Duration::from_secs(2);
                    camera.count_frame(encoded);
                    metrics::counter!("kamoder_camera_frames_total", "camera" => opts.index.to_string())
                        .increment(1);
                }
            }
            Ok::<_, crate::device::Error>(())
        })();
        if let Err(e) = result {
            tracing::warn!(error = %e, retry_in = ?pause, "capture failed");
            for camera in &cameras {
                camera.update(|s| {
                    s.state = "failed";
                    s.fps = 0.0;
                    s.error = Some(e.to_string());
                });
            }
            let until = Instant::now() + pause;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
            }
            pause = (pause * 2).min(Duration::from_secs(60));
        }
    }
}

/// Writes `value` to a procfs knob, ignoring failure (not root, no such knob).
fn proc_write(path: &str, value: &str) {
    if let Err(e) = std::fs::write(path, value) {
        tracing::debug!(path, error = %e, "procfs write failed");
    }
}

/// Frees and defragments memory for the encoder driver, whose channel
/// set-up allocates 16 KiB contiguous blocks and, in `ak_venc_adapter`
/// V1.0.74, oopses instead of failing when it cannot get them.
pub fn make_room_for_encoder() {
    proc_write("/proc/sys/vm/drop_caches", "3");
    proc_write("/proc/sys/vm/compact_memory", "1");
}

/// Opens one channel per sensor, once, right after the encoder: channels
/// are never closed and re-created while the service runs, because each
/// creation risks the driver oops above on a fragmented system.
pub fn open_channels(
    encoder: &Encoder,
    settings: Settings,
    count: usize,
) -> Result<Vec<Option<venc::Channel>>, venc::Error> {
    let dev = std::path::Path::new("/dev");
    let mut channels = Vec::new();
    for _ in 0..count {
        make_room_for_encoder();
        channels.push(Some(encoder.channel(dev, settings)?));
    }
    Ok(channels)
}

/// Serves the video socket: each client names a sensor index on one line
/// and then receives its access units, starting at the next IDR, as a
/// 13-byte header (`u32` BE length, `u8` flags bit 0 = IDR, `u64` BE capture
/// time in ms) followed by Annex-B data. A client that falls behind skips
/// to the next IDR.
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
    let Some(camera) = line
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|i| cameras.get(i))
    else {
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
        let mut head = [0u8; 13];
        head[0..4].copy_from_slice(&(packet.data.len() as u32).to_be_bytes());
        head[4] = u8::from(packet.keyframe);
        head[5..13].copy_from_slice(&packet.timestamp_ms.to_be_bytes());
        write.write_all(&head).await?;
        write.write_all(&packet.data).await?;
    }
}

/// Builds the per-sensor capture options.
pub fn options(
    index: u32,
    conf: std::path::PathBuf,
    dual: bool,
    main: Option<(u32, u32)>,
    preview: (u32, u32),
) -> (Options, usize) {
    let mut channels = Vec::new();
    if let Some((w, h)) = main {
        channels.push(ChannelSpec {
            node: 0,
            width: w,
            height: h,
            buffers: 3,
        });
    }
    channels.push(ChannelSpec {
        node: 1,
        width: preview.0,
        height: preview.1,
        buffers: 2,
    });
    let preview_index = channels.len() - 1;
    let opts = Options {
        index,
        conf,
        slot: 0,
        dual,
        channels,
        dev: "/dev".into(),
    };
    (opts, preview_index)
}

#[derive(Clone)]
pub struct AppState {
    pub cameras: Arc<Vec<Camera>>,
    pub quality: u8,
}

#[derive(Debug, Deserialize)]
pub struct CameraQuery {
    camera: Option<usize>,
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
    let index = q.camera.unwrap_or(0);
    let Some(camera) = state.cameras.get(index).cloned() else {
        return (StatusCode::NOT_FOUND, format!("no camera {index}")).into_response();
    };
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
        let cams = vec![Camera::default(), Camera::default()];
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
            data: &data,
            phys: 0,
            timestamp_ms: 7,
        };
        let half = half(&f);
        assert_eq!((half.width, half.height, half.timestamp_ms), (4, 2, 7));
        assert_eq!(&half.data[..8], &[0, 2, 4, 6, 16, 18, 20, 22]);
        // One chroma row of 4 bytes: pairs 0 and 2 of chroma row 0.
        assert_eq!(&half.data[8..], &[100, 101, 104, 105]);
    }
}
