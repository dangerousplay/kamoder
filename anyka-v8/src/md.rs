//! ISP motion statistics on the V8 SDK.
//!
//! Two differences from the EV200 version, both of which change the code:
//!
//! * `ak_vpss_md_get_stat` takes a **device id**, not an opaque handle;
//! * `struct vpss_md_info` is now just the block grid — the EV200 `stat_max`
//!   and `level` fields are gone, so the peak is recomputed here.

use crate::error::{check, Result};
use crate::sys;
use crate::vi::VideoDevice;

/// Horizontal block count in the ISP motion grid.
pub const GRID_WIDTH: usize = sys::VPSS_MD_DIMENSION_H_MAX as usize;
/// Vertical block count in the ISP motion grid.
pub const GRID_HEIGHT: usize = sys::VPSS_MD_DIMENSION_V_MAX as usize;

/// Raw per-block motion energy.
#[derive(Debug, Clone, Copy)]
pub struct MotionStat {
    /// Motion energy per block, `[row][column]`.
    pub blocks: [[u16; GRID_WIDTH]; GRID_HEIGHT],
}

impl MotionStat {
    /// Reads the current statistics for a device.
    pub fn read(device: &VideoDevice) -> Result<Self> {
        let mut raw = sys::vpss_md_info::default();
        // SAFETY: valid device id, live out-parameter.
        check("ak_vpss_md_get_stat", unsafe {
            sys::ak_vpss_md_get_stat(device.id(), &mut raw)
        })?;
        Ok(Self { blocks: raw.stat })
    }

    /// Largest block value. Recomputed because V8 no longer reports it.
    pub fn max(&self) -> u16 {
        self.blocks.iter().flatten().copied().max().unwrap_or(0)
    }

    /// Number of blocks whose energy exceeds `threshold`.
    pub fn blocks_above(&self, threshold: u16) -> usize {
        self.blocks
            .iter()
            .flatten()
            .filter(|&&value| value > threshold)
            .count()
    }

    /// The two-level threshold the vendor demo uses: a block counts as moving
    /// above `level_threshold`, and the frame counts as moving once more than
    /// `block_threshold` blocks do.
    pub fn is_motion(&self, level_threshold: u16, block_threshold: usize) -> bool {
        self.blocks_above(level_threshold) > block_threshold
    }

    /// Whether the ISP has produced any motion data yet.
    ///
    /// An all-zero grid means the statistics engine has not run — useful for
    /// telling "no movement" apart from "not actually capturing".
    pub fn is_empty(&self) -> bool {
        self.max() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> MotionStat {
        MotionStat {
            blocks: [[0u16; GRID_WIDTH]; GRID_HEIGHT],
        }
    }

    #[test]
    fn grid_matches_the_sdk_dimensions() {
        assert_eq!(GRID_WIDTH, 32);
        assert_eq!(GRID_HEIGHT, 24);
    }

    #[test]
    fn empty_grid_reports_no_motion() {
        let stat = grid();
        assert!(stat.is_empty());
        assert_eq!(stat.max(), 0);
        assert_eq!(stat.blocks_above(0), 0);
        assert!(!stat.is_motion(5000, 12));
    }

    #[test]
    fn max_and_threshold_counting() {
        let mut stat = grid();
        stat.blocks[0][0] = 100;
        stat.blocks[5][7] = 9000;
        stat.blocks[23][31] = 42;
        assert_eq!(stat.max(), 9000);
        assert!(!stat.is_empty());
        assert_eq!(stat.blocks_above(41), 3);
        assert_eq!(stat.blocks_above(100), 1);
        assert_eq!(stat.blocks_above(9000), 0);
    }

    #[test]
    fn motion_needs_more_than_the_block_threshold() {
        let mut stat = grid();
        for block in stat.blocks[0].iter_mut().take(13) {
            *block = 6000;
        }
        // Exactly the threshold is not motion; one more is.
        assert!(!stat.is_motion(5000, 13));
        assert!(stat.is_motion(5000, 12));
    }
}
