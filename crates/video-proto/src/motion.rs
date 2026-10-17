//! The motion socket: the ISP's per-cell motion energy, published by the
//! producer for the `follow` service (FOL-01, FOL-02, design D-F1).
//!
//! The AV100 ISP computes a 32×24 grid of motion energy as a by-product of
//! 3D noise reduction; reading it costs one ioctl, against 1.4 MB of luma
//! for a software frame difference. Only the producer can read it (it owns
//! `/dev/isp-param-N`), so it samples the grid in its capture loop and serves
//! it here. It is a socket of its own rather than a record type on the video
//! socket so the binary H.264 framing, and every consumer of it, stay
//! untouched.
//!
//! ```text
//! -> "0\n"                                          same line as the video socket
//! <- {"seq":41,"ts_ms":1234567,"cols":32,"rows":24,"max":812,"cells":[0,3,...]}\n
//! <- {"seq":42,...}\n
//! ```
//!
//! The client sends the sensor index ([`crate::subscribe_line`]); the
//! producer then writes one [`MotionGrid`] per sample as a JSON line, at most
//! [`MAX_LINE`] bytes without the `\n`, until either side closes. An unknown
//! index gets the connection closed without an answer, as on the video
//! socket. A board whose backend has no motion statistics accepts the
//! subscription and never writes. JSON rather than a binary record: about
//! 3 KB per sample at 10 Hz is nothing on loopback, and `socat` can read it.
//!
//! `cells` is row-major, `cells[row * cols + col]`, row 0 at the top of the
//! picture. Values are the ISP's raw energy: comparable with each other and
//! with `max` (the ISP's own maximum for the sample), with no fixed unit.
//! `seq` grows by one per sample of that sensor while the producer runs (a
//! gap means samples were skipped); `ts_ms` is the capture time of the latest
//! picture, in the clock of the video socket's timestamps.

use serde::{Deserialize, Serialize};
use snafu::{ensure, ResultExt, Snafu};

/// Path of the producer's motion socket.
pub const DEFAULT_MOTION_SOCKET: &str = "/tmp/kamoder-motion.sock";

/// Widest grid accepted: the AV100 3D-NR statistics are 32 cells across.
pub const MAX_COLS: u8 = 32;
/// Tallest grid accepted: the AV100 3D-NR statistics are 24 cells high.
pub const MAX_ROWS: u8 = 24;

/// Longest line accepted, bytes, without the `\n`. A full 32×24 grid of
/// five-digit values is about 4.7 KB; anything longer is not a grid.
pub const MAX_LINE: usize = 8 * 1024;

#[derive(Debug, Snafu)]
pub enum Error {
    /// The dimensions are out of range or do not match the cells.
    #[snafu(display(
        "motion grid {cols}x{rows} with {len} cells (want cols 1..={MAX_COLS}, rows 1..={MAX_ROWS}, cols*rows cells)"
    ))]
    Dimensions { cols: u8, rows: u8, len: usize },

    /// The line exceeded [`MAX_LINE`].
    #[snafu(display("motion line of {len} bytes exceeds {MAX_LINE}"))]
    TooLong { len: usize },

    #[snafu(display("cannot encode the motion grid: {source}"))]
    Encode { source: serde_json::Error },

    #[snafu(display("cannot decode the motion line: {source}"))]
    Decode { source: serde_json::Error },
}

type Result<T, E = Error> = std::result::Result<T, E>;

/// One sample of a sensor's motion statistics.
///
/// Every way of building one ([`MotionGrid::new`], deserialising) checks the
/// dimensions; the fields stay public for readers, and whoever changes them
/// keeps `cols * rows == cells.len()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Unchecked")]
pub struct MotionGrid {
    /// Sample number of this sensor, +1 per sample.
    pub seq: u64,
    /// Capture time of the latest picture, ms (video socket clock).
    pub ts_ms: u64,
    pub cols: u8,
    pub rows: u8,
    /// The ISP's maximum cell energy for this sample.
    pub max: u16,
    /// Row-major energy per cell, `cols * rows` of them.
    pub cells: Vec<u16>,
}

/// The wire shape before its dimensions are checked.
#[derive(Deserialize)]
struct Unchecked {
    seq: u64,
    ts_ms: u64,
    cols: u8,
    rows: u8,
    max: u16,
    cells: Vec<u16>,
}

impl TryFrom<Unchecked> for MotionGrid {
    type Error = Error;

    fn try_from(u: Unchecked) -> Result<MotionGrid> {
        MotionGrid::new(u.seq, u.ts_ms, u.cols, u.rows, u.max, u.cells)
    }
}

impl MotionGrid {
    /// A grid of `cols`×`rows` cells; fails unless `1..=MAX_COLS`,
    /// `1..=MAX_ROWS` and `cells.len() == cols * rows`.
    pub fn new(
        seq: u64,
        ts_ms: u64,
        cols: u8,
        rows: u8,
        max: u16,
        cells: Vec<u16>,
    ) -> Result<MotionGrid> {
        ensure!(
            (1..=MAX_COLS).contains(&cols)
                && (1..=MAX_ROWS).contains(&rows)
                && cells.len() == usize::from(cols) * usize::from(rows),
            DimensionsSnafu {
                cols,
                rows,
                len: cells.len()
            }
        );
        Ok(MotionGrid {
            seq,
            ts_ms,
            cols,
            rows,
            max,
            cells,
        })
    }

    /// Energy of the cell at `col`, `row`; `None` outside the grid.
    pub fn cell(&self, col: u8, row: u8) -> Option<u16> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        self.cells
            .get(usize::from(row) * usize::from(self.cols) + usize::from(col))
            .copied()
    }
}

/// Writes `grid` as one JSON line, `\n` included, into `out` (cleared
/// first, so a connection can reuse one buffer).
pub fn encode_line(grid: &MotionGrid, out: &mut Vec<u8>) -> Result<()> {
    out.clear();
    serde_json::to_writer(&mut *out, grid).context(EncodeSnafu)?;
    out.push(b'\n');
    Ok(())
}

/// Reads one line (with or without its `\n`) back into a checked grid.
pub fn decode_line(line: &[u8]) -> Result<MotionGrid> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    ensure!(line.len() <= MAX_LINE, TooLongSnafu { len: line.len() });
    serde_json::from_slice(line).context(DecodeSnafu)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> MotionGrid {
        let cells = (0..768u32).map(|i| (i * 85) as u16).collect();
        MotionGrid::new(42, 1_700_000_000_123, 32, 24, 65_535, cells).unwrap()
    }

    #[test]
    fn line_roundtrip() {
        let mut buf = vec![b'x'; 3];
        for g in [
            full(),
            MotionGrid::new(0, 0, 1, 1, 0, vec![0]).unwrap(),
            MotionGrid::new(u64::MAX, u64::MAX, 32, 16, 7, vec![u16::MAX; 512]).unwrap(),
        ] {
            encode_line(&g, &mut buf).unwrap();
            assert_eq!(buf.last(), Some(&b'\n'));
            assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 1, "one line");
            assert!(buf.len() <= MAX_LINE + 1, "{} bytes", buf.len());
            assert_eq!(decode_line(&buf).unwrap(), g);
            assert_eq!(decode_line(&buf[..buf.len() - 1]).unwrap(), g, "no \\n");
        }
    }

    #[test]
    fn wire_shape() {
        let g = MotionGrid::new(3, 9, 2, 1, 5, vec![1, 5]).unwrap();
        let mut buf = Vec::new();
        encode_line(&g, &mut buf).unwrap();
        assert_eq!(
            std::str::from_utf8(&buf).unwrap(),
            "{\"seq\":3,\"ts_ms\":9,\"cols\":2,\"rows\":1,\"max\":5,\"cells\":[1,5]}\n"
        );
        assert_eq!(g.cell(1, 0), Some(5));
        assert_eq!((g.cell(2, 0), g.cell(0, 1)), (None, None));
        assert_eq!(full().cell(31, 23), Some((767u32 * 85) as u16));
    }

    #[test]
    fn rejects_bad_dimensions() {
        for (cols, rows, len) in [
            (0, 1, 0),
            (1, 0, 0),
            (33, 1, 33),
            (1, 25, 25),
            (32, 24, 767),
            (32, 24, 769),
            (2, 2, 3),
        ] {
            assert!(
                matches!(
                    MotionGrid::new(0, 0, cols, rows, 0, vec![0; len]),
                    Err(Error::Dimensions { .. })
                ),
                "{cols}x{rows} with {len}"
            );
        }
        // Deserialising checks too.
        for bad in [
            r#"{"seq":0,"ts_ms":0,"cols":2,"rows":2,"max":0,"cells":[1,2,3]}"#,
            r#"{"seq":0,"ts_ms":0,"cols":40,"rows":1,"max":0,"cells":[]}"#,
            r#"{"seq":0,"ts_ms":0,"cols":1,"rows":1,"max":0}"#,
            r#"{"seq":0,"ts_ms":0,"cols":1,"rows":1,"max":0,"cells":[70000]}"#,
            "not json",
        ] {
            assert!(
                matches!(decode_line(bad.as_bytes()), Err(Error::Decode { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn rejects_long_lines() {
        let long = vec![b' '; MAX_LINE + 1];
        assert!(matches!(
            decode_line(&long),
            Err(Error::TooLong { len }) if len == MAX_LINE + 1
        ));
    }

    #[test]
    fn subscribe_line_and_socket() {
        assert_eq!(DEFAULT_MOTION_SOCKET, "/tmp/kamoder-motion.sock");
        assert_eq!(crate::DEFAULT_MOTION_SOCKET, DEFAULT_MOTION_SOCKET);
        // Same subscribe line as the video socket.
        assert_eq!(crate::parse_subscribe(&crate::subscribe_line(1)), Some(1));
    }
}
