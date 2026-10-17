//! When the updater acts by itself (OTA-14, OTA-18): pure decisions, time
//! passed in, so they are table-tested.
//!
//! - **Checks**: the first one [`ScheduleConfig::first_delay`] after start
//!   (2 min, so boot traffic and NTP settle first), and only once the clock
//!   is NTP-valid (TLS certificates cannot be checked against 1970). Then
//!   every [`ScheduleConfig::interval`] (12 h) ± 10 % jitter, so a fleet
//!   rebooted by one power cut does not hit GitHub at the same second; after
//!   a failed check, again after [`ScheduleConfig::retry`].
//! - **Auto-install** (opt-in): a downloaded, verified package, nothing
//!   else running, the running slot committed, and the local hour inside the
//!   maintenance window (none = any time). Tried once per package.

use std::time::Duration;

use update_proto::Settings;

/// Timing of the background loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleConfig {
    /// Delay of the first automatic check after start.
    pub first_delay: Duration,
    /// Time between automatic checks (before jitter).
    pub interval: Duration,
    /// Time before another try after a failed check.
    pub retry: Duration,
    /// How often the loop wakes to look at the clock and the window.
    pub tick: Duration,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            first_delay: Duration::from_secs(120),
            interval: Duration::from_secs(12 * 3600),
            retry: Duration::from_secs(3600),
            tick: Duration::from_secs(30),
        }
    }
}

/// `interval` moved by up to ±10 %, picked by `seed`.
pub fn jittered(interval: Duration, seed: u64) -> Duration {
    let tenth = interval.as_secs() / 10;
    if tenth == 0 {
        return interval;
    }
    let offset = seed % (2 * tenth + 1);
    Duration::from_secs(interval.as_secs() - tenth + offset)
}

/// Everything the auto-install decision looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoInstall<'a> {
    pub settings: &'a Settings,
    /// A verified package is downloaded.
    pub downloaded: bool,
    /// The running slot is still on trial.
    pub trial: bool,
    /// A job is running, or an install already waits for a reboot.
    pub busy: bool,
    /// This package was already tried automatically.
    pub attempted: bool,
    /// Local hour (`0..=23`), `None` while the clock is not set.
    pub local_hour: Option<u8>,
}

/// Whether to install the downloaded package now, unattended.
pub fn auto_install_due(a: &AutoInstall<'_>) -> bool {
    if !a.settings.auto_install || !a.downloaded || a.trial || a.busy || a.attempted {
        return false;
    }
    match a.settings.window {
        None => true,
        Some(window) => a.local_hour.is_some_and(|h| window.contains(h)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use update_proto::MaintenanceWindow;

    #[test]
    fn jitter_stays_within_ten_percent() {
        let interval = Duration::from_secs(43_200);
        let mut seen_low = false;
        let mut seen_high = false;
        for seed in 0..20_000u64 {
            let d = jittered(interval, seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            assert!(d >= Duration::from_secs(38_880) && d <= Duration::from_secs(47_520));
            seen_low |= d < Duration::from_secs(40_000);
            seen_high |= d > Duration::from_secs(46_000);
        }
        assert!(seen_low && seen_high);
        // Tiny intervals (tests) are left alone.
        assert_eq!(jittered(Duration::from_secs(5), 7), Duration::from_secs(5));
    }

    #[test]
    fn auto_install_rules() {
        let on = Settings {
            auto_install: true,
            ..Settings::default()
        };
        let windowed = Settings {
            auto_install: true,
            window: Some(MaintenanceWindow {
                start_hour: 2,
                end_hour: 4,
            }),
            ..Settings::default()
        };
        let off = Settings::default();
        let base = |settings| AutoInstall {
            settings,
            downloaded: true,
            trial: false,
            busy: false,
            attempted: false,
            local_hour: Some(3),
        };
        assert!(auto_install_due(&base(&on)));
        assert!(auto_install_due(&base(&windowed)));
        assert!(!auto_install_due(&base(&off)));
        // (case, due)
        let cases = [
            (
                AutoInstall {
                    local_hour: Some(4),
                    ..base(&windowed)
                },
                false,
            ),
            (
                AutoInstall {
                    local_hour: None,
                    ..base(&windowed)
                },
                false,
            ),
            (
                AutoInstall {
                    local_hour: None,
                    ..base(&on)
                },
                true,
            ),
            (
                AutoInstall {
                    downloaded: false,
                    ..base(&on)
                },
                false,
            ),
            (
                AutoInstall {
                    trial: true,
                    ..base(&on)
                },
                false,
            ),
            (
                AutoInstall {
                    busy: true,
                    ..base(&on)
                },
                false,
            ),
            (
                AutoInstall {
                    attempted: true,
                    ..base(&on)
                },
                false,
            ),
        ];
        for (case, due) in cases {
            assert_eq!(auto_install_due(&case), due, "{case:?}");
        }
    }
}
