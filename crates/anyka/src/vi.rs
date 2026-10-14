//! Video input (ISP capture).
//!
//! [`VideoInput`] owns the `ak_vi_open` handle and closes it on drop. Frames
//! come back as [`VideoFrame`] guards that call `ak_vi_release_frame` when they
//! go out of scope — the vendor libraries only keep a handful of DMA buffers,
//! so a leaked frame stalls capture within a second.

use crate::error::{check, Error, Result};
use crate::path_to_cstring;
use std::ffi::c_void;
use std::path::{Path, PathBuf};

/// Video input device. The AK3918 only has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoDevice {
    #[default]
    Dev0,
}

impl VideoDevice {
    fn as_raw(self) -> anyka_sys::video_dev_type::Type {
        match self {
            VideoDevice::Dev0 => anyka_sys::video_dev_type::VIDEO_DEV0,
        }
    }
}

/// The two YUV channels the ISP produces in parallel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoChannel {
    /// Full-size channel, capped by the sensor resolution.
    Main,
    /// Downscaled channel, capped at 640x480 by the hardware scaler.
    Sub,
}

impl VideoChannel {
    fn index(self) -> usize {
        match self {
            VideoChannel::Main => anyka_sys::video_channel::VIDEO_CHN_MAIN as usize,
            VideoChannel::Sub => anyka_sys::video_channel::VIDEO_CHN_SUB as usize,
        }
    }
}

/// Day/night ISP profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayNightMode {
    Day,
    Night,
}

impl DayNightMode {
    fn as_raw(self) -> anyka_sys::video_daynight_mode::Type {
        match self {
            DayNightMode::Day => anyka_sys::video_daynight_mode::VI_MODE_DAY,
            DayNightMode::Night => anyka_sys::video_daynight_mode::VI_MODE_NIGHT,
        }
    }
}

/// Per-channel output size plus the scaler ceiling the SDK wants alongside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resolution {
    pub width: i32,
    pub height: i32,
    pub max_width: i32,
    pub max_height: i32,
}

impl Resolution {
    /// Size with `max_*` mirroring `width`/`height`.
    pub fn new(width: i32, height: i32) -> Self {
        Self {
            width,
            height,
            max_width: width,
            max_height: height,
        }
    }

    /// Size with an explicit scaler ceiling.
    pub fn with_max(width: i32, height: i32, max_width: i32, max_height: i32) -> Self {
        Self {
            width,
            height,
            max_width,
            max_height,
        }
    }
}

/// Sensor crop window, in sensor pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Crop {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

/// Complete video input configuration: crop plus both channel resolutions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelAttr {
    pub crop: Crop,
    pub main: Resolution,
    pub sub: Resolution,
}

impl ChannelAttr {
    fn to_raw(self) -> anyka_sys::video_channel_attr {
        let mut raw = anyka_sys::video_channel_attr::default();
        raw.crop.left = self.crop.left;
        raw.crop.top = self.crop.top;
        raw.crop.width = self.crop.width;
        raw.crop.height = self.crop.height;
        for (slot, res) in [
            (VideoChannel::Main.index(), self.main),
            (VideoChannel::Sub.index(), self.sub),
        ] {
            raw.res[slot].width = res.width;
            raw.res[slot].height = res.height;
            raw.res[slot].max_width = res.max_width;
            raw.res[slot].max_height = res.max_height;
        }
        raw
    }

    fn from_raw(raw: &anyka_sys::video_channel_attr) -> Self {
        let read = |slot: usize| Resolution {
            width: raw.res[slot].width,
            height: raw.res[slot].height,
            max_width: raw.res[slot].max_width,
            max_height: raw.res[slot].max_height,
        };
        Self {
            crop: Crop {
                left: raw.crop.left,
                top: raw.crop.top,
                width: raw.crop.width,
                height: raw.crop.height,
            },
            main: read(VideoChannel::Main.index()),
            sub: read(VideoChannel::Sub.index()),
        }
    }
}

/// Environment variable overriding the ISP configuration directory.
pub const ISP_CONFIG_ENV: &str = "ANYKA_ISP_CONFIG";

/// Directories searched for the ISP sensor configuration, in order.
///
/// Firmware builds disagree about where this lives. The vendor demos hardcode
/// `/etc/jffs2/`, which does not exist on every unit — `/usr/local/` is the
/// path the C `venc_demo` carried as a fallback it never actually used, and
/// some builds only have `/rom/`.
pub const ISP_CONFIG_SEARCH: &[&str] = &["/etc/jffs2/", "/usr/local/", "/rom/", "/ipc/local/"];

/// An open video input device.
///
/// Dropping it calls `ak_vi_close`. Capture must be started explicitly with
/// [`VideoInput::capture_on`].
#[derive(Debug)]
pub struct VideoInput {
    handle: *mut c_void,
}

// SAFETY: the SDK serialises access to the vi handle internally — the vendor's
// own `rtsp` and `venc` demos hand the same handle to several worker threads
// and to the RTSP server at once. Frames are per-call buffers, so sharing the
// handle does not alias frame data.
unsafe impl Send for VideoInput {}
unsafe impl Sync for VideoInput {}

impl VideoInput {
    /// Probes the sensor and loads the matching ISP configuration.
    ///
    /// `config` is either an `isp.cfg` file or a directory to search
    /// (`/etc/jffs2/` on a stock camera). Must succeed before [`open`].
    ///
    /// [`open`]: VideoInput::open
    pub fn match_sensor(config: impl AsRef<Path>) -> Result<()> {
        let path = path_to_cstring("ak_vi_match_sensor", config.as_ref())?;
        // SAFETY: `path` is NUL-terminated and outlives the call.
        let ret = unsafe { anyka_sys::ak_vi_match_sensor(path.as_ptr()) };
        check("ak_vi_match_sensor", ret)
    }

    /// Finds the ISP configuration and matches the sensor.
    ///
    /// Tries `ANYKA_ISP_CONFIG` first if it is set, then each entry of
    /// [`ISP_CONFIG_SEARCH`]. Returns the directory that worked, so callers
    /// can log which one it was.
    ///
    /// Prefer this over [`match_sensor`] with a hardcoded path: the directory
    /// differs between firmware builds, and a wrong one fails at the very
    /// first SDK call with a message that does not say why.
    ///
    /// [`match_sensor`]: VideoInput::match_sensor
    pub fn match_sensor_auto() -> Result<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(override_path) = std::env::var_os(ISP_CONFIG_ENV) {
            candidates.push(PathBuf::from(override_path));
        }
        candidates.extend(ISP_CONFIG_SEARCH.iter().map(PathBuf::from));

        let mut last: Option<Error> = None;
        for candidate in candidates {
            if !candidate.is_dir() && !candidate.is_file() {
                continue;
            }
            match Self::match_sensor(&candidate) {
                Ok(()) => return Ok(candidate),
                Err(err) => {
                    tracing::debug!(path = %candidate.display(), %err, "sensor did not match");
                    last = Some(err);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            Error::invalid(
                "ak_vi_match_sensor",
                format!(
                    "no ISP configuration found; tried {}. Set {ISP_CONFIG_ENV} to override",
                    ISP_CONFIG_SEARCH.join(", ")
                ),
            )
        }))
    }

    /// Opens the video input device.
    pub fn open(dev: VideoDevice) -> Result<Self> {
        // SAFETY: takes an enum by value and returns an owned handle.
        let handle = unsafe { anyka_sys::ak_vi_open(dev.as_raw()) };
        if handle.is_null() {
            return Err(Error::null_handle("ak_vi_open"));
        }
        Ok(Self { handle })
    }

    /// The raw `void *` handle, for SDK calls this crate does not wrap yet.
    pub fn as_ptr(&self) -> *mut c_void {
        self.handle
    }

    /// Maximum resolution the attached sensor supports.
    pub fn sensor_resolution(&self) -> Result<Resolution> {
        let mut raw = anyka_sys::video_resolution::default();
        // SAFETY: valid handle, live out-parameter.
        let ret = unsafe { anyka_sys::ak_vi_get_sensor_resolution(self.handle, &mut raw) };
        check("ak_vi_get_sensor_resolution", ret)?;
        Ok(Resolution {
            width: raw.width,
            height: raw.height,
            max_width: raw.max_width,
            max_height: raw.max_height,
        })
    }

    /// Current crop and channel resolutions.
    pub fn channel_attr(&self) -> Result<ChannelAttr> {
        let mut raw = anyka_sys::video_channel_attr::default();
        // SAFETY: valid handle, live out-parameter.
        let ret = unsafe { anyka_sys::ak_vi_get_channel_attr(self.handle, &mut raw) };
        check("ak_vi_get_channel_attr", ret)?;
        Ok(ChannelAttr::from_raw(&raw))
    }

    /// Sets crop and channel resolutions. Call before [`capture_on`].
    ///
    /// [`capture_on`]: VideoInput::capture_on
    pub fn set_channel_attr(&self, attr: ChannelAttr) -> Result<()> {
        let raw = attr.to_raw();
        // SAFETY: valid handle; `raw` is read-only for the duration of the call.
        let ret = unsafe { anyka_sys::ak_vi_set_channel_attr(self.handle, &raw) };
        check("ak_vi_set_channel_attr", ret)
    }

    /// Changes resolutions while capture is running.
    pub fn change_channel_attr(&self, attr: ChannelAttr) -> Result<()> {
        let raw = attr.to_raw();
        // SAFETY: valid handle; `raw` is read-only for the duration of the call.
        let ret = unsafe { anyka_sys::ak_vi_change_channel_attr(self.handle, &raw) };
        check("ak_vi_change_channel_attr", ret)
    }

    /// Sensor capture rate, not the encoder frame rate.
    pub fn fps(&self) -> i32 {
        // SAFETY: valid handle.
        unsafe { anyka_sys::ak_vi_get_fps(self.handle) }
    }

    /// Sets the sensor capture rate. Must be within `1..=sensor max fps`.
    pub fn set_fps(&self, fps: i32) -> Result<()> {
        if fps < 1 {
            return Err(Error::invalid("ak_vi_set_fps", "fps must be at least 1"));
        }
        // SAFETY: valid handle, plain integer.
        let ret = unsafe { anyka_sys::ak_vi_set_fps(self.handle, fps) };
        check("ak_vi_set_fps", ret)
    }

    /// Flips vertically and/or mirrors horizontally. Both on means 180°.
    pub fn set_flip_mirror(&self, flip: bool, mirror: bool) -> Result<()> {
        // SAFETY: valid handle, plain integers.
        let ret =
            unsafe { anyka_sys::ak_vi_set_flip_mirror(self.handle, flip as i32, mirror as i32) };
        check("ak_vi_set_flip_mirror", ret)
    }

    /// Switches the ISP between the day and night profiles.
    pub fn switch_mode(&self, mode: DayNightMode) -> Result<()> {
        // SAFETY: valid handle, enum by value.
        let ret = unsafe { anyka_sys::ak_vi_switch_mode(self.handle, mode.as_raw()) };
        check("ak_vi_switch_mode", ret)
    }

    /// Starts ISP capture.
    pub fn capture_on(&self) -> Result<()> {
        // SAFETY: valid handle.
        let ret = unsafe { anyka_sys::ak_vi_capture_on(self.handle) };
        check("ak_vi_capture_on", ret)
    }

    /// Stops ISP capture.
    pub fn capture_off(&self) -> Result<()> {
        // SAFETY: valid handle.
        let ret = unsafe { anyka_sys::ak_vi_capture_off(self.handle) };
        check("ak_vi_capture_off", ret)
    }

    /// Drops any frames buffered by the ISP.
    pub fn clear_buffer(&self) -> Result<()> {
        // SAFETY: valid handle.
        let ret = unsafe { anyka_sys::ak_vi_clear_buffer(self.handle) };
        check("ak_vi_clear_buffer", ret)
    }

    /// Fetches one frame, holding both channels.
    ///
    /// The returned guard releases the frame on drop, so keep it alive only as
    /// long as the pixel data is actually needed.
    pub fn get_frame(&self) -> Result<VideoFrame<'_>> {
        let mut raw = anyka_sys::video_input_frame::default();
        // SAFETY: valid handle, live out-parameter. On success the SDK fills
        // `raw` with pointers it owns until `ak_vi_release_frame`, which
        // `VideoFrame::drop` guarantees.
        let ret = unsafe { anyka_sys::ak_vi_get_frame(self.handle, &mut raw) };
        check("ak_vi_get_frame", ret)?;
        Ok(VideoFrame { vi: self, raw })
    }
}

impl Drop for VideoInput {
    fn drop(&mut self) {
        // SAFETY: the handle is valid and this is the only place that closes it.
        unsafe { anyka_sys::ak_vi_close(self.handle) };
    }
}

/// One captured frame, borrowed from the SDK's DMA buffers.
///
/// Released back to the ISP on drop.
#[derive(Debug)]
pub struct VideoFrame<'vi> {
    vi: &'vi VideoInput,
    raw: anyka_sys::video_input_frame,
}

impl VideoFrame<'_> {
    /// YUV data for one channel.
    pub fn channel(&self, channel: VideoChannel) -> &[u8] {
        let frame = &self.raw.vi_frame[channel.index()];
        if frame.data.is_null() || frame.len == 0 {
            return &[];
        }
        // SAFETY: the SDK guarantees `data`/`len` describe a valid buffer until
        // `ak_vi_release_frame`, which cannot run before this borrow ends.
        unsafe { std::slice::from_raw_parts(frame.data, frame.len as usize) }
    }

    /// YUV data for the main (full-size) channel.
    pub fn main(&self) -> &[u8] {
        self.channel(VideoChannel::Main)
    }

    /// YUV data for the sub (downscaled) channel.
    pub fn sub(&self) -> &[u8] {
        self.channel(VideoChannel::Sub)
    }

    /// Capture timestamp in milliseconds since boot.
    pub fn timestamp_ms(&self, channel: VideoChannel) -> u64 {
        self.raw.vi_frame[channel.index()].ts
    }

    /// Frame sequence number as counted by the ISP.
    pub fn sequence(&self, channel: VideoChannel) -> u64 {
        self.raw.vi_frame[channel.index()].seq_no as u64
    }
}

impl Drop for VideoFrame<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` is the frame handed out by `ak_vi_get_frame` on the
        // same handle, released exactly once.
        unsafe { anyka_sys::ak_vi_release_frame(self.vi.handle, &mut self.raw) };
    }
}
