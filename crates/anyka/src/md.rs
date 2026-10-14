//! Motion detection.
//!
//! Two independent mechanisms exist in the SDK and both are wrapped here:
//!
//! * [`MotionDetector`] drives `libmpi_md` (`ak_md_*`), which runs its own
//!   detection thread and reports a trigger with a timestamp;
//! * [`MotionStat`] reads the ISP's raw per-block motion statistics through
//!   `ak_vpss_md_get_stat`, which is what the vendor `md_demo` thresholds by
//!   hand and needs no extra thread.

use crate::error::{check, Error, Result};
use crate::vi::VideoInput;

/// Horizontal block count in the ISP motion grid.
pub const GRID_WIDTH: usize = anyka_sys::VPSS_MD_DIMENSION_H_MAX as usize;
/// Vertical block count in the ISP motion grid.
pub const GRID_HEIGHT: usize = anyka_sys::VPSS_MD_DIMENSION_V_MAX as usize;

/// Raw per-block motion energy from the ISP.
#[derive(Debug, Clone, Copy)]
pub struct MotionStat {
    /// Motion energy per block, `[row][column]`.
    pub blocks: [[u16; GRID_WIDTH]; GRID_HEIGHT],
    /// Largest value in `blocks`.
    pub max: u16,
    /// Aggregate level the ISP computed.
    pub level: u32,
}

impl MotionStat {
    /// Reads the current statistics for a video input.
    pub fn read(vi: &VideoInput) -> Result<Self> {
        let mut raw = anyka_sys::vpss_md_info::default();
        // SAFETY: valid handle, live out-parameter.
        let ret = unsafe { anyka_sys::ak_vpss_md_get_stat(vi.as_ptr(), &mut raw) };
        check("ak_vpss_md_get_stat", ret)?;
        Ok(Self {
            blocks: raw.stat,
            max: raw.stat_max,
            level: raw.level,
        })
    }

    /// Number of blocks whose energy exceeds `threshold`.
    pub fn blocks_above(&self, threshold: u16) -> usize {
        self.blocks
            .iter()
            .flatten()
            .filter(|&&value| value > threshold)
            .count()
    }

    /// Applies the two-level threshold the vendor demo uses: a block counts as
    /// moving above `level_threshold`, and the frame counts as moving once more
    /// than `block_threshold` blocks do.
    pub fn is_motion(&self, level_threshold: u16, block_threshold: usize) -> bool {
        self.blocks_above(level_threshold) > block_threshold
    }
}

/// The SDK's own motion detection service.
///
/// `ak_md_init` starts a background thread inside `libmpi_md`; dropping this
/// calls `ak_md_destroy` to stop it.
#[derive(Debug)]
pub struct MotionDetector {
    enabled: bool,
}

impl MotionDetector {
    /// Initialises motion detection against an open video input.
    pub fn init(vi: &VideoInput) -> Result<Self> {
        // SAFETY: valid handle; the SDK keeps a reference to it until
        // `ak_md_destroy`, which `Drop` runs while `vi` is still borrowed by
        // the caller.
        let ret = unsafe { anyka_sys::ak_md_init(vi.as_ptr()) };
        check("ak_md_init", ret)?;
        Ok(Self { enabled: false })
    }

    /// How many frames per second to analyse. Must not exceed the sensor rate.
    pub fn set_fps(&self, fps: i32) -> Result<()> {
        if fps < 1 {
            return Err(Error::invalid("ak_md_set_fps", "fps must be at least 1"));
        }
        // SAFETY: validated integer, no pointers.
        let ret = unsafe { anyka_sys::ak_md_set_fps(fps) };
        check("ak_md_set_fps", ret)
    }

    /// Global detection sensitivity.
    pub fn set_sensitivity(&self, sensitivity: i32) -> Result<()> {
        // SAFETY: plain integer, no pointers.
        let ret = unsafe { anyka_sys::ak_md_set_global_sensitivity(sensitivity) };
        check("ak_md_set_global_sensitivity", ret)
    }

    /// Starts or stops analysis.
    pub fn set_enabled(&mut self, enable: bool) -> Result<()> {
        // SAFETY: plain integer, no pointers.
        let ret = unsafe { anyka_sys::ak_md_enable(enable as i32) };
        check("ak_md_enable", ret)?;
        self.enabled = enable;
        Ok(())
    }

    /// Waits for a motion event.
    ///
    /// Returns the trigger time in seconds since the epoch, or `None` if the
    /// wait timed out. A negative `timeout_ms` blocks indefinitely and `0`
    /// polls.
    pub fn wait(&self, timeout_ms: i32) -> Result<Option<i32>> {
        let mut sec: std::os::raw::c_int = 0;
        // SAFETY: `sec` is a live out-parameter; passing null for `area_md`
        // is what the SDK expects when area mode is not configured.
        let ret =
            unsafe { anyka_sys::ak_md_get_result(&mut sec, std::ptr::null_mut(), timeout_ms) };
        match ret {
            1 => Ok(Some(sec)),
            0 => Ok(None),
            _ => Err(crate::Error::from_sdk("ak_md_get_result")),
        }
    }
}

impl Drop for MotionDetector {
    fn drop(&mut self) {
        if self.enabled {
            // SAFETY: plain integer; stops the worker before tearing it down.
            unsafe { anyka_sys::ak_md_enable(0) };
        }
        // SAFETY: matches the successful `ak_md_init` in `init`.
        unsafe { anyka_sys::ak_md_destroy() };
    }
}
