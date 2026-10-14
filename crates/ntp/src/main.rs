//! The `ntp` service: keeps the camera clock set from SNTP servers.
//!
//! ```text
//! ntp [--servers 'a.st1.ntp.br pool.ntp.org'] [--interval 3600]   sync loop
//! ntp query                                       one round, print, clock untouched
//! ntp tz America/Sao_Paulo                        print the POSIX TZ string (init.sh)
//! ```
//!
//! - **Never exits on network errors** (NTP-02): a failed round is counted
//!   and retried after 5 s, doubling to 5 min; a successful one waits
//!   `--interval` (NTP-03).
//! - **`NTP_ENABLED=0`** (NTP-05) logs and exits 0. The supervisor entry is
//!   `restart = "on-failure"`, so that exit is final, while a crash (non-zero)
//!   is still restarted.
//! - **Exit status**: SIGTERM/SIGINT exit 0, the supervisor's "asked to stop".
//! - **`tz`** writes only the POSIX string to stdout, or a message to stderr
//!   and exits 1 for an unknown name; it opens no log file, so init.sh can
//!   run it before the stack is up.

// Linked for its side effect: `os` supplies the `pthread_setname_np` shim
// that the crosstool's uClibc 0.9.33 does not export but Rust's thread
// startup references.
extern crate os;

use clap::builder::BoolishValueParser;
use clap::{ArgAction, Args, Parser, Subcommand};
use eyre::Result;
use metrics::{counter, gauge};
use ntp::backoff::Backoff;
use ntp::client::{self, Synced};
use ntp::clock::{self, Clock, SystemClock};
use ntp::packet::NANOS;
use os::shutdown;
use std::io::Write;
use std::time::Duration;

/// Sets the camera clock from SNTP servers.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    sync: SyncArgs,
    /// Without a subcommand, keep the clock in sync.
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Print the POSIX TZ string for an IANA zone name (or a raw POSIX
    /// string, passed through); exit 1 if the name is unknown.
    Tz {
        /// IANA name such as America/Sao_Paulo, or a POSIX string such as <-03>3.
        #[arg(required_unless_present = "list")]
        name: Option<String>,
        /// List the built-in zone names and their POSIX strings.
        #[arg(long)]
        list: bool,
    },
    /// Run one sync round and print the measured offset; the clock is not
    /// changed.
    Query,
}

#[derive(Args, Debug)]
struct SyncArgs {
    /// SNTP servers, space separated, tried in order. Empty means the
    /// default; use --enabled to turn sync off.
    #[arg(long, env = "NTP_SERVERS", default_value = ntp::DEFAULT_SERVERS)]
    servers: String,
    /// Seconds between syncs once the clock is set (minimum 16).
    #[arg(long, env = "NTP_INTERVAL", default_value_t = 3600)]
    interval: u64,
    /// Seconds to wait for each server's reply.
    #[arg(long, default_value_t = client::TIMEOUT.as_secs())]
    timeout: u64,
    /// 0/false/no/off: log and exit 0 without touching the clock.
    #[arg(long, env = "NTP_ENABLED", default_value_t = true, action = ArgAction::Set,
          value_parser = BoolishValueParser::new())]
    enabled: bool,
}

/// Courtesy floor for public servers.
const MIN_INTERVAL: u64 = 16;

fn main() -> Result<()> {
    // Before telemetry, so `--help` and `tz` open no log files.
    let cli = Cli::parse();
    match cli.command {
        Some(Cmd::Tz { name, list }) => tz(name.as_deref(), list),
        Some(Cmd::Query) => {
            let _telemetry = telemetry::install("ntp")?;
            query(&cli.sync)
        }
        None => {
            let _telemetry = telemetry::install("ntp")?;
            ntp::describe_metrics();
            run(&cli.sync)
        }
    }
}

fn tz(name: Option<&str>, list: bool) -> Result<()> {
    if list {
        // writeln, not println: `ntp tz --list | head` must not panic on
        // the closed pipe.
        let mut out = std::io::stdout().lock();
        for (zone, posix) in ntp::tz::ZONES {
            if writeln!(out, "{zone:<22} {posix}").is_err() {
                break;
            }
        }
        return Ok(());
    }
    match ntp::tz::resolve(name.unwrap_or_default()) {
        Ok(posix) => {
            println!("{posix}");
            Ok(())
        }
        Err(error) => {
            eprintln!("ntp: {error}");
            std::process::exit(1);
        }
    }
}

fn timeout(args: &SyncArgs) -> Duration {
    Duration::from_secs(args.timeout.max(1))
}

fn query(args: &SyncArgs) -> Result<()> {
    let servers = ntp::server_list(&args.servers);
    let Synced {
        server,
        addr,
        sample,
    } = client::sync(&servers, timeout(args), &SystemClock, ntp::BUILD_FLOOR_UNIX)?;
    println!(
        "{server} ({addr}) stratum {}: offset {:+.3} s, delay {:.3} s",
        sample.stratum,
        seconds(sample.offset),
        seconds(sample.delay)
    );
    Ok(())
}

fn seconds(nanos: i64) -> f64 {
    nanos as f64 / NANOS as f64
}

fn run(args: &SyncArgs) -> Result<()> {
    shutdown::install();
    if !args.enabled {
        tracing::info!("time sync disabled (NTP_ENABLED=0); exiting");
        return Ok(());
    }
    let servers = ntp::server_list(&args.servers);
    let interval = Duration::from_secs(args.interval.max(MIN_INTERVAL));
    let timeout = timeout(args);
    tracing::info!(
        ?servers,
        interval_s = interval.as_secs(),
        "time sync starting"
    );

    let clock = SystemClock;
    let mut synced = false;
    let mut backoff = Backoff::default();
    while shutdown::running() {
        let wait = match round(&servers, timeout, &clock, !synced) {
            Ok(()) => {
                synced = true;
                backoff.reset();
                interval
            }
            Err(error) => {
                counter!(ntp::METRIC_FAILURES).increment(1);
                let delay = backoff.next_delay();
                tracing::warn!(%error, retry_s = delay.as_secs(), "time sync failed");
                delay
            }
        };
        sleep_while_running(wait);
    }
    tracing::info!("shutting down");
    Ok(())
}

/// One sync round plus the correction.
fn round(servers: &[String], timeout: Duration, clock: &dyn Clock, first: bool) -> Result<()> {
    let Synced {
        server,
        addr,
        sample,
    } = client::sync(servers, timeout, clock, ntp::BUILD_FLOOR_UNIX)?;
    let done = clock::correct(clock, sample.offset, first, ntp::BUILD_FLOOR_UNIX)?;
    gauge!(ntp::METRIC_OFFSET).set(seconds(sample.offset));
    gauge!(ntp::METRIC_LAST_SYNC).set(seconds(clock.now()));
    let fields = (seconds(sample.offset), seconds(sample.delay));
    if done == clock::Correction::Step {
        tracing::info!(%server, %addr, stratum = sample.stratum, offset_s = fields.0,
            delay_s = fields.1, "clock stepped");
    } else {
        tracing::debug!(%server, %addr, stratum = sample.stratum, offset_s = fields.0,
            delay_s = fields.1, "clock slewed");
    }
    Ok(())
}

/// Sleeps in short steps so SIGTERM is honoured within a quarter second.
fn sleep_while_running(total: Duration) {
    let step = Duration::from_millis(250);
    let mut slept = Duration::ZERO;
    while slept < total && shutdown::running() {
        std::thread::sleep(step);
        slept += step;
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_match_the_spec() {
        let cli = Cli::parse_from(["ntp"]);
        if std::env::var_os("NTP_SERVERS").is_none() {
            assert_eq!(cli.sync.servers, "a.st1.ntp.br b.st1.ntp.br pool.ntp.org");
        }
        if std::env::var_os("NTP_INTERVAL").is_none() {
            assert_eq!(cli.sync.interval, 3600);
        }
        if std::env::var_os("NTP_ENABLED").is_none() {
            assert!(cli.sync.enabled);
        }
        assert_eq!(cli.sync.timeout, 2);
        assert!(cli.command.is_none());
    }

    #[test]
    fn enabled_accepts_zero() {
        for off in ["0", "false", "no", "off"] {
            let cli = Cli::parse_from(["ntp", "--enabled", off]);
            assert!(!cli.sync.enabled, "{off}");
        }
        let cli = Cli::parse_from(["ntp", "--enabled", "1"]);
        assert!(cli.sync.enabled);
    }

    #[test]
    fn tz_subcommand_parses() {
        let cli = Cli::parse_from(["ntp", "tz", "America/Sao_Paulo"]);
        assert!(
            matches!(cli.command, Some(Cmd::Tz { name: Some(ref n), list: false }) if n == "America/Sao_Paulo")
        );
        let cli = Cli::parse_from(["ntp", "tz", "--list"]);
        assert!(matches!(
            cli.command,
            Some(Cmd::Tz {
                name: None,
                list: true
            })
        ));
        assert!(Cli::try_parse_from(["ntp", "tz"]).is_err());
    }

    #[test]
    fn options_and_query() {
        let cli = Cli::parse_from(["ntp", "--servers", "x y", "--interval", "60", "query"]);
        assert_eq!(cli.sync.servers, "x y");
        assert_eq!(cli.sync.interval, 60);
        assert!(matches!(cli.command, Some(Cmd::Query)));
    }
}
