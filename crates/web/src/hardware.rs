//! Hardware summary for the dashboard: SoC, memory, kernel, flash and the
//! image sensors the kernel found.
//!
//! Everything comes from procfs and sysfs, read relative to
//! [`Config::sys_root`](crate::state::Config) so tests can point it at a
//! fixture tree. Each image sensor is one V4L2 sub-device
//! (`/sys/class/video4linux/v4l-subdevN`, named `"<sensor> <bus>-<addr>"`),
//! and its capture channels are the `video-N-M` nodes with the same `N`.
//! A dual-sensor board such as the AV100 pan/tilt unit has two of each.

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use std::path::Path;

/// Routes under `/api`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/hardware", get(hardware))
}

/// `api.ts` `Hardware`.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Hardware {
    /// `Hardware` line of `/proc/cpuinfo`, e.g. `AK3918AV100`.
    pub soc: Option<String>,
    /// `model name`, e.g. `ARM926EJ-S rev 5 (v5l)`.
    pub cpu: Option<String>,
    pub bogomips: Option<f32>,
    /// Device-tree model string.
    pub board: Option<String>,
    /// Kernel release, e.g. `4.4.282`.
    pub kernel: Option<String>,
    /// RAM Linux manages; the video reserve (ION) is not included.
    pub memory_total_kb: Option<u64>,
    pub memory_available_kb: Option<u64>,
    pub uptime_s: Option<u64>,
    pub flash: Vec<FlashPartition>,
    pub cameras: Vec<Camera>,
    pub camera_count: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FlashPartition {
    pub device: String,
    pub name: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Camera {
    /// Sensor index: `N` of `v4l-subdevN` and of `/dev/video-N-*`.
    pub index: u32,
    /// Sensor driver name, e.g. `sc2331`.
    pub sensor: String,
    /// I²C location as the kernel names it, e.g. `0-0030`.
    pub bus: Option<String>,
    /// Capture channels (`/dev/video-N-M`): main, sub, and so on.
    pub channels: u32,
}

/// Reads the summary from the tree under `root` (`/` on the camera).
pub fn read(root: &Path) -> Hardware {
    let text = |rel: &str| std::fs::read_to_string(root.join(rel)).ok();
    let cpuinfo = text("proc/cpuinfo").unwrap_or_default();
    let meminfo = text("proc/meminfo").unwrap_or_default();
    let cameras = cameras(&root.join("sys/class/video4linux"));
    Hardware {
        soc: field(&cpuinfo, "Hardware"),
        cpu: field(&cpuinfo, "model name").or_else(|| field(&cpuinfo, "Processor")),
        bogomips: field(&cpuinfo, "BogoMIPS").and_then(|v| v.parse().ok()),
        // Device-tree strings are NUL-terminated.
        board: text("proc/device-tree/model")
            .map(|m| m.trim_end_matches('\0').trim().to_owned())
            .filter(|m| !m.is_empty()),
        kernel: text("proc/sys/kernel/osrelease").map(|r| r.trim().to_owned()),
        memory_total_kb: kib(&meminfo, "MemTotal"),
        memory_available_kb: kib(&meminfo, "MemAvailable"),
        uptime_s: text("proc/uptime").and_then(|u| u.split('.').next()?.trim().parse().ok()),
        flash: text("proc/mtd").map(|t| flash(&t)).unwrap_or_default(),
        camera_count: cameras.len(),
        cameras,
    }
}

/// `key<tabs>: value` from a `/proc/cpuinfo`-style file.
fn field(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == key).then(|| v.trim().to_owned())
    })
}

/// `MemTotal:  32788 kB` → 32788.
fn kib(meminfo: &str, key: &str) -> Option<u64> {
    field(meminfo, key)?.split_whitespace().next()?.parse().ok()
}

/// `/proc/mtd`: `mtd6: 004d7000 00001000 "APP"`.
fn flash(text: &str) -> Vec<FlashPartition> {
    text.lines()
        .filter_map(|line| {
            let (device, rest) = line.split_once(':')?;
            let mut fields = rest.split_whitespace();
            let size_bytes = u64::from_str_radix(fields.next()?, 16).ok()?;
            let _erase = fields.next()?;
            let name = fields.collect::<Vec<_>>().join(" ");
            Some(FlashPartition {
                device: device.trim().to_owned(),
                name: name.trim_matches('"').to_owned(),
                size_bytes,
            })
        })
        .collect()
}

/// One entry per `v4l-subdevN`, with the `video-N-*` nodes counted.
fn cameras(class: &Path) -> Vec<Camera> {
    let Ok(entries) = std::fs::read_dir(class) else {
        return Vec::new();
    };
    let names: Vec<String> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    let mut cameras: Vec<Camera> = names
        .iter()
        .filter_map(|n| n.strip_prefix("v4l-subdev")?.parse::<u32>().ok())
        .map(|index| {
            let label = std::fs::read_to_string(class.join(format!("v4l-subdev{index}/name")))
                .unwrap_or_default();
            let mut words = label.split_whitespace();
            let sensor = words.next().unwrap_or("unknown").to_owned();
            let bus = words.next().map(str::to_owned);
            let prefix = format!("video-{index}-");
            let channels = names.iter().filter(|n| n.starts_with(&prefix)).count() as u32;
            Camera {
                index,
                sensor,
                bus,
                channels,
            }
        })
        .collect();
    cameras.sort_by_key(|c| c.index);
    cameras
}

/// `GET /api/hardware` → `Hardware`.
async fn hardware(State(state): State<AppState>, _: AuthUser) -> Result<Json<Hardware>, ApiError> {
    let root = state.config.sys_root.clone();
    let hardware = tokio::task::spawn_blocking(move || read(&root))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(hardware))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// The live AV100 dual-sensor unit, trimmed.
    fn av100(root: &Path) {
        write(
            root,
            "proc/cpuinfo",
            "processor\t: 0\nmodel name\t: ARM926EJ-S rev 5 (v5l)\nBogoMIPS\t: 441.34\n\
             Hardware\t: AK3918AV100\nRevision\t: 0000\n",
        );
        write(
            root,
            "proc/meminfo",
            "MemTotal:          32788 kB\nMemFree:  532 kB\nMemAvailable:      13368 kB\n",
        );
        write(root, "proc/sys/kernel/osrelease", "4.4.282\n");
        write(
            root,
            "proc/device-tree/model",
            "EVB_CBDM_KM01N_V1.0.0 board\0",
        );
        write(root, "proc/uptime", "91403.79 90000.00\n");
        write(
            root,
            "proc/mtd",
            "dev:    size   erasesize  name\nmtd0: 00040000 00001000 \"UBOOT\"\n\
             mtd6: 004d7000 00001000 \"APP\"\n",
        );
        for (node, name) in [
            ("v4l-subdev0", "sc2331 0-0030"),
            ("v4l-subdev1", "sc2331 0-0030"),
            ("video-0-0", "input_video0-0"),
            ("video-0-1", "input_video0-1"),
            ("video-0-2", "input_video0-2"),
            ("video-1-0", "input_video1-0"),
            ("video-1-1", "input_video1-1"),
            ("video-1-2", "input_video1-2"),
            ("isp-param-0", "isp-param-0"),
        ] {
            write(
                root,
                &format!("sys/class/video4linux/{node}/name"),
                &format!("{name}\n"),
            );
        }
    }

    #[test]
    fn reads_the_dual_sensor_av100() {
        let root = crate::tests::scratch("hardware");
        av100(&root);
        let hw = read(&root);
        assert_eq!(hw.soc.as_deref(), Some("AK3918AV100"));
        assert_eq!(hw.cpu.as_deref(), Some("ARM926EJ-S rev 5 (v5l)"));
        assert_eq!(hw.bogomips, Some(441.34));
        assert_eq!(hw.board.as_deref(), Some("EVB_CBDM_KM01N_V1.0.0 board"));
        assert_eq!(hw.kernel.as_deref(), Some("4.4.282"));
        assert_eq!(hw.memory_total_kb, Some(32788));
        assert_eq!(hw.memory_available_kb, Some(13368));
        assert_eq!(hw.uptime_s, Some(91403));
        assert_eq!(
            hw.flash,
            vec![
                FlashPartition {
                    device: "mtd0".into(),
                    name: "UBOOT".into(),
                    size_bytes: 0x40000
                },
                FlashPartition {
                    device: "mtd6".into(),
                    name: "APP".into(),
                    size_bytes: 0x4d7000
                },
            ]
        );
        assert_eq!(hw.camera_count, 2);
        assert_eq!(
            hw.cameras,
            vec![
                Camera {
                    index: 0,
                    sensor: "sc2331".into(),
                    bus: Some("0-0030".into()),
                    channels: 3
                },
                Camera {
                    index: 1,
                    sensor: "sc2331".into(),
                    bus: Some("0-0030".into()),
                    channels: 3
                },
            ]
        );
    }

    #[test]
    fn missing_files_leave_fields_empty() {
        let root = crate::tests::scratch("hardware-empty");
        let hw = read(&root);
        assert_eq!(hw, Hardware::default());
    }
}
