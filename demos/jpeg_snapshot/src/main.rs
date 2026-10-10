//! JPEG snapshot server — Rust port of `cross-compile/jpeg_snapshot/main.c`.
//!
//! Captures from the ISP, encodes MJPEG frames on demand and serves the latest
//! one over HTTP at `http://<camera>:3000/snapshot.jpeg`.
//!
//! Encoding only runs while a request is outstanding, which is what keeps this
//! under ~60% CPU on a 400 MHz ARM926.

mod http;

use anyka::venc::{BitrateMode, EncodeGroup, EncodeParam, Encoder, OutputType, Profile};
use anyka::vi::{ChannelAttr, Crop, Resolution, VideoChannel, VideoDevice, VideoInput};
use anyka::{shutdown, time};
use clap::Parser;
use eyre::Result;
use http::SnapshotBuffer;
use metrics::{counter, describe_counter, describe_histogram, histogram};
use std::time::{Duration, Instant};

const DEFAULT_PORT: u16 = 3000;
/// Pause between capture attempts. 25 ms keeps CPU around 60-70%, as measured
/// by the original author.
const CAPTURE_INTERVAL: Duration = Duration::from_millis(25);

/// Serves the latest JPEG snapshot over HTTP.
///
/// `-h` is the height, as in the C original, so help is `--help` only.
#[derive(Parser)]
#[command(version, about, long_about = None, disable_help_flag = true)]
struct Args {
    /// Snapshot width, a multiple of 4.
    #[arg(short = 'w', default_value_t = 1280, value_parser = multiple_of_4)]
    width: i32,
    /// Snapshot height, a multiple of 4.
    #[arg(short = 'h', default_value_t = 720, value_parser = multiple_of_4)]
    height: i32,
    /// HTTP port.
    #[arg(short = 'p', default_value_t = DEFAULT_PORT)]
    port: u16,
    /// Print help.
    #[arg(long, action = clap::ArgAction::Help)]
    help: Option<bool>,
}

fn multiple_of_4(raw: &str) -> Result<i32, String> {
    let value: i32 = raw.parse().map_err(|_| format!("{raw} is not a number"))?;
    if value > 0 && value % 4 == 0 {
        Ok(value)
    } else {
        Err("width and height must be positive multiples of 4".to_owned())
    }
}

/// Sets up the ISP for the requested snapshot size.
///
/// The sub-channel scaler is capped at 640x480 in hardware, so anything larger
/// has to come from the main channel. That is the branch the C version wrote
/// as "janky but works".
fn capture_init(args: &Args) -> anyka::Result<(VideoInput, VideoChannel)> {
    let use_sub = args.width <= 640 && args.height <= 480;
    let channel = if use_sub {
        VideoChannel::Sub
    } else {
        VideoChannel::Main
    };

    let isp_config = VideoInput::match_sensor_auto()?;
    tracing::info!(config = %isp_config.display(), "sensor matched");
    let vi = VideoInput::open(VideoDevice::Dev0)?;
    let sensor = vi.sensor_resolution()?;
    tracing::info!(
        width = sensor.width,
        height = sensor.height,
        "sensor resolution"
    );

    let (sub_w, sub_h) = if use_sub {
        (args.width, args.height)
    } else {
        (640, 480)
    };
    let (main_w, main_h) = if use_sub {
        (640, 480)
    } else {
        (args.width, args.height)
    };

    vi.set_channel_attr(ChannelAttr {
        crop: Crop {
            left: 0,
            top: 0,
            width: sensor.width,
            height: sensor.height,
        },
        // The main channel's `max_*` bounds the sub-channel scaler, not the
        // main output — a quirk of this SDK, preserved from the C version.
        main: Resolution::with_max(main_w, main_h, sub_w, sub_h),
        sub: Resolution::with_max(sub_w, sub_h, sub_w, sub_h),
    })?;
    tracing::info!(
        ?channel,
        width = args.width,
        height = args.height,
        "serving channel"
    );

    vi.capture_on()?;
    // The lens is mounted upside down on these cameras.
    vi.set_flip_mirror(true, true)?;
    Ok((vi, channel))
}

fn jpeg_encoder(args: &Args) -> anyka::Result<Encoder> {
    Encoder::open(EncodeParam {
        width: args.width as u32,
        height: args.height as u32,
        min_qp: 20,
        max_qp: 51,
        fps: 10,
        gop_len: 20,
        kbps: 500,
        profile: Profile::Main,
        channel: VideoChannel::Sub,
        group: EncodeGroup::Picture,
        bitrate_mode: BitrateMode::Cbr,
        output: OutputType::Mjpeg,
    })
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let args = Args::parse();
    let _telemetry = telemetry::install("jpeg_snapshot")?;
    describe_counter!(
        "kamoder_snapshot_requests_total",
        "HTTP snapshot requests by outcome"
    );
    describe_counter!("kamoder_snapshot_bytes_total", "JPEG bytes served");
    describe_counter!("kamoder_snapshot_frames_total", "JPEG frames encoded");
    describe_histogram!(
        "kamoder_snapshot_encode_seconds",
        "Time spent in the hardware JPEG encoder"
    );

    shutdown::install();

    let (vi, channel) = capture_init(&args)?;
    let encoder = jpeg_encoder(&args)?;
    let snapshot = SnapshotBuffer::new();
    http::serve(args.port, snapshot.clone())?;

    let mut jpeg = Vec::new();
    while shutdown::running() {
        if !snapshot.wants_frame() {
            // Nobody is asking for images; leave the encoder idle.
            time::sleep(CAPTURE_INTERVAL);
            continue;
        }

        let Ok(frame) = vi.get_frame() else {
            time::sleep(Duration::from_millis(10));
            continue;
        };
        jpeg.clear();
        let started = Instant::now();
        let encoded = encoder.encode_frame(frame.channel(channel), &mut jpeg);
        histogram!("kamoder_snapshot_encode_seconds").record(started.elapsed().as_secs_f64());
        // Release the ISP buffer before touching the mutex: there are only a
        // few DMA buffers and holding one across a lock stalls capture.
        drop(frame);

        match encoded {
            Ok(0) => {}
            Ok(_) => {
                snapshot.store(&jpeg);
                snapshot.frame_encoded();
                counter!("kamoder_snapshot_frames_total").increment(1);
            }
            Err(error) => tracing::warn!(%error, "encode failed"),
        }
        time::sleep(CAPTURE_INTERVAL);
    }

    tracing::info!("stopping");
    vi.capture_off()?;
    Ok(())
}
