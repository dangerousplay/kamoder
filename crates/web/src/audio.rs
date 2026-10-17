//! Microphone and speaker, through the audio service's socket (AUD-08, 09,
//! 19, 20, 23, 24, 25).
//!
//! The `audio` service owns the codec and serves `/tmp/kamoder-audio.sock`
//! ([`audio_proto`]); this module translates for `webui/src/lib/api.ts`. A
//! missing socket is 503 `audio_unavailable` on the JSON routes, so the UI
//! shows the card as unavailable.
//!
//! ```text
//! GET /api/audio/devices                     (any user)
//!   → {"devices":[{"id":"mic","kind":"input","name":"Microphone","state":"active",
//!                  "rate":8000,"gain":5,"gain_range":[0,7],"error":null}, ...],
//!      "talk":{"https_port":443}}            (null when HTTPS is not up)
//! PUT /api/audio/devices/{id}  {"gain":4}    (admin)
//!   → the device after the change; 400 out_of_range, 404 not_found
//! ```
//!
//! `talk.https_port` is how the UI finds the HTTPS server for push-to-talk:
//! the microphone needs a secure context, so a page loaded over HTTP links
//! to `https://<same host>:<https_port>/` instead (AUD-21). It rides on the
//! devices answer because that is the call the audio card makes anyway.
//!
//! # WebSockets
//!
//! Browsers cannot set headers on a WebSocket, so both take `?token=` (a
//! stream token from `POST /api/logs/stream-token`, as the log stream and
//! MJPEG do). Refusals before the upgrade are plain HTTP errors (401, 403);
//! once the socket is open, failures are close codes the UI can read:
//!
//! ```text
//! GET /api/audio/talk?token=…     (admin)   push-to-talk (AUD-19, AUD-20)
//!   client → server  binary: S16LE mono 8 kHz PCM, any even length 2..=4096
//!                    bytes (320 = 20 ms recommended); each becomes one frame
//!                    to the speaker, stamped with the time it arrived
//!   client → server  text "end": stop talking (closing the socket does too)
//!   server → client  nothing but the close frame
//! GET /api/audio/listen?token=…   (any user) live microphone (AUD-23)
//!   server → client  binary: S16LE mono 8 kHz PCM, one message per 20 ms frame
//!   client → server  text "end" or close: stop
//!
//! close codes  1000 ended normally          1001 server shutting down
//!              4408 talk idle for 10 s      4409 speaker busy (other talker)
//!              4429 too many listeners      4503 audio service unavailable
//! ```
//!
//! Talk takes the speaker *before* the upgrade, so `busy` is known at once;
//! the audio service drives the amplifier and keeps it on ≤ 1 s after the
//! last frame (AUD-20). A talk message of the wrong size is dropped and
//! counted (`kamoder_web_audio_talk_bad_frames_total`), not fatal: a browser
//! glitch should not end the talk. Listen reads the capture socket on its own
//! task into a short queue; when the browser cannot keep up, frames are
//! dropped (`kamoder_web_audio_listen_dropped_frames_total`) rather than
//! buffered, so a slow phone costs neither RAM nor the other subscribers
//! (AUD-24).

use crate::auth::{AdminUser, AuthUser, StreamUser};
use crate::error::{ApiError, ApiJson};
use crate::state::AppState;
use audio_proto::{Client, ClientError, Device, ErrorCode, Frame, PlaySink, Talker};
use axum::body::Bytes;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use metrics::{counter, describe_counter};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Talk ends after this long without a message from the browser.
pub const TALK_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout for `status`/`set` on the audio socket: an HTTP request should
/// not hang for the protocol's default 5 s.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Frames queued for a listener before new ones are dropped: 200 ms.
pub const LISTEN_QUEUE: usize = 10;
/// A listener whose socket takes longer than this to accept one frame is
/// disconnected.
pub const LISTEN_SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest WebSocket message accepted: one maximal frame plus slack. The
/// tungstenite default (64 MB) is twice the camera's RAM.
pub const MAX_MESSAGE: usize = 2 * audio_proto::MAX_FRAME_BYTES;

/// Close code: the speaker is held by another talker.
pub const CLOSE_BUSY: u16 = 4409;
/// Close code: talk idle for [`TALK_IDLE_TIMEOUT`].
pub const CLOSE_IDLE: u16 = 4408;
/// Close code: [`crate::state::MAX_AUDIO_LISTENERS`] already listening.
pub const CLOSE_TOO_MANY: u16 = 4429;
/// Close code: the audio service is not there or failed.
pub const CLOSE_UNAVAILABLE: u16 = 4503;

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/audio/devices", get(devices))
        .route("/audio/devices/{id}", axum::routing::put(set_gain))
        .route("/audio/talk", get(talk))
        .route("/audio/listen", get(listen))
}

/// Registers metric descriptions.
pub fn describe_metrics() {
    describe_counter!(
        "kamoder_web_audio_talk_bad_frames_total",
        "Push-to-talk WebSocket messages dropped for a bad size or type"
    );
    describe_counter!(
        "kamoder_web_audio_listen_dropped_frames_total",
        "Microphone frames dropped because a listening browser lagged"
    );
}

/// `GET /api/audio/devices` answer.
#[derive(Debug, Serialize)]
pub struct DevicesResponse {
    pub devices: Vec<Device>,
    pub talk: TalkInfo,
}

/// Where push-to-talk is reachable.
#[derive(Debug, Serialize)]
pub struct TalkInfo {
    /// Port of the HTTPS server, `null` when it is not running.
    pub https_port: Option<u16>,
}

/// `PUT /api/audio/devices/{id}` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GainUpdate {
    pub gain: u32,
}

fn client(state: &AppState) -> Client {
    Client::new(&state.config.audio_socket)
}

/// Maps a failed `status`/`set` to the API error.
fn api_error(error: ClientError) -> ApiError {
    match error {
        ClientError::Rejected { code, message } => match code {
            ErrorCode::OutOfRange => {
                ApiError::new(StatusCode::BAD_REQUEST, "out_of_range", message)
            }
            ErrorCode::UnknownDevice => ApiError::not_found(message),
            ErrorCode::BadRequest => ApiError::bad_request(message),
            ErrorCode::Busy | ErrorCode::Unavailable => unavailable(message),
        },
        ClientError::Busy { message } => ApiError::conflict(message),
        error => {
            tracing::debug!(%error, "audio service unreachable");
            unavailable(format!("audio unavailable: {error}"))
        }
    }
}

fn unavailable(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "audio_unavailable",
        message,
    )
}

async fn device_list(state: &AppState) -> Result<Vec<Device>, ApiError> {
    client(state)
        .with_timeout(REQUEST_TIMEOUT)
        .status()
        .await
        .map_err(api_error)
}

/// `GET /api/audio/devices` → devices plus the HTTPS port for talk.
async fn devices(
    State(state): State<AppState>,
    _: AuthUser,
) -> Result<Json<DevicesResponse>, ApiError> {
    Ok(Json(DevicesResponse {
        devices: device_list(&state).await?,
        talk: TalkInfo {
            https_port: state.config.https_port,
        },
    }))
}

/// `PUT /api/audio/devices/{id}` (admin) `{gain}` → the device after it.
async fn set_gain(
    State(state): State<AppState>,
    _: AdminUser,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<GainUpdate>,
) -> Result<Json<Device>, ApiError> {
    client(&state)
        .with_timeout(REQUEST_TIMEOUT)
        .set(id.clone(), body.gain)
        .await
        .map_err(api_error)?;
    device_list(&state)
        .await?
        .into_iter()
        .find(|device| device.id == id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no audio device {id}")))
}

/// The upgrade, or the rejection in the `{error, code}` shape.
fn upgrade(
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<WebSocketUpgrade, ApiError> {
    let ws = ws.map_err(|rejection| {
        ApiError::new(rejection.status(), "bad_request", rejection.body_text())
    })?;
    Ok(ws
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .read_buffer_size(MAX_MESSAGE)
        // Every message is flushed by `send` anyway; keep the buffer small.
        .write_buffer_size(0))
}

/// Close code and reason for a failed `play`/`capture`.
fn close_for(error: &ClientError) -> (u16, &'static str) {
    match error {
        ClientError::Busy { .. } => (CLOSE_BUSY, "busy"),
        _ => (CLOSE_UNAVAILABLE, "audio_unavailable"),
    }
}

async fn close(mut socket: WebSocket, code: u16, reason: &'static str) {
    let frame = CloseFrame {
        code,
        reason: Utf8Bytes::from_static(reason),
    };
    let _ = socket.send(Message::Close(Some(frame))).await;
}

/// Monotonic milliseconds for talk frames' `ts_ms` (arbitrary zero, as the
/// protocol allows).
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// `GET /api/audio/talk?token=` (admin) — push-to-talk WebSocket.
async fn talk(
    State(state): State<AppState>,
    StreamUser(user): StreamUser,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::forbidden());
    }
    let ws = upgrade(ws)?;
    let sink = client(&state).play(Talker::Web).await;
    match &sink {
        Ok(_) => tracing::info!(user = %user.user, "talk started"),
        Err(error) => tracing::info!(user = %user.user, %error, "talk refused"),
    }
    Ok(ws.on_upgrade(move |socket| async move {
        match sink {
            Ok(sink) => talk_session(socket, sink, state).await,
            Err(error) => {
                let (code, reason) = close_for(&error);
                close(socket, code, reason).await;
            }
        }
    }))
}

async fn talk_session(mut socket: WebSocket, mut sink: PlaySink, state: AppState) {
    let shutdown = state.shutdown_signal();
    tokio::pin!(shutdown);
    // Logged at the end: a talk of only silence (peak near 0) means the
    // browser captured the wrong input, not that the speaker failed.
    let (mut frames, mut peak) = (0u32, 0u16);
    let ending = loop {
        let message = tokio::select! {
            message = tokio::time::timeout(TALK_IDLE_TIMEOUT, socket.recv()) => message,
            () = &mut shutdown => break Some((1001, "shutting down")),
        };
        let message = match message {
            Err(_) => break Some((CLOSE_IDLE, "idle")),
            Ok(None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => break None,
            Ok(Some(Ok(message))) => message,
        };
        match message {
            Message::Binary(bytes) => {
                if audio_proto::check_payload_len(bytes.len()).is_err() {
                    counter!("kamoder_web_audio_talk_bad_frames_total").increment(1);
                    continue;
                }
                let frame = Frame {
                    ts_ms: now_ms(),
                    samples: bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&pair| i16::from_le_bytes(pair))
                        .collect(),
                };
                frames += 1;
                peak = frame
                    .samples
                    .iter()
                    .map(|s| s.unsigned_abs())
                    .fold(peak, u16::max);
                if let Err(error) = sink.send(&frame).await {
                    tracing::warn!(%error, "talk: the audio service stopped taking frames");
                    break Some((CLOSE_UNAVAILABLE, "audio_unavailable"));
                }
            }
            Message::Text(text) if text.as_str() == "end" => break Some((1000, "end")),
            Message::Text(_) => {
                counter!("kamoder_web_audio_talk_bad_frames_total").increment(1);
            }
            // Pings are answered by tungstenite; they do count as activity.
            Message::Ping(_) | Message::Pong(_) | Message::Close(_) => {}
        }
    };
    // Release the speaker first: the amp tail starts now, not after the
    // close handshake.
    let _ = sink.finish().await;
    tracing::info!(frames, peak, "talk ended");
    if let Some((code, reason)) = ending {
        close(socket, code, reason).await;
    }
}

/// `GET /api/audio/listen?token=` (any user) — live microphone WebSocket.
async fn listen(
    State(state): State<AppState>,
    StreamUser(_user): StreamUser,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let ws = upgrade(ws)?;
    let Ok(permit) = state.audio_listeners.clone().try_acquire_owned() else {
        return Ok(ws.on_upgrade(|socket| close(socket, CLOSE_TOO_MANY, "too many listeners")));
    };
    let capture = client(&state).capture().await;
    Ok(ws.on_upgrade(move |socket| async move {
        let _permit = permit;
        match capture {
            Ok(capture) => listen_session(socket, capture, state).await,
            Err(error) => {
                tracing::debug!(%error, "listen refused");
                let (code, reason) = close_for(&error);
                close(socket, code, reason).await;
            }
        }
    }))
}

/// Aborts the capture reader however the session ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn listen_session(
    mut socket: WebSocket,
    mut capture: audio_proto::CaptureStream,
    state: AppState,
) {
    let (tx, mut rx) = mpsc::channel::<Bytes>(LISTEN_QUEUE);
    // Drains the capture socket at the service's pace whatever the browser
    // does; dropping `tx` (end or error) tells the loop below.
    let _reader = AbortOnDrop(tokio::spawn(async move {
        loop {
            let frame = match capture.next_frame().await {
                Ok(Some(frame)) => frame,
                Ok(None) => return,
                Err(error) => {
                    tracing::debug!(%error, "listen: capture ended");
                    return;
                }
            };
            let mut pcm = Vec::with_capacity(frame.samples.len() * 2);
            for sample in &frame.samples {
                pcm.extend_from_slice(&sample.to_le_bytes());
            }
            match tx.try_send(Bytes::from(pcm)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    counter!("kamoder_web_audio_listen_dropped_frames_total").increment(1);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            }
        }
    }));

    let shutdown = state.shutdown_signal();
    tokio::pin!(shutdown);
    let ending = loop {
        tokio::select! {
            pcm = rx.recv() => {
                let Some(pcm) = pcm else {
                    break Some((CLOSE_UNAVAILABLE, "audio_unavailable"));
                };
                let sent = tokio::time::timeout(
                    LISTEN_SEND_TIMEOUT,
                    socket.send(Message::Binary(pcm)),
                )
                .await;
                if !matches!(sent, Ok(Ok(()))) {
                    break None;
                }
            }
            message = socket.recv() => match message {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break None,
                Some(Ok(Message::Text(text))) if text.as_str() == "end" => break Some((1000, "end")),
                Some(Ok(_)) => {}
            },
            () = &mut shutdown => break Some((1001, "shutting down")),
        }
    };
    if let Some((code, reason)) = ending {
        close(socket, code, reason).await;
    }
}
