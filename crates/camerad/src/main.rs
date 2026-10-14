//! The camera service and its bring-up probe.
//!
//! ```text
//! camera serve [--listen 127.0.0.1:3000]     every sensor, snapshots over HTTP
//! camera probe --index 1 --frames 5 --save /tmp/cam1.jpg
//! ```
//!
//! What runs comes from the board profile (`board::current()`, `[video]`):
//! the backend (`anyka-av100`: the AV100 ISP and `/dev/venc`;
//! `test-pattern`: a moving picture, snapshots only), the sensors, the
//! tuning directory, the H.264 stream of each sensor and the snapshot size
//! and quality. Flags given on the command line win over the profile (see
//! `settings`). No vendor library is involved.
//!
//! AV100 tuning files are looked up as
//! `<conf-dir>/isp_<sensor>_mipi_2lane_mipi<N>.conf`, `<sensor>` being the
//! subdev's driver name (`sc2331`).

// Linked for its side effect: `os` supplies the `pthread_setname_np` shim
// the crosstool's uClibc does not export but Rust's thread startup references.
extern crate os;

mod settings;

use anyka_av100::device::{self, Sensor};
use anyka_av100::{venc, Av100Backend, Av100Encoder, SYSFS};
use camera::backend::{Backend, Encoder};
use camera::pattern::PatternBackend;
use camera::service::{self, AppState, Camera, CaptureOptions};
use clap::{Args, Parser, Subcommand};
use eyre::{bail, eyre, Context, Result};
use settings::{parse_size, Size, StreamFlags};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};

/// `[video] backend` of the AV100 boards.
const AV100: &str = "anyka-av100";
/// `[video] backend` of the hardware-free test pattern.
const TEST_PATTERN: &str = "test-pattern";
/// Tuning directory when neither `--conf-dir` nor the profile names one.
const DEFAULT_TUNING_DIR: &str = "/usr/local";
/// H.264 level 4.0 (`level_idc`), enough for 1080p30.
const H264_LEVEL: u32 = 40;
/// `probe --h264 --force` on a board without an AV100 profile.
const PROBE_FPS: u32 = 15;
const PROBE_GOP: u32 = 30;

/// Image sensor capture and JPEG snapshots, without vendor libraries.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Capture from every sensor of the board and serve snapshots.
    Serve(Serve),
    /// Bring one AV100 sensor up, grab frames, report, optionally save a JPEG.
    Probe(Probe),
}

#[derive(Args, Debug, Clone)]
struct Common {
    /// Directory holding the ISP tuning files [default: the profile's
    /// `video.tuning_dir`, else /usr/local].
    #[arg(long)]
    conf_dir: Option<PathBuf>,
    /// Main channel size (`WxH`), or `off`. The preview is the sub channel.
    #[arg(long, default_value = "off", value_parser = parse_size)]
    main: Size,
    /// Preview (sub channel) size, when there is no stream.
    #[arg(long, default_value = "640x360", value_parser = parse_size)]
    preview: Size,
}

#[derive(Args, Debug)]
struct Serve {
    #[command(flatten)]
    common: Common,
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: SocketAddr,
    /// JPEG quality, 1..=100 [default: the profile's `video.snapshot.quality`].
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=100))]
    quality: Option<u8>,
    /// Only these sensors (comma separated) [default: the profile's
    /// `video.sensors`]; those the kernel lacks are skipped with a warning.
    #[arg(long, value_delimiter = ',')]
    sensors: Vec<u32>,
    /// H.264 stream size of every sensor (the capture channel runs at it;
    /// snapshots are taken from it), or `off` [default: the profile's].
    #[arg(long, value_parser = parse_size)]
    stream: Option<Size>,
    /// Frames per second [default: the profile's].
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=60))]
    fps: Option<u32>,
    /// H.264 constant bitrate, kbit/s [default: the profile's].
    #[arg(long, value_parser = clap::value_parser!(u32).range(64..=8000))]
    kbps: Option<u32>,
    /// Frames between IDR pictures [default: the profile's].
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=600))]
    gop: Option<u32>,
    /// Snapshots are halved while wider than this [default: the profile's
    /// `video.snapshot.max_width`, else full size].
    #[arg(long)]
    snapshot_max_width: Option<u32>,
    /// Unix socket the `rtsp` service reads H.264 from.
    #[arg(long, default_value = video_proto::DEFAULT_SOCKET)]
    video_socket: PathBuf,
}

#[derive(Args, Debug)]
struct Probe {
    #[command(flatten)]
    common: Common,
    /// Probe even when the board profile's video backend is not anyka-av100
    /// (or there is no profile).
    #[arg(long)]
    force: bool,
    /// Sensor(s) to bring up, comma separated; frames are read from each in turn.
    #[arg(long, value_delimiter = ',', default_value = "0")]
    index: Vec<u32>,
    /// Set dual-sensor mode even when probing one sensor.
    #[arg(long)]
    dual: bool,
    #[arg(long, default_value_t = 5)]
    frames: u32,
    /// Explicit ISP tuning file.
    #[arg(long)]
    conf: Option<PathBuf>,
    /// Encode the frames with the hardware H.264 encoder and write the
    /// Annex-B stream of sensor N to this path with `{n}` replaced.
    #[arg(long)]
    h264: Option<String>,
    /// Bitrate for --h264.
    #[arg(long, default_value_t = 2048)]
    kbps: u32,
    /// Read the main channel instead of the preview (needs --main).
    #[arg(long)]
    from_main: bool,
    /// Write the last preview frame here as JPEG.
    #[arg(long)]
    save: Option<PathBuf>,
}

/// Tuning file for sensor `index`.
fn conf_for(dir: &Path, index: u32) -> Result<PathBuf> {
    Ok(anyka_av100::tuning_file(Path::new(SYSFS), dir, index)?)
}

/// `--conf-dir`, else the profile's `tuning_dir`, else /usr/local.
fn tuning_dir(flag: Option<PathBuf>, video: Option<&board::Video>) -> PathBuf {
    flag.or_else(|| video.and_then(|v| v.tuning_dir.clone()))
        .unwrap_or_else(|| DEFAULT_TUNING_DIR.into())
}

/// `/dev/venc` settings of a profile stream.
fn venc_settings(s: &board::Stream) -> Result<venc::Settings> {
    let profile = anyka_av100::venc_profile(&s.profile).ok_or_else(|| {
        eyre!(
            "unknown H.264 profile {:?} (baseline, main, high)",
            s.profile
        )
    })?;
    Ok(venc::Settings {
        width: s.width,
        height: s.height,
        fps: s.fps,
        gop: s.gop,
        kbps: s.kbps,
        profile,
        level: H264_LEVEL,
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let _telemetry = telemetry::install("camera")?;
    match cli.command {
        Command::Serve(s) => serve(s),
        Command::Probe(p) => probe(p),
    }
}

fn probe(p: Probe) -> Result<()> {
    // Board-specific: it drives the AV100 ISP and encoder directly.
    let board = match board::current() {
        Ok(b) if b.profile.video.backend == AV100 => Some(b),
        Ok(b) if p.force => {
            tracing::warn!(board = %b.name, backend = %b.profile.video.backend, "probing an AV100 anyway (--force)");
            None
        }
        Ok(b) => bail!(
            "probe drives the {AV100} video hardware, but board {} ({}) has video backend {:?}; use --force to probe anyway",
            b.name,
            b.path.display(),
            b.profile.video.backend
        ),
        Err(e) if p.force => {
            tracing::warn!(error = %e, "no board profile; probing an AV100 anyway (--force)");
            None
        }
        Err(e) => return Err(e).wrap_err("cannot resolve the board profile (use --force to probe anyway)"),
    };
    let video = board.as_ref().map(|b| &b.profile.video);
    let conf_dir = tuning_dir(p.common.conf_dir.clone(), video);
    let stream = video.map(|v| v.stream_for(p.index.first().copied().unwrap_or(0)));
    let preview = p
        .common
        .preview
        .0
        .ok_or_else(|| eyre::eyre!("--preview cannot be off"))?;
    let dual = p.dual || p.index.len() > 1;
    let mut sensors = Vec::new();
    let mut ch = 0;
    for &index in &p.index {
        let conf = match &p.conf {
            Some(c) => c.clone(),
            None => conf_for(&conf_dir, index)?,
        };
        let (opts, c) = anyka_av100::options(index, conf, dual, p.common.main.0, preview);
        ch = if p.from_main { 0 } else { c };
        sensors.push(Sensor::open(&opts)?);
    }
    for s in &mut sensors {
        s.start()?;
    }
    if let Some(pattern) = &p.h264 {
        return encode_probe(
            &mut sensors,
            ch,
            pattern,
            p.frames,
            p.common.preview.0,
            p.kbps,
            stream.as_ref(),
        );
    }
    let mut last = None;
    for n in 0..p.frames {
        let t = std::time::Instant::now();
        let k = n as usize % sensors.len();
        let f = match sensors[k].frame(ch, Duration::from_secs(3)) {
            Ok(f) => f,
            Err(e) => {
                println!("frame {n} (sensor {}): {e}", sensors[k].index());
                continue;
            }
        };
        let mean = f.data[..(f.width * f.height) as usize]
            .iter()
            .map(|&b| u64::from(b))
            .sum::<u64>()
            / u64::from(f.width * f.height);
        println!(
            "frame {n} (sensor {}): {}x{} ts={} ms luma_mean={mean} waited={:?}",
            sensors[k].index(),
            f.width,
            f.height,
            f.timestamp_ms,
            t.elapsed()
        );
        last = Some(f);
    }
    if let (Some(path), Some(f)) = (p.save, last) {
        let jpeg = camera::jpeg::encode(&f, 85)?;
        std::fs::write(&path, jpeg).wrap_err_with(|| format!("cannot write {}", path.display()))?;
        println!("saved {}", path.display());
    }
    Ok(())
}

/// `probe --h264`: hardware-encode `frames` pictures per sensor.
fn encode_probe(
    sensors: &mut [Sensor],
    ch: usize,
    pattern: &str,
    frames: u32,
    size: Option<(u32, u32)>,
    kbps: u32,
    stream: Option<&board::Stream>,
) -> Result<()> {
    use anyka_av100::venc::{self, Encoder, Settings};
    use std::io::Write;
    let (width, height) = size.ok_or_else(|| eyre::eyre!("--preview cannot be off"))?;
    let dev = Path::new("/dev");
    let encoder = Encoder::open(dev)?;
    println!("encoder: {}", encoder.version);
    // Rate, GOP and profile of the board's stream; the size is the preview's.
    let settings = match stream {
        Some(st) => Settings {
            width,
            height,
            kbps,
            ..venc_settings(st)?
        },
        None => Settings {
            width,
            height,
            fps: PROBE_FPS,
            gop: PROBE_GOP,
            kbps,
            profile: venc::PROFILE_MAIN,
            level: H264_LEVEL,
        },
    };
    let mut chans = Vec::new();
    let mut files = Vec::new();
    for s in sensors.iter() {
        chans.push(encoder.channel(dev, settings)?);
        let path = pattern.replace("{n}", &s.index().to_string());
        files.push(std::fs::File::create(&path).wrap_err_with(|| format!("cannot create {path}"))?);
    }
    let start = std::time::Instant::now();
    let mut bytes = vec![0usize; sensors.len()];
    for n in 0..frames {
        for k in 0..sensors.len() {
            let (chan, file) = (&mut chans[k], &mut files[k]);
            let t = std::time::Instant::now();
            let r = sensors[k].with_frame(ch, Duration::from_secs(3), |f| {
                chan.encode(&f, |pkt| {
                    let _ = file.write_all(pkt.data);
                    (pkt.data.len(), pkt.keyframe)
                })
            })?;
            let (len, key) = r?;
            bytes[k] += len;
            if n < 3 || n % 30 == 0 {
                println!(
                    "sensor {k} frame {n}: {len} B key={key} took {:?}",
                    t.elapsed()
                );
            }
        }
    }
    let secs = start.elapsed().as_secs_f32();
    for (k, b) in bytes.iter().enumerate() {
        println!(
            "sensor {k}: {frames} frames in {secs:.1}s = {:.1} fps, {:.0} kbit/s",
            frames as f32 / secs,
            *b as f32 * 8.0 / secs / 1000.0
        );
    }
    Ok(())
}

fn serve(s: Serve) -> Result<()> {
    let board = board::current().wrap_err("cannot resolve the board profile")?;
    let video = &board.profile.video;
    tracing::info!(board = %board.name, reason = %board.reason, backend = %video.backend, "board");
    let flags = StreamFlags {
        size: s.stream,
        fps: s.fps,
        kbps: s.kbps,
        gop: s.gop,
    };
    let snap = settings::snapshot(video, s.snapshot_max_width, s.quality);
    let opts = CaptureOptions {
        snapshot_max_width: snap.max_width,
        ..CaptureOptions::default()
    };
    let app = App {
        listen: s.listen,
        video_socket: s.video_socket.clone(),
        quality: snap.quality,
    };
    match video.backend.as_str() {
        AV100 => serve_av100(&s, video, &flags, opts, app),
        TEST_PATTERN => {
            let sensors = if s.sensors.is_empty() {
                video.sensors.clone()
            } else {
                s.sensors.clone()
            };
            let first = sensors.first().copied().unwrap_or(0);
            // One size and rate for every pattern sensor: the first one's.
            let (width, height, fps) = match settings::stream_for(video, first, &flags) {
                Some(st) => (st.width, st.height, st.fps),
                None => {
                    let (w, h) = s
                        .common
                        .preview
                        .0
                        .ok_or_else(|| eyre!("--preview cannot be off"))?;
                    (w, h, s.fps.unwrap_or(video.stream.fps))
                }
            };
            let backend = PatternBackend::new(sensors.clone(), width, height, fps);
            run(backend, Vec::new(), &sensors, opts, app)
        }
        other => bail!(
            "board {} ({}): video backend {other:?} is not supported by this build (known: {AV100}, {TEST_PATTERN})",
            board.name,
            board.path.display()
        ),
    }
}

/// The AV100: profile sensors that are present, `/dev/venc` channels opened
/// once up front, then the ISP backend.
fn serve_av100(
    s: &Serve,
    video: &board::Video,
    flags: &StreamFlags,
    opts: CaptureOptions,
    app: App,
) -> Result<()> {
    let found = device::sensors(Path::new(SYSFS));
    let (indices, missing) = settings::select_sensors(&video.sensors, &found, &s.sensors);
    if !missing.is_empty() {
        tracing::warn!(missing = ?missing, found = ?found, "expected sensors not present; serving the others");
    }
    if indices.is_empty() {
        bail!(
            "no image sensor found under {SYSFS} (expected {:?})",
            if s.sensors.is_empty() {
                &video.sensors
            } else {
                &s.sensors
            }
        );
    }
    let streams = indices
        .iter()
        .map(|&i| {
            settings::stream_for(video, i, flags)
                .map(|st| venc_settings(&st))
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    // The encoder and its channels first, before capture buffers and
    // clients fragment memory: its kernel init needs contiguous pages, which
    // are plentiful at boot and scarce once the system has been running a
    // while (see `Av100Encoder::open_all`). Opened once, never re-created.
    let encoders: Vec<Option<Box<dyn Encoder>>> =
        match streams.iter().copied().collect::<Option<Vec<_>>>() {
            Some(all) => Av100Encoder::open_all(&all)?
                .into_iter()
                .map(|c| Some(Box::new(c) as Box<dyn Encoder>))
                .collect(),
            None => Vec::new(),
        };
    let conf_dir = tuning_dir(s.common.conf_dir.clone(), Some(video));
    // Dual mode only when more than one sensor really runs.
    let dual = indices.len() > 1;
    let mut sensors = Vec::new();
    for (&index, stream) in indices.iter().zip(&streams) {
        let preview = match stream {
            Some(st) => (st.width, st.height),
            None => s
                .common
                .preview
                .0
                .ok_or_else(|| eyre!("--preview cannot be off"))?,
        };
        let conf = conf_for(&conf_dir, index)?;
        sensors.push(anyka_av100::options(
            index,
            conf,
            dual,
            s.common.main.0,
            preview,
        ));
    }
    run(Av100Backend::new(sensors), encoders, &indices, opts, app)
}

/// Where and how the service listens.
struct App {
    listen: SocketAddr,
    video_socket: PathBuf,
    quality: u8,
}

/// Captures from `backend` on its own thread and serves snapshots,
/// `/status` and the video socket until SIGTERM/SIGINT.
fn run<B: Backend + 'static>(
    backend: B,
    encoders: Vec<Option<Box<dyn Encoder>>>,
    indices: &[u32],
    opts: CaptureOptions,
    app: App,
) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let cameras: Vec<Camera> = indices.iter().map(|_| Camera::default()).collect();
    let worker = {
        let (cameras, stop) = (cameras.clone(), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || service::capture(backend, cameras, encoders, stop, opts))?
    };
    tracing::info!(sensors = ?indices, listen = %app.listen, "camera service starting");

    let state = AppState {
        cameras: Arc::new(cameras),
        quality: app.quality,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(app.listen)
            .await
            .wrap_err_with(|| format!("cannot listen on {}", app.listen))?;
        let _ = std::fs::remove_file(&app.video_socket);
        let video = tokio::net::UnixListener::bind(&app.video_socket)
            .wrap_err_with(|| format!("cannot listen on {}", app.video_socket.display()))?;
        tokio::spawn(service::serve_video(video, Arc::clone(&state.cameras)));
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        axum::serve(listener, service::router(state))
            .with_graceful_shutdown(async move {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                }
            })
            .await?;
        Ok::<_, eyre::Report>(())
    })?;
    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    tracing::info!("camera service stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_flags_default_to_not_given() {
        let cli = Cli::try_parse_from(["camera", "serve"]).unwrap();
        let Command::Serve(s) = cli.command else {
            panic!("not serve")
        };
        assert_eq!((s.quality, s.fps, s.kbps, s.gop), (None, None, None, None));
        assert!(s.stream.is_none() && s.common.conf_dir.is_none() && s.sensors.is_empty());
        assert_eq!(s.video_socket, Path::new("/tmp/kamoder-video.sock"));
        let cli =
            Cli::try_parse_from(["camera", "serve", "--stream", "off", "--kbps", "1000"]).unwrap();
        let Command::Serve(s) = cli.command else {
            panic!("not serve")
        };
        assert_eq!((s.stream, s.kbps), (Some(Size(None)), Some(1000)));
    }

    #[test]
    fn venc_settings_from_profile_stream() {
        let st = board::Stream {
            width: 1280,
            height: 720,
            fps: 15,
            kbps: 2048,
            gop: 30,
            profile: "main".into(),
        };
        let v = venc_settings(&st).unwrap();
        assert_eq!(
            v,
            venc::Settings {
                width: 1280,
                height: 720,
                fps: 15,
                gop: 30,
                kbps: 2048,
                profile: venc::PROFILE_MAIN,
                level: 40,
            }
        );
        let bad = board::Stream {
            profile: "extended".into(),
            ..st
        };
        assert!(venc_settings(&bad).is_err());
    }

    #[test]
    fn tuning_dir_precedence() {
        assert_eq!(tuning_dir(None, None), Path::new("/usr/local"));
        assert_eq!(tuning_dir(Some("/a".into()), None), Path::new("/a"));
    }
}
