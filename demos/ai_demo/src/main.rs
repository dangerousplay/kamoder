//! Audio input demo — Rust port of `cross-compile/ai_demo/ak_ai_demo.c`.
//!
//! Captures raw 16-bit PCM from the microphone into
//! `<dir>/<yyyyMMdd-HHmmss>.pcm`.
//!
//! Preconditions, unchanged from the C original: the SD card is mounted and
//! the vendor audio process (`anyka_ipc`) is stopped, because the ADC cannot
//! be opened twice.
//!
//! Play the result with:
//! `ffplay -f s16le -ar 8000 -ch_layout mono 20260920-101500.pcm`

use anyka::ai::{AudioIn, AudioSource, PcmParam, DEFAULT_FRAME_INTERVAL_MS};
use anyka::{shutdown, time};
use clap::Parser;
use eyre::{Context, Result};
use metrics::{counter, describe_counter};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_SAVE_DIR: &str = "/mnt/";
const DEFAULT_SECONDS: u64 = 20;
const SAMPLE_RATE: u32 = 8000;

/// Records PCM from the microphone to a file.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Directory the recording is written to.
    #[arg(short = 'd', default_value = DEFAULT_SAVE_DIR)]
    save_dir: PathBuf,
    /// Recording length, in seconds.
    #[arg(short = 't', default_value_t = DEFAULT_SECONDS, value_parser = clap::value_parser!(u64))]
    seconds: u64,
    /// Input volume, 0 (mute) to 12.
    #[arg(short = 'v', default_value_t = 7, value_parser = clap::value_parser!(i32).range(0..=12))]
    volume: i32,
}

impl Args {
    fn duration(&self) -> Duration {
        Duration::from_secs(self.seconds)
    }
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let args = Args::parse();
    let _telemetry = telemetry::install("ai_demo")?;
    describe_counter!("kamoder_audio_frames_total", "PCM frames read from the ADC");
    describe_counter!("kamoder_audio_bytes_total", "PCM bytes written to disk");

    shutdown::install();
    tracing::info!(sdk_version = %anyka::version::audio_in(), "ai demo");

    // Step 1: open the ADC. 16-bit mono is the only format the hardware does.
    let mut ai = AudioIn::open(PcmParam::mono(SAMPLE_RATE))?;

    // Step 2: configure before capture starts — all of these are rejected once
    // the ADC is running.
    ai.set_frame_interval(DEFAULT_FRAME_INTERVAL_MS)?;
    ai.set_source(AudioSource::Mic)?;
    ai.set_volume(args.volume)?;
    // Noise reduction, AGC and the resampler are all off in the C demo; they
    // only work at 8 kHz and colour the captured signal.
    ai.set_nr_agc(false)?;
    ai.set_resample(false)?;
    ai.clear_frame_buffer()?;

    // Step 3: start capturing.
    ai.start_capture()?;

    // Step 4: drain frames into the file.
    let path = time::timestamped_path(&args.save_dir, "pcm")?;
    let file = std::fs::File::create(&path)
        .wrap_err_with(|| format!("cannot create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    tracing::info!(path = %path.display(), "capture start");

    let mut start_ts = None;
    let mut written = 0usize;
    while shutdown::running() {
        // Non-blocking poll, matching the C demo's `ms = 0` plus a short sleep.
        let Ok(frame) = ai.get_frame(Some(Duration::ZERO)) else {
            time::sleep(Duration::from_millis(10));
            continue;
        };

        writer.write_all(frame.data())?;
        written += frame.data().len();
        counter!("kamoder_audio_frames_total").increment(1);
        counter!("kamoder_audio_bytes_total").increment(frame.data().len() as u64);

        let ts = frame.timestamp_ms();
        let start = *start_ts.get_or_insert(ts);
        // `frame` releases the SDK buffer here, before the next iteration asks
        // for another one.
        drop(frame);

        if ts.saturating_sub(start) >= args.duration().as_millis() as u64 {
            break;
        }
    }

    writer.flush()?;
    tracing::info!(bytes = written, path = %path.display(), "capture finished");

    // Step 5/6: stop and close. Dropping `ai` would do both, but the explicit
    // stop surfaces an error instead of swallowing it.
    ai.stop_capture()?;
    Ok(())
}
