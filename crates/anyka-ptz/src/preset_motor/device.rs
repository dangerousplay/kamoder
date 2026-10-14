//! The `/dev/preset_motor` ioctl ABI, and the one-method trait the state
//! machine drives it through.
//!
//! Everything here comes from `docs/notes/PRESET-MOTOR-ABI.md` (static analysis of
//! `presetmotor.ko` and the vendor `ipc`). The points that shape this file:
//!
//! * Commands are **plain integers 1..=10**, not `_IOC`-encoded, hence nix's
//!   `*_bad!` macros with the raw number.
//! * Every command moves **the same 40-byte struct**: the driver copies exactly
//!   40 bytes in (write commands) or out (read commands).
//! * Nothing blocks: each ioctl only updates the driver's global state, which a
//!   500 Hz hrtimer then acts on. Each call also `printk`s, so callers must not
//!   poll in a tight loop.
//!
//! [`MotorDevice`] exists so the state machine can run against a fake driver
//! in host tests; the real implementation is Linux-only.

use std::fmt;
use std::io;

/// The driver's argument block (ABI doc §2.1). Values are "user steps"
/// (10 phase steps each); the driver scales by 10 on the way in and out.
#[repr(C)]
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MotorArg {
    /// 0x00: pan travel for cmd 1 (borders) and 7 (homing).
    pub h_border: i32,
    /// 0x04: tilt travel for cmd 1 and 7.
    pub v_border: i32,
    /// 0x08: cmd 3; timer period is `load_ms + 1` ms. Must be non-zero.
    pub load_ms: i32,
    /// 0x0c: cmd 4/5; 0 = relative counts, 1 = absolute targets.
    pub preset_flag: u8,
    /// 0x0d: cmd 4; one of the `DIR_*` constants.
    pub direction: u8,
    /// 0x0e: padding, kept explicit so the layout is obvious.
    pub _pad: u16,
    /// 0x10: cmd 4; relative step count, 0 stops that axis.
    pub steps: i32,
    /// 0x14: cmd 5; pan target (absolute) or count (relative).
    pub h_preset_step: i32,
    /// 0x18: cmd 5; tilt target or count.
    pub v_preset_step: i32,
    /// 0x1c: cmd 6 out / cmd 10 in.
    pub h_now_pos: i32,
    /// 0x20: cmd 6 out / cmd 10 in.
    pub v_now_pos: i32,
    /// 0x24: cmd 9 out; `running_H + running_V` (0, 1 or 2).
    pub run_status: i32,
}

// The driver copies exactly 40 bytes; any other size corrupts the stack of
// either side.
const _: () = assert!(std::mem::size_of::<MotorArg>() == 40);

/// cmd 4 directions (driver frame): pan towards 0.
pub(crate) const DIR_H_DEC: u8 = 0;
/// Pan towards `h_border`.
pub(crate) const DIR_H_INC: u8 = 1;
/// Tilt towards `v_border`.
pub(crate) const DIR_V_INC: u8 = 2;
/// Tilt towards 0.
pub(crate) const DIR_V_DEC: u8 = 3;

/// The ioctls this crate issues. cmd 2 is invalid in the driver and cmd 8
/// only dumps state to dmesg, so neither is here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Cmd {
    /// Sets the software travel: `[0, h_border] × [0, v_border]`.
    SetBorder = 1,
    /// Sets the tick period (`load_ms`) and restarts the timer.
    SetSpeed = 3,
    /// Relative run of one axis (`direction`, `steps`).
    RunSteps = 4,
    /// Both axes at once: absolute targets or relative counts.
    RunToPos = 5,
    /// Reads `h_now_pos`/`v_now_pos`.
    GetPos = 6,
    /// Blind homing into the (0, 0) hard stops.
    Home = 7,
    /// Reads `run_status`.
    GetStatus = 9,
    /// Overwrites the driver's idea of the position; moves nothing.
    SetPos = 10,
}

impl Cmd {
    /// Label for logs and errors.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Cmd::SetBorder => "set border (cmd 1)",
            Cmd::SetSpeed => "set speed (cmd 3)",
            Cmd::RunSteps => "run steps (cmd 4)",
            Cmd::RunToPos => "run to position (cmd 5)",
            Cmd::GetPos => "get position (cmd 6)",
            Cmd::Home => "home (cmd 7)",
            Cmd::GetStatus => "get run status (cmd 9)",
            Cmd::SetPos => "set position (cmd 10)",
        }
    }
}

/// Something that executes preset_motor ioctls: the real device, or a fake.
pub(crate) trait MotorDevice: Send + fmt::Debug {
    /// Issues `cmd`. Write commands read `arg`; read commands fill it in.
    fn ioctl(&mut self, cmd: Cmd, arg: &mut MotorArg) -> io::Result<()>;
}

#[cfg(target_os = "linux")]
pub(crate) use real::RealDevice;

#[cfg(target_os = "linux")]
mod real {
    use super::{Cmd, MotorArg, MotorDevice};
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    // Raw request numbers (ABI doc §2 / §4): the driver dispatches on
    // `cmd - 1` through a jump table, with no magic, size or direction bits.
    nix::ioctl_write_ptr_bad!(pm_set_border, 1, MotorArg);
    nix::ioctl_write_ptr_bad!(pm_set_speed, 3, MotorArg);
    nix::ioctl_write_ptr_bad!(pm_run_steps, 4, MotorArg);
    nix::ioctl_write_ptr_bad!(pm_run_to_pos, 5, MotorArg);
    nix::ioctl_read_bad!(pm_get_pos, 6, MotorArg);
    nix::ioctl_write_ptr_bad!(pm_home, 7, MotorArg);
    nix::ioctl_read_bad!(pm_get_status, 9, MotorArg);
    nix::ioctl_write_ptr_bad!(pm_set_pos, 10, MotorArg);

    /// An open `/dev/preset_motor`.
    ///
    /// The fd must stay open while the motor may move: the driver's `release`
    /// only stops the step timer, leaving the coils energised in whatever
    /// phase they were in. The backend therefore keeps one of these for its
    /// whole lifetime and drops it only after the motor reports idle.
    #[derive(Debug)]
    pub(crate) struct RealDevice {
        file: File,
    }

    impl RealDevice {
        /// Opens the node read-write, as `ipc` does. Opening starts the
        /// driver's step timer.
        pub(crate) fn open(path: &Path) -> io::Result<Self> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?;
            Ok(Self { file })
        }
    }

    impl MotorDevice for RealDevice {
        fn ioctl(&mut self, cmd: Cmd, arg: &mut MotorArg) -> io::Result<()> {
            let fd = self.file.as_raw_fd();
            let arg: *mut MotorArg = arg;
            // SAFETY: `fd` is an open preset_motor fd owned by `self` for the
            // duration of the call. `arg` points to a live, exclusively
            // borrowed 40-byte `#[repr(C)]` MotorArg, which is exactly what
            // every command copies in or out (asserted at compile time). No
            // command retains the pointer after returning.
            let ret = unsafe {
                match cmd {
                    Cmd::SetBorder => pm_set_border(fd, arg),
                    Cmd::SetSpeed => pm_set_speed(fd, arg),
                    Cmd::RunSteps => pm_run_steps(fd, arg),
                    Cmd::RunToPos => pm_run_to_pos(fd, arg),
                    Cmd::GetPos => pm_get_pos(fd, arg),
                    Cmd::Home => pm_home(fd, arg),
                    Cmd::GetStatus => pm_get_status(fd, arg),
                    Cmd::SetPos => pm_set_pos(fd, arg),
                }
            }
            .map_err(io::Error::from)?;
            // cmd 3 returns `ak_timer_start()`'s result; everything else 0.
            if ret == 0 {
                Ok(())
            } else {
                Err(io::Error::other(format!("driver returned {ret}")))
            }
        }
    }
}
