//! Safe Rust wrappers around the Anyka AK39xx IPC SDK.
//!
//! The SDK is a set of C libraries that hand out `void *` handles and expect
//! the caller to pair every `open` with a `close`, every `get_frame` with a
//! `release_frame` and every `request_stream` with a `cancel_stream`. Getting
//! any of those wrong leaks DMA buffers or panics the ISP driver on a camera
//! with 32 MB of RAM.
//!
//! This crate turns each of those pairs into a type:
//!
//! * handles ([`VideoInput`], [`Encoder`], [`AudioIn`], [`AudioEncoder`], …)
//!   close themselves on drop,
//! * borrowed data ([`VideoFrame`], [`VideoStream`], [`AudioFrame`]) are
//!   guards that release the underlying buffer on drop and hand out `&[u8]`
//!   tied to that guard's lifetime,
//! * stream handles borrow the input and encoder they were built from, so the
//!   encoder cannot be closed while a stream is still running.
//!
//! ```no_run
//! use anyka::{Result, VideoInput, VideoDevice};
//!
//! # fn main() -> Result<()> {
//! VideoInput::match_sensor("/etc/jffs2/")?;
//! let vi = VideoInput::open(VideoDevice::Dev0)?;
//! vi.capture_on()?;
//! let frame = vi.get_frame()?;             // released when `frame` drops
//! println!("{} bytes", frame.main().len());
//! # Ok(())
//! # }
//! ```
//!
//! Everything not covered here is still reachable through [`anyka_sys`].

pub mod aenc;
pub mod ai;
pub mod error;
pub mod ir;
pub mod log;
pub mod md;
pub mod rtsp;
pub mod time;
pub mod venc;
pub mod vi;
pub mod watchdog;

/// Signal-driven shutdown, re-exported from the SDK-free `os` crate.
pub use os::shutdown;

pub use aenc::{AudioEncoder, AudioStreamHandle, AudioType, EncodedAudio};
pub use ai::{AudioFrame, AudioIn, AudioSource, PcmParam};
pub use error::{Error, Result};
pub use ir::{IrSensor, LightLevel};
pub use md::MotionDetector;
pub use rtsp::{RtspChannelConfig, RtspServer};
pub use venc::{
    BitrateMode, EncodeGroup, EncodeParam, Encoder, OutputType, Profile, VideoStream,
    VideoStreamHandle,
};
pub use vi::{
    ChannelAttr, Crop, DayNightMode, Resolution, VideoChannel, VideoDevice, VideoFrame, VideoInput,
};
pub use watchdog::Watchdog;

use std::ffi::CString;
use std::path::Path;

/// Converts a path into a NUL-terminated string for the C API.
///
/// Rejects interior NUL bytes rather than silently truncating the path, which
/// is what `CString::new` guards against.
pub(crate) fn path_to_cstring(op: &'static str, path: &Path) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::invalid(op, "path contains an interior NUL byte"))
}

/// Version strings reported by the SDK libraries.
///
/// One function per subsystem on purpose. A single `versions()` returning all
/// of them would make every binary reference `ak_vi_*`, `ak_venc_*` and
/// `ak_aenc_*`, which in turn forces it to link the whole SDK — and loading
/// the whole SDK breaks the audio path on real hardware. See the `anyka-link`
/// crate.
pub mod version {
    /// `libplat_common` version.
    pub fn common() -> String {
        // SAFETY: returns a pointer to a static string inside the SDK.
        unsafe { super::cstr(anyka_sys::ak_common_get_version()) }
    }

    /// `libplat_vi` version. Only call this from a binary that links video in.
    pub fn video_in() -> String {
        // SAFETY: as above.
        unsafe { super::cstr(anyka_sys::ak_vi_get_version()) }
    }

    /// `libmpi_venc` version. Only from a binary that links video encode.
    pub fn video_encode() -> String {
        // SAFETY: as above.
        unsafe { super::cstr(anyka_sys::ak_venc_get_version()) }
    }

    /// `libplat_ai` version. Only from a binary that links audio in.
    pub fn audio_in() -> String {
        // SAFETY: as above.
        unsafe { super::cstr(anyka_sys::ak_ai_get_version()) }
    }

    /// `libmpi_aenc` version. Only from a binary that links audio encode.
    pub fn audio_encode() -> String {
        // SAFETY: as above.
        unsafe { super::cstr(anyka_sys::ak_aenc_get_version()) }
    }
}

/// # Safety
/// `ptr` must be null or point to a NUL-terminated string that stays valid for
/// the duration of the call.
pub(crate) unsafe fn cstr(ptr: *const std::os::raw::c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}
