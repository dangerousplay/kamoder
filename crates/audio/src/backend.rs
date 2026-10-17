//! The codec seam (AUD-06): everything the service needs from audio hardware.
//!
//! Calls are synchronous and may block: `read` waits for the driver's next
//! period, `write` for room in its ring (neither `ak_pcm` node supports
//! `poll`). The runtime therefore calls them from dedicated threads only,
//! never from the tokio thread; `open_*` and `set_speaker` are quick ioctls
//! and are called from async code.
//!
//! Gain (capture) and volume (playback) are per open node in `ak_pcm`, so
//! they live on the open handles; the runtime applies the stored value right
//! after every open and again whenever `set` changes it.
//!
//! Backends: [`crate::fake::FakeBackend`] (host tests, boards without a
//! codec driver) and, behind feature `ak-pcm`, [`crate::ak::AkPcmBackend`].

use snafu::Snafu;
use std::path::Path;

/// A failed open, ioctl, read or write, with the driver's reason.
#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
#[snafu(display("{message}"))]
pub struct Error {
    pub message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What a node is opened with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamParams {
    /// Hardware rate, Hz (the board's `[audio] rate`).
    pub rate: u32,
    /// Bytes per driver period (S16 mono: two per sample).
    pub period_bytes: u32,
}

impl StreamParams {
    /// Samples per period.
    pub fn period_samples(&self) -> usize {
        self.period_bytes as usize / 2
    }

    /// Wall time of one period.
    pub fn period(&self) -> std::time::Duration {
        std::time::Duration::from_micros(
            self.period_samples() as u64 * 1_000_000 / u64::from(self.rate.max(1)),
        )
    }
}

/// The codec driver.
pub trait Backend: Send + Sync + 'static {
    /// Opens and starts the microphone node at `device`.
    fn open_capture(&self, device: &Path, params: StreamParams) -> Result<Box<dyn Capture>>;

    /// Opens and starts the speaker node at `device`.
    fn open_playback(&self, device: &Path, params: StreamParams) -> Result<Box<dyn Playback>>;

    /// Speaker amplifier on/off through the driver (D-A6 option a). May
    /// need an open playback node.
    fn set_speaker(&self, on: bool) -> Result<()>;
}

/// An open, started microphone.
pub trait Capture: Send {
    /// Blocks for the next period and appends its mono samples to `out`.
    fn read(&mut self, out: &mut Vec<i16>) -> Result<()>;

    /// Analog gain, driver units.
    fn set_gain(&mut self, gain: u32) -> Result<()>;
}

/// An open, started speaker.
pub trait Playback: Send {
    /// Blocks until all of `samples` (mono, hardware rate) is queued.
    fn write(&mut self, samples: &[i16]) -> Result<()>;

    /// Volume, driver units.
    fn set_gain(&mut self, gain: u32) -> Result<()>;
}

/// S16LE bytes to samples, appended to `out`.
pub fn bytes_to_samples(bytes: &[u8], out: &mut Vec<i16>) {
    out.extend(
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| i16::from_le_bytes(b)),
    );
}

/// Samples to S16LE bytes, replacing `out`'s contents.
pub fn samples_to_bytes(samples: &[i16], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_arithmetic() {
        let p = StreamParams {
            rate: 16000,
            period_bytes: 2048,
        };
        assert_eq!(p.period_samples(), 1024);
        assert_eq!(p.period().as_millis(), 64);
    }

    #[test]
    fn s16le_round_trip() {
        let samples = [0i16, 1, -1, i16::MAX, i16::MIN];
        let mut bytes = vec![9];
        samples_to_bytes(&samples, &mut bytes);
        assert_eq!(&bytes[..4], &[0, 0, 1, 0]);
        let mut back = Vec::new();
        bytes_to_samples(&bytes, &mut back);
        assert_eq!(back, samples);
    }
}
