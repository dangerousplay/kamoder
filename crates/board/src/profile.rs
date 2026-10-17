//! Profile types (the `boards/<name>.toml` schema), loading and validation.
//!
//! Every struct denies unknown keys: a typo such as `kpbs = 4096` would
//! otherwise be ignored and the camera would quietly run on the default.
//! Only `name`, `[match]` and `[video]` are required; a board without lights,
//! Wi-Fi or PTZ leaves the section out and the matching service is skipped.
//!
//! [`Profile::validate`] holds the rules serde cannot express (BRD-04),
//! mostly about GPIOs: two roles on one pin, or a light on the Wi-Fi power
//! pin, would let one service switch off another's hardware.

use crate::{InvalidSnafu, ParseSnafu, ReadSnafu, Result};
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

/// One board model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Equals the file stem, so `BOARD=<name>` finds `<name>.toml`.
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Detection rules. Empty for a profile that is only ever chosen by
    /// name or as the fallback.
    #[serde(rename = "match")]
    pub match_rules: MatchRules,
    #[serde(default)]
    pub platform: Option<Platform>,
    pub video: Video,
    #[serde(default)]
    pub lights: Option<Lights>,
    #[serde(default)]
    pub wifi: Option<Wifi>,
    #[serde(default)]
    pub ptz: Option<Ptz>,
    /// Microphone and speaker. Absent: no audio service, no RTSP audio
    /// track, no ONVIF audio source (AUD-07).
    #[serde(default)]
    pub audio: Option<Audio>,
}

/// All listed rules must hold; the profile's score is how many there are.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchRules {
    /// Exact `Hardware` value of `/proc/cpuinfo`.
    #[serde(default)]
    pub cpuinfo_hardware: Option<String>,
    /// Substring of one entry of `/proc/device-tree/compatible`.
    #[serde(default)]
    pub dt_compatible: Option<String>,
    /// Substring of `/proc/device-tree/model`.
    #[serde(default)]
    pub dt_model: Option<String>,
    /// Absolute paths that must exist; each counts as one rule.
    #[serde(default)]
    pub path_exists: Vec<PathBuf>,
}

impl MatchRules {
    /// How many rules are listed (the score when they all hold).
    pub fn count(&self) -> usize {
        usize::from(self.cpuinfo_hardware.is_some())
            + usize::from(self.dt_compatible.is_some())
            + usize::from(self.dt_model.is_some())
            + self.path_exists.len()
    }
}

/// What the stock firmware leaves running that must be dealt with at boot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    /// Vendor processes `init.sh` stops before the Rust services start.
    #[serde(default)]
    pub vendor_processes: Vec<String>,
    /// The vendor app holds `/dev/watchdog` at boot, so the feeder has to
    /// wait for it to be released instead of failing on EBUSY.
    #[serde(default)]
    pub watchdog_wait: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Video {
    /// Capture backend: `anyka-av100`, `test-pattern`. Kept a string so a
    /// profile naming a backend this build lacks still loads and the camera
    /// service can say so, instead of the whole profile being rejected.
    pub backend: String,
    /// Sensor indices the backend is expected to bring up; RTSP mounts one
    /// stream per entry.
    pub sensors: Vec<u32>,
    /// ISP tuning files, for backends that need them.
    #[serde(default)]
    pub tuning_dir: Option<PathBuf>,
    /// Encoder settings for every sensor, unless overridden.
    pub stream: Stream,
    /// Per-sensor exceptions to `stream`.
    #[serde(default, rename = "override")]
    pub overrides: Vec<StreamOverride>,
    #[serde(default)]
    pub snapshot: Snapshot,
}

impl Video {
    /// The stream settings of sensor `index`: `stream` with that sensor's
    /// override applied.
    pub fn stream_for(&self, index: u32) -> Stream {
        let mut s = self.stream.clone();
        if let Some(o) = self.overrides.iter().find(|o| o.index == index) {
            s.width = o.width.unwrap_or(s.width);
            s.height = o.height.unwrap_or(s.height);
            s.fps = o.fps.unwrap_or(s.fps);
            s.kbps = o.kbps.unwrap_or(s.kbps);
            s.gop = o.gop.unwrap_or(s.gop);
            if let Some(profile) = &o.profile {
                s.profile.clone_from(profile);
            }
        }
        s
    }
}

/// H.264 stream settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stream {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Constant bitrate, kbit/s.
    pub kbps: u32,
    /// Frames between IDR pictures.
    pub gop: u32,
    /// H.264 profile: `baseline`, `main` or `high`.
    pub profile: String,
}

/// Fields of [`Stream`] that differ for one sensor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamOverride {
    pub index: u32,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub fps: Option<u32>,
    #[serde(default)]
    pub kbps: Option<u32>,
    #[serde(default)]
    pub gop: Option<u32>,
    #[serde(default)]
    pub profile: Option<String>,
}

/// JPEG snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// Frames are halved while wider than this; `None` keeps full size.
    #[serde(default)]
    pub max_width: Option<u32>,
    /// JPEG quality, 1..=100.
    #[serde(default = "default_quality")]
    pub quality: u8,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            max_width: None,
            quality: default_quality(),
        }
    }
}

fn default_quality() -> u8 {
    80
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lights {
    /// The sysfs GPIO directory, for lines given by number.
    #[serde(default = "default_gpio_root")]
    pub gpio_root: PathBuf,
    #[serde(default, rename = "line")]
    pub lines: Vec<LightLine>,
}

fn default_gpio_root() -> PathBuf {
    PathBuf::from("/sys/class/gpio")
}

/// One light (or the speaker amplifier), addressed by sysfs GPIO number or
/// by the path of a directory holding a `value` file (`/sys/user-gpio/*`
/// on boards whose kernel exports named lines). Exactly one of the two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LightLine {
    pub role: Role,
    #[serde(default)]
    pub gpio: Option<u32>,
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Logical "on" drives the pin low.
    #[serde(default)]
    pub active_low: bool,
}

/// What a light line is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    IrCut,
    IrLeds,
    White,
    Alarm,
    Amp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wifi {
    pub iface: String,
    /// Kernel module (`.ko` path) that brings the interface up.
    #[serde(default)]
    pub module: Option<PathBuf>,
    #[serde(default)]
    pub module_args: Vec<String>,
    /// GPIO switching the module's power. Without it the power-cycle
    /// recovery step is skipped.
    #[serde(default)]
    pub power_gpio: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ptz {
    pub backend: PtzBackend,
    /// Driver device node.
    #[serde(default)]
    pub device: Option<PathBuf>,
    /// Kernel module that creates `device`.
    #[serde(default)]
    pub module: Option<PathBuf>,
    /// Pan range, degrees `[min, max]`.
    pub pan: [f32; 2],
    /// Tilt range, degrees `[min, max]`.
    pub tilt: [f32; 2],
    #[serde(default = "default_steps_per_degree")]
    pub steps_per_degree: f32,
    /// Motion following (D-F5). Absent: the board does not follow; an empty
    /// `[ptz.follow]` turns it on with the defaults of [`Follow`].
    #[serde(default)]
    pub follow: Option<Follow>,
}

fn default_steps_per_degree() -> f32 {
    1.0
}

/// Tuning of the follow-movement controller, `[ptz.follow]`.
///
/// Every key is optional; the defaults below are first guesses, not
/// measurements. `fov_deg` in particular was never measured on the AV100
/// and `t_on`/`t_off` are in the units of the ISP's u16 per-cell motion
/// energy, so both want tuning on hardware.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Follow {
    /// Motion grid samples per second, 1..=25.
    pub hz: u8,
    /// Centre dead zone as permille of the frame `[x, y]`: a target inside
    /// it does not move the head.
    pub dead_zone_permille: [u16; 2],
    /// Largest single move, degrees.
    pub max_step_deg: f32,
    /// Horizontal and vertical field of view, degrees; turns a grid offset
    /// into an angle. Unmeasured: tune on hardware.
    pub fov_deg: [f32; 2],
    /// Consecutive grids that must agree before the head moves.
    pub decision_frames: u8,
    /// Grids are ignored for this long after a move, while the image settles.
    pub settle_ms: u32,
    /// Seconds without motion before the head returns to `home`.
    pub return_after_s: u32,
    /// Seconds following stays paused after a manual (ONVIF/web) move.
    pub manual_hold_s: u32,
    /// Cell energy at which a cell becomes active. Tune on hardware.
    pub t_on: u16,
    /// Cell energy below which an active cell turns inactive (hysteresis);
    /// must be below `t_on`. Tune on hardware.
    pub t_off: u16,
    /// Fewest active cells that count as a target (filters noise).
    pub min_cells: u16,
    /// Active cells above this permille of the grid are a global change
    /// (light switch, IR-cut flip, own motion), not a target.
    pub max_fraction_permille: u16,
    /// Where to return to, `[pan, tilt]` degrees; `None` = the PTZ home.
    pub home: Option<[f32; 2]>,
}

impl Default for Follow {
    fn default() -> Self {
        Self {
            hz: 10,
            dead_zone_permille: [300, 400],
            max_step_deg: 15.0,
            fov_deg: [90.0, 50.0],
            decision_frames: 5,
            settle_ms: 300,
            return_after_s: 15,
            manual_hold_s: 60,
            t_on: 600,
            t_off: 300,
            min_cells: 2,
            max_fraction_permille: 600,
            home: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PtzBackend {
    /// `presetmotor.ko` driven directly.
    PresetMotor,
    /// The vendor SDK.
    Sdk,
    /// Simulated motor, for tests and the VM.
    Sim,
}

/// The board's audio codec, `[audio]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audio {
    /// Codec driver the `audio` service talks to, e.g. `ak-pcm`.
    pub backend: String,
    /// Rate asked of the driver, Hz. The wire format is always 8 kHz; a
    /// higher rate here is decimated in software (D-A5).
    #[serde(default = "default_audio_rate")]
    pub rate: u32,
    /// Microphones, `[[audio.input]]`.
    #[serde(default, rename = "input")]
    pub inputs: Vec<AudioDevice>,
    /// Speakers, `[[audio.output]]`.
    #[serde(default, rename = "output")]
    pub outputs: Vec<AudioDevice>,
}

fn default_audio_rate() -> u32 {
    8000
}

/// One microphone or speaker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioDevice {
    /// Stable id used by the API and ONVIF tokens, e.g. `mic0`.
    pub id: String,
    /// Shown in the web UI.
    pub name: String,
    /// Driver device node.
    pub device: PathBuf,
    /// Gain (input) or volume (output) applied at start, in driver units.
    #[serde(default)]
    pub gain: Option<u32>,
    /// Inclusive driver range of `gain`.
    pub gain_range: [u32; 2],
}

/// A broken profile rule: which key and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Dotted path, e.g. `lights.line[2].gpio`.
    pub key: String,
    pub reason: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.key, self.reason)
    }
}

impl std::error::Error for Violation {}

fn violation(key: impl Into<String>, reason: impl Into<String>) -> Violation {
    Violation {
        key: key.into(),
        reason: reason.into(),
    }
}

/// Reads, parses and validates one profile.
pub fn load(path: &Path) -> Result<Profile> {
    let text = std::fs::read_to_string(path).context(ReadSnafu { path })?;
    let profile: Profile = toml::from_str(&text).context(ParseSnafu { path })?;
    let invalid = |v: Violation| {
        InvalidSnafu {
            file: path,
            key: v.key,
            reason: v.reason,
        }
        .build()
    };
    profile.validate().map_err(invalid)?;
    let stem = path.file_stem().and_then(|s| s.to_str());
    if stem != Some(profile.name.as_str()) {
        return Err(invalid(violation(
            "name",
            format!(
                "{:?} must equal the file name without .toml ({stem:?})",
                profile.name
            ),
        )));
    }
    Ok(profile)
}

impl Profile {
    /// Checks the rules serde cannot (BRD-04). Stops at the first violation.
    pub fn validate(&self) -> Result<(), Violation> {
        if self.name.trim().is_empty() {
            return Err(violation("name", "must not be empty"));
        }
        self.video.validate()?;
        if let Some(lights) = &self.lights {
            lights.validate(self.wifi.as_ref().and_then(|w| w.power_gpio))?;
        }
        if let Some(wifi) = &self.wifi {
            if wifi.iface.trim().is_empty() {
                return Err(violation("wifi.iface", "must not be empty"));
            }
        }
        if let Some(ptz) = &self.ptz {
            ptz.validate()?;
        }
        if let Some(audio) = &self.audio {
            audio.validate()?;
        }
        Ok(())
    }
}

impl Audio {
    fn validate(&self) -> Result<(), Violation> {
        if self.backend.trim().is_empty() {
            return Err(violation("audio.backend", "must not be empty"));
        }
        if self.rate == 0 || !self.rate.is_multiple_of(8000) || self.rate > 48000 {
            return Err(violation(
                "audio.rate",
                format!(
                    "{} must be 8000, 16000, 24000, 32000, 40000 or 48000",
                    self.rate
                ),
            ));
        }
        if self.inputs.is_empty() && self.outputs.is_empty() {
            return Err(violation("audio", "needs at least one input or output"));
        }
        let mut ids = HashSet::new();
        let devices = self
            .inputs
            .iter()
            .map(|d| ("input", d))
            .chain(self.outputs.iter().map(|d| ("output", d)));
        for (i, (kind, device)) in devices.enumerate() {
            let index = if kind == "input" {
                i
            } else {
                i - self.inputs.len()
            };
            let key = format!("audio.{kind}[{index}]");
            if device.id.trim().is_empty() || !ids.insert(device.id.as_str()) {
                return Err(violation(
                    format!("{key}.id"),
                    format!("{:?} must be non-empty and unique", device.id),
                ));
            }
            let [min, max] = device.gain_range;
            if min > max {
                return Err(violation(
                    format!("{key}.gain_range"),
                    format!("[{min}, {max}] must be [min, max]"),
                ));
            }
            if let Some(gain) = device.gain {
                if !(min..=max).contains(&gain) {
                    return Err(violation(
                        format!("{key}.gain"),
                        format!("{gain} is outside [{min}, {max}]"),
                    ));
                }
            }
        }
        Ok(())
    }
}

impl Video {
    fn validate(&self) -> Result<(), Violation> {
        if self.backend.trim().is_empty() {
            return Err(violation("video.backend", "must not be empty"));
        }
        if self.sensors.is_empty() {
            return Err(violation("video.sensors", "must list at least one sensor"));
        }
        let mut seen = HashSet::new();
        if let Some(dup) = self.sensors.iter().find(|s| !seen.insert(**s)) {
            return Err(violation(
                "video.sensors",
                format!("sensor {dup} listed twice"),
            ));
        }
        self.stream.validate("video.stream")?;
        let mut seen = HashSet::new();
        for (i, o) in self.overrides.iter().enumerate() {
            let key = format!("video.override[{i}]");
            if !self.sensors.contains(&o.index) {
                return Err(violation(
                    format!("{key}.index"),
                    format!("sensor {} is not in video.sensors", o.index),
                ));
            }
            if !seen.insert(o.index) {
                return Err(violation(
                    format!("{key}.index"),
                    format!("sensor {} overridden twice", o.index),
                ));
            }
            self.stream_for(o.index).validate(&key)?;
        }
        if let Some(w) = self.snapshot.max_width {
            if w == 0 || w % 2 != 0 {
                return Err(violation(
                    "video.snapshot.max_width",
                    format!("{w} must be even and non-zero"),
                ));
            }
        }
        if !(1..=100).contains(&self.snapshot.quality) {
            return Err(violation(
                "video.snapshot.quality",
                format!("{} is outside 1..=100", self.snapshot.quality),
            ));
        }
        Ok(())
    }
}

impl Stream {
    /// `key` is where the settings came from, for the error.
    fn validate(&self, key: &str) -> Result<(), Violation> {
        // The encoder works on 4:2:0 pictures, so both sides must be even.
        for (field, v) in [("width", self.width), ("height", self.height)] {
            if v == 0 || v % 2 != 0 {
                return Err(violation(
                    format!("{key}.{field}"),
                    format!("{v} must be even and non-zero"),
                ));
            }
        }
        let ranges = [
            ("fps", self.fps, 1..=60),
            ("kbps", self.kbps, 64..=8000),
            ("gop", self.gop, 1..=600),
        ];
        for (field, v, range) in ranges {
            if !range.contains(&v) {
                return Err(violation(
                    format!("{key}.{field}"),
                    format!("{v} is outside {}..={}", range.start(), range.end()),
                ));
            }
        }
        if !matches!(self.profile.as_str(), "baseline" | "main" | "high") {
            return Err(violation(
                format!("{key}.profile"),
                format!("{:?} is not baseline, main or high", self.profile),
            ));
        }
        Ok(())
    }
}

impl Lights {
    fn validate(&self, wifi_power_gpio: Option<u32>) -> Result<(), Violation> {
        let mut roles = HashSet::new();
        let mut gpios = HashSet::new();
        for (i, line) in self.lines.iter().enumerate() {
            let key = format!("lights.line[{i}]");
            if !roles.insert(line.role) {
                return Err(violation(
                    format!("{key}.role"),
                    format!("role {:?} listed twice", line.role),
                ));
            }
            let gpio = match (line.gpio, &line.path) {
                (Some(gpio), None) => gpio,
                (None, Some(_)) => continue,
                _ => return Err(violation(key, "needs exactly one of `gpio` and `path`")),
            };
            if !gpios.insert(gpio) {
                return Err(violation(
                    format!("{key}.gpio"),
                    format!("GPIO {gpio} used by two lines"),
                ));
            }
            // PER-03: the lights service must never be able to cut the
            // Wi-Fi module's power.
            if wifi_power_gpio == Some(gpio) {
                return Err(violation(
                    format!("{key}.gpio"),
                    format!("GPIO {gpio} is wifi.power_gpio"),
                ));
            }
        }
        Ok(())
    }
}

impl Ptz {
    fn validate(&self) -> Result<(), Violation> {
        for (field, [min, max]) in [("pan", self.pan), ("tilt", self.tilt)] {
            if !(min.is_finite() && max.is_finite() && min < max) {
                return Err(violation(
                    format!("ptz.{field}"),
                    format!("[{min}, {max}] must be [min, max] with min < max"),
                ));
            }
        }
        if !(self.steps_per_degree.is_finite() && self.steps_per_degree > 0.0) {
            return Err(violation(
                "ptz.steps_per_degree",
                format!("{} must be positive", self.steps_per_degree),
            ));
        }
        if let Some(follow) = &self.follow {
            follow.validate(self)?;
        }
        Ok(())
    }
}

impl Follow {
    /// `ptz` is the enclosing section, for the range of `home`.
    fn validate(&self, ptz: &Ptz) -> Result<(), Violation> {
        const KEY: &str = "ptz.follow";
        if !(1..=25).contains(&self.hz) {
            return Err(violation(
                format!("{KEY}.hz"),
                format!("{} is outside 1..=25", self.hz),
            ));
        }
        if self.t_off >= self.t_on {
            return Err(violation(
                format!("{KEY}.t_off"),
                format!("{} must be below t_on ({})", self.t_off, self.t_on),
            ));
        }
        let permilles = [
            ("dead_zone_permille", self.dead_zone_permille[0]),
            ("dead_zone_permille", self.dead_zone_permille[1]),
            ("max_fraction_permille", self.max_fraction_permille),
        ];
        for (field, v) in permilles {
            if v > 1000 {
                return Err(violation(
                    format!("{KEY}.{field}"),
                    format!("{v} is above 1000"),
                ));
            }
        }
        let positive = [
            ("fov_deg", self.fov_deg[0]),
            ("fov_deg", self.fov_deg[1]),
            ("max_step_deg", self.max_step_deg),
        ];
        for (field, v) in positive {
            if !(v.is_finite() && v > 0.0) {
                return Err(violation(
                    format!("{KEY}.{field}"),
                    format!("{v} must be positive"),
                ));
            }
        }
        if self.decision_frames == 0 {
            return Err(violation(
                format!("{KEY}.decision_frames"),
                "must be at least 1",
            ));
        }
        if let Some([pan, tilt]) = self.home {
            let inside = |v: f32, [min, max]: [f32; 2]| v.is_finite() && (min..=max).contains(&v);
            if !(inside(pan, ptz.pan) && inside(tilt, ptz.tilt)) {
                return Err(violation(
                    format!("{KEY}.home"),
                    format!("[{pan}, {tilt}] is outside ptz.pan / ptz.tilt"),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use crate::Error;

    /// Every section filled in, valid.
    const SAMPLE: &str = r#"
name = "sample"
description = "test board"

[match]
cpuinfo_hardware = "SOC1"
path_exists = ["/dev/thing"]

[platform]
vendor_processes = ["ipc"]
watchdog_wait = true

[video]
backend = "anyka-av100"
sensors = [0, 1]
tuning_dir = "/usr/local"

[video.stream]
width = 1280
height = 720
fps = 15
kbps = 2048
gop = 30
profile = "main"

[[video.override]]
index = 1
width = 640
height = 360

[video.snapshot]
max_width = 640
quality = 80

[lights]
[[lights.line]]
role = "ir_cut"
gpio = 19

[[lights.line]]
role = "amp"
path = "/sys/user-gpio/amp"
active_low = true

[wifi]
iface = "wlan0"
power_gpio = 98

[ptz]
backend = "preset_motor"
pan = [0, 175]
tilt = [0, 70]
"#;

    fn parse(text: &str) -> Profile {
        toml::from_str(text).expect("parse")
    }

    /// Writes `text` as `<dir>/sample.toml` and loads it.
    fn load_text(dir: &TempDir, text: &str) -> (PathBuf, Result<Profile>) {
        let path = dir.path().join("sample.toml");
        std::fs::write(&path, text).unwrap();
        let profile = load(&path);
        (path, profile)
    }

    fn violation_of(text: &str) -> Violation {
        parse(text).validate().expect_err("should be invalid")
    }

    #[test]
    fn valid_sample_loads_with_defaults_and_overrides() {
        let dir = TempDir::new("valid");
        let (_, profile) = load_text(&dir, SAMPLE);
        let p = profile.expect("valid sample");
        assert_eq!(p.name, "sample");
        assert_eq!(p.match_rules.count(), 2);
        let lights = p.lights.as_ref().unwrap();
        assert_eq!(lights.gpio_root, Path::new("/sys/class/gpio"));
        assert_eq!(lights.lines[1].role, Role::Amp);
        assert!(lights.lines[1].active_low);
        assert_eq!(p.ptz.as_ref().unwrap().steps_per_degree, 1.0);
        assert_eq!(p.ptz.as_ref().unwrap().backend, PtzBackend::PresetMotor);
        let s1 = p.video.stream_for(1);
        assert_eq!((s1.width, s1.height, s1.kbps), (640, 360, 2048));
        assert_eq!(p.video.stream_for(0), p.video.stream);
    }

    #[test]
    fn optional_sections_may_be_left_out() {
        let p = parse(
            "name = \"x\"\n[match]\n[video]\nbackend = \"test-pattern\"\nsensors = [0]\n\
             [video.stream]\nwidth = 640\nheight = 360\nfps = 15\nkbps = 1024\ngop = 30\n\
             profile = \"main\"\n",
        );
        assert!(p.platform.is_none() && p.lights.is_none() && p.wifi.is_none());
        assert_eq!(p.video.snapshot, Snapshot::default());
        p.validate().unwrap();
    }

    #[test]
    fn unknown_key_is_rejected_naming_file_and_key() {
        let dir = TempDir::new("unknown");
        let text = SAMPLE.replace("kbps = 2048", "kbps = 2048\nkpbs = 4096");
        let (path, result) = load_text(&dir, &text);
        let err = result.expect_err("unknown key");
        assert!(matches!(err, Error::Parse { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        assert!(msg.contains("kpbs"), "{msg}");
    }

    #[test]
    fn invalid_error_names_file_and_key() {
        let dir = TempDir::new("invalid");
        let text = SAMPLE.replace("fps = 15", "fps = 0");
        let (path, result) = load_text(&dir, &text);
        match result {
            Err(Error::Invalid { file, key, reason }) => {
                assert_eq!(file, path);
                assert_eq!(key, "video.stream.fps");
                assert!(reason.contains("1..=60"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn name_must_match_file_stem() {
        let dir = TempDir::new("stem");
        let path = dir.path().join("other.toml");
        std::fs::write(&path, SAMPLE).unwrap();
        let err = load(&path).expect_err("stem mismatch");
        assert!(
            matches!(err, Error::Invalid { ref key, .. } if key == "name"),
            "{err:?}"
        );
    }

    #[test]
    fn duplicate_role_is_rejected() {
        let v = violation_of(&SAMPLE.replace("role = \"amp\"", "role = \"ir_cut\""));
        assert_eq!(v.key, "lights.line[1].role");
    }

    #[test]
    fn duplicate_gpio_is_rejected() {
        let text = SAMPLE.replace("path = \"/sys/user-gpio/amp\"", "gpio = 19");
        let v = violation_of(&text);
        assert_eq!(v.key, "lights.line[1].gpio");
        assert!(v.reason.contains("19"), "{v}");
    }

    #[test]
    fn wifi_power_gpio_in_lights_is_rejected() {
        let v = violation_of(&SAMPLE.replace("gpio = 19", "gpio = 98"));
        assert_eq!(v.key, "lights.line[0].gpio");
        assert!(v.reason.contains("wifi.power_gpio"), "{v}");
    }

    #[test]
    fn line_needs_exactly_one_of_gpio_and_path() {
        let both = SAMPLE.replace("gpio = 19", "gpio = 19\npath = \"/x\"");
        assert_eq!(violation_of(&both).key, "lights.line[0]");
        let neither = SAMPLE.replace("gpio = 19\n", "");
        assert_eq!(violation_of(&neither).key, "lights.line[0]");
    }

    #[test]
    fn stream_rules() {
        let odd = SAMPLE.replace("width = 1280", "width = 1281");
        assert_eq!(violation_of(&odd).key, "video.stream.width");
        let kbps = SAMPLE.replace("kbps = 2048", "kbps = 9000");
        assert_eq!(violation_of(&kbps).key, "video.stream.kbps");
        let profile = SAMPLE.replace("profile = \"main\"", "profile = \"extended\"");
        assert_eq!(violation_of(&profile).key, "video.stream.profile");
        // The merged override is checked too.
        let over = SAMPLE.replace("width = 640\nheight = 360", "width = 641");
        assert_eq!(violation_of(&over).key, "video.override[0].width");
        let stray = SAMPLE.replace("index = 1", "index = 5");
        assert_eq!(violation_of(&stray).key, "video.override[0].index");
    }

    #[test]
    fn ptz_ranges_must_be_ordered() {
        let v = violation_of(&SAMPLE.replace("pan = [0, 175]", "pan = [175, 0]"));
        assert_eq!(v.key, "ptz.pan");
    }

    /// SAMPLE with `[ptz.follow]` holding `body`.
    fn with_follow(body: &str) -> String {
        format!("{SAMPLE}\n[ptz.follow]\n{body}\n")
    }

    #[test]
    fn follow_absent_is_none() {
        assert_eq!(parse(SAMPLE).ptz.unwrap().follow, None);
    }

    #[test]
    fn follow_empty_table_takes_defaults() {
        let p = parse(&with_follow(""));
        p.validate().unwrap();
        let f = p.ptz.unwrap().follow.expect("table present");
        assert_eq!(f, Follow::default());
        assert_eq!(f.hz, 10);
        assert_eq!(f.dead_zone_permille, [300, 400]);
        assert_eq!(f.fov_deg, [90.0, 50.0]);
        assert_eq!((f.t_on, f.t_off), (600, 300));
        assert_eq!(f.home, None);
    }

    #[test]
    fn follow_keys_override_defaults() {
        let p = parse(&with_follow("hz = 5\nhome = [80, 20]\nt_on = 900"));
        p.validate().unwrap();
        let f = p.ptz.unwrap().follow.unwrap();
        assert_eq!((f.hz, f.t_on, f.t_off), (5, 900, 300));
        assert_eq!(f.home, Some([80.0, 20.0]));
    }

    #[test]
    fn follow_unknown_key_is_rejected() {
        let err = toml::from_str::<Profile>(&with_follow("hertz = 5")).unwrap_err();
        assert!(err.to_string().contains("hertz"), "{err}");
    }

    #[test]
    fn follow_invalid_values_are_rejected() {
        let cases = [
            ("hz = 0", "ptz.follow.hz"),
            ("hz = 26", "ptz.follow.hz"),
            ("t_on = 300\nt_off = 300", "ptz.follow.t_off"),
            (
                "dead_zone_permille = [300, 1001]",
                "ptz.follow.dead_zone_permille",
            ),
            (
                "max_fraction_permille = 1001",
                "ptz.follow.max_fraction_permille",
            ),
            ("fov_deg = [0, 50]", "ptz.follow.fov_deg"),
            ("fov_deg = [90, -1]", "ptz.follow.fov_deg"),
            ("max_step_deg = 0", "ptz.follow.max_step_deg"),
            ("decision_frames = 0", "ptz.follow.decision_frames"),
            ("home = [200, 10]", "ptz.follow.home"),
        ];
        for (body, key) in cases {
            assert_eq!(violation_of(&with_follow(body)).key, key, "{body}");
        }
    }

    #[test]
    fn follow_survives_the_json_handoff() {
        let ptz = parse(&with_follow("home = [10, 5]")).ptz.unwrap();
        let json = serde_json::to_string(&ptz).unwrap();
        assert_eq!(serde_json::from_str::<Ptz>(&json).unwrap(), ptz);
    }

    /// SAMPLE with an `[audio]` section: one mic, one speaker.
    fn with_audio(extra: &str) -> String {
        format!(
            "{SAMPLE}\n[audio]\nbackend = \"ak-pcm\"\n{extra}\n\
             [[audio.input]]\nid = \"mic0\"\nname = \"Mic\"\ndevice = \"/dev/c\"\ngain = 3\ngain_range = [0, 7]\n\
             [[audio.output]]\nid = \"spk0\"\nname = \"Speaker\"\ndevice = \"/dev/p\"\ngain_range = [0, 6]\n"
        )
    }

    #[test]
    fn audio_absent_is_none() {
        assert_eq!(parse(SAMPLE).audio, None);
    }

    #[test]
    fn audio_parses_with_default_rate() {
        let p = parse(&with_audio(""));
        p.validate().unwrap();
        let a = p.audio.unwrap();
        assert_eq!(a.rate, 8000);
        assert_eq!(a.inputs[0].id, "mic0");
        assert_eq!(a.inputs[0].gain, Some(3));
        assert_eq!(a.outputs[0].gain_range, [0, 6]);
    }

    #[test]
    fn audio_invalid_values_are_rejected() {
        assert_eq!(violation_of(&with_audio("rate = 11025")).key, "audio.rate");
        let dup = with_audio("").replace("\"spk0\"", "\"mic0\"");
        assert_eq!(violation_of(&dup).key, "audio.output[0].id");
        let gain = with_audio("").replace("gain = 3", "gain = 9");
        assert_eq!(violation_of(&gain).key, "audio.input[0].gain");
        let range = with_audio("").replace("[0, 6]", "[6, 0]");
        assert_eq!(violation_of(&range).key, "audio.output[0].gain_range");
    }
}
