//! Scripted [`Backend`] and [`Encoder`] for host tests (CAM-05): no device,
//! no timing beyond a short sleep per picture, failures on demand.
//!
//! Built for this crate's tests and, with the `test-support` feature, for
//! downstream crates (camerad) that want to drive the service end to end.

use crate::backend::{
    Backend, BoxError, Encoder, FrameRef, Packet, PixelFormat, SensorInfo, Session,
};
// AtomicUsize, not AtomicU64: ARMv5TE has no 64-bit atomics, and this module
// is also built for the camera with the `test-support` feature.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// What a [`FakeBackend`] has done so far, shared with the test.
#[derive(Debug, Default)]
pub struct FakeBackendStats {
    /// Calls to `start`, failed ones included.
    pub starts: AtomicUsize,
    /// Pictures lent, all sensors.
    pub frames: AtomicUsize,
}

/// `sensors` sensors of `width`×`height` NV12, one picture per `interval`.
#[derive(Debug, Clone)]
pub struct FakeBackend {
    pub sensors: usize,
    pub width: u32,
    pub height: u32,
    pub interval: Duration,
    /// The first this many `start` calls fail.
    pub fail_starts: usize,
    /// `with_frame` fails once, when this many pictures were lent in total.
    pub fail_frame: Option<usize>,
    pub stats: Arc<FakeBackendStats>,
}

impl FakeBackend {
    pub fn new(sensors: usize, width: u32, height: u32) -> Self {
        FakeBackend {
            sensors,
            width,
            height,
            interval: Duration::from_millis(2),
            fail_starts: 0,
            fail_frame: None,
            stats: Arc::default(),
        }
    }

    fn info(&self, slot: usize) -> SensorInfo {
        SensorInfo {
            index: slot as u32,
            width: self.width,
            height: self.height,
            active: None,
            id: None,
            tuning: Some(format!("fake{slot}.conf")),
        }
    }
}

impl Backend for FakeBackend {
    type Session = FakeSession;

    fn sensors(&self) -> Vec<SensorInfo> {
        (0..self.sensors).map(|i| self.info(i)).collect()
    }

    fn start(&mut self) -> Result<FakeSession, BoxError> {
        let n = self.stats.starts.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= self.fail_starts {
            return Err(format!("fake: start failed (attempt {n})").into());
        }
        let sensors = (0..self.sensors)
            .map(|i| SensorInfo {
                active: Some((self.width * 2, self.height * 2)),
                id: Some(format!("{:#06x}", 0xfa00 + i)),
                ..self.info(i)
            })
            .collect();
        Ok(FakeSession {
            sensors,
            data: vec![0; (self.width * self.height * 3 / 2) as usize],
            backend: self.clone(),
        })
    }
}

/// Started fake sensors.
pub struct FakeSession {
    sensors: Vec<SensorInfo>,
    data: Vec<u8>,
    backend: FakeBackend,
}

impl Session for FakeSession {
    fn sensors(&self) -> Vec<SensorInfo> {
        self.sensors.clone()
    }

    fn with_frame(
        &mut self,
        slot: usize,
        _timeout: Duration,
        f: &mut dyn FnMut(&FrameRef<'_>) -> Result<usize, BoxError>,
    ) -> Result<usize, BoxError> {
        std::thread::sleep(self.backend.interval);
        let n = self.backend.stats.frames.fetch_add(1, Ordering::Relaxed);
        if self.backend.fail_frame == Some(n) {
            return Err(format!("fake: sensor {slot}: no frame").into());
        }
        // Luma tracks the picture number so pictures differ.
        let luma = (self.backend.width * self.backend.height) as usize;
        self.data[..luma].fill(n as u8);
        f(&FrameRef {
            width: self.backend.width,
            height: self.backend.height,
            format: PixelFormat::Nv12,
            data: &self.data,
            phys: None,
            timestamp_ms: n as u64 * self.backend.interval.as_millis() as u64,
        })
    }
}

/// What a [`FakeEncoder`] has done so far, shared with the test.
#[derive(Debug, Default)]
pub struct FakeEncoderStats {
    pub encoded: AtomicUsize,
    pub keyframes: AtomicUsize,
    pub idr_requests: AtomicUsize,
}

/// Emits small fake Annex-B access units: SPS + PPS + IDR slice on the
/// first picture, after `request_idr` and every `gop` pictures; a non-IDR
/// slice otherwise. Each carries the picture number.
#[derive(Debug)]
pub struct FakeEncoder {
    pub width: u32,
    pub height: u32,
    pub gop: u64,
    pub stats: Arc<FakeEncoderStats>,
    frames: u64,
    idr: bool,
    buf: Vec<u8>,
}

/// SPS + PPS that open every fake IDR access unit.
pub const FAKE_PARAMETER_SETS: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x4d, 0x00, 0x28, 0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80,
];

impl FakeEncoder {
    pub fn new(width: u32, height: u32, gop: u64) -> Self {
        FakeEncoder {
            width,
            height,
            gop: gop.max(1),
            stats: Arc::default(),
            frames: 0,
            idr: true,
            buf: Vec::new(),
        }
    }
}

impl Encoder for FakeEncoder {
    fn info(&self) -> video_proto::StreamInfo {
        video_proto::StreamInfo {
            codec: "H.264".into(),
            profile: "Main".into(),
            width: self.width,
            height: self.height,
            fps: 15,
            gop: self.gop as u32,
            target_kbps: 512,
            kbps: 0.0,
            encoder: "fake".into(),
        }
    }

    fn encode(
        &mut self,
        frame: &FrameRef<'_>,
        out: &mut dyn FnMut(Packet<'_>),
    ) -> Result<usize, BoxError> {
        let want = (self.width * self.height * 3 / 2) as usize;
        if frame.data.len() != want {
            return Err(format!(
                "fake encoder: frame is {} bytes, expected {want}",
                frame.data.len()
            )
            .into());
        }
        let keyframe = std::mem::take(&mut self.idr) || self.frames.is_multiple_of(self.gop);
        self.buf.clear();
        if keyframe {
            self.buf.extend_from_slice(FAKE_PARAMETER_SETS);
            self.buf.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        } else {
            self.buf.extend_from_slice(&[0, 0, 0, 1, 0x41]);
        }
        self.buf.extend_from_slice(&self.frames.to_be_bytes());
        self.frames += 1;
        self.stats.encoded.fetch_add(1, Ordering::Relaxed);
        if keyframe {
            self.stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        out(Packet {
            data: &self.buf,
            keyframe,
        });
        Ok(self.buf.len())
    }

    fn request_idr(&mut self) -> Result<(), BoxError> {
        self.idr = true;
        self.stats.idr_requests.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_keyframes_on_first_request_and_gop() {
        let mut e = FakeEncoder::new(4, 2, 3);
        let data = vec![0; 12];
        let frame = FrameRef {
            width: 4,
            height: 2,
            format: PixelFormat::Nv12,
            data: &data,
            phys: None,
            timestamp_ms: 0,
        };
        let mut keys = Vec::new();
        for n in 0..7 {
            if n == 4 {
                e.request_idr().unwrap();
            }
            e.encode(&frame, &mut |p| keys.push(p.keyframe)).unwrap();
        }
        assert_eq!(keys, [true, false, false, true, true, false, true]);
        assert!(e
            .encode(&FrameRef { data: &[], ..frame }, &mut |_| {})
            .is_err());
    }

    #[test]
    fn backend_fails_scripted_starts() {
        let mut b = FakeBackend {
            fail_starts: 1,
            ..FakeBackend::new(2, 4, 2)
        };
        assert!(b.start().is_err());
        let mut s = b.start().unwrap();
        assert_eq!(s.sensors().len(), 2);
        let len = s
            .with_frame(1, Duration::ZERO, &mut |f| Ok(f.data.len()))
            .unwrap();
        assert_eq!(len, 12);
        assert_eq!(b.stats.starts.load(Ordering::Relaxed), 2);
    }
}
