//! EV200 backend over the vendor `ak_drv_ptz_*` API (`libplat_drv.so`).
//!
//! Ports the init sequence of `cross-compile/IOT-ANYKA-PTZdaemon`
//! (`ptz_daemon.c`, `ptz_driver.c`): open, set angle rate and travel, then
//! the self-check that homes both motors against their end stops. The library
//! drives `/dev/ak-motor0` (pan) and `/dev/ak-motor1` (tilt).
//!
//! # Deviations from `ptz_daemon.c`, and why
//!
//! * **Call order.** The daemon calls `set_degree`/`set_angle_rate` *after*
//!   `check_self`. The self-check reads both values to size its homing sweep
//!   and to validate the measured range, so calling them afterwards only
//!   affects a later self-check. This follows `ptz_driver.c` instead and sets
//!   them first.
//! * **`set_angle_rate` argument.** The header declares `(float, float)`; the
//!   daemon's hand-written prototype passes `int 0x40000000`. On this
//!   soft-float EABI both travel in `r0`/`r1` as raw bits, and `0x40000000` is
//!   the IEEE-754 encoding of `2.0f`, which the library then uses in float
//!   arithmetic (`__mulsf3`). So the daemon's value *is* `2.0`, and the
//!   bindgen prototype passes `2.0_f32` correctly. It is also the library's
//!   built-in default (`.data` holds `0x40000000` for both axes).
//! * **Start position.** The daemon assumes (180, 90). The self-check leaves
//!   each axis at *half its measured range*; with the nominal 360/192 that is
//!   (180, 96). See [`SdkConfig::post_check_position`].
//! * **Relative moves.** The daemon moves with `turn_to_pos`; this uses
//!   relative `turn` — see [`crate::stepper`] for why.
//!
//! # Threading
//!
//! Every SDK call after [`Sdk::open`] happens on one worker thread, which owns
//! the step loop. Trait methods only update the shared "desired" command under
//! a mutex and wake the worker, so they never block on the motors. The mutex
//! is held across `ak_drv_ptz_turn`/`ak_drv_ptz_get_status`; both only touch
//! library memory and post a semaphore, so that is microseconds.

use crate::error::{Error, Result};
use crate::preset::PresetStore;
use crate::stepper::{next_delta, AxisPhase, AxisRange, Desired, SdkConfig};
use crate::{check_velocity, Limits, Position, PtzBackend};
use anyka_sys::{ptz_device, ptz_feedback_pin, ptz_status, ptz_turn_direction};
use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

/// The library keeps all driver state in globals, so one handle per process.
static OPEN: AtomicBool = AtomicBool::new(false);

/// Worker stack size. The loop is shallow; this leaves room for tracing.
const WORKER_STACK: usize = 64 * 1024;

const PAN: usize = 0;
const TILT: usize = 1;

/// Converts an SDK status return into a `Result`, capturing the error number.
fn check(op: &'static str, ret: i32) -> Result<()> {
    if ret == 0 {
        return Ok(());
    }
    // SAFETY: reads the SDK's process-global error number; no arguments.
    let code = unsafe { anyka_sys::ak_get_error_no() };
    Err(Error::Sdk { op, code })
}

/// Thin safe wrappers, one per exported `ak_drv_ptz_*` symbol used.
///
/// Only functions `libplat_drv.so` really exports are called. The header
/// also declares `turn_stop`, `set_speed`, `reset_dg`, `get_step_pos`,
/// `setup_step_param` and `check_self_ex`; none of them are in the library.
mod drv {
    use super::*;

    pub(super) fn version() -> String {
        // SAFETY: no arguments; returns a pointer to a static string in the
        // library (or null), which is copied before returning.
        let ptr = unsafe { anyka_sys::ak_drv_ptz_get_version() };
        if ptr.is_null() {
            return String::from("unknown");
        }
        // SAFETY: non-null, NUL-terminated static string owned by the library.
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }

    pub(super) fn open() -> Result<()> {
        // SAFETY: no arguments. Opens /dev/ak-motor{0,1}; fails if already open.
        check("ak_drv_ptz_open", unsafe { anyka_sys::ak_drv_ptz_open() })
    }

    pub(super) fn set_angle_rate(h: f32, v: f32) {
        // SAFETY: plain floats, stored into library globals.
        unsafe { anyka_sys::ak_drv_ptz_set_angle_rate(h, v) }
    }

    pub(super) fn set_degree(h: i32, v: i32) {
        // SAFETY: plain integers, stored into library globals.
        unsafe { anyka_sys::ak_drv_ptz_set_degree(h, v) }
    }

    pub(super) fn check_self(feedback_pin: bool) -> Result<()> {
        let pin = if feedback_pin {
            ptz_feedback_pin::PTZ_FEEDBACK_PIN_EXIST
        } else {
            ptz_feedback_pin::PTZ_FEEDBACK_PIN_NONE
        };
        // SAFETY: enum value, no pointers. Blocks while both motors home, then
        // starts the library's per-axis worker threads.
        check("ak_drv_ptz_check_self", unsafe {
            anyka_sys::ak_drv_ptz_check_self(pin)
        })
    }

    /// `true` while the axis is executing a move.
    pub(super) fn busy(axis: usize) -> Result<bool> {
        let device = if axis == PAN {
            ptz_device::PTZ_DEV_H
        } else {
            ptz_device::PTZ_DEV_V
        };
        let mut status: ptz_status::Type = ptz_status::PTZ_WAIT_INIT;
        // SAFETY: `status` is a valid, writable enum-sized out-parameter that
        // outlives the call; the device index is 0 or 1.
        let ret = unsafe { anyka_sys::ak_drv_ptz_get_status(device, &mut status) };
        check("ak_drv_ptz_get_status", ret)?;
        Ok(status != ptz_status::PTZ_INIT_OK)
    }

    /// Relative move of `delta` whole degrees (sign = direction).
    pub(super) fn turn(axis: usize, delta: i32) -> Result<()> {
        let direction = match (axis == PAN, delta > 0) {
            (true, true) => ptz_turn_direction::PTZ_TURN_LEFT,
            (true, false) => ptz_turn_direction::PTZ_TURN_RIGHT,
            (false, true) => ptz_turn_direction::PTZ_TURN_UP,
            (false, false) => ptz_turn_direction::PTZ_TURN_DOWN,
        };
        // SAFETY: enum value and integer, no pointers. Only queues the move.
        let ret = unsafe { anyka_sys::ak_drv_ptz_turn(direction, delta.abs()) };
        check("ak_drv_ptz_turn", ret)
    }

    pub(super) fn close() {
        // SAFETY: no arguments. Joins the library's threads (after any move
        // in flight finishes) and closes the device files; a no-op if the
        // driver was never fully opened.
        unsafe { anyka_sys::ak_drv_ptz_close() };
    }
}

#[derive(Debug)]
struct State {
    desired: Desired,
    /// Our copy of the library's internal position, whole degrees.
    pos: [i32; 2],
    phase: [AxisPhase; 2],
    shutdown: bool,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replaces the desired command (latest wins) and wakes the worker.
    fn command(&self, desired: Desired) {
        self.lock().desired = desired;
        self.wake.notify_one();
    }
}

/// EV200 pan/tilt head driven through the vendor SDK.
///
/// Dropping it stops issuing steps, waits for the step in flight, and closes
/// the driver.
#[derive(Debug)]
pub struct Sdk {
    config: SdkConfig,
    ranges: [AxisRange; 2],
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    presets: PresetStore,
}

impl Sdk {
    /// Opens the driver and runs the self-check.
    ///
    /// **Blocks** while both motors sweep to their end stops and back to the
    /// centre — tens of seconds. Call it before starting the async runtime or
    /// from `spawn_blocking`.
    pub fn open(config: SdkConfig, presets: PresetStore) -> Result<Self> {
        config.validate()?;
        if OPEN
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::AlreadyOpen);
        }
        match Self::open_inner(config, presets) {
            Ok(sdk) => Ok(sdk),
            Err(e) => {
                drv::close();
                OPEN.store(false, Ordering::Release);
                Err(e)
            }
        }
    }

    fn open_inner(config: SdkConfig, presets: PresetStore) -> Result<Self> {
        tracing::info!(version = %drv::version(), "opening PTZ driver");
        drv::open()?;
        drv::set_angle_rate(config.angle_rate, config.angle_rate);
        drv::set_degree(config.travel_pan_deg, config.travel_tilt_deg);
        tracing::info!("PTZ self-check: homing both axes");
        drv::check_self(config.feedback_pin)?;
        // check_self returns 0 even when homing timed out; the library only
        // starts accepting moves if both axes ended in PTZ_INIT_OK.
        for axis in [PAN, TILT] {
            if drv::busy(axis)? {
                return Err(Error::Sdk {
                    op: "ak_drv_ptz_check_self",
                    code: -1,
                });
            }
        }

        let (pan, tilt) = config.post_check_position();
        let ranges = AxisRange::from_limits(&config.limits);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                desired: Desired::Idle,
                pos: [pan, tilt],
                phase: [AxisPhase::Idle; 2],
                shutdown: false,
            }),
            wake: Condvar::new(),
        });
        if let Some((pan, tilt)) = config.home {
            let (pan, tilt) = config.limits.clamp(pan, tilt)?;
            shared.lock().desired = Desired::Target {
                pan: pan.round() as i32,
                tilt: tilt.round() as i32,
            };
        }

        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("ptz-sdk".into())
                .stack_size(WORKER_STACK)
                .spawn(move || worker(&shared, config, ranges))
                .map_err(|source| Error::SpawnWorker { source })?
        };
        tracing::info!(pan, tilt, "PTZ ready");
        Ok(Self {
            config,
            ranges,
            shared,
            worker: Some(worker),
            presets,
        })
    }
}

impl Drop for Sdk {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.wake.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        drv::close();
        OPEN.store(false, Ordering::Release);
    }
}

/// The step loop. Runs until [`State::shutdown`].
fn worker(shared: &Shared, config: SdkConfig, ranges: [AxisRange; 2]) {
    let mut state = shared.lock();
    while !state.shutdown {
        let now = Instant::now();
        let goals = state.desired.goals(config.invert_pan, config.invert_tilt);
        for axis in [PAN, TILT] {
            match drv::busy(axis) {
                Ok(busy) => state.phase[axis].observe(busy, now, config.settle_timeout),
                Err(e) => tracing::warn!(axis, error = %e, "PTZ status poll failed"),
            }
            if !state.phase[axis].is_idle() {
                continue;
            }
            let delta = next_delta(
                state.pos[axis],
                ranges[axis],
                goals[axis],
                config.max_step_deg,
                config.goto_step_deg,
            );
            if delta == 0 {
                continue;
            }
            match drv::turn(axis, delta) {
                Ok(()) => {
                    state.pos[axis] += delta;
                    state.phase[axis] = AxisPhase::Issued {
                        at: now,
                        seen_busy: false,
                    };
                }
                Err(e) => {
                    // Do not retry every tick: drop the command.
                    tracing::warn!(axis, delta, error = %e, "PTZ step rejected; stopping");
                    state.desired = Desired::Idle;
                }
            }
        }

        let all_idle = state.phase.iter().all(AxisPhase::is_idle);
        if let Desired::Target { pan, tilt } = state.desired {
            if all_idle && state.pos == [pan, tilt] {
                state.desired = Desired::Idle;
            }
        }
        let active = !all_idle || state.desired != Desired::Idle;
        state = if active {
            shared
                .wake
                .wait_timeout(state, config.poll_interval)
                .unwrap_or_else(|e| e.into_inner())
                .0
        } else {
            shared.wake.wait(state).unwrap_or_else(|e| e.into_inner())
        };
    }
}

impl PtzBackend for Sdk {
    fn name(&self) -> &'static str {
        "sdk"
    }

    fn limits(&self) -> Limits {
        self.config.limits
    }

    fn continuous_move(&self, pan: f32, tilt: f32) -> Result<()> {
        let (pan, tilt) = check_velocity(pan, tilt)?;
        self.shared.command(if pan == 0.0 && tilt == 0.0 {
            Desired::Idle
        } else {
            Desired::Velocity { pan, tilt }
        });
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        // No SDK stop exists; the step in flight (≤ max_step_deg) finishes.
        self.shared.command(Desired::Idle);
        Ok(())
    }

    fn goto(&self, pan_deg: f32, tilt_deg: f32) -> Result<()> {
        let (pan, tilt) = self.config.limits.clamp(pan_deg, tilt_deg)?;
        self.shared.command(Desired::Target {
            pan: (pan.round() as i32).clamp(self.ranges[PAN].min, self.ranges[PAN].max),
            tilt: (tilt.round() as i32).clamp(self.ranges[TILT].min, self.ranges[TILT].max),
        });
        Ok(())
    }

    fn position(&self) -> Position {
        let state = self.shared.lock();
        let goals = state
            .desired
            .goals(self.config.invert_pan, self.config.invert_tilt);
        let moving = [PAN, TILT].into_iter().any(|axis| {
            !state.phase[axis].is_idle()
                || next_delta(
                    state.pos[axis],
                    self.ranges[axis],
                    goals[axis],
                    self.config.max_step_deg,
                    self.config.goto_step_deg,
                ) != 0
        });
        Position {
            pan_deg: state.pos[PAN] as f32,
            tilt_deg: state.pos[TILT] as f32,
            moving,
        }
    }

    fn presets(&self) -> &PresetStore {
        &self.presets
    }
}
