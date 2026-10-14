//! Safe wrappers for the **AK3918AV100 (V8)** Anyka SDK.
//!
//! Separate from the `anyka` crate on purpose. That one wraps the AK3918EV200
//! SDK (`libakispsdk.so` V3.1.00), which is a different generation, not a
//! different version: opaque `void *` handles versus numeric device/channel
//! ids, and one frame per fetch versus two. Serving both from one module would
//! need `#[cfg]` through every function and would make neither easier to read.
//!
//! Scope today is the MVP: video input and ISP motion statistics, enough to
//! prove the SDK swap on real hardware. See
//! `.specs/features/v8-sdk-port/spec.md`.
//!
//! ```no_run
//! use anyka_v8::{Channel, ChannelAttr, DeviceAttr, MotionStat, VideoDevice, VI_DEV0};
//!
//! # fn main() -> anyka_v8::Result<()> {
//! let dev = VideoDevice::open(VI_DEV0)?;
//! let config = dev.load_sensor_cfg_auto()?;
//! dev.set_attr(DeviceAttr::default())?;
//! let chn = dev.channel(Channel::Sub, ChannelAttr::default())?;
//! dev.enable()?;
//!
//! let frame = chn.get_frame()?;          // released on drop
//! let stat = MotionStat::read(&dev)?;
//! println!("{} bytes, peak {}", frame.data().len(), stat.max());
//! # let _ = config;
//! # Ok(())
//! # }
//! ```

/// The V8 half of the generated bindings, aliased so every module in this
/// crate can write `anyka_sys::` and mean the V8 generation.
pub(crate) use anyka_sys::v8 as sys;

pub mod error;
pub mod md;
pub mod vi;

pub use error::{Error, Result};
pub use md::{MotionStat, GRID_HEIGHT, GRID_WIDTH};
pub use vi::{
    Channel, ChannelAttr, Crop, DeviceAttr, Interface, VideoChannel, VideoDevice, VideoFrame,
    ISP_CONFIG_ENV, ISP_CONFIG_SEARCH, VI_DEV0,
};

/// SDK version strings, for startup banners.
pub mod version {
    /// `libplat_vi` version — expected to report the V8 generation.
    pub fn video_in() -> String {
        // SAFETY: returns a pointer to a static string inside the SDK.
        let ptr = unsafe { crate::sys::ak_vi_get_version() };
        if ptr.is_null() {
            return String::new();
        }
        // SAFETY: NUL-terminated and valid for the process lifetime.
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}
