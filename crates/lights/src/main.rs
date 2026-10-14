//! The lights service: takes the light and speaker-amp GPIOs, applies the
//! saved state (or the defaults: day, lights off, amp muted), serves
//! `/tmp/kamoder-lights.sock`, and leaves the safe state behind when it stops.
//!
//! ```text
//! lights --state etc/lights.json
//! ```
//!
//! - **Lines from the board profile** (`board::current()`: the supervisor's
//!   choice in `KAMODER_BOARD`, or resolved here). Lines missing at start
//!   are logged and reported as unsupported; a profile that maps a light to
//!   the Wi-Fi power GPIO is an error exit.
//! - **Started right after wdt_feeder** by the supervisor: on some boards
//!   (the AV100) the red/blue lights are lit from boot until driven.
//! - **Exit status**: SIGTERM/SIGINT apply the safe state and exit 0 — the
//!   supervisor's "asked to stop". A GPIO that cannot be taken at start is an
//!   error exit, so the supervisor retries.
//! - **`--gpio-root`** replaces the profile's `gpio_root`, e.g. a fake tree
//!   for the itest VM, which has no GPIOs (`KAMODER_LIGHTS_GPIO_ROOT`).
//! - **`--board-file`** loads one profile TOML instead, for development.

// Linked for its side effect: `os` supplies the `pthread_setname_np`
// shim that the crosstool's uClibc 0.9.33 does not export but Rust's thread
// startup references.
extern crate os;

use clap::Parser;
use eyre::{Context, Result};
use lights::owner::{self, Owner};
use lights::{store, Lights, Sysfs};
use lights_proto::Role;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

/// Owns the camera's lights and speaker amp.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Root of the sysfs GPIO tree; default: the board profile's
    /// `lights.gpio_root`.
    #[arg(long, env = "KAMODER_LIGHTS_GPIO_ROOT")]
    gpio_root: Option<PathBuf>,
    /// Board profile (TOML) to use instead of the detected board; for
    /// development and tests.
    #[arg(long)]
    board_file: Option<PathBuf>,
    /// Saved state (JSON); empty keeps changes in memory only.
    #[arg(long, default_value = "etc/lights.json")]
    state: String,
    /// Unix socket clients (web, onvif) use; empty disables it.
    #[arg(long, default_value = lights_proto::DEFAULT_SOCKET)]
    socket: String,
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let cli = Cli::parse();
    let _telemetry = telemetry::install("lights")?;
    lights::describe_metrics();
    run(cli)
}

fn run(cli: Cli) -> Result<()> {
    let store = (!cli.state.is_empty()).then(|| PathBuf::from(&cli.state));
    let initial = match &store {
        Some(path) => store::load(path).to_state(),
        None => lights_proto::State::default(),
    };
    let initial = owner::sanitize(initial);
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
    let gpio = Sysfs::from_profile(&profile, cli.gpio_root.as_deref())
        .wrap_err_with(|| format!("board {name}"))?;
    let root = gpio.root().to_path_buf();
    let supported: Vec<&str> = lights::Gpio::supported(&gpio)
        .iter()
        .map(Role::as_str)
        .collect();
    tracing::info!(board = %name, root = %root.display(), ?supported, "light lines");
    if supported.is_empty() {
        tracing::warn!(board = %name, "this board has no usable light lines");
    }
    let lights = Lights::start(Box::new(gpio), initial)
        .wrap_err_with(|| format!("cannot take the light GPIOs under {}", root.display()))?;
    tracing::info!(state = ?lights.state(), "lights applied");
    let owner = Arc::new(Owner::new(lights, store));

    // One thread: a few requests a minute and a blink timer.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let outcome = runtime.block_on(async {
        // Held for the lifetime of the service; dropping it removes the file.
        let _socket = if cli.socket.is_empty() {
            None
        } else {
            Some(owner::serve(Path::new(&cli.socket), Arc::clone(&owner))?)
        };
        let blink = tokio::spawn(owner::blink(Arc::clone(&owner)));
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        tracing::info!("shutting down");
        blink.abort();
        Ok::<_, eyre::Report>(())
    });
    // On every path, including a socket that could not be bound: the Lights
    // drop would do it too, but only once the last Arc is gone.
    owner.shutdown();
    outcome
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
        let cli = Cli::parse_from(["lights"]);
        if std::env::var_os("KAMODER_LIGHTS_GPIO_ROOT").is_none() {
            // The profile's root applies.
            assert_eq!(cli.gpio_root, None);
        }
        assert_eq!(cli.board_file, None);
        assert_eq!(cli.state, "etc/lights.json");
        assert_eq!(cli.socket, "/tmp/kamoder-lights.sock");
    }

    #[test]
    fn empty_paths_disable_persistence_and_socket() {
        let cli = Cli::parse_from(["lights", "--state", "", "--socket", "", "--gpio-root", "/x"]);
        assert!(cli.state.is_empty() && cli.socket.is_empty());
        assert_eq!(cli.gpio_root, Some(PathBuf::from("/x")));
    }
}
