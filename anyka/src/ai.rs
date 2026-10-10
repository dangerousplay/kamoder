//! Audio input (ADC capture).
//!
//! [`AudioIn`] owns the `ak_ai_open` handle. Captured PCM comes back as
//! [`AudioFrame`] guards that release the SDK buffer on drop.

use crate::error::{check, Error, Result};
use std::ffi::c_void;
use std::time::Duration;

/// PCM capture format.
///
/// The hardware only does 16-bit samples, which is why `sample_bits` is not
/// configurable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmParam {
    pub sample_rate: u32,
    pub channels: u32,
}

impl Default for PcmParam {
    fn default() -> Self {
        Self {
            sample_rate: 8000,
            channels: 1,
        }
    }
}

impl PcmParam {
    /// Mono capture at the given sample rate.
    pub fn mono(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            channels: 1,
        }
    }

    fn to_raw(self) -> anyka_sys::pcm_param {
        anyka_sys::pcm_param {
            sample_rate: self.sample_rate,
            sample_bits: 16,
            channel_num: self.channels,
        }
    }
}

/// Which analog input feeds the ADC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioSource {
    /// Let the PCM driver pick; the SDK default.
    #[default]
    Auto,
    Mic,
    LineIn,
}

impl AudioSource {
    fn as_raw(self) -> anyka_sys::ai_source::Type {
        match self {
            AudioSource::Auto => anyka_sys::ai_source::AI_SOURCE_AUTO,
            AudioSource::Mic => anyka_sys::ai_source::AI_SOURCE_MIC,
            AudioSource::LineIn => anyka_sys::ai_source::AI_SOURCE_LINEIN,
        }
    }

    /// Parses the `mic` / `linein` spelling the C demos accept on the command
    /// line.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "mic" => Some(AudioSource::Mic),
            "linein" => Some(AudioSource::LineIn),
            "auto" => Some(AudioSource::Auto),
            _ => None,
        }
    }
}

/// Default frame interval used by the SDK demos, in milliseconds.
pub const DEFAULT_FRAME_INTERVAL_MS: i32 = anyka_sys::AUDIO_DEFAULT_INTERVAL as i32;
/// AMR needs exactly 20 ms frames.
pub const AMR_FRAME_INTERVAL_MS: i32 = anyka_sys::AMR_FRAME_INTERVAL as i32;
/// Highest value [`AudioIn::set_volume`] accepts.
pub const MAX_VOLUME: i32 = 12;

/// An open audio input device.
#[derive(Debug)]
pub struct AudioIn {
    handle: *mut c_void,
    capturing: bool,
}

// SAFETY: the SDK serialises the ADC handle internally; the vendor `aenc` demo
// passes the same handle to the audio encoder running on another thread.
unsafe impl Send for AudioIn {}
unsafe impl Sync for AudioIn {}

impl AudioIn {
    /// Opens the ADC with the given PCM format.
    pub fn open(param: PcmParam) -> Result<Self> {
        if param.channels == 0 || param.channels > 2 {
            return Err(Error::invalid("ak_ai_open", "channels must be 1 or 2"));
        }
        let raw = param.to_raw();
        // SAFETY: `raw` is read-only for the call and fully initialised.
        let handle = unsafe { anyka_sys::ak_ai_open(&raw) };
        if handle.is_null() {
            return Err(Error::null_handle("ak_ai_open"));
        }
        Ok(Self {
            handle,
            capturing: false,
        })
    }

    /// The raw `void *` handle, for SDK calls this crate does not wrap yet.
    pub fn as_ptr(&self) -> *mut c_void {
        self.handle
    }

    /// Sets how much audio one frame covers. Must be called before capture.
    ///
    /// The SDK accepts 10-125 ms.
    pub fn set_frame_interval(&self, ms: i32) -> Result<()> {
        if !(10..=125).contains(&ms) {
            return Err(Error::invalid(
                "ak_ai_set_frame_interval",
                "frame interval must be in 10..=125 ms",
            ));
        }
        // SAFETY: valid handle, validated integer.
        let ret = unsafe { anyka_sys::ak_ai_set_frame_interval(self.handle, ms) };
        check("ak_ai_set_frame_interval", ret)
    }

    /// Selects the analog input.
    pub fn set_source(&self, source: AudioSource) -> Result<()> {
        // SAFETY: valid handle, enum by value.
        let ret = unsafe { anyka_sys::ak_ai_set_source(self.handle, source.as_raw()) };
        check("ak_ai_set_source", ret)
    }

    /// Enables noise reduction together with automatic gain control.
    ///
    /// Only works at an 8 kHz sample rate.
    pub fn set_nr_agc(&self, enable: bool) -> Result<()> {
        // SAFETY: valid handle, plain integer.
        let ret = unsafe { anyka_sys::ak_ai_set_nr_agc(self.handle, enable as i32) };
        check("ak_ai_set_nr_agc", ret)
    }

    /// Enables acoustic echo cancellation. Only works at 8 kHz.
    pub fn set_aec(&self, enable: bool) -> Result<()> {
        // SAFETY: valid handle, plain integer.
        let ret = unsafe { anyka_sys::ak_ai_set_aec(self.handle, enable as i32) };
        check("ak_ai_set_aec", ret)
    }

    /// Enables the software resampler.
    pub fn set_resample(&self, enable: bool) -> Result<()> {
        // SAFETY: valid handle, plain integer.
        let ret = unsafe { anyka_sys::ak_ai_set_resample(self.handle, enable as i32) };
        check("ak_ai_set_resample", ret)
    }

    /// Sets the capture volume, 0 (mute) to 12 (max).
    ///
    /// This spans both stages the hardware has: 0-8 is ADC gain, 9-12 adds the
    /// soft limiter on top.
    pub fn set_volume(&self, volume: i32) -> Result<()> {
        if !(0..=MAX_VOLUME).contains(&volume) {
            return Err(Error::invalid(
                "ak_ai_set_volume",
                format!("volume must be in 0..={MAX_VOLUME}"),
            ));
        }
        // SAFETY: valid handle, validated integer.
        let ret = unsafe { anyka_sys::ak_ai_set_volume(self.handle, volume) };
        check("ak_ai_set_volume", ret)
    }

    /// Discards anything already buffered by the driver.
    pub fn clear_frame_buffer(&self) -> Result<()> {
        // SAFETY: valid handle.
        let ret = unsafe { anyka_sys::ak_ai_clear_frame_buffer(self.handle) };
        check("ak_ai_clear_frame_buffer", ret)
    }

    /// Starts ADC capture. Configure everything else first.
    pub fn start_capture(&mut self) -> Result<()> {
        // SAFETY: valid handle.
        let ret = unsafe { anyka_sys::ak_ai_start_capture(self.handle) };
        check("ak_ai_start_capture", ret)?;
        self.capturing = true;
        Ok(())
    }

    /// Stops ADC capture.
    pub fn stop_capture(&mut self) -> Result<()> {
        // SAFETY: valid handle.
        let ret = unsafe { anyka_sys::ak_ai_stop_capture(self.handle) };
        check("ak_ai_stop_capture", ret)?;
        self.capturing = false;
        Ok(())
    }

    /// Fetches one PCM frame.
    ///
    /// `timeout` of `None` blocks until a frame is available;
    /// `Some(Duration::ZERO)` polls without blocking.
    pub fn get_frame(&self, timeout: Option<Duration>) -> Result<AudioFrame<'_>> {
        let ms: std::os::raw::c_long = match timeout {
            None => -1,
            Some(d) => d.as_millis().min(i32::MAX as u128) as std::os::raw::c_long,
        };
        let mut raw = anyka_sys::frame::default();
        // SAFETY: valid handle, live out-parameter. On success the SDK owns
        // `raw.data` until `ak_ai_release_frame`, which `AudioFrame::drop`
        // guarantees.
        let ret = unsafe { anyka_sys::ak_ai_get_frame(self.handle, &mut raw, ms) };
        check("ak_ai_get_frame", ret)?;
        Ok(AudioFrame { ai: self, raw })
    }
}

impl Drop for AudioIn {
    fn drop(&mut self) {
        if self.capturing {
            // SAFETY: valid handle. Closing while capturing leaves the ADC
            // running on the device, so it is always stopped first.
            unsafe { anyka_sys::ak_ai_stop_capture(self.handle) };
        }
        // SAFETY: valid handle, closed exactly once.
        unsafe { anyka_sys::ak_ai_close(self.handle) };
    }
}

/// One captured PCM frame, borrowed from the SDK's buffer.
#[derive(Debug)]
pub struct AudioFrame<'ai> {
    ai: &'ai AudioIn,
    raw: anyka_sys::frame,
}

impl AudioFrame<'_> {
    /// Raw little-endian 16-bit PCM samples.
    pub fn data(&self) -> &[u8] {
        if self.raw.data.is_null() || self.raw.len == 0 {
            return &[];
        }
        // SAFETY: valid until `ak_ai_release_frame`, which cannot run before
        // this borrow ends.
        unsafe { std::slice::from_raw_parts(self.raw.data, self.raw.len as usize) }
    }

    /// Capture timestamp in milliseconds.
    pub fn timestamp_ms(&self) -> u64 {
        self.raw.ts
    }

    /// The underlying `struct frame`, needed to feed [`crate::AudioEncoder`].
    pub(crate) fn raw(&self) -> &anyka_sys::frame {
        &self.raw
    }
}

impl Drop for AudioFrame<'_> {
    fn drop(&mut self) {
        // SAFETY: released exactly once, on the handle that produced it.
        unsafe { anyka_sys::ak_ai_release_frame(self.ai.handle, &mut self.raw) };
    }
}
