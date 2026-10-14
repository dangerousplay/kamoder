//! Side effects: the config file on `/rom`, the last-good record on the SD
//! card, restarting the station, and the hardware steps of
//! [`crate::recovery`] (driver modules, the USB Wi-Fi power on GPIO 98).

use crate::wpa::Network;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Where things live. Defaults match the AK3918AV100 firmware.
#[derive(Debug, Clone)]
pub struct Paths {
    pub wpa_conf: PathBuf,
    pub ctrl_dir: PathBuf,
    pub iface: String,
    pub dhcp_script: PathBuf,
    /// On the SD card: backups and the last-good record.
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn last_good(&self) -> PathBuf {
        self.state_dir.join("wpa_supplicant0.conf.good")
    }
}

/// Reads the configured network.
pub fn read_configured(paths: &Paths) -> Option<Network> {
    crate::wpa::parse_config(&std::fs::read_to_string(&paths.wpa_conf).ok()?)
}

/// Loads the last-known-good network recorded on a previous run.
pub fn load_last_good(paths: &Paths) -> Option<Network> {
    crate::wpa::parse_config(&std::fs::read_to_string(paths.last_good()).ok()?)
}

/// Persists the last-known-good network to the SD card, so a reboot does not
/// forget what worked.
pub fn save_last_good(paths: &Paths, network: &Network) -> std::io::Result<()> {
    std::fs::create_dir_all(&paths.state_dir)?;
    write_atomic(
        &paths.last_good(),
        network.render(&paths.ctrl_dir).as_bytes(),
    )
}

/// Writes `network` as the station config, backing up the current one first.
///
/// `wpa_supplicant0.conf.orig` is the camera's own config and is written only
/// once, ever; `.last` is whatever was there before this write.
pub fn write_config(paths: &Paths, network: &Network) -> std::io::Result<()> {
    // The backups are for a human; they are best-effort. What keeps the
    // camera reachable is the last-good record, which the reconciler always
    // takes before switching and holds in memory. A card gone read-only after
    // a power cut must not stop the switch itself.
    if let Err(error) = back_up(paths) {
        tracing::warn!(%error, dir = %paths.state_dir.display(), "cannot back up the station config; writing it anyway");
    }

    let mount = MountRw::new(&paths.wpa_conf)?;
    let result = write_atomic(&paths.wpa_conf, network.render(&paths.ctrl_dir).as_bytes());
    drop(mount);
    result
}

/// Copies the current station config into the state directory:
/// `.orig` once ever, `.last` every time.
fn back_up(paths: &Paths) -> std::io::Result<()> {
    if !paths.wpa_conf.is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(&paths.state_dir)?;
    let orig = paths.state_dir.join("wpa_supplicant0.conf.orig");
    if !orig.exists() {
        copy_private(&paths.wpa_conf, &orig)?;
    }
    copy_private(
        &paths.wpa_conf,
        &paths.state_dir.join("wpa_supplicant0.conf.last"),
    )
}

/// Restarts the station: stops wpa_supplicant and the DHCP client, resets the
/// interface, and starts both again against the config file.
///
/// The same sequence the firmware's own scripts use, so wpa_supplicant ends up
/// configured exactly as it would be after a boot. Deliberately *not*
/// `/usr/sbin/wifi_manage.sh`, which on some firmwares also restarts vendor
/// processes.
pub fn reassociate(paths: &Paths) -> std::io::Result<()> {
    for name in ["wpa_supplicant", "udhcpc", "hostapd", "udhcpd"] {
        stop_processes(name);
    }
    std::fs::create_dir_all(&paths.ctrl_dir)?;
    // A stale socket from a killed wpa_supplicant makes the new one refuse to
    // start ("ctrl_iface exists and seems to be in use").
    let _ = std::fs::remove_file(paths.ctrl_dir.join(&paths.iface));

    let iface = paths.iface.as_str();
    // Best effort: these fail harmlessly if the interface is already in the
    // requested state.
    let _ = run("ifconfig", &[iface, "down"]);
    let _ = run("ifconfig", &[iface, "0.0.0.0"]);
    let _ = run("iwconfig", &[iface, "mode", "Managed"]);
    let _ = run("ifconfig", &[iface, "up"]);

    let conf = paths.wpa_conf.to_string_lossy();
    let ctrl = paths.ctrl_dir.to_string_lossy();
    run(
        "wpa_supplicant",
        &[
            "-B",
            &format!("-i{iface}"),
            "-Dwext",
            "-c",
            &conf,
            "-C",
            &ctrl,
        ],
    )?;

    // DHCP only once the link is keyed. Started any earlier, udhcpc's first
    // discovers go nowhere, it backgrounds, and BusyBox then waits 20 s
    // before trying again — long enough for the reconciler's own retry to
    // restart the station and kill it just before it would have succeeded,
    // over and over. Found by the qemu integration tests.
    let associated = wait_for_association(paths, ASSOCIATION_TIMEOUT);
    if !associated {
        tracing::warn!(
            timeout_s = ASSOCIATION_TIMEOUT.as_secs(),
            "not associated yet; starting DHCP anyway, it keeps retrying"
        );
    }

    start_dhcp(paths)
}

/// Starts the DHCP client the way the firmware does.
fn start_dhcp(paths: &Paths) -> std::io::Result<()> {
    let script = paths.dhcp_script.to_string_lossy();
    // `-b`: go to the background if no lease arrives quickly, and keep
    // trying. `-t 4 -T 2 -A 5`: four discovers two seconds apart, then retry
    // after 5 s instead of BusyBox's default 20.
    run(
        "udhcpc",
        &[
            "-i",
            &paths.iface,
            "-b",
            "-t",
            "4",
            "-T",
            "2",
            "-A",
            "5",
            "-s",
            &script,
        ],
    )
}

/// The gentle step: keep wpa_supplicant running, ask it to reassociate
/// (fresh keys, fresh association, same process), then renew the lease.
///
/// BusyBox udhcpc renews on SIGUSR1; if no client is running, one is started.
pub fn renew(paths: &Paths) -> std::io::Result<()> {
    let reply = crate::wpa::query(&paths.ctrl_dir, &paths.iface, "REASSOCIATE")?;
    if reply.trim() != "OK" {
        return Err(std::io::Error::other(format!(
            "wpa_supplicant answered {:?} to REASSOCIATE",
            reply.trim()
        )));
    }
    if !wait_for_association(paths, RENEW_ASSOCIATION_TIMEOUT) {
        tracing::warn!(
            timeout_s = RENEW_ASSOCIATION_TIMEOUT.as_secs(),
            "not reassociated yet; renewing DHCP anyway"
        );
    }
    let clients = find_processes("udhcpc");
    if clients.is_empty() {
        tracing::info!("no DHCP client running; starting one");
        return start_dhcp(paths);
    }
    for pid in clients {
        let _ = kill(pid, Signal::SIGUSR1);
    }
    Ok(())
}

/// How long `renew` waits for the reassociation before renewing the lease.
const RENEW_ASSOCIATION_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for wpa_supplicant to associate before starting DHCP.
const ASSOCIATION_TIMEOUT: Duration = Duration::from_secs(20);

/// Polls wpa_supplicant's control socket until it reports `COMPLETED`.
fn wait_for_association(paths: &Paths, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if crate::wpa::query(&paths.ctrl_dir, &paths.iface, "STATUS")
            .map(|reply| crate::wpa::Status::parse(&reply).completed())
            .unwrap_or(false)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

/// The camera's side of [`crate::recovery::Ops`]: BusyBox commands, sysfs
/// and procfs. The only GPIO it can reach is
/// [`USB_POWER_GPIO`](crate::recovery::USB_POWER_GPIO).
pub struct SystemOps<'a> {
    pub paths: &'a Paths,
    /// `/sys/class/gpio`; a temporary directory in tests.
    pub gpio_root: PathBuf,
}

impl<'a> SystemOps<'a> {
    pub fn new(paths: &'a Paths) -> Self {
        Self {
            paths,
            gpio_root: PathBuf::from("/sys/class/gpio"),
        }
    }

    fn gpio_file(&self, name: &str) -> PathBuf {
        self.gpio_root
            .join(format!("gpio{}", crate::recovery::USB_POWER_GPIO))
            .join(name)
    }
}

impl crate::recovery::Ops for SystemOps<'_> {
    fn stop_station(&mut self) {
        for name in ["wpa_supplicant", "udhcpc"] {
            stop_processes(name);
        }
        let _ = std::fs::remove_file(self.paths.ctrl_dir.join(&self.paths.iface));
        let _ = run("ifconfig", &[&self.paths.iface, "down"]);
    }

    fn module_loaded(&mut self, module: &str) -> bool {
        std::fs::read_to_string("/proc/modules")
            .is_ok_and(|text| crate::recovery::module_listed(&text, module))
    }

    fn rmmod(&mut self, module: &str, timeout: Duration) -> std::io::Result<()> {
        run_with_timeout("rmmod", &[module], timeout)
    }

    fn insmod(&mut self, driver: &crate::recovery::Driver) -> std::io::Result<()> {
        let path = driver.path.to_string_lossy();
        let mut args = vec![path.as_ref()];
        args.extend(driver.args.iter().map(String::as_str));
        run_with_timeout("insmod", &args, driver.insmod_timeout)
    }

    fn wait_for_iface(&mut self, timeout: Duration) -> bool {
        let node = Path::new("/sys/class/net").join(&self.paths.iface);
        poll(timeout, || node.exists())
    }

    fn iface_up(&mut self, mtu: u32) -> std::io::Result<()> {
        let iface = self.paths.iface.as_str();
        let _ = run("ifconfig", &[iface, "mtu", &mtu.to_string()]);
        run("ifconfig", &[iface, "up"])
    }

    fn restart_station(&mut self) -> std::io::Result<()> {
        reassociate(self.paths)
    }

    fn power_on_level(&mut self, driver: &crate::recovery::Driver) -> std::io::Result<bool> {
        match std::fs::read_to_string(&driver.power_on_file) {
            Ok(text) => parse_level(&text).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{} holds {:?}, not 0 or 1",
                        driver.power_on_file.display(),
                        text.trim()
                    ),
                )
            }),
            // setup.sh: `power_val=1` unless the file says otherwise.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error),
        }
    }

    fn usb_power_ready(&mut self) -> bool {
        std::fs::read_to_string(self.gpio_file("direction")).is_ok_and(|d| d.trim() == "out")
    }

    fn read_usb_power(&mut self) -> std::io::Result<bool> {
        let text = std::fs::read_to_string(self.gpio_file("value"))?;
        parse_level(&text).ok_or_else(|| std::io::Error::other(format!("GPIO value {text:?}")))
    }

    fn write_usb_power(&mut self, high: bool) -> std::io::Result<()> {
        // `value` only: the pin is already an output (checked), and writing
        // `direction` would glitch it.
        std::fs::write(self.gpio_file("value"), if high { "1" } else { "0" })
    }

    fn wait_for_usb_device(&mut self, timeout: Duration) -> bool {
        poll(timeout, || {
            std::fs::read_dir("/sys/bus/usb/devices").is_ok_and(|entries| {
                let names: Vec<String> = entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                crate::recovery::usb_device_present(names.iter().map(String::as_str))
            })
        })
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// `0`/`1`, as sysfs and `setup.sh` write them.
fn parse_level(text: &str) -> Option<bool> {
    match text.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// Polls `ready` every 250 ms until it holds or `timeout` passes.
fn poll(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if ready() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// [`run`], but gives up after `timeout`.
///
/// For `rmmod`/`insmod` against a driver that may be wedged: the child can
/// end up in uninterruptible sleep, and the control loop must not end up
/// there with it. On timeout the child is sent SIGKILL and left to a reaper
/// thread, which collects it if it ever returns.
fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> std::io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "{program} exited with {status}"
                )))
            };
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            std::thread::spawn(move || child.wait());
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{program} still running after {}s", timeout.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Runs a helper to completion in its own process group, so the supervisor
/// stopping this service does not also take down the daemons it started.
fn run(program: &str, args: &[&str]) -> std::io::Result<()> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{program} exited with {status}"
        )))
    }
}

/// SIGTERM every process with this exact name, then SIGKILL any survivor.
///
/// Scans `/proc` because `killall`/`pidof` are not in every BusyBox build here.
fn stop_processes(name: &str) {
    let pids = find_processes(name);
    for &pid in &pids {
        let _ = kill(pid, Signal::SIGTERM);
    }
    if pids.is_empty() {
        return;
    }
    std::thread::sleep(Duration::from_millis(500));
    for pid in find_processes(name) {
        let _ = kill(pid, Signal::SIGKILL);
    }
}

/// Processes named `name` in *this* network namespace.
///
/// Only those can be holding our interface. A hostapd in another namespace is
/// another machine's access point as far as the network stack is concerned —
/// and in the qemu tests it literally is: stopping every hostapd by name took
/// the simulated access points down with it.
fn find_processes(name: &str) -> Vec<Pid> {
    let own_netns = std::fs::read_link("/proc/self/ns/net").ok();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            let comm = std::fs::read_to_string(entry.path().join("comm")).ok()?;
            if comm.trim_end() != name {
                return None;
            }
            // If either namespace link is unreadable, err on the side of
            // stopping it: that is the old behaviour, and the safe one on a
            // camera without namespaces.
            let netns = std::fs::read_link(entry.path().join("ns/net")).ok();
            let foreign =
                matches!((&own_netns, &netns), (Some(own), Some(theirs)) if own != theirs);
            (!foreign).then_some(Pid::from_raw(pid))
        })
        .collect()
}

/// Write beside the target, fsync, rename. A power cut mid-write leaves the
/// old file, never a truncated one — which on `/rom` would mean no Wi-Fi.
fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("new");
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

fn copy_private(from: &Path, to: &Path) -> std::io::Result<()> {
    write_atomic(to, &std::fs::read(from)?)
}

/// Remounts the filesystem holding a path read-write for the guard's
/// lifetime, and back to read-only afterwards if that is how it was found.
struct MountRw {
    remounted: Option<PathBuf>,
}

impl MountRw {
    fn new(path: &Path) -> std::io::Result<Self> {
        let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
        let Some((mountpoint, readonly)) = mount_of(&mounts, path) else {
            return Ok(Self { remounted: None });
        };
        if !readonly {
            return Ok(Self { remounted: None });
        }
        run_checked(
            "mount",
            &["-o", "remount,rw", &mountpoint.to_string_lossy()],
        )?;
        Ok(Self {
            remounted: Some(mountpoint),
        })
    }
}

impl Drop for MountRw {
    fn drop(&mut self) {
        if let Some(mountpoint) = &self.remounted {
            let _ = run_checked(
                "mount",
                &["-o", "remount,ro", &mountpoint.to_string_lossy()],
            );
        }
    }
}

fn run_checked(program: &str, args: &[&str]) -> std::io::Result<()> {
    let status = Command::new(program).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{program} {} exited with {status}",
            args.join(" ")
        )))
    }
}

/// The longest mountpoint that contains `path`, and whether it is read-only.
pub fn mount_of(mounts: &str, path: &Path) -> Option<(PathBuf, bool)> {
    mounts
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let mountpoint = Path::new(*fields.get(1)?);
            let options = fields.get(3)?;
            path.starts_with(mountpoint).then(|| {
                (
                    mountpoint.to_path_buf(),
                    options.split(',').any(|o| o == "ro"),
                )
            })
        })
        .max_by_key(|(mountpoint, _)| mountpoint.as_os_str().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTS: &str = "rootfs / rootfs rw 0 0\n\
/dev/root / squashfs ro,relatime 0 0\n\
/dev/mtdblock5 /rom jffs2 ro,relatime 0 0\n\
tmpfs /mnt tmpfs rw 0 0\n\
/dev/mmcblk0p1 /mnt/disc1 vfat rw,relatime 0 0\n";

    #[test]
    fn usb_power_through_a_fake_sysfs() {
        use crate::recovery::{Driver, Ops};
        let dir = std::env::temp_dir().join(format!("anyka-wifi-gpio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pin = dir.join("gpio98");
        let paths = Paths {
            wpa_conf: dir.join("wpa_supplicant0.conf"),
            ctrl_dir: dir.clone(),
            iface: "wlan0".into(),
            dhcp_script: PathBuf::new(),
            state_dir: dir.clone(),
        };
        let mut ops = SystemOps {
            paths: &paths,
            gpio_root: dir.clone(),
        };
        // Not exported by setup.sh: not ready, nothing written.
        assert!(!ops.usb_power_ready());

        std::fs::create_dir_all(&pin).unwrap();
        std::fs::write(pin.join("direction"), "in\n").unwrap();
        assert!(!ops.usb_power_ready(), "an input is not ours to drive");
        std::fs::write(pin.join("direction"), "out\n").unwrap();
        std::fs::write(pin.join("value"), "1\n").unwrap();
        assert!(ops.usb_power_ready());
        assert!(ops.read_usb_power().unwrap());
        ops.write_usb_power(false).unwrap();
        assert!(!ops.read_usb_power().unwrap());

        let driver = Driver {
            power_on_file: dir.join("usb_module_power_on"),
            ..Driver::default()
        };
        assert!(ops.power_on_level(&driver).unwrap(), "missing file means 1");
        std::fs::write(&driver.power_on_file, "0\n").unwrap();
        assert!(!ops.power_on_level(&driver).unwrap());
        std::fs::write(&driver.power_on_file, "garbage").unwrap();
        assert!(ops.power_on_level(&driver).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hung_helper_times_out() {
        let started = std::time::Instant::now();
        let error = run_with_timeout("sleep", &["10"], Duration::from_millis(300)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(run_with_timeout("true", &[], Duration::from_secs(5)).is_ok());
        assert!(run_with_timeout("false", &[], Duration::from_secs(5)).is_err());
    }

    #[test]
    fn longest_mountpoint_wins() {
        assert_eq!(
            mount_of(MOUNTS, Path::new("/rom/wpa_supplicant0.conf")),
            Some((PathBuf::from("/rom"), true))
        );
        assert_eq!(
            mount_of(MOUNTS, Path::new("/mnt/disc1/anyka_rust/backup")),
            Some((PathBuf::from("/mnt/disc1"), false))
        );
        assert_eq!(
            mount_of(MOUNTS, Path::new("/mnt/x")),
            Some((PathBuf::from("/mnt"), false))
        );
    }

    #[test]
    fn config_write_backs_up_once_and_last_good_round_trips() {
        let dir = std::env::temp_dir().join(format!("anyka-wifi-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            wpa_conf: dir.join("wpa_supplicant0.conf"),
            ctrl_dir: PathBuf::from("/etc/Wireless"),
            iface: "wlan0".into(),
            dhcp_script: PathBuf::from("/usr/share/udhcpc/default.script"),
            state_dir: dir.join("state"),
        };
        let first = Network {
            ssid: "first".into(),
            psk: "password1".into(),
        };
        let second = Network {
            ssid: "second".into(),
            psk: "password2".into(),
        };
        let third = Network {
            ssid: "third".into(),
            psk: "password3".into(),
        };

        std::fs::write(&paths.wpa_conf, first.render(&paths.ctrl_dir)).unwrap();
        write_config(&paths, &second).unwrap();
        write_config(&paths, &third).unwrap();

        assert_eq!(read_configured(&paths), Some(third));
        let orig =
            std::fs::read_to_string(paths.state_dir.join("wpa_supplicant0.conf.orig")).unwrap();
        let last =
            std::fs::read_to_string(paths.state_dir.join("wpa_supplicant0.conf.last")).unwrap();
        assert_eq!(crate::wpa::parse_config(&orig), Some(first));
        assert_eq!(crate::wpa::parse_config(&last), Some(second.clone()));

        assert_eq!(load_last_good(&paths), None);
        save_last_good(&paths, &second).unwrap();
        assert_eq!(load_last_good(&paths), Some(second));

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&paths.wpa_conf)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "config must not be world-readable");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
