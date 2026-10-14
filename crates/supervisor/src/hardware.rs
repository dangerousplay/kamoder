//! Hardware and C library detection for the boot banner.
//!
//! Reads only generic Linux interfaces (`/proc`, `/sys`, `/lib`), never a
//! vendor SDK, so the same code describes any camera this project runs on.
//! Every probe is optional: whatever a board does not expose is left out.
//! Paths are taken relative to `root` so tests can point it at a fixture.

use std::fs;
use std::path::Path;

/// Known USB Wi-Fi modules, from the vendor `/ipc/setup.sh` probe table.
const USB_WIFI: &[(&str, &str)] = &[
    ("0bda:f179", "Realtek RTL8188FU"),
    ("1e04:8888", "RDA5995"),
    ("8065:6000", "SSV6x5x"),
    ("8065:6011", "SSV6355"),
    ("8065:6012", "SSV6555"),
];

/// One `label: value` line of the banner's hardware section.
pub type Row = (&'static str, String);

pub fn detect(root: &Path) -> Vec<Row> {
    let read = |rel: &str| fs::read_to_string(root.join(rel)).ok();
    let mut rows = Vec::new();

    let cpuinfo = read("proc/cpuinfo").unwrap_or_default();
    let field = |key: &str| {
        cpuinfo.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            (k.trim() == key).then(|| v.trim().to_owned())
        })
    };

    let model = read("proc/device-tree/model").map(|m| m.trim_end_matches('\0').trim().to_owned());
    if let Some(board) = field("Hardware").or(model).filter(|b| !b.is_empty()) {
        rows.push(("Board", board));
    }

    if let Some(cpu) = field("model name").or_else(|| field("Processor")) {
        let mips = field("BogoMIPS")
            .map(|b| format!(", {b} BogoMIPS"))
            .unwrap_or_default();
        rows.push(("CPU", format!("{cpu}{mips}")));
    }

    let cmdline = read("proc/cmdline").unwrap_or_default();
    if let Some(total_kb) = read("proc/meminfo").as_deref().and_then(meminfo_total_kb) {
        let physical = cmdline
            .split_whitespace()
            .find_map(|arg| arg.strip_prefix("mem="))
            .map(|mem| format!(" of {mem} (rest reserved by the kernel/drivers)"))
            .unwrap_or_default();
        rows.push((
            "Memory",
            format!("{} usable{physical}", mib(total_kb * 1024)),
        ));
    }

    if let Some(mtd) = read("proc/mtd") {
        let sizes: Vec<u64> = mtd
            .lines()
            .skip(1)
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter_map(|hex| u64::from_str_radix(hex, 16).ok())
            .collect();
        if let Some(&largest) = sizes.iter().max() {
            rows.push((
                "Flash",
                format!("{}, {} MTD partitions", mib(largest), sizes.len()),
            ));
        }
    }

    if let Some(sensor) = read("sys/sensor_sys/sensor_id") {
        let sensor = sensor.trim();
        if !sensor.is_empty() {
            rows.push(("Sensor", sensor.to_owned()));
        }
    }

    if let Some(wifi) = usb_wifi(root) {
        let mac = read("sys/class/net/wlan0/address")
            .map(|mac| format!(", wlan0 {}", mac.trim()))
            .unwrap_or_default();
        rows.push(("Wi-Fi", format!("{wifi}{mac}")));
    }

    if let Some(sectors) = read("sys/block/mmcblk0/size").and_then(|s| s.trim().parse::<u64>().ok())
    {
        rows.push(("SD card", gib(sectors * 512)));
    }

    rows.push(("libc", libc(root)));
    rows
}

fn meminfo_total_kb(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse().ok())
}

fn usb_wifi(root: &Path) -> Option<String> {
    let devices = fs::read_dir(root.join("sys/bus/usb/devices")).ok()?;
    devices.flatten().find_map(|dev| {
        let id = |file: &str| fs::read_to_string(dev.path().join(file)).ok();
        let vid_pid = format!("{}:{}", id("idVendor")?.trim(), id("idProduct")?.trim());
        USB_WIFI
            .iter()
            .find(|(known, _)| *known == vid_pid)
            .map(|(_, name)| format!("{name} (USB {vid_pid})"))
    })
}

/// The C library this binary was built for and, when the runtime library
/// names its version in its file name, the version actually installed.
fn libc(root: &Path) -> String {
    let built_for = if cfg!(target_env = "uclibc") {
        "uClibc"
    } else if cfg!(target_env = "musl") {
        "musl"
    } else if cfg!(target_env = "gnu") {
        "glibc"
    } else {
        "unknown libc"
    };
    // uClibc: libc.so.0 -> libuClibc-1.0.31.so; glibc <2.34: libc.so.6 ->
    // libc-2.31.so. Newer glibc and musl carry no version in the name.
    let installed = ["lib/libc.so.0", "lib/libc.so.1", "lib/libc.so.6"]
        .iter()
        .find_map(|link| fs::read_link(root.join(link)).ok())
        .and_then(|target| {
            let name = target.file_name()?.to_str()?.to_owned();
            let version = name
                .trim_end_matches(".so")
                .rsplit_once('-')
                .map(|(_, v)| v.to_owned())?;
            version
                .starts_with(|c: char| c.is_ascii_digit())
                .then_some(version)
        });
    match installed {
        Some(version) => format!("{built_for} {version}"),
        None => built_for.to_owned(),
    }
}

fn mib(bytes: u64) -> String {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    if mib.fract() == 0.0 {
        format!("{mib:.0} MiB")
    } else {
        format!("{mib:.1} MiB")
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A fake root shaped like the AK3918AV100, from the captured
    /// `firmware_dump/*/system_info` and the rootfs `/lib`.
    fn av100_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("hw-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let write = |rel: &str, body: &str| {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        };
        write(
            "proc/cpuinfo",
            "processor\t: 0\nmodel name\t: ARM926EJ-S rev 5 (v5l)\nBogoMIPS\t: 441.34\n\
             Hardware\t: AK3918AV100\nRevision\t: 0000\n",
        );
        write(
            "proc/meminfo",
            "MemTotal:          32788 kB\nMemFree: 1 kB\n",
        );
        write(
            "proc/cmdline",
            "console=ttySAK0,115200n8 mem=64M memsize=64M\n",
        );
        write(
            "proc/mtd",
            "dev:    size   erasesize  name\nmtd0: 00037000 00001000 \"UBOOT\"\n\
             mtd6: 004d7000 00001000 \"APP\"\nmtd8: 00800000 00001000 \"FLASH\"\n",
        );
        write("sys/sensor_sys/sensor_id", "sc2331\n");
        write("sys/bus/usb/devices/1-1/idVendor", "8065\n");
        write("sys/bus/usb/devices/1-1/idProduct", "6011\n");
        write("sys/class/net/wlan0/address", "aa:bb:cc:dd:ee:ff\n");
        write("sys/block/mmcblk0/size", "62333952\n");
        fs::create_dir_all(root.join("lib")).unwrap();
        symlink("libuClibc-1.0.31.so", root.join("lib/libc.so.0")).unwrap();
        root
    }

    #[test]
    fn describes_the_av100() {
        let root = av100_root();
        let rows = detect(&root);
        let get = |label: &str| {
            rows.iter()
                .find(|(l, _)| *l == label)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("Board"), Some("AK3918AV100"));
        assert_eq!(get("CPU"), Some("ARM926EJ-S rev 5 (v5l), 441.34 BogoMIPS"));
        assert_eq!(
            get("Memory"),
            Some("32.0 MiB usable of 64M (rest reserved by the kernel/drivers)")
        );
        assert_eq!(get("Flash"), Some("8 MiB, 3 MTD partitions"));
        assert_eq!(get("Sensor"), Some("sc2331"));
        assert_eq!(
            get("Wi-Fi"),
            Some("SSV6355 (USB 8065:6011), wlan0 aa:bb:cc:dd:ee:ff")
        );
        assert_eq!(get("SD card"), Some("29.7 GiB"));
        assert!(get("libc").unwrap().ends_with(" 1.0.31"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_root_still_reports_libc_only() {
        let root = std::env::temp_dir().join(format!("hw-empty-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let rows = detect(&root);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "libc");
        fs::remove_dir_all(root).unwrap();
    }
}
