//! Event-driven process supervisor for the camera.
//!
//! Replaces `SD_card_contents/anyka_happy/service_supervisor.sh`, which woke
//! every 10 seconds, shelled out to `pidof`/`ps` for each service and restarted
//! anything missing. That costs CPU on an idle camera, takes up to 10 seconds
//! to notice a crash, and has no backoff — a service that dies instantly is
//! restarted forever.
//!
//! This version blocks until something actually happens. Child exits arrive
//! through tokio's `SIGCHLD`-driven process driver, so an idle supervisor
//! consumes nothing, and a crash is noticed immediately.
//!
//! ```sh
//! supervisor --config /mnt/anyka_rust/supervisor.toml
//! supervisor --config ... --check    # validate and exit
//! supervisor --config slots/b/supervisor.toml --slot-dir slots/b --data-dir .
//! supervisor board --shell            # board facts for init.sh to eval
//! ```
//!
//! At start the board profile is resolved once (`--board`/`BOARD`, else
//! detection), written to `--board-handoff` and passed to every service in
//! `KAMODER_BOARD`. A board that cannot be resolved is logged and the
//! services run without it: the watchdog feeder and Wi-Fi never depend on
//! `boards/` being right.
//!
//! Individual services can be inspected, stopped, started and restarted
//! through a Unix control socket; see [`supervisor::control`].

mod banner;
mod board_select;
mod config;
mod console;
mod hardware;
mod logs;
mod ready;
mod server;
mod service;
mod supervisor;

// Linked for its side effect: the `pthread_setname_np` shim the crosstool's
// uClibc does not export but Rust's thread startup references.
extern crate os;

use clap::{Parser, Subcommand};
use config::Config;
use eyre::{bail, Result, WrapErr};
use kameo::actor::Actor;
use metrics::{describe_counter, describe_gauge};
use std::path::{Path, PathBuf};
use supervisor::{Shutdown, Supervisor};

/// Supervises the camera's services: restarts them on exit with backoff,
/// rotates their logs, and stops them in reverse order on SIGTERM.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Service configuration.
    #[arg(long, default_value = "/mnt/disc1/anyka_rust/supervisor.toml")]
    config: PathBuf,
    /// Code directory (A/B slot): expands `{slot}` in service commands and
    /// arguments, and holds `boards/`. Default: the config's directory.
    #[arg(long)]
    slot_dir: Option<PathBuf>,
    /// Shared data directory (camera.conf, etc/, logs/): expands `{data}`,
    /// is every service's working directory and the base of a relative
    /// `log_dir`. Default: the config's directory.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Validate the configuration and exit.
    #[arg(long = "check")]
    check_only: bool,
    /// Unix socket for the control API (status/start/stop/restart). Created
    /// with mode 0600. Pass an empty value to disable it.
    #[arg(long, default_value = ::supervisor::control::DEFAULT_SOCKET)]
    control_socket: String,
    /// Console (the UART) for the boot banner, failure alerts with the
    /// failing service's last log lines, and log lines at `--console-level`.
    /// Pass an empty value to disable it.
    #[arg(long, default_value = "/dev/console")]
    console: PathBuf,
    /// Lowest level of log line copied to the console, from the supervisor
    /// and from every service: off, error, warn, info, debug or trace.
    #[arg(long, env = "ANYKA_CONSOLE_LEVEL", default_value = "info")]
    console_level: String,
    /// Print the boot banner to stdout and exit.
    #[arg(long)]
    banner: bool,
    /// Board profile directory; `{slot}`/`{data}` are expanded. Default:
    /// `boards/` in the slot directory (next to the config unless
    /// `--slot-dir` says otherwise).
    #[arg(long, env = "KAMODER_BOARDS_DIR")]
    boards_dir: Option<PathBuf>,
    /// Board profile name, skipping detection (init.sh exports `BOARD`
    /// from camera.conf).
    #[arg(long, env = "BOARD")]
    board: Option<String>,
    /// Where the resolved board is written for the services.
    #[arg(long, default_value = board::DEFAULT_HANDOFF_PATH)]
    board_handoff: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the board this camera resolves to, and exit (non-zero if it
    /// cannot be resolved). Needs no supervisor config.
    Board {
        /// Print `KAMODER_BOARD_NAME`, `VENDOR_PROCESSES` and
        /// `WATCHDOG_WAIT` as shell assignments, for init.sh to `eval`.
        #[arg(long)]
        shell: bool,
        /// Board profile directory. Default: `../boards` from the binary.
        #[arg(long, env = "KAMODER_BOARDS_DIR")]
        boards_dir: Option<PathBuf>,
        /// Board profile name, skipping detection.
        #[arg(long, env = "BOARD")]
        board: Option<String>,
    },
}

/// `supervisor board`: stdout carries only the answer, so `eval` never sees
/// a log line; errors go to stderr.
fn board_command(shell: bool, boards_dir: Option<PathBuf>, board: Option<&str>) -> Result<()> {
    let dir = boards_dir.unwrap_or_else(board_select::default_boards_dir);
    let resolved = board_select::resolve(&dir, board)
        .wrap_err_with(|| format!("cannot resolve the board from {}", dir.display()))?;
    if shell {
        print!("{}", board_select::shell(&resolved));
    } else {
        print!("{}", board_select::human(&resolved));
    }
    Ok(())
}

/// The board profile directory: `--boards-dir` (placeholders expanded), else
/// `boards/` in the slot directory.
fn boards_dir(flag: Option<&Path>, config: &Config) -> PathBuf {
    match flag {
        Some(dir) => match dir.to_str() {
            Some(text) => PathBuf::from(config::expand(text, &config.slot_dir, &config.data_dir)),
            None => dir.to_path_buf(),
        },
        None => config.slot_dir.join("boards"),
    }
}

fn describe_metrics() {
    describe_gauge!(
        "kamoder_supervisor_service_up",
        "1 while the service's process is running"
    );
    describe_gauge!(
        "kamoder_supervisor_last_exit_code",
        "Exit code of the last run, -1 if signalled"
    );
    describe_counter!(
        "kamoder_supervisor_restarts_total",
        "Process restarts attempted"
    );
    describe_counter!(
        "kamoder_supervisor_spawn_failures_total",
        "Processes that could not be spawned"
    );
    describe_counter!(
        "kamoder_supervisor_gave_up_total",
        "Services abandoned after a restart loop"
    );
    describe_counter!(
        "kamoder_supervisor_actor_panics_total",
        "Panics absorbed inside a service actor"
    );
    describe_counter!(
        "kamoder_supervisor_actor_restarts_total",
        "Service actors rebuilt by the root"
    );
    describe_counter!(
        "kamoder_supervisor_root_panics_total",
        "Panics absorbed by the root supervisor"
    );
    describe_counter!(
        "kamoder_supervisor_log_bytes_total",
        "Bytes written to service logs"
    );
    describe_counter!(
        "kamoder_supervisor_log_errors_total",
        "Service log lines that could not be written"
    );
    describe_counter!(
        "kamoder_supervisor_control_requests_total",
        "Control socket requests, by operation and result"
    );
    describe_counter!(
        "kamoder_supervisor_control_refused_total",
        "Control socket connections refused for being over the limit"
    );
}

fn main() -> Result<()> {
    // First, so "Started" in the banner is when this process began.
    let start = ready::StartTime::now();
    // Before telemetry, so `--help` does not open log files.
    let args = Args::parse();
    if let Some(Command::Board {
        shell,
        boards_dir,
        board,
    }) = args.command
    {
        return board_command(shell, boards_dir, board.as_deref());
    }
    let console_level =
        console::Level::parse_threshold(&args.console_level).map_err(|error| eyre::eyre!(error))?;
    let one_shot = args.banner || args.check_only;

    // The console (UART) carries the banner, failure alerts and — at
    // `--console-level` — the supervisor's and every service's log lines.
    // It is started before telemetry so the very first events reach it, and
    // nothing written to it can block: a full queue drops lines.
    let console = if one_shot || args.console.as_os_str().is_empty() {
        None
    } else {
        console::Console::start(args.console.clone()).ok()
    };
    let console_sink = console.clone().zip(console_level).map(|(console, level)| {
        let sink: telemetry::ConsoleSink = std::sync::Arc::new(move |line: String| {
            console.print(console::render_log(
                &console::Stamp::now(),
                "supervisor",
                &line,
                true,
            ));
        });
        (sink, level.directive().to_owned())
    });
    let _telemetry = telemetry::install_with_console("supervisor", console_sink)?;
    describe_metrics();

    let mut config = Config::load_with_dirs(
        &args.config,
        args.slot_dir.as_deref(),
        args.data_dir.as_deref(),
    )?;
    // Counted before the board can switch anything off: an empty config is
    // a mistake, a board without optional hardware is not.
    let configured = config.enabled_services().count();

    // `--check` resolves the board too, so it shows which services this
    // board would skip; a board that cannot be resolved never fails the
    // check (init.sh's preflight must not fall back to the feeder alone
    // because of `boards/`).
    let boards_dir = boards_dir(args.boards_dir.as_deref(), &config);
    let board = {
        let dir = &boards_dir;
        match board_select::resolve(dir, args.board.as_deref()) {
            Ok(resolved) => {
                tracing::info!(
                    board = %resolved.name,
                    reason = %resolved.reason,
                    profile = %resolved.path.display(),
                    "board resolved"
                );
                Some(resolved)
            }
            Err(error) => {
                tracing::error!(
                    %error,
                    boards_dir = %dir.display(),
                    "cannot resolve the board; services resolve it themselves, conditional services all start"
                );
                None
            }
        }
    };
    let skipped = config.apply_board(board.as_ref().map(|b| &b.profile));
    for (service, capability) in &skipped {
        tracing::info!(
            service,
            %capability,
            "service skipped: the board lacks its capability"
        );
    }
    let enabled = config.enabled_services().count();
    // The Wi-Fi interface the wifi service will manage (its `--iface`, else
    // the board's), for the banner and the ready report.
    let wifi_args = config
        .services
        .iter()
        .find(|s| s.name == "wifi" || s.command.file_name().is_some_and(|f| f == "wifi"))
        .map(|s| s.args.clone())
        .unwrap_or_default();
    let wifi_iface = ready::wifi_iface(
        &wifi_args,
        board.as_ref().and_then(|b| b.profile.wifi.as_ref()),
    );
    if args.banner {
        print!(
            "{}",
            banner::render(&config, &args.config, &start, &wifi_iface)
        );
        return Ok(());
    }
    if args.check_only {
        println!(
            "{}: ok, {enabled} service(s) enabled",
            args.config.display()
        );
        match &board {
            Some(resolved) => println!("  board: {} ({})", resolved.name, resolved.reason),
            None => println!(
                "  board: unresolved from {} (conditional services all start)",
                boards_dir.display()
            ),
        }
        for service in config.enabled_services() {
            let program = config.resolve(&service.command);
            let exists = if program.is_file() { "" } else { "  [MISSING]" };
            let when = service
                .when
                .map(|capability| format!("  (when {capability})"))
                .unwrap_or_default();
            println!(
                "  {:<16} {}{}{}",
                service.name,
                program.display(),
                exists,
                when
            );
        }
        for (service, capability) in &skipped {
            println!("  {service:<16} skipped: board lacks {capability}");
        }
        return Ok(());
    }
    if configured == 0 {
        bail!("{}: no services are enabled", args.config.display());
    }

    // Children read the board from the handoff; without one (resolution or
    // the write failed) they fall back to resolving it themselves.
    if let Some(resolved) = &board {
        match board::write_handoff(&args.board_handoff, resolved) {
            Ok(()) => {
                let path = args.board_handoff.to_string_lossy();
                config.inject_env(board::HANDOFF_ENV, &path);
            }
            Err(error) => tracing::error!(%error, "cannot write the board handoff"),
        }
    }

    match &console {
        Some(console) => console.print(banner::render(&config, &args.config, &start, &wifi_iface)),
        None if !args.console.as_os_str().is_empty() => {
            tracing::warn!(console = %args.console.display(), "console unavailable")
        }
        None => {}
    }

    // Logs live on the SD card with the other data, so resolve a relative
    // log_dir against the data directory rather than the working directory.
    let ready_console = console.clone();
    let log_dir = config.resolve_data(&config.supervisor.log_dir);
    let sink = logs::start(
        &log_dir,
        config.supervisor.log_max_bytes,
        config.supervisor.log_keep,
        console.clone(),
        console_level,
    )?;
    tracing::info!(
        log_dir = %log_dir.display(),
        max_bytes = config.supervisor.log_max_bytes,
        keep = config.supervisor.log_keep,
        services = enabled,
        "supervisor starting"
    );

    // A current-thread runtime. This process spends its life blocked on
    // SIGCHLD and a signal handler; worker threads would only cost memory.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .wrap_err("cannot start the tokio runtime")?;

    runtime.block_on(async move {
        let plan = Supervisor::plan(&config, sink.clone());
        let root = Supervisor::spawn((plan, sink.clone()));

        // Prints the ready report (time to ready, Wi-Fi, addresses, ports)
        // once everything is up, or what is missing after a timeout.
        tokio::spawn(ready::watch(
            root.clone(),
            ready_console,
            start,
            ready::wpa_ctrl_dirs(&wifi_args),
            wifi_iface,
        ));

        // The control socket is a convenience; supervision must not depend on
        // it, so a failure to bind is logged and the supervisor carries on.
        let control = if args.control_socket.is_empty() {
            tracing::info!("control socket disabled");
            None
        } else {
            match server::serve(Path::new(&args.control_socket), root.clone()) {
                Ok(server) => Some(server),
                Err(error) => {
                    tracing::error!(%error, "control socket unavailable");
                    None
                }
            }
        };

        wait_for_signal().await;
        tracing::info!("shutting down");
        // Stop taking requests and remove the socket before services go down,
        // so nobody starts something behind the shutdown's back.
        drop(control);

        // Bound the shutdown: a wedged child must not stop the camera from
        // rebooting or the supervisor from exiting.
        let timeout = config.supervisor.shutdown_timeout();
        if tokio::time::timeout(timeout, root.ask(Shutdown))
            .await
            .is_err()
        {
            tracing::warn!(
                timeout_secs = timeout.as_secs(),
                "shutdown timed out, exiting anyway"
            );
        }
        sink.flush();
        Ok::<_, eyre::Report>(())
    })?;

    Ok(())
}

/// Resolves on `SIGTERM` or `SIGINT`.
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(error) => {
            tracing::error!(%error, "cannot listen for SIGTERM");
            return;
        }
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(stream) => stream,
        Err(error) => {
            tracing::error!(%error, "cannot listen for SIGINT");
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("received SIGTERM"),
        _ = int.recv() => tracing::info!("received SIGINT"),
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    /// clap validates the argument definitions lazily, at parse time; this
    /// catches a clash on the host rather than on the camera.
    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    /// Without the new flags the supervisor runs as before: slot and data
    /// default to the config's directory (resolved in `Config`).
    #[test]
    fn slot_and_data_flags_are_optional() {
        let args = Args::try_parse_from(["supervisor", "--config", "x.toml"]).unwrap();
        assert!(args.slot_dir.is_none() && args.data_dir.is_none());
        let args = Args::try_parse_from([
            "supervisor",
            "--config",
            "/r/slots/b/supervisor.toml",
            "--slot-dir",
            "/r/slots/b",
            "--data-dir",
            "/r",
            "--check",
        ])
        .unwrap();
        assert_eq!(args.slot_dir.as_deref(), Some(Path::new("/r/slots/b")));
        assert_eq!(args.data_dir.as_deref(), Some(Path::new("/r")));
        assert!(args.check_only);
    }

    #[test]
    fn boards_dir_defaults_to_the_slot_and_expands_placeholders() {
        let dir = std::env::temp_dir().join(format!("supervisor-boards-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("supervisor.toml");
        std::fs::write(&path, "").unwrap();
        let slot = Path::new("/r/slots/b");
        let config = Config::load_with_dirs(&path, Some(slot), Some(Path::new("/r"))).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(boards_dir(None, &config), slot.join("boards"));
        assert_eq!(
            boards_dir(Some(Path::new("{data}/boards")), &config),
            Path::new("/r/boards")
        );
        assert_eq!(
            boards_dir(Some(Path::new("/elsewhere")), &config),
            Path::new("/elsewhere")
        );
    }
}
