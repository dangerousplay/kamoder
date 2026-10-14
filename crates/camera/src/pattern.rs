//! A [`Backend`] that needs no hardware: a moving NV12 test pattern
//! (CAM-08, partial).
//!
//! It is what an unknown board (`generic-v4l2` profile) and the QEMU system
//! tests run, so the service, `/status` and snapshots work end to end
//! without a sensor. There is no encoder: H.264 needs a hardware or
//! software encoder this crate does not have, so the video socket stays
//! silent and only snapshots are served.
//!
//! ```text
//! +--------------------------------------+
//! | white|yellow|cyan|green|mag|red|blue | <- 75 % colour bars, scrolling
//! |      |      |    |     |   |   |    |    left 4 px per picture
//! |##  #  ## #   #  ##   #               | <- picture number, low 32
//! +--------------------------------------+    bits, MSB first (white = 1)
//! ```
//!
//! Pictures are paced at `fps` per sensor by [`Session::with_frame`]
//! sleeping until the next one is due, like a sensor would; a caller that
//! falls behind gets the next picture at once rather than a burst.

use crate::backend::{Backend, BoxError, FrameRef, PixelFormat, SensorInfo, Session};
use std::time::{Duration, Instant};

/// BT.601 75 % colour bars, (Y, Cb, Cr), left to right.
const BARS: [(u8, u8, u8); 8] = [
    (180, 128, 128), // white
    (162, 44, 142),  // yellow
    (131, 156, 44),  // cyan
    (112, 72, 58),   // green
    (84, 184, 198),  // magenta
    (65, 100, 212),  // red
    (35, 212, 114),  // blue
    (16, 128, 128),  // black
];

/// Pixels the bars move per picture.
const SCROLL: u32 = 4;

/// Bits of the picture number drawn in the counter band.
const COUNTER_BITS: u32 = 32;

/// `sensors` test-pattern sensors of `width`×`height` NV12 at `fps`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternBackend {
    /// Board-level sensor numbers, one slot each.
    pub sensors: Vec<u32>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl PatternBackend {
    pub fn new(sensors: Vec<u32>, width: u32, height: u32, fps: u32) -> Self {
        PatternBackend {
            sensors,
            width,
            height,
            fps,
        }
    }

    fn info(&self, index: u32) -> SensorInfo {
        SensorInfo {
            index,
            width: self.width,
            height: self.height,
            active: None,
            id: None,
            tuning: None,
        }
    }
}

impl Backend for PatternBackend {
    type Session = PatternSession;

    fn sensors(&self) -> Vec<SensorInfo> {
        self.sensors.iter().map(|&i| self.info(i)).collect()
    }

    fn start(&mut self) -> Result<PatternSession, BoxError> {
        let (w, h) = (self.width, self.height);
        if w < 2 || h < 2 || w % 2 != 0 || h % 2 != 0 {
            return Err(format!("test pattern: size {w}x{h} must be even and non-zero").into());
        }
        if self.fps == 0 {
            return Err("test pattern: fps must be at least 1".into());
        }
        let started = Instant::now();
        let slots = self
            .sensors
            .iter()
            .enumerate()
            .map(|(slot, &index)| Slot {
                info: SensorInfo {
                    active: Some((w, h)),
                    id: Some("test-pattern".into()),
                    ..self.info(index)
                },
                data: vec![0; (w * h * 3 / 2) as usize],
                picture: 0,
                due: started,
                // Sensors start at different points so they are told apart.
                offset: slot as u32 * w / 3,
            })
            .collect();
        Ok(PatternSession {
            slots,
            period: Duration::from_secs(1) / self.fps,
            started,
        })
    }
}

struct Slot {
    info: SensorInfo,
    data: Vec<u8>,
    /// Number of the next picture.
    picture: u64,
    /// When the next picture is due.
    due: Instant,
    offset: u32,
}

/// Running test-pattern sensors.
pub struct PatternSession {
    slots: Vec<Slot>,
    period: Duration,
    started: Instant,
}

impl Session for PatternSession {
    fn sensors(&self) -> Vec<SensorInfo> {
        self.slots.iter().map(|s| s.info.clone()).collect()
    }

    fn with_frame(
        &mut self,
        slot: usize,
        _timeout: Duration,
        f: &mut dyn FnMut(&FrameRef<'_>) -> Result<usize, BoxError>,
    ) -> Result<usize, BoxError> {
        let period = self.period;
        let s = self
            .slots
            .get_mut(slot)
            .ok_or_else(|| format!("test pattern: no sensor slot {slot}"))?;
        let now = Instant::now();
        if s.due > now {
            std::thread::sleep(s.due - now);
        }
        // Behind by more than a picture: drop the backlog, no burst.
        let now = Instant::now();
        s.due = if now > s.due + period {
            now + period
        } else {
            s.due + period
        };
        let (w, h) = (s.info.width, s.info.height);
        let shift = (s.offset as u64 + s.picture * SCROLL as u64) % u64::from(w);
        render(&mut s.data, w, h, shift as u32, s.picture);
        let timestamp_ms = self.started.elapsed().as_millis() as u64;
        s.picture += 1;
        f(&FrameRef {
            width: w,
            height: h,
            format: PixelFormat::Nv12,
            data: &s.data,
            phys: None,
            timestamp_ms,
        })
    }
}

/// Draws picture `picture` into `data` (NV12, `w`×`h`, both even): bars
/// shifted left by `shift` pixels, and the picture number in the bottom
/// band.
fn render(data: &mut [u8], w: u32, h: u32, shift: u32, picture: u64) {
    let (w, h) = (w as usize, h as usize);
    let band = ((h / 8).max(2) & !1).min(h);
    let bars_h = h - band;
    let bar_of = |x: usize| BARS[((x + shift as usize) % w) * BARS.len() / w];
    let (luma, chroma) = data.split_at_mut(w * h);

    // Bars: every row the same.
    if bars_h > 0 {
        for (x, px) in luma[..w].iter_mut().enumerate() {
            *px = bar_of(x).0;
        }
        let (first, rest) = luma.split_at_mut(w);
        for row in rest[..(bars_h - 1) * w].chunks_exact_mut(w) {
            row.copy_from_slice(first);
        }
        for row in chroma[..bars_h / 2 * w].chunks_exact_mut(w) {
            for (n, pair) in row.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let (_, cb, cr) = bar_of(2 * n);
                *pair = [cb, cr];
            }
        }
    }

    // Counter band: the low bits of the picture number as white/black
    // cells (as many as fit, at most 32), grey after them.
    let cell = (w / COUNTER_BITS as usize).max(1);
    let bits = (w / cell).min(COUNTER_BITS as usize);
    for y in bars_h..h {
        let row = &mut luma[y * w..][..w];
        for (x, px) in row.iter_mut().enumerate() {
            let bit = x / cell;
            *px = if bit >= bits {
                128
            } else if (picture >> (bits - 1 - bit)) & 1 == 1 {
                235
            } else {
                16
            };
        }
    }
    chroma[bars_h / 2 * w..].fill(128);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Frame;

    fn grab(session: &mut PatternSession, slot: usize) -> Frame {
        let mut out = None;
        session
            .with_frame(slot, Duration::from_secs(1), &mut |f| {
                out = Some(Frame::copy_of(f));
                Ok(0)
            })
            .unwrap();
        out.unwrap()
    }

    /// Width and height from a JPEG's SOF0 segment.
    fn jpeg_size(jpeg: &[u8]) -> (u32, u32) {
        let mut i = 2;
        while i + 9 < jpeg.len() {
            assert_eq!(jpeg[i], 0xff, "marker at {i}");
            let marker = jpeg[i + 1];
            let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
            if marker == 0xc0 {
                let h = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]);
                let w = u16::from_be_bytes([jpeg[i + 7], jpeg[i + 8]]);
                return (w.into(), h.into());
            }
            i += 2 + len;
        }
        panic!("no SOF0");
    }

    #[test]
    fn pictures_move_and_are_nv12_sized() {
        let mut b = PatternBackend::new(vec![0, 1], 64, 32, 1000);
        let mut s = b.start().unwrap();
        let first = grab(&mut s, 0);
        let second = grab(&mut s, 0);
        assert_eq!((first.width, first.height), (64, 32));
        assert_eq!(first.data.len(), 64 * 32 * 3 / 2);
        assert_eq!(second.data.len(), first.data.len());
        assert_ne!(first.data, second.data, "consecutive pictures differ");
        // The other sensor is told apart.
        let other = grab(&mut s, 1);
        assert_ne!(other.data, first.data);
        let infos = s.sensors();
        assert_eq!(infos.iter().map(|i| i.index).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(infos[1].active, Some((64, 32)));
    }

    #[test]
    fn counter_band_changes_even_when_bars_wrap() {
        // 8 px wide, 4 px per picture: the bars repeat every 2 pictures.
        let mut b = PatternBackend::new(vec![0], 8, 16, 1000);
        let mut s = b.start().unwrap();
        let a = grab(&mut s, 0);
        let _ = grab(&mut s, 0);
        let c = grab(&mut s, 0);
        assert_ne!(a.data, c.data);
    }

    #[test]
    fn paces_itself_by_fps() {
        let mut b = PatternBackend::new(vec![0], 32, 16, 50);
        let mut s = b.start().unwrap();
        let t = Instant::now();
        for _ in 0..4 {
            grab(&mut s, 0);
        }
        // The first picture is due at once, then one per 20 ms.
        assert!(
            t.elapsed() >= Duration::from_millis(55),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn rejects_odd_sizes_and_zero_fps() {
        assert!(PatternBackend::new(vec![0], 33, 16, 15).start().is_err());
        assert!(PatternBackend::new(vec![0], 32, 16, 0).start().is_err());
    }

    #[test]
    fn snapshots_encode_at_the_pattern_size() {
        let mut b = PatternBackend::new(vec![0], 320, 180, 1000);
        let mut s = b.start().unwrap();
        let jpeg = crate::jpeg::encode(&grab(&mut s, 0), 80).unwrap();
        assert_eq!(jpeg_size(&jpeg), (320, 180));
    }
}
