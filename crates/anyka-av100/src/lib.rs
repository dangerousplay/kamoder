//! The AK3918AV100 video hardware behind the camera core traits, with no
//! vendor library: the ISP tuning loader (`conf`), the camera host driver's
//! V4L2 interface (`v4l2`, `device`) and the hardware H.264 encoder
//! (`venc`), adapted as [`Av100Backend`] (ISP + V4L2 capture) and
//! [`Av100Encoder`] (one `/dev/venc` channel per sensor). `pcm` holds the
//! audio codec's ioctl ABI (`ak_pcm.ko`) for the `audio` service.
//!
//! Hardware rules kept here, not in the generic service:
//! - all sensors are opened, then all started ([`Av100Backend::start`]):
//!   with `SET_DUAL_MODE` neither ISP delivers until both stream;
//! - the encoder and one channel per sensor are opened once, early, after
//!   freeing and compacting memory ([`Av100Encoder::open_all`]), and never
//!   re-created: `ak_venc_adapter` V1.0.74 oopses instead of failing when it
//!   cannot get contiguous DMA memory.

pub mod conf;
pub mod device;
pub mod pcm;
pub mod v4l2;
pub mod venc;

use crate::device::{ChannelSpec, Options, Sensor};
use crate::venc::Settings;
use camera::backend::{self, BoxError, Encoder, FrameRef, Packet, PixelFormat, SensorInfo};
use snafu::{ensure, ResultExt, Snafu};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use video_proto::motion::MotionGrid;

/// Where the kernel lists the sensors (`v4l-subdevN`) and their names.
pub const SYSFS: &str = "/sys/class/video4linux";

/// Why a sensor has no usable tuning file.
#[derive(Debug, Snafu)]
pub enum TuningError {
    #[snafu(display("no sensor {index}: {}: {source}", path.display()))]
    NoSensor {
        index: u32,
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("no ISP tuning file {} for sensor {index} ({sensor})", path.display()))]
    NoTuning {
        index: u32,
        sensor: String,
        path: PathBuf,
    },
}

/// Tuning file of sensor `index`: `<dir>/isp_<sensor>_mipi_2lane_mipi<N>.conf`,
/// `<sensor>` being the subdev's driver name under `sysfs` (`sc2331`).
pub fn tuning_file(sysfs: &Path, dir: &Path, index: u32) -> Result<PathBuf, TuningError> {
    let name_path = sysfs.join(format!("v4l-subdev{index}/name"));
    let name = std::fs::read_to_string(&name_path).context(NoSensorSnafu {
        index,
        path: name_path,
    })?;
    let sensor = name.split_whitespace().next().unwrap_or("unknown");
    let path = dir.join(format!("isp_{sensor}_mipi_2lane_mipi{index}.conf"));
    ensure!(
        path.is_file(),
        NoTuningSnafu {
            index,
            sensor,
            path
        }
    );
    Ok(path)
}

/// `venc` profile constant for a board profile's `profile` name
/// (`baseline`, `main`, `high`; case-insensitive).
pub fn venc_profile(name: &str) -> Option<u32> {
    match name.to_ascii_lowercase().as_str() {
        "baseline" => Some(venc::PROFILE_BASELINE),
        "main" => Some(venc::PROFILE_MAIN),
        "high" => Some(venc::PROFILE_HIGH),
        _ => None,
    }
}

/// Builds the capture options of one sensor and the position of the
/// channel the service reads (the preview, i.e. the sub channel).
pub fn options(
    index: u32,
    conf: PathBuf,
    dual: bool,
    main: Option<(u32, u32)>,
    preview: (u32, u32),
) -> (Options, usize) {
    let mut channels = Vec::new();
    if let Some((w, h)) = main {
        channels.push(ChannelSpec {
            node: 0,
            width: w,
            height: h,
            buffers: 3,
        });
    }
    channels.push(ChannelSpec {
        node: 1,
        width: preview.0,
        height: preview.1,
        buffers: 2,
    });
    let preview_index = channels.len() - 1;
    let opts = Options {
        index,
        conf,
        slot: 0,
        dual,
        channels,
        dev: "/dev".into(),
    };
    (opts, preview_index)
}

/// Every sensor of the board, each with the channel the service reads.
pub struct Av100Backend {
    sensors: Vec<(Options, usize)>,
}

impl Av100Backend {
    /// `sensors` from [`options`]; `dual` must be set on each when more than
    /// one sensor runs.
    pub fn new(sensors: Vec<(Options, usize)>) -> Self {
        Av100Backend { sensors }
    }

    fn info(opts: &Options, preview: usize) -> SensorInfo {
        let spec = opts.channels[preview];
        SensorInfo {
            index: opts.index,
            width: spec.width,
            height: spec.height,
            active: None,
            id: None,
            tuning: opts
                .conf
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
        }
    }
}

impl backend::Backend for Av100Backend {
    type Session = Av100Session;

    fn sensors(&self) -> Vec<SensorInfo> {
        self.sensors
            .iter()
            .map(|(opts, preview)| Self::info(opts, *preview))
            .collect()
    }

    /// Opens every sensor, then starts every sensor (dual mode).
    fn start(&mut self) -> Result<Av100Session, BoxError> {
        let mut open = Vec::new();
        for (opts, _) in &self.sensors {
            open.push(Sensor::open(opts)?);
        }
        for sensor in &mut open {
            sensor.start()?;
        }
        let infos = self
            .sensors
            .iter()
            .zip(&open)
            .map(|((opts, preview), sensor)| SensorInfo {
                active: Some((sensor.width, sensor.height)),
                id: Some(format!("{:#06x}", sensor.sensor_id)),
                ..Self::info(opts, *preview)
            })
            .collect();
        let previews = self.sensors.iter().map(|(_, p)| *p).collect();
        Ok(Av100Session {
            sensors: open,
            previews,
            infos,
        })
    }
}

/// Started sensors; dropping it stops and releases them all.
pub struct Av100Session {
    sensors: Vec<Sensor>,
    previews: Vec<usize>,
    infos: Vec<SensorInfo>,
}

impl backend::Session for Av100Session {
    fn sensors(&self) -> Vec<SensorInfo> {
        self.infos.clone()
    }

    fn with_frame(
        &mut self,
        slot: usize,
        timeout: Duration,
        f: &mut dyn FnMut(&FrameRef<'_>) -> Result<usize, BoxError>,
    ) -> Result<usize, BoxError> {
        let preview = self.previews[slot];
        // The capture buffer is lent as is; its physical address goes along
        // so the encoder reads it by DMA.
        self.sensors[slot].with_frame(preview, timeout, |d| {
            f(&FrameRef {
                width: d.width,
                height: d.height,
                format: PixelFormat::Nv12,
                data: d.data,
                phys: Some(d.phys),
                timestamp_ms: d.timestamp_ms,
            })
        })?
    }

    /// The ISP's 3D-NR motion grid (UNVERIFIED on hardware: the command
    /// comes from the vendor library's disassembly, see
    /// [`conf::GET_3D_NR_STAT`]).
    fn motion_grid(&mut self, slot: usize) -> Result<Option<MotionGrid>, BoxError> {
        let stat = self.sensors[slot].motion_grid()?;
        Ok(Some(MotionGrid::new(
            0,
            0,
            conf::NR3D_STAT_COLS as u8,
            conf::NR3D_STAT_ROWS as u8,
            stat.max,
            stat.cells.to_vec(),
        )?))
    }
}

/// Writes `value` to a procfs knob, ignoring failure (not root, no such knob).
fn proc_write(path: &str, value: &str) {
    if let Err(e) = std::fs::write(path, value) {
        tracing::debug!(path, error = %e, "procfs write failed");
    }
}

/// Frees and defragments memory for the encoder driver, whose channel
/// set-up allocates 16 KiB contiguous blocks and, in `ak_venc_adapter`
/// V1.0.74, oopses instead of failing when it cannot get them.
pub fn make_room_for_encoder() {
    proc_write("/proc/sys/vm/drop_caches", "3");
    proc_write("/proc/sys/vm/compact_memory", "1");
}

/// Opens `/dev/venc`. Its kernel init allocates 16 KiB contiguous blocks;
/// when memory is fragmented that fails, so drop the page cache, compact,
/// and try once more.
fn open_encoder(dev: &Path) -> Result<venc::Encoder, venc::Error> {
    // A larger free reserve keeps some higher-order blocks around for the
    // driver (the stock kernel keeps only 256 KiB).
    let _ = std::fs::write("/proc/sys/vm/min_free_kbytes", "1024");
    make_room_for_encoder();
    match venc::Encoder::open(dev) {
        Ok(e) => Ok(e),
        Err(first) => {
            tracing::warn!(error = %first, "encoder open failed; compacting memory and retrying");
            make_room_for_encoder();
            venc::Encoder::open(dev)
        }
    }
}

/// One `/dev/venc` H.264 channel.
pub struct Av100Encoder {
    chan: venc::Channel,
    encoder: Arc<venc::Encoder>,
    settings: Settings,
}

impl Av100Encoder {
    /// Opens the encoder and one channel per entry of `settings` (one per
    /// sensor, in slot order), once. Call it at start-up, before capture
    /// buffers and clients fragment memory: the encoder's kernel init and
    /// every channel set-up need contiguous pages, and channels are never
    /// closed and re-created while the service runs, because each creation
    /// risks the driver oops on a fragmented system.
    pub fn open_all(settings: &[Settings]) -> Result<Vec<Av100Encoder>, venc::Error> {
        let dev = Path::new("/dev");
        let encoder = Arc::new(open_encoder(dev)?);
        let mut channels = Vec::new();
        for &settings in settings {
            make_room_for_encoder();
            channels.push(Av100Encoder {
                chan: encoder.channel(dev, settings)?,
                encoder: Arc::clone(&encoder),
                settings,
            });
        }
        Ok(channels)
    }
}

/// `StreamInfo` of a channel with `settings` on `encoder` (version string).
pub fn stream_info(settings: &Settings, encoder: &str) -> video_proto::StreamInfo {
    video_proto::StreamInfo {
        codec: "H.264".into(),
        profile: match settings.profile {
            venc::PROFILE_BASELINE => "Baseline",
            venc::PROFILE_HIGH => "High",
            _ => "Main",
        }
        .into(),
        width: settings.width,
        height: settings.height,
        fps: settings.fps,
        gop: settings.gop,
        target_kbps: settings.kbps,
        kbps: 0.0,
        encoder: encoder.to_owned(),
    }
}

impl Encoder for Av100Encoder {
    fn info(&self) -> video_proto::StreamInfo {
        stream_info(&self.settings, &self.encoder.version)
    }

    fn encode(
        &mut self,
        frame: &FrameRef<'_>,
        out: &mut dyn FnMut(Packet<'_>),
    ) -> Result<usize, BoxError> {
        let phys = frame
            .phys
            .ok_or("encoder: the picture has no physical address (not a capture buffer)")?;
        let frame = device::FrameRef {
            width: frame.width,
            height: frame.height,
            data: frame.data,
            phys,
            timestamp_ms: frame.timestamp_ms,
        };
        Ok(self.chan.encode(&frame, |pkt| {
            out(Packet {
                data: pkt.data,
                keyframe: pkt.keyframe,
            });
            pkt.data.len()
        })?)
    }

    fn request_idr(&mut self) -> Result<(), BoxError> {
        Ok(self.chan.request_idr()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_put_the_preview_last() {
        let (o, p) = options(1, "/x/isp.conf".into(), true, None, (640, 360));
        assert_eq!((p, o.channels.len(), o.channels[0].node), (0, 1, 1));
        let (o, p) = options(
            1,
            "/x/isp.conf".into(),
            true,
            Some((1920, 1080)),
            (640, 360),
        );
        assert_eq!((p, o.channels[p].width, o.channels[p].node), (1, 640, 1));
        let b = Av100Backend::new(vec![(o, p)]);
        let info = backend::Backend::sensors(&b);
        assert_eq!(info[0].tuning.as_deref(), Some("isp.conf"));
        assert_eq!(
            (info[0].index, info[0].width, info[0].height),
            (1, 640, 360)
        );
    }

    #[test]
    fn stream_info_names_the_profile() {
        let s = Settings {
            width: 1280,
            height: 720,
            fps: 15,
            gop: 30,
            kbps: 2048,
            profile: venc::PROFILE_MAIN,
            level: 40,
        };
        let i = stream_info(&s, "V1.0.74");
        assert_eq!((i.codec.as_str(), i.profile.as_str()), ("H.264", "Main"));
        assert_eq!((i.width, i.gop, i.target_kbps), (1280, 30, 2048));
        let high = stream_info(
            &Settings {
                profile: venc::PROFILE_HIGH,
                ..s
            },
            "",
        );
        assert_eq!(high.profile, "High");
    }

    #[test]
    fn tuning_file_is_named_after_the_sensor_driver() {
        let root = std::env::temp_dir().join(format!("av100-tuning-{}", std::process::id()));
        let sysfs = root.join("sys");
        std::fs::create_dir_all(sysfs.join("v4l-subdev1")).unwrap();
        std::fs::write(sysfs.join("v4l-subdev1/name"), "sc2331 1-0030\n").unwrap();
        let err = tuning_file(&sysfs, &root, 1).unwrap_err();
        assert!(matches!(err, TuningError::NoTuning { ref sensor, .. } if sensor == "sc2331"));
        let conf = root.join("isp_sc2331_mipi_2lane_mipi1.conf");
        std::fs::write(&conf, b"").unwrap();
        assert_eq!(tuning_file(&sysfs, &root, 1).unwrap(), conf);
        assert!(matches!(
            tuning_file(&sysfs, &root, 0),
            Err(TuningError::NoSensor { index: 0, .. })
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The 3D-NR grid fits the motion socket's limits as is.
    #[test]
    fn motion_stat_fits_a_motion_grid() {
        use video_proto::motion::{MAX_COLS, MAX_ROWS};
        assert_eq!(conf::NR3D_STAT_COLS, usize::from(MAX_COLS));
        assert_eq!(conf::NR3D_STAT_ROWS, usize::from(MAX_ROWS));
        let raw = [0u8; conf::NR3D_STAT_LEN];
        let stat = device::MotionStat::parse(&raw);
        let g = MotionGrid::new(0, 0, 32, 24, stat.max, stat.cells.to_vec()).unwrap();
        assert_eq!(g.cells.len(), 768);
    }

    #[test]
    fn venc_profile_names() {
        assert_eq!(venc_profile("main"), Some(venc::PROFILE_MAIN));
        assert_eq!(venc_profile("High"), Some(venc::PROFILE_HIGH));
        assert_eq!(venc_profile("baseline"), Some(venc::PROFILE_BASELINE));
        assert_eq!(venc_profile("extended"), None);
    }
}
