//! The five GPIOs this service may drive, through sysfs.
//!
//! Kernel 4.4 has no GPIO character device (that ABI arrived in 4.8), so the
//! only interface is `/sys/class/gpio`. No existing crate fits: `sysfs_gpio`
//! hard-codes `/sys/class/gpio`, and the tests and the itest VM need a fake
//! tree under another root.
//!
//! # Allowlist
//!
//! The same sysfs tree switches the USB Wi-Fi module's power (GPIO 98 —
//! writing it drops the camera off the network), the sensor reset (18), the
//! motor phases and the reset key. Nothing here can reach those: the typed
//! API only knows [`Line`], and the one raw-number entry point,
//! [`Sysfs::drive`], checks [`ALLOWED`] before it touches any file.
//!
//! # No glitch on takeover
//!
//! A line is taken over by writing `high` or `low` to its `direction`, which
//! the kernel turns into a single "output at this level" call, rather than
//! `out` followed by a `value` write — `out` would drive the pin low for a
//! moment, which flashes a light or clicks the IR-cut. Later changes write
//! `value` only. Lines are never unexported: an unexported pin floats, and
//! the red/blue lights are lit while it does.

use snafu::{ensure, ResultExt, Snafu};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where the kernel puts the sysfs GPIO interface.
pub const DEFAULT_ROOT: &str = "/sys/class/gpio";

/// Every GPIO this crate may write. Anything else is refused.
pub const ALLOWED: [u32; 5] = [19, 37, 48, 67, 68];

/// A GPIO this service owns. The numbers are the sysfs numbers, live-confirmed
/// on the AV100.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Line {
    /// IR-cut filter, a held level: low = filter in (day), high = out (night).
    IrCut,
    /// IR illuminator LEDs, active-high.
    IrLeds,
    /// White light, active-high.
    White,
    /// Red and blue lights, active-high.
    Alarm,
    /// Speaker amplifier enable, active-low: high = muted.
    Amp,
}

impl Line {
    /// Every line, in the order they are reported.
    pub const ALL: [Line; 5] = [
        Line::IrCut,
        Line::IrLeds,
        Line::White,
        Line::Alarm,
        Line::Amp,
    ];

    /// The sysfs GPIO number.
    pub fn gpio(self) -> u32 {
        match self {
            Line::IrCut => 19,
            Line::IrLeds => 67,
            Line::White => 68,
            Line::Alarm => 48,
            Line::Amp => 37,
        }
    }

    /// Short name, for logs and metrics labels.
    pub fn name(self) -> &'static str {
        match self {
            Line::IrCut => "ir_cut",
            Line::IrLeds => "ir_leds",
            Line::White => "white",
            Line::Alarm => "alarm",
            Line::Amp => "amp",
        }
    }
}

impl TryFrom<u32> for Line {
    type Error = Error;

    fn try_from(gpio: u32) -> Result<Self, Error> {
        Line::ALL
            .into_iter()
            .find(|line| line.gpio() == gpio)
            .ok_or(Error::NotAllowed { gpio })
    }
}

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display(
        "GPIO {gpio} is not one of the lights GPIOs {ALLOWED:?}; refusing to touch it"
    ))]
    NotAllowed { gpio: u32 },

    #[snafu(display("cannot write {value:?} to {}: {source}", path.display()))]
    Write {
        path: PathBuf,
        value: String,
        source: std::io::Error,
    },

    #[snafu(display("GPIO {gpio} did not appear under {} after export", root.display()))]
    NotExported { gpio: u32, root: PathBuf },
}

/// Something that sets physical GPIO levels. [`Sysfs`] on the camera; tests
/// wrap it to record the order of writes.
pub trait Gpio {
    /// Drives `line` to the physical level `high`.
    fn set(&mut self, line: Line, high: bool) -> Result<(), Error>;
}

/// The sysfs GPIO tree at a configurable root.
#[derive(Debug)]
pub struct Sysfs {
    root: PathBuf,
    /// Lines already exported and switched to output by this process.
    taken: HashSet<u32>,
}

impl Sysfs {
    /// The tree at `root` (normally [`DEFAULT_ROOT`]). Nothing is touched
    /// until a line is first driven.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            taken: HashSet::new(),
        }
    }

    /// The root this tree lives at.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Drives GPIO `gpio` to `high`, exporting it and making it an output on
    /// first use. Refuses any number outside [`ALLOWED`] before touching a
    /// file.
    pub fn drive(&mut self, gpio: u32, high: bool) -> Result<(), Error> {
        ensure!(ALLOWED.contains(&gpio), NotAllowedSnafu { gpio });
        let dir = self.root.join(format!("gpio{gpio}"));
        if self.taken.contains(&gpio) {
            return write(&dir.join("value"), if high { "1" } else { "0" });
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
        // active_low is 0. Changing it does not move the pin.
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
    fn set(&mut self, line: Line, high: bool) -> Result<(), Error> {
        self.drive(line.gpio(), high)
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

    /// A fake `/sys/class/gpio`: `export`/`unexport` files and, for each
    /// number in `exported`, a `gpioN` directory with `direction`, `value`
    /// and `active_low` as the kernel shows a fresh input.
    pub(crate) fn fake_tree(tag: &str, exported: &[u32]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("kamoder-lights-gpio-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
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

    #[test]
    fn takeover_sets_direction_and_level_in_one_write() {
        let root = fake_tree("takeover", &ALLOWED);
        let mut gpio = Sysfs::new(&root);
        gpio.set(Line::Amp, true).unwrap();
        gpio.set(Line::Alarm, false).unwrap();
        assert_eq!(read(&root, 37, "direction"), "high");
        assert_eq!(read(&root, 48, "direction"), "low");
        // `value` was not written: the direction write already set the level.
        assert_eq!(read(&root, 37, "value"), "0\n");
        // Already-exported lines are not exported again.
        assert_eq!(std::fs::read_to_string(root.join("export")).unwrap(), "");

        // Later changes go to `value` only.
        gpio.set(Line::Amp, false).unwrap();
        assert_eq!(read(&root, 37, "value"), "0");
        gpio.set(Line::Amp, true).unwrap();
        assert_eq!(read(&root, 37, "value"), "1");
        assert_eq!(read(&root, 37, "direction"), "high");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exports_a_line_that_is_not_exported_yet() {
        let root = fake_tree("export", &[]);
        let mut gpio = Sysfs::new(&root);
        // The fake has no kernel behind it, so the directory never appears;
        // the export write itself must still have happened.
        let error = gpio.set(Line::White, true).unwrap_err();
        assert!(
            matches!(error, Error::NotExported { gpio: 68, .. }),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(root.join("export")).unwrap(), "68");

        // Once the "kernel" has created it, the takeover goes through.
        add_line(&root, 68);
        gpio.set(Line::White, true).unwrap();
        assert_eq!(read(&root, 68, "direction"), "high");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inverted_active_low_is_reset_before_the_takeover() {
        let root = fake_tree("active-low", &[67]);
        std::fs::write(root.join("gpio67/active_low"), "1\n").unwrap();
        Sysfs::new(&root).set(Line::IrLeds, false).unwrap();
        assert_eq!(read(&root, 67, "active_low"), "0");
        assert_eq!(read(&root, 67, "direction"), "low");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pins_outside_the_allowlist_are_refused_untouched() {
        // Wi-Fi power, sensor reset, reset key, motor phases.
        let forbidden = [98, 18, 56, 26, 55, 57, 58, 59, 65, 66, 69, 0, 20, 36, 99];
        let root = fake_tree("allowlist", &forbidden);
        let mut gpio = Sysfs::new(&root);
        for number in forbidden {
            for level in [false, true] {
                let error = gpio.drive(number, level).unwrap_err();
                assert!(matches!(error, Error::NotAllowed { gpio } if gpio == number));
            }
            assert!(Line::try_from(number).is_err());
            // Nothing written: still a fresh input.
            assert_eq!(read(&root, number, "direction"), "in\n");
            assert_eq!(read(&root, number, "value"), "0\n");
        }
        assert_eq!(std::fs::read_to_string(root.join("export")).unwrap(), "");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lines_are_exactly_the_allowlist() {
        let mut numbers: Vec<u32> = Line::ALL.iter().map(|line| line.gpio()).collect();
        numbers.sort_unstable();
        assert_eq!(numbers, ALLOWED);
        for line in Line::ALL {
            assert_eq!(Line::try_from(line.gpio()).unwrap(), line);
        }
    }

    #[test]
    fn a_missing_attribute_is_an_error_not_a_new_file() {
        let root = fake_tree("missing", &[19]);
        std::fs::remove_file(root.join("gpio19/direction")).unwrap();
        let error = Sysfs::new(&root).set(Line::IrCut, false).unwrap_err();
        assert!(matches!(error, Error::Write { .. }), "{error}");
        assert!(!root.join("gpio19/direction").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
