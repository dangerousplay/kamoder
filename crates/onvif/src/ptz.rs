//! PTZ: the one process that owns the motor (design D4).
//!
//! The board profile's `[ptz]` says which backend drives the head, its device
//! node, kernel module, travel ranges and steps per degree; [`plan`] turns it
//! into anyka-ptz's own [`OpenOptions`] (anyka-ptz knows nothing of boards).
//! `--ptz` / `ANYKA_ONVIF_PTZ` overrides the backend; anything the profile
//! leaves out keeps the backend's built-in default. A profile without
//! `[ptz]` means no pan/tilt head: PTZ is not offered unless `--ptz` asks for
//! it. `auto` probes the SDK device, then the preset_motor one; `sim` is the
//! in-memory backend used by itest.
//!
//! When a backend is present ([`open`]), the ONVIF PTZ service is registered
//! (handlers in `ptz_soap.rs`), the first sensor's media profile gains a
//! `PTZConfiguration`, and `/tmp/anyka-ptz.sock` is served for web
//! (`ptz_socket.rs`). Both front ends drive the same [`Ptz`], so the latest
//! command wins, whoever sent it (PTZ-04).
//!
//! A backend that fails to open is logged and PTZ is simply not offered:
//! losing pan/tilt must never take ONVIF discovery and streaming down with
//! it. Without a backend nothing is registered, GetCapabilities/GetServices
//! do not advertise PTZ (see `device.rs`) and PTZ calls fault as
//! unsupported. The library's own virtual PTZ (`onvif_device_rs::ptz`) is
//! deliberately not used: it would advertise a camera that moves when
//! nothing does.
//!
//! # Deadman
//!
//! A continuous move carries an optional deadline — ONVIF `Timeout`, or the
//! socket's `timeout_ms` (default 1 s there, because a browser can vanish
//! mid-press). One task per process watches the latest deadline and stops
//! the motor when it passes, unless a newer motion command replaced it.
//!
//! # Manual activity
//!
//! Every motion method takes the [`Source`] of the command. The time of the
//! last [`Source::Manual`] one (every ONVIF move, and socket requests not
//! tagged `follow`) is kept so the follow tracker can see a person steering
//! and pause; the socket's `status` reports it as `last_manual_ms_ago`.
//! Recorded when the command arrives, whether or not the backend accepts it:
//! the intent to steer is what matters.

use anyka_ptz::{BackendKind, Limits, OpenOptions, Position, Preset, PtzBackend};
pub use ptz_proto::Source;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// `--ptz`: which motor backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PtzMode {
    /// Probe the hardware backends, use the first present.
    Auto,
    /// In-memory simulated motor (itest, UI development).
    Sim,
    /// Vendor SDK `ak_drv_ptz_*` (EV200).
    Sdk,
    /// `/dev/preset_motor` ioctls (AV100).
    PresetMotor,
    /// No PTZ.
    None,
}

impl PtzMode {
    fn kind(self) -> BackendKind {
        match self {
            PtzMode::Auto => BackendKind::Auto,
            PtzMode::Sim => BackendKind::Sim,
            PtzMode::Sdk => BackendKind::Sdk,
            PtzMode::PresetMotor => BackendKind::PresetMotor,
            PtzMode::None => BackendKind::None,
        }
    }
}

impl From<board::PtzBackend> for PtzMode {
    fn from(backend: board::PtzBackend) -> Self {
        match backend {
            board::PtzBackend::PresetMotor => PtzMode::PresetMotor,
            board::PtzBackend::Sdk => PtzMode::Sdk,
            board::PtzBackend::Sim => PtzMode::Sim,
        }
    }
}

/// Which backend to open and with what, from `--ptz` (`cli`, which includes
/// `ANYKA_ONVIF_PTZ`), the board's `[ptz]` and the built-in defaults, in that
/// order of precedence. `presets` empty keeps presets in memory.
///
/// The profile describes the board's own head, so its device, module and
/// steps per degree apply only to the backend it names (or the one `auto`
/// would find for it); its ranges also apply to `sim`, which then simulates
/// that head. A `--ptz` naming another hardware backend gets that backend's
/// built-in defaults.
pub fn plan(
    cli: Option<PtzMode>,
    board: Option<&board::Ptz>,
    presets: &Path,
) -> (PtzMode, OpenOptions) {
    let mode = match (cli, board) {
        (Some(mode), _) => mode,
        (None, Some(head)) => head.backend.into(),
        (None, None) => PtzMode::None,
    };
    let mut options = OpenOptions {
        presets_path: (!presets.as_os_str().is_empty()).then(|| presets.to_path_buf()),
        ..OpenOptions::default()
    };
    let Some(head) = board else {
        return (mode, options);
    };
    let limits = Limits {
        pan_min_deg: head.pan[0],
        pan_max_deg: head.pan[1],
        tilt_min_deg: head.tilt[0],
        tilt_max_deg: head.tilt[1],
    };
    let target = match mode {
        PtzMode::Auto => head.backend.into(),
        other => other,
    };
    match (target, head.backend) {
        (PtzMode::Sim, _) => options.sim.limits = limits,
        (PtzMode::PresetMotor, board::PtzBackend::PresetMotor) => {
            if let Some(device) = &head.device {
                options.preset_motor_dev = device.clone();
            }
            if let Some(module) = &head.module {
                options.preset_motor.module_path = module.clone();
            }
            options.preset_motor.steps_per_degree_pan = head.steps_per_degree;
            options.preset_motor.steps_per_degree_tilt = head.steps_per_degree;
            options.preset_motor.limits = Some(limits);
        }
        // The library loads its own driver, so `module` has no use here.
        // `steps_per_degree` is not mapped onto `SdkConfig::angle_rate`: the
        // profile defaults it to 1.0 while the SDK needs 2.0, and an omitted
        // key cannot be told from an explicit 1.0.
        (PtzMode::Sdk, board::PtzBackend::Sdk) => {
            if let Some(device) = &head.device {
                options.ak_motor_dev = device.clone();
            }
            options.sdk.limits = limits;
        }
        _ => tracing::info!(
            ?mode,
            board = ?head.backend,
            "--ptz overrides the board's PTZ backend; using its built-in settings"
        ),
    }
    (mode, options)
}

/// Opens the backend for `mode`; `None` = run without PTZ.
///
/// Blocking: the SDK backend runs its homing sweep here, so call this before
/// the runtime starts serving.
pub fn open(mode: PtzMode, options: &OpenOptions) -> Option<Box<dyn PtzBackend>> {
    match anyka_ptz::open_auto(mode.kind(), options) {
        Ok(Some(backend)) => {
            tracing::info!(backend = backend.name(), limits = ?backend.limits(), "PTZ backend open");
            Some(backend)
        }
        Ok(None) => {
            if mode != PtzMode::None {
                tracing::info!(?mode, "no PTZ on this camera; not advertised");
            }
            None
        }
        Err(error) => {
            tracing::warn!(?mode, %error, "cannot open the PTZ backend; running without PTZ");
            None
        }
    }
}

/// The shared motor: a backend plus the deadman for timed moves.
#[derive(Debug)]
pub struct Ptz {
    backend: Arc<dyn PtzBackend>,
    /// Deadline of the current continuous move, if it has one.
    deadline: watch::Sender<Option<Instant>>,
    /// When the last manual motion command arrived (tokio clock, so paused
    /// tests control it).
    last_manual: Mutex<Option<Instant>>,
}

impl Ptz {
    /// Wraps `backend` and starts the deadman task; needs a tokio runtime
    /// context. The task ends when the `Ptz` is dropped.
    pub fn new(backend: Box<dyn PtzBackend>) -> Arc<Self> {
        let backend: Arc<dyn PtzBackend> = Arc::from(backend);
        let (deadline, watcher) = watch::channel(None);
        tokio::spawn(deadman(Arc::clone(&backend), watcher));
        Arc::new(Self {
            backend,
            deadline,
            last_manual: Mutex::new(None),
        })
    }

    pub fn name(&self) -> &'static str {
        self.backend.name()
    }

    pub fn limits(&self) -> Limits {
        self.backend.limits()
    }

    pub fn position(&self) -> Position {
        self.backend.position()
    }

    pub fn presets(&self) -> Vec<Preset> {
        self.backend.list_presets()
    }

    /// Time since the last manual motion command; `None` if there has been
    /// none.
    pub fn last_manual_ago(&self) -> Option<Duration> {
        let at = *self.last_manual.lock().unwrap_or_else(|e| e.into_inner());
        at.map(|at| at.elapsed())
    }

    /// Notes a motion command from `source`.
    fn record(&self, source: Source) {
        if source == Source::Manual {
            *self.last_manual.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }
    }

    /// Continuous move; stopped automatically after `timeout` if given and
    /// no newer motion command arrives first.
    pub fn continuous_move(
        &self,
        pan: f32,
        tilt: f32,
        timeout: Option<Duration>,
        source: Source,
    ) -> anyka_ptz::Result<()> {
        self.record(source);
        self.backend.continuous_move(pan, tilt)?;
        let moving = pan != 0.0 || tilt != 0.0;
        self.set_deadline(timeout.filter(|_| moving).map(|t| Instant::now() + t));
        Ok(())
    }

    pub fn stop(&self, source: Source) -> anyka_ptz::Result<()> {
        self.record(source);
        self.set_deadline(None);
        self.backend.stop()
    }

    /// Absolute move in degrees (clamped by the backend).
    pub fn goto(&self, pan_deg: f32, tilt_deg: f32, source: Source) -> anyka_ptz::Result<()> {
        self.record(source);
        self.backend.goto(pan_deg, tilt_deg)?;
        self.set_deadline(None);
        Ok(())
    }

    pub fn set_preset(&self, name: &str, token: Option<&str>) -> anyka_ptz::Result<Preset> {
        self.backend.set_preset(name, token)
    }

    pub fn goto_preset(&self, token: &str, source: Source) -> anyka_ptz::Result<()> {
        self.record(source);
        self.backend.goto_preset(token)?;
        self.set_deadline(None);
        Ok(())
    }

    pub fn remove_preset(&self, token: &str) -> anyka_ptz::Result<()> {
        self.backend.remove_preset(token)
    }

    /// The backend's homing run if it has one, else the centre of the limits.
    pub fn home(&self, source: Source) -> anyka_ptz::Result<()> {
        self.record(source);
        match self.backend.home() {
            Err(anyka_ptz::Error::Unsupported { .. }) => {
                let (pan, tilt) = self.limits().center();
                self.backend.goto(pan, tilt)?;
            }
            other => other?,
        }
        self.set_deadline(None);
        Ok(())
    }

    fn set_deadline(&self, deadline: Option<Instant>) {
        self.deadline.send_replace(deadline);
    }
}

/// Stops the motor when the latest move's deadline passes.
async fn deadman(backend: Arc<dyn PtzBackend>, mut deadline: watch::Receiver<Option<Instant>>) {
    loop {
        let current = *deadline.borrow_and_update();
        let Some(at) = current else {
            if deadline.changed().await.is_err() {
                return;
            }
            continue;
        };
        tokio::select! {
            changed = deadline.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = tokio::time::sleep_until(at) => {
                // A newer command may have landed in the same tick; it wins.
                if deadline.has_changed().unwrap_or(false) {
                    continue;
                }
                tracing::debug!("PTZ move timed out; stopping");
                metrics::counter!("kamoder_onvif_ptz_deadman_stops_total").increment(1);
                if let Err(error) = backend.stop() {
                    tracing::warn!(%error, "PTZ deadman stop failed");
                }
                if deadline.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use anyka_ptz::{PresetStore, Sim, SimConfig};

    const M: Source = Source::Manual;

    /// A simulated head on the real clock (the deadman uses tokio time).
    pub fn sim() -> Arc<Ptz> {
        Ptz::new(Box::new(
            Sim::new(SimConfig::default(), PresetStore::in_memory()).unwrap(),
        ))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deadman_stops_a_timed_move() {
        let ptz = sim();
        ptz.continuous_move(1.0, 0.0, Some(Duration::from_millis(50)), M)
            .unwrap();
        assert!(ptz.position().moving);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!ptz.position().moving, "deadman stopped the move");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_newer_command_replaces_the_deadline() {
        let ptz = sim();
        ptz.continuous_move(1.0, 0.0, Some(Duration::from_millis(50)), M)
            .unwrap();
        // Re-sent with a longer deadline before the first expires.
        tokio::time::sleep(Duration::from_millis(20)).await;
        ptz.continuous_move(1.0, 0.0, Some(Duration::from_millis(300)), M)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            ptz.position().moving,
            "the first deadline no longer applies"
        );
        // An untimed move is not stopped at all.
        ptz.continuous_move(0.0, 1.0, None, M).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(ptz.position().moving);
        ptz.stop(M).unwrap();
        assert!(!ptz.position().moving);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn home_falls_back_to_the_backend_or_centre() {
        let ptz = sim();
        // Sim implements home(): it travels to the minimum.
        ptz.home(M).unwrap();
        assert!(ptz.position().moving);
        ptz.stop(M).unwrap();
    }

    /// The follow tracker's commands are not manual activity; everyone
    /// else's are.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn only_manual_commands_count_as_manual_activity() {
        let ptz = sim();
        assert_eq!(ptz.last_manual_ago(), None);
        let f = Source::Follow;
        ptz.continuous_move(0.5, 0.0, None, f).unwrap();
        ptz.goto(10.0, 10.0, f).unwrap();
        ptz.home(f).unwrap();
        ptz.stop(f).unwrap();
        assert_eq!(ptz.last_manual_ago(), None, "follow moves are not manual");

        ptz.goto(20.0, 20.0, M).unwrap();
        assert_eq!(ptz.last_manual_ago(), Some(Duration::ZERO));
        tokio::time::advance(Duration::from_millis(1500)).await;
        ptz.continuous_move(0.0, 0.5, None, f).unwrap();
        ptz.stop(f).unwrap();
        assert_eq!(
            ptz.last_manual_ago(),
            Some(Duration::from_millis(1500)),
            "a follow move leaves the manual time alone"
        );
        // A failed manual command still shows intent.
        assert!(ptz.goto_preset("nope", M).is_err());
        assert_eq!(ptz.last_manual_ago(), Some(Duration::ZERO));
    }

    #[test]
    fn modes_map_to_backends() {
        assert_eq!(PtzMode::PresetMotor.kind(), BackendKind::PresetMotor);
        assert_eq!(PtzMode::None.kind(), BackendKind::None);
        let (_, options) = plan(None, None, Path::new(""));
        assert!(open(PtzMode::None, &options).is_none());
        let sim = open(PtzMode::Sim, &options).expect("sim always opens");
        assert_eq!(sim.name(), "sim");
    }

    fn av100_head() -> board::Ptz {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../boards/anyka-av100-ptz.toml");
        board::load(&path)
            .unwrap()
            .ptz
            .expect("the AV100 has a head")
    }

    fn head(backend: board::PtzBackend) -> board::Ptz {
        board::Ptz {
            backend,
            device: Some("/dev/motor-x".into()),
            module: Some("/lib/motor-x.ko".into()),
            pan: [10.0, 300.0],
            tilt: [5.0, 90.0],
            steps_per_degree: 2.5,
            follow: None,
        }
    }

    const HEAD_LIMITS: Limits = Limits {
        pan_min_deg: 10.0,
        pan_max_deg: 300.0,
        tilt_min_deg: 5.0,
        tilt_max_deg: 90.0,
    };

    #[test]
    fn the_board_profile_drives_the_backend() {
        let (mode, options) = plan(None, Some(&av100_head()), Path::new("etc/p.json"));
        assert_eq!(mode, PtzMode::PresetMotor);
        assert_eq!(options.presets_path, Some("etc/p.json".into()));
        assert_eq!(options.preset_motor_dev, Path::new("/dev/preset_motor"));
        assert_eq!(
            options.preset_motor.module_path,
            Path::new("/ipc/patch/ko/presetmotor.ko")
        );
        assert_eq!(
            options.preset_motor.limits,
            Some(Limits {
                pan_min_deg: 0.0,
                pan_max_deg: 175.0,
                tilt_min_deg: 0.0,
                tilt_max_deg: 70.0,
            })
        );
        assert_eq!(options.preset_motor.steps_per_degree_pan, 1.0);

        let (mode, options) = plan(
            None,
            Some(&head(board::PtzBackend::PresetMotor)),
            Path::new(""),
        );
        assert_eq!(mode, PtzMode::PresetMotor);
        assert_eq!(options.presets_path, None);
        assert_eq!(options.preset_motor_dev, Path::new("/dev/motor-x"));
        assert_eq!(
            options.preset_motor.module_path,
            Path::new("/lib/motor-x.ko")
        );
        assert_eq!(options.preset_motor.limits, Some(HEAD_LIMITS));
        assert_eq!(options.preset_motor.steps_per_degree_tilt, 2.5);
        // Untouched: another backend's settings.
        assert_eq!(options.sdk, anyka_ptz::SdkConfig::default());

        let (mode, options) = plan(None, Some(&head(board::PtzBackend::Sdk)), Path::new(""));
        assert_eq!(mode, PtzMode::Sdk);
        assert_eq!(options.ak_motor_dev, Path::new("/dev/motor-x"));
        assert_eq!(options.sdk.limits, HEAD_LIMITS);
        assert_eq!(
            options.preset_motor,
            anyka_ptz::PresetMotorConfig::default()
        );

        let (mode, options) = plan(None, Some(&head(board::PtzBackend::Sim)), Path::new(""));
        assert_eq!(mode, PtzMode::Sim);
        assert_eq!(options.sim.limits, HEAD_LIMITS);
    }

    #[test]
    fn missing_profile_values_keep_the_built_in_defaults() {
        let sparse = board::Ptz {
            device: None,
            module: None,
            ..head(board::PtzBackend::PresetMotor)
        };
        let (_, options) = plan(None, Some(&sparse), Path::new(""));
        let defaults = OpenOptions::default();
        assert_eq!(options.preset_motor_dev, defaults.preset_motor_dev);
        assert_eq!(
            options.preset_motor.module_path,
            defaults.preset_motor.module_path
        );
        assert_eq!(options.preset_motor.limits, Some(HEAD_LIMITS));
    }

    #[test]
    fn the_command_line_overrides_the_board() {
        let board = head(board::PtzBackend::PresetMotor);
        // `sim` simulates the board's head.
        let (mode, options) = plan(Some(PtzMode::Sim), Some(&board), Path::new(""));
        assert_eq!(mode, PtzMode::Sim);
        assert_eq!(options.sim.limits, HEAD_LIMITS);
        assert_eq!(
            options.preset_motor_dev,
            OpenOptions::default().preset_motor_dev
        );

        // Another hardware backend: its own built-in settings only.
        let (mode, options) = plan(Some(PtzMode::Sdk), Some(&board), Path::new(""));
        assert_eq!(mode, PtzMode::Sdk);
        assert_eq!(options.sdk, anyka_ptz::SdkConfig::default());
        assert_eq!(
            options.preset_motor,
            anyka_ptz::PresetMotorConfig::default()
        );

        // `auto` probes with the board's device paths.
        let (mode, options) = plan(Some(PtzMode::Auto), Some(&board), Path::new(""));
        assert_eq!(mode, PtzMode::Auto);
        assert_eq!(options.preset_motor_dev, Path::new("/dev/motor-x"));

        let (mode, _) = plan(Some(PtzMode::None), Some(&board), Path::new(""));
        assert_eq!(mode, PtzMode::None);
    }

    /// No `[ptz]`: nothing is opened, so `main` registers no PTZ service,
    /// advertises none (`service::tests::soap_end_to_end` checks the
    /// capabilities without a motor) and serves no PTZ socket.
    #[test]
    fn a_board_without_ptz_runs_without_ptz() {
        let (mode, options) = plan(None, None, Path::new("etc/p.json"));
        assert_eq!(mode, PtzMode::None);
        assert!(open(mode, &options).is_none());
        // `--ptz sim` still works on such a board, with the sim's defaults.
        let (mode, options) = plan(Some(PtzMode::Sim), None, Path::new(""));
        assert_eq!(mode, PtzMode::Sim);
        assert_eq!(options.sim, anyka_ptz::SimConfig::default());
    }

    /// The board's ranges reach the opened backend.
    #[test]
    fn sim_opens_with_the_board_limits() {
        let (mode, options) = plan(None, Some(&head(board::PtzBackend::Sim)), Path::new(""));
        let backend = open(mode, &options).expect("sim always opens");
        assert_eq!(backend.limits(), HEAD_LIMITS);
    }
}
