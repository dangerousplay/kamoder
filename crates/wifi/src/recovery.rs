//! The two hardware rungs of the recovery ladder: reloading the Wi-Fi driver
//! and power-cycling the USB Wi-Fi module.
//!
//! Restarting wpa_supplicant cannot help when the kernel side is stuck. On
//! the AV100 the SSV6x5x USB driver can wedge under heavy traffic, and the
//! camera then stays off the network until it is power-cycled. These steps
//! do the same things the firmware does at boot (`/ipc/setup.sh`): `insmod`
//! the driver with its config file, raise `wlan0`, and switch the module's
//! USB power on GPIO 98.
//!
//! The sequences are written against [`Ops`], so their order and failure
//! paths are unit-tested with a fake, including the one rule that must
//! never break: **once GPIO 98 has been switched off, it is switched back
//! on**, whatever fails after that.

use std::path::PathBuf;
use std::time::Duration;

/// The only GPIO this crate can write: the USB Wi-Fi module's power switch.
/// It is not configurable on purpose.
pub const USB_POWER_GPIO: u32 = 98;

/// What to reload, and how long each step may take. Defaults are the AV100
/// firmware's (`/ipc/setup.sh`, `lsmod`).
#[derive(Debug, Clone)]
pub struct Driver {
    /// Module name as `/proc/modules` and `rmmod` know it.
    pub module: String,
    /// The `.ko` to `insmod`.
    pub path: PathBuf,
    /// Module parameters, passed through unchanged.
    pub args: Vec<String>,
    /// File holding the GPIO 98 level that powers the module on (`0`/`1`),
    /// written by `setup.sh`. Missing means `1`, as in `setup.sh`.
    pub power_on_file: PathBuf,
    /// MTU `setup.sh` sets on the fresh interface.
    pub mtu: u32,
    pub rmmod_timeout: Duration,
    pub insmod_timeout: Duration,
    /// How long `wlan0` may take to appear after `insmod`.
    pub iface_timeout: Duration,
    /// How long the module is left unpowered. At least 2 s.
    pub power_off: Duration,
    /// How long the module may take to enumerate on the USB bus.
    pub usb_timeout: Duration,
}

impl Default for Driver {
    fn default() -> Self {
        Self {
            module: "ssv6x5x".into(),
            path: "/ipc/modules/wifi/ssv6x5x.ko".into(),
            args: vec!["stacfgpath=/ipc/modules/wifi/ssv6x5x-wifi.cfg".into()],
            power_on_file: "/rom/usb_module_power_on".into(),
            mtu: 1460,
            rmmod_timeout: Duration::from_secs(20),
            insmod_timeout: Duration::from_secs(30),
            iface_timeout: Duration::from_secs(15),
            power_off: Duration::from_secs(3),
            usb_timeout: Duration::from_secs(20),
        }
    }
}

/// The side effects the sequences need. [`crate::system::SystemOps`] on the
/// camera; a recording fake in the tests.
pub trait Ops {
    /// Stops wpa_supplicant and the DHCP client and takes the interface down.
    fn stop_station(&mut self);
    /// Whether `/proc/modules` lists `module`.
    fn module_loaded(&mut self, module: &str) -> bool;
    fn rmmod(&mut self, module: &str, timeout: Duration) -> std::io::Result<()>;
    fn insmod(&mut self, driver: &Driver) -> std::io::Result<()>;
    /// Waits for the interface to exist; `false` on timeout.
    fn wait_for_iface(&mut self, timeout: Duration) -> bool;
    /// Sets the MTU and raises the interface.
    fn iface_up(&mut self, mtu: u32) -> std::io::Result<()>;
    /// The firmware's station start (wpa_supplicant + udhcpc).
    fn restart_station(&mut self) -> std::io::Result<()>;
    /// The level that powers the module on, from `driver.power_on_file`.
    fn power_on_level(&mut self, driver: &Driver) -> std::io::Result<bool>;
    /// GPIO 98 is exported and an output — i.e. `setup.sh` set it up. It is
    /// never exported or re-directed here.
    fn usb_power_ready(&mut self) -> bool;
    fn read_usb_power(&mut self) -> std::io::Result<bool>;
    fn write_usb_power(&mut self, high: bool) -> std::io::Result<()>;
    /// Waits for a device on the USB bus (not a root hub); `false` on timeout.
    fn wait_for_usb_device(&mut self, timeout: Duration) -> bool;
    fn sleep(&mut self, duration: Duration);
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The step ran in full.
    Done,
    /// The step does not apply here (no such driver loaded, GPIO 98 not set
    /// up); a lower step ran instead. The string says why.
    FellBack(String),
}

#[derive(Debug, snafu::Snafu)]
pub enum Error {
    #[snafu(display("rmmod {module}: {source}"))]
    Rmmod {
        module: String,
        source: std::io::Error,
    },
    #[snafu(display("insmod {}: {source}", path.display()))]
    Insmod {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("interface did not appear within {}s of insmod", timeout.as_secs()))]
    NoInterface { timeout: Duration },
    #[snafu(display("cannot raise the interface: {source}"))]
    IfaceUp { source: std::io::Error },
    #[snafu(display("cannot restart the station: {source}"))]
    Station { source: std::io::Error },
    #[snafu(display("cannot read the USB power-on level: {source}"))]
    PowerLevel { source: std::io::Error },
    #[snafu(display("cannot switch GPIO {USB_POWER_GPIO} off: {source}"))]
    PowerOff { source: std::io::Error },
    #[snafu(display(
        "GPIO {USB_POWER_GPIO} could NOT be restored to {on}: the Wi-Fi module may be unpowered"
    ))]
    PowerRestore { on: u8 },
    #[snafu(display("no USB device enumerated within {}s of power-on", timeout.as_secs()))]
    NoUsbDevice { timeout: Duration },
}

/// Runs the hardware steps. Carries one bit across calls: whether this
/// process unloaded the driver and has not managed to load it back.
#[derive(Debug, Default)]
pub struct Recovery {
    pub driver: Driver,
    unloaded: bool,
}

impl Recovery {
    pub fn new(driver: Driver) -> Self {
        Self {
            driver,
            unloaded: false,
        }
    }

    /// Unload and reload the driver, then restart the station.
    ///
    /// Applies only to a driver that is loaded (or that this process
    /// unloaded): on a camera with another module, or in the VM on
    /// `mac80211_hwsim`, it is a plain station restart.
    pub fn reset_driver(&mut self, ops: &mut impl Ops) -> Result<Outcome, Error> {
        let module = self.driver.module.clone();
        if !self.unloaded && !ops.module_loaded(&module) {
            restart(ops)?;
            return Ok(Outcome::FellBack(format!("{module} is not loaded")));
        }
        ops.stop_station();
        if ops.module_loaded(&module) {
            tracing::info!(module, "unloading the Wi-Fi driver");
            ops.rmmod(&module, self.driver.rmmod_timeout)
                .map_err(|source| Error::Rmmod {
                    module: module.clone(),
                    source,
                })?;
            self.unloaded = true;
        }
        self.reload(ops)?;
        Ok(Outcome::Done)
    }

    /// Power-cycle the USB module on GPIO 98, then reload the driver and
    /// restart the station.
    ///
    /// Falls back to [`Recovery::reset_driver`] when the pin is not set up
    /// as `setup.sh` leaves it, or the power-on level is unknown: guessing
    /// would risk leaving the module off.
    pub fn power_cycle(&mut self, ops: &mut impl Ops) -> Result<Outcome, Error> {
        if !ops.usb_power_ready() {
            return self.fall_back(
                ops,
                format!("GPIO {USB_POWER_GPIO} is not an exported output"),
            );
        }
        let on = match ops.power_on_level(&self.driver) {
            Ok(on) => on,
            Err(source) => {
                let why = Error::PowerLevel { source }.to_string();
                return self.fall_back(ops, why);
            }
        };

        ops.stop_station();
        let module = self.driver.module.clone();
        if ops.module_loaded(&module) {
            // Best effort: cutting the power disconnects the device anyway,
            // and a wedged driver may not let go until it has.
            match ops.rmmod(&module, self.driver.rmmod_timeout) {
                Ok(()) => self.unloaded = true,
                Err(error) => {
                    tracing::warn!(module, %error, "rmmod failed; cycling the power regardless")
                }
            }
        }

        {
            let power = PowerGuard::off(ops, on)?;
            let off_for = self.driver.power_off.max(Duration::from_secs(2));
            power.ops.sleep(off_for);
            power.restore()?;
        }

        if !ops.wait_for_usb_device(self.driver.usb_timeout) {
            return Err(Error::NoUsbDevice {
                timeout: self.driver.usb_timeout,
            });
        }
        // A driver that stayed loaded probes the re-enumerated device by
        // itself; one that was unloaded is loaded back.
        self.reload(ops)?;
        Ok(Outcome::Done)
    }

    /// Restores GPIO 98 to the power-on level if it is not there — after a
    /// SIGKILL in the middle of a power cycle, say. Touches nothing unless
    /// `setup.sh` exported the pin and the level is known.
    pub fn ensure_usb_power_on(&self, ops: &mut impl Ops) -> Result<bool, Error> {
        if !ops.usb_power_ready() {
            return Ok(false);
        }
        let on = ops
            .power_on_level(&self.driver)
            .map_err(|source| Error::PowerLevel { source })?;
        if ops.read_usb_power().ok() == Some(on) {
            return Ok(false);
        }
        tracing::warn!(
            gpio = USB_POWER_GPIO,
            level = u8::from(on),
            "USB Wi-Fi module found unpowered; restoring"
        );
        restore_power(ops, on)?;
        Ok(true)
    }

    fn fall_back(&mut self, ops: &mut impl Ops, why: String) -> Result<Outcome, Error> {
        tracing::warn!(reason = %why, "USB power cycle not possible; resetting the driver instead");
        match self.reset_driver(ops)? {
            Outcome::Done => Ok(Outcome::FellBack(why)),
            Outcome::FellBack(also) => Ok(Outcome::FellBack(format!("{why}; {also}"))),
        }
    }

    /// `insmod` if needed, wait for the interface, raise it, start the
    /// station.
    fn reload(&mut self, ops: &mut impl Ops) -> Result<(), Error> {
        if !ops.module_loaded(&self.driver.module) {
            tracing::info!(
                path = %self.driver.path.display(),
                args = ?self.driver.args,
                "loading the Wi-Fi driver"
            );
            ops.insmod(&self.driver).map_err(|source| Error::Insmod {
                path: self.driver.path.clone(),
                source,
            })?;
        }
        self.unloaded = false;
        if !ops.wait_for_iface(self.driver.iface_timeout) {
            return Err(Error::NoInterface {
                timeout: self.driver.iface_timeout,
            });
        }
        ops.iface_up(self.driver.mtu)
            .map_err(|source| Error::IfaceUp { source })?;
        restart(ops)
    }
}

fn restart(ops: &mut impl Ops) -> Result<(), Error> {
    ops.restart_station()
        .map_err(|source| Error::Station { source })
}

/// Writes the power-on level, retrying: this is the write that must not be
/// lost. Reads it back to be sure.
fn restore_power(ops: &mut impl Ops, on: bool) -> Result<(), Error> {
    for attempt in 1..=3 {
        match ops.write_usb_power(on) {
            Ok(()) if ops.read_usb_power().ok() == Some(on) => {
                tracing::info!(
                    gpio = USB_POWER_GPIO,
                    level = u8::from(on),
                    "USB Wi-Fi module powered on"
                );
                return Ok(());
            }
            Ok(()) => tracing::error!(
                gpio = USB_POWER_GPIO,
                attempt,
                "power-on level did not stick"
            ),
            Err(error) => {
                tracing::error!(gpio = USB_POWER_GPIO, attempt, %error, "cannot restore power-on level")
            }
        }
        ops.sleep(Duration::from_millis(200));
    }
    metrics::counter!("kamoder_wifi_usb_power_restore_failures_total").increment(1);
    Err(Error::PowerRestore { on: u8::from(on) })
}

/// GPIO 98 held at the "off" level. Restores "on" when dropped — on an early
/// return or a panic — unless [`PowerGuard::restore`] already did.
struct PowerGuard<'a, O: Ops> {
    ops: &'a mut O,
    on: bool,
    restored: bool,
}

impl<'a, O: Ops> PowerGuard<'a, O> {
    fn off(ops: &'a mut O, on: bool) -> Result<Self, Error> {
        tracing::warn!(
            gpio = USB_POWER_GPIO,
            level = u8::from(!on),
            "powering the USB Wi-Fi module off"
        );
        let guard = Self {
            ops,
            on,
            restored: false,
        };
        // Armed before the write: a write that fails half-way (or reports an
        // error after it took effect) is still undone.
        if let Err(source) = guard.ops.write_usb_power(!on) {
            return Err(Error::PowerOff { source });
        }
        Ok(guard)
    }

    fn restore(mut self) -> Result<(), Error> {
        self.restored = true;
        restore_power(self.ops, self.on)
    }
}

impl<O: Ops> Drop for PowerGuard<'_, O> {
    fn drop(&mut self) {
        if !self.restored {
            let _ = restore_power(self.ops, self.on);
        }
    }
}

/// Whether `/sys/bus/usb/devices` lists a device rather than only root hubs
/// (`usb1`) and interfaces (`1-1:1.0`). Devices are named `<bus>-<port>[.<port>…]`.
pub fn usb_device_present<'a>(names: impl IntoIterator<Item = &'a str>) -> bool {
    names.into_iter().any(|name| {
        let Some((bus, ports)) = name.split_once('-') else {
            return false;
        };
        !bus.is_empty()
            && bus.bytes().all(|b| b.is_ascii_digit())
            && !ports.is_empty()
            && ports.bytes().all(|b| b.is_ascii_digit() || b == b'.')
    })
}

/// Whether `/proc/modules` lists `module`.
pub fn module_listed(proc_modules: &str, module: &str) -> bool {
    proc_modules
        .lines()
        .any(|line| line.split_whitespace().next() == Some(module))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// Records every call; each knob makes one step fail.
    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        loaded: bool,
        gpio_ready: bool,
        power: bool,
        on_level: Option<bool>,
        fail_rmmod: bool,
        fail_insmod: bool,
        no_iface: bool,
        no_usb: bool,
        fail_power_off: bool,
        /// Writes of the "on" level fail this many times.
        fail_power_on: u32,
        panic_in_sleep: bool,
    }

    impl Fake {
        fn camera() -> Self {
            Self {
                loaded: true,
                gpio_ready: true,
                power: true,
                on_level: Some(true),
                ..Self::default()
            }
        }

        fn log(&mut self, call: impl Into<String>) {
            self.calls.push(call.into());
        }
    }

    impl Ops for Fake {
        fn stop_station(&mut self) {
            self.log("stop_station");
        }
        fn module_loaded(&mut self, _: &str) -> bool {
            self.loaded
        }
        fn rmmod(&mut self, module: &str, _: Duration) -> io::Result<()> {
            self.log(format!("rmmod {module}"));
            if self.fail_rmmod {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
            }
            self.loaded = false;
            Ok(())
        }
        fn insmod(&mut self, driver: &Driver) -> io::Result<()> {
            self.log(format!(
                "insmod {} {}",
                driver.path.display(),
                driver.args.join(" ")
            ));
            if self.fail_insmod {
                return Err(io::Error::other("exit 1"));
            }
            self.loaded = true;
            Ok(())
        }
        fn wait_for_iface(&mut self, _: Duration) -> bool {
            self.log("wait_for_iface");
            !self.no_iface
        }
        fn iface_up(&mut self, mtu: u32) -> io::Result<()> {
            self.log(format!("iface_up {mtu}"));
            Ok(())
        }
        fn restart_station(&mut self) -> io::Result<()> {
            self.log("restart_station");
            Ok(())
        }
        fn power_on_level(&mut self, _: &Driver) -> io::Result<bool> {
            self.on_level
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not 0/1"))
        }
        fn usb_power_ready(&mut self) -> bool {
            self.gpio_ready
        }
        fn read_usb_power(&mut self) -> io::Result<bool> {
            Ok(self.power)
        }
        fn write_usb_power(&mut self, high: bool) -> io::Result<()> {
            self.log(format!("gpio98={}", u8::from(high)));
            let on = self.on_level == Some(high);
            if on && self.fail_power_on > 0 {
                self.fail_power_on -= 1;
                return Err(io::Error::other("EIO"));
            }
            if !on && self.fail_power_off {
                // The write took effect but reported an error.
                self.power = high;
                return Err(io::Error::other("EIO"));
            }
            self.power = high;
            Ok(())
        }
        fn wait_for_usb_device(&mut self, _: Duration) -> bool {
            self.log("wait_for_usb");
            !self.no_usb
        }
        fn sleep(&mut self, duration: Duration) {
            if duration >= Duration::from_secs(1) {
                self.log(format!("sleep {}s", duration.as_secs()));
                assert!(!self.panic_in_sleep, "injected panic while powered off");
            }
        }
    }

    const INSMOD: &str =
        "insmod /ipc/modules/wifi/ssv6x5x.ko stacfgpath=/ipc/modules/wifi/ssv6x5x-wifi.cfg";

    #[test]
    fn driver_reset_runs_the_firmware_sequence() {
        let mut fake = Fake::camera();
        let mut recovery = Recovery::default();
        assert_eq!(recovery.reset_driver(&mut fake).unwrap(), Outcome::Done);
        assert_eq!(
            fake.calls,
            [
                "stop_station",
                "rmmod ssv6x5x",
                INSMOD,
                "wait_for_iface",
                "iface_up 1460",
                "restart_station"
            ]
        );
    }

    #[test]
    fn driver_reset_without_the_driver_is_a_station_restart() {
        // The VM: mac80211_hwsim, no ssv6x5x.
        let mut fake = Fake {
            loaded: false,
            ..Fake::camera()
        };
        let outcome = Recovery::default().reset_driver(&mut fake).unwrap();
        assert!(matches!(outcome, Outcome::FellBack(_)));
        assert_eq!(fake.calls, ["restart_station"]);
    }

    #[test]
    fn a_failed_reload_is_retried_next_time() {
        let mut fake = Fake {
            fail_insmod: true,
            ..Fake::camera()
        };
        let mut recovery = Recovery::default();
        assert!(matches!(
            recovery.reset_driver(&mut fake),
            Err(Error::Insmod { .. })
        ));
        // Now unloaded; the next reset must load it rather than fall back.
        fake.fail_insmod = false;
        fake.calls.clear();
        assert_eq!(recovery.reset_driver(&mut fake).unwrap(), Outcome::Done);
        assert_eq!(
            fake.calls[..2],
            ["stop_station".to_owned(), INSMOD.to_owned()]
        );
    }

    #[test]
    fn a_hung_rmmod_fails_the_reset_without_loading_twice() {
        let mut fake = Fake {
            fail_rmmod: true,
            ..Fake::camera()
        };
        assert!(matches!(
            Recovery::default().reset_driver(&mut fake),
            Err(Error::Rmmod { .. })
        ));
        assert!(!fake.calls.iter().any(|c| c.starts_with("insmod")));
    }

    #[test]
    fn power_cycle_sequence() {
        let mut fake = Fake::camera();
        assert_eq!(
            Recovery::default().power_cycle(&mut fake).unwrap(),
            Outcome::Done
        );
        assert_eq!(
            fake.calls,
            [
                "stop_station",
                "rmmod ssv6x5x",
                "gpio98=0",
                "sleep 3s",
                "gpio98=1",
                "wait_for_usb",
                INSMOD,
                "wait_for_iface",
                "iface_up 1460",
                "restart_station"
            ]
        );
        assert!(fake.power);
    }

    #[test]
    fn power_cycle_honours_an_active_low_module() {
        let mut fake = Fake {
            on_level: Some(false),
            power: false,
            ..Fake::camera()
        };
        Recovery::default().power_cycle(&mut fake).unwrap();
        let gpio: Vec<_> = fake
            .calls
            .iter()
            .filter(|c| c.starts_with("gpio98"))
            .collect();
        assert_eq!(gpio, ["gpio98=1", "gpio98=0"]);
        assert!(!fake.power);
    }

    #[test]
    fn power_is_off_for_at_least_two_seconds() {
        let mut fake = Fake::camera();
        let mut recovery = Recovery::new(Driver {
            power_off: Duration::from_millis(100),
            ..Driver::default()
        });
        recovery.power_cycle(&mut fake).unwrap();
        assert!(
            fake.calls.contains(&"sleep 2s".to_owned()),
            "{:?}",
            fake.calls
        );
    }

    #[test]
    fn power_is_restored_when_the_module_does_not_come_back() {
        let mut fake = Fake {
            no_usb: true,
            ..Fake::camera()
        };
        assert!(matches!(
            Recovery::default().power_cycle(&mut fake),
            Err(Error::NoUsbDevice { .. })
        ));
        assert!(fake.power);
    }

    #[test]
    fn power_is_restored_when_switching_off_reports_an_error() {
        let mut fake = Fake {
            fail_power_off: true,
            ..Fake::camera()
        };
        assert!(matches!(
            Recovery::default().power_cycle(&mut fake),
            Err(Error::PowerOff { .. })
        ));
        assert!(fake.power, "{:?}", fake.calls);
        assert_eq!(fake.calls.last().unwrap(), "gpio98=1");
    }

    #[test]
    fn power_restore_is_retried() {
        let mut fake = Fake {
            fail_power_on: 2,
            ..Fake::camera()
        };
        assert_eq!(
            Recovery::default().power_cycle(&mut fake).unwrap(),
            Outcome::Done
        );
        assert!(fake.power);
        let ons = fake.calls.iter().filter(|c| *c == "gpio98=1").count();
        assert_eq!(ons, 3);
    }

    #[test]
    fn a_restore_that_never_sticks_is_an_error_and_stops_there() {
        let mut fake = Fake {
            fail_power_on: 100,
            ..Fake::camera()
        };
        assert!(matches!(
            Recovery::default().power_cycle(&mut fake),
            Err(Error::PowerRestore { on: 1 })
        ));
        assert!(!fake.calls.iter().any(|c| c.starts_with("insmod")));
    }

    #[test]
    fn power_is_restored_on_a_panic() {
        let mut fake = Fake {
            panic_in_sleep: true,
            ..Fake::camera()
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = Recovery::default().power_cycle(&mut fake);
        }));
        assert!(result.is_err());
        assert!(fake.power);
        assert_eq!(fake.calls.last().unwrap(), "gpio98=1");
    }

    #[test]
    fn a_rmmod_failure_does_not_stop_the_power_cycle() {
        let mut fake = Fake {
            fail_rmmod: true,
            ..Fake::camera()
        };
        assert_eq!(
            Recovery::default().power_cycle(&mut fake).unwrap(),
            Outcome::Done
        );
        // Still loaded: it probes the re-enumerated device by itself.
        assert!(!fake.calls.iter().any(|c| c.starts_with("insmod")));
        assert!(fake.calls.contains(&"restart_station".to_owned()));
    }

    #[test]
    fn gpio_not_set_up_by_the_firmware_is_never_touched() {
        let mut fake = Fake {
            gpio_ready: false,
            ..Fake::camera()
        };
        let outcome = Recovery::default().power_cycle(&mut fake).unwrap();
        assert!(matches!(outcome, Outcome::FellBack(_)));
        assert!(!fake.calls.iter().any(|c| c.starts_with("gpio")));
        // The driver reset ran instead.
        assert!(fake.calls.contains(&INSMOD.to_owned()));
    }

    #[test]
    fn an_unknown_power_on_level_is_never_guessed() {
        let mut fake = Fake {
            on_level: None,
            ..Fake::camera()
        };
        let outcome = Recovery::default().power_cycle(&mut fake).unwrap();
        assert!(matches!(outcome, Outcome::FellBack(_)));
        assert!(!fake.calls.iter().any(|c| c.starts_with("gpio")));
    }

    #[test]
    fn the_vm_falls_all_the_way_back_to_a_station_restart() {
        let mut fake = Fake {
            loaded: false,
            gpio_ready: false,
            ..Fake::default()
        };
        let outcome = Recovery::default().power_cycle(&mut fake).unwrap();
        assert!(matches!(outcome, Outcome::FellBack(_)));
        assert_eq!(fake.calls, ["restart_station"]);
    }

    #[test]
    fn start_up_restores_power_only_when_off() {
        let recovery = Recovery::default();
        let mut fake = Fake::camera();
        assert!(!recovery.ensure_usb_power_on(&mut fake).unwrap());
        assert!(fake.calls.is_empty());

        let mut fake = Fake {
            power: false,
            ..Fake::camera()
        };
        assert!(recovery.ensure_usb_power_on(&mut fake).unwrap());
        assert_eq!(fake.calls, ["gpio98=1"]);

        let mut fake = Fake {
            power: false,
            gpio_ready: false,
            ..Fake::camera()
        };
        assert!(!recovery.ensure_usb_power_on(&mut fake).unwrap());
        assert!(fake.calls.is_empty());
    }

    #[test]
    fn usb_devices_versus_hubs_and_interfaces() {
        assert!(!usb_device_present(["usb1", "1-0:1.0"]));
        assert!(usb_device_present(["usb1", "1-1", "1-1:1.0"]));
        assert!(usb_device_present(["2-1.3"]));
        assert!(!usb_device_present(["-1", "1-", "x-1"]));
    }

    #[test]
    fn proc_modules_lookup() {
        // From the AV100's /proc/modules.
        let modules = "ssv6x5x 978901 0 - Live 0xbf2ca000 (O)\n\
mac80211 334152 0 - Live 0xbf277000\n\
cfg80211 231941 2 ssv6x5x,mac80211, Live 0xbf0ea000\n";
        assert!(module_listed(modules, "ssv6x5x"));
        assert!(module_listed(modules, "mac80211"));
        assert!(!module_listed(modules, "ssv6x5"));
        assert!(!module_listed(modules, "8188fu"));
    }
}
