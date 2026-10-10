//! PTZ: the one process that owns the motor (design D4).
//!
//! `--ptz` resolves to an `anyka_ptz::PtzBackend` ([`open`]); `auto` probes
//! the EV200 SDK device, then the AV100 `/dev/preset_motor`; `sim` is the
//! in-memory backend used by itest. When a backend is present, the ONVIF PTZ
//! service is registered ([`attach`], handlers in `ptz_soap.rs`), the media
//! profile gains a `PTZConfiguration`, and `/tmp/anyka-ptz.sock` is served
//! for web (`ptz_socket.rs`). Both front ends drive the same [`Ptz`],
//! so the latest command wins, whoever sent it (PTZ-04).
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

use anyka_ptz::{BackendKind, Limits, OpenOptions, Position, Preset, PtzBackend};
use std::path::Path;
use std::sync::Arc;
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

/// Opens the backend for `mode`; `None` = run without PTZ.
///
/// Blocking: the SDK backend runs its homing sweep here, so call this before
/// the runtime starts serving. `presets` empty keeps presets in memory.
pub fn open(mode: PtzMode, presets: &Path) -> Option<Box<dyn PtzBackend>> {
    let options = OpenOptions {
        presets_path: (!presets.as_os_str().is_empty()).then(|| presets.to_path_buf()),
        ..OpenOptions::default()
    };
    match anyka_ptz::open_auto(mode.kind(), &options) {
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
}

impl Ptz {
    /// Wraps `backend` and starts the deadman task; needs a tokio runtime
    /// context. The task ends when the `Ptz` is dropped.
    pub fn new(backend: Box<dyn PtzBackend>) -> Arc<Self> {
        let backend: Arc<dyn PtzBackend> = Arc::from(backend);
        let (deadline, watcher) = watch::channel(None);
        tokio::spawn(deadman(Arc::clone(&backend), watcher));
        Arc::new(Self { backend, deadline })
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

    /// Continuous move; stopped automatically after `timeout` if given and
    /// no newer motion command arrives first.
    pub fn continuous_move(
        &self,
        pan: f32,
        tilt: f32,
        timeout: Option<Duration>,
    ) -> anyka_ptz::Result<()> {
        self.backend.continuous_move(pan, tilt)?;
        let moving = pan != 0.0 || tilt != 0.0;
        self.set_deadline(timeout.filter(|_| moving).map(|t| Instant::now() + t));
        Ok(())
    }

    pub fn stop(&self) -> anyka_ptz::Result<()> {
        self.set_deadline(None);
        self.backend.stop()
    }

    /// Absolute move in degrees (clamped by the backend).
    pub fn goto(&self, pan_deg: f32, tilt_deg: f32) -> anyka_ptz::Result<()> {
        self.backend.goto(pan_deg, tilt_deg)?;
        self.set_deadline(None);
        Ok(())
    }

    pub fn set_preset(&self, name: &str, token: Option<&str>) -> anyka_ptz::Result<Preset> {
        self.backend.set_preset(name, token)
    }

    pub fn goto_preset(&self, token: &str) -> anyka_ptz::Result<()> {
        self.backend.goto_preset(token)?;
        self.set_deadline(None);
        Ok(())
    }

    pub fn remove_preset(&self, token: &str) -> anyka_ptz::Result<()> {
        self.backend.remove_preset(token)
    }

    /// The backend's homing run if it has one, else the centre of the limits.
    pub fn home(&self) -> anyka_ptz::Result<()> {
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

    /// A simulated head on the real clock (the deadman uses tokio time).
    pub fn sim() -> Arc<Ptz> {
        Ptz::new(Box::new(
            Sim::new(SimConfig::default(), PresetStore::in_memory()).unwrap(),
        ))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deadman_stops_a_timed_move() {
        let ptz = sim();
        ptz.continuous_move(1.0, 0.0, Some(Duration::from_millis(50)))
            .unwrap();
        assert!(ptz.position().moving);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!ptz.position().moving, "deadman stopped the move");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_newer_command_replaces_the_deadline() {
        let ptz = sim();
        ptz.continuous_move(1.0, 0.0, Some(Duration::from_millis(50)))
            .unwrap();
        // Re-sent with a longer deadline before the first expires.
        tokio::time::sleep(Duration::from_millis(20)).await;
        ptz.continuous_move(1.0, 0.0, Some(Duration::from_millis(300)))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            ptz.position().moving,
            "the first deadline no longer applies"
        );
        // An untimed move is not stopped at all.
        ptz.continuous_move(0.0, 1.0, None).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(ptz.position().moving);
        ptz.stop().unwrap();
        assert!(!ptz.position().moving);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn home_falls_back_to_the_backend_or_centre() {
        let ptz = sim();
        // Sim implements home(): it travels to the minimum.
        ptz.home().unwrap();
        assert!(ptz.position().moving);
        ptz.stop().unwrap();
    }

    #[test]
    fn modes_map_to_backends() {
        assert_eq!(PtzMode::PresetMotor.kind(), BackendKind::PresetMotor);
        assert_eq!(PtzMode::None.kind(), BackendKind::None);
        assert!(open(PtzMode::None, Path::new("")).is_none());
        let sim = open(PtzMode::Sim, Path::new("")).expect("sim always opens");
        assert_eq!(sim.name(), "sim");
    }
}
