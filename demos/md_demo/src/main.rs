//! Motion detection demo — Rust port of
//! `cross-compile/md_demo/ak_vpss_md_demo.c`.
//!
//! Reads the ISP's per-block motion energy every frame and applies the same
//! two-level threshold as the C original: a block counts as moving above
//! `LEVEL_THRESHOLD`, and the frame counts as moving once more than
//! `BLOCK_THRESHOLD` blocks do.

use anyka::md::MotionStat;
use anyka::shutdown;
use anyka::vi::{ChannelAttr, Crop, Resolution, VideoDevice, VideoInput};
use clap::Parser;
use eyre::Result;
use metrics::{counter, describe_counter, describe_gauge, gauge};

const LEVEL_THRESHOLD: u16 = 5000;
const BLOCK_THRESHOLD: usize = 12;
const DEFAULT_FRAMES: usize = 500;

/// ISP motion detection statistics (EV200 SDK).
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
    let _telemetry = telemetry::install("md_demo")?;
    describe_counter!("kamoder_md_frames_total", "Frames analysed for motion");
    describe_counter!(
        "kamoder_md_events_total",
        "Frames that crossed the motion threshold"
    );
    describe_gauge!(
        "kamoder_md_active_blocks",
        "Blocks above the level threshold in the last frame"
    );

    shutdown::install();

    let frames = args.frames;

    let isp_config = VideoInput::match_sensor_auto()?;
    tracing::info!(config = %isp_config.display(), "sensor matched");

    let vi = VideoInput::open(VideoDevice::Dev0)?;
    if let Ok(res) = vi.sensor_resolution() {
        tracing::info!(width = res.width, height = res.height, "sensor resolution");
    }

    // Same geometry as the C demo. `max_*` is the scaler ceiling, which the
    // hardware caps at 640x480 regardless of the requested output size.
    vi.set_channel_attr(ChannelAttr {
        crop: Crop {
            left: 0,
            top: 0,
            width: 1280,
            height: 720,
        },
        main: Resolution::with_max(1280, 720, 640, 360),
        sub: Resolution::with_max(640, 360, 640, 480),
    })?;
    vi.set_fps(25)?;
    tracing::info!(fps = vi.fps(), "capture configured");

    vi.capture_on()?;

    // The ISP needs a couple of seconds of frames before its motion statistics
    // settle, so the first batch is fetched and dropped.
    let warmup = match vi.fps() {
        fps if fps > 0 => (fps * 2) as usize,
        _ => 40,
    };
    for _ in 0..warmup {
        if !shutdown::running() {
            break;
        }
        // The frame guard releases the ISP buffer at the end of each iteration.
        let _ = vi.get_frame();
    }

    for index in 0..frames {
        if !shutdown::running() {
            tracing::info!(frames = index, "interrupted");
            break;
        }
        let Ok(frame) = vi.get_frame() else {
            tracing::warn!("get frame failed");
            continue;
        };
        match MotionStat::read(&vi) {
            Ok(stat) => {
                let active = stat.blocks_above(LEVEL_THRESHOLD);
                counter!("kamoder_md_frames_total").increment(1);
                gauge!("kamoder_md_active_blocks").set(active as f64);
                if stat.is_motion(LEVEL_THRESHOLD, BLOCK_THRESHOLD) {
                    counter!("kamoder_md_events_total").increment(1);
                    tracing::info!(active_blocks = active, peak = stat.max, "motion detected");
                } else {
                    tracing::debug!(active_blocks = active, peak = stat.max, "no motion");
                }
            }
            Err(error) => tracing::warn!(%error, "cannot read motion statistics"),
        }
        drop(frame);
    }

    // `capture_off` is explicit so a failure is reported; dropping `vi` closes
    // the device either way.
    vi.capture_off()?;
    tracing::info!("md demo finished");
    Ok(())
}
