//! The audio service: owns the codec, serves `/tmp/kamoder-audio.sock`.
//!
//! ```text
//! audio --state etc/audio.json
//! ```
//!
//! - **Only on boards with audio**: without `[audio]` in the board profile
//!   (`board::current()`, or `--board-file`) it logs and exits 0, so a
//!   supervisor entry with `restart = "on-failure"` stays stopped.
//! - **Backend** from `[audio] backend`: `ak-pcm` (Anyka `ak_pcm.ko`, needs
//!   feature `ak-pcm`, on by default) or `fake` (a quiet 440 Hz tone and a
//!   speaker that plays nowhere; development and the itest VM). Any other is
//!   an error exit.
//! - **Device errors never exit**: they show as `error` in `status`, are
//!   counted and retried with backoff (AUD-05). A socket that cannot be
//!   bound is an error exit.
//! - **Amp** (`--amp`, D-A6): `driver` (ioctl on the playback node,
//!   default), `lights` (`set_mute` on the lights socket), or `none`.
//! - **Exit status**: SIGTERM/SIGINT switch the amp off and exit 0.

// Linked for its side effect: `os` supplies the `pthread_setname_np`
// shim that the crosstool's uClibc 0.9.33 does not export but Rust's thread
// startup references.
extern crate os;

use audio::fake::{FakeBackend, Signal};
use audio::{server, Amp, Backend, Runtime, Settings};
use clap::{Parser, ValueEnum};
use eyre::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

/// `[audio] backend` of the Anyka codec driver.
const AK_PCM: &str = "ak-pcm";
/// `[audio] backend` without hardware.
const FAKE: &str = "fake";

/// Who switches the speaker amplifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AmpKind {
    /// The codec driver's speaker ioctl. Not on the AV100: its driver
    /// ignores the DT polarity and drives the line high (= muted) for "on".
    Driver,
    /// The lights service (`set_mute`), owner of the amp GPIO.
    Lights,
    /// No switchable amplifier.
    None,
}

/// Owns the camera's microphone and speaker.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Board profile (TOML) to use instead of the detected board; for
    /// development and tests.
    #[arg(long)]
    board_file: Option<PathBuf>,
    /// Saved gains and volumes (JSON); empty keeps changes in memory only.
    #[arg(long, default_value = "etc/audio.json")]
    state: String,
    /// Unix socket for clients (rtsp, web, onvif).
    #[arg(long, default_value = audio_proto::DEFAULT_SOCKET)]
    socket: PathBuf,
    /// Who switches the speaker amplifier.
    #[arg(long, value_enum, default_value_t = AmpKind::Lights)]
    amp: AmpKind,
    /// The lights service's socket, for `--amp lights`.
    #[arg(long, default_value = lights_proto::DEFAULT_SOCKET)]
    lights_socket: PathBuf,
    /// Driver period in bytes (S16 mono; 512 is 32 ms at 8 kHz).
    #[arg(long, default_value_t = audio::runtime::DEFAULT_PERIOD_BYTES)]
    period_bytes: u32,
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let cli = Cli::parse();
    let _telemetry = telemetry::install("audio")?;
    audio::describe_metrics();
    run(cli)
}

fn backend(name: &str) -> Result<Arc<dyn Backend>> {
    Ok(match name {
        #[cfg(feature = "ak-pcm")]
        AK_PCM => Arc::new(audio::ak::AkPcmBackend::new()),
        FAKE => Arc::new(FakeBackend::new(Signal::Sine {
            hz: 440.0,
            amplitude: 2000.0,
        })),
        other => {
            let known = if cfg!(feature = "ak-pcm") {
                format!("{AK_PCM}, {FAKE}")
            } else {
                FAKE.to_owned()
            };
            bail!("audio backend {other:?} is not supported by this build (known: {known})")
        }
    })
}

fn run(cli: Cli) -> Result<()> {
    let (name, profile) = match &cli.board_file {
        Some(path) => {
            let profile = board::load(path)?;
            (format!("{} ({})", profile.name, path.display()), profile)
        }
        None => {
            let resolved = board::current().wrap_err("cannot determine the board")?;
            (
                format!("{} ({})", resolved.name, resolved.reason),
                resolved.profile,
            )
        }
    };
    let Some(audio) = profile.audio else {
        tracing::info!(board = %name, "the board has no [audio]; nothing to do");
        return Ok(());
    };
    let backend = backend(&audio.backend).wrap_err_with(|| format!("board {name}"))?;
    let settings = Settings {
        period_bytes: cli.period_bytes,
        ..Settings::default()
    };
    let store = (!cli.state.is_empty()).then(|| PathBuf::from(&cli.state));
    tracing::info!(
        board = %name,
        backend = %audio.backend,
        rate = audio.rate,
        inputs = audio.inputs.len(),
        outputs = audio.outputs.len(),
        amp = ?cli.amp,
        period_bytes = cli.period_bytes,
        "audio starting"
    );

    // One thread for the sockets; capture and playback have their own.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let amp = match cli.amp {
            AmpKind::Driver => Amp::Driver,
            AmpKind::Lights => Amp::Lights(lights_proto::Client::new(&cli.lights_socket)),
            AmpKind::None => Amp::None,
        };
        let audio = Runtime::start(&audio, backend, amp, settings, store)?;
        // Held for the lifetime of the service; dropping it removes the file.
        let _socket = server::serve(Path::new(&cli.socket), audio.clone())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        tracing::info!("shutting down");
        audio.shutdown().await;
        Ok::<_, eyre::Report>(())
    })
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    /// clap validates the argument definitions lazily, at parse time; this
    /// catches a clash on the host rather than on the camera.
    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_match_the_design() {
        let cli = Cli::parse_from(["audio"]);
        assert_eq!(cli.board_file, None);
        assert_eq!(cli.state, "etc/audio.json");
        assert_eq!(cli.socket, Path::new("/tmp/kamoder-audio.sock"));
        assert_eq!(cli.amp, AmpKind::Lights);
        assert_eq!(cli.lights_socket, Path::new("/tmp/kamoder-lights.sock"));
        assert_eq!(cli.period_bytes, 512);
    }

    #[test]
    fn amp_choices() {
        for (value, want) in [
            ("driver", AmpKind::Driver),
            ("lights", AmpKind::Lights),
            ("none", AmpKind::None),
        ] {
            assert_eq!(Cli::parse_from(["audio", "--amp", value]).amp, want);
        }
        assert!(Cli::try_parse_from(["audio", "--amp", "gpio"]).is_err());
    }

    #[test]
    fn backends() {
        assert!(backend(FAKE).is_ok());
        assert_eq!(backend(AK_PCM).is_ok(), cfg!(feature = "ak-pcm"));
        let err = backend("alsa").err().unwrap().to_string();
        assert!(err.contains("alsa") && err.contains(FAKE), "{err}");
    }

    #[test]
    fn boards_without_audio_exit_cleanly() {
        let dir = std::env::temp_dir().join(format!("kamoder-audio-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let generic = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../boards/generic-v4l2.toml");
        let cli = Cli::parse_from([
            "audio".as_ref(),
            "--board-file".as_ref(),
            generic.as_os_str(),
            "--socket".as_ref(),
            dir.join("never.sock").as_os_str(),
        ]);
        run(cli).unwrap();
        assert!(!dir.join("never.sock").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
