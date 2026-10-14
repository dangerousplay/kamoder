//! `wifi` — keeps the camera on a Wi-Fi network it can be reached on.
//!
//! Runs under `supervisor`. See the crate docs for the model and
//! `reconcile` for the policy.

use clap::{Args, Parser, Subcommand};
use eyre::{Result, WrapErr};
use metrics::{counter, describe_counter, describe_gauge, gauge};
use os::shutdown;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wifi::hardware::{self, Hardware};
use wifi::reconcile::{self, Action, Memory, Observed, Policy};
use wifi::recovery::{self, Outcome, Recovery};
use wifi::system::{self, Paths, SystemOps};
use wifi::{net, settings, wpa};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    paths: PathArgs,

    #[command(flatten)]
    run: RunArgs,

    #[command(flatten)]
    driver: DriverArgs,

    /// Without a subcommand, reconcile continuously.
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print what is configured, what is live, and what would be done; change nothing.
    Status,
}

#[derive(Args)]
struct PathArgs {
    /// Settings file holding wifi_ssid / wifi_password. Re-read every pass,
    /// so edits take effect without a restart.
    #[arg(long, env = "ANYKA_WIFI_CONFIG", default_value = "camera.conf")]
    config: PathBuf,
    /// Station config that wpa_supplicant reads.
    #[arg(
        long,
        env = "ANYKA_WIFI_WPA_CONF",
        default_value = "/rom/wpa_supplicant0.conf"
    )]
    wpa_conf: PathBuf,
    /// wpa_supplicant control socket directory (`-C`).
    #[arg(long, env = "ANYKA_WIFI_CTRL_DIR", default_value = "/etc/Wireless")]
    ctrl_dir: PathBuf,
    /// Wi-Fi interface. Default: the board profile's `[wifi] iface`, else
    /// wlan0.
    #[arg(long, env = "ANYKA_WIFI_IFACE")]
    iface: Option<String>,
    #[arg(
        long,
        env = "ANYKA_WIFI_DHCP_SCRIPT",
        default_value = "/usr/share/udhcpc/default.script"
    )]
    dhcp_script: PathBuf,
    /// Backups of the station config and the last-known-good record. Keep it
    /// on the SD card so it survives a reboot.
    #[arg(long, env = "ANYKA_WIFI_STATE_DIR", default_value = "backup")]
    state_dir: PathBuf,
}

#[derive(Args)]
struct RunArgs {
    /// Seconds between passes.
    #[arg(long, env = "ANYKA_WIFI_INTERVAL", default_value_t = 10)]
    interval: u64,
    /// Seconds an unhealthy link is left alone before the first retry.
    #[arg(long, env = "ANYKA_WIFI_GRACE", default_value_t = 45)]
    grace: u64,
    /// Seconds before the first retry; doubles after each failure.
    #[arg(long, env = "ANYKA_WIFI_BACKOFF", default_value_t = 10)]
    backoff: u64,
    /// Ceiling for the retry backoff, in seconds. At most 300: the camera
    /// keeps trying, at least every five minutes, for as long as it is down.
    #[arg(long, env = "ANYKA_WIFI_BACKOFF_MAX", default_value_t = 300)]
    backoff_max: u64,
    /// Failed retries before rolling back to the last-known-good network.
    #[arg(long, env = "ANYKA_WIFI_MAX_ATTEMPTS", default_value_t = 4)]
    max_attempts: u32,
    /// Seconds the desired network is held off after a rollback.
    #[arg(long, env = "ANYKA_WIFI_ROLLBACK_HOLD", default_value_t = 1800)]
    rollback_hold: u64,
    /// Consecutive unanswered gateway checks, on a link that is otherwise
    /// up, before it counts as down. 1: the first miss.
    #[arg(long, env = "ANYKA_WIFI_GATEWAY_FAILURES", default_value_t = 3)]
    gateway_failures: u32,
    /// Failed station restarts, on a network known to work, before the Wi-Fi
    /// driver is reloaded. 0: never reload the driver or cycle its power.
    #[arg(long, env = "ANYKA_WIFI_DRIVER_RESET_AFTER", default_value_t = 2)]
    driver_reset_after: u32,
    /// Failed driver reloads before the USB Wi-Fi module's power is cycled
    /// (the board's `[wifi] power_gpio`; a driver reload on a board without
    /// one). 0: never.
    #[arg(long, env = "ANYKA_WIFI_POWER_CYCLE_AFTER", default_value_t = 1)]
    power_cycle_after: u32,
    /// Log every decision but touch nothing.
    #[arg(long, env = "ANYKA_WIFI_DRY_RUN")]
    dry_run: bool,
}

/// The Wi-Fi driver the hardware steps reload. Defaults come from the board
/// profile's `[wifi]` section; without a readable profile they are what the
/// AV100 firmware's `/ipc/setup.sh` loads (see `wifi::hardware`).
#[derive(Args)]
struct DriverArgs {
    /// Driver module name, as in `lsmod`. Default: the file name of the
    /// driver path (ssv6x5x).
    #[arg(long, env = "ANYKA_WIFI_DRIVER_MODULE")]
    driver_module: Option<String>,
    /// The module file to `insmod`. Default: the board profile's
    /// `[wifi] module`, else /ipc/modules/wifi/ssv6x5x.ko.
    #[arg(long, env = "ANYKA_WIFI_DRIVER_PATH")]
    driver_path: Option<PathBuf>,
    /// Module parameters, space-separated. Default: the board profile's
    /// `[wifi] module_args`, else
    /// stacfgpath=/ipc/modules/wifi/ssv6x5x-wifi.cfg.
    #[arg(long, env = "ANYKA_WIFI_DRIVER_ARGS")]
    driver_args: Option<String>,
    /// The power GPIO level that powers the USB Wi-Fi module on, as recorded
    /// by the firmware. Missing means 1.
    #[arg(
        long,
        env = "ANYKA_WIFI_USB_POWER_ON_FILE",
        default_value = "/rom/usb_module_power_on"
    )]
    usb_power_on_file: PathBuf,
}

impl Cli {
    fn overrides(&self) -> hardware::Overrides {
        hardware::Overrides {
            iface: self.paths.iface.clone(),
            driver_module: self.driver.driver_module.clone(),
            driver_path: self.driver.driver_path.clone(),
            driver_args: self.driver.driver_args.clone(),
        }
    }
}

impl DriverArgs {
    fn driver(&self, hw: &Hardware) -> recovery::Driver {
        recovery::Driver {
            module: hw.driver_module.clone(),
            path: hw.driver_path.clone(),
            args: hw.driver_args.clone(),
            power_gpio: hw.power_gpio,
            power_on_file: self.usb_power_on_file.clone(),
            ..recovery::Driver::default()
        }
    }
}

impl RunArgs {
    fn policy(&self) -> Policy {
        Policy {
            grace: Duration::from_secs(self.grace),
            backoff_initial: Duration::from_secs(self.backoff.max(1)),
            backoff_max: Duration::from_secs(self.backoff_max.max(self.backoff).max(1))
                .min(reconcile::BACKOFF_CEILING),
            max_attempts: self.max_attempts,
            rollback_hold: Duration::from_secs(self.rollback_hold),
            gateway_failures: self.gateway_failures.max(1),
            driver_reset_after: self.driver_reset_after,
            power_cycle_after: self.power_cycle_after,
        }
    }
}

impl PathArgs {
    fn paths(&self, hw: &Hardware) -> Paths {
        Paths {
            wpa_conf: self.wpa_conf.clone(),
            ctrl_dir: self.ctrl_dir.clone(),
            iface: hw.iface.clone(),
            dhcp_script: self.dhcp_script.clone(),
            state_dir: self.state_dir.clone(),
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let _telemetry = telemetry::install("wifi")?;
    let resolved = match board::current() {
        Ok(resolved) => Some(resolved),
        Err(error) => {
            // Never fatal: Wi-Fi may be the only way in to fix the card.
            tracing::warn!(%error, "no board profile; using the built-in AV100 Wi-Fi defaults");
            None
        }
    };
    let source = match &resolved {
        None => hardware::Board::Unresolved,
        Some(resolved) => match &resolved.profile.wifi {
            Some(wifi) => hardware::Board::Wifi(wifi),
            None => {
                tracing::warn!(
                    board = resolved.name,
                    "board profile has no [wifi]; built-in interface and driver, no power GPIO"
                );
                hardware::Board::NoWifi
            }
        },
    };
    let hw = hardware::merge(&cli.overrides(), source);
    tracing::info!(
        board = resolved.as_ref().map_or("-", |r| r.name.as_str()),
        iface = hw.iface,
        module = hw.driver_module,
        path = %hw.driver_path.display(),
        power_gpio = ?hw.power_gpio,
        "wifi hardware"
    );
    let paths = cli.paths.paths(&hw);
    match cli.command {
        None => run(&cli.paths.config, &paths, &cli.run, cli.driver.driver(&hw)),
        Some(Cmd::Status) => status(&cli.paths.config, &paths, &cli.run.policy()),
    }
}

/// Reads the desired network, dropping one that wpa_supplicant would reject.
fn desired(config: &Path, warned: &mut Option<String>) -> Option<wpa::Network> {
    let desired = match settings::load(config) {
        Ok(desired) => desired,
        Err(error) => {
            report_once(warned, format!("cannot read {}: {error}", config.display()));
            return None;
        }
    };
    let network = desired.network?;
    match network.validate() {
        Ok(()) => {
            *warned = None;
            Some(network)
        }
        Err(error) => {
            report_once(
                warned,
                format!("ignoring wifi settings in {}: {error}", config.display()),
            );
            None
        }
    }
}

fn report_once(last: &mut Option<String>, message: String) {
    if last.as_deref() != Some(message.as_str()) {
        tracing::error!("{message}");
        *last = Some(message);
    }
}

fn observe(paths: &Paths) -> Observed {
    let configured = system::read_configured(paths);
    let status = wpa::query(&paths.ctrl_dir, &paths.iface, "STATUS")
        .map(|reply| wpa::Status::parse(&reply))
        .unwrap_or_default();
    let has_address = net::ipv4_address(&paths.iface).is_some();
    // Only worth the ~3 s ARP probe when everything else already looks up.
    let gateway_reachable = if status.completed() && has_address {
        net::default_gateway(&paths.iface).map(|gw| net::gateway_reachable(&paths.iface, gw))
    } else {
        None
    };
    Observed {
        configured,
        associated: status.completed(),
        associated_ssid: status.ssid,
        has_address,
        gateway_reachable,
    }
}

fn describe_metrics() {
    describe_gauge!(
        "kamoder_wifi_healthy",
        "1 when associated, addressed and the gateway answers"
    );
    describe_gauge!(
        "kamoder_wifi_attempts",
        "Reassociations since the link was last healthy"
    );
    describe_counter!(
        "kamoder_wifi_actions_total",
        "Reconcile actions taken, by action"
    );
    describe_counter!(
        "kamoder_wifi_action_failures_total",
        "Reconcile actions that failed, by action"
    );
    describe_gauge!(
        "kamoder_wifi_escalation_level",
        "Last recovery step taken: 0 none/healthy, 1 renew, 2 restart, 3 driver reset, 4 USB power cycle"
    );
    describe_gauge!(
        "kamoder_wifi_gateway_failures",
        "Consecutive unanswered gateway checks on an otherwise up link"
    );
    describe_counter!(
        "kamoder_wifi_usb_power_restore_failures_total",
        "Times the power GPIO could not be put back to the USB Wi-Fi power-on level"
    );
}

fn run(config: &Path, paths: &Paths, args: &RunArgs, driver: recovery::Driver) -> Result<()> {
    shutdown::install();
    describe_metrics();
    let policy = args.policy();
    let interval = Duration::from_secs(args.interval.max(1));
    let mut recovery = Recovery::new(driver);

    // A previous run killed in the middle of a power cycle would have left
    // the Wi-Fi module unpowered, and nothing else would ever switch it back.
    if !args.dry_run {
        match recovery.ensure_usb_power_on(&mut SystemOps::new(paths)) {
            Ok(true) => tracing::warn!("restored USB Wi-Fi power left off by an earlier run"),
            Ok(false) => {}
            Err(error) => tracing::error!(%error, "cannot check the USB Wi-Fi power"),
        }
    }

    let mut memory = Memory {
        last_good: system::load_last_good(paths),
        ..Memory::default()
    };
    tracing::info!(
        config = %config.display(),
        wpa_conf = %paths.wpa_conf.display(),
        last_good = ?memory.last_good.as_ref().map(|n| &n.ssid),
        dry_run = args.dry_run,
        "wifi started"
    );

    let mut warned = None;
    let mut was_healthy = None;
    while shutdown::running() {
        let now = Instant::now();
        let want = desired(config, &mut warned);
        let observed = observe(paths);
        let healthy = memory.observe(&observed, &policy, now);
        if healthy && observed.gateway_silent() {
            tracing::warn!(
                misses = memory.gateway_failures,
                threshold = policy.gateway_failures,
                "gateway did not answer; tolerating"
            );
        }

        if was_healthy != Some(healthy) {
            tracing::info!(
                healthy,
                configured = ?observed.configured.as_ref().map(|n| &n.ssid),
                associated = ?observed.associated_ssid,
                address = observed.has_address,
                gateway = ?observed.gateway_reachable,
                "link state"
            );
            was_healthy = Some(healthy);
        }
        gauge!("kamoder_wifi_healthy").set(if healthy { 1.0 } else { 0.0 });
        gauge!("kamoder_wifi_gateway_failures").set(f64::from(memory.gateway_failures));
        if observed.healthy() {
            gauge!("kamoder_wifi_escalation_level").set(0.0);
        }

        let action = reconcile::decide(want.as_ref(), &observed, &memory, &policy, now);
        if action != Action::Wait {
            let label = label(&action);
            tracing::info!(action = label, target = ?target_ssid(&action), attempts = memory.attempts, "reconcile");
            counter!("kamoder_wifi_actions_total", "action" => label).increment(1);
            if let Some(level) = reconcile::escalation_level(&action) {
                gauge!("kamoder_wifi_escalation_level").set(f64::from(level));
            }
            let outcome = if args.dry_run {
                Ok(())
            } else {
                execute(paths, &mut recovery, &action)
            };
            match outcome {
                Ok(()) => memory.performed(&action, observed.configured.as_ref(), &policy, now),
                Err(error) => {
                    counter!("kamoder_wifi_action_failures_total", "action" => label).increment(1);
                    tracing::error!(
                        action = label,
                        error = format!("{error:#}"),
                        "action failed"
                    );
                    // Some failures must still move the state machine on:
                    // * a failed recovery step (renew, reassociation, driver
                    //   reset, power cycle) counts towards the retry budget
                    //   and moves the ladder up, or a broken radio would be
                    //   restarted every pass at the same rung;
                    // * a last-good record that cannot be *saved* (card
                    //   read-only after a power cut, or full) is still true —
                    //   keep it in memory, or the loop repeats `record_good`
                    //   forever and never converges. Found by the qemu tests.
                    if reconcile::escalation_level(&action).is_some()
                        || matches!(action, Action::RecordGood(_))
                    {
                        memory.performed(&action, observed.configured.as_ref(), &policy, now);
                    }
                }
            }
        }
        gauge!("kamoder_wifi_attempts").set(f64::from(memory.attempts));

        sleep_interruptibly(interval);
    }
    tracing::info!("wifi stopping");
    Ok(())
}

fn execute(paths: &Paths, recovery: &mut Recovery, action: &Action) -> Result<()> {
    let started = Instant::now();
    let report = |outcome: Outcome| {
        let elapsed_s = started.elapsed().as_secs();
        match outcome {
            Outcome::Done => {
                tracing::info!(action = label(action), elapsed_s, "recovery step done")
            }
            Outcome::FellBack(reason) => tracing::warn!(
                action = label(action),
                elapsed_s,
                %reason,
                "recovery step not applicable; ran a lower step instead"
            ),
        }
    };
    match action {
        Action::Wait => Ok(()),
        Action::RecordGood(network) => {
            system::save_last_good(paths, network).wrap_err("cannot save last-good network")
        }
        Action::Apply(network) | Action::RollBack(network) => {
            system::write_config(paths, network).wrap_err("cannot write station config")?;
            system::reassociate(paths).wrap_err("cannot restart the station")
        }
        Action::Reassociate => system::reassociate(paths).wrap_err("cannot restart the station"),
        Action::Renew => system::renew(paths).wrap_err("cannot renew the link"),
        Action::ResetDriver => {
            let outcome = recovery
                .reset_driver(&mut SystemOps::new(paths))
                .wrap_err("driver reset failed")?;
            report(outcome);
            Ok(())
        }
        Action::PowerCycle => {
            let outcome = recovery
                .power_cycle(&mut SystemOps::new(paths))
                .wrap_err("USB power cycle failed")?;
            report(outcome);
            Ok(())
        }
    }
}

fn label(action: &Action) -> &'static str {
    match action {
        Action::Wait => "wait",
        Action::RecordGood(_) => "record_good",
        Action::Apply(_) => "apply",
        Action::Reassociate => "reassociate",
        Action::Renew => "renew",
        Action::ResetDriver => "reset_driver",
        Action::PowerCycle => "power_cycle",
        Action::RollBack(_) => "rollback",
    }
}

fn target_ssid(action: &Action) -> Option<&str> {
    match action {
        Action::RecordGood(n) | Action::Apply(n) | Action::RollBack(n) => Some(&n.ssid),
        Action::Wait
        | Action::Renew
        | Action::Reassociate
        | Action::ResetDriver
        | Action::PowerCycle => None,
    }
}

fn sleep_interruptibly(total: Duration) {
    let step = Duration::from_millis(250);
    let mut slept = Duration::ZERO;
    while slept < total && shutdown::running() {
        std::thread::sleep(step);
        slept += step;
    }
}

fn status(config: &Path, paths: &Paths, policy: &Policy) -> Result<()> {
    let mut warned = None;
    let want = desired(config, &mut warned);
    let observed = observe(paths);
    let memory = Memory {
        last_good: system::load_last_good(paths),
        ..Memory::default()
    };
    let ssid = |n: &Option<wpa::Network>| n.as_ref().map_or("-".to_owned(), |n| n.ssid.clone());

    println!(
        "desired     {}",
        want.as_ref()
            .map_or("(keep current)".to_owned(), |n| n.ssid.clone())
    );
    println!("configured  {}", ssid(&observed.configured));
    println!("last good   {}", ssid(&memory.last_good));
    println!(
        "associated  {} ({})",
        observed.associated_ssid.as_deref().unwrap_or("-"),
        if observed.associated {
            "COMPLETED"
        } else {
            "not completed"
        }
    );
    println!(
        "address     {}",
        net::ipv4_address(&paths.iface).map_or("-".to_owned(), |a| a.to_string())
    );
    println!(
        "gateway     {}",
        match (
            net::default_gateway(&paths.iface),
            observed.gateway_reachable
        ) {
            (Some(gw), Some(true)) => format!("{gw} reachable"),
            (Some(gw), Some(false)) => format!("{gw} NOT reachable"),
            (Some(gw), None) => format!("{gw} (not probed)"),
            (None, _) => "-".to_owned(),
        }
    );
    println!("healthy     {}", observed.healthy());

    // What the first pass of `run` would do, ignoring the grace period and
    // the gateway guard.
    let policy = Policy {
        grace: Duration::ZERO,
        gateway_failures: 1,
        ..*policy
    };
    let now = Instant::now();
    let mut memory = memory;
    memory.observe(&observed, &policy, now);
    let action = reconcile::decide(want.as_ref(), &observed, &memory, &policy, now);
    println!(
        "next action {}{}",
        label(&action),
        target_ssid(&action)
            .map(|s| format!(" -> {s}"))
            .unwrap_or_default()
    );
    Ok(())
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
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn bare_invocation_runs_with_defaults() {
        let cli = Cli::parse_from(["wifi"]);
        assert!(cli.command.is_none());
        assert!(!cli.run.dry_run);
        assert_eq!(
            cli.paths.wpa_conf,
            PathBuf::from("/rom/wpa_supplicant0.conf")
        );
        assert_eq!(cli.run.max_attempts, 4);
        assert_eq!(cli.run.gateway_failures, 3);
        // No board profile: exactly the AV100 values from before profiles.
        let hw = hardware::merge(&cli.overrides(), hardware::Board::Unresolved);
        assert_eq!(cli.paths.paths(&hw).iface, "wlan0");
        let driver = cli.driver.driver(&hw);
        assert_eq!(driver.power_gpio, Some(98));
        assert_eq!(driver.module, "ssv6x5x");
        assert_eq!(driver.path, PathBuf::from("/ipc/modules/wifi/ssv6x5x.ko"));
        assert_eq!(
            driver.args,
            ["stacfgpath=/ipc/modules/wifi/ssv6x5x-wifi.cfg"]
        );
        let policy = cli.run.policy();
        assert_eq!(
            (policy.driver_reset_after, policy.power_cycle_after),
            (2, 1)
        );
    }

    #[test]
    fn backoff_max_is_clamped_to_the_ceiling() {
        let cli = Cli::parse_from(["wifi", "--backoff-max", "3600"]);
        assert_eq!(cli.run.policy().backoff_max, reconcile::BACKOFF_CEILING);
    }

    #[test]
    fn supervisor_arguments_parse() {
        let cli = Cli::parse_from([
            "wifi",
            "--config",
            "camera.conf",
            "--state-dir",
            "backup",
            "--dry-run",
        ]);
        assert!(cli.run.dry_run);
        assert_eq!(cli.paths.state_dir, PathBuf::from("backup"));
    }

    #[test]
    fn status_subcommand() {
        let cli = Cli::parse_from(["wifi", "--iface", "wlan1", "status"]);
        assert!(matches!(cli.command, Some(Cmd::Status)));
        assert_eq!(cli.paths.iface.as_deref(), Some("wlan1"));
    }

    #[test]
    fn flags_override_the_board_profile() {
        let wifi = board::Wifi {
            iface: "wlan3".into(),
            module: Some("/lib/8188fu.ko".into()),
            module_args: vec!["x=1".into()],
            power_gpio: None,
        };
        let cli = Cli::parse_from(["wifi"]);
        let hw = hardware::merge(&cli.overrides(), hardware::Board::Wifi(&wifi));
        assert_eq!(cli.paths.paths(&hw).iface, "wlan3");
        let driver = cli.driver.driver(&hw);
        assert_eq!(
            (driver.module.as_str(), driver.power_gpio),
            ("8188fu", None)
        );

        let cli = Cli::parse_from([
            "wifi",
            "--iface",
            "wlan1",
            "--driver-path",
            "/x/ssv6x5x.ko",
            "--driver-args",
            "",
        ]);
        let hw = hardware::merge(&cli.overrides(), hardware::Board::Wifi(&wifi));
        assert_eq!(cli.paths.paths(&hw).iface, "wlan1");
        let driver = cli.driver.driver(&hw);
        assert_eq!(driver.module, "ssv6x5x");
        assert_eq!(driver.path, PathBuf::from("/x/ssv6x5x.ko"));
        assert!(driver.args.is_empty());
    }
}
