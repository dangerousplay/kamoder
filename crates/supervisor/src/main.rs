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
//! ```
//!
//! Individual services can be inspected, stopped, started and restarted
//! through a Unix control socket; see [`supervisor::control`].

mod banner;
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

use clap::Parser;
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
                console::uptime(),
                "supervisor",
                &line,
                true,
            ));
        });
        (sink, level.directive().to_owned())
    });
    let _telemetry = telemetry::install_with_console("supervisor", console_sink)?;
    describe_metrics();

    let config = Config::load(&args.config)?;
    let enabled = config.enabled_services().count();
    if args.banner {
        print!("{}", banner::render(&config, &args.config, &start));
        return Ok(());
    }
    if args.check_only {
        println!(
            "{}: ok, {enabled} service(s) enabled",
            args.config.display()
        );
        for service in config.enabled_services() {
            let program = config.resolve(&service.command);
            let exists = if program.is_file() { "" } else { "  [MISSING]" };
            println!("  {:<16} {}{}", service.name, program.display(), exists);
        }
        return Ok(());
    }
    if enabled == 0 {
        bail!("{}: no services are enabled", args.config.display());
    }

    match &console {
        Some(console) => console.print(banner::render(&config, &args.config, &start)),
        None if !args.console.as_os_str().is_empty() => {
            tracing::warn!(console = %args.console.display(), "console unavailable")
        }
        None => {}
    }

    // Logs live on the SD card, so resolve a relative log_dir against the
    // config rather than the working directory.
    let ready_console = console.clone();
    let log_dir = config.resolve(&config.supervisor.log_dir);
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
        let wifi_args = config
            .services
            .iter()
            .find(|s| s.name == "wifi" || s.command.file_name().is_some_and(|f| f == "wifi"))
            .map(|s| s.args.clone())
            .unwrap_or_default();
        // Resolved here for now; the supervisor will resolve the board once
        // at start and hand it to every service (BRD-05).
        let board = board::current()
            .inspect_err(|error| tracing::warn!(%error, "no board profile for the ready report"))
            .ok();
        let wifi_iface = ready::wifi_iface(
            &wifi_args,
            board.as_ref().and_then(|b| b.profile.wifi.as_ref()),
        );
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
}
