//! Motion planning for the EV200 SDK backend, kept free of FFI so it runs in
//! host tests.
//!
//! # What the vendor library actually does
//!
//! Established by disassembling `sdk/lib/rtsp/libplat_drv.so` (the header's
//! comments are wrong in several places):
//!
//! * Each axis has a worker thread inside the SDK, started by
//!   `ak_drv_ptz_check_self`. `ak_drv_ptz_turn(dir, deg)` only stores the
//!   request and posts that thread's semaphore; it returns at once.
//! * The request is rejected (`-1`, "motor busy") unless the axis status is
//!   `PTZ_INIT_OK`. The thread sets `PTZ_SYS_WAIT` while the motor runs and
//!   back to `PTZ_INIT_OK` when the driver reports the stop. **A move cannot
//!   be interrupted**: the library exports no stop call (`ak_drv_ptz_turn_stop`
//!   is declared in the header but absent from the `.so`).
//! * `deg` is in **degrees**, not motor steps. The library converts with
//!   `steps = deg * 4096 * angle_rate / 720` (so `angle_rate = 2.0` means
//!   4096 steps per revolution, ≈11.4 steps/°), clamps the result to
//!   `[0, max]` of that axis, and updates its internal position as soon as the
//!   ioctl is accepted.
//! * `PTZ_TURN_LEFT`/`PTZ_TURN_UP` *increase* the internal degree count,
//!   `RIGHT`/`DOWN` decrease it. Whether "left" is physically left depends on
//!   how the motor is wired — hence the `invert_*` options.
//! * Moves of more than 16° (pan) / 8° (tilt) run at the fast speed (100),
//!   shorter ones at the slow speed (15).
//!
//! # How continuous motion is built on that
//!
//! Since a move cannot be cancelled, continuous motion is a chain of short
//! moves: whenever an axis is idle and still has a velocity command, the
//! worker issues the next step (1..=`max_step_deg`, scaled by speed). Stop and
//! "latest command wins" therefore take effect at the end of the step in
//! flight. `goto` is chunked the same way (`goto_step_deg`) so that a new
//! command can take over part-way through a long travel.
//!
//! Only relative `ak_drv_ptz_turn` is used: `ak_drv_ptz_turn_to_pos` needs
//! *both* axes idle and returns `-1` whenever either axis is already at its
//! target, which makes its result useless for error handling.

use crate::{Limits, Result};
use std::time::{Duration, Instant};

/// Configuration of the EV200 SDK backend.
///
/// Available on every build so consumers can carry it in their own config
/// even where the backend itself is compiled out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SdkConfig {
    /// Software travel limits, in the SDK's degree units (0 = end stop found
    /// by the self-check). Must lie within `[0, travel_*_deg]`.
    pub limits: Limits,
    /// Nominal pan travel passed to `ak_drv_ptz_set_degree` before the
    /// self-check. The library accepts a measured range within ±20° of it.
    pub travel_pan_deg: i32,
    /// Nominal tilt travel passed to `ak_drv_ptz_set_degree`.
    pub travel_tilt_deg: i32,
    /// Steps-per-degree factor for `ak_drv_ptz_set_angle_rate` (see module
    /// docs); 2.0 is both the library default and what `ptz_daemon.c` sets.
    pub angle_rate: f32,
    /// Board has end-stop feedback pins (`PTZ_FEEDBACK_PIN_EXIST`).
    pub feedback_pin: bool,
    /// Largest single step of a continuous move, in degrees. Bounds how far
    /// the head coasts after a stop.
    pub max_step_deg: i32,
    /// Largest single step of a `goto`, in degrees.
    pub goto_step_deg: i32,
    /// Positive pan velocity turns towards decreasing degrees.
    pub invert_pan: bool,
    /// Positive tilt velocity turns towards decreasing degrees.
    pub invert_tilt: bool,
    /// How often the worker polls motor status while something is moving.
    pub poll_interval: Duration,
    /// After issuing a step, how long an axis that never reported busy is
    /// still treated as moving (the SDK thread may not have picked it up yet).
    pub settle_timeout: Duration,
    /// Where to go after the self-check; `None` stays at the centre the
    /// self-check leaves the head in.
    pub home: Option<(f32, f32)>,
}

/// Pan motor node the `Auto` probe looks for to pick the SDK backend (the
/// library opens it and `/dev/ak-motor1` itself). Built-in default of
/// [`OpenOptions::ak_motor_dev`](crate::OpenOptions::ak_motor_dev).
pub const SDK_MOTOR_DEVICE: &str = "/dev/ak-motor0";

impl Limits {
    /// The limits `ptz_daemon.c` enforces on EV200 heads: X 0–350, Y 0–120.
    /// The SDK backend's built-in default, used when no board profile gives
    /// the head's ranges.
    pub const EV200: Limits = Limits {
        pan_min_deg: 0.0,
        pan_max_deg: 350.0,
        tilt_min_deg: 0.0,
        tilt_max_deg: 120.0,
    };
}

impl Default for SdkConfig {
    fn default() -> Self {
        Self {
            limits: Limits::EV200,
            // ptz_daemon.c: ak_drv_ptz_set_degree(0x168, 0xc0).
            travel_pan_deg: 360,
            travel_tilt_deg: 192,
            angle_rate: 2.0,
            feedback_pin: false,
            max_step_deg: 5,
            goto_step_deg: 30,
            invert_pan: false,
            invert_tilt: false,
            poll_interval: Duration::from_millis(20),
            settle_timeout: Duration::from_millis(300),
            home: None,
        }
    }
}

impl SdkConfig {
    /// Checks the values the SDK would misbehave on.
    pub fn validate(&self) -> Result<()> {
        use crate::Error;
        self.limits.validate()?;
        if self.travel_pan_deg <= 0 || self.travel_tilt_deg <= 0 {
            return Err(Error::invalid("travel", "must be positive"));
        }
        if self.limits.pan_min_deg < 0.0
            || self.limits.tilt_min_deg < 0.0
            || self.limits.pan_max_deg > self.travel_pan_deg as f32
            || self.limits.tilt_max_deg > self.travel_tilt_deg as f32
        {
            return Err(Error::invalid(
                "limits",
                "must lie within [0, travel] on each axis",
            ));
        }
        if !(self.angle_rate.is_finite() && self.angle_rate > 0.0) {
            return Err(Error::invalid("angle_rate", "must be a positive number"));
        }
        if self.max_step_deg < 1 || self.goto_step_deg < 1 {
            return Err(Error::invalid("step size", "must be at least 1 degree"));
        }
        if self.poll_interval.is_zero() {
            return Err(Error::invalid("poll_interval", "must be non-zero"));
        }
        Ok(())
    }

    /// Where the self-check leaves the head: the middle of each axis.
    ///
    /// The library homes against the end stop, then turns `max / 2`, where
    /// `max` is the *measured* range (within ±20° of the nominal travel). It
    /// does not export the measured value, so the nominal one is assumed.
    pub fn post_check_position(&self) -> (i32, i32) {
        (self.travel_pan_deg / 2, self.travel_tilt_deg / 2)
    }
}

/// What the user last asked for. Latest command wins: each new one replaces it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Desired {
    Idle,
    /// Normalised user velocities in `[-1, 1]`, before inversion.
    Velocity {
        pan: f32,
        tilt: f32,
    },
    /// Absolute target in whole SDK degrees, inside the limits.
    Target {
        pan: i32,
        tilt: i32,
    },
}

/// One axis's share of [`Desired`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum AxisGoal {
    Hold,
    /// Signed speed in `[-1, 1]` along increasing SDK degrees.
    Velocity(f32),
    Target(i32),
}

impl Desired {
    /// Splits into per-axis goals, applying the direction inversion.
    pub(crate) fn goals(self, invert_pan: bool, invert_tilt: bool) -> [AxisGoal; 2] {
        let sign = |invert: bool, v: f32| if invert { -v } else { v };
        match self {
            Desired::Idle => [AxisGoal::Hold, AxisGoal::Hold],
            Desired::Velocity { pan, tilt } => [
                AxisGoal::Velocity(sign(invert_pan, pan)),
                AxisGoal::Velocity(sign(invert_tilt, tilt)),
            ],
            Desired::Target { pan, tilt } => [AxisGoal::Target(pan), AxisGoal::Target(tilt)],
        }
    }
}

/// Integer travel range of one axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AxisRange {
    pub min: i32,
    pub max: i32,
}

impl AxisRange {
    /// Pan and tilt ranges, rounded inwards to whole degrees.
    pub(crate) fn from_limits(limits: &Limits) -> [AxisRange; 2] {
        [
            AxisRange {
                min: limits.pan_min_deg.ceil() as i32,
                max: limits.pan_max_deg.floor() as i32,
            },
            AxisRange {
                min: limits.tilt_min_deg.ceil() as i32,
                max: limits.tilt_max_deg.floor() as i32,
            },
        ]
    }
}

/// Signed degrees to turn next on one axis; 0 means nothing to do.
pub(crate) fn next_delta(
    pos: i32,
    range: AxisRange,
    goal: AxisGoal,
    max_step: i32,
    goto_step: i32,
) -> i32 {
    let wanted = match goal {
        AxisGoal::Hold => 0,
        AxisGoal::Velocity(0.0) => 0,
        AxisGoal::Velocity(v) => {
            // Speed scales the step: the SDK offers no speed control for
            // short moves, so a shorter step is the only way to go slower.
            let size = ((v.abs() * max_step as f32).round() as i32).clamp(1, max_step);
            if v > 0.0 {
                size
            } else {
                -size
            }
        }
        AxisGoal::Target(t) => (t - pos).clamp(-goto_step, goto_step),
    };
    (pos + wanted).clamp(range.min, range.max) - pos
}

/// Tracks whether one axis may accept a new step.
///
/// The SDK status alone is not enough: right after `ak_drv_ptz_turn` returns,
/// the SDK thread may not have woken yet and the status still reads idle.
/// Issuing again then would overwrite the pending request and the internal
/// position would diverge from ours, so an axis counts as busy until it has
/// been seen busy and then idle — or, for a step too short to ever be seen
/// busy, until `settle_timeout` has passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AxisPhase {
    Idle,
    Issued { at: Instant, seen_busy: bool },
}

impl AxisPhase {
    /// Folds in one status poll.
    pub(crate) fn observe(&mut self, sdk_busy: bool, now: Instant, settle: Duration) {
        if let AxisPhase::Issued { at, seen_busy } = self {
            if sdk_busy {
                *seen_busy = true;
            } else if *seen_busy || now.saturating_duration_since(*at) >= settle {
                *self = AxisPhase::Idle;
            }
        }
    }

    pub(crate) fn is_idle(&self) -> bool {
        matches!(self, AxisPhase::Idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGE: AxisRange = AxisRange { min: 0, max: 100 };

    #[test]
    fn velocity_steps_scale_with_speed() {
        assert_eq!(next_delta(50, RANGE, AxisGoal::Velocity(1.0), 5, 30), 5);
        assert_eq!(next_delta(50, RANGE, AxisGoal::Velocity(-0.5), 5, 30), -3);
        // Tiny but non-zero speeds still move one degree at a time.
        assert_eq!(next_delta(50, RANGE, AxisGoal::Velocity(0.01), 5, 30), 1);
        assert_eq!(next_delta(50, RANGE, AxisGoal::Velocity(0.0), 5, 30), 0);
        assert_eq!(next_delta(50, RANGE, AxisGoal::Hold, 5, 30), 0);
    }

    #[test]
    fn steps_stop_at_the_limits() {
        assert_eq!(next_delta(98, RANGE, AxisGoal::Velocity(1.0), 5, 30), 2);
        assert_eq!(next_delta(100, RANGE, AxisGoal::Velocity(1.0), 5, 30), 0);
        assert_eq!(next_delta(0, RANGE, AxisGoal::Velocity(-1.0), 5, 30), 0);
    }

    #[test]
    fn goto_is_chunked() {
        assert_eq!(next_delta(0, RANGE, AxisGoal::Target(100), 5, 30), 30);
        assert_eq!(next_delta(90, RANGE, AxisGoal::Target(100), 5, 30), 10);
        assert_eq!(next_delta(100, RANGE, AxisGoal::Target(100), 5, 30), 0);
        assert_eq!(next_delta(50, RANGE, AxisGoal::Target(0), 5, 30), -30);
    }

    #[test]
    fn inversion_flips_only_velocity() {
        let v = Desired::Velocity {
            pan: 0.5,
            tilt: -1.0,
        };
        assert_eq!(
            v.goals(true, false),
            [AxisGoal::Velocity(-0.5), AxisGoal::Velocity(-1.0)]
        );
        let t = Desired::Target { pan: 3, tilt: 4 };
        assert_eq!(
            t.goals(true, true),
            [AxisGoal::Target(3), AxisGoal::Target(4)]
        );
    }

    #[test]
    fn ranges_round_inwards() {
        let limits = Limits {
            pan_min_deg: 0.5,
            pan_max_deg: 349.9,
            tilt_min_deg: 0.0,
            tilt_max_deg: 120.0,
        };
        assert_eq!(
            AxisRange::from_limits(&limits),
            [
                AxisRange { min: 1, max: 349 },
                AxisRange { min: 0, max: 120 }
            ]
        );
    }

    #[test]
    fn phase_waits_for_busy_then_idle() {
        let t0 = Instant::now();
        let settle = Duration::from_millis(300);
        let mut phase = AxisPhase::Issued {
            at: t0,
            seen_busy: false,
        };
        // SDK thread has not woken yet: still counts as moving.
        phase.observe(false, t0 + Duration::from_millis(20), settle);
        assert!(!phase.is_idle());
        phase.observe(true, t0 + Duration::from_millis(40), settle);
        assert!(!phase.is_idle());
        phase.observe(false, t0 + Duration::from_millis(60), settle);
        assert!(phase.is_idle());
    }

    #[test]
    fn phase_times_out_for_steps_too_short_to_see() {
        let t0 = Instant::now();
        let settle = Duration::from_millis(300);
        let mut phase = AxisPhase::Issued {
            at: t0,
            seen_busy: false,
        };
        phase.observe(false, t0 + Duration::from_millis(299), settle);
        assert!(!phase.is_idle());
        phase.observe(false, t0 + Duration::from_millis(300), settle);
        assert!(phase.is_idle());
    }

    #[test]
    fn config_validation() {
        assert!(SdkConfig::default().validate().is_ok());
        let mut bad = SdkConfig::default();
        bad.limits.tilt_max_deg = 200.0;
        assert!(bad.validate().is_err(), "tilt limit beyond 192° travel");
        let bad = SdkConfig {
            angle_rate: 0.0,
            ..SdkConfig::default()
        };
        assert!(bad.validate().is_err());
        assert_eq!(SdkConfig::default().post_check_position(), (180, 96));
    }
}
