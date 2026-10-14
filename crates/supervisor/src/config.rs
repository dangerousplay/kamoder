//! Supervisor configuration, loaded from TOML.

use serde::Deserialize;
use snafu::{ensure, ResultExt, Snafu};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot read {}: {source}", path.display()))]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("cannot parse {}: {source}", path.display()))]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[snafu(display("{}: {reason}", path.display()))]
    Invalid { path: PathBuf, reason: String },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What to do when a supervised process exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    /// Restart whatever the exit status was. Right for long-running daemons.
    #[default]
    Always,
    /// Restart only on a non-zero exit or a signal. Right for anything that
    /// legitimately finishes.
    OnFailure,
    /// Never restart. Right for one-shot setup steps.
    Never,
}

impl Restart {
    /// Whether an exit with this outcome should be followed by a restart.
    pub fn should_restart(self, success: bool) -> bool {
        match self {
            Restart::Always => true,
            Restart::OnFailure => !success,
            Restart::Never => false,
        }
    }
}

/// One supervised process.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// Identifier used in logs, metrics and the log file name.
    pub name: String,
    /// Executable. A relative path is resolved against the config's directory.
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment entries, on top of the supervisor's own.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Working directory. Defaults to the config's directory.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub restart: Restart,
    /// Refuse `stop` and `restart` through the control socket. Set on the
    /// watchdog feeder: stopping it reboots the board a few seconds later, so
    /// no web button should be able to do that by accident.
    #[serde(default)]
    pub critical: bool,

    /// Delay before the first restart attempt.
    #[serde(default = "default_backoff_initial_ms")]
    pub backoff_initial_ms: u64,
    /// Ceiling for the exponential backoff.
    #[serde(default = "default_backoff_max_ms")]
    pub backoff_max_ms: u64,
    /// Restarts allowed inside `restart_window_secs` before the service is
    /// given up on. Without this a service that dies instantly would rewrite
    /// the SD card forever.
    #[serde(default = "default_max_restarts")]
    pub max_restarts: u32,
    #[serde(default = "default_restart_window_secs")]
    pub restart_window_secs: u64,
    /// How long to wait after `SIGTERM` before sending `SIGKILL`.
    #[serde(default = "default_stop_timeout_secs")]
    pub stop_timeout_secs: u64,
}

impl Service {
    pub fn backoff_initial(&self) -> Duration {
        Duration::from_millis(self.backoff_initial_ms.max(1))
    }

    pub fn backoff_max(&self) -> Duration {
        Duration::from_millis(self.backoff_max_ms.max(self.backoff_initial_ms.max(1)))
    }

    pub fn restart_window(&self) -> Duration {
        Duration::from_secs(self.restart_window_secs.max(1))
    }

    pub fn stop_timeout(&self) -> Duration {
        Duration::from_secs(self.stop_timeout_secs.max(1))
    }
}

/// Supervisor-wide settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Directory for the per-service log files. On the SD card by default.
    #[serde(default = "default_log_dir")]
    pub log_dir: PathBuf,
    /// Rotate a log once it passes this size.
    #[serde(default = "default_log_max_bytes")]
    pub log_max_bytes: usize,
    /// How many rotated generations to keep per service.
    #[serde(default = "default_log_keep")]
    pub log_keep: usize,
    /// How long to wait for every service to stop before exiting anyway.
    #[serde(default = "default_shutdown_timeout_secs")]
    pub shutdown_timeout_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            log_dir: default_log_dir(),
            log_max_bytes: default_log_max_bytes(),
            log_keep: default_log_keep(),
            shutdown_timeout_secs: default_shutdown_timeout_secs(),
        }
    }
}

impl Settings {
    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_secs(self.shutdown_timeout_secs.max(1))
    }
}

/// The whole configuration file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub supervisor: Settings,
    #[serde(default, rename = "service")]
    pub services: Vec<Service>,
    /// Directory the config was loaded from; relative paths resolve here.
    #[serde(skip)]
    pub base_dir: PathBuf,
}

impl Config {
    /// Loads and validates a configuration file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).context(ReadSnafu { path })?;
        let mut config: Config = toml::from_str(&text).context(ParseSnafu { path })?;
        config.base_dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        config.validate(path)?;
        Ok(config)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        let mut seen = HashSet::new();
        for service in &self.services {
            ensure!(
                !service.name.is_empty(),
                InvalidSnafu {
                    path,
                    reason: "a service has an empty name"
                }
            );
            // The name becomes a file name and a metric label.
            ensure!(
                service
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
                InvalidSnafu {
                    path,
                    reason: format!(
                        "service name {:?} must be alphanumeric, '-', '_' or '.'",
                        service.name
                    )
                }
            );
            // A critical service is one the board cannot run without, so
            // leaving it switched off is almost certainly a mistake — and the
            // control socket would refuse to stop it once started anyway.
            ensure!(
                !service.critical || service.enabled,
                InvalidSnafu {
                    path,
                    reason: format!(
                        "service {:?} is critical and must not be disabled",
                        service.name
                    )
                }
            );
            ensure!(
                seen.insert(service.name.as_str()),
                InvalidSnafu {
                    path,
                    reason: format!("duplicate service name {:?}", service.name)
                }
            );
        }
        ensure!(
            self.supervisor.log_max_bytes > 0,
            InvalidSnafu {
                path,
                reason: "log_max_bytes must be greater than 0"
            }
        );
        Ok(())
    }

    /// Services that are switched on, in declaration order.
    ///
    /// Order matters: the watchdog feeder is normally first, so the timer is
    /// being fed before anything slower starts.
    pub fn enabled_services(&self) -> impl Iterator<Item = &Service> {
        self.services.iter().filter(|service| service.enabled)
    }

    /// Resolves a service's executable against the config directory.
    pub fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_dir.join(path)
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_backoff_initial_ms() -> u64 {
    500
}
fn default_backoff_max_ms() -> u64 {
    30_000
}
fn default_max_restarts() -> u32 {
    10
}
fn default_restart_window_secs() -> u64 {
    60
}
fn default_stop_timeout_secs() -> u64 {
    10
}
fn default_log_dir() -> PathBuf {
    PathBuf::from("logs")
}
fn default_log_max_bytes() -> usize {
    1024 * 1024
}
fn default_log_keep() -> usize {
    5
}
fn default_shutdown_timeout_secs() -> u64 {
    15
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_policies() {
        assert!(Restart::Always.should_restart(true));
        assert!(Restart::Always.should_restart(false));
        assert!(!Restart::OnFailure.should_restart(true));
        assert!(Restart::OnFailure.should_restart(false));
        assert!(!Restart::Never.should_restart(false));
    }

    #[test]
    fn backoff_max_is_never_below_initial() {
        let service = Service {
            name: "x".into(),
            command: "x".into(),
            args: vec![],
            env: vec![],
            working_dir: None,
            enabled: true,
            restart: Restart::Always,
            critical: false,
            backoff_initial_ms: 5_000,
            backoff_max_ms: 100,
            max_restarts: 1,
            restart_window_secs: 1,
            stop_timeout_secs: 1,
        };
        assert_eq!(service.backoff_max(), Duration::from_millis(5_000));
    }

    fn load(text: &str) -> Result<Config> {
        let dir = std::env::temp_dir().join(format!(
            "anyka-supervisor-config-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("supervisor.toml");
        std::fs::write(&path, text).unwrap();
        let result = Config::load(&path);
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn critical_defaults_to_false_and_parses() {
        let config = load(
            r#"
            [[service]]
            name = "a"
            command = "bin/a"
            [[service]]
            name = "wdt"
            command = "bin/wdt"
            critical = true
            "#,
        )
        .unwrap();
        assert!(!config.services[0].critical);
        assert!(config.services[1].critical);
    }

    #[test]
    fn a_disabled_critical_service_is_rejected() {
        let error = load(
            r#"
            [[service]]
            name = "wdt"
            command = "bin/wdt"
            critical = true
            enabled = false
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("critical"), "{error}");
    }
}
