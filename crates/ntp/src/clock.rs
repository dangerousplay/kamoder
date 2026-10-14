//! The system clock behind a trait, and the step-or-slew decision (NTP-01).
//!
//! Tests inject a fake [`Clock`]; only [`SystemClock`] ever calls
//! `clock_settime`, and no test constructs one for writing.
//!
//! Step vs slew: a step (`clock_settime(CLOCK_REALTIME)`) jumps the clock;
//! `adjtime` slews it at the kernel's 500 ppm, so 0.5 s, the largest offset
//! slewed here, is absorbed in about 17 minutes, well inside the default
//! one-hour resync. The first sync always steps: at boot the clock reads 1970
//! and slewing 56 years is not an option. Both calls exist in the camera's
//! uClibc-ng 1.0.31 and in the crosstool's 0.9.33; should `adjtime` fail at
//! run time anyway (EPERM, ENOSYS), the offset is stepped instead.

use crate::packet::NANOS;
use std::io;

/// Offsets larger than this are stepped, smaller ones slewed.
pub const STEP_THRESHOLD_NANOS: i64 = NANOS / 2;

/// The realtime clock, as far as the sync loop needs it.
pub trait Clock {
    /// Current time, Unix nanoseconds.
    fn now(&self) -> i64;
    /// Jumps the clock by `offset` nanoseconds.
    fn step(&self, offset: i64) -> io::Result<()>;
    /// Starts a gradual correction of `offset` nanoseconds.
    fn slew(&self, offset: i64) -> io::Result<()>;
}

/// How an offset is corrected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Correction {
    Step,
    Slew,
}

impl Correction {
    pub fn as_str(self) -> &'static str {
        match self {
            Correction::Step => "step",
            Correction::Slew => "slew",
        }
    }
}

/// Step on the first sync, when the local clock is before the build floor
/// (still 1970), or when the offset exceeds [`STEP_THRESHOLD_NANOS`];
/// slew otherwise.
pub fn decide(offset: i64, first: bool, now: i64, floor_unix: i64) -> Correction {
    if first
        || now.div_euclid(NANOS) < floor_unix
        || offset.unsigned_abs() > STEP_THRESHOLD_NANOS as u64
    {
        Correction::Step
    } else {
        Correction::Slew
    }
}

/// Applies `offset` the way [`decide`] says; a failed slew falls back to a
/// step. Returns what was actually done.
pub fn correct(
    clock: &dyn Clock,
    offset: i64,
    first: bool,
    floor_unix: i64,
) -> io::Result<Correction> {
    match decide(offset, first, clock.now(), floor_unix) {
        Correction::Step => clock.step(offset).map(|()| Correction::Step),
        Correction::Slew => match clock.slew(offset) {
            Ok(()) => Ok(Correction::Slew),
            Err(error) => {
                tracing::warn!(%error, "adjtime failed; stepping instead");
                clock.step(offset).map(|()| Correction::Step)
            }
        },
    }
}

/// `CLOCK_REALTIME` through libc.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

fn realtime() -> io::Result<libc::timespec> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ts)
}

/// `time_t` is 32 bits on this target: past 2038 the conversion fails rather
/// than wrapping to 1901.
fn to_time_t(secs: i64) -> io::Result<libc::time_t> {
    libc::time_t::try_from(secs).map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))
}

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        match realtime() {
            #[allow(
                clippy::useless_conversion,
                reason = "time_t is i32 on the camera, i64 on the host"
            )]
            Ok(ts) => i64::from(ts.tv_sec) * NANOS + i64::from(ts.tv_nsec),
            // CLOCK_REALTIME cannot fail on Linux; 0 makes the next sync step.
            Err(_) => 0,
        }
    }

    fn step(&self, offset: i64) -> io::Result<()> {
        // Read again right before setting, so the time spent since the
        // reply arrived is not lost.
        let target = self.now().saturating_add(offset);
        let ts = libc::timespec {
            tv_sec: to_time_t(target.div_euclid(NANOS))?,
            tv_nsec: target.rem_euclid(NANOS) as _,
        };
        // SAFETY: `ts` is a valid timespec with tv_nsec in 0..1e9; the call
        // only reads it.
        if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn slew(&self, offset: i64) -> io::Result<()> {
        let micros = offset / 1_000;
        let delta = libc::timeval {
            tv_sec: to_time_t(micros.div_euclid(1_000_000))?,
            tv_usec: micros.rem_euclid(1_000_000) as _,
        };
        // SAFETY: `delta` is a valid timeval with tv_usec in 0..1e6; a null
        // `olddelta` is allowed and means "do not report".
        if unsafe { libc::adjtime(&delta, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// A clock that records corrections instead of making them.
    #[derive(Debug, Default)]
    pub struct FakeClock {
        pub now: Cell<i64>,
        pub fail_slew: bool,
        pub log: RefCell<Vec<(Correction, i64)>>,
    }

    impl FakeClock {
        pub fn at(now: i64) -> Self {
            FakeClock {
                now: Cell::new(now),
                ..FakeClock::default()
            }
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> i64 {
            self.now.get()
        }
        fn step(&self, offset: i64) -> io::Result<()> {
            self.now.set(self.now.get() + offset);
            self.log.borrow_mut().push((Correction::Step, offset));
            Ok(())
        }
        fn slew(&self, offset: i64) -> io::Result<()> {
            if self.fail_slew {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            self.log.borrow_mut().push((Correction::Slew, offset));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeClock;
    use super::*;

    const FLOOR: i64 = crate::BUILD_FLOOR_UNIX;
    const SYNCED: i64 = (FLOOR + 1_000) * NANOS;

    #[test]
    fn first_sync_always_steps() {
        assert_eq!(decide(1_000, true, SYNCED, FLOOR), Correction::Step);
    }

    #[test]
    fn clock_before_floor_steps() {
        assert_eq!(decide(1_000, false, 0, FLOOR), Correction::Step);
    }

    #[test]
    fn small_offsets_slew_large_ones_step() {
        assert_eq!(
            decide(STEP_THRESHOLD_NANOS, false, SYNCED, FLOOR),
            Correction::Slew
        );
        assert_eq!(
            decide(-STEP_THRESHOLD_NANOS, false, SYNCED, FLOOR),
            Correction::Slew
        );
        assert_eq!(
            decide(STEP_THRESHOLD_NANOS + 1, false, SYNCED, FLOOR),
            Correction::Step
        );
        assert_eq!(
            decide(-STEP_THRESHOLD_NANOS - 1, false, SYNCED, FLOOR),
            Correction::Step
        );
    }

    #[test]
    fn correct_steps_the_fake_clock() {
        let clock = FakeClock::at(0);
        let offset = (FLOOR + 5) * NANOS;
        assert_eq!(
            correct(&clock, offset, true, FLOOR).unwrap(),
            Correction::Step
        );
        assert_eq!(clock.now(), offset);
        assert_eq!(*clock.log.borrow(), [(Correction::Step, offset)]);
    }

    #[test]
    fn failed_slew_falls_back_to_step() {
        let clock = FakeClock {
            fail_slew: true,
            ..FakeClock::at(SYNCED)
        };
        assert_eq!(
            correct(&clock, 1_000, false, FLOOR).unwrap(),
            Correction::Step
        );
        assert_eq!(*clock.log.borrow(), [(Correction::Step, 1_000)]);
    }

    #[test]
    fn slew_is_recorded() {
        let clock = FakeClock::at(SYNCED);
        assert_eq!(
            correct(&clock, -2_000, false, FLOOR).unwrap(),
            Correction::Slew
        );
        assert_eq!(clock.now(), SYNCED);
    }
}
