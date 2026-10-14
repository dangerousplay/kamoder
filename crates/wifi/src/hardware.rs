//! Which interface, driver and power switch this camera's Wi-Fi has
//! (PER-04).
//!
//! Three sources, per field, first one set wins:
//!
//! ```text
//!   --iface / ANYKA_WIFI_IFACE, --driver-* / ANYKA_WIFI_DRIVER_*   (CLI/env)
//!   [wifi] in the board profile                                     (board)
//!   the AV100 values this service always used                       (built-in)
//! ```
//!
//! The built-in values are used whole only when no profile can be read at
//! all: Wi-Fi is often the only way into the camera, so a broken or missing
//! `boards/` directory on the card must leave the AV100 exactly as it was,
//! power-cycle rung included. A profile that *was* read but has no `[wifi]`
//! section keeps the built-in interface and driver (a driver that is not
//! loaded is never reloaded, see [`crate::recovery`]) but gets no power
//! GPIO: driving a pin the board never declared could switch off something
//! else entirely.
//!
//! [`merge`] is pure so the precedence is unit-tested on the host.

use std::path::{Path, PathBuf};

/// Interface name when neither the CLI nor a profile names one. Also the
/// supervisor's fallback for its ready report.
pub const FALLBACK_IFACE: &str = "wlan0"; // board-literal-ok: AV100 recovery default (no profile readable)
/// The AV100's driver, as `/ipc/setup.sh` loads it.
pub const FALLBACK_DRIVER_PATH: &str = "/ipc/modules/wifi/ssv6x5x.ko"; // board-literal-ok: AV100 recovery default
pub const FALLBACK_DRIVER_ARGS: &str = "stacfgpath=/ipc/modules/wifi/ssv6x5x-wifi.cfg"; // board-literal-ok: AV100 recovery default
/// The AV100's USB Wi-Fi power switch.
pub const FALLBACK_POWER_GPIO: u32 = 98; // board-literal-ok: AV100 recovery default

/// What the board profile said about Wi-Fi.
#[derive(Debug, Clone, Copy)]
pub enum Board<'a> {
    /// No profile could be resolved: use the built-in values, all of them.
    Unresolved,
    /// A profile without a `[wifi]` section.
    NoWifi,
    Wifi(&'a board::Wifi),
}

/// Values given on the command line or in the environment.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub iface: Option<String>,
    /// Module name as `lsmod` shows it.
    pub driver_module: Option<String>,
    pub driver_path: Option<PathBuf>,
    /// Module parameters, space-separated.
    pub driver_args: Option<String>,
}

/// The merged result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hardware {
    pub iface: String,
    /// Module name as `/proc/modules` lists it; empty when there is no
    /// loadable driver (the reload step then only restarts the station).
    pub driver_module: String,
    pub driver_path: PathBuf,
    pub driver_args: Vec<String>,
    /// `None`: the power-cycle step is skipped.
    pub power_gpio: Option<u32>,
}

/// Applies the precedence CLI/env > board > built-in, field by field.
pub fn merge(overrides: &Overrides, board: Board<'_>) -> Hardware {
    let (iface, path, args, power_gpio) = match board {
        Board::Unresolved | Board::NoWifi => (
            FALLBACK_IFACE.to_owned(),
            PathBuf::from(FALLBACK_DRIVER_PATH),
            split_args(FALLBACK_DRIVER_ARGS),
            matches!(board, Board::Unresolved).then_some(FALLBACK_POWER_GPIO),
        ),
        Board::Wifi(wifi) => (
            wifi.iface.clone(),
            wifi.module.clone().unwrap_or_default(),
            wifi.module_args.clone(),
            wifi.power_gpio,
        ),
    };
    let driver_path = overrides.driver_path.clone().unwrap_or(path);
    let driver_module = overrides
        .driver_module
        .clone()
        .unwrap_or_else(|| module_name(&driver_path));
    Hardware {
        iface: overrides.iface.clone().unwrap_or(iface),
        driver_module,
        driver_path,
        driver_args: overrides.driver_args.as_deref().map_or(args, split_args),
        power_gpio,
    }
}

/// The name the kernel gives a module loaded from `path`: the file stem,
/// with `-` read as `_` (as `/proc/modules` lists it). Empty for no path.
pub fn module_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.strip_suffix(".ko").unwrap_or(name).replace('-', "_"))
        .unwrap_or_default()
}

fn split_args(args: &str) -> Vec<String> {
    args.split_whitespace().map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn av100() -> board::Wifi {
        board::Wifi {
            iface: "wlan0".into(),
            module: Some("/ipc/modules/wifi/ssv6x5x.ko".into()),
            module_args: vec!["stacfgpath=/ipc/modules/wifi/ssv6x5x-wifi.cfg".into()],
            power_gpio: Some(98),
        }
    }

    fn builtin() -> Hardware {
        Hardware {
            iface: "wlan0".into(),
            driver_module: "ssv6x5x".into(),
            driver_path: "/ipc/modules/wifi/ssv6x5x.ko".into(),
            driver_args: vec!["stacfgpath=/ipc/modules/wifi/ssv6x5x-wifi.cfg".into()],
            power_gpio: Some(98),
        }
    }

    #[test]
    fn unresolved_board_is_exactly_the_old_defaults() {
        assert_eq!(merge(&Overrides::default(), Board::Unresolved), builtin());
    }

    #[test]
    fn av100_profile_matches_the_old_defaults() {
        let wifi = av100();
        assert_eq!(merge(&Overrides::default(), Board::Wifi(&wifi)), builtin());
    }

    #[test]
    fn shipped_av100_profile_matches_the_old_defaults() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../boards/anyka-av100-ptz.toml");
        let profile = board::load(&path).unwrap();
        let wifi = profile.wifi.as_ref().unwrap();
        assert_eq!(merge(&Overrides::default(), Board::Wifi(wifi)), builtin());
    }

    #[test]
    fn profile_values_are_used() {
        let wifi = board::Wifi {
            iface: "wlan1".into(),
            module: Some("/lib/modules/8188-fu.ko".into()),
            module_args: vec!["rtw_power_mgnt=0".into(), "rtw_ips_mode=0".into()],
            power_gpio: Some(7),
        };
        let hw = merge(&Overrides::default(), Board::Wifi(&wifi));
        assert_eq!(hw.iface, "wlan1");
        assert_eq!(hw.driver_module, "8188_fu");
        assert_eq!(hw.driver_path, PathBuf::from("/lib/modules/8188-fu.ko"));
        assert_eq!(hw.driver_args, ["rtw_power_mgnt=0", "rtw_ips_mode=0"]);
        assert_eq!(hw.power_gpio, Some(7));
    }

    #[test]
    fn profile_without_power_gpio_or_module() {
        let wifi = board::Wifi {
            iface: "wlan0".into(),
            module: None,
            module_args: Vec::new(),
            power_gpio: None,
        };
        let hw = merge(&Overrides::default(), Board::Wifi(&wifi));
        assert_eq!(hw.power_gpio, None);
        assert_eq!(hw.driver_module, "");
        assert!(hw.driver_args.is_empty());
    }

    #[test]
    fn profile_without_wifi_never_drives_a_gpio() {
        let hw = merge(&Overrides::default(), Board::NoWifi);
        assert_eq!(hw.power_gpio, None);
        assert_eq!(
            hw,
            Hardware {
                power_gpio: None,
                ..builtin()
            }
        );
    }

    #[test]
    fn cli_overrides_the_profile() {
        let wifi = av100();
        let overrides = Overrides {
            iface: Some("wlan9".into()),
            driver_module: Some("custom".into()),
            driver_path: Some("/tmp/other.ko".into()),
            driver_args: Some("a=1  b=2".into()),
        };
        let hw = merge(&overrides, Board::Wifi(&wifi));
        assert_eq!(hw.iface, "wlan9");
        assert_eq!(hw.driver_module, "custom");
        assert_eq!(hw.driver_path, PathBuf::from("/tmp/other.ko"));
        assert_eq!(hw.driver_args, ["a=1", "b=2"]);
        // Not a CLI setting: the profile's stays.
        assert_eq!(hw.power_gpio, Some(98));
    }

    #[test]
    fn cli_overrides_the_built_in_values() {
        let overrides = Overrides {
            iface: Some("wlan1".into()),
            driver_path: Some("/tmp/my-drv.ko".into()),
            driver_args: Some(String::new()),
            ..Overrides::default()
        };
        let hw = merge(&overrides, Board::Unresolved);
        assert_eq!(hw.iface, "wlan1");
        // Module name follows the overridden path.
        assert_eq!(hw.driver_module, "my_drv");
        assert!(hw.driver_args.is_empty());
        assert_eq!(hw.power_gpio, Some(98));
    }

    #[test]
    fn module_names() {
        assert_eq!(module_name(Path::new("/a/ssv6x5x.ko")), "ssv6x5x");
        assert_eq!(module_name(Path::new("8188-fu.ko")), "8188_fu");
        assert_eq!(module_name(Path::new("")), "");
    }
}
