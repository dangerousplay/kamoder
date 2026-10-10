//! In-memory PTZ head for itests and UI development.
//!
//! No thread and no timer: the state records the current command and when it
//! started, and every call first integrates position up to "now". That makes
//! the simulation exact for any clock, including [`ManualClock`] in tests,
//! and costs nothing while idle.
//!
//! [`ManualClock`]: crate::ManualClock

use crate::clock::{Clock, SystemClock};
use crate::error::{Error, Result};
use crate::preset::PresetStore;
use crate::{check_velocity, Limits, Position, PtzBackend};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

/// Tunables for [`Sim`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimConfig {
    /// Travel range; defaults to the EV200 software limits.
    pub limits: Limits,
    /// Pan speed at full deflection (`|pan| = 1`), degrees per second.
    pub pan_speed_deg_s: f32,
    /// Tilt speed at full deflection, degrees per second.
    pub tilt_speed_deg_s: f32,
    /// Starting position; `None` means the centre of `limits`.
    pub home: Option<(f32, f32)>,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            limits: Limits::EV200,
            pan_speed_deg_s: 40.0,
            tilt_speed_deg_s: 20.0,
            home: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Motion {
    Idle,
    /// Normalised velocities in `[-1, 1]`.
    Velocity {
        pan: f32,
        tilt: f32,
    },
    /// Absolute target in degrees, already clamped.
    Target {
        pan: f32,
        tilt: f32,
    },
}

#[derive(Debug)]
struct State {
    pan: f32,
    tilt: f32,
    motion: Motion,
    /// Instant up to which `pan`/`tilt` have been integrated.
    at: Instant,
}

/// Simulated backend: time-integrated position clamped to the limits.
#[derive(Debug)]
pub struct Sim {
    config: SimConfig,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    presets: PresetStore,
}

impl Sim {
    /// A simulator on the real clock.
    pub fn new(config: SimConfig, presets: PresetStore) -> Result<Self> {
        Self::with_clock(config, presets, Arc::new(SystemClock))
    }

    /// A simulator on an injected clock.
    pub fn with_clock(
        config: SimConfig,
        presets: PresetStore,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        config.limits.validate()?;
        for (what, speed) in [
            ("pan speed", config.pan_speed_deg_s),
            ("tilt speed", config.tilt_speed_deg_s),
        ] {
            if !(speed.is_finite() && speed > 0.0) {
                return Err(Error::invalid(what, "must be a positive number"));
            }
        }
        let (pan, tilt) = match config.home {
            Some((pan, tilt)) => config.limits.clamp(pan, tilt)?,
            None => config.limits.center(),
        };
        let at = clock.now();
        Ok(Self {
            config,
            clock,
            state: Mutex::new(State {
                pan,
                tilt,
                motion: Motion::Idle,
                at,
            }),
            presets,
        })
    }

    /// Locks the state and integrates it up to now.
    fn advance(&self) -> MutexGuard<'_, State> {
        let now = self.clock.now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let dt = now.saturating_duration_since(state.at).as_secs_f32();
        state.at = now;
        let limits = &self.config.limits;
        let (pan_speed, tilt_speed) = (self.config.pan_speed_deg_s, self.config.tilt_speed_deg_s);
        match state.motion {
            Motion::Idle => {}
            Motion::Velocity { pan, tilt } => {
                state.pan = (state.pan + pan * pan_speed * dt)
                    .clamp(limits.pan_min_deg, limits.pan_max_deg);
                state.tilt = (state.tilt + tilt * tilt_speed * dt)
                    .clamp(limits.tilt_min_deg, limits.tilt_max_deg);
            }
            Motion::Target { pan, tilt } => {
                state.pan = approach(state.pan, pan, pan_speed * dt);
                state.tilt = approach(state.tilt, tilt, tilt_speed * dt);
                if state.pan == pan && state.tilt == tilt {
                    state.motion = Motion::Idle;
                }
            }
        }
        state
    }

    fn is_moving(&self, state: &State) -> bool {
        let l = &self.config.limits;
        match state.motion {
            Motion::Idle => false,
            Motion::Target { .. } => true,
            // Holding the pad against an end stop is not motion.
            Motion::Velocity { pan, tilt } => {
                let pan_free = (pan > 0.0 && state.pan < l.pan_max_deg)
                    || (pan < 0.0 && state.pan > l.pan_min_deg);
                let tilt_free = (tilt > 0.0 && state.tilt < l.tilt_max_deg)
                    || (tilt < 0.0 && state.tilt > l.tilt_min_deg);
                pan_free || tilt_free
            }
        }
    }
}

/// Moves `from` towards `to` by at most `max_step`.
fn approach(from: f32, to: f32, max_step: f32) -> f32 {
    let delta = to - from;
    if delta.abs() <= max_step {
        to
    } else {
        from + max_step.copysign(delta)
    }
}

impl PtzBackend for Sim {
    fn name(&self) -> &'static str {
        "sim"
    }

    fn limits(&self) -> Limits {
        self.config.limits
    }

    fn continuous_move(&self, pan: f32, tilt: f32) -> Result<()> {
        let (pan, tilt) = check_velocity(pan, tilt)?;
        let mut state = self.advance();
        state.motion = if pan == 0.0 && tilt == 0.0 {
            Motion::Idle
        } else {
            Motion::Velocity { pan, tilt }
        };
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        self.advance().motion = Motion::Idle;
        Ok(())
    }

    fn goto(&self, pan_deg: f32, tilt_deg: f32) -> Result<()> {
        let (pan, tilt) = self.config.limits.clamp(pan_deg, tilt_deg)?;
        self.advance().motion = Motion::Target { pan, tilt };
        Ok(())
    }

    fn position(&self) -> Position {
        let state = self.advance();
        Position {
            pan_deg: state.pan,
            tilt_deg: state.tilt,
            moving: self.is_moving(&state),
        }
    }

    fn presets(&self) -> &PresetStore {
        &self.presets
    }

    /// Travels to the minimum of both axes, like a real homing run.
    fn home(&self) -> Result<()> {
        let l = self.config.limits;
        self.goto(l.pan_min_deg, l.tilt_min_deg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::test_support::TempDir;
    use std::time::Duration;

    fn sim() -> (Sim, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new());
        let config = SimConfig {
            limits: Limits {
                pan_min_deg: 0.0,
                pan_max_deg: 100.0,
                tilt_min_deg: 0.0,
                tilt_max_deg: 50.0,
            },
            pan_speed_deg_s: 10.0,
            tilt_speed_deg_s: 5.0,
            home: Some((50.0, 25.0)),
        };
        let sim = Sim::with_clock(config, PresetStore::in_memory(), clock.clone()).unwrap();
        (sim, clock)
    }

    fn secs(s: f32) -> Duration {
        Duration::from_secs_f32(s)
    }

    #[test]
    fn integrates_velocity_over_time() {
        let (sim, clock) = sim();
        sim.continuous_move(1.0, -0.5).unwrap();
        clock.advance(secs(2.0));
        let p = sim.position();
        assert_eq!((p.pan_deg, p.tilt_deg), (70.0, 20.0));
        assert!(p.moving);
        sim.stop().unwrap();
        clock.advance(secs(5.0));
        let p = sim.position();
        assert_eq!((p.pan_deg, p.tilt_deg), (70.0, 20.0));
        assert!(!p.moving);
    }

    #[test]
    fn clamps_at_limits_and_reports_not_moving() {
        let (sim, clock) = sim();
        sim.continuous_move(-1.0, 1.0).unwrap();
        clock.advance(secs(60.0));
        let p = sim.position();
        assert_eq!((p.pan_deg, p.tilt_deg), (0.0, 50.0));
        assert!(!p.moving, "pinned against both end stops");
        sim.continuous_move(1.0, 0.0).unwrap();
        assert!(sim.position().moving, "backing off the stop moves again");
    }

    #[test]
    fn goto_travels_at_speed_then_stops() {
        let (sim, clock) = sim();
        sim.goto(80.0, 30.0).unwrap();
        clock.advance(secs(1.0));
        let p = sim.position();
        assert_eq!((p.pan_deg, p.tilt_deg), (60.0, 30.0));
        assert!(p.moving);
        clock.advance(secs(3.0));
        let p = sim.position();
        assert_eq!((p.pan_deg, p.tilt_deg), (80.0, 30.0));
        assert!(!p.moving);
    }

    #[test]
    fn goto_is_clamped_and_rejects_nan() {
        let (sim, clock) = sim();
        sim.goto(500.0, -10.0).unwrap();
        clock.advance(secs(100.0));
        let p = sim.position();
        assert_eq!((p.pan_deg, p.tilt_deg), (100.0, 0.0));
        assert!(sim.goto(f32::NAN, 0.0).is_err());
    }

    #[test]
    fn latest_command_wins() {
        let (sim, clock) = sim();
        sim.goto(100.0, 25.0).unwrap();
        clock.advance(secs(1.0)); // pan 60
        sim.continuous_move(-1.0, 0.0).unwrap(); // supersedes the goto
        clock.advance(secs(1.0)); // pan 50
        sim.continuous_move(0.0, 1.0).unwrap(); // supersedes the pan move
        clock.advance(secs(1.0)); // tilt 30
        let p = sim.position();
        assert_eq!((p.pan_deg, p.tilt_deg), (50.0, 30.0));
        sim.goto(50.0, 30.0).unwrap();
        assert!(
            !sim.position().moving,
            "goto to the current spot finishes at once"
        );
    }

    #[test]
    fn velocity_is_clamped_and_validated() {
        let (sim, clock) = sim();
        sim.continuous_move(5.0, 0.0).unwrap(); // treated as 1.0
        clock.advance(secs(1.0));
        assert_eq!(sim.position().pan_deg, 60.0);
        assert!(sim.continuous_move(f32::INFINITY, 0.0).is_err());
        assert!(sim.continuous_move(0.0, f32::NAN).is_err());
        sim.continuous_move(0.0, 0.0).unwrap();
        assert!(!sim.position().moving, "zero velocity is a stop");
    }

    #[test]
    fn presets_store_and_recall_positions() {
        let dir = TempDir::new("sim-presets");
        let path = dir.path().join("presets.json");
        let clock = Arc::new(ManualClock::new());
        let config = SimConfig {
            home: Some((10.0, 10.0)),
            ..SimConfig::default()
        };
        let sim =
            Sim::with_clock(config, PresetStore::load(&path).unwrap(), clock.clone()).unwrap();
        let door = sim.set_preset("Door", None).unwrap();
        assert_eq!((door.pan_deg, door.tilt_deg), (10.0, 10.0));

        sim.goto(200.0, 100.0).unwrap();
        clock.advance(secs(60.0));
        sim.goto_preset(&door.token).unwrap();
        clock.advance(secs(60.0));
        let p = sim.position();
        assert_eq!((p.pan_deg, p.tilt_deg), (10.0, 10.0));
        assert!(sim.goto_preset("nope").is_err());

        // A fresh backend over the same file sees the preset.
        let again = Sim::new(SimConfig::default(), PresetStore::load(&path).unwrap()).unwrap();
        assert_eq!(again.list_presets(), vec![door.clone()]);
        again.remove_preset(&door.token).unwrap();
        assert!(PresetStore::load(&path).unwrap().list().is_empty());
    }

    #[test]
    fn home_goes_to_the_minimum() {
        let (sim, clock) = sim();
        sim.home().unwrap();
        assert!(sim.position().moving);
        clock.advance(secs(60.0));
        let p = sim.position();
        assert_eq!((p.pan_deg, p.tilt_deg, p.moving), (0.0, 0.0, false));
    }

    #[test]
    fn rejects_bad_config() {
        let bad = SimConfig {
            pan_speed_deg_s: 0.0,
            ..SimConfig::default()
        };
        assert!(Sim::new(bad, PresetStore::in_memory()).is_err());
        let bad = SimConfig {
            limits: Limits {
                pan_min_deg: 10.0,
                pan_max_deg: 5.0,
                ..Limits::EV200
            },
            ..SimConfig::default()
        };
        assert!(Sim::new(bad, PresetStore::in_memory()).is_err());
    }
}
