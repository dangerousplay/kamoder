//! Video encoding (H.264, H.265 and MJPEG).
//!
//! Two modes, both wrapped here:
//!
//! * **single frame** — [`Encoder::encode_frame`] takes one YUV buffer, used
//!   for JPEG snapshots;
//! * **stream** — [`Encoder::request_stream`] binds the encoder to a
//!   [`VideoInput`] and the hardware pushes encoded frames into a queue that
//!   [`VideoStreamHandle::get_stream`] drains.
//!
//! The stream handle borrows both the input and the encoder, so neither can be
//! closed while encoding is running — which in C is a use-after-free that shows
//! up as a kernel oops in the ISP driver.

use crate::error::{check, Error, Result};
use crate::vi::VideoInput;
use std::ffi::c_void;

/// Encoder output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputType {
    H264,
    Mjpeg,
    H265,
}

impl OutputType {
    fn as_raw(self) -> anyka_sys::encode_output_type::Type {
        match self {
            OutputType::H264 => anyka_sys::encode_output_type::H264_ENC_TYPE,
            OutputType::Mjpeg => anyka_sys::encode_output_type::MJPEG_ENC_TYPE,
            OutputType::H265 => anyka_sys::encode_output_type::HEVC_ENC_TYPE,
        }
    }
}

/// H.264/H.265 profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Main,
    High,
    Base,
    HevcMain,
    HevcMainStill,
}

impl Profile {
    fn as_raw(self) -> anyka_sys::profile_mode::Type {
        match self {
            Profile::Main => anyka_sys::profile_mode::PROFILE_MAIN,
            Profile::High => anyka_sys::profile_mode::PROFILE_HIGH,
            Profile::Base => anyka_sys::profile_mode::PROFILE_BASE,
            Profile::HevcMain => anyka_sys::profile_mode::PROFILE_HEVC_MAIN,
            Profile::HevcMainStill => anyka_sys::profile_mode::PROFILE_HEVC_MAIN_STILL,
        }
    }
}

/// Rate control mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitrateMode {
    Cbr,
    Vbr,
}

impl BitrateMode {
    fn as_raw(self) -> anyka_sys::bitrate_ctrl_mode::Type {
        match self {
            BitrateMode::Cbr => anyka_sys::bitrate_ctrl_mode::BR_MODE_CBR,
            BitrateMode::Vbr => anyka_sys::bitrate_ctrl_mode::BR_MODE_VBR,
        }
    }
}

/// Which encoder slot to use. The SDK keeps one hardware context per group, so
/// two encoders in the same group cannot run at the same time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeGroup {
    /// Local recording.
    Record,
    /// Main channel, network.
    MainNet,
    /// Sub channel, network.
    SubNet,
    /// Still pictures (JPEG).
    Picture,
}

impl EncodeGroup {
    fn as_raw(self) -> anyka_sys::encode_group_type::Type {
        match self {
            EncodeGroup::Record => anyka_sys::encode_group_type::ENCODE_RECORD,
            EncodeGroup::MainNet => anyka_sys::encode_group_type::ENCODE_MAINCHN_NET,
            EncodeGroup::SubNet => anyka_sys::encode_group_type::ENCODE_SUBCHN_NET,
            EncodeGroup::Picture => anyka_sys::encode_group_type::ENCODE_PICTURE,
        }
    }
}

/// Encoder configuration, mirroring `struct encode_param`.
#[derive(Debug, Clone, Copy)]
pub struct EncodeParam {
    /// Must be a multiple of 4; the hardware silently corrupts other widths.
    pub width: u32,
    /// Must be a multiple of 4.
    pub height: u32,
    /// Lower bound on the quantiser, 20-25 in the vendor presets.
    pub min_qp: i32,
    /// Upper bound on the quantiser, 45-51 in the vendor presets.
    pub max_qp: i32,
    pub fps: i32,
    /// Distance between I-frames, in frames.
    pub gop_len: i32,
    /// Target bitrate in kbps.
    pub kbps: i32,
    pub profile: Profile,
    /// Which ISP channel feeds the encoder.
    pub channel: crate::vi::VideoChannel,
    pub group: EncodeGroup,
    pub bitrate_mode: BitrateMode,
    pub output: OutputType,
}

impl Default for EncodeParam {
    fn default() -> Self {
        Self {
            width: 640,
            height: 480,
            min_qp: 20,
            max_qp: 51,
            fps: 25,
            gop_len: 50,
            kbps: 1000,
            profile: Profile::Main,
            channel: crate::vi::VideoChannel::Sub,
            group: EncodeGroup::MainNet,
            bitrate_mode: BitrateMode::Cbr,
            output: OutputType::H264,
        }
    }
}

impl EncodeParam {
    fn to_raw(self) -> Result<anyka_sys::encode_param> {
        if !self.width.is_multiple_of(4) || !self.height.is_multiple_of(4) {
            return Err(Error::invalid(
                "ak_venc_open",
                "width and height must be multiples of 4",
            ));
        }
        let use_chn = match self.channel {
            crate::vi::VideoChannel::Main => anyka_sys::encode_use_chn::ENCODE_MAIN_CHN,
            crate::vi::VideoChannel::Sub => anyka_sys::encode_use_chn::ENCODE_SUB_CHN,
        };
        let mut raw = anyka_sys::encode_param::default();
        raw.width = self.width as std::os::raw::c_ulong;
        raw.height = self.height as std::os::raw::c_ulong;
        raw.minqp = self.min_qp as std::os::raw::c_long;
        raw.maxqp = self.max_qp as std::os::raw::c_long;
        raw.fps = self.fps as std::os::raw::c_long;
        raw.goplen = self.gop_len as std::os::raw::c_long;
        raw.bps = self.kbps as std::os::raw::c_long;
        raw.profile = self.profile.as_raw();
        raw.use_chn = use_chn;
        raw.enc_grp = self.group.as_raw();
        raw.br_mode = self.bitrate_mode.as_raw();
        raw.enc_out_type = self.output.as_raw();
        Ok(raw)
    }
}

/// An open video encoder.
#[derive(Debug)]
pub struct Encoder {
    handle: *mut c_void,
}

// SAFETY: the encoder handle is only used through `&self`/`&mut self` here,
// and the vendor demos move encoder handles into worker threads. The SDK
// serialises the underlying hardware context internally.
unsafe impl Send for Encoder {}
unsafe impl Sync for Encoder {}

impl Encoder {
    /// Opens an encoder with the given configuration.
    pub fn open(param: EncodeParam) -> Result<Self> {
        let raw = param.to_raw()?;
        // SAFETY: `raw` is read-only for the call and fully initialised.
        let handle = unsafe { anyka_sys::ak_venc_open(&raw) };
        if handle.is_null() {
            return Err(Error::null_handle("ak_venc_open"));
        }
        Ok(Self { handle })
    }

    /// The raw `void *` handle, for SDK calls this crate does not wrap yet.
    pub fn as_ptr(&self) -> *mut c_void {
        self.handle
    }

    /// Encodes a single YUV frame, appending the result to `out`.
    ///
    /// This is the path used for JPEG snapshots. The SDK allocates the output
    /// with `malloc` and hands ownership to the caller, so the bytes are copied
    /// into `out` and the SDK buffer is freed before returning — which is the
    /// leak the C `jpeg_snapshot` demo works around with a double buffer.
    pub fn encode_frame(&self, yuv: &[u8], out: &mut Vec<u8>) -> Result<usize> {
        let mut stream = anyka_sys::video_stream::default();
        // SAFETY: `yuv` is a live slice passed as a read-only pointer with its
        // real length; `stream` is a live out-parameter.
        let ret = unsafe {
            anyka_sys::ak_venc_send_frame(
                self.handle,
                yuv.as_ptr(),
                yuv.len() as std::os::raw::c_uint,
                &mut stream,
            )
        };
        if ret < 0 {
            return Err(Error::from_sdk("ak_venc_send_frame"));
        }
        if stream.data.is_null() || stream.len == 0 {
            return Ok(0);
        }
        let len = stream.len as usize;
        // SAFETY: on success the SDK guarantees `data[..len]` is initialised,
        // and that the buffer came from `malloc` and is ours to free.
        unsafe {
            out.extend_from_slice(std::slice::from_raw_parts(stream.data, len));
            libc::free(stream.data as *mut c_void);
        }
        Ok(len)
    }

    /// Binds this encoder to a video input and starts streaming.
    ///
    /// The returned handle borrows both, so capture cannot be torn down from
    /// under a running encode.
    pub fn request_stream<'a>(&'a self, vi: &'a VideoInput) -> Result<VideoStreamHandle<'a>> {
        // SAFETY: both handles are valid and outlive the returned borrow.
        let handle = unsafe { anyka_sys::ak_venc_request_stream(vi.as_ptr(), self.handle) };
        if handle.is_null() {
            return Err(Error::null_handle("ak_venc_request_stream"));
        }
        Ok(VideoStreamHandle {
            handle,
            _encoder: self,
        })
    }

    /// Forces the next encoded frame to be an I-frame.
    pub fn force_iframe(&self) -> Result<()> {
        // SAFETY: valid handle.
        let ret = unsafe { anyka_sys::ak_venc_set_iframe(self.handle) };
        check("ak_venc_set_iframe", ret)
    }

    /// Changes the target bitrate, in kbps.
    pub fn set_bitrate(&self, kbps: i32) -> Result<()> {
        // SAFETY: valid handle, plain integer.
        let ret = unsafe { anyka_sys::ak_venc_set_rc(self.handle, kbps) };
        check("ak_venc_set_rc", ret)
    }

    /// Sets the MJPEG quality level, 0 (best) to 9 (smallest).
    pub fn set_mjpeg_quality(&self, level: i32) -> Result<()> {
        if !(0..=9).contains(&level) {
            return Err(Error::invalid(
                "ak_venc_set_mjpeg_qlevel",
                "quality level must be in 0..=9",
            ));
        }
        // SAFETY: valid handle, validated integer.
        let ret = unsafe { anyka_sys::ak_venc_set_mjpeg_qlevel(self.handle, level) };
        check("ak_venc_set_mjpeg_qlevel", ret)
    }

    /// Encoder frame rate, as opposed to the sensor rate.
    pub fn fps(&self) -> i32 {
        // SAFETY: valid handle.
        unsafe { anyka_sys::ak_venc_get_fps(self.handle) }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: valid handle; any stream borrowing it has already been
        // cancelled, because the borrow checker required it to drop first.
        unsafe { anyka_sys::ak_venc_close(self.handle) };
    }
}

/// A running encode session, produced by [`Encoder::request_stream`].
#[derive(Debug)]
pub struct VideoStreamHandle<'a> {
    handle: *mut c_void,
    _encoder: &'a Encoder,
}

// SAFETY: the vendor `venc` demo hands the stream handle to a worker thread
// that owns all `get_stream`/`release_stream` calls for it. Sharing across
// threads without exclusive access is not sound, so only `Send` is claimed.
unsafe impl Send for VideoStreamHandle<'_> {}

impl VideoStreamHandle<'_> {
    /// Takes the next encoded frame, if one is queued.
    ///
    /// Returns `Err` when the queue is empty — the vendor API uses the same
    /// error path for "no data yet", so callers poll with a short sleep.
    pub fn get_stream(&self) -> Result<VideoStream<'_>> {
        let mut raw = anyka_sys::video_stream::default();
        // SAFETY: valid handle, live out-parameter. On success the SDK owns
        // `raw.data` until `ak_venc_release_stream`, which `VideoStream::drop`
        // guarantees.
        let ret = unsafe { anyka_sys::ak_venc_get_stream(self.handle, &mut raw) };
        check("ak_venc_get_stream", ret)?;
        Ok(VideoStream { owner: self, raw })
    }

    /// Current measured bitrate and frame rate.
    pub fn rate_stat(&self) -> Result<RateStat> {
        let mut raw = anyka_sys::venc_rate_stat::default();
        // SAFETY: valid handle, live out-parameter.
        let ret = unsafe { anyka_sys::ak_venc_get_rate_stat(self.handle, &mut raw) };
        check("ak_venc_get_rate_stat", ret)?;
        Ok(RateStat {
            kbps: raw.bps,
            fps: raw.fps,
            gop: raw.gop,
        })
    }
}

impl Drop for VideoStreamHandle<'_> {
    fn drop(&mut self) {
        // SAFETY: valid handle; no `VideoStream` can still borrow it here.
        unsafe { anyka_sys::ak_venc_cancel_stream(self.handle) };
    }
}

/// Measured encoder output rate.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateStat {
    pub kbps: i32,
    pub fps: f32,
    pub gop: i32,
}

/// One encoded frame, borrowed from the SDK's stream queue.
#[derive(Debug)]
pub struct VideoStream<'h> {
    owner: &'h VideoStreamHandle<'h>,
    raw: anyka_sys::video_stream,
}

impl VideoStream<'_> {
    /// The encoded bytes (one NAL-unit sequence, or one JPEG).
    pub fn data(&self) -> &[u8] {
        if self.raw.data.is_null() || self.raw.len == 0 {
            return &[];
        }
        // SAFETY: the SDK guarantees the buffer stays valid until
        // `ak_venc_release_stream`, which cannot run before this borrow ends.
        unsafe { std::slice::from_raw_parts(self.raw.data, self.raw.len as usize) }
    }

    /// Whether this frame is a keyframe.
    pub fn is_keyframe(&self) -> bool {
        self.raw.frame_type == anyka_sys::video_frame_type::FRAME_TYPE_I
            || self.raw.frame_type == anyka_sys::video_frame_type::FRAME_TYPE_PI
    }

    /// Timestamp in milliseconds since boot.
    pub fn timestamp_ms(&self) -> u64 {
        self.raw.ts
    }

    /// Sequence number as counted by the encoder.
    pub fn sequence(&self) -> u64 {
        self.raw.seq_no as u64
    }
}

impl Drop for VideoStream<'_> {
    fn drop(&mut self) {
        // SAFETY: released exactly once, on the handle that produced it.
        unsafe { anyka_sys::ak_venc_release_stream(self.owner.handle, &mut self.raw) };
    }
}
