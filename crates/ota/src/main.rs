//! The update service (design §6): checks the release channel, downloads
//! and verifies signed packages, installs them into the inactive A/B slot,
//! and commits a trial boot once it is healthy (OTA-11, OTA-14..19).
//!
//! ```text
//! update --socket /tmp/kamoder-update.sock --supervisor-socket /tmp/anyka-supervisor.sock
//! ```
//!
//! - **Paths**: the data root is `--data-root`, else `$KAMODER_DATA`, else
//!   the working directory (the supervisor runs services there). The
//!   running slot is `--slot-dir`, else `$KAMODER_SLOT`, else the data root
//!   (a legacy tree). State and downloads live in `<data root>/update/`.
//! - **Board**: `board::current()` (the supervisor's handoff), matched
//!   against the manifest's `boards`; an unresolvable board is logged and
//!   every package is then refused with `board`.
//! - **Keys**: the update keys compiled into `ota-manifest`. Without one the
//!   service still runs (status, rollback, health commit), but every
//!   package is refused (`no_trusted_key`).
//! - **Reboots**: `sync` + `reboot(RB_AUTOBOOT)`; `--no-reboot` only logs
//!   (development).
//! - **Exit status**: SIGTERM/SIGINT exit 0; a socket that cannot be bound is
//!   an error exit.

// Linked for its side effect: `os` supplies the `pthread_setname_np`
// shim that the crosstool's uClibc 0.9.33 does not export but Rust's thread
// startup references (and the TLS stack spawns threads through tokio).
extern crate os;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use eyre::{Context, Result};
use ota::service::health::HealthConfig;
use ota::service::schedule::ScheduleConfig;
use ota::service::source::GithubSource;
use ota::service::{self, server, Config, Service};
use ota::Layout;
use ota_manifest::{TrustedKeys, Version};
use tokio::signal::unix::{signal, SignalKind};

/// Version compiled in (`KAMODER_VERSION` from the release build, else the
/// crate version); the slot's `VERSION` file takes precedence.
const BUILD_VERSION: &str = match option_env!("KAMODER_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// Over-the-air updates: checks, downloads, installs, health commit.
#[derive(Parser, Debug)]
#[command(version = BUILD_VERSION, about, long_about = None)]
struct Cli {
    /// Unix socket for status and commands (web).
    #[arg(long, default_value = update_proto::DEFAULT_SOCKET)]
    socket: PathBuf,
    /// Data root (shared data, `update/`, `boot/`) [default: $KAMODER_DATA,
    /// else the working directory].
    #[arg(long, env = "KAMODER_DATA")]
    data_root: Option<PathBuf>,
    /// The running slot's directory [default: $KAMODER_SLOT, else the data
    /// root].
    #[arg(long, env = "KAMODER_SLOT")]
    slot_dir: Option<PathBuf>,
    /// The supervisor's control socket (trial health check).
    #[arg(long, default_value = supervisor::control::DEFAULT_SOCKET)]
    supervisor_socket: PathBuf,
    /// Web server the trial health check probes with `GET /`.
    #[arg(long, default_value = "127.0.0.1:80")]
    web_addr: SocketAddr,
    /// Seconds every health probe must pass before a trial slot commits.
    #[arg(long, default_value_t = 180)]
    health_stable_secs: u64,
    /// Seconds between health probes.
    #[arg(long, default_value_t = 15)]
    health_poll_secs: u64,
    /// The boot shim's trial window, for the deadline in the status.
    #[arg(long, default_value_t = 600)]
    trial_window_secs: u64,
    /// Seconds between automatic checks (±10 % jitter).
    #[arg(long, default_value_t = 43_200)]
    check_interval_secs: u64,
    /// Seconds after start before the first automatic check.
    #[arg(long, default_value_t = 120)]
    first_check_secs: u64,
    /// Repository (`owner/name`) used until settings are saved
    /// (`UPDATE_REPO` in camera.conf) [default: dangerousplay/kamoder].
    #[arg(long, env = "UPDATE_REPO")]
    repo: Option<String>,
    /// Download rate cap in KiB/s; 0 = unlimited (protects the Wi-Fi).
    #[arg(long, default_value_t = 256)]
    rate_limit_kib: u32,
    /// Log instead of rebooting (development).
    #[arg(long)]
    no_reboot: bool,
}

impl Cli {
    fn data_root(&self) -> Result<PathBuf> {
        match self
            .data_root
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
        {
            Some(path) => Ok(path.clone()),
            None => std::env::current_dir().wrap_err("cannot determine the working directory"),
        }
    }
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let cli = Cli::parse();
    let _telemetry = telemetry::install("update")?;
    service::describe_metrics();
    run(cli)
}

fn run(cli: Cli) -> Result<()> {
    let data_root = cli.data_root()?;
    let slot_dir = cli
        .slot_dir
        .clone()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| data_root.clone());
    let board = match board::current() {
        Ok(resolved) => {
            tracing::info!(board = %resolved.name, reason = %resolved.reason, "board");
            resolved.name
        }
        Err(error) => {
            tracing::warn!(%error, "cannot determine the board; packages will be refused");
            String::new()
        }
    };
    let keys = TrustedKeys::builtin().wrap_err("the built-in update key is malformed")?;
    let compiled_version: Version = BUILD_VERSION
        .trim_start_matches('v')
        .parse()
        .unwrap_or_else(|_| Version::new(0, 0, 0));

    let fetcher = fetch::Fetcher::new(fetch::Config {
        user_agent: format!("kamoder-update/{BUILD_VERSION}"),
        rate_limit_bps: (cli.rate_limit_kib > 0).then(|| cli.rate_limit_kib.saturating_mul(1024)),
        ..fetch::Config::default()
    })?;

    let reboot: service::RebootFn = if cli.no_reboot {
        Arc::new(|| tracing::warn!("reboot skipped (--no-reboot)"))
    } else {
        Arc::new(service::system_reboot)
    };
    let config = Config {
        layout: Layout::new(&data_root),
        slot_dir,
        board,
        target: service::TARGET.to_owned(),
        keys,
        compiled_version,
        supervisor_socket: cli.supervisor_socket.clone(),
        web_addr: cli.web_addr,
        health: HealthConfig {
            poll: Duration::from_secs(cli.health_poll_secs.max(1)),
            stable: Duration::from_secs(cli.health_stable_secs),
            trial_window: Duration::from_secs(cli.trial_window_secs),
        },
        schedule: ScheduleConfig {
            first_delay: Duration::from_secs(cli.first_check_secs),
            interval: Duration::from_secs(cli.check_interval_secs.max(60)),
            ..ScheduleConfig::default()
        },
        reboot_delay: service::REBOOT_DELAY,
        reboot,
        clock: Arc::new(unix_now),
        seed_repo: cli.repo.clone(),
    };
    tracing::info!(
        data_root = %data_root.display(),
        slot_dir = %config.slot_dir.display(),
        socket = %cli.socket.display(),
        "update starting"
    );

    // One thread; package reads and slot writes go to spawn_blocking.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let service = Service::new(config, GithubSource::new(fetcher))
            .wrap_err("cannot create the update directory")?;
        // Held for the lifetime of the service; dropping it removes the file.
        let _socket = server::serve(&cli.socket, Arc::clone(&service))?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = service.run() => {}
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        tracing::info!("shutting down");
        Ok::<_, eyre::Report>(())
    })
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
        let cli = Cli::parse_from(["update"]);
        assert_eq!(cli.socket, PathBuf::from("/tmp/kamoder-update.sock"));
        assert_eq!(
            cli.supervisor_socket,
            PathBuf::from("/tmp/anyka-supervisor.sock")
        );
        assert_eq!(cli.web_addr, "127.0.0.1:80".parse::<SocketAddr>().unwrap());
        assert_eq!(cli.health_stable_secs, 180);
        assert_eq!(cli.health_poll_secs, 15);
        assert_eq!(cli.trial_window_secs, 600);
        assert_eq!(cli.check_interval_secs, 43_200);
        assert_eq!(cli.first_check_secs, 120);
        assert_eq!(cli.rate_limit_kib, 256);
        assert!(!cli.no_reboot);
        if std::env::var_os("KAMODER_DATA").is_none() {
            assert_eq!(cli.data_root, None);
            assert_eq!(cli.data_root().unwrap(), std::env::current_dir().unwrap());
        }
    }

    #[test]
    fn supervisor_toml_flags_parse() {
        let cli = Cli::parse_from([
            "update",
            "--socket",
            "/tmp/kamoder-update.sock",
            "--supervisor-socket",
            "/tmp/anyka-supervisor.sock",
            "--data-root",
            "/mnt/disc1/anyka_rust",
            "--slot-dir",
            "/mnt/disc1/anyka_rust/slots/b",
            "--no-reboot",
        ]);
        assert_eq!(
            cli.data_root().unwrap(),
            PathBuf::from("/mnt/disc1/anyka_rust")
        );
        assert_eq!(
            cli.slot_dir,
            Some(PathBuf::from("/mnt/disc1/anyka_rust/slots/b"))
        );
        assert!(cli.no_reboot);
    }

    #[test]
    fn build_version_parses() {
        assert!(BUILD_VERSION
            .trim_start_matches('v')
            .parse::<Version>()
            .is_ok());
    }
}
