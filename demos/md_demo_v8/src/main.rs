//! Motion detection on the V8 SDK — the MVP that proves the SDK swap.
//!
//! Same two-level threshold as `md_demo` (the EV200 port), but driven through
//! the V8 device/channel API. It is deliberately loud about what it observed:
//! the acceptance criterion for this port is *real frame data*, not a zero exit
//! code, so every frame's length, sequence number and physical address are
//! available and the run ends with a summary that cannot be faked by a
//! no-op capture path.

use anyka_v8::{Channel, ChannelAttr, DeviceAttr, MotionStat, VideoDevice, VI_DEV0};
use clap::Parser;
use eyre::{bail, Result};
use metrics::{counter, describe_counter, describe_gauge, gauge};
use os::shutdown;

const LEVEL_THRESHOLD: u16 = 5000;
const BLOCK_THRESHOLD: usize = 12;
const DEFAULT_FRAMES: usize = 200;

/// ISP motion detection on the AK3918AV100 V8 SDK; fails unless real frames are captured.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Frames to analyse after the warm-up.
    #[arg(default_value_t = DEFAULT_FRAMES)]
    frames: usize,
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let args = Args::parse();
    let _telemetry = telemetry::install("md_demo_v8")?;
    describe_counter!("kamoder_md_frames_total", "Frames analysed for motion");
    describe_counter!(
        "kamoder_md_events_total",
        "Frames that crossed the motion threshold"
    );
    describe_gauge!(
        "kamoder_md_active_blocks",
        "Blocks above the level threshold in the last frame"
    );
    describe_gauge!("kamoder_md_frame_bytes", "Size of the last captured frame");

    shutdown::install();

    let frames = args.frames;

    tracing::info!(version = %anyka_v8::version::video_in(), "V8 video input");

    let device = VideoDevice::open(VI_DEV0)?;
    let config = device.load_sensor_cfg_auto()?;
    tracing::info!(config = %config.display(), "sensor matched");

    match device.sensor_id() {
        Ok(id) => tracing::info!(sensor_id = format!("{id:#x}"), "sensor detected"),
        Err(error) => tracing::warn!(%error, "sensor id unavailable"),
    }

    // The sensor's own maximum drives the device geometry — hardcoding 1280x720
    // is what makes the vendor demos fail on boards with a different sensor.
    let mut attr = DeviceAttr::default();
    match device.sensor_resolution() {
        Ok((width, height)) if width > 0 && height > 0 => {
            tracing::info!(width, height, "sensor resolution");
            attr.max_width = width;
            attr.max_height = height;
            attr.crop = anyka_v8::Crop {
                left: 0,
                top: 0,
                width,
                height,
            };
        }
        Ok(_) => tracing::warn!("sensor reported a zero resolution; using defaults"),
        Err(error) => tracing::warn!(%error, "sensor resolution unavailable; using defaults"),
    }
    device.set_attr(attr)?;

    // Sub channel: smallest path that still exercises the ISP, and the least
    // DMA pressure on a 32 MB device.
    let channel = device.channel(Channel::Sub, ChannelAttr::default())?;
    device.enable()?;
    tracing::info!("capture started");

    // The ISP's motion statistics need a couple of seconds of frames before
    // they settle, so the first batch is fetched and dropped.
    let warmup = (attr.frame_rate.max(1) * 2) as usize;
    for _ in 0..warmup {
        if !shutdown::running() {
            break;
        }
        let _ = channel.get_frame();
    }

    let mut captured = 0usize;
    let mut total_bytes = 0u64;
    let mut events = 0usize;
    let mut peak = 0u16;
    let mut first_sequence = None;
    let mut last_sequence = 0u64;

    for index in 0..frames {
        if !shutdown::running() {
            tracing::info!(frames = index, "interrupted");
            break;
        }
        let frame = match channel.get_frame() {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!(%error, "get frame failed");
                continue;
            }
        };

        let bytes = frame.data().len();
        captured += 1;
        total_bytes += bytes as u64;
        first_sequence.get_or_insert(frame.sequence());
        last_sequence = frame.sequence();
        counter!("kamoder_md_frames_total").increment(1);
        gauge!("kamoder_md_frame_bytes").set(bytes as f64);

        if captured <= 3 {
            // The first few frames in full, so a log from the camera is enough
            // to tell real capture from a zero-filled stub.
            tracing::info!(
                seq = frame.sequence(),
                ts_ms = frame.timestamp_ms(),
                bytes,
                phys = format!("{:#x}", frame.phys_addr()),
                "frame captured"
            );
        }

        match MotionStat::read(&device) {
            Ok(stat) => {
                let active = stat.blocks_above(LEVEL_THRESHOLD);
                peak = peak.max(stat.max());
                gauge!("kamoder_md_active_blocks").set(active as f64);
                if stat.is_motion(LEVEL_THRESHOLD, BLOCK_THRESHOLD) {
                    events += 1;
                    counter!("kamoder_md_events_total").increment(1);
                    tracing::info!(active_blocks = active, peak = stat.max(), "motion detected");
                } else {
                    tracing::debug!(active_blocks = active, peak = stat.max(), "no motion");
                }
            }
            Err(error) => tracing::warn!(%error, "cannot read motion statistics"),
        }
    }

    tracing::info!(
        captured,
        total_bytes,
        events,
        motion_peak = peak,
        first_sequence,
        last_sequence,
        "md demo v8 finished"
    );

    // A run that captured nothing must not look like success: the whole point
    // of this binary is to prove the V8 path moves real pixels.
    if captured == 0 {
        bail!("no frames captured");
    }
    if total_bytes == 0 {
        bail!("{captured} frames captured but every one was empty");
    }
    Ok(())
}
