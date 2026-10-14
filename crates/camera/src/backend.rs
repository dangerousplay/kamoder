//! What the camera service needs from a board's video hardware (CAM-01).
//!
//! The service (`service::capture`) only knows these traits: a [`Backend`]
//! brings every sensor of the board up at once and hands out a [`Session`]
//! that lends pictures in place; an [`Encoder`] turns one picture into H.264
//! access units. Board-specific code (the AV100 ISP and `/dev/venc`, a test
//! pattern, the host fakes) implements them; nothing here touches a device.
//!
//! Two hardware constraints shape the API:
//! - sensors start together (AV100 dual mode: neither ISP delivers a picture
//!   until both stream), so there is no per-sensor start, only
//!   [`Backend::start`] for all of them, and dropping the session tears all
//!   of them down;
//! - an encoder channel is created once and kept for the life of the process
//!   (the AV100 driver oopses when re-creating channels on fragmented memory,
//!   CAM-03), so encoders live outside the session and survive its retries.

use std::time::Duration;

/// Error of any backend or encoder; the service only reports it.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Layout of a lent picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PixelFormat {
    /// Y plane (`width*height`, stride = width) then interleaved CbCr.
    #[default]
    Nv12,
}

/// A picture still in the backend's buffer (no copy).
#[derive(Debug, Clone, Copy)]
pub struct FrameRef<'a> {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: &'a [u8],
    /// Physical address of `data` when the buffer is DMA memory a hardware
    /// encoder can read directly (AV100); `None` for ordinary memory.
    pub phys: Option<u32>,
    /// Capture time, ms.
    pub timestamp_ms: u64,
}

/// One NV12 picture copied out of a capture buffer.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Y plane (`width*height`, stride = width) then interleaved CbCr.
    pub data: Vec<u8>,
    /// Driver timestamp in milliseconds.
    pub timestamp_ms: u64,
}

impl Frame {
    /// Copies a lent picture out.
    pub fn copy_of(f: &FrameRef<'_>) -> Frame {
        Frame {
            width: f.width,
            height: f.height,
            data: f.data.to_vec(),
            timestamp_ms: f.timestamp_ms,
        }
    }

    /// Lends this picture like a capture buffer (no physical address).
    pub fn view(&self) -> FrameRef<'_> {
        FrameRef {
            width: self.width,
            height: self.height,
            format: PixelFormat::Nv12,
            data: &self.data,
            phys: None,
            timestamp_ms: self.timestamp_ms,
        }
    }
}

/// One sensor as the service reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SensorInfo {
    /// Board-level sensor number (device node `N`, metric label).
    pub index: u32,
    /// Size of the pictures [`Session::with_frame`] lends.
    pub width: u32,
    pub height: u32,
    /// Sensor active area, once brought up.
    pub active: Option<(u32, u32)>,
    /// Chip id, once brought up.
    pub id: Option<String>,
    /// ISP tuning in use.
    pub tuning: Option<String>,
}

/// All sensors of a board, brought up together (AV100 dual mode).
pub trait Backend: Send {
    type Session: Session;
    /// The sensors [`start`](Backend::start) brings up, as known before
    /// starting (index, picture size, tuning), one per slot.
    fn sensors(&self) -> Vec<SensorInfo>;
    /// Brings every sensor up and starts it; dropping the session tears
    /// them all down.
    fn start(&mut self) -> Result<Self::Session, BoxError>;
}

/// Running sensors.
pub trait Session {
    /// Same slots as [`Backend::sensors`], now with what bring-up learnt.
    fn sensors(&self) -> Vec<SensorInfo>;
    /// Waits up to `timeout` for the next picture of sensor `slot`
    /// (position in `sensors()`) and lends it to `f` in place.
    fn with_frame(
        &mut self,
        slot: usize,
        timeout: Duration,
        f: &mut dyn FnMut(&FrameRef<'_>) -> Result<usize, BoxError>,
    ) -> Result<usize, BoxError>;
}

/// One H.264 channel per sensor, created once at start-up and kept for the
/// life of the process (AV100 driver oopses on re-create: CAM-03).
pub trait Encoder: Send {
    /// Codec, profile, size, fps, GOP, target bitrate, encoder name.
    fn info(&self) -> video_proto::StreamInfo;
    /// Encodes one picture, lends each access unit to `out`, and returns the
    /// encoded size in bytes.
    fn encode(
        &mut self,
        frame: &FrameRef<'_>,
        out: &mut dyn FnMut(Packet<'_>),
    ) -> Result<usize, BoxError>;
    /// Makes the next picture an IDR.
    fn request_idr(&mut self) -> Result<(), BoxError>;
}

/// One encoded access unit (Annex-B), borrowed from the encoder.
#[derive(Debug, Clone, Copy)]
pub struct Packet<'a> {
    pub data: &'a [u8],
    pub keyframe: bool,
}
