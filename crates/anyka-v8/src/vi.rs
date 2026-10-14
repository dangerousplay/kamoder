//! Video input on the V8 SDK.
//!
//! The EV200 SDK handed out an opaque `void *` and folded everything into it.
//! V8 splits the same job across numeric **device** and **channel** ids with an
//! explicit enable step, and the call order is load-bearing:
//!
//! ```text
//! ak_vi_open(dev) -> load_sensor_cfg -> set_dev_attr
//!                 -> set_chn_attr -> enable_chn -> enable_dev -> get_frame
//! ```
//!
//! Getting that order wrong fails deep inside the ISP with a packed status and
//! no explanation, so it is encoded in the types: you cannot get a
//! [`VideoChannel`] without having configured the device, and you cannot pull
//! a frame without an enabled channel.

use crate::error::{check, Error, Result};
use crate::sys;
use std::cell::Cell;
use std::path::{Path, PathBuf};

/// Environment variable overriding the ISP configuration directory.
pub const ISP_CONFIG_ENV: &str = "ANYKA_ISP_CONFIG";

/// Directories searched for the ISP sensor configuration, in order.
///
/// Firmware builds disagree about where this lives. On the AV100 unit tested,
/// the configs are in `/usr/local`, with copies in `/rom` and `/ipc/local`;
/// `/etc/jffs2/` — which the vendor C demos hardcode — does not exist there.
pub const ISP_CONFIG_SEARCH: &[&str] = &["/usr/local/", "/rom/", "/ipc/local/", "/etc/jffs2/"];

/// Video input device id. The AK3918AV100 has one.
pub const VI_DEV0: i32 = 0;

/// Which capture channel to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Full-size channel.
    Main,
    /// Downscaled channel.
    Sub,
}

impl Channel {
    /// V8 numbers channels globally (`VIDEO_CHN0`…), not per device.
    pub fn id(self) -> i32 {
        match self {
            Channel::Main => sys::ak_vi_chn_enum::VIDEO_CHN0 as i32,
            Channel::Sub => sys::ak_vi_chn_enum::VIDEO_CHN1 as i32,
        }
    }
}

/// Sensor interface wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Interface {
    /// Parallel DVP.
    Dvp,
    /// MIPI, one lane.
    Mipi1,
    /// MIPI, two lanes — what the sc2331 on the tested board uses.
    #[default]
    Mipi2,
}

impl Interface {
    fn as_raw(self) -> sys::VI_INTF_MODE_E {
        match self {
            Interface::Dvp => sys::AK_VI_INTF_MODE::VI_INTF_DVP,
            Interface::Mipi1 => sys::AK_VI_INTF_MODE::VI_INTF_MIPI_1,
            Interface::Mipi2 => sys::AK_VI_INTF_MODE::VI_INTF_MIPI_2,
        }
    }
}

/// Crop window, in sensor pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Crop {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

/// Device-level configuration.
#[derive(Debug, Clone, Copy)]
pub struct DeviceAttr {
    pub interface: Interface,
    pub crop: Crop,
    /// Ceiling for the main channel.
    pub max_width: i32,
    pub max_height: i32,
    /// Ceiling for the sub channel.
    pub sub_max_width: i32,
    pub sub_max_height: i32,
    /// ISP source frame rate.
    pub frame_rate: i32,
}

impl Default for DeviceAttr {
    fn default() -> Self {
        Self {
            interface: Interface::Mipi2,
            crop: Crop::default(),
            max_width: 1280,
            max_height: 720,
            sub_max_width: 640,
            sub_max_height: 480,
            frame_rate: 25,
        }
    }
}

/// Per-channel configuration.
#[derive(Debug, Clone, Copy)]
pub struct ChannelAttr {
    pub width: i32,
    pub height: i32,
    pub frame_rate: i32,
    /// How many frame buffers the channel keeps. The SDK will not start with
    /// zero, and every buffer is DMA memory on a 32 MB device — 2 or 3 is the
    /// working range.
    pub frame_depth: i32,
}

impl Default for ChannelAttr {
    fn default() -> Self {
        Self {
            width: 640,
            height: 360,
            frame_rate: 25,
            frame_depth: 2,
        }
    }
}

/// An open video input device.
///
/// Dropping it disables and closes the device.
#[derive(Debug)]
pub struct VideoDevice {
    dev: i32,
    /// `Cell` because the device is enabled *after* its channels exist, and
    /// those channels hold a shared borrow of it. See [`VideoDevice::enable`].
    enabled: Cell<bool>,
}

impl VideoDevice {
    /// Opens the device. Nothing else may be called before this.
    pub fn open(dev: i32) -> Result<Self> {
        // SAFETY: plain integer argument, no pointers.
        check("ak_vi_open", unsafe { sys::ak_vi_open(dev) })?;
        Ok(Self {
            dev,
            enabled: Cell::new(false),
        })
    }

    /// The raw device id, for SDK calls this crate does not wrap yet.
    pub fn id(&self) -> i32 {
        self.dev
    }

    /// Loads an ISP configuration and matches the sensor.
    pub fn load_sensor_cfg(&self, config: impl AsRef<Path>) -> Result<()> {
        let path = config.as_ref();
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| Error::invalid("ak_vi_load_sensor_cfg", "path contains a NUL byte"))?;
        // SAFETY: valid device id; `c_path` is NUL-terminated and outlives the
        // call.
        check("ak_vi_load_sensor_cfg", unsafe {
            sys::ak_vi_load_sensor_cfg(self.dev, c_path.as_ptr())
        })
    }

    /// Finds an ISP configuration and matches the sensor.
    ///
    /// Tries `ANYKA_ISP_CONFIG` first, then [`ISP_CONFIG_SEARCH`]. Returns the
    /// directory that worked so callers can log it — which one succeeded is
    /// the single most useful fact when bringing up a new board.
    pub fn load_sensor_cfg_auto(&self) -> Result<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(over) = std::env::var_os(ISP_CONFIG_ENV) {
            candidates.push(PathBuf::from(over));
        }
        candidates.extend(ISP_CONFIG_SEARCH.iter().map(PathBuf::from));

        let mut last = None;
        for candidate in candidates {
            if !candidate.is_dir() && !candidate.is_file() {
                continue;
            }
            match self.load_sensor_cfg(&candidate) {
                Ok(()) => return Ok(candidate),
                Err(err) => {
                    tracing::debug!(path = %candidate.display(), %err, "sensor did not match");
                    last = Some(err);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            Error::invalid(
                "ak_vi_load_sensor_cfg",
                format!(
                    "no ISP configuration found; tried {}. Set {ISP_CONFIG_ENV} to override",
                    ISP_CONFIG_SEARCH.join(", ")
                ),
            )
        }))
    }

    /// Sensor id reported by the driver, e.g. `0x2331`.
    pub fn sensor_id(&self) -> Result<u32> {
        let mut id: std::os::raw::c_int = 0;
        // SAFETY: valid device id, live out-parameter.
        check("ak_vi_get_sensor_id", unsafe {
            sys::ak_vi_get_sensor_id(self.dev, &mut id)
        })?;
        Ok(id as u32)
    }

    /// Maximum resolution the attached sensor supports.
    pub fn sensor_resolution(&self) -> Result<(i32, i32)> {
        let mut res = sys::RECTANGLE_S::default();
        // SAFETY: valid device id, live out-parameter.
        check("ak_vi_get_sensor_resolution", unsafe {
            sys::ak_vi_get_sensor_resolution(self.dev, &mut res)
        })?;
        Ok((res.width, res.height))
    }

    /// Applies device-level attributes. Call before configuring a channel.
    pub fn set_attr(&self, attr: DeviceAttr) -> Result<()> {
        let mut raw = attr.to_raw(self.dev);
        // SAFETY: valid device id; `raw` is fully initialised and only read.
        check("ak_vi_set_dev_attr", unsafe {
            sys::ak_vi_set_dev_attr(self.dev, &mut raw)
        })
    }

    /// Configures and enables a capture channel.
    ///
    /// The returned channel borrows the device, so the device cannot be closed
    /// while capture is running.
    pub fn channel(&self, channel: Channel, attr: ChannelAttr) -> Result<VideoChannel<'_>> {
        if attr.width <= 0 || attr.height <= 0 {
            return Err(Error::invalid(
                "ak_vi_set_chn_attr",
                "resolution must be positive",
            ));
        }
        if attr.frame_depth < 1 {
            return Err(Error::invalid(
                "ak_vi_set_chn_attr",
                "frame_depth must be at least 1",
            ));
        }

        let chn = channel.id();
        let mut raw = attr.to_raw(chn);
        // SAFETY: `raw` is fully initialised and only read during the call.
        check("ak_vi_set_chn_attr", unsafe {
            sys::ak_vi_set_chn_attr(chn, &mut raw)
        })?;
        // SAFETY: plain integer argument.
        check("ak_vi_enable_chn", unsafe { sys::ak_vi_enable_chn(chn) })?;
        Ok(VideoChannel { chn, _device: self })
    }

    /// Starts the device. Channels must already be enabled.
    ///
    /// Takes `&self`, not `&mut self`: the SDK requires `enable_dev` *after*
    /// `enable_chn`, and by then the [`VideoChannel`] holds a shared borrow of
    /// the device. Demanding `&mut` here would make the correct call order
    /// unexpressible.
    pub fn enable(&self) -> Result<()> {
        // SAFETY: plain integer argument.
        check("ak_vi_enable_dev", unsafe {
            sys::ak_vi_enable_dev(self.dev)
        })?;
        self.enabled.set(true);
        Ok(())
    }
}

impl Drop for VideoDevice {
    fn drop(&mut self) {
        if self.enabled.get() {
            // SAFETY: valid device id. Closing an enabled device leaves the
            // ISP holding buffers, so it is always disabled first.
            unsafe { sys::ak_vi_disable_dev(self.dev) };
        }
        // SAFETY: valid device id, closed exactly once.
        unsafe { sys::ak_vi_close(self.dev) };
    }
}

impl DeviceAttr {
    fn to_raw(self, dev: i32) -> sys::VI_DEV_ATTR {
        sys::VI_DEV_ATTR {
            dev_id: dev,
            interf_mode: self.interface.as_raw(),
            // ISP enabled — the alternative paths are bypass and raw capture,
            // neither of which produces the YUV these demos consume.
            data_path: sys::AK_VI_DATA_PATH::VI_PATH_ISP,
            data_type: sys::AK_VI_DATA_TYPE::VI_DATA_TYPE_YUV420SP,
            crop: sys::crop_info {
                left: self.crop.left,
                top: self.crop.top,
                width: self.crop.width,
                height: self.crop.height,
            },
            max_width: self.max_width,
            max_height: self.max_height,
            sub_max_width: self.sub_max_width,
            sub_max_height: self.sub_max_height,
            frame_rate: self.frame_rate,
            // `fd` is filled in by the SDK when the device is opened.
            ..Default::default()
        }
    }
}

impl ChannelAttr {
    fn to_raw(self, chn: i32) -> sys::VI_CHN_ATTR {
        sys::VI_CHN_ATTR {
            chn_id: chn,
            frame_rate: self.frame_rate,
            res: sys::RECTANGLE_S {
                width: self.width,
                height: self.height,
            },
            frame_depth: self.frame_depth,
        }
    }
}

/// An enabled capture channel.
///
/// Disabled on drop, before the device it borrows can be closed.
#[derive(Debug)]
pub struct VideoChannel<'dev> {
    chn: i32,
    _device: &'dev VideoDevice,
}

impl VideoChannel<'_> {
    /// The raw channel id.
    pub fn id(&self) -> i32 {
        self.chn
    }

    /// Fetches one frame.
    ///
    /// The guard releases it on drop — the ISP keeps only `frame_depth`
    /// buffers, so holding one stalls capture within a few frames.
    pub fn get_frame(&self) -> Result<VideoFrame<'_>> {
        let mut raw = sys::video_input_frame::default();
        // SAFETY: valid channel id, live out-parameter. On success the SDK
        // owns the buffer until `ak_vi_release_frame`, which `VideoFrame::drop`
        // guarantees.
        check("ak_vi_get_frame", unsafe {
            sys::ak_vi_get_frame(self.chn, &mut raw)
        })?;
        Ok(VideoFrame { channel: self, raw })
    }
}

impl Drop for VideoChannel<'_> {
    fn drop(&mut self) {
        // SAFETY: valid channel id, disabled exactly once.
        unsafe { sys::ak_vi_disable_chn(self.chn) };
    }
}

/// One captured frame, borrowed from the SDK's buffers.
///
/// V8 hands back a single frame per call — the EV200 struct carried main and
/// sub together, so there is no `main()`/`sub()` split here.
#[derive(Debug)]
pub struct VideoFrame<'chn> {
    channel: &'chn VideoChannel<'chn>,
    raw: sys::video_input_frame,
}

impl VideoFrame<'_> {
    /// The YUV bytes.
    pub fn data(&self) -> &[u8] {
        let frame = &self.raw.vi_frame;
        if frame.data.is_null() || frame.len == 0 {
            return &[];
        }
        // SAFETY: the SDK guarantees `data[..len]` stays valid until
        // `ak_vi_release_frame`, which cannot run before this borrow ends.
        unsafe { std::slice::from_raw_parts(frame.data, frame.len as usize) }
    }

    /// Capture timestamp in milliseconds.
    pub fn timestamp_ms(&self) -> u64 {
        self.raw.vi_frame.ts
    }

    /// Frame sequence number from the ISP.
    pub fn sequence(&self) -> u64 {
        self.raw.vi_frame.seq_no as u64
    }

    /// Physical address of the buffer, for zero-copy handoff to the encoder.
    pub fn phys_addr(&self) -> u64 {
        self.raw.phyaddr as u64
    }
}

impl Drop for VideoFrame<'_> {
    fn drop(&mut self) {
        // SAFETY: released exactly once, on the channel that produced it.
        unsafe { sys::ak_vi_release_frame(self.channel.chn, &mut self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_map_to_distinct_ids() {
        assert_eq!(Channel::Main.id(), 0);
        assert_eq!(Channel::Sub.id(), 1);
        assert_ne!(Channel::Main.id(), Channel::Sub.id());
    }

    #[test]
    fn device_attr_enables_the_isp_path() {
        let raw = DeviceAttr::default().to_raw(VI_DEV0);
        assert_eq!(raw.dev_id, VI_DEV0);
        assert_eq!(raw.data_path, sys::AK_VI_DATA_PATH::VI_PATH_ISP);
        assert_eq!(raw.data_type, sys::AK_VI_DATA_TYPE::VI_DATA_TYPE_YUV420SP);
        assert_eq!(raw.interf_mode, sys::AK_VI_INTF_MODE::VI_INTF_MIPI_2);
    }

    #[test]
    fn channel_attr_carries_resolution_and_depth() {
        let attr = ChannelAttr {
            width: 640,
            height: 360,
            frame_rate: 20,
            frame_depth: 3,
        };
        let raw = attr.to_raw(Channel::Sub.id());
        assert_eq!(raw.chn_id, 1);
        assert_eq!(raw.res.width, 640);
        assert_eq!(raw.res.height, 360);
        assert_eq!(raw.frame_rate, 20);
        assert_eq!(raw.frame_depth, 3);
    }

    #[test]
    fn crop_survives_conversion() {
        let attr = DeviceAttr {
            crop: Crop {
                left: 4,
                top: 8,
                width: 1280,
                height: 720,
            },
            ..DeviceAttr::default()
        };
        let raw = attr.to_raw(VI_DEV0);
        assert_eq!((raw.crop.left, raw.crop.top), (4, 8));
        assert_eq!((raw.crop.width, raw.crop.height), (1280, 720));
    }
}
