//! The lights service: takes the light and speaker-amp GPIOs, applies the
//! saved state (or the defaults: day, lights off, amp muted), serves
//! `/tmp/kamoder-lights.sock`, and leaves the safe state behind when it stops.
//!
//! ```text
//! lights --state etc/lights.json
//! ```
//!
//! - **Started right after wdt_feeder** by the supervisor: the red/blue
//!   lights are lit from boot until something drives GPIO 48.
//! - **Exit status**: SIGTERM/SIGINT apply the safe state and exit 0 — the
//!   supervisor's "asked to stop". A GPIO that cannot be taken at start is an
//!   error exit, so the supervisor retries.
//! - **`--gpio-root`** points at a fake tree for the itest VM, which has no
//!   GPIOs (`KAMODER_LIGHTS_GPIO_ROOT`).

// Linked for its side effect: `os` supplies the `pthread_setname_np`
// shim that the crosstool's uClibc 0.9.33 does not export but Rust's thread
// startup references.
extern crate os;

use clap::Parser;
use eyre::{Context, Result};
use lights::owner::{self, Owner};
use lights::{store, Lights, Sysfs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

/// Owns the camera's lights and speaker amp.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Root of the sysfs GPIO tree.
    #[arg(long, env = "KAMODER_LIGHTS_GPIO_ROOT", default_value = lights::gpio::DEFAULT_ROOT)]
    gpio_root: PathBuf,
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
    let gpio = Sysfs::new(&cli.gpio_root);
    let lights = Lights::start(Box::new(gpio), initial).wrap_err_with(|| {
        format!(
            "cannot take the light GPIOs under {}",
            cli.gpio_root.display()
        )
    })?;
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
            assert_eq!(cli.gpio_root, PathBuf::from("/sys/class/gpio"));
        }
        assert_eq!(cli.state, "etc/lights.json");
        assert_eq!(cli.socket, "/tmp/kamoder-lights.sock");
    }

    #[test]
    fn empty_paths_disable_persistence_and_socket() {
        let cli = Cli::parse_from(["lights", "--state", "", "--socket", "", "--gpio-root", "/x"]);
        assert!(cli.state.is_empty() && cli.socket.is_empty());
        assert_eq!(cli.gpio_root, PathBuf::from("/x"));
    }
}
