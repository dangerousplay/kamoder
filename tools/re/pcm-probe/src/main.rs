//! RE aid for the audio spike (T0): drives `ak_pcm.ko` through
//! `anyka_av100::pcm` and prints what the driver does.
//!
//! ```text
//! pcm-probe info
//! pcm-probe capture --rate 8000 --period 512 --secs 5 --out /tmp/c.wav
//! pcm-probe play --rate 16000 --tone 1000 --secs 2 --source 4 --gain 3 --speaker on
//! pcm-probe ioctl --node p --cmd 0x50e3
//! ```
//!
//! Stop the vendor `ipc` first: capture admits one opener.

use anyka_av100::pcm::{self, ioc, PcmDevice, PcmParams};
use clap::{Parser, Subcommand, ValueEnum};
use eyre::{bail, Result, WrapErr};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(about = "Probe the AV100 ak_pcm.ko audio nodes")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Stream parameters shared by capture and play.
#[derive(clap::Args)]
struct Stream {
    /// Sample rate, Hz.
    #[arg(long, default_value_t = 16_000)]
    rate: u32,
    #[arg(long, default_value_t = 1)]
    channels: u32,
    /// Bytes per driver period (capture needs >= 512).
    #[arg(long, default_value_t = 2048)]
    period: u32,
    /// Periods in the driver ring.
    #[arg(long, default_value_t = 8)]
    periods: u32,
    #[arg(long, default_value_t = 5)]
    secs: u32,
    /// SET_SOURCE value (playback: vendor uses 4). Unset: leave as is.
    #[arg(long)]
    source: Option<u32>,
    /// SET_GAIN value (vendor playback: 3). Unset: leave as is.
    #[arg(long)]
    gain: Option<u32>,
}

impl Stream {
    fn params(&self) -> PcmParams {
        PcmParams::new(self.rate, self.channels, 16, self.period, self.periods)
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Node {
    P,
    C,
    L,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum Speaker {
    On,
    Off,
    Skip,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open each node and print GET_PARS, source, gain, rate.
    Info,
    /// SET_PARS, START, read; per-second stats; optional WAV.
    Capture {
        #[command(flatten)]
        stream: Stream,
        #[arg(long, default_value = pcm::CAPTURE_NODE)]
        node: PathBuf,
        /// Write the samples to this WAV file.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// SET_PARS, START, play a sine tone.
    Play {
        #[command(flatten)]
        stream: Stream,
        #[arg(long, default_value = pcm::PLAYBACK_NODE)]
        node: PathBuf,
        /// Tone frequency, Hz.
        #[arg(long, default_value_t = 1000)]
        tone: u32,
        /// Peak level, percent of full scale.
        #[arg(long, default_value_t = 50)]
        level: u32,
        /// SET_SPEAKER (0x40045081) on before / off after, or not at all.
        #[arg(long, value_enum, default_value_t = Speaker::Skip)]
        speaker: Speaker,
    },
    /// One raw ioctl; the argument buffer is sized from the command.
    Ioctl {
        #[arg(long, value_enum)]
        node: Node,
        /// Command number, e.g. 0x800850e6.
        #[arg(long, value_parser = parse_u32)]
        cmd: u32,
        /// First argument word for write commands.
        #[arg(long, value_parser = parse_u32)]
        arg: Option<u32>,
    },
}

fn parse_u32(s: &str) -> Result<u32, String> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => s.parse(),
    }
    .map_err(|e| e.to_string())
}

fn show<T: std::fmt::Debug>(what: &str, r: Result<T, pcm::Error>) {
    match r {
        Ok(v) => println!("  {what:<10} {v:?}"),
        Err(e) => println!("  {what:<10} error: {e}"),
    }
}

type Open = fn(&Path) -> Result<PcmDevice, pcm::Error>;

fn info() {
    let nodes: [(&str, Open); 3] = [
        (pcm::CAPTURE_NODE, PcmDevice::open_capture),
        (pcm::PLAYBACK_NODE, PcmDevice::open_playback),
        (pcm::LOOPBACK_NODE, PcmDevice::open_loopback),
    ];
    for (path, open) in nodes {
        println!("{path}:");
        match open(Path::new(path)) {
            Ok(dev) => {
                show("params", dev.params());
                show("source", dev.source());
                show("gain", dev.gain());
                show("rate", dev.rate());
            }
            Err(e) => println!("  {e}"),
        }
    }
}

/// Applies source and gain, as the vendor does after START.
fn setup(dev: &PcmDevice, s: &Stream) -> Result<()> {
    let p = s.params();
    dev.set_params(&p)?;
    println!("SET_PARS {p:?}");
    dev.start()?;
    show("rate", dev.rate());
    show("params", dev.params());
    if let Some(v) = s.source {
        dev.set_source(v)?;
    }
    if let Some(v) = s.gain {
        dev.set_gain(v)?;
    }
    show("source", dev.source());
    show("gain", dev.gain());
    Ok(())
}

fn capture(s: &Stream, node: &Path, out: Option<&Path>) -> Result<()> {
    let dev = PcmDevice::open_capture(node)?;
    setup(&dev, s)?;
    let bps = s.params().bytes_per_second() as usize;
    let mut buf = vec![0u8; s.period as usize];
    let mut all = Vec::with_capacity(bps * s.secs as usize);
    let begin = Instant::now();
    for sec in 0..s.secs {
        let (mut sum, mut n, mut zeros, mut reads) = (0f64, 0usize, 0usize, 0usize);
        let (mut min_b, mut max_b, mut max_lat) = (usize::MAX, 0usize, Duration::ZERO);
        let mut got = 0;
        while got < bps {
            let t = Instant::now();
            let len = dev.read(&mut buf)?;
            let lat = t.elapsed();
            if len == 0 {
                bail!("read returned 0 after {:?}", begin.elapsed());
            }
            reads += 1;
            got += len;
            min_b = min_b.min(len);
            max_b = max_b.max(len);
            max_lat = max_lat.max(lat);
            for c in buf[..len].as_chunks::<2>().0 {
                let v = i16::from_le_bytes(*c);
                sum += f64::from(v) * f64::from(v);
                zeros += usize::from(v == 0);
                n += 1;
            }
            all.extend_from_slice(&buf[..len]);
        }
        let rms = (sum / n.max(1) as f64).sqrt();
        let dbfs = 20.0 * (rms.max(1.0) / 32768.0).log10();
        println!(
            "s{sec}: {got} B in {reads} reads ({min_b}..{max_b} B), max read {max_lat:?}, \
             rms {rms:.0} ({dbfs:.1} dBFS), zeros {:.1}%, t={:.2}s",
            100.0 * zeros as f64 / n.max(1) as f64,
            begin.elapsed().as_secs_f64()
        );
    }
    if let Some(out) = out {
        write_wav(out, s.rate, s.channels as u16, &all)?;
        println!("wrote {} ({} B of samples)", out.display(), all.len());
    }
    Ok(())
}

fn write_wav(path: &Path, rate: u32, channels: u16, data: &[u8]) -> Result<()> {
    let mut f = std::fs::File::create(path).wrap_err_with(|| path.display().to_string())?;
    let len = data.len() as u32;
    let block = channels * 2;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + len).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    h.extend_from_slice(&block.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&len.to_le_bytes());
    f.write_all(&h)?;
    f.write_all(data)?;
    Ok(())
}

fn play(s: &Stream, node: &Path, tone: u32, level: u32, speaker: Speaker) -> Result<()> {
    let dev = PcmDevice::open_playback(node)?;
    setup(&dev, s)?;
    if speaker != Speaker::Skip {
        dev.set_speaker(speaker == Speaker::On)?;
        println!("SET_SPEAKER {}", speaker == Speaker::On);
    }
    let amp = 32767.0 * f64::from(level.min(100)) / 100.0;
    let frames = (s.rate * s.secs) as usize;
    let step = 2.0 * std::f64::consts::PI * f64::from(tone) / f64::from(s.rate);
    let mut pcm = Vec::with_capacity(frames * s.channels as usize * 2);
    for i in 0..frames {
        let v = ((i as f64 * step).sin() * amp) as i16;
        for _ in 0..s.channels {
            pcm.extend_from_slice(&v.to_le_bytes());
        }
    }
    let begin = Instant::now();
    let (mut writes, mut max_lat) = (0usize, Duration::ZERO);
    let mut rest = &pcm[..];
    while !rest.is_empty() {
        let chunk = &rest[..rest.len().min(s.period as usize)];
        let t = Instant::now();
        let n = dev.write(chunk)?;
        max_lat = max_lat.max(t.elapsed());
        writes += 1;
        if n == 0 {
            bail!("write returned 0 after {writes} writes");
        }
        rest = &rest[n..];
    }
    println!(
        "wrote {} B in {writes} writes over {:?}, max write {max_lat:?}",
        pcm.len(),
        begin.elapsed()
    );
    // Let the driver ring drain before closing.
    let ring = u64::from(s.period * s.periods);
    let bps = u64::from(s.params().bytes_per_second().max(1));
    std::thread::sleep(Duration::from_millis(ring * 1000 / bps + 100));
    if speaker == Speaker::On {
        dev.set_speaker(false)?;
        println!("SET_SPEAKER false");
    }
    Ok(())
}

fn raw(node: Node, cmd: u32, arg: Option<u32>) -> Result<()> {
    let dev = match node {
        Node::P => PcmDevice::open_playback(Path::new(pcm::PLAYBACK_NODE)),
        Node::C => PcmDevice::open_capture(Path::new(pcm::CAPTURE_NODE)),
        Node::L => PcmDevice::open_loopback(Path::new(pcm::LOOPBACK_NODE)),
    }?;
    let size = ioc::size(cmd);
    println!(
        "{cmd:#010x}: dir {} type {:?} nr {:#04x} size {size}",
        ioc::dir(cmd),
        ioc::kind(cmd) as char,
        ioc::nr(cmd)
    );
    if size == 0 {
        if arg.is_some() {
            bail!("{cmd:#x} encodes no argument; --arg is not passed");
        }
        dev.ioctl_none(cmd)?;
        println!("ok");
        return Ok(());
    }
    let mut words = vec![0u32; size.div_ceil(4)];
    if let Some(a) = arg {
        words[0] = a;
    }
    dev.ioctl_words(cmd, &mut words, "raw ioctl")?;
    let hex: Vec<String> = words.iter().map(|w| format!("{w:#010x}")).collect();
    println!("ok: [{}]", hex.join(", "));
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Info => {
            info();
            Ok(())
        }
        Cmd::Capture { stream, node, out } => capture(&stream, &node, out.as_deref()),
        Cmd::Play {
            stream,
            node,
            tone,
            level,
            speaker,
        } => play(&stream, &node, tone, level, speaker),
        Cmd::Ioctl { node, cmd, arg } => raw(node, cmd, arg),
    }
}
