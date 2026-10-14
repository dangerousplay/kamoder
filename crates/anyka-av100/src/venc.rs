//! The AV100 hardware video encoder (`ak_venc_adapter.ko`), driven directly.
//!
//! The kernel module holds the whole encoder — rate control, SPS/PPS,
//! scheduling — so userspace only hands it parameter blocks: find a free
//! channel, size and create its DMA pools, map the bitstream pool, create the
//! channel, then encode frames synchronously by physical address. Input is
//! packed NV12 straight from a capture buffer (no copy); output is an Annex-B
//! H.264 access unit read from the mapped pool and released after use.
//!
//! Recovered from `libmpi_venc` V2.0.46 and the kernel module, and checked
//! against traces of the vendor application (see `tools/re/venc-protocol`).
//! `/dev/venc` admits one opener system-wide, so one [`Encoder`] per process
//! hosts every channel.

use crate::device::{FrameRef, Map};
use crate::v4l2;
use snafu::{ResultExt, Snafu};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const VENC_VERSION: u32 = 0x8004_56cf;
const VENC_FIND_CHANNEL: u32 = 0x405c_56ce;
const VENC_DMA_NEEDS: u32 = 0x8054_56ec;
const CHN_POOL_CREATE: u32 = 0x4020_56ed;
const CHN_POOL_FREE: u32 = 0x4020_56ee;
const CHN_CREATE: u32 = 0x405c_56e0;
const CHN_DESTROY: u32 = 0x4004_56eb;
const CHN_ENCODE: u32 = 0x4638_56e6;
const CHN_RELEASE: u32 = 0x4020_56ff;
const CHN_REQUEST_IDR: u32 = 0x4004_56f3;

/// `eProfile` values for H.264.
pub const PROFILE_BASELINE: u32 = 0x42;
pub const PROFILE_MAIN: u32 = 0x4d;
pub const PROFILE_HIGH: u32 = 0x64;

const CHROMA_420: u32 = 1;
const RC_CBR: u32 = 1;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot open {}: {source} (another process may hold the encoder)", path.display()))]
    Open { path: PathBuf, source: io::Error },
    #[snafu(display("encoder: {what} failed: {source}"))]
    Ioctl {
        what: &'static str,
        source: io::Error,
    },
    #[snafu(display("encoder: no free channel"))]
    NoChannel,
    #[snafu(display(
        "encoder: {width}x{height} is not supported (width a multiple of 32, height of 8)"
    ))]
    Size { width: u32, height: u32 },
    #[snafu(display("encoder: frame is {got} bytes, expected {want}"))]
    FrameSize { got: usize, want: usize },
}

type Result<T, E = Error> = std::result::Result<T, E>;

fn open(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .context(OpenSnafu { path })
}

/// `ioctl` on a word buffer of any size.
fn call(fd: &File, cmd: u32, words: &mut [u32], what: &'static str) -> Result<()> {
    // SAFETY: each command's argument is a struct of `words.len()` 32-bit
    // words (the size is encoded in `cmd`); the buffer lives for the call.
    unsafe { v4l2::ioctl(fd.as_fd(), cmd, words.as_mut_ptr().cast()) }
        .map(drop)
        .context(IoctlSnafu { what })
}

fn put_u16(words: &mut [u32], byte: usize, v: u16) {
    let w = &mut words[byte / 4];
    let shift = (byte % 4) * 8;
    *w = (*w & !(0xffff << shift)) | (u32::from(v) << shift);
}

fn put_u8(words: &mut [u32], byte: usize, v: u8) {
    let w = &mut words[byte / 4];
    let shift = (byte % 4) * 8;
    *w = (*w & !(0xff << shift)) | (u32::from(v) << shift);
}

/// H.264 settings of one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Frames between IDR pictures.
    pub gop: u32,
    /// Constant bitrate target.
    pub kbps: u32,
    pub profile: u32,
    /// `level_idc` × 10 / 10, e.g. 40 for level 4.0.
    pub level: u32,
}

impl Settings {
    /// `core[0x5c]`: the channel parameter block.
    fn core(&self, index: u32) -> [u32; 0x5c / 4] {
        let mut c = [0u32; 0x5c / 4];
        c[0] = index;
        c[1] = self.profile;
        c[2] = CHROMA_420;
        put_u16(&mut c, 0x0c, self.width as u16);
        put_u16(&mut c, 0x0e, self.height as u16);
        c[0x18 / 4] = RC_CBR;
        put_u16(&mut c, 0x1c, self.fps as u16);
        put_u16(&mut c, 0x1e, self.gop as u16);
        c[0x24 / 4] = self.level;
        put_u16(&mut c, 0x28, self.kbps as u16);
        c
    }

    /// Bitstream pool size, as the vendor library sizes it.
    fn stream_pool(&self) -> u32 {
        let area = self.width * self.height;
        let base = if area > 230_400 {
            area * 3 / 8
        } else {
            0x20000
        };
        base.max(self.kbps * 256).div_ceil(32) * 32
    }
}

/// The encoder device; channels are created from it.
pub struct Encoder {
    dev: File,
    pub version: String,
}

impl Encoder {
    pub fn open(dev: &Path) -> Result<Encoder> {
        let file = open(&dev.join("venc"))?;
        let mut v = [0u32; 32];
        call(&file, VENC_VERSION, &mut v, "version")?;
        let bytes: Vec<u8> = v.iter().flat_map(|w| w.to_le_bytes()).collect();
        let version = String::from_utf8_lossy(bytes.split(|&b| b == 0).next().unwrap_or(&[]))
            .trim()
            .to_owned();
        Ok(Encoder { dev: file, version })
    }

    /// Creates an H.264 channel.
    pub fn channel(&self, dev: &Path, s: Settings) -> Result<Channel> {
        if !s.width.is_multiple_of(32)
            || !s.height.is_multiple_of(8)
            || s.width > 3072
            || s.height > 3840
        {
            return SizeSnafu {
                width: s.width,
                height: s.height,
            }
            .fail();
        }
        // A free channel, by name.
        let mut index = None;
        for n in 0..8u32 {
            let mut q = [0u32; 0x6c / 4];
            let name = format!("venc-chn{n}");
            for (i, b) in name.bytes().enumerate() {
                put_u8(&mut q, i, b);
            }
            q[0x0c / 4..0x0c / 4 + 0x5c / 4].copy_from_slice(&s.core(n));
            call(&self.dev, VENC_FIND_CHANNEL, &mut q, "find channel")?;
            if q[0x68 / 4] == 1 {
                index = Some(n);
                break;
            }
        }
        let index = index.ok_or(Error::NoChannel)?;
        let chn = open(&dev.join(format!("venc-chn{index}")))?;

        // Work memory the encoder needs for this size.
        let mut need = [0u32; 0x54 / 4];
        need[0] = s.profile;
        need[1] = CHROMA_420;
        put_u16(&mut need, 0x08, s.width as u16);
        put_u16(&mut need, 0x0a, s.height as u16);
        put_u16(&mut need, 0x0c, 1);
        put_u8(&mut need, 0x1c, 1);
        call(&self.dev, VENC_DMA_NEEDS, &mut need, "DMA size query")?;
        let n = (need[0] as usize).min(20);
        let work: u32 = need[1..=n].iter().map(|v| v.div_ceil(32) * 32).sum();

        let mut pool = [0u32; 8];
        pool[0] = 1;
        pool[7] = work;
        call(&chn, CHN_POOL_CREATE, &mut pool, "work pool")?;
        let size = s.stream_pool();
        let mut pool = [0u32; 8];
        pool[0] = 1;
        pool[1] = 1;
        pool[7] = size;
        call(&chn, CHN_POOL_CREATE, &mut pool, "stream pool")?;
        let kbase = pool[4];
        let base = Map::new(chn.as_fd(), size as usize, 0).context(IoctlSnafu {
            what: "stream pool mmap",
        })?;

        let mut core = s.core(index);
        call(&chn, CHN_CREATE, &mut core, "create channel")?;
        tracing::info!(
            index,
            width = s.width,
            height = s.height,
            fps = s.fps,
            kbps = s.kbps,
            "H.264 channel open"
        );
        Ok(Channel {
            chn,
            index,
            base,
            kbase,
            settings: s,
            created: true,
        })
    }
}

/// One encoded access unit, borrowed from the stream pool.
pub struct Packet<'a> {
    /// Annex-B NAL units.
    pub data: &'a [u8],
    pub keyframe: bool,
    pub timestamp: u64,
}

/// An H.264 encoder channel.
pub struct Channel {
    chn: File,
    index: u32,
    base: Map,
    kbase: u32,
    pub settings: Settings,
    created: bool,
}

impl Channel {
    /// Encodes one captured picture and lends the result to `use_packet`.
    pub fn encode<R>(
        &mut self,
        frame: &FrameRef<'_>,
        use_packet: impl FnOnce(Packet<'_>) -> R,
    ) -> Result<R> {
        let want = (self.settings.width * self.settings.height * 3 / 2) as usize;
        if frame.data.len() != want {
            return FrameSizeSnafu {
                got: frame.data.len(),
                want,
            }
            .fail();
        }
        let mut e = vec![0u32; 0x638 / 4];
        e[0] = self.index;
        e[1] = frame.data.as_ptr() as usize as u32;
        e[2] = frame.phys;
        e[3] = want as u32;
        put_u16(&mut e, 0x18, 0x20);
        put_u16(
            &mut e,
            0x1a,
            if frame.height.is_multiple_of(24) {
                24
            } else {
                16
            },
        );
        call(&self.chn, CHN_ENCODE, &mut e, "encode")?;
        let (kaddr, len) = (e[0x620 / 4], e[0x624 / 4] as usize);
        let ts = u64::from(e[0x628 / 4]) | u64::from(e[0x62c / 4]) << 32;
        let (seq, kind) = (e[0x630 / 4], e[0x634 / 4]);
        let offset = kaddr.wrapping_sub(self.kbase) as usize;
        let result = use_packet(Packet {
            data: self.base.slice(offset, len),
            keyframe: kind == 2 || kind == 3,
            timestamp: ts,
        });
        let mut r = [
            self.index,
            len as u32,
            e[0x628 / 4],
            e[0x62c / 4],
            seq,
            0,
            kaddr,
            0,
        ];
        call(&self.chn, CHN_RELEASE, &mut r, "release stream")?;
        Ok(result)
    }

    /// Makes the next picture an IDR.
    pub fn request_idr(&self) -> Result<()> {
        let mut i = [self.index];
        call(&self.chn, CHN_REQUEST_IDR, &mut i, "request IDR")
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        if self.created {
            let mut i = [self.index];
            let _ = call(&self.chn, CHN_DESTROY, &mut i, "destroy");
            let mut work = [1u32, 0, 0, 0, 0, 0, 0, 0];
            let _ = call(&self.chn, CHN_POOL_FREE, &mut work, "free work pool");
        }
        // The stream pool stays mapped until `base` drops after this body;
        // closing the channel fd then releases it with the channel.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_block_layout() {
        let s = Settings {
            width: 1280,
            height: 720,
            fps: 15,
            gop: 30,
            kbps: 2048,
            profile: PROFILE_MAIN,
            level: 40,
        };
        let c = s.core(1);
        assert_eq!(c[0], 1);
        assert_eq!(c[1], 0x4d);
        assert_eq!(c[3], 1280 | 720 << 16);
        assert_eq!(c[6], RC_CBR);
        assert_eq!(c[7], 15 | 30 << 16);
        assert_eq!(c[9], 40);
        assert_eq!(c[10], 2048);
        // max(w*h*3/8 = 345600, kbps*256 = 524288)
        assert_eq!(s.stream_pool(), 524_288);
    }
}
