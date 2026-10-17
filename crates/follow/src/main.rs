//! The follow service: keeps a moving subject centred by steering the PTZ
//! head from camerad's motion grids.
//!
//! ```text
//! follow --state etc/follow.json
//! ```
//!
//! - **Only on boards that follow**: without `[ptz.follow]` in the board
//!   profile (`board::current()`, or `--board-file`) it logs and exits 0, so
//!   a supervisor entry with `restart = "on-failure"` stays stopped (D-F5).
//! - **The switch** comes from `--state` (`etc/follow.json`, written by
//!   `set` on the socket); when that file does not exist yet,
//!   `--enabled`/`FOLLOW_ENABLED` from `camera.conf` seeds it (default off,
//!   FOL-14).
//! - **Peers**: grids from `--motion-socket` (camerad), moves through
//!   `--ptz-socket` (onvif) tagged `source = follow`, so they do not count
//!   as manual activity. Either may be down; the state is then
//!   `unavailable` and both are retried.
//! - **Exit status**: SIGTERM/SIGINT stop a move of ours that is still
//!   running and exit 0. A socket that cannot be bound is an error exit.

// Linked for its side effect: `os` supplies the `pthread_setname_np`
// shim that the crosstool's uClibc 0.9.33 does not export but Rust's thread
// startup references.
extern crate os;

use clap::builder::BoolishValueParser;
use clap::{ArgAction, Parser};
use eyre::{Context, Result};
use follow::runtime::{Runtime, Settings};
use follow::server::{self, Shared};
use follow::store;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

/// Follows movement with the PTZ head.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Board profile (TOML) to use instead of the detected board; for
    /// development and tests.
    #[arg(long)]
    board_file: Option<PathBuf>,
    /// Saved switch (JSON); empty keeps changes in memory only.
    #[arg(long, default_value = "etc/follow.json")]
    state: String,
    /// Switch at start while no saved state exists (0/1, true/false,
    /// yes/no, on/off); default off.
    #[arg(long, env = "FOLLOW_ENABLED", action = ArgAction::Set,
          value_parser = BoolishValueParser::new())]
    enabled: Option<bool>,
    /// Unix socket for status and the switch (web); empty disables it.
    #[arg(long, default_value = follow_proto::DEFAULT_SOCKET)]
    socket: String,
    /// camerad's motion grid socket.
    #[arg(long, default_value = video_proto::motion::DEFAULT_MOTION_SOCKET)]
    motion_socket: PathBuf,
    /// Board sensor index to follow.
    #[arg(long, default_value_t = 0)]
    camera: u32,
    /// The PTZ owner's socket (onvif).
    #[arg(long, default_value = ptz_proto::DEFAULT_SOCKET)]
    ptz_socket: PathBuf,
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let cli = Cli::parse();
    let _telemetry = telemetry::install("follow")?;
    follow::describe_metrics();
    run(cli)
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
    let Some(follow) = profile.ptz.as_ref().and_then(|ptz| ptz.follow.as_ref()) else {
        tracing::info!(board = %name, "the board has no [ptz.follow]; nothing to do");
        return Ok(());
    };
    let settings = Settings {
        motion_socket: cli.motion_socket,
        camera: cli.camera,
        ptz_socket: cli.ptz_socket,
        ..Settings::from_board(follow)
    };
    let store = (!cli.state.is_empty()).then(|| PathBuf::from(&cli.state));
    let enabled = store::initial(store.as_deref(), cli.enabled);
    tracing::info!(
        board = %name,
        enabled,
        motion = %settings.motion_socket.display(),
        ptz = %settings.ptz_socket.display(),
        camera = settings.camera,
        "follow starting"
    );
    let shared = Shared::new(enabled, store);

    // One thread: ten grids a second and a few requests a minute.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        // Held for the lifetime of the service; dropping it removes the file.
        let _socket = if cli.socket.is_empty() {
            None
        } else {
            Some(server::serve(Path::new(&cli.socket), Arc::clone(&shared))?)
        };
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut tracker = Runtime::new(settings, shared);
        tokio::select! {
            _ = tracker.run() => {}
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        tracing::info!("shutting down");
        tracker.shutdown().await;
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
        let cli = Cli::parse_from(["follow"]);
        if std::env::var_os("FOLLOW_ENABLED").is_none() {
            assert_eq!(cli.enabled, None);
        }
        assert_eq!(cli.board_file, None);
        assert_eq!(cli.state, "etc/follow.json");
        assert_eq!(cli.socket, "/tmp/kamoder-follow.sock");
        assert_eq!(cli.motion_socket, Path::new("/tmp/kamoder-motion.sock"));
        assert_eq!(cli.camera, 0);
        assert_eq!(cli.ptz_socket, Path::new("/tmp/anyka-ptz.sock"));
    }

    #[test]
    fn enabled_takes_camera_conf_spellings() {
        for (value, want) in [("1", true), ("0", false), ("true", true), ("off", false)] {
            let cli = Cli::parse_from(["follow", "--enabled", value]);
            assert_eq!(cli.enabled, Some(want), "{value}");
        }
        assert!(Cli::try_parse_from(["follow", "--enabled", "maybe"]).is_err());
    }
}
