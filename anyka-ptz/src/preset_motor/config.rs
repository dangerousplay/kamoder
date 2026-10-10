//! Configuration of the AV100 backend: our own knobs, the motor settings read
//! from the vendor `device.config`, and the persisted last position.

use crate::error::{Error, Result};
use crate::preset::write_atomic;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The fastest status poll allowed. Every preset_motor ioctl `printk`s, so
/// polling faster than 20 Hz floods dmesg (ABI doc, "Driver rules").
pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Configuration of the AV100 `/dev/preset_motor` backend.
///
/// Available on every build so consumers can carry it in their own config.
///
/// # Units
///
/// The driver has no notion of angle: positions are "user steps" (10 motor
/// phase steps each), `0..=border` per axis, with 0 at the hard stop homing
/// drives into. The backend reports `steps / steps_per_degree_*` as
/// "degrees". **The real factor has not been measured yet** (hardware check
/// T18), so the default of 1.0 makes the reported "degrees" *raw steps*:
/// pan `0..=175`, tilt `0..=70` on the reference unit.
#[derive(Debug, Clone, PartialEq)]
pub struct PresetMotorConfig {
    /// Module loaded when the device node is missing (`ipc` loads it from
    /// the same path, and we stop `ipc`).
    pub module_path: PathBuf,
    /// Vendor settings file with the `motor_*` keys; `None` skips it and
    /// uses the defaults below. Read once at open.
    pub device_config: Option<PathBuf>,
    /// Overrides `motor_bottom_steps` (pan travel, user steps).
    pub border_pan_steps: Option<i32>,
    /// Overrides `motor_top_steps` (tilt travel, user steps).
    pub border_tilt_steps: Option<i32>,
    /// Overrides `motor_speed` (tick period − 1, ms; higher is slower).
    pub load_ms: Option<i32>,
    /// User steps per reported degree, pan. 1.0 = report raw steps.
    pub steps_per_degree_pan: f32,
    /// User steps per reported degree, tilt.
    pub steps_per_degree_tilt: f32,
    /// Flip the pan velocity on top of `motor_bottom_dir`.
    pub invert_pan: bool,
    /// Flip the tilt velocity on top of `motor_top_dir`.
    pub invert_tilt: bool,
    /// Last-position file. `None` lets [`open_auto`](crate::open_auto) put
    /// it next to the presets file; with neither, nothing is persisted and
    /// every open homes.
    pub position_path: Option<PathBuf>,
    /// When the module was already loaded, adopt the driver's position if it
    /// is non-zero and inside the borders (it survives our restarts, and a
    /// vendor `ipc` that ran first leaves it homed).
    pub trust_driver_position: bool,
    /// Step count of one continuous-move request (cmd 4). Must exceed both
    /// borders so a held pad always reaches the end stop.
    pub run_steps: i32,
    /// Status poll period while moving; at least [`MIN_POLL_INTERVAL`].
    pub poll_interval: Duration,
    /// While moving, `position()` re-reads the driver at most this often.
    pub position_max_age: Duration,
    /// How long to wait for `/dev/preset_motor` after `insmod`.
    pub node_timeout: Duration,
    /// How long drop waits for the motor to report idle before closing.
    pub close_timeout: Duration,
}

impl Default for PresetMotorConfig {
    fn default() -> Self {
        Self {
            module_path: PathBuf::from("/ipc/patch/ko/presetmotor.ko"),
            device_config: Some(PathBuf::from(VENDOR_DEVICE_CONFIG)),
            border_pan_steps: None,
            border_tilt_steps: None,
            load_ms: None,
            steps_per_degree_pan: 1.0,
            steps_per_degree_tilt: 1.0,
            invert_pan: false,
            invert_tilt: false,
            position_path: None,
            trust_driver_position: true,
            // What `ipc` uses for its continuous moves.
            run_steps: 3000,
            poll_interval: Duration::from_millis(100),
            position_max_age: Duration::from_millis(250),
            node_timeout: Duration::from_secs(2),
            close_timeout: Duration::from_secs(2),
        }
    }
}

/// Where the vendor firmware keeps its settings (`auto_upgrade.sh` and
/// `button_upgrade.sh` copy `custom_setting.ini` there; `ipc` reads it).
pub const VENDOR_DEVICE_CONFIG: &str = "/rom/device.config";

impl PresetMotorConfig {
    /// Checks our own knobs; the motor settings are checked once resolved.
    pub fn validate(&self) -> Result<()> {
        for (what, v) in [
            ("steps_per_degree_pan", self.steps_per_degree_pan),
            ("steps_per_degree_tilt", self.steps_per_degree_tilt),
        ] {
            if !(v.is_finite() && v > 0.0) {
                return Err(Error::invalid(what, "must be a positive number"));
            }
        }
        if self.poll_interval < MIN_POLL_INTERVAL {
            return Err(Error::invalid(
                "poll_interval",
                "at least 50 ms: every ioctl is logged to dmesg",
            ));
        }
        Ok(())
    }

    /// Reads `device_config` (if any) and applies the overrides.
    pub(crate) fn resolve(&self) -> Result<MotorSettings> {
        let mut settings = match &self.device_config {
            Some(path) => MotorSettings::load(path)?,
            None => MotorSettings::default(),
        };
        if let Some(v) = self.border_pan_steps {
            settings.border[0] = v;
        }
        if let Some(v) = self.border_tilt_steps {
            settings.border[1] = v;
        }
        if let Some(v) = self.load_ms {
            settings.load_ms = v;
        }
        settings.reverse[0] ^= self.invert_pan;
        settings.reverse[1] ^= self.invert_tilt;
        settings.validate(self.run_steps)?;
        Ok(settings)
    }
}

/// The motor parameters `ipc` takes from `device.config` (ABI doc §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MotorSettings {
    /// Travel in user steps: `[pan, tilt]` (`motor_bottom_steps`,
    /// `motor_top_steps`; "bottom" is the pan motor, "top" the tilt one).
    pub border: [i32; 2],
    /// Tick period − 1 in ms (`motor_speed`).
    pub load_ms: i32,
    /// A positive velocity on this axis runs towards step 0. Starts as
    /// `motor_*_dir ^ reset_sensor_direction`, like `vMotorRun`.
    pub reverse: [bool; 2],
}

impl Default for MotorSettings {
    /// The values in the reference unit's `/rom/device.config`.
    fn default() -> Self {
        Self {
            border: [175, 70],
            load_ms: 4,
            reverse: [false, true],
        }
    }
}

impl MotorSettings {
    /// Parses a vendor `key=value` file; a missing file gives the defaults.
    /// Keys absent from the file also keep their defaults.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    path = %path.display(),
                    "no vendor device.config; using the reference motor settings"
                );
                Ok(Self::default())
            }
            Err(source) => Err(Error::ReadDeviceConfig {
                path: path.to_owned(),
                source,
            }),
        }
    }

    pub(crate) fn parse(text: &str) -> Self {
        let mut s = Self::default();
        let mut dir = [None::<bool>; 2];
        let mut sensor_dir = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let Ok(value) = value.trim().parse::<i32>() else {
                continue;
            };
            match key.trim() {
                "motor_bottom_steps" => s.border[0] = value,
                "motor_top_steps" => s.border[1] = value,
                // ipc: `motor_speed` 0 means 5.
                "motor_speed" => s.load_ms = if value == 0 { 5 } else { value },
                "motor_bottom_dir" => dir[0] = Some(value != 0),
                "motor_top_dir" => dir[1] = Some(value != 0),
                "reset_sensor_direction" => sensor_dir = value != 0,
                _ => {}
            }
        }
        for axis in 0..2 {
            s.reverse[axis] = dir[axis].unwrap_or(s.reverse[axis]) ^ sensor_dir;
        }
        s
    }

    fn validate(&self, run_steps: i32) -> Result<()> {
        // The driver multiplies by 10 into u32 counters; keep far from that.
        if self.border.iter().any(|b| !(1..=10_000).contains(b)) {
            return Err(Error::invalid("motor border", "must be 1..=10000 steps"));
        }
        if !(1..=1000).contains(&self.load_ms) {
            return Err(Error::invalid(
                "load_ms",
                "must be 1..=1000 (0 is rejected by the driver)",
            ));
        }
        if self.border.iter().any(|&b| run_steps <= b) || run_steps > 100_000 {
            return Err(Error::invalid(
                "run_steps",
                "must exceed both borders (and stay below 100000)",
            ));
        }
        Ok(())
    }

    /// Time for one user step (10 ticks of `load_ms + 1` ms).
    fn step_time(&self) -> Duration {
        Duration::from_millis(10 * (self.load_ms as u64 + 1))
    }

    /// Full travel of the longer axis at the driver's single speed.
    pub(crate) fn travel_time(&self) -> Duration {
        self.step_time() * self.border[0].max(self.border[1]) as u32
    }

    /// Homing drives `border + 4` steps blind on each axis, concurrently
    /// (ABI doc §2.2); ≈ 9 s for 175 steps at `load_ms` 4.
    pub(crate) fn homing_time(&self) -> Duration {
        self.step_time() * (self.border[0].max(self.border[1]) + 4) as u32
    }
}

/// What the last-position file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedPosition {
    pub pan_steps: i32,
    pub tilt_steps: i32,
    /// Written as `true` when a move starts, `false` once it ends. A `true`
    /// on open means we lost power (or crashed) mid-move: the position is
    /// stale and the head must be homed.
    pub moving: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct PositionFile {
    version: u32,
    #[serde(flatten)]
    position: SavedPosition,
}

/// The last-position file; write failures are logged, never fatal.
#[derive(Debug)]
pub(crate) struct PositionStore {
    path: Option<PathBuf>,
    /// What the file holds now (as far as we know).
    last: Option<SavedPosition>,
}

impl PositionStore {
    /// Reads `path`; an unreadable or malformed file counts as "no position"
    /// (the cost is one homing run, not lost user data).
    pub(crate) fn open(path: Option<PathBuf>) -> Self {
        let last = path.as_deref().and_then(|p| match std::fs::read(p) {
            Ok(bytes) => match serde_json::from_slice::<PositionFile>(&bytes) {
                Ok(file) => Some(file.position),
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e, "ignoring malformed PTZ position file");
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "cannot read PTZ position file");
                None
            }
        });
        Self { path, last }
    }

    /// The saved position, if one exists and was not left mid-move.
    pub(crate) fn clean(&self) -> Option<[i32; 2]> {
        self.last
            .filter(|p| !p.moving)
            .map(|p| [p.pan_steps, p.tilt_steps])
    }

    /// Records that a move started (once per run of moves).
    pub(crate) fn mark_moving(&mut self) {
        let pos = match self.last {
            Some(p) if p.moving => return,
            Some(p) => [p.pan_steps, p.tilt_steps],
            None => [0, 0],
        };
        self.write(pos, true);
    }

    /// Records where the head came to rest.
    pub(crate) fn save(&mut self, pos: [i32; 2]) {
        self.write(pos, false);
    }

    fn write(&mut self, pos: [i32; 2], moving: bool) {
        let position = SavedPosition {
            pan_steps: pos[0],
            tilt_steps: pos[1],
            moving,
        };
        if self.last == Some(position) {
            return;
        }
        let Some(path) = &self.path else {
            self.last = Some(position);
            return;
        };
        let file = PositionFile {
            version: 1,
            position,
        };
        let result = serde_json::to_vec(&file)
            .map_err(std::io::Error::other)
            .and_then(|mut bytes| {
                bytes.push(b'\n');
                write_atomic(path, &bytes)
            });
        match result {
            Ok(()) => self.last = Some(position),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot save PTZ position")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    const VENDOR: &str = "#motor\nsupport_preset_motor_drive=1\nmotor_speed=4\nmotor_top_dir=1\n\
                          motor_top_scale=16\nmotor_top_steps=70\nmotor_bottom_dir=0\n\
                          motor_bottom_scale=27\nmotor_bottom_steps=175\n# motor_speed=9\n";

    #[test]
    fn parses_the_vendor_file() {
        let s = MotorSettings::parse(VENDOR);
        assert_eq!(s, MotorSettings::default());
        assert_eq!(s.border, [175, 70]);
        assert_eq!(s.load_ms, 4, "commented-out lines are ignored");
        assert_eq!(s.reverse, [false, true]);

        let s = MotorSettings::parse(
            "motor_bottom_steps = 200\nmotor_speed=0\nreset_sensor_direction=1\njunk\nmotor_top_steps=x",
        );
        assert_eq!(s.border, [200, 70]);
        assert_eq!(s.load_ms, 5, "motor_speed 0 means 5, as in ipc");
        assert_eq!(s.reverse, [true, false], "sensor direction flips both");
    }

    #[test]
    fn timings_follow_the_driver_arithmetic() {
        let s = MotorSettings::default();
        // (175 + 4) steps × 10 ticks × 5 ms.
        assert_eq!(s.homing_time(), Duration::from_millis(8950));
        assert_eq!(s.travel_time(), Duration::from_millis(8750));
    }

    #[test]
    fn resolve_applies_overrides_and_validates() {
        let dir = TempDir::new("pm-config");
        let path = dir.path().join("device.config");
        std::fs::write(&path, VENDOR).unwrap();
        let config = PresetMotorConfig {
            device_config: Some(path),
            border_tilt_steps: Some(60),
            invert_pan: true,
            invert_tilt: true,
            ..PresetMotorConfig::default()
        };
        let s = config.resolve().unwrap();
        assert_eq!(s.border, [175, 60]);
        assert_eq!(s.reverse, [true, false]);

        let missing = PresetMotorConfig {
            device_config: Some(dir.path().join("nope")),
            ..PresetMotorConfig::default()
        };
        assert_eq!(missing.resolve().unwrap(), MotorSettings::default());

        let bad = PresetMotorConfig {
            device_config: None,
            load_ms: Some(0),
            ..PresetMotorConfig::default()
        };
        assert!(bad.resolve().is_err());
        let bad = PresetMotorConfig {
            device_config: None,
            run_steps: 100,
            ..PresetMotorConfig::default()
        };
        assert!(bad.resolve().is_err(), "run_steps below the pan border");
    }

    #[test]
    fn validate_enforces_the_poll_floor() {
        assert!(PresetMotorConfig::default().validate().is_ok());
        let fast = PresetMotorConfig {
            poll_interval: Duration::from_millis(20),
            ..PresetMotorConfig::default()
        };
        assert!(fast.validate().is_err());
        let bad = PresetMotorConfig {
            steps_per_degree_pan: 0.0,
            ..PresetMotorConfig::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn position_store_round_trips_and_tracks_moves() {
        let dir = TempDir::new("pm-position");
        let path = dir.path().join("ptz-position.json");
        let mut store = PositionStore::open(Some(path.clone()));
        assert_eq!(store.clean(), None);
        store.save([85, 19]);
        assert_eq!(
            PositionStore::open(Some(path.clone())).clean(),
            Some([85, 19])
        );

        store.mark_moving();
        let reopened = PositionStore::open(Some(path.clone()));
        assert_eq!(reopened.clean(), None, "a move in flight is not trusted");
        assert_eq!(
            reopened.last,
            Some(SavedPosition {
                pan_steps: 85,
                tilt_steps: 19,
                moving: true
            })
        );
        store.save([10, 5]);
        assert_eq!(
            PositionStore::open(Some(path.clone())).clean(),
            Some([10, 5])
        );

        std::fs::write(&path, b"garbage").unwrap();
        assert_eq!(PositionStore::open(Some(path)).clean(), None);
    }
}
