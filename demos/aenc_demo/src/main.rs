//! Audio encoder demo — Rust port of `cross-compile/aenc_demo/ak_aenc_demo.c`.
//!
//! Captures from the microphone or line-in, encodes with the hardware/DSP
//! codec and writes `<dir>/<yyyyMMdd-HHmmss>.<ext>`.
//!
//! The vendor audio process must be stopped first, since it holds the ADC.

use anyka::aenc::{AudioEncodeParam, AudioEncoder, AudioType};
use anyka::ai::{AudioIn, AudioSource, PcmParam, MAX_VOLUME};
use anyka::{shutdown, time};
use clap::Parser;
use eyre::{Context, Result};
use metrics::{counter, describe_counter};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Duration;

/// Records from the microphone or line-in and encodes it to a file.
///
/// Positional, in the vendor demo's order, so existing scripts keep working:
///   aenc_demo 8000 1 mp3 /mnt/ 10 7 mic
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Sample rate in Hz, 8000-48000.
    #[arg(value_parser = clap::value_parser!(u32).range(8000..=48000))]
    sample_rate: u32,
    /// Channel count.
    channels: u32,
    /// mp3 | amr | aac | g711a | g711u | pcm
    #[arg(value_parser = parse_codec)]
    codec: AudioType,
    /// Directory the file is written to.
    save_dir: PathBuf,
    /// Recording length in seconds (at least 1).
    #[arg(value_parser = clap::value_parser!(u64).range(1..))]
    seconds: u64,
    /// Input volume, 0 to MAX_VOLUME.
    #[arg(value_parser = parse_volume)]
    volume: i32,
    /// mic | linein
    #[arg(value_parser = parse_source)]
    source: AudioSource,
}

impl Args {
    fn duration(&self) -> Duration {
        Duration::from_secs(self.seconds)
    }
}

fn parse_codec(raw: &str) -> Result<AudioType, String> {
    AudioType::parse(raw).ok_or_else(|| format!("unsupported audio encode type {raw}"))
}

fn parse_source(raw: &str) -> Result<AudioSource, String> {
    AudioSource::parse(raw).ok_or_else(|| "source must be mic or linein".to_owned())
}

fn parse_volume(raw: &str) -> Result<i32, String> {
    let volume: i32 = raw.parse().map_err(|_| format!("{raw} is not a number"))?;
    if (0..=MAX_VOLUME).contains(&volume) {
        Ok(volume)
    } else {
        Err(format!("volume must be in 0..={MAX_VOLUME}"))
    }
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let args = Args::parse();
    let _telemetry = telemetry::install("aenc_demo")?;
    describe_counter!("kamoder_aenc_chunks_total", "Encoded audio chunks drained");
    describe_counter!("kamoder_aenc_bytes_total", "Encoded audio bytes written");

    shutdown::install();
    tracing::info!(sdk_version = %anyka::version::audio_encode(), "aenc demo");

    // Open the output first, so a bad path fails before the ADC is touched.
    let path = time::timestamped_path(&args.save_dir, args.codec.extension())?;
    let file = std::fs::File::create(&path)
        .wrap_err_with(|| format!("cannot create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    // AMR needs a magic header to be playable as a standalone file.
    writer.write_all(args.codec.file_header())?;
    tracing::info!(path = %path.display(), codec = ?args.codec, "recording");

    // Audio input.
    let mut ai = AudioIn::open(PcmParam {
        sample_rate: args.sample_rate,
        channels: args.channels,
    })?;
    ai.set_aec(false)?;
    ai.set_nr_agc(false)?;
    ai.set_resample(false)?;
    ai.set_volume(args.volume)?;
    ai.set_source(args.source)?;
    ai.clear_frame_buffer()?;

    // Each codec consumes a fixed number of samples per frame, so the ADC has
    // to hand over exactly that much audio or the encoder stalls.
    let interval = args.codec.frame_interval_ms(args.sample_rate);
    ai.set_frame_interval(interval)?;
    tracing::info!(frame_interval_ms = interval, "audio input configured");

    ai.start_capture()?;

    // Audio encoder, bound to the input.
    let encoder = AudioEncoder::open(AudioEncodeParam {
        codec: args.codec,
        sample_rate: args.sample_rate,
        channels: args.channels,
    })?;
    let stream = encoder.request_stream(&ai)?;

    let started = time::ostime_ms();
    let mut written = 0usize;
    while shutdown::running() {
        if time::ostime_ms().saturating_sub(started) >= args.duration().as_millis() as u64 {
            tracing::info!("recording time is up");
            break;
        }
        // `drain` releases every SDK list node before returning, so nothing
        // leaks even if the write below fails.
        for chunk in stream.drain() {
            writer.write_all(&chunk.data)?;
            written += chunk.data.len();
            counter!("kamoder_aenc_chunks_total").increment(1);
            counter!("kamoder_aenc_bytes_total").increment(chunk.data.len() as u64);
        }
        time::sleep(Duration::from_millis(10));
    }

    writer.flush()?;
    tracing::info!(bytes = written, path = %path.display(), "recording finished");

    // Explicit teardown order: stream, encoder, capture, input. Dropping in
    // reverse declaration order does the same thing, but the ADC stop is
    // spelled out so a failure is reported rather than ignored.
    drop(stream);
    drop(encoder);
    ai.stop_capture()?;
    Ok(())
}
