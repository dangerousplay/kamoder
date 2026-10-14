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

/// Something a board may or may not have, named by a service's `when` key
/// (PKG-02). A service whose capability the resolved board lacks is not
/// started, so a camera without a motor never runs (and restart-loops) the
/// PTZ service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum Capability {
    /// The profile has a `[ptz]` section.
    Ptz,
    /// The profile has a `[lights]` section.
    Lights,
    /// The profile has a `[wifi]` section.
    Wifi,
    /// The profile names a capture backend.
    Video,
    /// The capture backend produces encoded video, i.e. it is not the
    /// `test-pattern` stand-in.
    VideoEncoder,
}

impl Capability {
    const ALL: [Capability; 5] = [
        Capability::Ptz,
        Capability::Lights,
        Capability::Wifi,
        Capability::Video,
        Capability::VideoEncoder,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Ptz => "ptz",
            Capability::Lights => "lights",
            Capability::Wifi => "wifi",
            Capability::Video => "video",
            Capability::VideoEncoder => "video.encoder",
        }
    }

    /// Whether `profile` provides this capability.
    pub fn is_met(self, profile: &board::Profile) -> bool {
        let backend = profile.video.backend.trim();
        match self {
            Capability::Ptz => profile.ptz.is_some(),
            Capability::Lights => profile.lights.is_some(),
            Capability::Wifi => profile.wifi.is_some(),
            Capability::Video => !backend.is_empty(),
            Capability::VideoEncoder => !backend.is_empty() && backend != "test-pattern",
        }
    }
}

impl TryFrom<String> for Capability {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Capability::ALL
            .into_iter()
            .find(|capability| capability.as_str() == value)
            .ok_or_else(|| {
                let known: Vec<_> = Capability::ALL.iter().map(|c| c.as_str()).collect();
                format!(
                    "unknown capability {value:?} in `when`; expected one of {}",
                    known.join(", ")
                )
            })
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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
    /// Start only on boards that have this capability. Ignored (the service
    /// starts) when no board could be resolved, which keeps the behaviour
    /// from before board profiles existed.
    #[serde(default)]
    pub when: Option<Capability>,

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
            // A board problem must never cost the board its critical
            // services (the watchdog feeder above all), so they cannot be
            // made conditional on the board.
            ensure!(
                !service.critical || service.when.is_none(),
                InvalidSnafu {
                    path,
                    reason: format!(
                        "service {:?} is critical and must not have `when`",
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

    /// Disables every service whose `when` capability `board` lacks and
    /// returns them with the missing capability. With no board (resolution
    /// failed) nothing is disabled.
    pub fn apply_board(&mut self, board: Option<&board::Profile>) -> Vec<(String, Capability)> {
        let Some(profile) = board else {
            return Vec::new();
        };
        let mut skipped = Vec::new();
        for service in &mut self.services {
            if let Some(capability) = service.when {
                if service.enabled && !capability.is_met(profile) {
                    service.enabled = false;
                    skipped.push((service.name.clone(), capability));
                }
            }
        }
        skipped
    }

    /// Gives every service `key=value` in its environment, unless the
    /// service's own `env` sets `key` (those entries are applied later and
    /// so win anyway; this keeps the list free of duplicates).
    pub fn inject_env(&mut self, key: &str, value: &str) {
        for service in &mut self.services {
            if !service.env.iter().any(|(k, _)| k == key) {
                service.env.insert(0, (key.to_owned(), value.to_owned()));
            }
        }
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
            when: None,
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

    fn profile(name: &str) -> board::Profile {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../boards")
            .join(format!("{name}.toml"));
        board::load(&path).unwrap()
    }

    const GATED: &str = r#"
        [[service]]
        name = "wdt"
        command = "bin/wdt"
        critical = true
        [[service]]
        name = "ptz"
        command = "bin/ptz"
        when = "ptz"
        [[service]]
        name = "lights"
        command = "bin/lights"
        when = "lights"
        [[service]]
        name = "wifi"
        command = "bin/wifi"
        when = "wifi"
        [[service]]
        name = "camera"
        command = "bin/camera"
        when = "video"
        [[service]]
        name = "rtsp"
        command = "bin/rtsp"
        when = "video.encoder"
        [[service]]
        name = "web"
        command = "bin/web"
    "#;

    fn enabled(config: &Config) -> Vec<&str> {
        config.enabled_services().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn when_parses_every_capability() {
        let config = load(GATED).unwrap();
        let when: Vec<_> = config.services.iter().map(|s| s.when).collect();
        assert_eq!(
            when,
            [
                None,
                Some(Capability::Ptz),
                Some(Capability::Lights),
                Some(Capability::Wifi),
                Some(Capability::Video),
                Some(Capability::VideoEncoder),
                None,
            ]
        );
    }

    #[test]
    fn an_unknown_capability_is_a_load_error() {
        let error = load(
            r#"
            [[service]]
            name = "x"
            command = "bin/x"
            when = "zoom"
            "#,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("zoom"), "{message}");
        assert!(message.contains("video.encoder"), "{message}");
    }

    #[test]
    fn a_critical_service_cannot_be_conditional() {
        let error = load(
            r#"
            [[service]]
            name = "wdt"
            command = "bin/wdt"
            critical = true
            when = "wifi"
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("when"), "{error}");
    }

    #[test]
    fn the_full_board_starts_everything() {
        let mut config = load(GATED).unwrap();
        let skipped = config.apply_board(Some(&profile("anyka-av100-ptz")));
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(
            enabled(&config),
            ["wdt", "ptz", "lights", "wifi", "camera", "rtsp", "web"]
        );
    }

    #[test]
    fn missing_capabilities_skip_their_services() {
        let mut config = load(GATED).unwrap();
        let mut generic = profile("generic-v4l2");
        generic.ptz = None;
        generic.lights = None;
        generic.wifi = None;
        let skipped = config.apply_board(Some(&generic));
        assert_eq!(
            skipped,
            [
                ("ptz".to_owned(), Capability::Ptz),
                ("lights".to_owned(), Capability::Lights),
                ("wifi".to_owned(), Capability::Wifi),
                ("rtsp".to_owned(), Capability::VideoEncoder),
            ]
        );
        // The test pattern is still video, just not encoded video.
        assert_eq!(enabled(&config), ["wdt", "camera", "web"]);
    }

    #[test]
    fn no_backend_means_no_video() {
        let mut config = load(GATED).unwrap();
        let mut bare = profile("anyka-av100-ptz");
        bare.video.backend = String::new();
        config.apply_board(Some(&bare));
        assert!(!enabled(&config).contains(&"camera"));
        assert!(!enabled(&config).contains(&"rtsp"));
    }

    #[test]
    fn an_unresolved_board_starts_conditional_services() {
        let mut config = load(GATED).unwrap();
        assert!(config.apply_board(None).is_empty());
        assert_eq!(enabled(&config).len(), 7);
    }

    #[test]
    fn a_disabled_service_is_not_reported_as_skipped() {
        let mut config = load(
            r#"
            [[service]]
            name = "ptz"
            command = "bin/ptz"
            when = "ptz"
            enabled = false
            "#,
        )
        .unwrap();
        let mut generic = profile("generic-v4l2");
        generic.ptz = None;
        assert!(config.apply_board(Some(&generic)).is_empty());
    }

    #[test]
    fn injected_env_yields_to_the_services_own() {
        let mut config = load(
            r#"
            [[service]]
            name = "a"
            command = "bin/a"
            env = [["X", "1"]]
            [[service]]
            name = "b"
            command = "bin/b"
            env = [["KAMODER_BOARD", "/custom.json"], ["Y", "2"]]
            "#,
        )
        .unwrap();
        config.inject_env("KAMODER_BOARD", "/tmp/kamoder-board.json");
        assert_eq!(
            config.services[0].env,
            [
                (
                    "KAMODER_BOARD".to_owned(),
                    "/tmp/kamoder-board.json".to_owned()
                ),
                ("X".to_owned(), "1".to_owned()),
            ]
        );
        assert_eq!(
            config.services[1].env,
            [
                ("KAMODER_BOARD".to_owned(), "/custom.json".to_owned()),
                ("Y".to_owned(), "2".to_owned()),
            ]
        );
    }

    /// The configuration shipped in `tools/` must keep loading as the
    /// schema grows.
    #[test]
    fn the_shipped_config_loads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/supervisor.toml");
        let config = Config::load(&path).unwrap();
        assert!(config.enabled_services().any(|s| s.critical));
    }

    fn shipped() -> Config {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/supervisor.toml");
        Config::load(&path).unwrap()
    }

    const SHIPPED_SERVICES: [&str; 9] = [
        "wdt_feeder",
        "lights",
        "wifi",
        "ntp",
        "sshd",
        "web",
        "onvif",
        "rtsp",
        "snapshot",
    ];

    /// The AV100 runs every shipped service, in today's order (PKG-02).
    #[test]
    fn the_shipped_config_runs_everything_on_the_av100() {
        let mut config = shipped();
        let skipped = config.apply_board(Some(&profile("anyka-av100-ptz")));
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(enabled(&config), SHIPPED_SERVICES);
    }

    /// Without a board (boards/ missing or broken) nothing is skipped.
    #[test]
    fn the_shipped_config_runs_everything_without_a_board() {
        let mut config = shipped();
        assert!(config.apply_board(None).is_empty());
        assert_eq!(enabled(&config), SHIPPED_SERVICES);
    }

    /// On the generic fallback: no lights (unknown GPIOs) and no rtsp (the
    /// test pattern has no encoder); the feeder and Wi-Fi always run.
    #[test]
    fn the_shipped_config_on_the_generic_board() {
        let mut config = shipped();
        let skipped = config.apply_board(Some(&profile("generic-v4l2")));
        let skipped: Vec<_> = skipped
            .iter()
            .map(|(s, c)| (s.as_str(), c.as_str()))
            .collect();
        assert_eq!(skipped, [("lights", "lights"), ("rtsp", "video.encoder")]);
        assert_eq!(
            enabled(&config),
            [
                "wdt_feeder",
                "wifi",
                "ntp",
                "sshd",
                "web",
                "onvif",
                "snapshot"
            ]
        );
        // Wi-Fi is the only way in: it must never depend on the board.
        let wifi = config.services.iter().find(|s| s.name == "wifi").unwrap();
        assert!(wifi.when.is_none());
        // Every board needs a clock; NTP_ENABLED=0 exits 0, which must be
        // final rather than restarted (NTP-05).
        let ntp = config.services.iter().find(|s| s.name == "ntp").unwrap();
        assert!(ntp.when.is_none());
        assert_eq!(ntp.restart, Restart::OnFailure);
    }
}
