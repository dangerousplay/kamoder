//! The AV100 audio codec (`ak_pcm.ko`), driven directly: no ALSA, no vendor
//! library.
//!
//! The module registers three character devices (major 248), one per path
//! through the codec:
//!
//! ```text
//! /dev/pcmC0D0p  playback   write() interleaved samples, opened O_WRONLY
//! /dev/pcmC0D0c  capture    read()  interleaved samples, opened O_RDONLY
//! /dev/pcmC0D0l  loopback   read()  what is being played, opened O_RDWR
//! ```
//!
//! There is no `mmap` and no `poll`: reads and writes block on the driver's
//! period ring. Capture admits one opener; a second `open` fails while
//! another process (the vendor `ipc`, or another Kamoder service) holds it.
//!
//! Sources, so the guesses are visible:
//! - disassembly of the unstripped `ak_pcm.ko` (ioctl switch, the 512-byte
//!   minimum capture period, the open-once check);
//! - the vendor application's ioctl trace (`tools/re/traces/ipcrun/ipc.trace`
//!   in the RE workspace, not this repository): capture `GET_SOURCE`/`GET_GAIN`
//!   at line 4700, playback `SET_PARS {0,16000,1,16,2048,8,0}` → `START` →
//!   `0x800450ea` → `SET_SOURCE 4` → `SET_GAIN 3` from line 5208;
//! - `dmesg` of the same boot (module banner, node creation).
//!
//! Commands named `IOC_50Ex_UNKNOWN` were found in the driver but never seen
//! used; they are exported only for experiments ([`PcmDevice::ioctl_none`],
//! [`PcmDevice::ioctl_words`]). Nothing here is verified on hardware yet
//! (audio spike T0).

use crate::v4l2;
use snafu::{ensure, ResultExt, Snafu};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Default capture node.
pub const CAPTURE_NODE: &str = "/dev/pcmC0D0c";
/// Default playback node.
pub const PLAYBACK_NODE: &str = "/dev/pcmC0D0p";
/// Default loopback node (reads back what the playback path plays).
pub const LOOPBACK_NODE: &str = "/dev/pcmC0D0l";

/// `_IOW('P', 0xe0, 28)`: set [`PcmParams`]. Traced (ipc.trace:5208).
pub const SET_PARS: u32 = 0x401c_50e0;
/// `_IOR('P', 0xf2, 28)`: read [`PcmParams`] back. From the driver's switch;
/// the layout is inferred to match `SET_PARS` (same size), not traced.
pub const GET_PARS: u32 = 0x801c_50f2;
/// `_IOW('P', 0x10, u32)`: select the input/output path. The vendor sets 4
/// on playback; capture reported 2.
pub const SET_SOURCE: u32 = 0x4004_5010;
/// `_IOR('P', 0x10, u32)`: current path.
pub const GET_SOURCE: u32 = 0x8004_5010;
/// `_IOW('P', 0x30, u32)`: analog gain (capture) / volume (playback), in the
/// driver's own steps. The vendor sets 3 on playback.
pub const SET_GAIN: u32 = 0x4004_5030;
/// `_IOR('P', 0x30, u32)`: current gain.
pub const GET_GAIN: u32 = 0x8004_5030;
/// `_IO('P', 0xe1)`: start the stream after `SET_PARS`. Traced.
pub const START: u32 = 0x0000_50e1;
/// `_IO('P', 0xe3)`: no argument, never traced. Possibly reset / pause /
/// stop; unknown.
pub const IOC_50E3_UNKNOWN: u32 = 0x0000_50e3;
/// `_IOR('P', 0xea, u32)`: the vendor reads it right after `START` and gets
/// 16000, so taken to be the effective sample rate. Inferred.
pub const GET_RATE: u32 = 0x8004_50ea;
/// `_IOR('P', 0xe6, 8)`: 8 bytes out, never traced; maybe a timestamp or
/// a frame counter. Unknown.
pub const IOC_50E6_UNKNOWN: u32 = 0x8008_50e6;
/// `_IOR('P', 0xe7, 8)`: like [`IOC_50E6_UNKNOWN`]. Unknown.
pub const IOC_50E7_UNKNOWN: u32 = 0x8008_50e7;
/// `_IOW('P', 0x81, u32)`: playback node only. Probably drives the speaker
/// amplifier enable (the DT `speak-gpios` line); 1 on, 0 off. Inferred from
/// the driver, not traced.
pub const SET_SPEAKER: u32 = 0x4004_5081;

/// `ak_pcm.ko` rejects capture periods smaller than this (bytes).
pub const MIN_CAPTURE_PERIOD: u32 = 512;

/// Linux `_IOC` field decoding (ARM uses the generic layout: nr 8 bits,
/// type 8, size 14, direction 2).
pub mod ioc {
    pub const NONE: u32 = 0;
    pub const WRITE: u32 = 1;
    pub const READ: u32 = 2;

    pub const fn dir(cmd: u32) -> u32 {
        cmd >> 30
    }
    pub const fn size(cmd: u32) -> usize {
        ((cmd >> 16) & 0x3fff) as usize
    }
    pub const fn kind(cmd: u32) -> u8 {
        (cmd >> 8) as u8
    }
    pub const fn nr(cmd: u32) -> u8 {
        cmd as u8
    }
    /// `_IOC(dir, type, nr, size)`.
    pub const fn encode(dir: u32, kind: u8, nr: u8, size: usize) -> u32 {
        (dir << 30) | ((size as u32 & 0x3fff) << 16) | ((kind as u32) << 8) | nr as u32
    }
}

/// The `SET_PARS`/`GET_PARS` block: 7 × u32.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PcmParams {
    /// Always 0 in the vendor trace; meaning unknown.
    pub unknown0: u32,
    /// Sample rate, Hz.
    pub rate: u32,
    pub channels: u32,
    /// Bits per sample (16: S16LE).
    pub bits: u32,
    /// Bytes per driver period; ≥ [`MIN_CAPTURE_PERIOD`] on capture.
    pub period_bytes: u32,
    /// Periods in the driver's ring.
    pub periods: u32,
    /// Always 0 in the vendor trace; meaning unknown.
    pub unknown6: u32,
}

const _: () = assert!(std::mem::size_of::<PcmParams>() == 28);
const _: () = assert!(ioc::size(SET_PARS) == std::mem::size_of::<PcmParams>());
const _: () = assert!(ioc::size(GET_PARS) == std::mem::size_of::<PcmParams>());

impl PcmParams {
    /// What the vendor `ipc` set on playback: 16 kHz mono S16, 8 × 2048 B.
    pub const VENDOR: PcmParams = PcmParams::new(16_000, 1, 16, 2048, 8);

    pub const fn new(rate: u32, channels: u32, bits: u32, period_bytes: u32, periods: u32) -> Self {
        PcmParams {
            unknown0: 0,
            rate,
            channels,
            bits,
            period_bytes,
            periods,
            unknown6: 0,
        }
    }

    /// Bytes per second of audio at these parameters.
    pub const fn bytes_per_second(&self) -> u32 {
        self.rate * self.channels * self.bits / 8
    }

    fn to_words(self) -> [u32; 7] {
        [
            self.unknown0,
            self.rate,
            self.channels,
            self.bits,
            self.period_bytes,
            self.periods,
            self.unknown6,
        ]
    }

    fn from_words(w: [u32; 7]) -> Self {
        PcmParams {
            unknown0: w[0],
            rate: w[1],
            channels: w[2],
            bits: w[3],
            period_bytes: w[4],
            periods: w[5],
            unknown6: w[6],
        }
    }
}

/// Which node a [`PcmDevice`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Capture,
    Playback,
    Loopback,
}

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot open {}: {source} (capture admits one opener; is ipc or another service holding it?)", path.display()))]
    Open { path: PathBuf, source: io::Error },
    #[snafu(display("pcm {}: {what} failed: {source}", path.display()))]
    Ioctl {
        path: PathBuf,
        what: &'static str,
        source: io::Error,
    },
    #[snafu(display("pcm {}: {what}: {source}", path.display()))]
    Io {
        path: PathBuf,
        what: &'static str,
        source: io::Error,
    },
    #[snafu(display("pcm: ioctl {cmd:#010x} needs {need} bytes, buffer has {have}"))]
    ArgSize { cmd: u32, need: usize, have: usize },
    #[snafu(display("pcm {}: {what} is not supported on a {direction:?} node", path.display()))]
    WrongNode {
        path: PathBuf,
        what: &'static str,
        direction: Direction,
    },
}

type Result<T, E = Error> = std::result::Result<T, E>;

/// One open `ak_pcm` node. Closing it (drop) stops the stream.
pub struct PcmDevice {
    file: File,
    path: PathBuf,
    direction: Direction,
}

impl PcmDevice {
    /// Opens the capture node `O_RDONLY`, as the vendor does.
    pub fn open_capture(path: &Path) -> Result<Self> {
        Self::open(path, Direction::Capture)
    }

    /// Opens the playback node `O_WRONLY`, as the vendor does.
    pub fn open_playback(path: &Path) -> Result<Self> {
        Self::open(path, Direction::Playback)
    }

    /// Opens the loopback node `O_RDWR`, as the vendor does.
    pub fn open_loopback(path: &Path) -> Result<Self> {
        Self::open(path, Direction::Loopback)
    }

    fn open(path: &Path, direction: Direction) -> Result<Self> {
        let mut o = OpenOptions::new();
        match direction {
            Direction::Capture => o.read(true),
            Direction::Playback => o.write(true),
            Direction::Loopback => o.read(true).write(true),
        };
        let file = o
            .custom_flags(libc::O_CLOEXEC)
            .open(path)
            .context(OpenSnafu { path })?;
        Ok(PcmDevice {
            file,
            path: path.to_owned(),
            direction,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// Runs `cmd` on a word buffer, checking it is at least the size the
    /// command encodes (the kernel copies that many bytes).
    pub fn ioctl_words(&self, cmd: u32, words: &mut [u32], what: &'static str) -> Result<()> {
        let need = ioc::size(cmd);
        ensure!(
            words.len() * 4 >= need,
            ArgSizeSnafu {
                cmd,
                need,
                have: words.len() * 4
            }
        );
        // SAFETY: `words` is at least as large as the size encoded in `cmd`
        // (checked above), 4-byte aligned, and lives for the call.
        unsafe { v4l2::ioctl(self.file.as_fd(), cmd, words.as_mut_ptr().cast()) }
            .map(drop)
            .context(IoctlSnafu {
                path: &self.path,
                what,
            })
    }

    /// Runs a command that takes no argument (`START`, `IOC_50E3_UNKNOWN`).
    /// Commands that encode a size go through [`Self::ioctl_words`] instead.
    pub fn ioctl_none(&self, cmd: u32) -> Result<()> {
        ensure!(
            ioc::size(cmd) == 0,
            ArgSizeSnafu {
                cmd,
                need: ioc::size(cmd),
                have: 0usize
            }
        );
        // SAFETY: the command encodes no argument size, so the kernel does
        // not dereference the (null) argument.
        unsafe { v4l2::ioctl(self.file.as_fd(), cmd, std::ptr::null_mut()) }
            .map(drop)
            .context(IoctlSnafu {
                path: &self.path,
                what: "ioctl",
            })
    }

    fn get_u32(&self, cmd: u32, what: &'static str) -> Result<u32> {
        let mut v = [0u32];
        self.ioctl_words(cmd, &mut v, what)?;
        Ok(v[0])
    }

    fn set_u32(&self, cmd: u32, value: u32, what: &'static str) -> Result<()> {
        let mut v = [value];
        self.ioctl_words(cmd, &mut v, what)
    }

    /// `SET_PARS`. Must precede [`Self::start`].
    pub fn set_params(&self, p: &PcmParams) -> Result<()> {
        let mut w = p.to_words();
        self.ioctl_words(SET_PARS, &mut w, "SET_PARS")
    }

    /// `GET_PARS` (layout inferred, see [`GET_PARS`]).
    pub fn params(&self) -> Result<PcmParams> {
        let mut w = [0u32; 7];
        self.ioctl_words(GET_PARS, &mut w, "GET_PARS")?;
        Ok(PcmParams::from_words(w))
    }

    /// `START`.
    pub fn start(&self) -> Result<()> {
        // SAFETY: as in `ioctl_none`; START encodes no argument.
        unsafe { v4l2::ioctl(self.file.as_fd(), START, std::ptr::null_mut()) }
            .map(drop)
            .context(IoctlSnafu {
                path: &self.path,
                what: "START",
            })
    }

    pub fn set_source(&self, source: u32) -> Result<()> {
        self.set_u32(SET_SOURCE, source, "SET_SOURCE")
    }

    pub fn source(&self) -> Result<u32> {
        self.get_u32(GET_SOURCE, "GET_SOURCE")
    }

    pub fn set_gain(&self, gain: u32) -> Result<()> {
        self.set_u32(SET_GAIN, gain, "SET_GAIN")
    }

    pub fn gain(&self) -> Result<u32> {
        self.get_u32(GET_GAIN, "GET_GAIN")
    }

    /// Effective sample rate (`0x800450ea`, inferred).
    pub fn rate(&self) -> Result<u32> {
        self.get_u32(GET_RATE, "GET_RATE")
    }

    /// Speaker amplifier on/off (`0x40045081`, inferred). Playback only.
    pub fn set_speaker(&self, on: bool) -> Result<()> {
        ensure!(
            self.direction == Direction::Playback,
            WrongNodeSnafu {
                path: &self.path,
                what: "SET_SPEAKER",
                direction: self.direction
            }
        );
        self.set_u32(SET_SPEAKER, u32::from(on), "SET_SPEAKER")
    }

    /// Blocking read of captured (or looped-back) samples; returns the byte
    /// count, which the driver may cut at a period boundary.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            match (&self.file).read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => {
                    return r.context(IoSnafu {
                        path: &self.path,
                        what: "read",
                    })
                }
            }
        }
    }

    /// Blocking write of samples to play; returns the bytes accepted.
    pub fn write(&self, buf: &[u8]) -> Result<usize> {
        loop {
            match (&self.file).write(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => {
                    return r.context(IoSnafu {
                        path: &self.path,
                        what: "write",
                    })
                }
            }
        }
    }
}

impl AsFd for PcmDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn params_layout() {
        assert_eq!(size_of::<PcmParams>(), 28);
        assert_eq!(offset_of!(PcmParams, unknown0), 0);
        assert_eq!(offset_of!(PcmParams, rate), 4);
        assert_eq!(offset_of!(PcmParams, channels), 8);
        assert_eq!(offset_of!(PcmParams, bits), 12);
        assert_eq!(offset_of!(PcmParams, period_bytes), 16);
        assert_eq!(offset_of!(PcmParams, periods), 20);
        assert_eq!(offset_of!(PcmParams, unknown6), 24);
    }

    #[test]
    fn vendor_params_match_trace_bytes() {
        // ipc.trace:5209-5210, the SET_PARS argument as the kernel saw it.
        let traced: [u8; 28] = [
            0x00, 0x00, 0x00, 0x00, 0x80, 0x3e, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x10, 0x00,
            0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let bytes: Vec<u8> = PcmParams::VENDOR
            .to_words()
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        assert_eq!(bytes, traced);
        assert_eq!(
            PcmParams::from_words(PcmParams::VENDOR.to_words()),
            PcmParams::VENDOR
        );
        assert_eq!(PcmParams::VENDOR.bytes_per_second(), 32_000);
    }

    #[test]
    fn ioctl_numbers_decode() {
        use ioc::{NONE, READ, WRITE};
        // (cmd, dir, size, nr)
        let table: &[(u32, u32, usize, u8)] = &[
            (SET_PARS, WRITE, 28, 0xe0),
            (GET_PARS, READ, 28, 0xf2),
            (SET_SOURCE, WRITE, 4, 0x10),
            (GET_SOURCE, READ, 4, 0x10),
            (SET_GAIN, WRITE, 4, 0x30),
            (GET_GAIN, READ, 4, 0x30),
            (START, NONE, 0, 0xe1),
            (IOC_50E3_UNKNOWN, NONE, 0, 0xe3),
            (GET_RATE, READ, 4, 0xea),
            (IOC_50E6_UNKNOWN, READ, 8, 0xe6),
            (IOC_50E7_UNKNOWN, READ, 8, 0xe7),
            (SET_SPEAKER, WRITE, 4, 0x81),
        ];
        for &(cmd, dir, size, nr) in table {
            assert_eq!(ioc::kind(cmd), b'P', "{cmd:#x}");
            assert_eq!(ioc::dir(cmd), dir, "{cmd:#x}");
            assert_eq!(ioc::size(cmd), size, "{cmd:#x}");
            assert_eq!(ioc::nr(cmd), nr, "{cmd:#x}");
            assert_eq!(ioc::encode(dir, b'P', nr, size), cmd);
        }
    }

    #[test]
    fn opening_a_missing_node_names_it() {
        let err = PcmDevice::open_capture(Path::new("/nonexistent/pcmC0D0c"))
            .err()
            .expect("no such node");
        assert!(err.to_string().contains("/nonexistent/pcmC0D0c"));
    }

    #[test]
    fn size_checks_need_no_hardware() {
        // /dev/null accepts the open; the size checks fail before any ioctl.
        let dev = PcmDevice::open_playback(Path::new("/dev/null")).unwrap();
        let mut one = [0u32; 1];
        assert!(matches!(
            dev.ioctl_words(SET_PARS, &mut one, "SET_PARS"),
            Err(Error::ArgSize {
                need: 28,
                have: 4,
                ..
            })
        ));
        assert!(matches!(
            dev.ioctl_none(GET_RATE),
            Err(Error::ArgSize { .. })
        ));
        let cap = PcmDevice::open_capture(Path::new("/dev/null")).unwrap();
        assert!(matches!(
            cap.set_speaker(true),
            Err(Error::WrongNode { .. })
        ));
    }
}
