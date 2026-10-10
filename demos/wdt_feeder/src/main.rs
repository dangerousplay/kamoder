//! Hardware watchdog feeder — Rust port of `cross-compile/wdt_feeder/wdt_feeder.c`.
//!
//! The AK3918 arms a hardware watchdog early in boot. Stopping the vendor
//! `anyka_ipc` process therefore reboots the board within seconds unless
//! something else keeps feeding the timer; that is what this does.
//!
//! It can also watch a named process and deliberately stop feeding if that
//! process dies, letting the watchdog recover the board.
//!
//! This binary deliberately uses no SDK library, so it runs on a stock camera
//! with nothing else installed.

use clap::Parser;
use eyre::Result;
use metrics::{counter, describe_counter, describe_gauge, gauge};
use os::shutdown;
use os::CharDevWatchdog;
use std::path::PathBuf;
use std::time::Duration;

/// Consecutive failed health checks before handing the board to the watchdog.
const MAX_FAILED_CHECKS: u32 = 5;

/// Feeds the hardware watchdog, optionally only while a named process lives.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Watchdog device.
    #[arg(short = 'd', default_value = "/dev/watchdog")]
    device: PathBuf,
    /// Hardware timeout to program, in seconds.
    #[arg(short = 't', default_value_t = 8)]
    timeout_seconds: i32,
    /// Seconds between feeds (minimum 1).
    #[arg(short = 'i', default_value = "1", value_parser = parse_interval)]
    interval: Duration,
    /// Stop feeding — and let the watchdog reboot the board — once this
    /// process (matched against /proc/<pid>/comm) has been gone for a while.
    #[arg(short = 'm')]
    monitored_service: Option<String>,
    /// While another process holds the device (EBUSY), keep retrying instead
    /// of exiting. On the AV100 the vendor app owns /dev/watchdog at boot;
    /// waiting keeps this process alive, so init.sh's vendor guard sees it
    /// and kills the vendor app, and the device is taken within one retry.
    #[arg(short = 'w', long = "wait-busy")]
    wait_busy: bool,
}

/// How often to retry a busy watchdog device. Must stay well under the
/// hardware timeout: once the holder dies, the timer runs unfed until this
/// process gets the device.
const BUSY_RETRY: Duration = Duration::from_millis(250);

/// Opens the device, retrying while it is busy if asked to.
///
/// `None` means shutdown was requested while waiting.
fn open_watchdog(args: &Args) -> Result<Option<CharDevWatchdog>> {
    let mut waiting = false;
    loop {
        match CharDevWatchdog::open(&args.device, args.timeout_seconds) {
            Ok(watchdog) => return Ok(Some(watchdog)),
            Err(os::watchdog::Error::Open { ref source, .. })
                if args.wait_busy && source.raw_os_error() == Some(libc::EBUSY) =>
            {
                if !waiting {
                    tracing::warn!("watchdog busy (held by another process), waiting for it");
                    waiting = true;
                }
                if !shutdown::running() {
                    return Ok(None);
                }
                std::thread::sleep(BUSY_RETRY);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn parse_interval(raw: &str) -> Result<Duration, std::num::ParseIntError> {
    Ok(Duration::from_secs(raw.parse::<u64>()?.max(1)))
}

/// Looks for a process whose `/proc/<pid>/comm` matches `name`.
///
/// The C version walked `/proc` with `opendir`/`fopen` and compared with
/// `strcmp`; this is the same scan, minus the manual buffer handling.
fn is_process_running(name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(pid) = file_name.to_str() else {
            continue;
        };
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
            if comm.trim_end_matches(['\n', '\r']) == name {
                return true;
            }
        }
    }
    false
}

fn describe_metrics() {
    describe_counter!("kamoder_watchdog_feeds_total", "Watchdog keepalives sent");
    describe_gauge!(
        "kamoder_watchdog_service_up",
        "1 when the monitored process is running, 0 when it is not"
    );
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let args = Args::parse();
    let _telemetry = telemetry::install("wdt_feeder")?;
    describe_metrics();

    shutdown::install();

    tracing::info!(
        device = %args.device.display(),
        timeout_seconds = args.timeout_seconds,
        interval_seconds = args.interval.as_secs(),
        "opening watchdog"
    );
    let Some(mut watchdog) = open_watchdog(&args)? else {
        tracing::info!("shut down while waiting for the watchdog");
        return Ok(());
    };
    tracing::info!("watchdog feeder started");

    let mut failed_checks = 0;
    while shutdown::running() {
        if let Some(service) = &args.monitored_service {
            if is_process_running(service) {
                failed_checks = 0;
                gauge!("kamoder_watchdog_service_up").set(1.0);
            } else {
                failed_checks += 1;
                gauge!("kamoder_watchdog_service_up").set(0.0);
                tracing::warn!(
                    service,
                    failed_checks,
                    limit = MAX_FAILED_CHECKS,
                    "monitored process not detected"
                );
                if failed_checks >= MAX_FAILED_CHECKS {
                    tracing::error!(
                        service,
                        "critical service dead, letting the watchdog reboot the board"
                    );
                    // Skip the magic close so the timer keeps running and the
                    // board reboots, which is the whole point of this branch.
                    watchdog.leave_armed();
                    return Ok(());
                }
            }
        }

        watchdog.feed()?;
        counter!("kamoder_watchdog_feeds_total").increment(1);
        std::thread::sleep(args.interval);
    }

    // Dropping `watchdog` writes the magic 'V' that disarms the timer.
    tracing::info!("shutting down, disarming the watchdog");
    drop(watchdog);
    Ok(())
}
