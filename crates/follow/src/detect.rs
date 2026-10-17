//! Moving-subject detector over the ISP motion grid (FOL-04..08).
//!
//! The AV100 ISP already reduces every frame to a grid of at most 32×24
//! `u16` "3D-NR" energies (`video-proto::motion`), so detection never touches
//! pixels. Per grid the detector:
//!
//! 1. compares each cell with a slow running baseline
//!    (`bg += (cur - bg) >> k`, kept in Q4 fixed point so small differences
//!    still converge) and marks it hot with hysteresis: on when
//!    `cur - bg > t_on`, off again only when `cur - bg < t_off` (FOL-04);
//! 2. if more than `max_fraction_permille` of the cells are hot, treats the
//!    grid as a global change (lighting, IR-cut switch, the head's own
//!    motion), ignores it and re-seeds the baseline from it (FOL-07);
//! 3. finds 4-connected components of hot cells and keeps the largest one of
//!    at least `min_cells` cells, with its energy-weighted centroid and
//!    bounding box (FOL-05);
//! 4. reports it only once a blob has been present in `decision_frames`
//!    consecutive grids, so single-frame blips are ignored (FOL-06).
//!
//! Everything per cell is integer (FOL-08): the ARM926 has no FPU and
//! soft-float is about 19× slower. All buffers are fixed-size arrays inside
//! [`Detector`] (about 5 KB), allocated once, so nothing allocates per grid
//! on a board whose encoder oopses when memory fragments. The flood fill uses
//! an explicit fixed stack instead of recursion; each cell is pushed at most
//! once, so a [`MAX_CELLS`] stack always suffices.
//!
//! The grid arrives as plain `(cols, rows, cells)` rather than a
//! `video-proto` type, so the detector stays independent of the transport.
//!
//! Centroid continuity between frames is not checked: at 10 Hz a walking
//! subject moves at most a cell or two, and the largest-blob rule already
//! prefers it. Add it if hardware shows the target hopping between blobs.

use snafu::{ensure, Snafu};

/// Most grid columns accepted (the ISP reports 32).
pub const MAX_COLS: usize = 32;

/// Most grid rows accepted (the ISP buffer holds 24; the vendor reads 16).
pub const MAX_ROWS: usize = 24;

/// Most cells in a grid.
pub const MAX_CELLS: usize = MAX_COLS * MAX_ROWS;

/// Fixed-point shift of the stored baseline (Q4), so `(cur - bg) >> k` does
/// not stall once the difference is below `2^k`.
const BG_SHIFT: u32 = 4;

/// Errors from [`Detector::update`].
#[derive(Debug, Snafu, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    #[snafu(display(
        "invalid motion grid {cols}x{rows} with {len} cells (at most {MAX_COLS}x{MAX_ROWS}, \
         cols*rows must equal the cell count)"
    ))]
    InvalidGrid {
        cols: usize,
        rows: usize,
        len: usize,
    },
}

type Result<T, E = Error> = std::result::Result<T, E>;

/// Detector tuning. The defaults are educated guesses for `u16` 3D-NR
/// energies and have **not been tuned on hardware**: the value range of the
/// AV100 grid is still unmeasured (design spike T0). The board profile
/// (`[ptz.follow]`) overrides them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectConfig {
    /// A cell turns hot when `cur - baseline` exceeds this.
    pub t_on: u16,
    /// A hot cell stays hot until `cur - baseline` drops below this
    /// (≤ `t_on`).
    pub t_off: u16,
    /// Baseline speed: each grid moves it by `1 / 2^k` of the difference.
    /// 3 at 10 Hz ≈ a 0.8 s time constant.
    pub k: u8,
    /// Smallest blob, in cells, that may become a target.
    pub min_cells: u16,
    /// Consecutive grids with a blob before it becomes a target.
    pub decision_frames: u16,
    /// More hot cells than this share of the grid (per mille) is a global
    /// change, not a subject.
    pub max_fraction_permille: u16,
}

impl Default for DetectConfig {
    fn default() -> Self {
        Self {
            t_on: 1000,
            t_off: 500,
            k: 3,
            min_cells: 2,
            decision_frames: 5,
            max_fraction_permille: 600,
        }
    }
}

/// Inclusive bounding box of a blob, in grid cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BBox {
    pub col_min: u8,
    pub row_min: u8,
    pub col_max: u8,
    pub row_max: u8,
}

/// A persistent moving subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    /// Energy-weighted centroid, 0 = left edge, 1000 = right edge.
    pub cx_permille: u16,
    /// Energy-weighted centroid, 0 = top edge, 1000 = bottom edge.
    pub cy_permille: u16,
    pub bbox: BBox,
    /// Hot cells in the blob.
    pub cells: u16,
}

/// One connected component found in the current grid.
#[derive(Debug, Clone, Copy)]
struct Blob {
    cells: u16,
    energy: u64,
    sum_x: u64,
    sum_y: u64,
    bbox: BBox,
}

/// The detector state: baseline, hot map and persistence counter. See the
/// module docs.
#[derive(Debug, Clone)]
pub struct Detector {
    config: DetectConfig,
    cols: usize,
    rows: usize,
    /// No baseline yet: the next grid seeds it.
    seeded: bool,
    /// Baseline per cell, Q4 fixed point.
    baseline: [i32; MAX_CELLS],
    /// Positive excess over the baseline per cell, the blob weight.
    excess: [u32; MAX_CELLS],
    /// Hot cells, one bit per column.
    hot: [u32; MAX_ROWS],
    /// Cells already assigned to a component in this grid.
    visited: [u32; MAX_ROWS],
    /// Flood-fill stack of cell indices.
    stack: [u16; MAX_CELLS],
    /// Consecutive grids that had a blob.
    streak: u16,
    hot_count: u16,
}

impl Detector {
    pub fn new(config: DetectConfig) -> Self {
        Self {
            config,
            cols: 0,
            rows: 0,
            seeded: false,
            baseline: [0; MAX_CELLS],
            excess: [0; MAX_CELLS],
            hot: [0; MAX_ROWS],
            visited: [0; MAX_ROWS],
            stack: [0; MAX_CELLS],
            streak: 0,
            hot_count: 0,
        }
    }

    pub fn config(&self) -> &DetectConfig {
        &self.config
    }

    /// Forgets the baseline and any streak; the next grid seeds the
    /// baseline and yields nothing. Call after the head settles (FOL-10).
    pub fn reseed(&mut self) {
        self.seeded = false;
        self.streak = 0;
        self.hot = [0; MAX_ROWS];
        self.hot_count = 0;
    }

    /// Hot cells in the last grid (after hysteresis), for metrics/overlay.
    pub fn hot_count(&self) -> u16 {
        self.hot_count
    }

    /// Whether the cell was hot in the last grid; `false` outside the grid.
    pub fn is_hot(&self, col: usize, row: usize) -> bool {
        col < self.cols && row < self.rows && self.hot[row] & (1 << col) != 0
    }

    /// Feeds one grid (`cells` row-major, `cols * rows` long) and returns
    /// the target, if a blob has persisted for `decision_frames` grids.
    pub fn update(&mut self, cols: usize, rows: usize, cells: &[u16]) -> Result<Option<Detection>> {
        ensure!(
            (1..=MAX_COLS).contains(&cols)
                && (1..=MAX_ROWS).contains(&rows)
                && cols * rows == cells.len(),
            InvalidGridSnafu {
                cols,
                rows,
                len: cells.len()
            }
        );
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            self.reseed();
        }
        if !self.seeded {
            self.seed(cells);
            return Ok(None);
        }

        let hot = self.classify(cells);
        let limit = cells.len() as u32 * u32::from(self.config.max_fraction_permille) / 1000;
        if u32::from(hot) > limit {
            tracing::debug!(hot, cells = cells.len(), "global change, re-seeding");
            self.seed(cells);
            return Ok(None);
        }

        match self.largest_blob() {
            Some(blob) if blob.cells >= self.config.min_cells => {
                self.streak = self.streak.saturating_add(1);
                if self.streak >= self.config.decision_frames {
                    Ok(Some(self.detection(&blob)))
                } else {
                    Ok(None)
                }
            }
            _ => {
                self.streak = 0;
                Ok(None)
            }
        }
    }

    /// Baseline := this grid; nothing hot, no streak.
    fn seed(&mut self, cells: &[u16]) {
        for (bg, &cur) in self.baseline.iter_mut().zip(cells) {
            *bg = i32::from(cur) << BG_SHIFT;
        }
        self.hot = [0; MAX_ROWS];
        self.hot_count = 0;
        self.streak = 0;
        self.seeded = true;
    }

    /// Hysteresis against the baseline, then the baseline update. Returns
    /// the hot count.
    fn classify(&mut self, cells: &[u16]) -> u16 {
        let t_on = i32::from(self.config.t_on);
        let t_off = i32::from(self.config.t_off);
        let k = u32::from(self.config.k).min(15);
        let mut count = 0u16;
        for row in 0..self.rows {
            let mut bits = 0u32;
            for col in 0..self.cols {
                let i = row * self.cols + col;
                let cur = i32::from(cells[i]);
                let bg = &mut self.baseline[i];
                let diff = cur - (*bg >> BG_SHIFT);
                let was_hot = self.hot[row] & (1 << col) != 0;
                let is_hot = if was_hot { diff >= t_off } else { diff > t_on };
                if is_hot {
                    bits |= 1 << col;
                    count += 1;
                }
                self.excess[i] = diff.max(1) as u32;
                *bg += ((cur << BG_SHIFT) - *bg) >> k;
            }
            self.hot[row] = bits;
        }
        self.hot_count = count;
        count
    }

    /// Largest 4-connected component (by cells, then energy).
    fn largest_blob(&mut self) -> Option<Blob> {
        self.visited = [0; MAX_ROWS];
        let mut best: Option<Blob> = None;
        for row in 0..self.rows {
            for col in 0..self.cols {
                let bit = 1 << col;
                if self.hot[row] & bit == 0 || self.visited[row] & bit != 0 {
                    continue;
                }
                let blob = self.fill(col, row);
                let better = best.is_none_or(|b| (blob.cells, blob.energy) > (b.cells, b.energy));
                if better {
                    best = Some(blob);
                }
            }
        }
        best
    }

    /// Flood fill from a hot, unvisited cell.
    fn fill(&mut self, col: usize, row: usize) -> Blob {
        let cols = self.cols;
        let mut blob = Blob {
            cells: 0,
            energy: 0,
            sum_x: 0,
            sum_y: 0,
            bbox: BBox {
                col_min: col as u8,
                row_min: row as u8,
                col_max: col as u8,
                row_max: row as u8,
            },
        };
        let mut top = 0;
        self.visited[row] |= 1 << col;
        self.stack[top] = (row * cols + col) as u16;
        top += 1;
        while top > 0 {
            top -= 1;
            let i = usize::from(self.stack[top]);
            let (r, c) = (i / cols, i % cols);
            let w = u64::from(self.excess[i]);
            blob.cells += 1;
            blob.energy += w;
            // Cell centres at (2c + 1) / 2cols, kept doubled to stay integer.
            blob.sum_x += w * (2 * c as u64 + 1);
            blob.sum_y += w * (2 * r as u64 + 1);
            blob.bbox.col_min = blob.bbox.col_min.min(c as u8);
            blob.bbox.col_max = blob.bbox.col_max.max(c as u8);
            blob.bbox.row_min = blob.bbox.row_min.min(r as u8);
            blob.bbox.row_max = blob.bbox.row_max.max(r as u8);

            // `wrapping_sub` turns "left of column 0" into a huge index that
            // the bounds check rejects.
            for (nc, nr) in [
                (c.wrapping_sub(1), r),
                (c + 1, r),
                (c, r.wrapping_sub(1)),
                (c, r + 1),
            ] {
                if nc >= cols || nr >= self.rows {
                    continue;
                }
                let bit = 1 << nc;
                if self.hot[nr] & bit != 0 && self.visited[nr] & bit == 0 {
                    self.visited[nr] |= bit;
                    self.stack[top] = (nr * cols + nc) as u16;
                    top += 1;
                }
            }
        }
        blob
    }

    fn detection(&self, blob: &Blob) -> Detection {
        let permille = |sum: u64, n: usize| -> u16 {
            let den = 2 * n as u64 * blob.energy;
            ((sum * 1000 + den / 2) / den).min(1000) as u16
        };
        Detection {
            cx_permille: permille(blob.sum_x, self.cols),
            cy_permille: permille(blob.sum_y, self.rows),
            bbox: blob.bbox,
            cells: blob.cells,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: usize = 32;
    const ROWS: usize = 24;
    const BASE: u16 = 200;
    const HOT: u16 = 3000;

    fn grid(hot: &[(usize, usize)]) -> Vec<u16> {
        let mut g = vec![BASE; COLS * ROWS];
        for &(c, r) in hot {
            g[r * COLS + c] = HOT;
        }
        g
    }

    fn square(c0: usize, r0: usize, n: usize) -> Vec<(usize, usize)> {
        (0..n)
            .flat_map(|dc| (0..n).map(move |dr| (c0 + dc, r0 + dr)))
            .collect()
    }

    fn seeded() -> Detector {
        let mut d = Detector::new(DetectConfig::default());
        assert_eq!(d.update(COLS, ROWS, &grid(&[])).unwrap(), None);
        d
    }

    #[test]
    fn rejects_bad_dimensions() {
        let mut d = Detector::new(DetectConfig::default());
        for (cols, rows, len) in [(33, 24, 33 * 24), (32, 25, 32 * 25), (0, 1, 0), (4, 4, 15)] {
            assert_eq!(
                d.update(cols, rows, &vec![0; len]),
                Err(Error::InvalidGrid { cols, rows, len })
            );
        }
    }

    #[test]
    fn static_scene_yields_nothing() {
        let mut d = seeded();
        // Sensor noise well under t_on.
        for i in 0..50u16 {
            let g: Vec<u16> = (0..COLS * ROWS)
                .map(|c| BASE + ((c as u16 + i) % 7) * 20)
                .collect();
            assert_eq!(d.update(COLS, ROWS, &g).unwrap(), None);
        }
        assert_eq!(d.hot_count(), 0);
    }

    #[test]
    fn moving_blob_becomes_a_target_after_decision_frames() {
        let mut d = seeded();
        let frames = DetectConfig::default().decision_frames as usize;
        // Jumps three columns per grid so every frame lands on cells whose
        // baseline is untouched and the weights stay equal.
        for i in 0..frames {
            let found = d
                .update(COLS, ROWS, &grid(&square(1 + 3 * i, 10, 3)))
                .unwrap();
            if i + 1 < frames {
                assert_eq!(found, None, "frame {i}");
            } else {
                let t = found.expect("target");
                assert_eq!(t.cells, 9);
                // Columns 13..=15 → centre of column 14 = 29/64.
                assert_eq!(t.cx_permille, 453);
                // Rows 10..=12 → centre of row 11 = 23/48.
                assert_eq!(t.cy_permille, 479);
                assert_eq!(
                    t.bbox,
                    BBox {
                        col_min: 13,
                        row_min: 10,
                        col_max: 15,
                        row_max: 12
                    }
                );
            }
        }
        assert!(d.is_hot(14, 11) && !d.is_hot(0, 0) && !d.is_hot(40, 0));
    }

    #[test]
    fn single_frame_blip_is_ignored() {
        let mut d = seeded();
        assert_eq!(d.update(COLS, ROWS, &grid(&square(5, 5, 2))).unwrap(), None);
        for _ in 0..10 {
            assert_eq!(d.update(COLS, ROWS, &grid(&[])).unwrap(), None);
        }
        // The streak restarted: a new blob needs the full count again.
        let frames = DetectConfig::default().decision_frames;
        for i in 1..frames {
            assert_eq!(
                d.update(COLS, ROWS, &grid(&square(5 + i as usize, 5, 2)))
                    .unwrap(),
                None
            );
        }
    }

    #[test]
    fn blobs_below_min_cells_are_ignored() {
        let mut d = seeded();
        for i in 0..10 {
            assert_eq!(d.update(COLS, ROWS, &grid(&[(3 + i, 3)])).unwrap(), None);
        }
    }

    #[test]
    fn global_flash_is_ignored_and_reseeds() {
        let mut d = seeded();
        let bright = vec![HOT; COLS * ROWS];
        assert_eq!(d.update(COLS, ROWS, &bright).unwrap(), None);
        // The baseline is now the bright scene: staying bright is static.
        for _ in 0..10 {
            assert_eq!(d.update(COLS, ROWS, &bright).unwrap(), None);
            assert_eq!(d.hot_count(), 0);
        }
    }

    #[test]
    fn of_two_blobs_the_largest_wins() {
        let mut d = seeded();
        let mut found = None;
        for i in 0..5 {
            let mut hot = square(2, 2 + i, 2); // 4 cells, left
            hot.extend(square(24, 15 - i, 3)); // 9 cells, right
            found = d.update(COLS, ROWS, &grid(&hot)).unwrap();
        }
        let t = found.expect("target");
        assert_eq!(t.cells, 9);
        assert!(t.cx_permille > 700, "{t:?}");
    }

    #[test]
    fn diagonal_cells_are_separate_components() {
        let mut d = seeded();
        let mut found = None;
        for _ in 0..5 {
            found = d
                .update(COLS, ROWS, &grid(&[(5, 5), (6, 6), (7, 7)]))
                .unwrap();
        }
        // Three 1-cell blobs, all below min_cells = 2.
        assert_eq!(found, None);
    }

    #[test]
    fn hysteresis_keeps_a_fading_cell_hot() {
        let config = DetectConfig {
            decision_frames: 1,
            min_cells: 1,
            k: 15, // effectively frozen baseline for this test
            ..DetectConfig::default()
        };
        let mut d = Detector::new(config);
        d.update(COLS, ROWS, &grid(&[])).unwrap();
        let mut g = grid(&[]);
        g[0] = BASE + 1200; // above t_on
        assert!(d.update(COLS, ROWS, &g).unwrap().is_some());
        g[0] = BASE + 700; // between t_off and t_on: stays hot
        assert!(d.update(COLS, ROWS, &g).unwrap().is_some());
        g[0] = BASE + 400; // below t_off: off
        assert!(d.update(COLS, ROWS, &g).unwrap().is_none());
        g[0] = BASE + 700; // between again, but was off: stays off
        assert!(d.update(COLS, ROWS, &g).unwrap().is_none());
    }

    #[test]
    fn reseed_drops_the_streak_and_baseline() {
        let mut d = seeded();
        for i in 0..4 {
            d.update(COLS, ROWS, &grid(&square(4 + i, 4, 2))).unwrap();
        }
        d.reseed();
        // First grid after re-seed only seeds, even with a blob in it.
        assert_eq!(d.update(COLS, ROWS, &grid(&square(8, 4, 2))).unwrap(), None);
        assert_eq!(d.hot_count(), 0);
    }

    #[test]
    fn baseline_absorbs_a_subject_that_stops() {
        let mut d = seeded();
        let still = grid(&square(10, 10, 3));
        let mut last = None;
        for _ in 0..60 {
            last = d.update(COLS, ROWS, &still).unwrap();
        }
        assert_eq!(last, None);
        assert_eq!(d.hot_count(), 0);
    }

    #[test]
    fn smaller_grids_are_accepted() {
        let mut d = Detector::new(DetectConfig::default());
        let (cols, rows) = (32, 16);
        d.update(cols, rows, &vec![BASE; cols * rows]).unwrap();
        let mut found = None;
        for _ in 0..5 {
            let mut g = vec![BASE; cols * rows];
            for c in [30, 31] {
                g[15 * cols + c] = HOT;
            }
            found = d.update(cols, rows, &g).unwrap();
        }
        let t = found.expect("target");
        assert_eq!((t.cx_permille, t.cy_permille), (969, 969));
    }
}
