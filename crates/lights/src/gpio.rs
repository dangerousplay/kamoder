//! The light GPIOs this service may drive, through sysfs, as listed by the
//! board profile (`[[lights.line]]` in `boards/<name>.toml`).
//!
//! Kernel 4.4 has no GPIO character device (that ABI arrived in 4.8), so the
//! only interface is sysfs. No existing crate fits: `sysfs_gpio` hard-codes
//! `/sys/class/gpio`, and the tests and the itest VM need a fake tree under
//! another root.
//!
//! # Two kinds of line
//!
//! - **Numbered** (`gpio = N`): `<gpio_root>/gpioN`, exported on first use,
//!   taken over through `direction`, then driven through `value`.
//! - **Path-addressed** (`path = "/sys/user-gpio/IR_LED"`): a directory some
//!   kernels export per named line, holding only a `value` file the driver
//!   already owns. Only `value` is written.
//!
//! Either kind has its own polarity (`active_low`); the rest of the crate
//! speaks logical on/off only.
//!
//! # Allowlist
//!
//! The same sysfs tree switches the Wi-Fi module's power (writing it drops
//! the camera off the network — often the only way in), the sensor reset, the
//! motor phases and the reset key. Nothing here can reach those: the typed
//! API only knows [`Line`], a [`Sysfs`] holds exactly the profile's lines,
//! the one raw-number entry point, [`Sysfs::drive`], checks
//! [`Sysfs::allowed`] before it touches any file, and a profile that lists
//! the Wi-Fi power GPIO as a light is refused at load ([`Error::PowerGpio`]).
//!
//! # No glitch on takeover
//!
//! A numbered line is taken over by writing `high` or `low` to its
//! `direction`, which the kernel turns into a single "output at this level"
//! call, rather than `out` followed by a `value` write — `out` would drive
//! the pin low for a moment, which flashes a light or clicks the IR-cut.
//! Later changes write `value` only. Lines are never unexported: an
//! unexported pin floats, and some lights are lit while it does.

use board::{LightLine, Profile};
use lights_proto::Roles;
use snafu::{ensure, ResultExt, Snafu};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// What a line is for. The same type as the protocol's role, so the
/// `supported` set clients see is exactly the set of lines held here.
pub use lights_proto::Role as Line;

/// Where the kernel puts the sysfs GPIO interface when a profile does not
/// say otherwise.
pub const DEFAULT_ROOT: &str = "/sys/class/gpio";

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("GPIO {gpio} is not a light line of this board; refusing to touch it"))]
    NotAllowed { gpio: u32 },

    #[snafu(display("this board has no {line} line"))]
    Unsupported { line: Line },

    /// The profile lists the Wi-Fi power GPIO as a light (PER-03).
    #[snafu(display(
        "board profile maps light {line} to {address}, the Wi-Fi power GPIO {gpio}; \
         refusing to start rather than risk cutting Wi-Fi"
    ))]
    PowerGpio {
        line: Line,
        address: String,
        gpio: u32,
    },

    #[snafu(display("cannot write {value:?} to {}: {source}", path.display()))]
    Write {
        path: PathBuf,
        value: String,
        source: std::io::Error,
    },

    #[snafu(display("GPIO {gpio} did not appear under {} after export", root.display()))]
    NotExported { gpio: u32, root: PathBuf },
}

/// Something that sets light levels. [`Sysfs`] on the camera; tests wrap it
/// to record the order of writes.
pub trait Gpio {
    /// Turns `line` logically on or off; the backend applies the polarity.
    fn set(&mut self, line: Line, on: bool) -> Result<(), Error>;

    /// The lines this backend can drive.
    fn supported(&self) -> Roles;
}

/// How a line is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    /// `<gpio_root>/gpioN`.
    Number(u32),
    /// A directory with a `value` file.
    Path(PathBuf),
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Address::Number(n) => write!(f, "GPIO {n}"),
            Address::Path(path) => write!(f, "{}", path.display()),
        }
    }
}

/// One line of the profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub line: Line,
    pub address: Address,
    /// Logical on drives the pin low.
    pub active_low: bool,
}

impl Pin {
    /// The pin a profile line describes; `None` for a line with neither a
    /// number nor a path (the profile loader rejects those already).
    pub fn from_profile(line: &LightLine) -> Option<Self> {
        let address = match (line.gpio, &line.path) {
            (Some(n), _) => Address::Number(n),
            (None, Some(path)) => Address::Path(path.clone()),
            (None, None) => return None,
        };
        Some(Self {
            line: role(line.role),
            address,
            active_low: line.active_low,
        })
    }
}

/// The protocol role of a profile role.
pub fn role(role: board::Role) -> Line {
    match role {
        board::Role::IrCut => Line::IrCut,
        board::Role::IrLeds => Line::IrLeds,
        board::Role::White => Line::White,
        board::Role::Alarm => Line::Alarm,
        board::Role::Amp => Line::Amp,
    }
}

/// The board's light lines in a sysfs tree.
#[derive(Debug)]
pub struct Sysfs {
    root: PathBuf,
    /// The lines that exist; only these are ever written.
    pins: HashMap<Line, Pin>,
    /// Profile lines missing at start, with the reason.
    unavailable: Vec<(Line, String)>,
    /// Numbered lines already exported and switched to output by this
    /// process.
    taken: HashSet<u32>,
}

impl Sysfs {
    /// The light lines of `profile`. `root_override` (`--gpio-root`)
    /// replaces the profile's `gpio_root`. A profile without `[lights]` gives
    /// a tree with no lines.
    ///
    /// Fails if a line is the Wi-Fi power GPIO; see [`Sysfs::new`].
    pub fn from_profile(profile: &Profile, root_override: Option<&Path>) -> Result<Self, Error> {
        let (root, pins) = match &profile.lights {
            Some(lights) => (
                lights.gpio_root.clone(),
                lights.lines.iter().filter_map(Pin::from_profile).collect(),
            ),
            None => (PathBuf::from(DEFAULT_ROOT), Vec::new()),
        };
        let root = root_override.map_or(root, Path::to_path_buf);
        let power = profile.wifi.as_ref().and_then(|wifi| wifi.power_gpio);
        Self::new(root, pins, power)
    }

    /// `pins` under `root`. Nothing is written here.
    ///
    /// - A pin on `power_gpio` (by number, or a path naming `gpioN`) is an
    ///   error: the whole service refuses to start rather than run with a
    ///   map that could cut Wi-Fi.
    /// - A pin that does not exist (path without a `value` file; number with
    ///   neither `gpioN` nor `export` under `root`) is left out and logged;
    ///   the other lines still work.
    pub fn new(
        root: impl Into<PathBuf>,
        pins: Vec<Pin>,
        power_gpio: Option<u32>,
    ) -> Result<Self, Error> {
        let root = root.into();
        if let Some(power) = power_gpio {
            for pin in &pins {
                let hit = match &pin.address {
                    Address::Number(n) => *n == power,
                    Address::Path(path) => {
                        path.file_name().and_then(|name| name.to_str())
                            == Some(&format!("gpio{power}"))
                    }
                };
                ensure!(
                    !hit,
                    PowerGpioSnafu {
                        line: pin.line,
                        address: pin.address.to_string(),
                        gpio: power,
                    }
                );
            }
        }

        let mut available = HashMap::new();
        let mut unavailable = Vec::new();
        for pin in pins {
            match missing(&root, &pin.address) {
                None => {
                    available.insert(pin.line, pin);
                }
                Some(reason) => {
                    tracing::warn!(line = pin.line.as_str(), %reason, "light line unavailable");
                    unavailable.push((pin.line, reason));
                }
            }
        }
        Ok(Self {
            root,
            pins: available,
            unavailable,
            taken: HashSet::new(),
        })
    }

    /// The root numbered lines live under.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Profile lines that were missing at start, and why.
    pub fn unavailable(&self) -> &[(Line, String)] {
        &self.unavailable
    }

    /// The GPIO numbers this tree may write.
    pub fn allowed(&self) -> impl Iterator<Item = u32> + '_ {
        self.pins.values().filter_map(|pin| match pin.address {
            Address::Number(n) => Some(n),
            Address::Path(_) => None,
        })
    }

    /// Drives GPIO `gpio` to the physical level `high`, exporting it and
    /// making it an output on first use. Refuses any number that is not one
    /// of this board's available numbered light lines before touching a
    /// file.
    pub fn drive(&mut self, gpio: u32, high: bool) -> Result<(), Error> {
        ensure!(self.allowed().any(|n| n == gpio), NotAllowedSnafu { gpio });
        let dir = self.root.join(format!("gpio{gpio}"));
        if self.taken.contains(&gpio) {
            return write(&dir.join("value"), level(high));
        }

        if !dir.is_dir() {
            // EBUSY means another process exported it in between: fine.
            match write(&self.root.join("export"), &gpio.to_string()) {
                Err(Error::Write { source, .. }) if source.raw_os_error() == Some(libc::EBUSY) => {}
                other => other?,
            }
            ensure!(
                dir.is_dir(),
                NotExportedSnafu {
                    gpio,
                    root: self.root.clone()
                }
            );
        }
        // `value` and `direction`'s high/low mean the same level only while
        // the kernel's active_low is 0; polarity is applied here instead.
        // Changing it does not move the pin.
        let active_low = dir.join("active_low");
        if std::fs::read_to_string(&active_low).is_ok_and(|text| text.trim() != "0") {
            write(&active_low, "0")?;
        }
        write(&dir.join("direction"), if high { "high" } else { "low" })?;
        self.taken.insert(gpio);
        Ok(())
    }
}

impl Gpio for Sysfs {
    fn set(&mut self, line: Line, on: bool) -> Result<(), Error> {
        let Some(pin) = self.pins.get(&line) else {
            return UnsupportedSnafu { line }.fail();
        };
        let high = on != pin.active_low;
        match &pin.address {
            Address::Number(n) => {
                let n = *n;
                self.drive(n, high)
            }
            Address::Path(path) => write(&path.join("value"), level(high)),
        }
    }

    fn supported(&self) -> Roles {
        self.pins.keys().copied().collect()
    }
}

/// Why `address` cannot be driven, or `None` if it can.
fn missing(root: &Path, address: &Address) -> Option<String> {
    match address {
        Address::Number(n) => {
            let dir = root.join(format!("gpio{n}"));
            let export = root.join("export");
            (!dir.is_dir() && !export.is_file())
                .then(|| format!("neither {} nor {} exists", dir.display(), export.display()))
        }
        Address::Path(path) => {
            let value = path.join("value");
            (!value.is_file()).then(|| format!("{} does not exist", value.display()))
        }
    }
}

fn level(high: bool) -> &'static str {
    if high {
        "1"
    } else {
        "0"
    }
}

/// Writes `value` to an existing sysfs attribute in one `write(2)`. Never
/// creates the file: on a real tree it always exists, and a missing one in a
/// fake tree is a test failure, not something to paper over.
fn write(path: &Path, value: &str) -> Result<(), Error> {
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .and_then(|mut file| file.write_all(value.as_bytes()))
        .context(WriteSnafu { path, value })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The board this crate was written for, from `boards/`.
    pub(crate) fn av100() -> Profile {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../boards/anyka-av100-ptz.toml");
        board::load(&path).unwrap()
    }

    /// The GPIO numbers of the AV100 light lines.
    pub(crate) fn av100_numbers() -> Vec<u32> {
        av100()
            .lights
            .unwrap()
            .lines
            .iter()
            .filter_map(|line| line.gpio)
            .collect()
    }

    /// The AV100 lines over a fake tree at `root`.
    pub(crate) fn av100_sysfs(root: &Path) -> Sysfs {
        Sysfs::from_profile(&av100(), Some(root)).unwrap()
    }

    /// A fresh directory under the temp dir.
    pub(crate) fn scratch(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "kamoder-lights-gpio-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake `/sys/class/gpio`: `export`/`unexport` files and, for each
    /// number in `exported`, a `gpioN` directory with `direction`, `value`
    /// and `active_low` as the kernel shows a fresh input.
    pub(crate) fn fake_tree(tag: &str, exported: &[u32]) -> PathBuf {
        let root = scratch(tag);
        std::fs::write(root.join("export"), "").unwrap();
        std::fs::write(root.join("unexport"), "").unwrap();
        for gpio in exported {
            add_line(&root, *gpio);
        }
        root
    }

    pub(crate) fn add_line(root: &Path, gpio: u32) {
        let dir = root.join(format!("gpio{gpio}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("direction"), "in\n").unwrap();
        std::fs::write(dir.join("value"), "0\n").unwrap();
        std::fs::write(dir.join("active_low"), "0\n").unwrap();
    }

    pub(crate) fn read(root: &Path, gpio: u32, attribute: &str) -> String {
        std::fs::read_to_string(root.join(format!("gpio{gpio}/{attribute}"))).unwrap()
    }

    /// A `/sys/user-gpio/<name>` style line: a directory with `value` only.
    pub(crate) fn add_named(dir: &Path, name: &str) -> PathBuf {
        let line = dir.join(name);
        std::fs::create_dir_all(&line).unwrap();
        std::fs::write(line.join("value"), "0\n").unwrap();
        line
    }

    fn pin(line: Line, address: Address, active_low: bool) -> Pin {
        Pin {
            line,
            address,
            active_low,
        }
    }

    #[test]
    fn av100_profile_supports_every_role() {
        let root = fake_tree("av100", &av100_numbers());
        let gpio = av100_sysfs(&root);
        assert_eq!(gpio.supported(), Roles::ALL);
        assert!(gpio.unavailable().is_empty());
        let mut allowed: Vec<u32> = gpio.allowed().collect();
        allowed.sort_unstable();
        let mut numbers = av100_numbers();
        numbers.sort_unstable();
        assert_eq!(allowed, numbers);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn takeover_sets_direction_and_level_in_one_write() {
        let root = fake_tree("takeover", &[1, 2]);
        let pins = vec![
            pin(Line::Amp, Address::Number(1), true),
            pin(Line::Alarm, Address::Number(2), false),
        ];
        let mut gpio = Sysfs::new(&root, pins, None).unwrap();
        // Amp is active-low: off = high.
        gpio.set(Line::Amp, false).unwrap();
        gpio.set(Line::Alarm, false).unwrap();
        assert_eq!(read(&root, 1, "direction"), "high");
        assert_eq!(read(&root, 2, "direction"), "low");
        // `value` was not written: the direction write already set the level.
        assert_eq!(read(&root, 1, "value"), "0\n");
        // Already-exported lines are not exported again.
        assert_eq!(std::fs::read_to_string(root.join("export")).unwrap(), "");

        // Later changes go to `value` only.
        gpio.set(Line::Amp, true).unwrap();
        assert_eq!(read(&root, 1, "value"), "0");
        gpio.set(Line::Amp, false).unwrap();
        assert_eq!(read(&root, 1, "value"), "1");
        assert_eq!(read(&root, 1, "direction"), "high");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_low_is_respected_on_both_kinds_of_line() {
        let root = fake_tree("polarity", &[5, 6]);
        let named = scratch("polarity-named");
        let leds = add_named(&named, "IR_LED");
        let cut = add_named(&named, "IR_CUT");
        let pins = vec![
            pin(Line::White, Address::Number(5), false),
            pin(Line::Amp, Address::Number(6), true),
            pin(Line::IrLeds, Address::Path(leds.clone()), true),
            pin(Line::IrCut, Address::Path(cut.clone()), false),
        ];
        let mut gpio = Sysfs::new(&root, pins, None).unwrap();
        for on in [true, false, true] {
            for line in [Line::White, Line::Amp, Line::IrLeds, Line::IrCut] {
                gpio.set(line, on).unwrap();
            }
        }
        // Last write: on.
        assert_eq!(read(&root, 5, "value"), "1");
        assert_eq!(read(&root, 6, "value"), "0");
        let value = |dir: &Path| std::fs::read_to_string(dir.join("value")).unwrap();
        assert_eq!(value(&leds), "0");
        assert_eq!(value(&cut), "1");
        gpio.set(Line::IrLeds, false).unwrap();
        assert_eq!(value(&leds), "1");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(named).unwrap();
    }

    #[test]
    fn path_lines_write_value_only() {
        let root = fake_tree("path-only", &[]);
        let named = scratch("path-only-named");
        let leds = add_named(&named, "IR_LED");
        let pins = vec![pin(Line::IrLeds, Address::Path(leds.clone()), false)];
        let mut gpio = Sysfs::new(&root, pins, None).unwrap();
        gpio.set(Line::IrLeds, true).unwrap();
        assert_eq!(std::fs::read_to_string(leds.join("value")).unwrap(), "1");
        // Nothing else appeared, nothing was exported.
        assert_eq!(std::fs::read_dir(&leds).unwrap().count(), 1);
        assert_eq!(std::fs::read_to_string(root.join("export")).unwrap(), "");
        assert_eq!(gpio.allowed().count(), 0);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(named).unwrap();
    }

    #[test]
    fn exports_a_line_that_is_not_exported_yet() {
        let root = fake_tree("export", &[]);
        let pins = vec![pin(Line::White, Address::Number(7), false)];
        let mut gpio = Sysfs::new(&root, pins, None).unwrap();
        // The fake has no kernel behind it, so the directory never appears;
        // the export write itself must still have happened.
        let error = gpio.set(Line::White, true).unwrap_err();
        assert!(
            matches!(error, Error::NotExported { gpio: 7, .. }),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(root.join("export")).unwrap(), "7");

        // Once the "kernel" has created it, the takeover goes through.
        add_line(&root, 7);
        gpio.set(Line::White, true).unwrap();
        assert_eq!(read(&root, 7, "direction"), "high");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inverted_active_low_is_reset_before_the_takeover() {
        let root = fake_tree("active-low", &[8]);
        std::fs::write(root.join("gpio8/active_low"), "1\n").unwrap();
        let pins = vec![pin(Line::IrLeds, Address::Number(8), false)];
        let mut gpio = Sysfs::new(&root, pins, None).unwrap();
        gpio.set(Line::IrLeds, false).unwrap();
        assert_eq!(read(&root, 8, "active_low"), "0");
        assert_eq!(read(&root, 8, "direction"), "low");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pins_outside_the_profile_are_refused_untouched() {
        let profile = av100();
        let power = profile.wifi.as_ref().unwrap().power_gpio.unwrap();
        let lights = av100_numbers();
        // Wi-Fi power plus everything else on the tree that is not a light.
        let forbidden: Vec<u32> = (0..128).filter(|n| !lights.contains(n)).collect();
        assert!(forbidden.contains(&power));
        let root = fake_tree("allowlist", &forbidden);
        let mut gpio = av100_sysfs(&root);
        for number in forbidden {
            for level in [false, true] {
                let error = gpio.drive(number, level).unwrap_err();
                assert!(matches!(error, Error::NotAllowed { gpio } if gpio == number));
            }
            // Nothing written: still a fresh input.
            assert_eq!(read(&root, number, "direction"), "in\n");
            assert_eq!(read(&root, number, "value"), "0\n");
        }
        assert_eq!(std::fs::read_to_string(root.join("export")).unwrap(), "");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn power_gpio_as_a_light_refuses_to_start() {
        let mut profile = av100();
        let power = profile.wifi.as_ref().unwrap().power_gpio.unwrap();
        let root = fake_tree("power", &[power]);
        let lights = profile.lights.as_mut().unwrap();
        lights.lines[0].gpio = Some(power);
        let error = Sysfs::from_profile(&profile, Some(&root)).unwrap_err();
        assert!(
            matches!(error, Error::PowerGpio { gpio, .. } if gpio == power),
            "{error}"
        );
        assert!(error.to_string().contains("Wi-Fi"), "{error}");

        // Also by path, e.g. `path = "/sys/class/gpio/gpio<power>"`.
        let by_path = root.join(format!("gpio{power}"));
        let pins = vec![pin(Line::White, Address::Path(by_path), false)];
        let error = Sysfs::new(&root, pins, Some(power)).unwrap_err();
        assert!(matches!(error, Error::PowerGpio { .. }), "{error}");

        // Nothing was written either way.
        assert_eq!(read(&root, power, "direction"), "in\n");
        assert_eq!(std::fs::read_to_string(root.join("export")).unwrap(), "");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_lines_are_unavailable_and_the_rest_works() {
        let named = scratch("missing-named");
        let cut = add_named(&named, "IR_CUT");
        // No gpio root at all, and a path that does not exist.
        let root = named.join("no-such-gpio-root");
        let pins = vec![
            pin(Line::IrCut, Address::Path(cut.clone()), false),
            pin(Line::IrLeds, Address::Path(named.join("IR_LED")), false),
            pin(Line::White, Address::Number(7), false),
        ];
        let mut gpio = Sysfs::new(&root, pins, None).unwrap();
        assert_eq!(gpio.supported(), [Line::IrCut].into_iter().collect());
        let missing: Vec<Line> = gpio.unavailable().iter().map(|(line, _)| *line).collect();
        assert_eq!(missing, [Line::IrLeds, Line::White]);
        gpio.set(Line::IrCut, true).unwrap();
        assert!(matches!(
            gpio.set(Line::IrLeds, true),
            Err(Error::Unsupported { line: Line::IrLeds })
        ));
        assert!(!named.join("IR_LED").exists());
        std::fs::remove_dir_all(named).unwrap();
    }

    #[test]
    fn ev_style_profile_with_path_lines() {
        let dir = scratch("ev-profile");
        let user_gpio = dir.join("user-gpio");
        let cut = add_named(&user_gpio, "IR_CUT");
        let leds = add_named(&user_gpio, "IR_LED");
        let toml = format!(
            r#"
name = "ev-test"
[match]
[video]
backend = "test-pattern"
sensors = [0]
[video.stream]
width = 640
height = 480
fps = 10
kbps = 512
gop = 20
profile = "main"
[[lights.line]]
role = "ir_cut"
path = "{}"
[[lights.line]]
role = "ir_leds"
path = "{}"
active_low = true
"#,
            cut.display(),
            leds.display()
        );
        let path = dir.join("ev-test.toml");
        std::fs::write(&path, toml).unwrap();
        let profile = board::load(&path).unwrap();
        let mut gpio = Sysfs::from_profile(&profile, None).unwrap();
        assert_eq!(
            gpio.supported(),
            [Line::IrCut, Line::IrLeds].into_iter().collect()
        );
        assert!(gpio.unavailable().is_empty());
        assert_eq!(gpio.allowed().count(), 0);
        for line in [Line::White, Line::Alarm, Line::Amp] {
            assert!(matches!(
                gpio.set(line, true),
                Err(Error::Unsupported { .. })
            ));
        }
        gpio.set(Line::IrLeds, true).unwrap();
        gpio.set(Line::IrCut, true).unwrap();
        assert_eq!(std::fs::read_to_string(leds.join("value")).unwrap(), "0");
        assert_eq!(std::fs::read_to_string(cut.join("value")).unwrap(), "1");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_profile_without_lights_has_no_lines() {
        let mut profile = av100();
        profile.lights = None;
        let gpio = Sysfs::from_profile(&profile, None).unwrap();
        assert!(gpio.supported().is_empty());
        assert_eq!(gpio.root(), Path::new(DEFAULT_ROOT));
    }

    #[test]
    fn the_root_override_replaces_the_profile_root() {
        let root = fake_tree("override", &[]);
        let gpio = av100_sysfs(&root);
        assert_eq!(gpio.root(), root);
        let gpio = Sysfs::from_profile(&av100(), None).unwrap();
        assert_eq!(gpio.root(), Path::new("/sys/class/gpio"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_missing_attribute_is_an_error_not_a_new_file() {
        let root = fake_tree("missing", &[9]);
        std::fs::remove_file(root.join("gpio9/direction")).unwrap();
        let pins = vec![pin(Line::IrCut, Address::Number(9), false)];
        let error = Sysfs::new(&root, pins, None)
            .unwrap()
            .set(Line::IrCut, false)
            .unwrap_err();
        assert!(matches!(error, Error::Write { .. }), "{error}");
        assert!(!root.join("gpio9/direction").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
