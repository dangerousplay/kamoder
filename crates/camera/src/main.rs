//! The camera service and its bring-up probe.
//!
//! ```text
//! camera serve [--listen 127.0.0.1:3000]     every sensor, snapshots over HTTP
//! camera probe --index 1 --frames 5 --save /tmp/cam1.jpg
//! ```
//!
//! Tuning files are looked up as `<conf-dir>/isp_<sensor>_mipi_2lane_mipi<N>.conf`,
//! `<sensor>` being the subdev's driver name (`sc2331`). No vendor library is
//! involved: everything goes through the kernel's V4L2 nodes.

// Linked for its side effect: `os` supplies the `pthread_setname_np` shim
// the crosstool's uClibc does not export but Rust's thread startup references.
extern crate os;

use camera::av100::{self, Av100Backend, Av100Encoder};
use camera::backend::Encoder;
use camera::device::{self, Sensor};
use camera::service::{self, AppState, Camera, CaptureOptions};
use clap::{Args, Parser, Subcommand};
use eyre::{bail, Context, Result};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};

const SYSFS: &str = "/sys/class/video4linux";

/// Image sensor capture and JPEG snapshots, without vendor libraries.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Capture from every sensor and serve snapshots.
    Serve(Serve),
    /// Bring one sensor up, grab frames, report, optionally save a JPEG.
    Probe(Probe),
}

#[derive(Args, Debug, Clone)]
struct Common {
    /// Directory holding the ISP tuning files.
    #[arg(long, default_value = "/usr/local")]
    conf_dir: PathBuf,
    /// Main channel size (`WxH`), or `off`. The preview is the sub channel.
    #[arg(long, default_value = "off", value_parser = parse_size)]
    main: Size,
    /// Preview (sub channel) size.
    #[arg(long, default_value = "640x360", value_parser = parse_size)]
    preview: Size,
}

#[derive(Args, Debug)]
struct Serve {
    #[command(flatten)]
    common: Common,
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: SocketAddr,
    /// JPEG quality, 1..=100.
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u8).range(1..=100))]
    quality: u8,
    /// Only these sensors (comma separated); all by default.
    #[arg(long, value_delimiter = ',')]
    sensors: Vec<u32>,
    /// Hardware H.264 stream size per sensor (the capture channel runs at
    /// it; snapshots are taken from it, halved above 640 wide), or `off`.
    #[arg(long, default_value = "1280x720", value_parser = parse_size)]
    stream: Size,
    /// H.264 constant bitrate, kbit/s.
    #[arg(long, default_value_t = 2048, value_parser = clap::value_parser!(u32).range(64..=8000))]
    kbps: u32,
    /// Frames between IDR pictures.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=600))]
    gop: u32,
    /// Unix socket the `rtsp` service reads H.264 from.
    #[arg(long, default_value = "/tmp/kamoder-video.sock")]
    video_socket: PathBuf,
}

#[derive(Args, Debug)]
struct Probe {
    #[command(flatten)]
    common: Common,
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

#[derive(Debug, Clone, Copy)]
struct Size(Option<(u32, u32)>);

fn parse_size(s: &str) -> Result<Size, String> {
    if s == "off" {
        return Ok(Size(None));
    }
    let (w, h) = s.split_once('x').ok_or("expected WxH or off")?;
    let w = w.parse::<u32>().map_err(|e| e.to_string())?;
    let h = h.parse::<u32>().map_err(|e| e.to_string())?;
    if w < 32 || h < 18 || w % 2 != 0 || h % 2 != 0 {
        return Err("size too small or odd".into());
    }
    Ok(Size(Some((w, h))))
}

/// Tuning file for sensor `index`.
fn conf_for(dir: &Path, index: u32) -> Result<PathBuf> {
    let name = std::fs::read_to_string(Path::new(SYSFS).join(format!("v4l-subdev{index}/name")))
        .wrap_err_with(|| format!("no sensor {index}"))?;
    let sensor = name.split_whitespace().next().unwrap_or("unknown");
    let path = dir.join(format!("isp_{sensor}_mipi_2lane_mipi{index}.conf"));
    if !path.is_file() {
        bail!(
            "no ISP tuning file {} for sensor {index} ({sensor})",
            path.display()
        );
    }
    Ok(path)
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
            None => conf_for(&p.common.conf_dir, index)?,
        };
        let (opts, c) = av100::options(index, conf, dual, p.common.main.0, preview);
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
) -> Result<()> {
    use camera::venc::{self, Encoder, Settings};
    use std::io::Write;
    let (width, height) = size.ok_or_else(|| eyre::eyre!("--preview cannot be off"))?;
    let dev = Path::new("/dev");
    let encoder = Encoder::open(dev)?;
    println!("encoder: {}", encoder.version);
    let settings = Settings {
        width,
        height,
        fps: 15,
        gop: 30,
        kbps,
        profile: venc::PROFILE_MAIN,
        level: 40,
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
    let found = device::sensors(Path::new(SYSFS));
    let indices: Vec<u32> = if s.sensors.is_empty() {
        found.clone()
    } else {
        s.sensors.clone()
    };
    if indices.is_empty() {
        bail!("no image sensor found under {SYSFS}");
    }
    // The encoder first: its kernel init needs contiguous pages, which are
    // plentiful at boot and scarce once the system has been running a while.
    let encoder = match s.stream.0 {
        Some((width, height)) => Some((
            Arc::new(av100::open_encoder()?),
            camera::venc::Settings {
                width,
                height,
                fps: 15,
                gop: s.gop,
                kbps: s.kbps,
                profile: camera::venc::PROFILE_MAIN,
                level: 40,
            },
        )),
        None => None,
    };
    let preview = match &encoder {
        Some((_, st)) => (st.width, st.height),
        None => s
            .common
            .preview
            .0
            .ok_or_else(|| eyre::eyre!("--preview cannot be off"))?,
    };
    // Channels right away too, before capture buffers and clients fragment
    // memory (see `Av100Encoder::open_all`).
    let encoders: Vec<Option<Box<dyn Encoder>>> = match &encoder {
        Some((e, st)) => Av100Encoder::open_all(e, *st, indices.len())?
            .into_iter()
            .map(|c| Some(Box::new(c) as Box<dyn Encoder>))
            .collect(),
        None => Vec::new(),
    };
    let dual = indices.len() > 1;
    let stop = Arc::new(AtomicBool::new(false));
    let mut cameras = Vec::new();
    let mut sensors = Vec::new();
    for &index in &indices {
        cameras.push(Camera::default());
        let conf = conf_for(&s.common.conf_dir, index)?;
        sensors.push(av100::options(index, conf, dual, s.common.main.0, preview));
    }
    let worker = {
        let (cameras, stop) = (cameras.clone(), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                service::capture(
                    Av100Backend::new(sensors),
                    cameras,
                    encoders,
                    stop,
                    CaptureOptions::default(),
                )
            })?
    };
    tracing::info!(sensors = ?indices, listen = %s.listen, "camera service starting");

    let state = AppState {
        cameras: Arc::new(cameras),
        quality: s.quality,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(s.listen)
            .await
            .wrap_err_with(|| format!("cannot listen on {}", s.listen))?;
        let _ = std::fs::remove_file(&s.video_socket);
        let video = tokio::net::UnixListener::bind(&s.video_socket)
            .wrap_err_with(|| format!("cannot listen on {}", s.video_socket.display()))?;
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
