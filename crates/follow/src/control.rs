//! Follow controller: turns detections into PTZ moves (FOL-09..13).
//!
//! A pure state machine in the style of `wifi::reconcile::decide`: the
//! runtime feeds it one [`Input`] per motion grid together with `now`, and
//! carries out the single [`Action`] it returns. No I/O and no clock reads, so
//! every rule is table-tested with injected time.
//!
//! ```text
//!            target outside dead zone            head stopped + settle_ms
//!   Idle/Tracking ──── Goto ────► Settling ────────── Reseed ───────► Idle
//!        │                                                              ▲
//!        │ no target for return_after                                   │
//!        └──────────── GoHome ──► Returning ── stopped + settle_ms ─────┘
//!   any ── manual command < manual_hold ago ──► Paused ── expired, Reseed ─► Idle
//!   any ── disabled ──► Disabled (Stop if our move was running) ── enabled, Reseed ─► Idle
//! ```
//!
//! Why these rules:
//!
//! - **Settling** (FOL-10): the head's own motion lights up the whole grid,
//!   so detection is meaningless while the motors run and for `settle` after;
//!   the detector is re-seeded on exit. The head may be moving because of
//!   anybody; any `moving` pushes the end of settling out.
//! - **Proportional step, clamped** (FOL-09): the offset from the frame centre
//!   converts to degrees through the field of view, scaled by `gain` and
//!   capped at `max_step_deg`, so one wrong detection never swings the head
//!   across the room. Inside the centred dead zone nothing moves.
//! - **Soft limits** (FOL-13): the goal is clamped per axis to the board's
//!   limits; an axis at its limit simply does not move while the other still
//!   tracks.
//! - **Return home once** (FOL-11): after `return_after` without a target,
//!   one `GoHome`; not repeated until a target has been seen again. This also
//!   happens once after start, which re-homes the open-loop steppers.
//! - **Manual hold** (FOL-12): a person steering the camera wins for
//!   `manual_hold`; `onvif` reports how long ago the last non-follow command
//!   came (ptz-proto `last_manual_ms_ago`).
//!
//! The degree maths is a handful of `f32` operations per decision, not per
//! cell, which is cheap even with soft-float. The field of view is **not yet
//! measured** on the AV100; the defaults are guesses to be replaced from the
//! board profile.

use crate::detect::Detection;
use ptz_proto::{Limits, Position};
use std::time::{Duration, Instant};

/// Controller tuning. Defaults per the spec; the board profile
/// (`[ptz.follow]`) overrides them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlConfig {
    /// Width of the centred horizontal dead zone, per mille of the frame.
    pub dead_zone_x_permille: u16,
    /// Height of the centred vertical dead zone, per mille of the frame.
    pub dead_zone_y_permille: u16,
    /// Share of the measured offset to correct per move (1.0 = all of it).
    /// Below 1 to avoid overshoot from a noisy centroid.
    pub gain: f32,
    /// Largest move per axis per decision, degrees.
    pub max_step_deg: f32,
    /// Smaller corrections than this (after clamping) are skipped, degrees.
    pub min_step_deg: f32,
    /// Horizontal field of view, degrees. Unmeasured on hardware.
    pub fov_h_deg: f32,
    /// Vertical field of view, degrees. Unmeasured on hardware.
    pub fov_v_deg: f32,
    /// Pan grows to the right of the image unless set.
    pub invert_pan: bool,
    /// Tilt grows towards the top of the image unless set.
    pub invert_tilt: bool,
    /// Quiet time after the head stops before detecting again.
    pub settle: Duration,
    /// Time without a target before returning home.
    pub return_after: Duration,
    /// Time a manual PTZ command suspends tracking.
    pub manual_hold: Duration,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            dead_zone_x_permille: 300,
            dead_zone_y_permille: 400,
            gain: 0.8,
            max_step_deg: 20.0,
            min_step_deg: 1.0,
            fov_h_deg: 90.0,
            fov_v_deg: 50.0,
            invert_pan: false,
            invert_tilt: false,
            settle: Duration::from_millis(300),
            return_after: Duration::from_secs(15),
            manual_hold: Duration::from_secs(60),
        }
    }
}

/// What the controller is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Tracking,
    /// After a move: waiting for the head to stop and `until` to pass.
    Settling {
        until: Instant,
    },
    /// After `GoHome`: same wait as `Settling`.
    Returning {
        until: Instant,
    },
    /// A manual command came in; hold until `until`.
    Paused {
        until: Instant,
    },
    Disabled,
}

impl State {
    /// The wire/metrics state.
    pub fn wire(self) -> follow_proto::State {
        match self {
            State::Idle => follow_proto::State::Idle,
            State::Tracking => follow_proto::State::Tracking,
            State::Settling { .. } => follow_proto::State::Settling,
            State::Returning { .. } => follow_proto::State::Returning,
            State::Paused { .. } => follow_proto::State::Paused,
            State::Disabled => follow_proto::State::Disabled,
        }
    }
}

/// Everything one decision looks at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Input {
    /// The persisted switch.
    pub enabled: bool,
    /// The detector's target for this grid.
    pub detection: Option<Detection>,
    /// Latest PTZ status.
    pub position: Position,
    /// Soft limits from the PTZ owner.
    pub limits: Limits,
    /// How long ago the last manual PTZ command came, if known.
    pub last_manual_ms_ago: Option<u64>,
}

/// What the runtime must do now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    None,
    /// Absolute move, already clamped to the soft limits.
    Goto {
        pan_deg: f32,
        tilt_deg: f32,
    },
    /// Go to the home preset.
    GoHome,
    /// Re-seed the detector's baseline.
    Reseed,
    /// Stop the motors.
    Stop,
}

/// The controller state across decisions.
#[derive(Debug, Clone)]
pub struct Controller {
    config: ControlConfig,
    state: State,
    /// When a target was last seen (or tracking (re)started).
    last_seen: Option<Instant>,
    /// `GoHome` already sent since the last target.
    homed: bool,
}

impl Controller {
    /// A controller in [`State::Idle`]; the return-home clock starts at the
    /// first decision.
    pub fn new(config: ControlConfig) -> Self {
        Self {
            config,
            state: State::Idle,
            last_seen: None,
            homed: false,
        }
    }

    pub fn config(&self) -> &ControlConfig {
        &self.config
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// One decision. See the module docs for the rules.
    pub fn decide(&mut self, input: &Input, now: Instant) -> Action {
        let moving = input.position.moving;

        if !input.enabled {
            let ours = matches!(self.state, State::Settling { .. } | State::Returning { .. });
            if self.state == State::Disabled {
                return Action::None;
            }
            self.state = State::Disabled;
            return if ours && moving {
                Action::Stop
            } else {
                Action::None
            };
        }
        if self.state == State::Disabled {
            self.restart(now);
            return Action::Reseed;
        }

        let hold = self.config.manual_hold;
        if let Some(ago) = input.last_manual_ms_ago {
            let ago = Duration::from_millis(ago);
            if ago < hold {
                self.state = State::Paused {
                    until: now + (hold - ago),
                };
                return Action::None;
            }
        }
        if let State::Paused { until } = self.state {
            if now < until {
                return Action::None;
            }
            self.restart(now);
            return Action::Reseed;
        }

        match self.state {
            State::Settling { until } | State::Returning { until } => {
                if moving {
                    self.extend_settling(now);
                    return Action::None;
                }
                if now < until {
                    return Action::None;
                }
                self.state = State::Idle;
                return Action::Reseed;
            }
            State::Idle | State::Tracking if moving => {
                // Somebody else moved the head; the grid is useless.
                self.state = State::Settling {
                    until: now + self.config.settle,
                };
                return Action::None;
            }
            _ => {}
        }

        let last_seen = *self.last_seen.get_or_insert(now);
        let Some(target) = input.detection else {
            self.state = State::Idle;
            if !self.homed && now.saturating_duration_since(last_seen) >= self.config.return_after {
                self.homed = true;
                self.state = State::Returning {
                    until: now + self.config.settle,
                };
                return Action::GoHome;
            }
            return Action::None;
        };

        self.last_seen = Some(now);
        self.homed = false;
        self.state = State::Tracking;
        match self.step(&target, &input.position, &input.limits) {
            Some((pan_deg, tilt_deg)) => {
                self.state = State::Settling {
                    until: now + self.config.settle,
                };
                Action::Goto { pan_deg, tilt_deg }
            }
            None => Action::None,
        }
    }

    /// Fresh start after enable or a manual hold.
    fn restart(&mut self, now: Instant) {
        self.state = State::Idle;
        self.last_seen = Some(now);
        self.homed = false;
    }

    fn extend_settling(&mut self, now: Instant) {
        let until = now + self.config.settle;
        self.state = match self.state {
            State::Returning { .. } => State::Returning { until },
            _ => State::Settling { until },
        };
    }

    /// The clamped goal for `target`, or `None` when no axis needs to move.
    fn step(&self, target: &Detection, pos: &Position, limits: &Limits) -> Option<(f32, f32)> {
        let c = &self.config;
        let dx = axis_delta(
            target.cx_permille,
            c.dead_zone_x_permille,
            c.fov_h_deg,
            c,
            c.invert_pan,
        );
        // Image y grows downwards; tilt grows upwards.
        let dy = axis_delta(
            target.cy_permille,
            c.dead_zone_y_permille,
            c.fov_v_deg,
            c,
            !c.invert_tilt,
        );
        let pan = (pos.pan_deg + dx).clamp(limits.pan_min_deg, limits.pan_max_deg);
        let tilt = (pos.tilt_deg + dy).clamp(limits.tilt_min_deg, limits.tilt_max_deg);
        let pan_moves = (pan - pos.pan_deg).abs() >= c.min_step_deg;
        let tilt_moves = (tilt - pos.tilt_deg).abs() >= c.min_step_deg;
        if !pan_moves && !tilt_moves {
            return None;
        }
        Some((
            if pan_moves { pan } else { pos.pan_deg },
            if tilt_moves { tilt } else { pos.tilt_deg },
        ))
    }
}

/// Degrees to move one axis for a centroid at `at` per mille: zero inside
/// the centred dead zone, else `offset × fov × gain`, capped at
/// `max_step_deg`.
fn axis_delta(at: u16, dead_zone: u16, fov_deg: f32, c: &ControlConfig, invert: bool) -> f32 {
    let offset = i32::from(at.min(1000)) - 500;
    if offset.unsigned_abs() <= u32::from(dead_zone / 2) {
        return 0.0;
    }
    let deg = (offset as f32 / 1000.0 * fov_deg * c.gain).clamp(-c.max_step_deg, c.max_step_deg);
    if invert {
        -deg
    } else {
        deg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::BBox;

    const LIMITS: Limits = Limits {
        pan_min_deg: 0.0,
        pan_max_deg: 350.0,
        tilt_min_deg: 0.0,
        tilt_max_deg: 65.0,
    };

    fn target(x: u16, y: u16) -> Option<Detection> {
        Some(Detection {
            cx_permille: x,
            cy_permille: y,
            bbox: BBox {
                col_min: 0,
                row_min: 0,
                col_max: 1,
                row_max: 1,
            },
            cells: 4,
        })
    }

    fn input(detection: Option<Detection>, pan: f32, tilt: f32, moving: bool) -> Input {
        Input {
            enabled: true,
            detection,
            position: Position {
                pan_deg: pan,
                tilt_deg: tilt,
                moving,
            },
            limits: LIMITS,
            last_manual_ms_ago: None,
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn steps_toward_the_target() {
        // (x, y, pan, tilt) -> expected Goto, or None.
        type Case = (u16, u16, f32, f32, Option<(f32, f32)>);
        let cases: [Case; 8] = [
            // Centre and dead-zone edges: no move.
            (500, 500, 180.0, 30.0, None),
            (650, 700, 180.0, 30.0, None),
            (350, 300, 180.0, 30.0, None),
            // Right: 250‰ × 90° × 0.8 = 18°.
            (750, 500, 180.0, 30.0, Some((198.0, 30.0))),
            // Far left: 500‰ × 90 × 0.8 = 36° → capped at 20°.
            (0, 500, 180.0, 30.0, Some((160.0, 30.0))),
            // Down: 300‰ × 50° × 0.8 = 12° → tilt decreases.
            (500, 800, 180.0, 30.0, Some((180.0, 18.0))),
            // Pan at its max: only tilt moves (FOL-13).
            (900, 100, 350.0, 30.0, Some((350.0, 46.0))),
            // Both axes at limits: nothing.
            (1000, 0, 350.0, 65.0, None),
        ];
        for (x, y, pan, tilt, want) in cases {
            let mut c = Controller::new(ControlConfig::default());
            let got = c.decide(&input(target(x, y), pan, tilt, false), Instant::now());
            let want = want.map_or(Action::None, |(pan_deg, tilt_deg)| Action::Goto {
                pan_deg,
                tilt_deg,
            });
            match (got, want) {
                (
                    Action::Goto { pan_deg, tilt_deg },
                    Action::Goto {
                        pan_deg: wp,
                        tilt_deg: wt,
                    },
                ) => assert!(
                    (pan_deg - wp).abs() < 0.01 && (tilt_deg - wt).abs() < 0.01,
                    "({x},{y}) at ({pan},{tilt}): {got:?} != {want:?}"
                ),
                _ => assert_eq!(got, want, "({x},{y}) at ({pan},{tilt})"),
            }
        }
    }

    #[test]
    fn clamps_to_soft_limits() {
        let mut c = Controller::new(ControlConfig::default());
        let got = c.decide(
            &input(target(1000, 500), 345.0, 30.0, false),
            Instant::now(),
        );
        assert_eq!(
            got,
            Action::Goto {
                pan_deg: 350.0,
                tilt_deg: 30.0
            }
        );
    }

    #[test]
    fn inverted_axes() {
        let config = ControlConfig {
            invert_pan: true,
            invert_tilt: true,
            ..ControlConfig::default()
        };
        let mut c = Controller::new(config);
        let got = c.decide(&input(target(750, 800), 180.0, 30.0, false), Instant::now());
        assert_eq!(
            got,
            Action::Goto {
                pan_deg: 162.0,
                tilt_deg: 42.0
            }
        );
    }

    #[test]
    fn settles_after_a_move_then_reseeds() {
        let t0 = Instant::now();
        let mut c = Controller::new(ControlConfig::default());
        assert!(matches!(
            c.decide(&input(target(900, 500), 180.0, 30.0, false), t0),
            Action::Goto { .. }
        ));
        assert_eq!(c.state().wire(), follow_proto::State::Settling);
        // (offset from t0 in ms, moving, detection?) -> action, wire state.
        let steps = [
            (100, true, true, Action::None, follow_proto::State::Settling),
            (
                1500,
                true,
                true,
                Action::None,
                follow_proto::State::Settling,
            ),
            // Stopped at 1600; settle runs until 1500 + 300.
            (
                1600,
                false,
                true,
                Action::None,
                follow_proto::State::Settling,
            ),
            (
                1799,
                false,
                true,
                Action::None,
                follow_proto::State::Settling,
            ),
            (1800, false, true, Action::Reseed, follow_proto::State::Idle),
        ];
        for (at, moving, det, want, state) in steps {
            let d = if det { target(900, 500) } else { None };
            let got = c.decide(&input(d, 180.0, 30.0, moving), t0 + ms(at));
            assert_eq!(got, want, "at {at} ms");
            assert_eq!(c.state().wire(), state, "at {at} ms");
        }
        // Tracking resumes on the next detection.
        assert!(matches!(
            c.decide(&input(target(900, 500), 200.0, 30.0, false), t0 + ms(1900)),
            Action::Goto { .. }
        ));
    }

    #[test]
    fn foreign_motion_suspends_detection() {
        let t0 = Instant::now();
        let mut c = Controller::new(ControlConfig::default());
        let got = c.decide(&input(target(900, 500), 180.0, 30.0, true), t0);
        assert_eq!(got, Action::None);
        assert_eq!(c.state().wire(), follow_proto::State::Settling);
        assert_eq!(
            c.decide(&input(None, 180.0, 30.0, false), t0 + ms(300)),
            Action::Reseed
        );
    }

    #[test]
    fn target_inside_dead_zone_is_tracking() {
        let mut c = Controller::new(ControlConfig::default());
        let got = c.decide(&input(target(520, 480), 180.0, 30.0, false), Instant::now());
        assert_eq!(got, Action::None);
        assert_eq!(c.state(), State::Tracking);
    }

    #[test]
    fn returns_home_once_after_losing_the_target() {
        let t0 = Instant::now();
        let mut c = Controller::new(ControlConfig::default());
        c.decide(&input(target(520, 500), 180.0, 30.0, false), t0);
        assert_eq!(c.state(), State::Tracking);
        // (ms, moving) -> action, wire state.
        let steps = [
            (1_000, false, Action::None, follow_proto::State::Idle),
            (14_999, false, Action::None, follow_proto::State::Idle),
            (
                15_000,
                false,
                Action::GoHome,
                follow_proto::State::Returning,
            ),
            (15_100, true, Action::None, follow_proto::State::Returning),
            (20_000, true, Action::None, follow_proto::State::Returning),
            (20_100, false, Action::None, follow_proto::State::Returning),
            (20_300, false, Action::Reseed, follow_proto::State::Idle),
            // Home already: no second GoHome.
            (60_000, false, Action::None, follow_proto::State::Idle),
        ];
        for (at, moving, want, state) in steps {
            let got = c.decide(&input(None, 180.0, 30.0, moving), t0 + ms(at));
            assert_eq!(got, want, "at {at} ms");
            assert_eq!(c.state().wire(), state, "at {at} ms");
        }
        // A new target re-arms the return.
        c.decide(
            &input(target(500, 500), 180.0, 30.0, false),
            t0 + ms(61_000),
        );
        assert_eq!(
            c.decide(&input(None, 180.0, 30.0, false), t0 + ms(76_000)),
            Action::GoHome
        );
    }

    #[test]
    fn manual_command_pauses_tracking() {
        let t0 = Instant::now();
        let mut c = Controller::new(ControlConfig::default());
        let mut i = input(target(900, 500), 180.0, 30.0, false);
        i.last_manual_ms_ago = Some(10_000);
        assert_eq!(c.decide(&i, t0), Action::None);
        assert_eq!(
            c.state(),
            State::Paused {
                until: t0 + ms(50_000)
            }
        );
        // Still paused even if onvif stops reporting the age.
        i.last_manual_ms_ago = None;
        assert_eq!(c.decide(&i, t0 + ms(49_999)), Action::None);
        assert_eq!(c.state().wire(), follow_proto::State::Paused);
        // Expired: re-seed first, then track.
        assert_eq!(c.decide(&i, t0 + ms(50_000)), Action::Reseed);
        assert_eq!(c.state(), State::Idle);
        assert!(matches!(c.decide(&i, t0 + ms(50_100)), Action::Goto { .. }));
        // An old manual command does not pause.
        let mut c = Controller::new(ControlConfig::default());
        i.last_manual_ms_ago = Some(60_000);
        assert!(matches!(c.decide(&i, t0), Action::Goto { .. }));
    }

    #[test]
    fn disabling_stops_our_move_and_reenabling_reseeds() {
        let t0 = Instant::now();
        let mut c = Controller::new(ControlConfig::default());
        assert!(matches!(
            c.decide(&input(target(900, 500), 180.0, 30.0, false), t0),
            Action::Goto { .. }
        ));
        let mut i = input(target(900, 500), 190.0, 30.0, true);
        i.enabled = false;
        assert_eq!(c.decide(&i, t0 + ms(100)), Action::Stop);
        assert_eq!(c.state(), State::Disabled);
        assert_eq!(c.decide(&i, t0 + ms(200)), Action::None);
        i.enabled = true;
        i.position.moving = false;
        assert_eq!(c.decide(&i, t0 + ms(300)), Action::Reseed);
        assert_eq!(c.state(), State::Idle);
    }

    #[test]
    fn disabling_while_idle_or_someone_else_moves_does_not_stop() {
        let t0 = Instant::now();
        let mut c = Controller::new(ControlConfig::default());
        let mut i = input(None, 180.0, 30.0, true);
        i.enabled = false;
        assert_eq!(c.decide(&i, t0), Action::None);
        assert_eq!(c.state().wire(), follow_proto::State::Disabled);
    }

    #[test]
    fn disabled_controller_never_returns_home() {
        let t0 = Instant::now();
        let mut c = Controller::new(ControlConfig::default());
        let mut i = input(None, 180.0, 30.0, false);
        i.enabled = false;
        for s in [0, 20, 40, 80] {
            assert_eq!(c.decide(&i, t0 + Duration::from_secs(s)), Action::None);
        }
    }
}
