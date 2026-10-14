//! The AV100 motion state machine: turns "latest command" into preset_motor
//! ioctls and decides when to poll. Free of threads and wall-clock time —
//! every entry point takes `now` — so host tests drive it step by step
//! against [`FakeDevice`](super::fake::FakeDevice).
//!
//! # How each command maps onto the driver (ABI doc §2.2)
//!
//! * **Continuous move**: one cmd 4 `{flag 0, dir, run_steps}` per moving
//!   axis, issued only when that axis's direction changes. The driver runs
//!   both axes in the same tick, so a diagonal is simply two cmd 4s. An axis
//!   that drops to zero speed while the other keeps going gets cmd 4
//!   `{dir, steps 0}` — safe *only* because we are in relative mode then.
//! * **Stop**: cmd 5 `{flag 0, H 0, V 0}`, which zeroes both counters and
//!   the absolute flag in one call. The vendor's cmd 4 `{steps 0}` is *not*
//!   used for a full stop: cmd 4 also forces the mode byte to relative, so
//!   during an absolute move the other axis's pending target (×10) would be
//!   re-read as a relative step count and run that far. Leaving an absolute
//!   move for a continuous one therefore goes through this stop first.
//! * **Goto**: cmd 5 `{flag 1, H, V}` with targets clamped to the borders; a
//!   target past a border makes the driver skip every tick and never finish.
//! * **Home**: cmd 7. It drives blind for `border + 4` steps and cannot be
//!   interrupted, so new commands are held (latest wins) until it finishes.
//!
//! Completion is polled with cmd 9 every `poll_interval` (≥ 50 ms) while
//! something moves, never while idle; the position (cmd 6) is read once when a
//! move ends, and otherwise only when a caller asks and the cached value is
//! older than `position_max_age`.

use super::config::{MotorSettings, PositionStore};
use super::device::{Cmd, MotorArg, MotorDevice, DIR_H_DEC, DIR_H_INC, DIR_V_DEC, DIR_V_INC};
use crate::error::{Error, Result};
use std::time::{Duration, Instant};

/// How long after a stop we keep polling for idle before giving up.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Extra time allowed beyond the computed travel/homing time.
const MOVE_MARGIN: Duration = Duration::from_secs(3);

/// A request from the trait methods, in the driver's step frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Stop,
    /// Per-axis sign (-1, 0, 1) along increasing steps.
    Velocity([i8; 2]),
    /// Absolute target in user steps (clamped again here).
    Goto([i32; 2]),
    Home,
}

/// How the start position was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Start {
    /// The already-loaded driver knew it.
    Driver([i32; 2]),
    /// Restored from the last-position file with cmd 10.
    Restored([i32; 2]),
    /// Unknown: homing was started.
    Homing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    /// Relative runs; the driver direction per axis (`None` = axis stopped).
    Continuous {
        dirs: [Option<u8>; 2],
    },
    Goto {
        target: [i32; 2],
        deadline: Instant,
    },
    /// Stop issued; waiting for the driver to report idle.
    Stopping {
        deadline: Instant,
    },
    Homing {
        blind_until: Instant,
        deadline: Instant,
    },
}

/// Tunables the machine needs from the config.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tuning {
    pub run_steps: i32,
    pub poll_interval: Duration,
    pub position_max_age: Duration,
    pub close_timeout: Duration,
}

#[derive(Debug)]
pub(crate) struct Machine {
    dev: Box<dyn MotorDevice>,
    settings: MotorSettings,
    tuning: Tuning,
    store: PositionStore,
    phase: Phase,
    /// Latest command not yet applied.
    pending: Option<Command>,
    /// Last position read from the driver, user steps.
    pos: [i32; 2],
    pos_at: Instant,
    next_poll: Instant,
    /// Directions of a continuous move that ran into the end stops. Asking
    /// for the same again (a pad held at the limit) is a no-op, instead of
    /// an ioctl, a poll and two file writes per resend.
    ended: Option<[Option<u8>; 2]>,
}

impl Machine {
    /// Initialises the driver (cmd 3, cmd 1) and establishes the position.
    ///
    /// `driver_warm`: the module was loaded before we opened it, so its
    /// position may be meaningful (see
    /// [`trust_driver_position`](super::PresetMotorConfig::trust_driver_position)).
    pub(crate) fn init(
        dev: Box<dyn MotorDevice>,
        settings: MotorSettings,
        tuning: Tuning,
        store: PositionStore,
        driver_warm: bool,
        now: Instant,
    ) -> Result<(Self, Start)> {
        let mut m = Self {
            dev,
            settings,
            tuning,
            store,
            phase: Phase::Idle,
            pending: None,
            pos: [0, 0],
            pos_at: now,
            next_poll: now,
            ended: None,
        };
        m.ioctl(
            Cmd::SetSpeed,
            MotorArg {
                load_ms: settings.load_ms,
                ..MotorArg::default()
            },
        )?;
        m.ioctl(
            Cmd::SetBorder,
            MotorArg {
                h_border: settings.border[0],
                v_border: settings.border[1],
                ..MotorArg::default()
            },
        )?;

        let driver_pos = if driver_warm {
            Some(m.read_pos()?)
        } else {
            None
        };
        let start = choose_start(driver_pos, m.store.clean(), settings.border);
        match start {
            Start::Driver(pos) => m.pos = pos,
            Start::Restored(pos) => {
                m.ioctl(
                    Cmd::SetPos,
                    MotorArg {
                        h_now_pos: pos[0],
                        v_now_pos: pos[1],
                        ..MotorArg::default()
                    },
                )?;
                m.pos = pos;
            }
            Start::Homing => m.apply(Command::Home, now),
        }
        m.pos_at = now;
        Ok((m, start))
    }

    /// Records a command; it takes effect at the next [`step`](Self::step).
    pub(crate) fn command(&mut self, cmd: Command) {
        self.pending = Some(cmd);
    }

    /// Applies the pending command and polls if due. Returns when to call
    /// again (`None`: nothing to do until the next command).
    pub(crate) fn step(&mut self, now: Instant) -> Option<Instant> {
        self.apply_pending(now);
        self.poll(now);
        // Homing may just have finished and released a held command.
        self.apply_pending(now);
        match self.phase {
            Phase::Idle => None,
            Phase::Homing { blind_until, .. } if now < blind_until => Some(blind_until),
            _ => Some(self.next_poll),
        }
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.phase == Phase::Idle
    }

    /// Position in user steps and whether a move is in progress or queued.
    /// Re-reads the driver only while moving and the cache is stale.
    pub(crate) fn position(&mut self, now: Instant) -> ([i32; 2], bool) {
        let moving = self.phase != Phase::Idle
            || matches!(
                self.pending,
                Some(Command::Velocity(_) | Command::Goto(_) | Command::Home)
            );
        // Homing does not update the driver's position until it is done.
        let homing = matches!(self.phase, Phase::Homing { .. });
        if self.phase != Phase::Idle
            && !homing
            && now.saturating_duration_since(self.pos_at) >= self.tuning.position_max_age
        {
            match self.read_pos() {
                Ok(pos) => {
                    self.pos = pos;
                    self.pos_at = now;
                }
                Err(e) => tracing::warn!(error = %e, "PTZ position read failed"),
            }
        }
        (self.pos, moving)
    }

    /// Starts an orderly shutdown: drops queued commands and stops motion.
    /// Returns how long the caller should keep stepping for idle.
    pub(crate) fn begin_shutdown(&mut self, now: Instant) -> Duration {
        self.pending = None;
        match self.phase {
            Phase::Idle => Duration::ZERO,
            // Cannot be interrupted; let it finish rather than leave the
            // coils energised mid-sweep.
            Phase::Homing { blind_until, .. } => {
                blind_until.saturating_duration_since(now) + self.tuning.close_timeout
            }
            Phase::Stopping { .. } => self.tuning.close_timeout,
            Phase::Continuous { .. } | Phase::Goto { .. } => {
                self.stop_all(now);
                self.tuning.close_timeout
            }
        }
    }

    fn apply_pending(&mut self, now: Instant) {
        if matches!(self.phase, Phase::Homing { .. }) {
            return;
        }
        if let Some(cmd) = self.pending.take() {
            self.apply(cmd, now);
        }
    }

    fn apply(&mut self, cmd: Command, now: Instant) {
        if !matches!(cmd, Command::Velocity(_)) {
            self.ended = None;
        }
        match cmd {
            Command::Stop | Command::Velocity([0, 0]) => {
                if matches!(self.phase, Phase::Continuous { .. } | Phase::Goto { .. }) {
                    self.stop_all(now);
                }
            }
            Command::Velocity(signs) => self.run(signs, now),
            Command::Goto(target) => {
                let target = self.clamp(target);
                let ok = self.motion_ioctl(
                    Cmd::RunToPos,
                    MotorArg {
                        preset_flag: 1,
                        h_preset_step: target[0],
                        v_preset_step: target[1],
                        ..MotorArg::default()
                    },
                    now,
                );
                if ok {
                    self.begin_move(
                        Phase::Goto {
                            target,
                            deadline: now + self.settings.travel_time() + MOVE_MARGIN,
                        },
                        now,
                    );
                }
            }
            Command::Home => {
                let ok = self.motion_ioctl(
                    Cmd::Home,
                    MotorArg {
                        h_border: self.settings.border[0],
                        v_border: self.settings.border[1],
                        ..MotorArg::default()
                    },
                    now,
                );
                if ok {
                    let blind_until = now + self.settings.homing_time();
                    self.store.mark_moving();
                    self.phase = Phase::Homing {
                        blind_until,
                        deadline: blind_until + MOVE_MARGIN,
                    };
                    self.next_poll = blind_until;
                }
            }
        }
    }

    /// Continuous motion: (re)issue cmd 4 only for axes whose direction changed.
    fn run(&mut self, signs: [i8; 2], now: Instant) {
        let wanted = [
            match signs[0].signum() {
                1 => Some(DIR_H_INC),
                -1 => Some(DIR_H_DEC),
                _ => None,
            },
            match signs[1].signum() {
                1 => Some(DIR_V_INC),
                -1 => Some(DIR_V_DEC),
                _ => None,
            },
        ];
        let current = match self.phase {
            Phase::Continuous { dirs } => dirs,
            Phase::Goto { .. } => {
                // cmd 4 would turn the other axis's absolute target into a
                // relative count; clear the absolute move first.
                self.stop_all(now);
                [None, None]
            }
            _ => [None, None],
        };
        if self.phase == Phase::Idle && self.ended == Some(wanted) {
            return;
        }
        self.ended = None;
        let was_running = matches!(self.phase, Phase::Continuous { .. });
        let mut issued = false;
        for axis in 0..2 {
            let (direction, steps) = match (current[axis], wanted[axis]) {
                (was, want) if was == want => continue,
                (_, Some(dir)) => (dir, self.tuning.run_steps),
                // Relative mode here, so this only stops this axis.
                (Some(dir), None) => (dir, 0),
                (None, None) => continue,
            };
            let ok = self.motion_ioctl(
                Cmd::RunSteps,
                MotorArg {
                    preset_flag: 0,
                    direction,
                    steps,
                    ..MotorArg::default()
                },
                now,
            );
            if !ok {
                return;
            }
            issued = true;
        }
        if was_running && !issued {
            // The same velocity again (pads resend while held): nothing to
            // tell the driver, and the poll cadence stays as it was.
            return;
        }
        self.begin_move(Phase::Continuous { dirs: wanted }, now);
    }

    fn begin_move(&mut self, phase: Phase, now: Instant) {
        self.store.mark_moving();
        self.phase = phase;
        self.next_poll = now + self.settle();
    }

    /// Delay before the first status poll after a motion ioctl.
    ///
    /// `run_status` is the driver's `running_H + running_V`, which only the
    /// timer tick sets; polled before the next tick, a move that was just
    /// started still reads idle. Three ticks is a comfortable margin; the
    /// poll interval normally dominates (a tick is `load_ms + 1` ms).
    fn settle(&self) -> Duration {
        let tick = Duration::from_millis(self.settings.load_ms as u64 + 1);
        self.tuning.poll_interval.max(tick * 3)
    }

    /// cmd 5 `{flag 0, 0, 0}`: both counters to zero, relative mode.
    fn stop_all(&mut self, now: Instant) {
        let result = self.ioctl(Cmd::RunToPos, MotorArg::default());
        if let Err(e) = result {
            tracing::error!(error = %e, "PTZ stop failed");
        }
        self.phase = Phase::Stopping {
            deadline: now + STOP_TIMEOUT,
        };
        self.next_poll = now + self.settle();
    }

    /// Issues a motion ioctl; on failure logs it and stops everything.
    fn motion_ioctl(&mut self, cmd: Cmd, arg: MotorArg, now: Instant) -> bool {
        match self.ioctl(cmd, arg) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(error = %e, "PTZ command rejected; stopping");
                self.stop_all(now);
                false
            }
        }
    }

    fn poll(&mut self, now: Instant) {
        let deadline = match self.phase {
            Phase::Idle => return,
            Phase::Homing { blind_until, .. } if now < blind_until => return,
            Phase::Continuous { .. } => None,
            Phase::Goto { deadline, .. }
            | Phase::Stopping { deadline }
            | Phase::Homing { deadline, .. } => Some(deadline),
        };
        if now < self.next_poll {
            return;
        }
        self.next_poll = now + self.tuning.poll_interval;
        let status = match self.ioctl(Cmd::GetStatus, MotorArg::default()) {
            Ok(arg) => arg.run_status,
            Err(e) => {
                tracing::warn!(error = %e, "PTZ status poll failed");
                -1
            }
        };
        if status == 0 {
            self.finish(now, true);
        } else if deadline.is_some_and(|d| now >= d) {
            match self.phase {
                Phase::Goto { target, .. } => {
                    tracing::warn!(?target, "PTZ goto timed out; stopping");
                    self.stop_all(now);
                }
                phase => {
                    // Leave the "moving" marker in place: the position is
                    // doubtful, and the next start should home.
                    tracing::error!(?phase, status, "PTZ motor never reported idle");
                    self.finish(now, false);
                }
            }
        }
    }

    /// The motor is idle: read where it ended up and remember it.
    fn finish(&mut self, now: Instant, save: bool) {
        self.ended = match self.phase {
            Phase::Continuous { dirs } => Some(dirs),
            _ => None,
        };
        match self.read_pos() {
            Ok(pos) => {
                if let Phase::Goto { target, .. } = self.phase {
                    if pos != target {
                        tracing::warn!(?pos, ?target, "PTZ goto ended off target");
                    }
                }
                self.pos = pos;
                self.pos_at = now;
                if save {
                    self.store.save(pos);
                }
            }
            Err(e) => tracing::warn!(error = %e, "PTZ position read failed"),
        }
        self.phase = Phase::Idle;
    }

    fn clamp(&self, target: [i32; 2]) -> [i32; 2] {
        [
            target[0].clamp(0, self.settings.border[0]),
            target[1].clamp(0, self.settings.border[1]),
        ]
    }

    fn read_pos(&mut self) -> Result<[i32; 2]> {
        let arg = self.ioctl(Cmd::GetPos, MotorArg::default())?;
        Ok([arg.h_now_pos, arg.v_now_pos])
    }

    fn ioctl(&mut self, cmd: Cmd, mut arg: MotorArg) -> Result<MotorArg> {
        self.dev
            .ioctl(cmd, &mut arg)
            .map_err(|source| Error::Motor {
                op: cmd.name(),
                source,
            })?;
        Ok(arg)
    }
}

/// Picks the start position: a warm driver's non-zero, in-range position;
/// else a cleanly saved one (clamped to the borders); else homing.
///
/// (0, 0) from the driver is ambiguous — a freshly loaded module reads the
/// same as one that just homed — so it is not trusted.
pub(crate) fn choose_start(
    driver: Option<[i32; 2]>,
    saved: Option<[i32; 2]>,
    border: [i32; 2],
) -> Start {
    let inside = |p: [i32; 2]| (0..=border[0]).contains(&p[0]) && (0..=border[1]).contains(&p[1]);
    if let Some(pos) = driver.filter(|&p| p != [0, 0] && inside(p)) {
        return Start::Driver(pos);
    }
    if let Some(pos) = saved {
        return Start::Restored([pos[0].clamp(0, border[0]), pos[1].clamp(0, border[1])]);
    }
    Start::Homing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset_motor::fake::FakeDevice;
    use crate::test_support::TempDir;
    use std::path::PathBuf;

    const POLL: Duration = Duration::from_millis(100);

    fn tuning() -> Tuning {
        Tuning {
            run_steps: 3000,
            poll_interval: POLL,
            position_max_age: Duration::from_millis(250),
            close_timeout: Duration::from_secs(2),
        }
    }

    struct Rig {
        m: Machine,
        dev: FakeDevice,
        t: Instant,
        _dir: TempDir,
        path: PathBuf,
    }

    impl Rig {
        /// A machine whose position was restored to `pos`.
        fn at(pos: [i32; 2]) -> Self {
            let dir = TempDir::new("pm-machine");
            let path = dir.path().join("ptz-position.json");
            let mut store = PositionStore::open(Some(path.clone()));
            store.save(pos);
            Self::with_store(dir, path, false)
        }

        fn with_store(dir: TempDir, path: PathBuf, warm: bool) -> Self {
            let dev = FakeDevice::default();
            let t = Instant::now();
            let store = PositionStore::open(Some(path.clone()));
            let (m, _) = Machine::init(
                Box::new(dev.clone()),
                MotorSettings::default(),
                tuning(),
                store,
                warm,
                t,
            )
            .unwrap();
            dev.driver().log.clear();
            Self {
                m,
                dev,
                t,
                _dir: dir,
                path,
            }
        }

        /// Advances time by `d`, driver time by as many user steps (50 ms
        /// each at load_ms 4), then steps the machine.
        fn run(&mut self, d: Duration) -> Option<Instant> {
            self.t += d;
            self.dev.driver().tick((d.as_millis() / 50) as u32);
            self.m.step(self.t)
        }

        fn cmds(&self) -> Vec<Cmd> {
            self.dev.driver().cmds()
        }

        fn last_arg(&self) -> MotorArg {
            self.dev.driver().log.last().unwrap().1
        }

        fn saved(&self) -> Option<[i32; 2]> {
            PositionStore::open(Some(self.path.clone())).clean()
        }
    }

    #[test]
    fn init_sets_speed_and_borders_then_restores() {
        let dir = TempDir::new("pm-init");
        let path = dir.path().join("pos.json");
        PositionStore::open(Some(path.clone())).save([85, 19]);
        let dev = FakeDevice::default();
        let t = Instant::now();
        let (mut m, start) = Machine::init(
            Box::new(dev.clone()),
            MotorSettings::default(),
            tuning(),
            PositionStore::open(Some(path)),
            false,
            t,
        )
        .unwrap();
        assert_eq!(start, Start::Restored([85, 19]));
        let d = dev.driver();
        assert_eq!(d.cmds(), vec![Cmd::SetSpeed, Cmd::SetBorder, Cmd::SetPos]);
        assert_eq!(d.log[0].1.load_ms, 4);
        assert_eq!((d.log[1].1.h_border, d.log[1].1.v_border), (175, 70));
        assert_eq!(d.now, [85, 19]);
        drop(d);
        assert!(m.is_idle());
        assert_eq!(m.step(t), None, "idle: no polling");
        assert_eq!(m.position(t), ([85, 19], false));
    }

    #[test]
    fn homes_without_a_saved_position_and_holds_commands() {
        let dir = TempDir::new("pm-home");
        let path = dir.path().join("pos.json");
        let dev = FakeDevice::default();
        let t0 = Instant::now();
        let (mut m, start) = Machine::init(
            Box::new(dev.clone()),
            MotorSettings::default(),
            tuning(),
            PositionStore::open(Some(path.clone())),
            false,
            t0,
        )
        .unwrap();
        assert_eq!(start, Start::Homing);
        assert_eq!(dev.driver().cmds().last(), Some(&Cmd::Home));
        let home = MotorSettings::default().homing_time();
        assert_eq!(m.step(t0), Some(t0 + home), "blind until homing ends");
        assert!(m.position(t0).1, "homing counts as moving");

        // A goto during homing is held, not sent.
        m.command(Command::Goto([100, 30]));
        let n = dev.driver().log.len();
        assert_eq!(m.step(t0 + Duration::from_secs(1)), Some(t0 + home));
        assert_eq!(dev.driver().log.len(), n, "no ioctl during the blind wait");

        dev.driver().tick(179);
        let t = t0 + home;
        m.step(t);
        let d = dev.driver();
        let tail: Vec<_> = d.cmds()[n..].to_vec();
        assert_eq!(tail, vec![Cmd::GetStatus, Cmd::GetPos, Cmd::RunToPos]);
        assert_eq!(d.log.last().unwrap().1.preset_flag, 1);
        drop(d);
        assert_eq!(m.position(t).0, [0, 0], "homed position read back");
    }

    #[test]
    fn warm_driver_position_wins_over_the_file() {
        assert_eq!(
            choose_start(Some([40, 10]), Some([85, 19]), [175, 70]),
            Start::Driver([40, 10])
        );
        assert_eq!(
            choose_start(Some([0, 0]), Some([85, 19]), [175, 70]),
            Start::Restored([85, 19]),
            "(0, 0) is ambiguous"
        );
        assert_eq!(
            choose_start(Some([500, 10]), None, [175, 70]),
            Start::Homing,
            "out-of-range driver value is not trusted"
        );
        assert_eq!(
            choose_start(None, Some([300, -4]), [175, 70]),
            Start::Restored([175, 0]),
            "saved position is clamped (borders may have shrunk)"
        );
        assert_eq!(choose_start(None, None, [175, 70]), Start::Homing);

        let dir = TempDir::new("pm-warm");
        let path = dir.path().join("pos.json");
        PositionStore::open(Some(path.clone())).save([85, 19]);
        let dev = FakeDevice::default();
        dev.driver().now = [40, 10];
        let (_, start) = Machine::init(
            Box::new(dev.clone()),
            MotorSettings::default(),
            tuning(),
            PositionStore::open(Some(path)),
            true,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(start, Start::Driver([40, 10]));
        assert!(!dev.driver().cmds().contains(&Cmd::SetPos));
    }

    #[test]
    fn interrupted_move_forces_homing() {
        let dir = TempDir::new("pm-dirty");
        let path = dir.path().join("pos.json");
        let mut store = PositionStore::open(Some(path.clone()));
        store.save([85, 19]);
        store.mark_moving();
        let rig = Rig::with_store(dir, path, false);
        assert!(!rig.m.is_idle(), "stale position: homing");
    }

    #[test]
    fn goto_is_clamped_polled_and_saved() {
        let mut r = Rig::at([10, 10]);
        r.m.command(Command::Goto([500, -3]));
        let wake = r.m.step(r.t);
        assert_eq!(r.cmds(), vec![Cmd::RunToPos]);
        let arg = r.last_arg();
        assert_eq!(
            (arg.preset_flag, arg.h_preset_step, arg.v_preset_step),
            (1, 175, 0),
            "out-of-range targets would hang the driver"
        );
        assert_eq!(wake, Some(r.t + POLL));
        assert_eq!(r.saved(), None, "moving marker written");

        // Not due yet: no ioctl.
        r.t += Duration::from_millis(30);
        r.m.step(r.t);
        assert_eq!(r.cmds().len(), 1);

        let mut polls = 0;
        while !r.m.is_idle() {
            r.run(POLL);
            polls += 1;
            assert!(polls < 200);
        }
        let cmds = r.cmds();
        assert_eq!(cmds.iter().filter(|c| **c == Cmd::GetStatus).count(), polls);
        assert_eq!(
            cmds.last(),
            Some(&Cmd::GetPos),
            "one position read at the end"
        );
        assert!(!r.dev.driver().stalled);
        assert_eq!(r.m.position(r.t), ([175, 0], false));
        assert_eq!(r.saved(), Some([175, 0]));
        assert_eq!(r.m.step(r.t), None, "idle again: polling stops");
    }

    #[test]
    fn continuous_issues_cmd4_once_per_direction() {
        let mut r = Rig::at([50, 30]);
        r.m.command(Command::Velocity([1, -1]));
        r.m.step(r.t);
        {
            let d = r.dev.driver();
            assert_eq!(d.cmds(), vec![Cmd::RunSteps, Cmd::RunSteps]);
            assert_eq!((d.log[0].1.direction, d.log[0].1.steps), (DIR_H_INC, 3000));
            assert_eq!((d.log[1].1.direction, d.log[1].1.steps), (DIR_V_DEC, 3000));
        }
        // Same direction again: nothing re-issued.
        r.m.command(Command::Velocity([1, -1]));
        r.run(Duration::from_millis(100));
        assert_eq!(r.cmds(), vec![Cmd::RunSteps, Cmd::RunSteps, Cmd::GetStatus]);
        // Tilt stops, pan reverses: cmd 4 steps 0 on tilt is fine in
        // relative mode.
        r.m.command(Command::Velocity([-1, 0]));
        r.run(Duration::from_millis(10));
        let d = r.dev.driver();
        let tail = &d.log[d.log.len() - 2..];
        assert_eq!((tail[0].1.direction, tail[0].1.steps), (DIR_H_DEC, 3000));
        assert_eq!((tail[1].1.direction, tail[1].1.steps), (DIR_V_DEC, 0));
        assert!(!d.hazard);
    }

    #[test]
    fn diagonal_moves_both_axes_at_once() {
        let mut r = Rig::at([50, 30]);
        r.m.command(Command::Velocity([1, 1]));
        r.m.step(r.t);
        r.run(Duration::from_millis(500)); // 10 user steps
        assert_eq!(r.dev.driver().now, [60, 40]);
    }

    #[test]
    fn continuous_ends_idle_at_the_border() {
        let mut r = Rig::at([170, 30]);
        r.m.command(Command::Velocity([1, 0]));
        r.m.step(r.t);
        r.run(Duration::from_millis(500));
        assert!(r.m.is_idle(), "pinned at the border reports idle");
        assert_eq!(r.m.position(r.t), ([175, 30], false));
        assert_eq!(r.saved(), Some([175, 30]));

        // Holding the pad against the stop: resends cost nothing...
        let n = r.cmds().len();
        r.m.command(Command::Velocity([1, 0]));
        r.run(POLL);
        assert_eq!(r.cmds().len(), n);
        // ...but backing off moves again.
        r.m.command(Command::Velocity([-1, 0]));
        r.m.step(r.t);
        assert_eq!(r.last_arg().direction, DIR_H_DEC);
    }

    #[test]
    fn stop_uses_cmd5_zero() {
        let mut r = Rig::at([50, 30]);
        r.m.command(Command::Velocity([1, 1]));
        r.m.step(r.t);
        r.run(Duration::from_millis(200));
        r.m.command(Command::Stop);
        r.m.step(r.t);
        let arg = r.last_arg();
        assert_eq!(r.cmds().last(), Some(&Cmd::RunToPos));
        assert_eq!(arg, MotorArg::default(), "flag 0, H 0, V 0");
        r.run(POLL);
        assert!(r.m.is_idle());
        assert_eq!(r.saved(), Some([54, 34]));

        // Stop while idle sends nothing.
        let n = r.cmds().len();
        r.m.command(Command::Stop);
        r.m.step(r.t);
        assert_eq!(r.cmds().len(), n);
    }

    #[test]
    fn continuous_after_goto_stops_the_absolute_move_first() {
        let mut r = Rig::at([50, 30]);
        r.m.command(Command::Goto([100, 60]));
        r.m.step(r.t);
        r.run(Duration::from_millis(100));
        r.m.command(Command::Velocity([0, -1]));
        r.m.step(r.t);
        let d = r.dev.driver();
        let n = d.log.len();
        assert_eq!(d.log[n - 2], (Cmd::RunToPos, MotorArg::default()));
        assert_eq!(d.log[n - 1].0, Cmd::RunSteps);
        assert!(!d.hazard, "no cmd 4 while an absolute target is pending");
        drop(d);
        let before = r.dev.driver().now;
        r.run(Duration::from_millis(250));
        let after = r.dev.driver().now;
        assert_eq!(after[0], before[0], "pan no longer moves");
        assert_eq!(after[1], before[1] - 5);
    }

    #[test]
    fn latest_command_wins() {
        let mut r = Rig::at([50, 30]);
        r.m.command(Command::Goto([0, 0]));
        r.m.command(Command::Velocity([1, 0]));
        r.m.command(Command::Goto([60, 35]));
        r.m.step(r.t);
        assert_eq!(r.cmds(), vec![Cmd::RunToPos], "only the last one is sent");
        let arg = r.last_arg();
        assert_eq!((arg.h_preset_step, arg.v_preset_step), (60, 35));
        // A goto supersedes a goto directly (cmd 5 overwrites both axes).
        r.run(POLL);
        r.m.command(Command::Goto([40, 20]));
        r.m.step(r.t);
        while !r.m.is_idle() {
            r.run(POLL);
        }
        assert_eq!(r.m.position(r.t).0, [40, 20]);
    }

    #[test]
    fn position_is_cached_while_moving() {
        let mut r = Rig::at([50, 30]);
        r.m.command(Command::Velocity([1, 0]));
        r.m.step(r.t);
        let n = r.cmds().len();
        let (_, moving) = r.m.position(r.t);
        assert!(moving);
        assert_eq!(r.cmds().len(), n, "fresh cache: no ioctl");
        r.t += Duration::from_millis(300);
        r.dev.driver().tick(6);
        assert_eq!(r.m.position(r.t).0, [56, 30]);
        assert_eq!(r.cmds().last(), Some(&Cmd::GetPos));
        let n = r.cmds().len();
        r.m.position(r.t + Duration::from_millis(100));
        assert_eq!(r.cmds().len(), n, "rate limited");
    }

    #[test]
    fn home_command_rehomes() {
        let mut r = Rig::at([50, 30]);
        r.m.command(Command::Home);
        let wake = r.m.step(r.t);
        assert_eq!(r.cmds(), vec![Cmd::Home]);
        let arg = r.last_arg();
        assert_eq!((arg.h_border, arg.v_border), (175, 70));
        assert_eq!(wake, Some(r.t + MotorSettings::default().homing_time()));
        r.run(MotorSettings::default().homing_time());
        assert!(r.m.is_idle());
        assert_eq!(r.saved(), Some([0, 0]));
    }

    #[test]
    fn failed_ioctl_stops() {
        let mut r = Rig::at([50, 30]);
        r.dev.driver().fail = Some(Cmd::RunSteps);
        r.m.command(Command::Velocity([1, 0]));
        r.m.step(r.t);
        assert_eq!(r.cmds(), vec![Cmd::RunSteps, Cmd::RunToPos]);
        r.dev.driver().fail = None;
        r.run(POLL);
        assert!(r.m.is_idle());
    }

    #[test]
    fn stuck_goto_times_out_into_a_stop() {
        let mut r = Rig::at([50, 30]);
        r.m.command(Command::Goto([60, 30]));
        r.m.step(r.t);
        // A driver that never finishes (no ticks, axis stays "running").
        r.dev.driver().running = [true, false];
        let limit = MotorSettings::default().travel_time() + MOVE_MARGIN;
        r.t += limit;
        r.m.step(r.t);
        assert_eq!(r.last_arg(), MotorArg::default(), "timeout sends a stop");
    }

    #[test]
    fn shutdown_stops_and_waits() {
        let mut r = Rig::at([50, 30]);
        r.m.command(Command::Velocity([1, 0]));
        r.m.step(r.t);
        r.m.command(Command::Goto([0, 0]));
        assert_eq!(r.m.begin_shutdown(r.t), Duration::from_secs(2));
        assert_eq!(r.last_arg(), MotorArg::default());
        r.run(POLL);
        assert!(r.m.is_idle());
        let stops = r.cmds().iter().filter(|c| **c == Cmd::RunToPos).count();
        assert_eq!(stops, 1, "the queued goto was dropped");
    }
}
