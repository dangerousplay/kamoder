//! One image sensor and its ISP: bring-up, capture channels, frames.
//!
//! Follows the vendor library's device sequence (see `conf` for where it
//! comes from): open `/dev/isp-param-N`, `/dev/v4l-subdevN` and the three
//! `/dev/video-N-M` nodes; load the ISP tuning and the sensor register table;
//! map the 3D-NR reference; configure the channels; then the usual V4L2 MMAP
//! streaming. Everything after load (auto exposure, white balance) runs in the
//! kernel driver, so a started sensor needs no further ISP calls.

use crate::conf::{self, IspCommand, Subfile};
use crate::v4l2::{self, buf, private};
use snafu::{ResultExt, Snafu};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot open {}: {source}", path.display()))]
    Open { path: PathBuf, source: io::Error },
    #[snafu(display("cannot read the ISP config {}: {source}", path.display()))]
    ReadConf { path: PathBuf, source: io::Error },
    #[snafu(display("bad ISP config {}: {source}", path.display()))]
    Conf { path: PathBuf, source: conf::Error },
    #[snafu(display("sensor {index}: {what} failed: {source}"))]
    Ioctl {
        index: u32,
        what: String,
        source: io::Error,
    },
    #[snafu(display("sensor {index}: no frame within {timeout:?}"))]
    Timeout { index: u32, timeout: Duration },
}

type Result<T, E = Error> = std::result::Result<T, E>;

/// Output channel of an ISP: `video-N-0` main, `-1` sub, `-2` third.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelSpec {
    pub node: usize,
    pub width: u32,
    pub height: u32,
    /// Capture buffers (2..=4; the vendor uses 3 on main, 2 on sub).
    pub buffers: u32,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// `N` of the device nodes.
    pub index: u32,
    /// ISP tuning file for this sensor.
    pub conf: PathBuf,
    /// Parameter set: 0 day, 1 night.
    pub slot: u8,
    /// Both sensors of a dual board run at once (`SET_DUAL_MODE`).
    pub dual: bool,
    pub channels: Vec<ChannelSpec>,
    /// Directory holding the device nodes (`/dev`).
    pub dev: PathBuf,
}

/// A shared memory mapping, unmapped on drop.
pub(crate) struct Map {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is plain memory owned by this value; access is
// serialised by `&`/`&mut` on the owning `Sensor`.
unsafe impl Send for Map {}

impl Map {
    pub(crate) fn new(fd: BorrowedFd<'_>, len: usize, offset: u32) -> io::Result<Map> {
        // SAFETY: a fresh shared mapping of a device fd; failure is checked.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                offset as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Map {
            ptr: ptr.cast(),
            len,
        })
    }

    /// `len` bytes at `offset`, clamped to the mapping.
    pub(crate) fn slice(&self, offset: usize, len: usize) -> &[u8] {
        let offset = offset.min(self.len);
        let len = len.min(self.len - offset);
        // SAFETY: the range was clamped to the mapping, which lives as long as self.
        unsafe { std::slice::from_raw_parts(self.ptr.add(offset), len) }
    }

    pub(crate) fn bytes(&self, len: usize) -> &[u8] {
        // SAFETY: `len` is clamped to the mapping, which lives as long as self.
        unsafe { std::slice::from_raw_parts(self.ptr, len.min(self.len)) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what `new` mapped.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

struct Channel {
    spec: ChannelSpec,
    maps: Vec<Map>,
    streaming: bool,
}

/// A picture still in the driver's capture buffer.
pub struct FrameRef<'a> {
    pub width: u32,
    pub height: u32,
    /// Packed NV12: Y (`width*height`) then interleaved CbCr.
    pub data: &'a [u8],
    /// Physical address of `data`, for DMA engines such as the encoder.
    pub phys: u32,
    pub timestamp_ms: u64,
}

/// One NV12 picture copied out of a capture buffer (the service's type).
pub use camera::backend::Frame;

/// A brought-up sensor. Dropping it stops streaming and releases everything.
pub struct Sensor {
    index: u32,
    isp: File,
    _subdev: File,
    nodes: Vec<File>,
    _nr3d: Option<Map>,
    channels: Vec<Channel>,
    /// Sensor active area.
    pub width: u32,
    pub height: u32,
    pub sensor_id: u32,
}

fn open(path: PathBuf, nonblock: bool) -> Result<File> {
    let mut o = OpenOptions::new();
    o.read(true).write(true).custom_flags(libc::O_CLOEXEC);
    if nonblock {
        o.custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    o.open(&path).context(OpenSnafu { path })
}

impl Sensor {
    fn fail<'a>(&self, what: &'a str) -> impl FnOnce(io::Error) -> Error + 'a {
        let index = self.index;
        move |source| Error::Ioctl {
            index,
            what: what.to_owned(),
            source,
        }
    }

    /// Sends one ISP command: `S_PARM {cmd, pointer to an aligned copy}`.
    fn isp_command(&self, c: &IspCommand) -> Result<()> {
        let mut aligned = vec![0u32; c.data.len().div_ceil(4).max(1)];
        // SAFETY: `aligned` holds at least `c.data.len()` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                c.data.as_ptr(),
                aligned.as_mut_ptr().cast(),
                c.data.len(),
            )
        };
        let ptr = aligned.as_mut_ptr() as usize as u32;
        v4l2::private(self.isp.as_fd(), c.cmd, &[ptr])
            .map_err(self.fail(&format!("ISP command {:#010x}", c.cmd)))
    }

    /// Sensor register table through the subdev (`AK_SENSOR_SET_INIT`).
    fn sensor_init(&self, regs: &[(u16, u16)]) -> Result<()> {
        let mut table: Vec<u32> = regs
            .iter()
            .map(|&(r, v)| u32::from(r) | u32::from(v) << 16)
            .collect();
        let mut arg = [regs.len() as u32, table.as_mut_ptr() as usize as u32];
        // SAFETY: `{u16 count, u16 0, u32 ptr}` with `table` alive for the call.
        unsafe {
            v4l2::ioctl(
                self._subdev.as_fd(),
                v4l2::AK_SENSOR_SET_INIT,
                arg.as_mut_ptr().cast(),
            )
        }
        .map(drop)
        .map_err(self.fail("sensor register table"))
    }

    fn node(&self, n: usize) -> BorrowedFd<'_> {
        self.nodes[n].as_fd()
    }

    /// Opens and configures the sensor; channels are set up but not started.
    pub fn open(opts: &Options) -> Result<Sensor> {
        let data = std::fs::read(&opts.conf).context(ReadConfSnafu { path: &opts.conf })?;
        let subfiles = conf::parse(&data).context(ConfSnafu { path: &opts.conf })?;
        let sub: &Subfile =
            conf::select(&subfiles, opts.slot).context(ConfSnafu { path: &opts.conf })?;

        let n = opts.index;
        let dev = |name: String| opts.dev.join(name);
        let isp = open(dev(format!("isp-param-{n}")), false)?;
        let subdev = open(dev(format!("v4l-subdev{n}")), false)?;
        let nodes = (0..3)
            .map(|m| open(dev(format!("video-{n}-{m}")), true))
            .collect::<Result<Vec<_>>>()?;
        let mut s = Sensor {
            index: n,
            isp,
            _subdev: subdev,
            nodes,
            _nr3d: None,
            channels: Vec::new(),
            width: 0,
            height: 0,
            sensor_id: 0,
        };

        let mut ctrl = [v4l2::CTRL_SENSOR_ID, 0];
        v4l2::ioctl_words(s._subdev.as_fd(), v4l2::VIDIOC_G_CTRL, &mut ctrl)
            .map_err(s.fail("sensor id"))?;
        s.sensor_id = ctrl[1];
        if s.sensor_id != sub.sensor_id {
            tracing::warn!(
                index = n,
                sensor = format_args!("{:#x}", s.sensor_id),
                conf = format_args!("{:#x}", sub.sensor_id),
                "ISP config is for another sensor"
            );
        }

        for c in sub.commands() {
            s.isp_command(&c)?;
        }
        s.sensor_init(&sub.sensor_regs)?;
        s.isp_command(&sub.exposure())?;

        let (w, h) = v4l2::cropcap(s.node(0)).map_err(s.fail("CROPCAP"))?;
        (s.width, s.height) = (w, h);

        // 3D noise reduction reference frames: the ISP writes them into a
        // buffer this process maps from the param device.
        s.isp_command(&IspCommand {
            cmd: conf::SET_3D_NR_REF,
            data: [w.to_le_bytes(), h.to_le_bytes()].concat(),
        })?;
        let nr_len = ((w * h * 3 / 2) * 5 / 4 + 0x40) as usize;
        s._nr3d = Some(Map::new(s.isp.as_fd(), nr_len, 0).map_err(s.fail("3D-NR mmap"))?);

        if opts.dual {
            v4l2::private(s.node(0), private::SET_DUAL_MODE, &[1]).map_err(s.fail("dual mode"))?;
        }
        v4l2::private(s.node(0), private::SET_FRAME_RATE, &[0]).map_err(s.fail("frame rate"))?;
        v4l2::private(s.node(0), private::SET_GLOBAL_CROP, &[0, 0, 0, w, h])
            .map_err(s.fail("global crop"))?;

        for spec in &opts.channels {
            s.configure(*spec)?;
        }
        for i in 0..s.channels.len() {
            s.allocate(i, s.channels[i].spec.buffers)?;
        }
        tracing::info!(
            index = n,
            sensor = format_args!("{:#x}", s.sensor_id),
            width = w,
            height = h,
            channels = s.channels.len(),
            "sensor configured"
        );
        Ok(s)
    }

    fn configure(&mut self, spec: ChannelSpec) -> Result<()> {
        let fd = self.node(spec.node);
        v4l2::private(fd, private::SET_DONE_MODE, &[0]).map_err(self.fail("done mode"))?;
        v4l2::cropcap(fd).map_err(self.fail("CROPCAP"))?;
        let mut crop = [v4l2::BUF_TYPE_VIDEO_CAPTURE, 0, 0, self.width, self.height];
        v4l2::ioctl_words(fd, v4l2::VIDIOC_S_CROP, &mut crop).map_err(self.fail("S_CROP"))?;
        let mut fmt = [0u32; 51];
        fmt[0] = v4l2::BUF_TYPE_VIDEO_CAPTURE;
        fmt[1] = spec.width;
        fmt[2] = spec.height;
        fmt[3] = v4l2::FOURCC_NV12;
        fmt[4] = v4l2::FIELD_ANY_AK;
        v4l2::ioctl_words(fd, v4l2::VIDIOC_S_FMT, &mut fmt).map_err(self.fail("S_FMT"))?;
        v4l2::private(
            fd,
            private::SET_CHN_MAX_DMA_SIZE,
            &[spec.width * spec.height * 3 / 2],
        )
        .map_err(self.fail("max DMA size"))?;
        self.channels.push(Channel {
            spec,
            maps: Vec::new(),
            streaming: false,
        });
        Ok(())
    }

    fn allocate(&mut self, i: usize, count: u32) -> Result<()> {
        let fd = self.node(self.channels[i].spec.node);
        let mut req = [count, v4l2::BUF_TYPE_VIDEO_CAPTURE, v4l2::MEMORY_MMAP, 0, 0];
        v4l2::ioctl_words(fd, v4l2::VIDIOC_REQBUFS, &mut req).map_err(self.fail("REQBUFS"))?;
        let mut maps = Vec::new();
        for b in 0..req[0] {
            let mut q = v4l2::buffer(b);
            v4l2::ioctl_words(fd, v4l2::VIDIOC_QUERYBUF, &mut q).map_err(self.fail("QUERYBUF"))?;
            maps.push(
                Map::new(fd, q[buf::LENGTH] as usize, q[buf::OFFSET])
                    .map_err(self.fail("buffer mmap"))?,
            );
        }
        self.channels[i].maps = maps;
        Ok(())
    }

    /// Queues every buffer and starts streaming on all channels.
    pub fn start(&mut self) -> Result<()> {
        for i in 0..self.channels.len() {
            let fd = self.node(self.channels[i].spec.node);
            for b in 0..self.channels[i].maps.len() as u32 {
                let mut q = v4l2::buffer(b);
                v4l2::ioctl_words(fd, v4l2::VIDIOC_QBUF, &mut q).map_err(self.fail("QBUF"))?;
            }
            let mut t = [v4l2::BUF_TYPE_VIDEO_CAPTURE];
            v4l2::ioctl_words(fd, v4l2::VIDIOC_STREAMON, &mut t).map_err(self.fail("STREAMON"))?;
            self.channels[i].streaming = true;
        }
        Ok(())
    }

    /// Waits for the next picture on channel `i` (in `Options::channels`
    /// order) and copies it out.
    pub fn frame(&mut self, i: usize, timeout: Duration) -> Result<Frame> {
        self.with_frame(i, timeout, |f| Frame {
            width: f.width,
            height: f.height,
            data: f.data.to_vec(),
            timestamp_ms: f.timestamp_ms,
        })
    }

    /// Waits for the next picture on channel `i` and lends it to `use_frame`
    /// in place (no copy), then gives the buffer back to the driver. The
    /// physical address lets the hardware encoder read it directly.
    pub fn with_frame<R>(
        &mut self,
        i: usize,
        timeout: Duration,
        use_frame: impl FnOnce(FrameRef<'_>) -> R,
    ) -> Result<R> {
        let ch = &self.channels[i];
        let fd = self.node(ch.spec.node);
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Called at most once, but from inside the retry loop.
        let mut use_frame = Some(use_frame);
        loop {
            // SAFETY: one valid pollfd.
            let r = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(self.fail("poll")(e));
            }
            if r == 0 {
                return TimeoutSnafu {
                    index: self.index,
                    timeout,
                }
                .fail();
            }
            let mut q = v4l2::buffer(0);
            v4l2::ioctl_words(fd, v4l2::VIDIOC_DQBUF, &mut q).map_err(self.fail("DQBUF"))?;
            let ts = u64::from(q[buf::TS_SEC]) * 1000 + u64::from(q[buf::TS_USEC]) / 1000;
            let (w, h) = (ch.spec.width, ch.spec.height);
            let len = (w * h * 3 / 2) as usize;
            let map = &ch.maps[q[buf::INDEX] as usize];
            // A zero timestamp marks a frame the driver dropped.
            let result = (ts != 0 && map.len >= len).then(|| {
                (use_frame.take().expect("called once"))(FrameRef {
                    width: w,
                    height: h,
                    data: map.bytes(len),
                    // The Anyka driver reports the buffer's physical address
                    // in `sequence`.
                    phys: q[buf::SEQUENCE],
                    timestamp_ms: ts,
                })
            });
            let mut back = v4l2::buffer(q[buf::INDEX]);
            v4l2::ioctl_words(fd, v4l2::VIDIOC_QBUF, &mut back).map_err(self.fail("QBUF"))?;
            if let Some(r) = result {
                return Ok(r);
            }
        }
    }

    pub fn index(&self) -> u32 {
        self.index
    }
}

impl Drop for Sensor {
    fn drop(&mut self) {
        for ch in &mut self.channels {
            let fd = self.nodes[ch.spec.node].as_fd();
            if ch.streaming {
                let mut t = [v4l2::BUF_TYPE_VIDEO_CAPTURE];
                let _ = v4l2::ioctl_words(fd, v4l2::VIDIOC_STREAMOFF, &mut t);
            }
            ch.maps.clear();
            let _ = v4l2::private(fd, private::SET_CHN_BUFS_RELEASE, &[]);
        }
    }
}

/// Image sensors present: indices of `/sys/class/video4linux/v4l-subdevN`.
pub fn sensors(sysfs: &Path) -> Vec<u32> {
    let mut v: Vec<u32> = std::fs::read_dir(sysfs)
        .map(|d| {
            d.filter_map(|e| {
                e.ok()?
                    .file_name()
                    .to_str()?
                    .strip_prefix("v4l-subdev")?
                    .parse()
                    .ok()
            })
            .collect()
        })
        .unwrap_or_default();
    v.sort_unstable();
    v
}
