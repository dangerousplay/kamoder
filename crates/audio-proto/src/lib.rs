//! Protocol of the audio service socket, G.711, and an async client.
//!
//! The `audio` service is the only owner of the codec (D-A1): it captures the
//! microphone, plays to the speaker and drives the amplifier. It serves
//! [`DEFAULT_SOCKET`] (mode 0600); `rtsp`, `web` and `onvif` are clients.
//! This crate holds the wire types, the frame format, the [`g711`] codecs and
//! the client, and nothing that touches `/dev/pcm*`, so a client never links
//! a hardware backend (the same split as `ptz-proto`, `lights-proto` and
//! `video-proto`).
//!
//! # Handshake (D-A4)
//!
//! One socket, op first. The client sends one [`Request`] as a JSON line; the
//! service answers with one [`Response`] JSON line. `status` and `set` end
//! there (the connection may carry further requests). `capture` and `play`
//! are answered with `streaming` (or an error) and the connection then
//! switches to binary [`Frame`]s for good: from the service for `capture`,
//! to the service for `play`. Closing the connection ends the stream. Only
//! one talker holds the speaker; a second `play` gets `busy` (AUD-04).
//!
//! ```text
//! -> {"op":"status"}
//! <- {"result":"status","devices":[{"id":"mic","kind":"input","name":"Microphone",
//!     "state":"active","rate":8000,"gain":5,"gain_range":[0,7],"error":null}, ...]}
//! -> {"op":"set","device":"speaker","gain":4}
//! <- {"result":"ok"}
//! -> {"op":"set","device":"speaker","gain":99}
//! <- {"result":"error","code":"out_of_range","message":"gain 99 outside 0..=7"}
//!
//! -> {"op":"capture"}
//! <- {"result":"streaming","rate":8000,"channels":1,"frame_samples":160}
//! <- [frame][frame][frame]...                       (every 20 ms)
//!
//! -> {"op":"play","client":"web"}
//! <- {"result":"streaming","rate":8000,"channels":1,"frame_samples":160}
//! -> [frame][frame][frame]...
//!
//! -> {"op":"play","client":"onvif"}                 (while web talks)
//! <- {"result":"error","code":"busy","message":"speaker held by web"}
//! ```
//!
//! # Frames (D-A3)
//!
//! Linear S16LE, mono, [`RATE`] Hz. The service sends [`FRAME_SAMPLES`]
//! samples (20 ms, [`FRAME_BYTES`] bytes) per frame; a player may send any
//! even length up to [`MAX_FRAME_BYTES`]. Each frame carries the
//! `video-proto` record header, so both sockets share one parser:
//!
//! ```text
//! [len: u32 BE][flags: u8][ts_ms: u64 BE][len bytes of S16LE samples]
//!
//!    len     payload bytes, even, 2..=MAX_FRAME_BYTES
//!    flags   0 (no flag is defined for audio; readers ignore it)
//!    ts_ms   capture time of the first sample, ms, monotonic, arbitrary zero
//! ```
//!
//! Consumers compand themselves: `rtsp` encodes A-law (PCMA) with
//! [`g711::alaw_encode`], `web` passes PCM through to the browser.

pub mod g711;

use serde::{Deserialize, Serialize};
use snafu::{ensure, ResultExt, Snafu};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use video_proto::{PacketHeader, HEADER_LEN};

/// Where the audio service listens unless told otherwise (`/tmp` is a tmpfs).
pub const DEFAULT_SOCKET: &str = "/tmp/kamoder-audio.sock";

/// Sample rate on the socket, Hz. G.711 (PCMA) is defined at 8 kHz, so
/// nothing downstream has to resample (D-A3, D-A5).
pub const RATE: u32 = 8000;

/// Channels on the socket: the camera has one microphone and one speaker.
pub const CHANNELS: u8 = 1;

/// Samples per frame the service sends: 20 ms at [`RATE`], the usual RTP
/// packetisation time for PCMA.
pub const FRAME_SAMPLES: u32 = 160;

/// Bytes per frame the service sends (S16LE mono).
pub const FRAME_BYTES: usize = FRAME_SAMPLES as usize * 2;

/// Largest frame payload either side accepts, bytes: 2048 samples, 256 ms.
/// Anything larger means the stream is out of sync.
pub const MAX_FRAME_BYTES: usize = 4096;

/// Longest accepted request line, in bytes, excluding the newline.
pub const MAX_LINE: usize = 1024;

/// Longest response line the client accepts (`status` lists every device).
pub const MAX_RESPONSE: usize = 16 * 1024;

/// Default timeout of [`Client`]: connect plus one request/response, and
/// each frame read or write after a stream started. Capture delivers a frame
/// every 20 ms, so a silent service for this long is stuck.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Direction of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// Microphone (capture).
    Input,
    /// Speaker (playback).
    Output,
}

/// What a device is doing (AUD-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    /// Capturing for at least one subscriber, or playing for a talker.
    Active,
    /// Open on demand; nobody uses it now.
    Idle,
    /// The last open or ioctl failed; see [`Device::error`]. Retried with
    /// backoff (AUD-05).
    Error,
}

/// One device, as `status` reports it. Every field is always present;
/// `Option`s are `null`, never omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Stable id from the board profile (`mic`, `speaker`), used by `set`.
    pub id: String,
    pub kind: DeviceKind,
    /// Human-readable name from the board profile.
    pub name: String,
    pub state: DeviceState,
    /// Rate on the socket, Hz (the hardware may run at another and be
    /// resampled, D-A5).
    pub rate: u32,
    /// Current gain (input) or volume (output), driver units; `null` when
    /// the device has no gain control.
    pub gain: Option<u32>,
    /// Accepted gain values, inclusive: `[min, max]`.
    pub gain_range: [u32; 2],
    /// Why the device is in [`DeviceState::Error`].
    pub error: Option<String>,
}

/// Who asks to play, for the busy message, logs and metrics labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Talker {
    /// Push-to-talk in the web UI.
    Web,
    /// ONVIF / RTSP backchannel.
    Onvif,
    /// Anything else (tools, tests).
    Other,
}

impl Talker {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Talker::Web => "web",
            Talker::Onvif => "onvif",
            Talker::Other => "other",
        }
    }
}

impl std::fmt::Display for Talker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One request, as sent by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Every [`Device`].
    Status,
    /// Set a device's gain or volume; persisted across restarts (AUD-25).
    Set { device: String, gain: u32 },
    /// Receive microphone frames until the connection closes.
    Capture,
    /// Send speaker frames until the connection closes; one talker at a time.
    Play { client: Talker },
}

impl Request {
    /// Short name of the operation, for logs and metrics labels.
    pub fn op(&self) -> &'static str {
        match self {
            Request::Status => "status",
            Request::Set { .. } => "set",
            Request::Capture => "capture",
            Request::Play { .. } => "play",
        }
    }
}

/// Format of the frames that follow a `streaming` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamFormat {
    /// Hz.
    pub rate: u32,
    pub channels: u8,
    /// Samples per frame the service sends (a player may send other sizes).
    pub frame_samples: u32,
}

impl StreamFormat {
    /// The only format this protocol version uses (D-A3).
    pub const DEFAULT: StreamFormat = StreamFormat {
        rate: RATE,
        channels: CHANNELS,
        frame_samples: FRAME_SAMPLES,
    };
}

impl Default for StreamFormat {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One response, as sent by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// Answer to [`Request::Status`].
    Status { devices: Vec<Device> },
    /// The `set` was carried out.
    Ok,
    /// Answer to `capture`/`play`; binary frames follow. The fields sit next
    /// to `result`.
    Streaming(StreamFormat),
    /// The request was not carried out. After `capture`/`play` the service
    /// closes the connection.
    Error { code: ErrorCode, message: String },
}

/// Machine-readable reason a request failed. Suggested HTTP mapping for
/// `web`: `bad_request`/`out_of_range` → 400, `unknown_device` → 404,
/// `busy` → 409, `unavailable` → 503 (as is a connect failure,
/// `audio_unavailable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Another talker holds the speaker (AUD-04).
    Busy,
    /// `set` named a device the board does not have.
    UnknownDevice,
    /// `set` gain outside [`Device::gain_range`].
    OutOfRange,
    /// The device cannot be used now (open/ioctl failing, AUD-05).
    Unavailable,
    /// The line was not a valid request (unknown op, missing field, not
    /// JSON, longer than [`MAX_LINE`]).
    BadRequest,
}

impl ErrorCode {
    /// Wire name, for metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Busy => "busy",
            ErrorCode::UnknownDevice => "unknown_device",
            ErrorCode::OutOfRange => "out_of_range",
            ErrorCode::Unavailable => "unavailable",
            ErrorCode::BadRequest => "bad_request",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A frame that cannot be on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Snafu)]
pub enum FrameError {
    #[snafu(display("frame shorter than its {HEADER_LEN}-byte header"))]
    ShortHeader,

    #[snafu(display("frame payload of {len} bytes is not a whole number of S16 samples"))]
    OddLength { len: usize },

    #[snafu(display("frame payload of {len} bytes outside 2..={MAX_FRAME_BYTES}"))]
    BadLength { len: usize },

    #[snafu(display("frame header says {expected} payload bytes, got {actual}"))]
    LengthMismatch { expected: usize, actual: usize },
}

/// Checks a payload length from a header or a buffer: even and
/// `2..=MAX_FRAME_BYTES`. A reader calls this before allocating the payload.
pub fn check_payload_len(len: usize) -> Result<(), FrameError> {
    ensure!((2..=MAX_FRAME_BYTES).contains(&len), BadLengthSnafu { len });
    ensure!(len.is_multiple_of(2), OddLengthSnafu { len });
    Ok(())
}

/// A run of samples on the socket.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    /// Capture time of the first sample, ms (monotonic, arbitrary zero).
    pub ts_ms: u64,
    /// Mono samples at [`RATE`].
    pub samples: Vec<i16>,
}

impl Frame {
    /// The header this frame is sent with.
    pub fn header(&self) -> PacketHeader {
        PacketHeader {
            len: (self.samples.len() * 2) as u32,
            keyframe: false,
            timestamp_ms: self.ts_ms,
        }
    }

    /// Header plus S16LE payload, appended to `out` (reuse it to avoid an
    /// allocation per frame). Does not validate; see [`Frame::encode`].
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.reserve(HEADER_LEN + self.samples.len() * 2);
        out.extend_from_slice(&self.header().encode());
        for s in &self.samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }

    /// Header plus S16LE payload, ready to write.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// A frame from its decoded header and its payload bytes.
    pub fn from_parts(header: &PacketHeader, payload: &[u8]) -> Result<Frame, FrameError> {
        check_payload_len(payload.len())?;
        ensure!(
            header.len as usize == payload.len(),
            LengthMismatchSnafu {
                expected: header.len as usize,
                actual: payload.len(),
            }
        );
        Ok(Frame {
            ts_ms: header.timestamp_ms,
            samples: payload
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&b| i16::from_le_bytes(b))
                .collect(),
        })
    }

    /// One whole record (header plus exactly its payload), as
    /// [`Frame::encode`] writes it.
    pub fn decode(record: &[u8]) -> Result<Frame, FrameError> {
        let (head, payload) = record
            .split_first_chunk::<HEADER_LEN>()
            .ok_or(FrameError::ShortHeader)?;
        Frame::from_parts(&PacketHeader::decode(head), payload)
    }
}

/// Errors from [`Client`] and its streams.
#[derive(Debug, Snafu)]
#[non_exhaustive]
pub enum ClientError {
    #[snafu(display("cannot connect to {}: {source}", path.display()))]
    Connect {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("cannot encode the request: {source}"))]
    Encode { source: serde_json::Error },

    #[snafu(display("I/O error on the audio socket: {source}"))]
    Io { source: std::io::Error },

    #[snafu(display("the audio service closed the connection without answering"))]
    Closed,

    #[snafu(display("the audio service's response exceeds {MAX_RESPONSE} bytes"))]
    ResponseTooLong,

    #[snafu(display("cannot decode the audio service's response: {source}"))]
    Decode { source: serde_json::Error },

    #[snafu(display("no response from the audio service within {} ms", timeout.as_millis()))]
    Timeout { timeout: Duration },

    /// Another talker holds the speaker ([`ErrorCode::Busy`]).
    #[snafu(display("the speaker is busy: {message}"))]
    Busy { message: String },

    /// The service refused the request with any other [`ErrorCode`].
    #[snafu(display("the audio service refused the request ({code}): {message}"))]
    Rejected { code: ErrorCode, message: String },

    #[snafu(display("unexpected `{result}` answer to `{op}`"))]
    Unexpected {
        op: &'static str,
        result: &'static str,
    },

    #[snafu(display("bad frame on the audio socket: {source}"))]
    BadFrame { source: FrameError },
}

/// Client for the audio socket. `status`/`set` use one connection per
/// request; `capture`/`play` keep theirs for the stream.
#[derive(Debug, Clone)]
pub struct Client {
    path: PathBuf,
    timeout: Duration,
}

impl Default for Client {
    fn default() -> Self {
        Self::new(DEFAULT_SOCKET)
    }
}

impl Client {
    /// A client for the socket at `path`, with [`DEFAULT_TIMEOUT`].
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Replaces the timeout (handshake, and each frame read/write).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The socket this client talks to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sends one request and waits for its response, raw. A
    /// [`Response::Error`] is a successful round trip and is returned as
    /// `Ok`. For `capture`/`play` use [`Client::capture`]/[`Client::play`],
    /// which keep the connection.
    pub async fn request(&self, req: &Request) -> Result<Response, ClientError> {
        let (_, response) = self.handshake(req).await?;
        Ok(response)
    }

    /// Every device ([`Request::Status`]).
    pub async fn status(&self) -> Result<Vec<Device>, ClientError> {
        let req = Request::Status;
        match self.request(&req).await? {
            Response::Status { devices } => Ok(devices),
            other => Err(refused(&req, other)),
        }
    }

    /// Sets a device's gain or volume ([`Request::Set`]).
    pub async fn set(&self, device: impl Into<String>, gain: u32) -> Result<(), ClientError> {
        let req = Request::Set {
            device: device.into(),
            gain,
        };
        match self.request(&req).await? {
            Response::Ok => Ok(()),
            other => Err(refused(&req, other)),
        }
    }

    /// Subscribes to the microphone ([`Request::Capture`]).
    pub async fn capture(&self) -> Result<CaptureStream, ClientError> {
        let req = Request::Capture;
        match self.handshake(&req).await? {
            (reader, Response::Streaming(format)) => Ok(CaptureStream {
                reader,
                format,
                timeout: self.timeout,
            }),
            (_, other) => Err(refused(&req, other)),
        }
    }

    /// Takes the speaker ([`Request::Play`]). [`ClientError::Busy`] when
    /// another talker holds it.
    pub async fn play(&self, talker: Talker) -> Result<PlaySink, ClientError> {
        let req = Request::Play { client: talker };
        match self.handshake(&req).await? {
            (reader, Response::Streaming(format)) => Ok(PlaySink {
                // The service sends nothing after `streaming`, so no
                // buffered bytes are lost here.
                stream: reader.into_inner(),
                format,
                timeout: self.timeout,
                buf: Vec::with_capacity(HEADER_LEN + MAX_FRAME_BYTES),
            }),
            (_, other) => Err(refused(&req, other)),
        }
    }

    /// Connects, sends `req`, reads one response line; under the timeout.
    async fn handshake(
        &self,
        req: &Request,
    ) -> Result<(BufReader<UnixStream>, Response), ClientError> {
        let timeout = self.timeout;
        tokio::time::timeout(timeout, handshake(&self.path, req))
            .await
            .unwrap_or(Err(ClientError::Timeout { timeout }))
    }
}

/// Turns an error or mismatched response into the matching [`ClientError`].
fn refused(req: &Request, response: Response) -> ClientError {
    match response {
        Response::Error {
            code: ErrorCode::Busy,
            message,
        } => ClientError::Busy { message },
        Response::Error { code, message } => ClientError::Rejected { code, message },
        other => ClientError::Unexpected {
            op: req.op(),
            result: match other {
                Response::Status { .. } => "status",
                Response::Ok => "ok",
                Response::Streaming(_) => "streaming",
                Response::Error { .. } => "error",
            },
        },
    }
}

async fn handshake(
    path: &Path,
    request: &Request,
) -> Result<(BufReader<UnixStream>, Response), ClientError> {
    let mut stream = UnixStream::connect(path)
        .await
        .context(ConnectSnafu { path })?;

    let mut line = serde_json::to_vec(request).context(EncodeSnafu)?;
    line.push(b'\n');
    stream.write_all(&line).await.context(IoSnafu)?;
    stream.flush().await.context(IoSnafu)?;

    let mut reader = BufReader::new(stream);
    let response = read_response(&mut reader).await?;
    Ok((reader, response))
}

/// Reads one response line, at most [`MAX_RESPONSE`] bytes, leaving whatever
/// follows it (frames) in `reader`.
async fn read_response<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Response, ClientError> {
    let mut buf = Vec::new();
    (&mut *reader)
        .take(MAX_RESPONSE as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await
        .context(IoSnafu)?;
    if buf.last() != Some(&b'\n') {
        ensure!(buf.len() <= MAX_RESPONSE, ResponseTooLongSnafu);
        return ClosedSnafu.fail();
    }
    serde_json::from_slice(&buf).context(DecodeSnafu)
}

/// Microphone frames from a `capture` connection. Dropping it unsubscribes.
#[derive(Debug)]
pub struct CaptureStream {
    reader: BufReader<UnixStream>,
    format: StreamFormat,
    timeout: Duration,
}

impl CaptureStream {
    /// What the service announced in its `streaming` reply.
    pub fn format(&self) -> StreamFormat {
        self.format
    }

    /// The next frame; `None` when the service closed the connection between
    /// frames. A close inside a frame is an I/O error, a frame later than
    /// the client's timeout is [`ClientError::Timeout`].
    pub async fn next_frame(&mut self) -> Result<Option<Frame>, ClientError> {
        let timeout = self.timeout;
        tokio::time::timeout(timeout, self.read_frame())
            .await
            .unwrap_or(Err(ClientError::Timeout { timeout }))
    }

    async fn read_frame(&mut self) -> Result<Option<Frame>, ClientError> {
        // A clean end is EOF before the first header byte.
        if self.reader.fill_buf().await.context(IoSnafu)?.is_empty() {
            return Ok(None);
        }
        let mut head = [0u8; HEADER_LEN];
        self.reader.read_exact(&mut head).await.context(IoSnafu)?;
        let header = PacketHeader::decode(&head);
        check_payload_len(header.len as usize).context(BadFrameSnafu)?;
        let mut payload = vec![0u8; header.len as usize];
        self.reader
            .read_exact(&mut payload)
            .await
            .context(IoSnafu)?;
        Frame::from_parts(&header, &payload)
            .map(Some)
            .context(BadFrameSnafu)
    }
}

/// Speaker frames to a `play` connection. Dropping it releases the speaker
/// (the service keeps the amplifier on for a short tail, AUD-20).
#[derive(Debug)]
pub struct PlaySink {
    stream: UnixStream,
    format: StreamFormat,
    timeout: Duration,
    /// Reused encode buffer: no allocation per frame.
    buf: Vec<u8>,
}

impl PlaySink {
    /// What the service announced in its `streaming` reply.
    pub fn format(&self) -> StreamFormat {
        self.format
    }

    /// Writes one frame. Fails with [`ClientError::BadFrame`] before writing
    /// anything if the frame is empty or larger than [`MAX_FRAME_BYTES`].
    pub async fn send(&mut self, frame: &Frame) -> Result<(), ClientError> {
        check_payload_len(frame.samples.len() * 2).context(BadFrameSnafu)?;
        self.buf.clear();
        frame.encode_into(&mut self.buf);
        let timeout = self.timeout;
        tokio::time::timeout(timeout, self.stream.write_all(&self.buf))
            .await
            .map_err(|_| ClientError::Timeout { timeout })?
            .context(IoSnafu)
    }

    /// Ends the talk cleanly: shuts down the write side so the service sees
    /// EOF after the last frame.
    pub async fn finish(mut self) -> Result<(), ClientError> {
        self.stream.shutdown().await.context(IoSnafu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::UnixListener;

    fn mic() -> Device {
        Device {
            id: "mic".into(),
            kind: DeviceKind::Input,
            name: "Microphone".into(),
            state: DeviceState::Active,
            rate: 8000,
            gain: Some(5),
            gain_range: [0, 7],
            error: None,
        }
    }

    #[test]
    fn frame_constants_agree() {
        assert_eq!(FRAME_BYTES, 320);
        assert_eq!(FRAME_SAMPLES * 1000 / RATE, 20, "20 ms frames");
        assert_eq!(StreamFormat::default(), StreamFormat::DEFAULT);
    }

    #[test]
    fn requests_use_the_documented_wire_format() {
        let cases = [
            (Request::Status, r#"{"op":"status"}"#),
            (
                Request::Set {
                    device: "speaker".into(),
                    gain: 4,
                },
                r#"{"op":"set","device":"speaker","gain":4}"#,
            ),
            (Request::Capture, r#"{"op":"capture"}"#),
            (
                Request::Play {
                    client: Talker::Web,
                },
                r#"{"op":"play","client":"web"}"#,
            ),
            (
                Request::Play {
                    client: Talker::Onvif,
                },
                r#"{"op":"play","client":"onvif"}"#,
            ),
            (
                Request::Play {
                    client: Talker::Other,
                },
                r#"{"op":"play","client":"other"}"#,
            ),
        ];
        for (request, wire) in cases {
            assert_eq!(serde_json::to_string(&request).unwrap(), wire);
            let back: Request = serde_json::from_str(wire).unwrap();
            assert_eq!(back, request);
            assert_eq!(request.op(), back.op());
        }
        for talker in [Talker::Web, Talker::Onvif, Talker::Other] {
            assert_eq!(
                serde_json::to_value(talker).unwrap(),
                json!(talker.as_str())
            );
        }
    }

    #[test]
    fn malformed_requests_are_rejected() {
        for bad in [
            r#"{"op":"record"}"#,
            r#"{"op":"set","device":"mic"}"#,
            r#"{"op":"set","device":"mic","gain":-1}"#,
            r#"{"op":"play"}"#,
            r#"{"op":"play","client":"sip"}"#,
            r#"{"op":"set","device":"mic","gain":1,"rate":16000}"#,
            "not json",
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn responses_use_the_documented_wire_format() {
        let speaker = Device {
            id: "speaker".into(),
            kind: DeviceKind::Output,
            name: "Speaker".into(),
            state: DeviceState::Error,
            rate: 8000,
            gain: None,
            gain_range: [0, 0],
            error: Some("open failed".into()),
        };
        let cases = [
            (
                Response::Status {
                    devices: vec![mic(), speaker],
                },
                concat!(
                    r#"{"result":"status","devices":["#,
                    r#"{"id":"mic","kind":"input","name":"Microphone","state":"active","#,
                    r#""rate":8000,"gain":5,"gain_range":[0,7],"error":null},"#,
                    r#"{"id":"speaker","kind":"output","name":"Speaker","state":"error","#,
                    r#""rate":8000,"gain":null,"gain_range":[0,0],"error":"open failed"}]}"#
                ),
            ),
            (Response::Ok, r#"{"result":"ok"}"#),
            (
                Response::Streaming(StreamFormat::DEFAULT),
                r#"{"result":"streaming","rate":8000,"channels":1,"frame_samples":160}"#,
            ),
            (
                Response::Error {
                    code: ErrorCode::Busy,
                    message: "speaker held by web".into(),
                },
                r#"{"result":"error","code":"busy","message":"speaker held by web"}"#,
            ),
        ];
        for (response, wire) in cases {
            assert_eq!(serde_json::to_string(&response).unwrap(), wire);
            let back: Response = serde_json::from_str(wire).unwrap();
            assert_eq!(back, response);
        }
        for code in [
            ErrorCode::Busy,
            ErrorCode::UnknownDevice,
            ErrorCode::OutOfRange,
            ErrorCode::Unavailable,
            ErrorCode::BadRequest,
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), json!(code.as_str()));
        }
    }

    #[test]
    fn frames_round_trip_through_the_video_header() {
        let frame = Frame {
            ts_ms: 0x0102_0304_0506,
            samples: vec![0, 1, -1, i16::MAX, i16::MIN, 0x1234],
        };
        let bytes = frame.encode();
        assert_eq!(bytes.len(), HEADER_LEN + 12);
        assert_eq!(&bytes[..4], &12u32.to_be_bytes(), "len u32 BE");
        assert_eq!(bytes[4], 0, "no flags");
        assert_eq!(&bytes[5..13], &0x0102_0304_0506u64.to_be_bytes());
        assert_eq!(&bytes[13..17], &[0, 0, 1, 0], "S16LE");
        assert_eq!(&bytes[23..25], &[0x34, 0x12]);
        assert_eq!(Frame::decode(&bytes).unwrap(), frame);

        let full = Frame {
            ts_ms: 7,
            samples: vec![3; FRAME_SAMPLES as usize],
        };
        assert_eq!(full.encode().len(), HEADER_LEN + FRAME_BYTES);
        assert_eq!(Frame::decode(&full.encode()).unwrap(), full);
    }

    #[test]
    fn bad_frames_are_rejected() {
        let good = Frame {
            ts_ms: 1,
            samples: vec![1, 2],
        }
        .encode();
        assert_eq!(Frame::decode(&good[..5]), Err(FrameError::ShortHeader));
        assert_eq!(
            Frame::decode(&good[..good.len() - 1]),
            Err(FrameError::OddLength { len: 3 })
        );
        assert_eq!(
            Frame::decode(&good[..good.len() - 2]),
            Err(FrameError::LengthMismatch {
                expected: 4,
                actual: 2
            })
        );
        assert_eq!(
            Frame::decode(&good[..HEADER_LEN]),
            Err(FrameError::BadLength { len: 0 })
        );
        let huge = Frame {
            ts_ms: 1,
            samples: vec![0; MAX_FRAME_BYTES / 2 + 1],
        }
        .encode();
        assert_eq!(
            Frame::decode(&huge),
            Err(FrameError::BadLength {
                len: MAX_FRAME_BYTES + 2
            })
        );
        assert!(check_payload_len(MAX_FRAME_BYTES).is_ok());
    }

    /// A socket path unique to this test process and `name`.
    fn socket(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kamoder-audio-proto-{name}-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Accepts one connection, checks its request line, answers `reply`
    /// and hands back the connection.
    async fn serve_once(
        listener: &UnixListener,
        expect: &str,
        reply: Response,
    ) -> BufReader<UnixStream> {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, format!("{expect}\n"));
        let mut out = serde_json::to_vec(&reply).unwrap();
        out.push(b'\n');
        reader.get_mut().write_all(&out).await.unwrap();
        reader
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_reads_status_and_set_errors() {
        let path = socket("status");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let devices = vec![mic()];
            serve_once(
                &listener,
                r#"{"op":"status"}"#,
                Response::Status { devices },
            )
            .await;
            serve_once(
                &listener,
                r#"{"op":"set","device":"mic","gain":9}"#,
                Response::Error {
                    code: ErrorCode::OutOfRange,
                    message: "gain 9 outside 0..=7".into(),
                },
            )
            .await;
        });

        let client = Client::new(&path);
        assert_eq!(client.status().await.unwrap(), vec![mic()]);
        let error = client.set("mic", 9).await.unwrap_err();
        assert!(
            matches!(
                error,
                ClientError::Rejected {
                    code: ErrorCode::OutOfRange,
                    ..
                }
            ),
            "{error}"
        );
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capture_receives_frames_until_close() {
        let path = socket("capture");
        let listener = UnixListener::bind(&path).unwrap();
        let frames = [
            Frame {
                ts_ms: 1000,
                samples: vec![1; FRAME_SAMPLES as usize],
            },
            Frame {
                ts_ms: 1020,
                samples: vec![-2; FRAME_SAMPLES as usize],
            },
        ];
        let sent = frames.clone();
        let server = tokio::spawn(async move {
            let mut reader = serve_once(
                &listener,
                r#"{"op":"capture"}"#,
                Response::Streaming(StreamFormat::DEFAULT),
            )
            .await;
            // Both frames in one write: the client must split them.
            let mut out = Vec::new();
            for f in &sent {
                f.encode_into(&mut out);
            }
            reader.get_mut().write_all(&out).await.unwrap();
        });

        let mut stream = Client::new(&path).capture().await.unwrap();
        assert_eq!(stream.format(), StreamFormat::DEFAULT);
        assert_eq!(
            stream.next_frame().await.unwrap().as_ref(),
            Some(&frames[0])
        );
        assert_eq!(
            stream.next_frame().await.unwrap().as_ref(),
            Some(&frames[1])
        );
        server.await.unwrap();
        assert_eq!(stream.next_frame().await.unwrap(), None, "clean close");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn play_sends_frames_and_reports_busy() {
        let path = socket("play");
        let listener = UnixListener::bind(&path).unwrap();
        let frame = Frame {
            ts_ms: 5,
            samples: (0..FRAME_SAMPLES as i16).collect(),
        };
        let expected = frame.encode();
        let server = tokio::spawn(async move {
            let mut reader = serve_once(
                &listener,
                r#"{"op":"play","client":"web"}"#,
                Response::Streaming(StreamFormat::DEFAULT),
            )
            .await;

            // Second talker while the first one holds the speaker.
            serve_once(
                &listener,
                r#"{"op":"play","client":"onvif"}"#,
                Response::Error {
                    code: ErrorCode::Busy,
                    message: "speaker held by web".into(),
                },
            )
            .await;

            let mut got = Vec::new();
            reader.read_to_end(&mut got).await.unwrap();
            assert_eq!(got, expected);
        });

        let client = Client::new(&path);
        let mut sink = client.play(Talker::Web).await.unwrap();
        assert_eq!(sink.format(), StreamFormat::DEFAULT);
        let error = client.play(Talker::Onvif).await.unwrap_err();
        assert!(matches!(error, ClientError::Busy { .. }), "{error}");

        let empty = Frame::default();
        assert!(matches!(
            sink.send(&empty).await,
            Err(ClientError::BadFrame { .. })
        ));
        sink.send(&frame).await.unwrap();
        sink.finish().await.unwrap();
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_reports_a_missing_service() {
        let path = socket("none");
        let error = Client::new(&path).status().await.unwrap_err();
        assert!(matches!(error, ClientError::Connect { .. }), "{error}");
    }
}
