//! A codec without hardware: host tests, and boards whose `[audio] backend`
//! is `fake` (development, the itest VM).
//!
//! Capture produces a sine or silence, one period per `read`, paced by wall
//! time like the real driver (or as fast as asked, unpaced). Playback
//! records what it is given; the speaker switch records every toggle. Each
//! open, read and playback open can be scripted to fail, to drive the
//! error and retry paths (AUD-05).
//!
//! Clones share state, so a test keeps one clone and hands another to the
//! runtime.

use crate::backend::{Backend, Capture, Error, Playback, Result, StreamParams};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

/// What the fake microphone hears.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    Silence,
    /// A tone at `hz`, peak `amplitude`.
    Sine {
        hz: f64,
        amplitude: f64,
    },
}

#[derive(Debug)]
struct State {
    signal: Signal,
    paced: bool,
    fail_capture_opens: u32,
    fail_reads: u32,
    fail_playback_opens: u32,
    fail_writes: u32,
    capture_opens: u32,
    capturing: bool,
    playback_opens: u32,
    playing: bool,
    played: Vec<i16>,
    speaker: Vec<bool>,
    gains: Vec<(PathBuf, u32)>,
}

/// The fake codec; see the module docs.
#[derive(Debug, Clone)]
pub struct FakeBackend {
    state: Arc<Mutex<State>>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new(Signal::Silence)
    }
}

impl FakeBackend {
    /// Paced capture of `signal`.
    pub fn new(signal: Signal) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                signal,
                paced: true,
                fail_capture_opens: 0,
                fail_reads: 0,
                fail_playback_opens: 0,
                fail_writes: 0,
                capture_opens: 0,
                capturing: false,
                playback_opens: 0,
                playing: false,
                played: Vec::new(),
                speaker: Vec::new(),
                gains: Vec::new(),
            })),
        }
    }

    /// `false`: reads return at once instead of one period of wall time.
    pub fn paced(self, paced: bool) -> Self {
        self.lock().paced = paced;
        self
    }

    /// The next `n` capture opens fail.
    pub fn fail_capture_opens(&self, n: u32) {
        self.lock().fail_capture_opens = n;
    }

    /// The next `n` reads fail.
    pub fn fail_reads(&self, n: u32) {
        self.lock().fail_reads = n;
    }

    /// The next `n` playback opens fail.
    pub fn fail_playback_opens(&self, n: u32) {
        self.lock().fail_playback_opens = n;
    }

    /// The next `n` writes fail.
    pub fn fail_writes(&self, n: u32) {
        self.lock().fail_writes = n;
    }

    /// Successful capture opens so far.
    pub fn capture_opens(&self) -> u32 {
        self.lock().capture_opens
    }

    /// A capture handle is open.
    pub fn capturing(&self) -> bool {
        self.lock().capturing
    }

    /// Successful playback opens so far.
    pub fn playback_opens(&self) -> u32 {
        self.lock().playback_opens
    }

    /// A playback handle is open.
    pub fn playing(&self) -> bool {
        self.lock().playing
    }

    /// Everything written to the speaker, at the hardware rate.
    pub fn played(&self) -> Vec<i16> {
        self.lock().played.clone()
    }

    /// Every `set_speaker` call, in order.
    pub fn speaker_log(&self) -> Vec<bool> {
        self.lock().speaker.clone()
    }

    /// The last `set_speaker` value (off before any).
    pub fn speaker_on(&self) -> bool {
        self.lock().speaker.last().copied().unwrap_or(false)
    }

    /// Every gain applied, as (node, value).
    pub fn gains(&self) -> Vec<(PathBuf, u32)> {
        self.lock().gains.clone()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Decrements `counter` and fails with `what` while it is non-zero.
fn scripted(counter: &mut u32, what: &str) -> Result<()> {
    if *counter > 0 {
        *counter -= 1;
        return Err(Error::new(format!("fake: {what} failed (scripted)")));
    }
    Ok(())
}

impl Backend for FakeBackend {
    fn open_capture(&self, device: &Path, params: StreamParams) -> Result<Box<dyn Capture>> {
        let mut s = self.lock();
        scripted(&mut s.fail_capture_opens, "capture open")?;
        if s.capturing {
            return Err(Error::new(format!(
                "fake: {} is already open",
                device.display()
            )));
        }
        s.capture_opens += 1;
        s.capturing = true;
        Ok(Box::new(FakeCapture {
            fake: self.clone(),
            device: device.to_path_buf(),
            params,
            n: 0,
            next: None,
        }))
    }

    fn open_playback(&self, device: &Path, _params: StreamParams) -> Result<Box<dyn Playback>> {
        let mut s = self.lock();
        scripted(&mut s.fail_playback_opens, "playback open")?;
        s.playback_opens += 1;
        s.playing = true;
        Ok(Box::new(FakePlayback {
            fake: self.clone(),
            device: device.to_path_buf(),
        }))
    }

    fn set_speaker(&self, on: bool) -> Result<()> {
        self.lock().speaker.push(on);
        Ok(())
    }
}

struct FakeCapture {
    fake: FakeBackend,
    device: PathBuf,
    params: StreamParams,
    /// Samples produced so far (the sine's phase).
    n: u64,
    /// When the next period is due, when paced.
    next: Option<Instant>,
}

impl Capture for FakeCapture {
    fn read(&mut self, out: &mut Vec<i16>) -> Result<()> {
        let (signal, paced) = {
            let mut s = self.fake.lock();
            scripted(&mut s.fail_reads, "read")?;
            (s.signal, s.paced)
        };
        if paced {
            let period = self.params.period();
            let due = *self.next.get_or_insert_with(Instant::now) + period;
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
            // After a long stall, resume from now rather than catching up.
            self.next = Some(if now > due + period * 4 { now } else { due });
        }
        let rate = f64::from(self.params.rate);
        for _ in 0..self.params.period_samples() {
            let sample = match signal {
                Signal::Silence => 0,
                Signal::Sine { hz, amplitude } => {
                    let t = self.n as f64 / rate;
                    (amplitude * (2.0 * std::f64::consts::PI * hz * t).sin()).round() as i16
                }
            };
            out.push(sample);
            self.n += 1;
        }
        Ok(())
    }

    fn set_gain(&mut self, gain: u32) -> Result<()> {
        self.fake.lock().gains.push((self.device.clone(), gain));
        Ok(())
    }
}

impl Drop for FakeCapture {
    fn drop(&mut self) {
        self.fake.lock().capturing = false;
    }
}

struct FakePlayback {
    fake: FakeBackend,
    device: PathBuf,
}

impl Playback for FakePlayback {
    fn write(&mut self, samples: &[i16]) -> Result<()> {
        let mut s = self.fake.lock();
        scripted(&mut s.fail_writes, "write")?;
        s.played.extend_from_slice(samples);
        Ok(())
    }

    fn set_gain(&mut self, gain: u32) -> Result<()> {
        self.fake.lock().gains.push((self.device.clone(), gain));
        Ok(())
    }
}

impl Drop for FakePlayback {
    fn drop(&mut self) {
        self.fake.lock().playing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn params() -> StreamParams {
        StreamParams {
            rate: 16000,
            period_bytes: 640,
        }
    }

    #[test]
    fn capture_is_paced_and_continuous() {
        let fake = FakeBackend::new(Signal::Sine {
            hz: 1000.0,
            amplitude: 1000.0,
        });
        let mut cap = fake.open_capture(Path::new("mic"), params()).unwrap();
        assert!(fake.capturing());
        assert!(fake.open_capture(Path::new("mic"), params()).is_err());
        let start = Instant::now();
        let mut out = Vec::new();
        for _ in 0..5 {
            cap.read(&mut out).unwrap();
        }
        // 5 × 20 ms, the first one included.
        assert!(start.elapsed() >= Duration::from_millis(90));
        assert_eq!(out.len(), 5 * 320);
        // 16 samples per cycle of 1 kHz at 16 kHz, across the read seams.
        assert_eq!(out[0], 0);
        assert_eq!(out[4], 1000);
        assert_eq!(out[320 + 4], 1000);
        drop(cap);
        assert!(!fake.capturing());
        assert_eq!(fake.capture_opens(), 1);
    }

    #[test]
    fn scripted_failures_and_records() {
        let fake = FakeBackend::default().paced(false);
        fake.fail_capture_opens(1);
        assert!(fake.open_capture(Path::new("mic"), params()).is_err());
        let mut cap = fake.open_capture(Path::new("mic"), params()).unwrap();
        fake.fail_reads(1);
        let mut out = Vec::new();
        assert!(cap.read(&mut out).is_err());
        cap.read(&mut out).unwrap();
        assert!(out.iter().all(|&s| s == 0));
        cap.set_gain(3).unwrap();

        fake.fail_playback_opens(1);
        assert!(fake.open_playback(Path::new("spk"), params()).is_err());
        let mut play = fake.open_playback(Path::new("spk"), params()).unwrap();
        play.write(&[1, 2, 3]).unwrap();
        fake.fail_writes(1);
        assert!(play.write(&[4]).is_err());
        fake.set_speaker(true).unwrap();
        fake.set_speaker(false).unwrap();
        assert_eq!(fake.played(), [1, 2, 3]);
        assert_eq!(fake.speaker_log(), [true, false]);
        assert!(!fake.speaker_on());
        assert_eq!(fake.gains(), [(PathBuf::from("mic"), 3)]);
        assert!(fake.playing());
        drop(play);
        assert!(!fake.playing());
    }
}
