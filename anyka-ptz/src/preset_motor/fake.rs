//! A model of `presetmotor.ko` for host tests, in user-step units.
//!
//! It follows the ABI doc's §2.2 semantics closely enough to catch the
//! mistakes that matter: out-of-range absolute targets stall forever, cmd 4
//! during an absolute move turns the other axis's target into a relative
//! count (flagged in [`FakeDriver::hazard`]), homing defers other motion.
//! Time only passes when a test calls [`FakeDriver::tick`].

use super::device::{Cmd, MotorArg, MotorDevice, DIR_H_DEC, DIR_H_INC, DIR_V_DEC, DIR_V_INC};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Debug, Default)]
pub(crate) struct FakeDriver {
    pub border: [i32; 2],
    pub load_ms: i32,
    pub now: [i32; 2],
    /// `preSet_Flag`: counters are absolute targets.
    pub absolute: bool,
    /// `steps_H`/`steps_V`: remaining count, or target when `absolute`.
    pub steps: [i32; 2],
    pub dir: [u8; 2],
    pub running: [bool; 2],
    /// Remaining homing ticks; motion is deferred while non-zero.
    pub homing: i32,
    /// An absolute target outside the borders stalled the driver.
    pub stalled: bool,
    /// cmd 4 arrived while an absolute move was pending on the other axis.
    pub hazard: bool,
    /// Every ioctl issued, in order.
    pub log: Vec<(Cmd, MotorArg)>,
    /// Make this command fail with EPERM.
    pub fail: Option<Cmd>,
}

impl FakeDriver {
    /// Advances `n` user steps of driver time.
    pub(crate) fn tick(&mut self, n: u32) {
        for _ in 0..n {
            self.tick_once();
        }
    }

    fn tick_once(&mut self) {
        if self.homing > 0 {
            self.homing -= 1;
            return;
        }
        if self.absolute {
            if self.steps[0] > self.border[0]
                || self.steps[1] > self.border[1]
                || self.steps.iter().any(|&t| t < 0)
            {
                self.stalled = true;
                return;
            }
            for axis in 0..2 {
                let delta = (self.steps[axis] - self.now[axis]).signum();
                self.now[axis] += delta;
                self.running[axis] = delta != 0;
            }
            if self.now == self.steps {
                self.absolute = false;
                self.steps = [0, 0];
            }
            return;
        }
        for axis in 0..2 {
            if self.steps[axis] <= 0 {
                self.running[axis] = false;
                continue;
            }
            self.steps[axis] -= 1;
            let (up, down) = if axis == 0 {
                (DIR_H_INC, DIR_H_DEC)
            } else {
                (DIR_V_INC, DIR_V_DEC)
            };
            let d = self.dir[axis];
            let can = (d == up && self.now[axis] < self.border[axis])
                || (d == down && self.now[axis] > 0);
            if can {
                self.now[axis] += if d == up { 1 } else { -1 };
            }
            self.running[axis] = can;
        }
    }

    pub(crate) fn status(&self) -> i32 {
        if self.homing > 0 {
            return 2;
        }
        // Like the driver: only the tick sets `running`, so a move issued
        // since the last tick still reads idle.
        self.running.iter().filter(|r| **r).count() as i32
    }

    /// The commands issued so far, without arguments.
    pub(crate) fn cmds(&self) -> Vec<Cmd> {
        self.log.iter().map(|(c, _)| *c).collect()
    }

    fn apply(&mut self, cmd: Cmd, arg: &mut MotorArg) -> io::Result<()> {
        self.log.push((cmd, *arg));
        if self.fail == Some(cmd) {
            return Err(io::Error::from_raw_os_error(1));
        }
        match cmd {
            Cmd::SetBorder => self.border = [arg.h_border, arg.v_border],
            Cmd::SetSpeed => {
                if arg.load_ms == 0 {
                    return Err(io::Error::from_raw_os_error(1));
                }
                self.load_ms = arg.load_ms;
            }
            Cmd::RunSteps => {
                let axis = match arg.direction {
                    DIR_H_DEC | DIR_H_INC => 0,
                    DIR_V_INC | DIR_V_DEC => 1,
                    _ => return Ok(()),
                };
                if self.absolute && self.steps[1 - axis] != 0 && arg.preset_flag == 0 {
                    self.hazard = true;
                }
                self.absolute = arg.preset_flag == 1;
                self.steps[axis] = arg.steps;
                self.dir[axis] = arg.direction;
            }
            Cmd::RunToPos => {
                self.absolute = arg.preset_flag == 1;
                self.steps = [arg.h_preset_step, arg.v_preset_step];
            }
            Cmd::GetPos => {
                *arg = MotorArg {
                    h_now_pos: self.now[0],
                    v_now_pos: self.now[1],
                    ..MotorArg::default()
                }
            }
            Cmd::Home => {
                self.border = [arg.h_border, arg.v_border];
                self.now = [0, 0];
                self.homing = arg.h_border.max(arg.v_border) + 4;
            }
            Cmd::GetStatus => {
                *arg = MotorArg {
                    run_status: self.status(),
                    ..MotorArg::default()
                }
            }
            Cmd::SetPos => self.now = [arg.h_now_pos, arg.v_now_pos],
        }
        Ok(())
    }
}

/// A [`MotorDevice`] over a shared [`FakeDriver`] the test keeps a handle to.
#[derive(Debug, Clone, Default)]
pub(crate) struct FakeDevice(pub Arc<Mutex<FakeDriver>>);

impl FakeDevice {
    pub(crate) fn driver(&self) -> MutexGuard<'_, FakeDriver> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl MotorDevice for FakeDevice {
    fn ioctl(&mut self, cmd: Cmd, arg: &mut MotorArg) -> io::Result<()> {
        self.driver().apply(cmd, arg)
    }
}
