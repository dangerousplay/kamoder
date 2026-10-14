//! Pan/tilt control for Anyka AK3918 cameras behind one trait.
//!
//! # Why a trait
//!
//! The two camera generations drive their motors in unrelated ways:
//!
//! * **EV200** ships a vendor library, `libplat_drv.so`, whose `ak_drv_ptz_*`
//!   API drives `/dev/ak-motor0` (pan) and `/dev/ak-motor1` (tilt). The
//!   [`Sdk`] backend wraps it (cargo feature `sdk`, ARM builds only).
//! * **AV100** has `/dev/preset_motor` (`presetmotor.ko`) and no PTZ library
//!   at all. The [`PresetMotor`] backend speaks its ioctl ABI directly
//!   (`docs/notes/PRESET-MOTOR-ABI.md`); Linux only, no SDK.
//!
//! [`Sim`] is a time-integrated fake used by itests and UI development.
//!
//! Only one process may own the motors (design decision D4): `onvif`
//! opens the backend, and everything else talks to it over a socket.
//!
//! # Sync, non-blocking methods
//!
//! The consumers run a tokio *current-thread* runtime. The trait is plain
//! `&self` + `Send + Sync`, so a backend can sit in an `Arc<dyn PtzBackend>`
//! and be called straight from async handlers: no method waits for the motor.
//! Motion methods record the new command and return; the hardware backend
//! executes it on its own worker thread. The exceptions are construction
//! ([`Sdk::open`] blocks for the homing sweep) and the preset methods, which
//! write a small file.
//!
//! # Latest command wins (PTZ-04)
//!
//! Every motion call replaces whatever was running; nothing is queued. On the
//! SDK backend the replacement takes effect once the short step in flight
//! finishes, because the vendor library cannot abort a move.
//!
//! # Units
//!
//! Positions are degrees in the backend's own frame (for the SDK: 0 is the end
//! stop found by the self-check). The AV100 backend reports raw motor steps
//! until its steps-per-degree factor is measured; see [`PresetMotorConfig`]. Velocities are normalised to `[-1, 1]`, as
//! ONVIF's generic velocity space is; out-of-range values are clamped.

mod clock;
mod error;
mod preset;
mod preset_motor;
#[cfg(all(feature = "sdk", target_arch = "arm"))]
mod sdk;
mod sim;
#[cfg_attr(
    not(all(feature = "sdk", target_arch = "arm")),
    allow(dead_code, reason = "only the SDK backend drives the stepper")
)]
pub mod stepper;

#[cfg(test)]
mod test_support;

// Links the `pthread_setname_np` shim: the backends spawn threads, and the
// crosstool's uClibc lacks that symbol.
#[cfg(target_arch = "arm")]
use os as _;

pub use clock::{Clock, ManualClock, SystemClock};
pub use error::{Error, Result};
pub use preset::{Preset, PresetStore, MAX_NAME_CHARS, MAX_PRESETS};
pub use preset_motor::{
    PresetMotor, PresetMotorConfig, MIN_POLL_INTERVAL, PRESET_MOTOR_DEVICE, PRESET_MOTOR_MODULE,
    VENDOR_DEVICE_CONFIG,
};
#[cfg(all(feature = "sdk", target_arch = "arm"))]
pub use sdk::Sdk;
pub use sim::{Sim, SimConfig};
pub use stepper::{SdkConfig, SDK_MOTOR_DEVICE};

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

/// Where the head is.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Pan angle in degrees.
    pub pan_deg: f32,
    /// Tilt angle in degrees.
    pub tilt_deg: f32,
    /// A move is in progress (ONVIF `MoveStatus` `MOVING`).
    pub moving: bool,
}

/// Software travel limits in degrees, inclusive.
///
/// They describe one board's head, so they come from its profile; each
/// backend has its own built-in default (the SDK's is [`Limits::EV200`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub pan_min_deg: f32,
    pub pan_max_deg: f32,
    pub tilt_min_deg: f32,
    pub tilt_max_deg: f32,
}

impl Limits {
    /// Checks that both ranges are finite and non-empty.
    pub fn validate(&self) -> Result<()> {
        let ranges = [
            (self.pan_min_deg, self.pan_max_deg),
            (self.tilt_min_deg, self.tilt_max_deg),
        ];
        if ranges
            .iter()
            .all(|(lo, hi)| lo.is_finite() && hi.is_finite() && lo < hi)
        {
            Ok(())
        } else {
            Err(Error::invalid(
                "limits",
                "each range needs finite min < max",
            ))
        }
    }

    /// Clamps a target into the limits; non-finite angles are rejected.
    pub fn clamp(&self, pan_deg: f32, tilt_deg: f32) -> Result<(f32, f32)> {
        if !pan_deg.is_finite() || !tilt_deg.is_finite() {
            return Err(Error::invalid("position", "angles must be finite"));
        }
        Ok((
            pan_deg.clamp(self.pan_min_deg, self.pan_max_deg),
            tilt_deg.clamp(self.tilt_min_deg, self.tilt_max_deg),
        ))
    }

    /// Middle of both ranges.
    pub fn center(&self) -> (f32, f32) {
        (
            (self.pan_min_deg + self.pan_max_deg) / 2.0,
            (self.tilt_min_deg + self.tilt_max_deg) / 2.0,
        )
    }
}

/// A pan/tilt head. See the crate docs for the threading contract.
pub trait PtzBackend: Send + Sync + fmt::Debug {
    /// Short backend identifier: `"sim"`, `"sdk"`, …
    fn name(&self) -> &'static str;

    /// Travel limits every target is clamped to.
    fn limits(&self) -> Limits;

    /// Moves at normalised speeds `pan`, `tilt` ∈ `[-1, 1]` until superseded
    /// or stopped. Both zero is a stop.
    fn continuous_move(&self, pan: f32, tilt: f32) -> Result<()>;

    /// Stops whatever is running.
    fn stop(&self) -> Result<()>;

    /// Moves to absolute angles, clamped to [`limits`](Self::limits).
    fn goto(&self, pan_deg: f32, tilt_deg: f32) -> Result<()>;

    /// Current angles and whether a move is in progress.
    fn position(&self) -> Position;

    /// The backend's preset table.
    fn presets(&self) -> &PresetStore;

    /// All presets.
    fn list_presets(&self) -> Vec<Preset> {
        self.presets().list()
    }

    /// Saves the current position as a preset (new when `token` is `None`).
    fn set_preset(&self, name: &str, token: Option<&str>) -> Result<Preset> {
        let here = self.position();
        self.presets().set(name, token, here.pan_deg, here.tilt_deg)
    }

    /// Moves to a saved preset.
    fn goto_preset(&self, token: &str) -> Result<()> {
        let preset = self.presets().get(token)?;
        self.goto(preset.pan_deg, preset.tilt_deg)
    }

    /// Deletes a preset.
    fn remove_preset(&self, token: &str) -> Result<()> {
        self.presets().remove(token)
    }

    /// Re-establishes the position reference by driving to the end stops;
    /// ends at the minimum of both axes. Returns at once; `position()`
    /// reports `moving` until done. Backends that cannot re-home at run time
    /// return [`Error::Unsupported`].
    fn home(&self) -> Result<()> {
        Err(Error::unsupported(
            self.name(),
            "homing on demand is not supported by this backend",
        ))
    }
}

/// Validates and clamps a normalised velocity pair.
pub(crate) fn check_velocity(pan: f32, tilt: f32) -> Result<(f32, f32)> {
    if !pan.is_finite() || !tilt.is_finite() {
        return Err(Error::invalid("velocity", "must be finite"));
    }
    Ok((pan.clamp(-1.0, 1.0), tilt.clamp(-1.0, 1.0)))
}

/// Which backend [`open_auto`] should use (`--ptz <kind>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// Probe [`OpenOptions::ak_motor_dev`], then
    /// [`OpenOptions::preset_motor_dev`] (or its module file), else no PTZ.
    #[default]
    Auto,
    /// EV200 vendor SDK.
    Sdk,
    /// AV100 `/dev/preset_motor`.
    PresetMotor,
    /// In-memory simulation.
    Sim,
    /// No PTZ; routes answer 503.
    None,
}

impl BackendKind {
    const NAMES: [(&'static str, BackendKind); 5] = [
        ("auto", BackendKind::Auto),
        ("sdk", BackendKind::Sdk),
        ("preset_motor", BackendKind::PresetMotor),
        ("sim", BackendKind::Sim),
        ("none", BackendKind::None),
    ];
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = Self::NAMES
            .iter()
            .find(|(_, kind)| kind == self)
            .map(|(name, _)| *name)
            .unwrap_or("?");
        f.write_str(name)
    }
}

impl FromStr for BackendKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let wanted = s.trim().to_ascii_lowercase().replace('-', "_");
        Self::NAMES
            .iter()
            .find(|(name, _)| *name == wanted)
            .map(|(_, kind)| *kind)
            .ok_or_else(|| {
                Error::invalid(
                    "PTZ backend",
                    format!("{s:?}; expected auto|sdk|preset_motor|sim|none"),
                )
            })
    }
}

/// Everything [`open_auto`] may need.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    /// Preset file; `None` keeps presets in memory only.
    pub presets_path: Option<PathBuf>,
    /// Settings for [`BackendKind::Sim`].
    pub sim: SimConfig,
    /// Settings for [`BackendKind::Sdk`].
    pub sdk: SdkConfig,
    /// SDK motor device probed by `Auto`; defaults to
    /// [`SDK_MOTOR_DEVICE`].
    pub ak_motor_dev: PathBuf,
    /// preset_motor device, probed by `Auto` and opened by
    /// [`BackendKind::PresetMotor`]; defaults to [`PRESET_MOTOR_DEVICE`].
    pub preset_motor_dev: PathBuf,
    /// Settings for [`BackendKind::PresetMotor`]. Its `module_path` is also
    /// probed by `Auto` when the device node does not exist yet.
    pub preset_motor: PresetMotorConfig,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            presets_path: None,
            sim: SimConfig::default(),
            sdk: SdkConfig::default(),
            ak_motor_dev: PathBuf::from(SDK_MOTOR_DEVICE),
            preset_motor_dev: PathBuf::from(PRESET_MOTOR_DEVICE),
            preset_motor: PresetMotorConfig::default(),
        }
    }
}

/// Opens the requested backend; `Ok(None)` means "this camera has no PTZ".
///
/// May block for the SDK self-check (see [`Sdk::open`](crate::Sdk)).
pub fn open_auto(kind: BackendKind, options: &OpenOptions) -> Result<Option<Box<dyn PtzBackend>>> {
    let presets = || match &options.presets_path {
        Some(path) => PresetStore::load(path),
        None => Ok(PresetStore::in_memory()),
    };
    match kind {
        BackendKind::None => Ok(None),
        BackendKind::Sim => Ok(Some(Box::new(Sim::new(options.sim, presets()?)?))),
        BackendKind::Sdk => open_sdk(options.sdk, presets()?).map(Some),
        BackendKind::PresetMotor => open_preset_motor(options, presets()?).map(Some),
        BackendKind::Auto => match auto_kind(options) {
            BackendKind::Sdk => {
                tracing::info!(dev = %options.ak_motor_dev.display(), "PTZ: using the SDK backend");
                open_sdk(options.sdk, presets()?).map(Some)
            }
            BackendKind::PresetMotor => {
                tracing::info!(dev = %options.preset_motor_dev.display(), "PTZ: using the preset_motor backend");
                open_preset_motor(options, presets()?).map(Some)
            }
            _ => {
                tracing::info!("PTZ: no motor device found");
                Ok(None)
            }
        },
    }
}

/// What `Auto` resolves to: the SDK motor device → SDK (if compiled in); the
/// preset_motor device or its module file (not loaded yet because `ipc` never
/// ran) → PresetMotor; else None.
fn auto_kind(options: &OpenOptions) -> BackendKind {
    if SDK_BUILT && options.ak_motor_dev.exists() {
        BackendKind::Sdk
    } else if options.preset_motor_dev.exists() || options.preset_motor.module_path.exists() {
        BackendKind::PresetMotor
    } else {
        BackendKind::None
    }
}

/// Opens the AV100 backend; the last-position file defaults to
/// `ptz-position.json` next to the presets file.
fn open_preset_motor(options: &OpenOptions, presets: PresetStore) -> Result<Box<dyn PtzBackend>> {
    let mut config = options.preset_motor.clone();
    if config.position_path.is_none() {
        config.position_path = options
            .presets_path
            .as_ref()
            .map(|p| p.with_file_name(POSITION_FILE));
    }
    Ok(Box::new(PresetMotor::open(
        &options.preset_motor_dev,
        &config,
        presets,
    )?))
}

/// Whether the EV200 SDK backend is compiled in; `Auto` never picks it otherwise.
const SDK_BUILT: bool = cfg!(all(feature = "sdk", target_arch = "arm"));

/// File name of the AV100 last-position file (see [`open_auto`]).
pub const POSITION_FILE: &str = "ptz-position.json";

#[cfg(all(feature = "sdk", target_arch = "arm"))]
fn open_sdk(config: SdkConfig, presets: PresetStore) -> Result<Box<dyn PtzBackend>> {
    Ok(Box::new(Sdk::open(config, presets)?))
}

#[cfg(not(all(feature = "sdk", target_arch = "arm")))]
fn open_sdk(_config: SdkConfig, _presets: PresetStore) -> Result<Box<dyn PtzBackend>> {
    Err(Error::unsupported(
        "sdk",
        "built without the `sdk` feature or not for the camera (ARM)",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn options(dir: &TempDir) -> OpenOptions {
        OpenOptions {
            ak_motor_dev: dir.path().join("ak-motor0"),
            preset_motor_dev: dir.path().join("preset_motor"),
            preset_motor: PresetMotorConfig {
                module_path: dir.path().join("presetmotor.ko"),
                device_config: None,
                ..PresetMotorConfig::default()
            },
            ..OpenOptions::default()
        }
    }

    #[test]
    fn kind_parses_and_prints() {
        for name in ["auto", "sdk", "preset_motor", "sim", "none"] {
            let kind: BackendKind = name.parse().unwrap();
            assert_eq!(kind.to_string(), name);
        }
        assert_eq!(
            "Preset-Motor".parse::<BackendKind>().unwrap(),
            BackendKind::PresetMotor
        );
        assert!("bogus".parse::<BackendKind>().is_err());
    }

    #[test]
    fn auto_without_devices_is_none() {
        let dir = TempDir::new("probe-none");
        assert!(open_auto(BackendKind::Auto, &options(&dir))
            .unwrap()
            .is_none());
        assert!(open_auto(BackendKind::None, &options(&dir))
            .unwrap()
            .is_none());
    }

    /// A plain file stands in for the node: it opens, and the first ioctl
    /// fails with ENOTTY, which proves the probe routed to preset_motor.
    #[cfg(target_os = "linux")]
    #[test]
    fn auto_routes_to_preset_motor() {
        let dir = TempDir::new("probe-av100");
        let opts = options(&dir);
        std::fs::write(&opts.preset_motor_dev, b"").unwrap();
        for kind in [BackendKind::Auto, BackendKind::PresetMotor] {
            assert!(matches!(
                open_auto(kind, &opts),
                Err(Error::Motor {
                    op: "set speed (cmd 3)",
                    ..
                })
            ));
        }
    }

    #[test]
    fn auto_probe_order() {
        let dir = TempDir::new("probe-order");
        let opts = options(&dir);
        assert_eq!(auto_kind(&opts), BackendKind::None);
        // The module file alone is enough: open_preset_motor insmods it.
        std::fs::write(&opts.preset_motor.module_path, b"").unwrap();
        assert_eq!(auto_kind(&opts), BackendKind::PresetMotor);
        std::fs::remove_file(&opts.preset_motor.module_path).unwrap();
        std::fs::write(&opts.preset_motor_dev, b"").unwrap();
        assert_eq!(auto_kind(&opts), BackendKind::PresetMotor);
        std::fs::write(&opts.ak_motor_dev, b"").unwrap();
        let expected = if SDK_BUILT {
            BackendKind::Sdk
        } else {
            BackendKind::PresetMotor
        };
        assert_eq!(auto_kind(&opts), expected);
    }

    #[test]
    fn home_defaults_to_unsupported() {
        #[derive(Debug)]
        struct Fixed(PresetStore);
        impl PtzBackend for Fixed {
            fn name(&self) -> &'static str {
                "fixed"
            }
            fn limits(&self) -> Limits {
                Limits {
                    pan_min_deg: 0.0,
                    pan_max_deg: 1.0,
                    tilt_min_deg: 0.0,
                    tilt_max_deg: 1.0,
                }
            }
            fn continuous_move(&self, _: f32, _: f32) -> Result<()> {
                Ok(())
            }
            fn stop(&self) -> Result<()> {
                Ok(())
            }
            fn goto(&self, _: f32, _: f32) -> Result<()> {
                Ok(())
            }
            fn position(&self) -> Position {
                Position {
                    pan_deg: 0.0,
                    tilt_deg: 0.0,
                    moving: false,
                }
            }
            fn presets(&self) -> &PresetStore {
                &self.0
            }
        }
        assert!(matches!(
            Fixed(PresetStore::in_memory()).home(),
            Err(Error::Unsupported {
                backend: "fixed",
                ..
            })
        ));
    }

    #[cfg(not(target_arch = "arm"))]
    #[test]
    fn sdk_is_unsupported_on_the_host() {
        let dir = TempDir::new("probe-ev200");
        let opts = options(&dir);
        std::fs::write(&opts.ak_motor_dev, b"").unwrap();
        assert!(matches!(
            open_auto(BackendKind::Sdk, &opts),
            Err(Error::Unsupported { backend: "sdk", .. })
        ));
    }

    #[test]
    fn sim_opens_with_persistent_presets() {
        let dir = TempDir::new("probe-sim");
        let opts = OpenOptions {
            presets_path: Some(dir.path().join("presets.json")),
            ..options(&dir)
        };
        let ptz = open_auto(BackendKind::Sim, &opts).unwrap().unwrap();
        assert_eq!(ptz.name(), "sim");
        let preset = ptz.set_preset("Home", None).unwrap();
        drop(ptz);
        let ptz = open_auto(BackendKind::Sim, &opts).unwrap().unwrap();
        assert_eq!(ptz.list_presets(), vec![preset]);
    }

    #[test]
    fn limits_validate_and_clamp() {
        let limits = Limits {
            pan_min_deg: 0.0,
            pan_max_deg: 350.0,
            tilt_min_deg: 0.0,
            tilt_max_deg: 120.0,
        };
        assert!(limits.validate().is_ok());
        assert_eq!(limits.clamp(-5.0, 500.0).unwrap(), (0.0, 120.0));
        assert!(limits.clamp(f32::NAN, 0.0).is_err());
        assert_eq!(limits.center(), (175.0, 60.0));
        let empty = Limits {
            pan_max_deg: 0.0,
            ..limits
        };
        assert!(empty.validate().is_err());
    }
}
