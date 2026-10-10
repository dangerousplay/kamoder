//! Video encoder demo — Rust port of
//! `cross-compile/venc_demo/ak_venc_demo.c`.
//!
//! Encodes `frame_count` frames of H.264 from the main channel into
//! `/mnt/video_encode/<timestamp>_<group>.str`, then grabs a few single-frame
//! JPEG stills.
//!
//! The C original ran the stream loop on an `ak_thread` and tore the encoder
//! down from inside that thread, which is why it needed a global handle array
//! and a `work_nr` counter. Here the stream handle borrows the encoder and the
//! video input, so the same teardown falls out of scope exit.

use anyka::venc::{BitrateMode, EncodeGroup, EncodeParam, Encoder, OutputType, Profile};
use anyka::vi::{ChannelAttr, Crop, Resolution, VideoChannel, VideoDevice, VideoInput};
use anyka::{shutdown, time};
use clap::Parser;
use eyre::{Context, Result};
use metrics::{counter, describe_counter};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SAVE_DIR: &str = "/mnt/video_encode";
const JPEG_STILLS: usize = 20;

/// The resolution table from the C demo. Indices 6-11 are valid for the main
/// channel, 0-5 for the sub channel, because the sub-channel scaler tops out
/// at 640x480. `RESOLUTION_HELP` repeats it for `--help`.
const RESOLUTIONS: [(u32, u32, &str); 12] = [
    (176, 144, "QCIF"),
    (320, 180, "180P"),
    (320, 240, "QVGA"),
    (352, 288, "CIF"),
    (640, 360, "360P"),
    (640, 480, "VGA"),
    (720, 576, "PAL"),
    (720, 480, "NTSC"),
    (1280, 720, "720P"),
    (1280, 800, "WXGA"),
    (1280, 960, "960P"),
    (1920, 1080, "1080P"),
];

/// Resolution indices, shown under `--help`. Must match `RESOLUTIONS`.
const RESOLUTION_HELP: &str = "\
Resolution indices:
  sub_res  00 QCIF  176x144     main_res 06 PAL    720x576
  sub_res  01 180P  320x180     main_res 07 NTSC   720x480
  sub_res  02 QVGA  320x240     main_res 08 720P   1280x720
  sub_res  03 CIF   352x288     main_res 09 WXGA   1280x800
  sub_res  04 360P  640x360     main_res 10 960P   1280x960
  sub_res  05 VGA   640x480     main_res 11 1080P  1920x1080

e.g. venc_demo 1000 10 5   captures 1000 frames into /mnt/video_encode";

/// Encodes main and sub channel to files (H.264/H.265 plus JPEG stills).
#[derive(Parser)]
#[command(version, about, long_about = None, after_help = RESOLUTION_HELP)]
struct Args {
    /// Frames to capture.
    frame_count: usize,
    /// Main channel resolution index, 6-11.
    #[arg(value_parser = clap::value_parser!(u8).range(6..=11))]
    main: u8,
    /// Sub channel resolution index, 0-5.
    #[arg(value_parser = clap::value_parser!(u8).range(0..=5))]
    sub: u8,
}

/// Channel geometry for the two selected resolution indices.
fn channel_attr(main: usize, sub: usize) -> ChannelAttr {
    let (main_w, main_h, _) = RESOLUTIONS[main];
    let (sub_w, sub_h, _) = RESOLUTIONS[sub];
    ChannelAttr {
        crop: Crop {
            left: 0,
            top: 0,
            width: main_w as i32,
            height: main_h as i32,
        },
        // The main channel's `max_*` actually bounds the sub-channel scaler,
        // which is why it carries the *sub* resolution here. Keeping the C
        // demo's values verbatim: changing them silently breaks capture.
        main: Resolution::with_max(main_w as i32, main_h as i32, sub_w as i32, sub_h as i32),
        sub: Resolution::with_max(sub_w as i32, sub_h as i32, 640, 480),
    }
}

fn stream_encode_param(main: usize) -> EncodeParam {
    let (width, height, _) = RESOLUTIONS[main];
    EncodeParam {
        width,
        height,
        min_qp: 20,
        max_qp: 51,
        fps: 10,
        gop_len: 20,
        kbps: 500,
        profile: Profile::Main,
        channel: VideoChannel::Main,
        group: EncodeGroup::MainNet,
        bitrate_mode: BitrateMode::Cbr,
        output: OutputType::H264,
    }
}

fn picture_encode_param(sub: usize) -> EncodeParam {
    let (width, height, _) = RESOLUTIONS[sub];
    EncodeParam {
        width,
        height,
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
    }
}

fn output_path(dir: &Path, suffix: &str, ext: &str) -> anyka::Result<PathBuf> {
    Ok(dir.join(format!(
        "{}_{suffix}.{ext}",
        time::Date::now()?.to_filename_stamp()
    )))
}

/// Encodes `frame_count` frames into one file.
fn record_stream(vi: &VideoInput, dir: &Path, main: usize, frame_count: usize) -> Result<()> {
    let encoder = Encoder::open(stream_encode_param(main))?;
    let stream = encoder.request_stream(vi)?;

    let path = output_path(dir, "mainnet", "str")?;
    let file = std::fs::File::create(&path)
        .wrap_err_with(|| format!("cannot create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    tracing::info!(path = %path.display(), "writing H.264 stream");

    let mut remaining = frame_count;
    while remaining > 0 && shutdown::running() {
        let Ok(chunk) = stream.get_stream() else {
            // No frame queued yet; the SDK reports that through the error path.
            time::sleep(Duration::from_millis(10));
            continue;
        };
        writer.write_all(chunk.data())?;
        counter!("kamoder_venc_frames_total").increment(1);
        counter!("kamoder_venc_bytes_total").increment(chunk.data().len() as u64);
        if chunk.is_keyframe() {
            counter!("kamoder_venc_keyframes_total").increment(1);
        }
        if remaining.is_multiple_of(25) {
            tracing::debug!(
                bytes = chunk.data().len(),
                keyframe = chunk.is_keyframe(),
                remaining,
                "encoded frame"
            );
        }
        // Releases the encoder's buffer before asking for the next one.
        drop(chunk);
        remaining -= 1;
    }

    writer.flush()?;
    // `stream` then `encoder` drop here, in that order: cancel then close.
    Ok(())
}

/// Grabs single-frame JPEG stills from the sub channel.
fn capture_stills(vi: &VideoInput, dir: &Path, sub: usize, count: usize) -> Result<()> {
    // One encoder for all stills. The C demo opened and closed one per picture,
    // which is slower and leaks the encoder handle on any early return.
    let encoder = Encoder::open(picture_encode_param(sub))?;
    let mut jpeg = Vec::new();

    for index in 0..count {
        if !shutdown::running() {
            break;
        }
        let Ok(frame) = vi.get_frame() else {
            time::sleep(Duration::from_millis(20));
            continue;
        };
        jpeg.clear();
        let written = encoder.encode_frame(frame.sub(), &mut jpeg)?;
        // Release the ISP buffer as early as possible: there are only a few.
        drop(frame);

        if written == 0 {
            continue;
        }
        let path = output_path(dir, &format!("{index:03}"), "jpg")?;
        std::fs::write(&path, &jpeg)
            .wrap_err_with(|| format!("cannot write {}", path.display()))?;
        counter!("kamoder_venc_stills_total").increment(1);
        tracing::info!(path = %path.display(), bytes = written, "saved still");
        time::sleep(Duration::from_secs(1));
    }
    Ok(())
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let args = Args::parse();
    let _telemetry = telemetry::install("venc_demo")?;
    describe_counter!("kamoder_venc_frames_total", "Encoded video frames");
    describe_counter!("kamoder_venc_bytes_total", "Encoded video bytes written");
    describe_counter!("kamoder_venc_keyframes_total", "Encoded keyframes");
    describe_counter!("kamoder_venc_stills_total", "JPEG stills written");

    shutdown::install();
    tracing::info!(frames = args.frame_count, "video encode demo start");

    let dir = PathBuf::from(SAVE_DIR);
    std::fs::create_dir_all(&dir).wrap_err_with(|| format!("cannot create {SAVE_DIR}"))?;

    let isp_config = VideoInput::match_sensor_auto()?;
    tracing::info!(config = %isp_config.display(), "sensor matched");
    let vi = VideoInput::open(VideoDevice::Dev0)?;
    if let Ok(res) = vi.sensor_resolution() {
        tracing::info!(width = res.width, height = res.height, "sensor resolution");
    }
    vi.set_channel_attr(channel_attr(usize::from(args.main), usize::from(args.sub)))?;
    vi.set_fps(25)?;
    tracing::info!(fps = vi.fps(), "capture configured");

    vi.capture_on()?;
    // The lens is mounted upside down on these cameras.
    vi.set_flip_mirror(true, true)?;

    record_stream(&vi, &dir, usize::from(args.main), args.frame_count)?;
    capture_stills(&vi, &dir, usize::from(args.sub), JPEG_STILLS)?;

    tracing::info!("closing video input");
    vi.capture_off()?;
    Ok(())
}
