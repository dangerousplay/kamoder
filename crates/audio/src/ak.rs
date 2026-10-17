//! The `ak-pcm` backend: Anyka's `ak_pcm.ko` character devices, through the
//! ioctl ABI in the hardware crate's `pcm` module (D-A2). No vendor library.
//!
//! Sequences, as the vendor `ipc` drove them (playback traced, capture
//! assumed symmetric until the hardware spike T0):
//!
//! ```text
//! capture   open O_RDONLY → SET_PARS {0, rate, 1, 16, period, 8, 0} → START
//!           → SET_GAIN g → read() one period at a time
//! playback  open O_WRONLY → SET_PARS {…} → START → GET_RATE → SET_SOURCE 4
//!           → SET_GAIN v → write() → SET_SPEAKER 1/0 around talk (D-A6 a)
//! ```
//!
//! The speaker ioctl lives on the playback node, so the backend remembers
//! the node the runtime has open and `set_speaker` uses it; with the node
//! closed there is nothing to switch (the driver drops the amp on release).

use crate::backend::{
    bytes_to_samples, samples_to_bytes, Backend, Capture, Error, Playback, Result, StreamParams,
};
use anyka_av100::pcm::{PcmDevice, PcmParams, MIN_CAPTURE_PERIOD}; // board-literal-ok: the ak-pcm binding (feature `ak-pcm`)
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

/// Periods in the driver ring, as the vendor used.
pub const PERIODS: u32 = 8;

/// `SET_SOURCE` value the vendor sets on playback (the speaker path).
pub const PLAYBACK_SOURCE: u32 = 4;

/// The `ak_pcm` driver.
#[derive(Debug, Default)]
pub struct AkPcmBackend {
    /// The open playback node, for `SET_SPEAKER`.
    playback: Mutex<Weak<PcmDevice>>,
}

impl AkPcmBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

fn fail(error: impl std::fmt::Display) -> Error {
    Error::new(error.to_string())
}

fn params(p: StreamParams) -> PcmParams {
    PcmParams::new(p.rate, 1, 16, p.period_bytes, PERIODS)
}

impl Backend for AkPcmBackend {
    fn open_capture(&self, device: &Path, p: StreamParams) -> Result<Box<dyn Capture>> {
        if p.period_bytes < MIN_CAPTURE_PERIOD || !p.period_bytes.is_multiple_of(2) {
            return Err(Error::new(format!(
                "capture period of {} bytes: ak_pcm needs an even size of at least {MIN_CAPTURE_PERIOD}",
                p.period_bytes
            )));
        }
        let dev = PcmDevice::open_capture(device).map_err(fail)?;
        dev.set_params(&params(p)).map_err(fail)?;
        dev.start().map_err(fail)?;
        tracing::debug!(node = %device.display(), rate = p.rate, period = p.period_bytes, "capture started");
        Ok(Box::new(AkCapture {
            dev,
            buf: vec![0; p.period_bytes as usize],
        }))
    }

    fn open_playback(&self, device: &Path, p: StreamParams) -> Result<Box<dyn Playback>> {
        let dev = PcmDevice::open_playback(device).map_err(fail)?;
        dev.set_params(&params(p)).map_err(fail)?;
        dev.start().map_err(fail)?;
        match dev.rate() {
            Ok(rate) if rate != p.rate => {
                tracing::warn!(node = %device.display(), asked = p.rate, rate, "the driver plays at another rate");
            }
            Ok(_) => {}
            Err(error) => tracing::debug!(%error, "GET_RATE failed"),
        }
        dev.set_source(PLAYBACK_SOURCE).map_err(fail)?;
        let dev = Arc::new(dev);
        *self
            .playback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Arc::downgrade(&dev);
        tracing::debug!(node = %device.display(), rate = p.rate, period = p.period_bytes, "playback started");
        Ok(Box::new(AkPlayback {
            dev,
            buf: Vec::with_capacity(p.period_bytes as usize),
        }))
    }

    fn set_speaker(&self, on: bool) -> Result<()> {
        let dev = self
            .playback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .upgrade();
        match dev {
            Some(dev) => dev.set_speaker(on).map_err(fail),
            None if !on => Ok(()),
            None => Err(Error::new("the playback node is not open")),
        }
    }
}

struct AkCapture {
    dev: PcmDevice,
    buf: Vec<u8>,
}

impl Capture for AkCapture {
    fn read(&mut self, out: &mut Vec<i16>) -> Result<()> {
        let n = self.dev.read(&mut self.buf).map_err(fail)?;
        if n < 2 {
            // Blocking reads never return empty while the stream runs.
            return Err(Error::new(format!(
                "{}: read returned {n} bytes",
                self.dev.path().display()
            )));
        }
        bytes_to_samples(&self.buf[..n & !1], out);
        Ok(())
    }

    fn set_gain(&mut self, gain: u32) -> Result<()> {
        self.dev.set_gain(gain).map_err(fail)
    }
}

struct AkPlayback {
    dev: Arc<PcmDevice>,
    buf: Vec<u8>,
}

impl Playback for AkPlayback {
    fn write(&mut self, samples: &[i16]) -> Result<()> {
        samples_to_bytes(samples, &mut self.buf);
        let mut rest = &self.buf[..];
        while !rest.is_empty() {
            let n = self.dev.write(rest).map_err(fail)?;
            if n == 0 {
                return Err(Error::new(format!(
                    "{}: write accepted nothing",
                    self.dev.path().display()
                )));
            }
            rest = &rest[n..];
        }
        Ok(())
    }

    fn set_gain(&mut self, gain: u32) -> Result<()> {
        self.dev.set_gain(gain).map_err(fail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: StreamParams = StreamParams {
        rate: 16000,
        period_bytes: 2048,
    };

    #[test]
    fn vendor_parameters() {
        assert_eq!(params(P), PcmParams::VENDOR);
    }

    #[test]
    fn errors_need_no_hardware() {
        let ak = AkPcmBackend::new();
        let small = StreamParams {
            period_bytes: 256,
            ..P
        };
        let err = ak
            .open_capture(Path::new("/dev/null"), small)
            .err()
            .unwrap();
        assert!(err.message.contains("at least 512"), "{err}");
        let err = ak
            .open_capture(Path::new("/nonexistent/mic"), P)
            .err()
            .unwrap();
        assert!(err.message.contains("/nonexistent/mic"), "{err}");
        // No playback node open: off is a no-op, on cannot work.
        assert!(ak.set_speaker(false).is_ok());
        assert!(ak.set_speaker(true).is_err());
    }
}
