//! RTSP server demo — Rust port of `cross-compile/rtsp/ak_rtsp_demo.c`.
//!
//! Starts the SDK's own RTSP server on both channels:
//!
//! * `rtsp://<camera>:554/vs0` — main channel
//! * `rtsp://<camera>:554/vs1` — sub channel
//!
//! The C version also ran a photosensitive day/night thread that poked
//! `/sys/user-gpio/*` through `system("echo ... > ...")`. That is reimplemented
//! here as direct file writes: shelling out with an interpolated string is a
//! command-injection shape, and `system()` on a 32 MB device forks the whole
//! address space for every switch.

use anyka::rtsp::{RtspChannelConfig, RtspServer};
use anyka::venc::{BitrateMode, OutputType};
use anyka::vi::{ChannelAttr, Crop, DayNightMode, Resolution, VideoDevice, VideoInput};
use anyka::{shutdown, time, IrSensor, LightLevel};
use clap::Parser;
use eyre::Result;
use metrics::{counter, describe_counter, describe_gauge, gauge};
use std::path::Path;
use std::time::Duration;

const IRCUT_GPIO: &str = "/sys/user-gpio/gpio-ircut_a";
const IRLED_GPIO: &str = "/sys/user-gpio/IR_LED";
const DAY_NIGHT_POLL: Duration = Duration::from_secs(5);

/// RTSP server for the main and sub channels, with day/night switching.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long, default_value_t = 1280)]
    main_width: i32,
    #[arg(long, default_value_t = 720)]
    main_height: i32,
    #[arg(long, default_value_t = 640)]
    sub_width: i32,
    #[arg(long, default_value_t = 480)]
    sub_height: i32,
    /// Main channel target bitrate.
    #[arg(long, default_value_t = 2000)]
    main_kbps: i32,
    /// Sub channel target bitrate.
    #[arg(long, default_value_t = 200)]
    sub_kbps: i32,
    /// Main stream path: rtsp://<camera>/<name>
    #[arg(long = "main-name", default_value = "vs0")]
    main_suffix: String,
    /// Sub stream path.
    #[arg(long = "sub-name", default_value = "vs1")]
    sub_suffix: String,
    /// GOP setting passed to the encoder.
    #[arg(long, default_value_t = 2)]
    gop: i32,
    #[arg(long = "minqp", default_value_t = 20)]
    min_qp: i32,
    #[arg(long = "maxqp", default_value_t = 51)]
    max_qp: i32,
    #[arg(long, default_value_t = 25)]
    fps: i32,
}

/// Writes `value` to a `/sys/user-gpio` node.
///
/// Replaces the C demo's `sprintf` + `system("echo ...")`, which spawned a
/// shell with an interpolated command string on every day/night transition.
fn set_gpio(path: &str, value: u8) {
    if let Err(error) = std::fs::write(Path::new(path), format!("{value}\n")) {
        tracing::warn!(path, %error, "cannot write GPIO node");
    }
}

/// Switches the ISP profile and the IR hardware together.
///
/// Both GPIO values and their ordering are kept exactly as the C demo drove
/// them under `DAY_LEVEL_LL`: the two nodes are active-low on this board, so
/// day writes `1` to both and night writes `0`. The order differs per
/// direction so the IR-cut filter never moves while the scene is unlit.
fn apply_day_night(vi: &VideoInput, day: bool) -> Result<()> {
    let level = u8::from(day);
    counter!("kamoder_daynight_transitions_total").increment(1);
    gauge!("kamoder_daynight_is_day").set(f64::from(level));
    if day {
        tracing::info!("switching to day mode");
        set_gpio(IRCUT_GPIO, level);
        vi.switch_mode(DayNightMode::Day)?;
        set_gpio(IRLED_GPIO, level);
    } else {
        tracing::info!("switching to night mode");
        set_gpio(IRLED_GPIO, level);
        vi.switch_mode(DayNightMode::Night)?;
        set_gpio(IRCUT_GPIO, level);
    }
    time::sleep(Duration::from_millis(300));
    Ok(())
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let args = Args::parse();
    let _telemetry = telemetry::install("rtsp_demo")?;
    describe_counter!(
        "kamoder_daynight_transitions_total",
        "Day/night profile switches applied"
    );
    describe_gauge!("kamoder_daynight_is_day", "1 in day mode, 0 in night mode");

    // Installs SIGINT/SIGTERM handling and ignores SIGPIPE, so an RTSP client
    // that vanishes mid-stream does not kill the process.
    shutdown::install();

    let isp_config = VideoInput::match_sensor_auto()?;
    tracing::info!(config = %isp_config.display(), "sensor matched");
    let vi = VideoInput::open(VideoDevice::Dev0)?;
    let sensor = vi.sensor_resolution()?;
    tracing::info!(
        width = sensor.width,
        height = sensor.height,
        "sensor resolution"
    );

    vi.set_channel_attr(ChannelAttr {
        crop: Crop {
            left: 0,
            top: 0,
            width: sensor.width,
            height: sensor.height,
        },
        main: Resolution::with_max(args.main_width, args.main_height, 640, 480),
        sub: Resolution::with_max(args.sub_width, args.sub_height, 640, 480),
    })?;
    vi.capture_on()?;
    // The lens is mounted upside down on these cameras.
    vi.set_flip_mirror(true, true)?;
    tracing::info!("video input ready");

    let main = RtspChannelConfig {
        width: args.main_width,
        height: args.main_height,
        fps: args.fps,
        max_kbps: args.main_kbps,
        min_qp: args.min_qp,
        max_qp: args.max_qp,
        gop_len: args.gop,
        codec: OutputType::H264,
        bitrate_mode: BitrateMode::Cbr,
        suffix: args.main_suffix.clone(),
    };
    let sub = RtspChannelConfig {
        width: args.sub_width,
        height: args.sub_height,
        max_kbps: args.sub_kbps,
        suffix: args.sub_suffix.clone(),
        ..main.clone()
    };

    let server = RtspServer::start(&vi, &main, &sub)?;
    tracing::info!(
        main = %format!("rtsp://<camera>:554/{}", args.main_suffix),
        sub = %format!("rtsp://<camera>:554/{}", args.sub_suffix),
        "rtsp server started"
    );

    // Day/night follows the IR photosensor. A `None` reading means the driver
    // has no value yet, which is not an error worth acting on.
    let ir = IrSensor::init()?;
    let mut previous: Option<LightLevel> = None;
    while shutdown::running() {
        if let Some(level) = ir.light_level() {
            if previous != Some(level) {
                apply_day_night(&vi, level == LightLevel::Day)?;
                previous = Some(level);
            }
        }
        time::sleep(DAY_NIGHT_POLL);
    }

    tracing::info!("stopping rtsp");
    // Stops both channels and calls `ak_rtsp_exit`.
    drop(server);
    vi.capture_off()?;
    Ok(())
}
