//! Where frames come from.
//!
//! [`VideoSource`] is the seam between the RTSP server and the encoder. Today
//! the only implementation is [`FileSource`], an Annex-B `.h264` file played
//! in a loop, which is what the host tests and the qemu runs use. Phase C adds
//! the camera's hardware encoder (`/dev/venc`, fed by V4L2 capture) as a
//! second implementation of the same trait, selected with `--source venc:…`;
//! nothing in the server changes for it.

use crate::h264::{nal_units, AccessUnitSplitter, Frame};
use bytes::Bytes;
use snafu::{ensure, ResultExt, Snafu};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The RTP clock rate for video (RFC 6184 § 8.2.1).
pub const CLOCK_RATE: u64 = 90_000;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot read {}: {source}", path.display()))]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("{} has no H.264 access units (not an Annex-B stream?)", path.display()))]
    NoFrames { path: PathBuf },

    #[snafu(display("{} has no IDR picture, so no client could ever start decoding", path.display()))]
    NoKeyframe { path: PathBuf },

    #[snafu(display("frame rate must be between 1 and 120, not {fps}"))]
    FrameRate { fps: u32 },

    #[snafu(display("unknown source {spec:?}; expected file:PATH"))]
    Spec { spec: String },
}

/// A live H.264 elementary stream, one access unit at a time.
///
/// `next_frame` paces itself: it resolves when the next picture is due, as an
/// encoder does. Timestamps are on the 90 kHz clock and increase
/// monotonically; the data is Annex-B with start codes. `Ok(None)` ends the
/// stream. Implementations must not block the runtime thread.
pub trait VideoSource: Send + 'static {
    fn next_frame(&mut self) -> impl Future<Output = Result<Option<Frame>, Error>> + Send;
}

/// Which source to open, parsed from `--source`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceSpec {
    /// `file:PATH` — an Annex-B `.h264` file, looped.
    File(PathBuf),
}

impl std::str::FromStr for SourceSpec {
    type Err = Error;

    fn from_str(spec: &str) -> Result<Self, Error> {
        match spec.split_once(':') {
            Some(("file", path)) if !path.is_empty() => Ok(SourceSpec::File(path.into())),
            _ => SpecSnafu { spec }.fail(),
        }
    }
}

/// An Annex-B file played in a loop at a fixed frame rate.
///
/// The whole file is read and split into access units up front: it is a test
/// clip, and splitting once keeps the per-frame work to a clone of `Bytes`.
pub struct FileSource {
    frames: Vec<(Bytes, bool)>,
    next: usize,
    /// Frames emitted so far; the timestamp is derived from it so it never
    /// drifts, whatever the frame rate.
    count: u64,
    fps: u64,
    ticker: tokio::time::Interval,
}

impl FileSource {
    pub fn open(path: &Path, fps: u32) -> Result<Self, Error> {
        let data = std::fs::read(path).context(ReadSnafu { path })?;
        Self::from_bytes(Bytes::from(data), fps, path)
    }

    /// Splits `data`; `path` only names it in errors.
    pub fn from_bytes(data: Bytes, fps: u32, path: &Path) -> Result<Self, Error> {
        ensure!((1..=120).contains(&fps), FrameRateSnafu { fps });
        let mut splitter = AccessUnitSplitter::default();
        let mut frames: Vec<(Bytes, bool)> = nal_units(&data)
            .filter_map(|nal| splitter.push(&nal))
            .collect();
        frames.extend(splitter.finish());
        ensure!(!frames.is_empty(), NoFramesSnafu { path });
        // Start at the first IDR; anything before it is undecodable.
        let first = frames
            .iter()
            .position(|(_, key)| *key)
            .ok_or_else(|| NoKeyframeSnafu { path }.build())?;
        frames.drain(..first);
        let mut ticker = tokio::time::interval(Duration::from_secs(1) / fps);
        // A stalled runtime should not answer with a burst of catch-up frames.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Ok(Self {
            frames,
            next: 0,
            count: 0,
            fps: fps.into(),
            ticker,
        })
    }

    /// Access units in one pass of the file.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

impl VideoSource for FileSource {
    async fn next_frame(&mut self) -> Result<Option<Frame>, Error> {
        self.ticker.tick().await;
        let (data, keyframe) = self.frames[self.next].clone();
        self.next = (self.next + 1) % self.frames.len();
        let pts = self.count * CLOCK_RATE / self.fps;
        self.count += 1;
        Ok(Some(Frame {
            data,
            pts,
            keyframe,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip() -> FileSource {
        let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/clip.h264"));
        FileSource::open(path, 25).unwrap()
    }

    #[test]
    fn source_specs() {
        assert_eq!(
            "file:/tmp/a.h264".parse::<SourceSpec>().unwrap(),
            SourceSpec::File("/tmp/a.h264".into())
        );
        assert!("file:".parse::<SourceSpec>().is_err());
        assert!("venc:0".parse::<SourceSpec>().is_err());
        assert!("clip.h264".parse::<SourceSpec>().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn the_test_clip_loops_with_steady_timestamps() {
        let mut source = clip();
        // 2 s at 25 fps, GOP 25.
        assert_eq!(source.len(), 50);
        let mut keyframes = Vec::new();
        for index in 0..101u64 {
            let frame = source.next_frame().await.unwrap().unwrap();
            assert_eq!(frame.pts, index * 3600);
            if frame.keyframe {
                keyframes.push(index);
            }
        }
        assert_eq!(keyframes, [0, 25, 50, 75, 100]);
    }

    #[test]
    fn rejects_garbage_and_streams_without_idr() {
        let path = Path::new("x.h264");
        assert!(matches!(
            FileSource::from_bytes(Bytes::from_static(b"not h264"), 25, path),
            Err(Error::NoFrames { .. })
        ));
        let only_p = Bytes::from_static(&[0, 0, 0, 1, 0x41, 0x9a, 0, 0, 0, 1, 0x41, 0x9a]);
        assert!(matches!(
            FileSource::from_bytes(only_p, 25, path),
            Err(Error::NoKeyframe { .. })
        ));
        assert!(matches!(
            FileSource::from_bytes(Bytes::new(), 0, path),
            Err(Error::FrameRate { fps: 0 })
        ));
    }
}
