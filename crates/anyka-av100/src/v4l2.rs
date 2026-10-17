//! The slice of V4L2 the Anyka camera host driver (`ak_isp.ko`) needs, plus
//! its private commands.
//!
//! Structures are kept as arrays of 32-bit words with named offsets rather
//! than `#[repr(C)]` mirrors of `<linux/videodev2.h>`: on this ARM EABI target
//! every field is 32-bit (pointers and `long` included), so word offsets are
//! exact, and the ioctl numbers below already encode the sizes the kernel
//! checks. The numbers are the ones the vendor stack was traced sending.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

pub const VIDIOC_REQBUFS: u32 = 0xc014_5608;
pub const VIDIOC_QUERYBUF: u32 = 0xc044_5609;
pub const VIDIOC_QBUF: u32 = 0xc044_560f;
pub const VIDIOC_DQBUF: u32 = 0xc044_5611;
pub const VIDIOC_STREAMON: u32 = 0x4004_5612;
pub const VIDIOC_STREAMOFF: u32 = 0x4004_5613;
pub const VIDIOC_S_FMT: u32 = 0xc0cc_5605;
pub const VIDIOC_S_PARM: u32 = 0xc0cc_5616;
pub const VIDIOC_G_CTRL: u32 = 0xc008_561b;
pub const VIDIOC_CROPCAP: u32 = 0xc02c_563a;
pub const VIDIOC_S_CROP: u32 = 0x4014_563c;

pub const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
pub const MEMORY_MMAP: u32 = 1;
/// `V4L2_FIELD_INTERLACED`, which the Anyka driver expects in `S_FMT`.
pub const FIELD_ANY_AK: u32 = 4;
pub const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

/// Anyka private commands (`ak_video_priv_cmd.h`), sent through `S_PARM`.
pub mod private {
    pub const SET_GLOBAL_CROP: u32 = 0x101;
    pub const SET_DONE_MODE: u32 = 0x103;
    pub const SET_FRAME_RATE: u32 = 0x106;
    pub const SET_DUAL_MODE: u32 = 0x107;
    pub const SET_CHN_MAX_DMA_SIZE: u32 = 0x109;
    pub const SET_CHN_BUFS_RELEASE: u32 = 0x10a;
}

/// Sensor-id control on the subdev (`SENSOR_GET_ID`).
pub const CTRL_SENSOR_ID: u32 = 0x0091_0001;
/// `AK_SENSOR_SET_INIT`: `{u16 count, u16 0, u32 ptr → {u16 reg, u16 val}[]}`.
pub const AK_SENSOR_SET_INIT: u32 = 0x4008_5301;

/// `ioctl` with the vendor library's EINTR retry.
///
/// # Safety
/// `arg` must point to memory laid out as `cmd` expects, valid for the
/// whole call (the kernel reads and writes the size encoded in `cmd`, or for
/// the Anyka ISP commands the size of the struct they name).
pub unsafe fn ioctl(fd: BorrowedFd<'_>, cmd: u32, arg: *mut libc::c_void) -> io::Result<i32> {
    loop {
        let r = libc::ioctl(fd.as_raw_fd(), cmd as _, arg);
        if r >= 0 {
            return Ok(r);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Runs `cmd` on a word buffer.
pub fn ioctl_words<const N: usize>(
    fd: BorrowedFd<'_>,
    cmd: u32,
    words: &mut [u32; N],
) -> io::Result<()> {
    // SAFETY: every command used with this helper takes a struct of exactly
    // N 32-bit words (the size is in `cmd`), and `words` lives for the call.
    unsafe { ioctl(fd, cmd, words.as_mut_ptr().cast()) }.map(drop)
}

/// `struct v4l2_streamparm` (204 bytes) carrying a private command:
/// `{type = 1, raw_data = {cmd, payload…}}`.
pub fn private(fd: BorrowedFd<'_>, cmd: u32, payload: &[u32]) -> io::Result<()> {
    let mut p = [0u32; 51];
    p[0] = BUF_TYPE_VIDEO_CAPTURE;
    p[1] = cmd;
    p[2..2 + payload.len()].copy_from_slice(payload);
    ioctl_words(fd, VIDIOC_S_PARM, &mut p)
}

/// A private command whose argument is a pointer to `buf`, for the ISP's
/// "get" commands: `S_PARM {type = 1, cmd, &buf}`, and the kernel copies its
/// struct into `buf`. [`private`] can only pass words by value.
///
/// `buf` must be at least as large as the struct `cmd` names: the driver
/// copies its own size, not `buf.len()`. Words keep it 4-byte aligned, as
/// the driver's `u32` fields need.
pub fn private_query(fd: BorrowedFd<'_>, cmd: u32, buf: &mut [u32]) -> io::Result<()> {
    // The ABI field is 32 bits; on the 32-bit target a pointer fits exactly.
    let ptr = buf.as_mut_ptr() as usize as u32;
    private(fd, cmd, &[ptr])
}

/// `struct v4l2_buffer` word offsets.
pub mod buf {
    pub const WORDS: usize = 17;
    pub const INDEX: usize = 0;
    pub const TYPE: usize = 1;
    pub const BYTESUSED: usize = 2;
    pub const TS_SEC: usize = 5;
    pub const TS_USEC: usize = 6;
    pub const SEQUENCE: usize = 11;
    pub const MEMORY: usize = 12;
    pub const OFFSET: usize = 13;
    pub const LENGTH: usize = 14;
}

/// A `v4l2_buffer` for `index` (or for DQBUF, any).
pub fn buffer(index: u32) -> [u32; buf::WORDS] {
    let mut b = [0u32; buf::WORDS];
    b[buf::INDEX] = index;
    b[buf::TYPE] = BUF_TYPE_VIDEO_CAPTURE;
    b[buf::MEMORY] = MEMORY_MMAP;
    b
}

/// `VIDIOC_CROPCAP` → sensor bounds `(width, height)`.
pub fn cropcap(fd: BorrowedFd<'_>) -> io::Result<(u32, u32)> {
    let mut c = [0u32; 11];
    c[0] = BUF_TYPE_VIDEO_CAPTURE;
    ioctl_words(fd, VIDIOC_CROPCAP, &mut c)?;
    Ok((c[3], c[4]))
}
