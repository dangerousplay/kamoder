//! AV100 backend over `/dev/preset_motor` (`presetmotor.ko`), no SDK.
//!
//! The ABI is documented in `docs/notes/PRESET-MOTOR-ABI.md`; [`machine`] explains
//! how PTZ commands map onto it. In short: two blind steppers (no limit
//! switches), software borders, one fixed speed, positions in "user steps",
//! and ioctls that never block but each log to dmesg.
//!
//! # Opening
//!
//! 1. If the device node is missing, `insmod` the module (the vendor `ipc`
//!    normally does this, and we stop `ipc`), then wait for the node.
//! 2. Open it and keep the fd for the backend's lifetime: closing it stops
//!    the step timer but leaves the coils energised in their last phase.
//! 3. cmd 3 (speed) and cmd 1 (borders), from the vendor `device.config`.
//! 4. Establish the position, cheapest trustworthy source first:
//!    * the module was already loaded and reports a non-zero in-range
//!      position (it survives our restarts, and `ipc` homes it at boot);
//!    * else the last-position file, if it was not left mid-move → cmd 10;
//!    * else home (cmd 7): ≈ 9 s of blind grinding into the (0, 0) stops, in
//!      the background — `open` does not wait for it.
//!
//! # Threading
//!
//! Like [`Sdk`](crate::Sdk): trait methods record the latest command under a
//! mutex and wake one worker thread, which issues the ioctls and polls for
//! completion. The ioctls return at once, so the mutex is never held long.
//!
//! # Units
//!
//! Reported "degrees" are user steps divided by
//! [`PresetMotorConfig::steps_per_degree_pan`]/`_tilt`. With the default 1.0
//! they **are raw steps** (pan `0..=175`, tilt `0..=70`) until the factor is
//! measured on hardware (T18). Direction polarity follows the vendor
//! `motor_bottom_dir`/`motor_top_dir`/`reset_sensor_direction` keys plus
//! `invert_*`, and affects velocities only; positions are always in the
//! driver's frame (0 = the homing stop).
//!
//! # Limitations
//!
//! * One speed: any non-zero velocity runs at the `motor_speed` rate. cmd 3
//!   restarts the shared timer and applies to both axes, so per-move speed
//!   changes are not attempted.
//! * Homing cannot be interrupted; commands issued meanwhile run after it.

mod config;
mod device;
#[cfg(test)]
mod fake;
mod machine;

pub use config::{
    PresetMotorConfig, MIN_POLL_INTERVAL, PRESET_MOTOR_DEVICE, PRESET_MOTOR_MODULE,
    VENDOR_DEVICE_CONFIG,
};

use crate::error::{Error, Result};
use crate::preset::PresetStore;
use crate::{check_velocity, Limits, Position, PtzBackend};
use config::{MotorSettings, PositionStore};
use device::MotorDevice;
use machine::{Command, Machine, Start, Tuning};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Worker stack size; the loop is shallow.
const WORKER_STACK: usize = 64 * 1024;

#[derive(Debug)]
struct Inner {
    machine: Machine,
    shutdown: bool,
}

#[derive(Debug)]
struct Shared {
    inner: Mutex<Inner>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn command(&self, cmd: Command) {
        self.lock().machine.command(cmd);
        self.wake.notify_one();
    }
}

/// AV100 pan/tilt head driven through `/dev/preset_motor`.
///
/// Dropping it stops the motor, waits (bounded) until the driver reports
/// idle, saves the position and only then closes the device.
#[derive(Debug)]
pub struct PresetMotor {
    limits: Limits,
    border: [i32; 2],
    steps_per_degree: [f32; 2],
    /// Positive velocity runs towards step 0 on this axis.
    reverse: [bool; 2],
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    presets: PresetStore,
    // Dropped last, after the worker has closed the device.
    _guard: Option<OpenGuard>,
}

impl PresetMotor {
    /// Loads the module if needed, opens `dev` and initialises the driver.
    ///
    /// Does not block on the motor: when homing is needed it runs in the
    /// background and [`position`](PtzBackend::position) reports `moving`.
    /// May block up to [`PresetMotorConfig::node_timeout`] after `insmod`.
    #[cfg(target_os = "linux")]
    pub fn open(dev: &Path, config: &PresetMotorConfig, presets: PresetStore) -> Result<Self> {
        config.validate()?;
        let settings = config.resolve()?;
        let guard = OpenGuard::acquire()?;
        let warm = dev.exists();
        if !warm {
            load_module(&config.module_path, dev, config.node_timeout)?;
        }
        let device =
            device::RealDevice::open(dev).map_err(|source| Error::Motor { op: "open", source })?;
        tracing::info!(dev = %dev.display(), warm, ?settings, "opening preset_motor");
        let mut motor = Self::with_device(
            Box::new(device),
            settings,
            config,
            presets,
            warm && config.trust_driver_position,
        )?;
        motor._guard = Some(guard);
        Ok(motor)
    }

    /// Only Linux has the driver.
    #[cfg(not(target_os = "linux"))]
    pub fn open(_dev: &Path, _config: &PresetMotorConfig, _presets: PresetStore) -> Result<Self> {
        Err(Error::unsupported(
            "preset_motor",
            "only available on Linux",
        ))
    }

    /// Builds the backend over any [`MotorDevice`] (the real one or a fake).
    fn with_device(
        device: Box<dyn MotorDevice>,
        settings: MotorSettings,
        config: &PresetMotorConfig,
        presets: PresetStore,
        warm: bool,
    ) -> Result<Self> {
        let tuning = Tuning {
            run_steps: config.run_steps,
            poll_interval: config.poll_interval,
            position_max_age: config.position_max_age,
            close_timeout: config.close_timeout,
        };
        let store = PositionStore::open(config.position_path.clone());
        let (machine, start) =
            Machine::init(device, settings, tuning, store, warm, Instant::now())?;
        match start {
            Start::Driver(pos) => {
                tracing::info!(?pos, "PTZ: position taken from the loaded driver")
            }
            Start::Restored(pos) => tracing::info!(?pos, "PTZ: position restored from file"),
            Start::Homing => tracing::info!("PTZ: position unknown; homing (≈ 9 s)"),
        }
        let steps_per_degree = [config.steps_per_degree_pan, config.steps_per_degree_tilt];
        let travel = Limits {
            pan_min_deg: 0.0,
            pan_max_deg: settings.border[0] as f32 / steps_per_degree[0],
            tilt_min_deg: 0.0,
            tilt_max_deg: settings.border[1] as f32 / steps_per_degree[1],
        };
        let limits = match config.limits {
            Some(wanted) => narrow(travel, wanted),
            None => travel,
        };
        limits.validate()?;

        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                machine,
                shutdown: false,
            }),
            wake: Condvar::new(),
        });
        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("ptz-preset".into())
                .stack_size(WORKER_STACK)
                .spawn(move || worker(&shared))
                .map_err(|source| Error::SpawnWorker { source })?
        };
        Ok(Self {
            limits,
            border: settings.border,
            steps_per_degree,
            reverse: settings.reverse,
            shared,
            worker: Some(worker),
            presets,
            _guard: None,
        })
    }

    fn to_steps(&self, axis: usize, deg: f32) -> i32 {
        ((deg * self.steps_per_degree[axis]).round() as i32).clamp(0, self.border[axis])
    }

    fn to_deg(&self, axis: usize, steps: i32) -> f32 {
        steps as f32 / self.steps_per_degree[axis]
    }
}

/// The software `limits` cut down to the driver's `travel`: the borders are
/// the hardware truth, a profile can only restrict them.
fn narrow(travel: Limits, wanted: Limits) -> Limits {
    let limits = Limits {
        pan_min_deg: wanted.pan_min_deg.max(travel.pan_min_deg),
        pan_max_deg: wanted.pan_max_deg.min(travel.pan_max_deg),
        tilt_min_deg: wanted.tilt_min_deg.max(travel.tilt_min_deg),
        tilt_max_deg: wanted.tilt_max_deg.min(travel.tilt_max_deg),
    };
    if limits != wanted {
        tracing::warn!(
            ?wanted,
            ?travel,
            ?limits,
            "PTZ limits exceed the motor travel; narrowed"
        );
    }
    limits
}

impl Drop for PresetMotor {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.wake.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // The device (fd) closes when the last Arc<Shared> goes, right here.
    }
}

/// Steps the machine until shutdown, sleeping until its next deadline.
fn worker(shared: &Shared) {
    let mut inner = shared.lock();
    let mut stop_by: Option<Instant> = None;
    loop {
        let now = Instant::now();
        if inner.shutdown && stop_by.is_none() {
            stop_by = Some(now + inner.machine.begin_shutdown(now));
        }
        let wake = inner.machine.step(now);
        if let Some(stop_by) = stop_by {
            if inner.machine.is_idle() {
                break;
            }
            if now >= stop_by {
                tracing::error!("PTZ motor still busy at shutdown; closing anyway");
                break;
            }
        }
        let wake = match (wake, stop_by) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        inner = match wake {
            Some(at) => {
                let timeout = at
                    .saturating_duration_since(now)
                    .max(Duration::from_millis(1));
                shared
                    .wake
                    .wait_timeout(inner, timeout)
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            }
            None => shared.wake.wait(inner).unwrap_or_else(|e| e.into_inner()),
        };
    }
}

impl PtzBackend for PresetMotor {
    fn name(&self) -> &'static str {
        "preset_motor"
    }

    fn limits(&self) -> Limits {
        self.limits
    }

    fn continuous_move(&self, pan: f32, tilt: f32) -> Result<()> {
        let (pan, tilt) = check_velocity(pan, tilt)?;
        // One speed only: the sign is all the driver can use.
        let sign = |v: f32, reverse: bool| -> i8 {
            let s = if v > 0.0 {
                1
            } else if v < 0.0 {
                -1
            } else {
                0
            };
            if reverse {
                -s
            } else {
                s
            }
        };
        let signs = [sign(pan, self.reverse[0]), sign(tilt, self.reverse[1])];
        self.shared.command(if signs == [0, 0] {
            Command::Stop
        } else {
            Command::Velocity(signs)
        });
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        self.shared.command(Command::Stop);
        Ok(())
    }

    fn goto(&self, pan_deg: f32, tilt_deg: f32) -> Result<()> {
        let (pan, tilt) = self.limits.clamp(pan_deg, tilt_deg)?;
        self.shared.command(Command::Goto([
            self.to_steps(0, pan),
            self.to_steps(1, tilt),
        ]));
        Ok(())
    }

    fn position(&self) -> Position {
        let (steps, moving) = self.shared.lock().machine.position(Instant::now());
        Position {
            pan_deg: self.to_deg(0, steps[0]),
            tilt_deg: self.to_deg(1, steps[1]),
            moving,
        }
    }

    fn presets(&self) -> &PresetStore {
        &self.presets
    }

    /// Re-references against the (0, 0) hard stops: ≈ 9 s of blind travel
    /// that cannot be interrupted. Commands issued meanwhile run afterwards.
    fn home(&self) -> Result<()> {
        self.shared.command(Command::Home);
        Ok(())
    }
}

/// The driver state is global and unlocked; one owner per process.
#[derive(Debug)]
struct OpenGuard;

static OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl OpenGuard {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn acquire() -> Result<Self> {
        use std::sync::atomic::Ordering;
        OPEN.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| OpenGuard)
            .map_err(|_| Error::AlreadyOpen)
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        OPEN.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// `insmod module`, then waits up to `timeout` for `dev` to appear.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn load_module(module: &Path, dev: &Path, timeout: Duration) -> Result<()> {
    let fail = |reason: String| Error::LoadModule {
        module: module.to_owned(),
        reason,
    };
    if !module.exists() {
        return Err(fail("module file not found".into()));
    }
    tracing::info!(module = %module.display(), "loading preset_motor module");
    match std::process::Command::new("insmod").arg(module).status() {
        // "File exists" (already loaded) also fails; the node check decides.
        Ok(status) if !status.success() => {
            tracing::warn!(%status, "insmod failed; waiting for the node anyway")
        }
        Ok(_) => {}
        Err(e) => return Err(fail(format!("cannot run insmod: {e}"))),
    }
    let deadline = Instant::now() + timeout;
    while !dev.exists() {
        if Instant::now() >= deadline {
            // init_module returns 0 even when it gives up and deregisters.
            return Err(fail(format!(
                "{} did not appear (see dmesg: \"get hw timer for ptz fail\"?)",
                dev.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::device::Cmd;
    use super::fake::FakeDevice;
    use super::*;
    use crate::test_support::TempDir;

    fn config(dir: &TempDir) -> PresetMotorConfig {
        PresetMotorConfig {
            device_config: None,
            position_path: Some(dir.path().join("ptz-position.json")),
            ..PresetMotorConfig::default()
        }
    }

    fn open(dir: &TempDir, config: &PresetMotorConfig) -> (PresetMotor, FakeDevice) {
        let dev = FakeDevice::default();
        let ptz = PresetMotor::with_device(
            Box::new(dev.clone()),
            config.resolve().unwrap(),
            config,
            PresetStore::load(dir.path().join("presets.json")).unwrap(),
            false,
        )
        .unwrap();
        (ptz, dev)
    }

    /// Lets the driver run `steps` user steps and the worker poll.
    fn run(dev: &FakeDevice, steps: u32) {
        dev.driver().tick(steps);
        std::thread::sleep(Duration::from_millis(250));
    }

    #[test]
    fn end_to_end_with_worker_thread() {
        let dir = TempDir::new("pm-backend");
        std::fs::write(
            dir.path().join("ptz-position.json"),
            br#"{"version":1,"pan_steps":85,"tilt_steps":19,"moving":false}"#,
        )
        .unwrap();
        let (ptz, dev) = open(&dir, &config(&dir));
        assert_eq!(ptz.name(), "preset_motor");
        assert_eq!(
            ptz.limits(),
            Limits {
                pan_min_deg: 0.0,
                pan_max_deg: 175.0,
                tilt_min_deg: 0.0,
                tilt_max_deg: 70.0
            },
            "raw steps until measured"
        );
        let p = ptz.position();
        assert_eq!((p.pan_deg, p.tilt_deg, p.moving), (85.0, 19.0, false));

        ptz.goto(1000.0, 10.0).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(ptz.position().moving);
        run(&dev, 200);
        let p = ptz.position();
        assert_eq!((p.pan_deg, p.tilt_deg, p.moving), (175.0, 10.0, false));

        // Preset at the current spot, then away and back.
        let preset = ptz.set_preset("Door", None).unwrap();
        ptz.goto(0.0, 0.0).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        run(&dev, 200);
        ptz.goto_preset(&preset.token).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        run(&dev, 200);
        assert_eq!(dev.driver().now, [175, 10]);

        // Default polarity: pan + is up the steps, tilt + is down (top_dir 1).
        ptz.continuous_move(0.3, 0.3).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let dirs: Vec<u8> = dev
            .driver()
            .log
            .iter()
            .rev()
            .take(2)
            .map(|(_, a)| a.direction)
            .collect();
        assert!(dirs.contains(&device::DIR_V_DEC), "{dirs:?}");
        ptz.stop().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            dev.driver().log.last().unwrap(),
            &(Cmd::RunToPos, device::MotorArg::default())
        );
        run(&dev, 5);

        drop(ptz);
        let saved = std::fs::read_to_string(dir.path().join("ptz-position.json")).unwrap();
        assert!(saved.contains("\"moving\":false"), "{saved}");
    }

    #[test]
    fn drop_stops_a_moving_head() {
        let dir = TempDir::new("pm-drop");
        let mut cfg = config(&dir);
        cfg.close_timeout = Duration::from_millis(400);
        std::fs::write(
            dir.path().join("ptz-position.json"),
            br#"{"version":1,"pan_steps":50,"tilt_steps":30,"moving":false}"#,
        )
        .unwrap();
        let (ptz, dev) = open(&dir, &cfg);
        ptz.continuous_move(1.0, 0.0).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        dev.driver().tick(3);
        let ticker = {
            let dev = dev.clone();
            std::thread::spawn(move || {
                for _ in 0..10 {
                    std::thread::sleep(Duration::from_millis(50));
                    dev.driver().tick(1);
                }
            })
        };
        drop(ptz);
        ticker.join().unwrap();
        let d = dev.driver();
        assert!(d
            .log
            .contains(&(Cmd::RunToPos, device::MotorArg::default())));
        assert_eq!(d.status(), 0);
    }

    #[test]
    fn inverted_axes_and_step_scale() {
        let dir = TempDir::new("pm-scale");
        let cfg = PresetMotorConfig {
            steps_per_degree_pan: 0.5,
            steps_per_degree_tilt: 2.0,
            invert_tilt: true,
            ..config(&dir)
        };
        std::fs::write(
            dir.path().join("ptz-position.json"),
            br#"{"version":1,"pan_steps":10,"tilt_steps":10,"moving":false}"#,
        )
        .unwrap();
        let (ptz, dev) = open(&dir, &cfg);
        assert_eq!(ptz.limits().pan_max_deg, 350.0);
        assert_eq!(ptz.limits().tilt_max_deg, 35.0);
        assert_eq!(ptz.position().pan_deg, 20.0);
        ptz.goto(100.0, 20.0).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let arg = dev.driver().log.last().unwrap().1;
        assert_eq!((arg.h_preset_step, arg.v_preset_step), (50, 40));
        run(&dev, 100);
        ptz.continuous_move(0.0, 1.0).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            dev.driver().log.last().unwrap().1.direction,
            device::DIR_V_INC,
            "top_dir 1 flipped back by invert_tilt"
        );
    }

    #[test]
    fn configured_limits_are_narrowed_to_the_travel() {
        let dir = TempDir::new("pm-limits");
        std::fs::write(
            dir.path().join("ptz-position.json"),
            br#"{"version":1,"pan_steps":10,"tilt_steps":10,"moving":false}"#,
        )
        .unwrap();
        let cfg = PresetMotorConfig {
            limits: Some(Limits {
                pan_min_deg: 20.0,
                pan_max_deg: 150.0,
                tilt_min_deg: -10.0,
                tilt_max_deg: 500.0,
            }),
            ..config(&dir)
        };
        let (ptz, dev) = open(&dir, &cfg);
        assert_eq!(
            ptz.limits(),
            Limits {
                pan_min_deg: 20.0,
                pan_max_deg: 150.0,
                tilt_min_deg: 0.0,
                tilt_max_deg: 70.0
            }
        );
        ptz.goto(170.0, 0.0).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let arg = dev.driver().log.last().unwrap().1;
        assert_eq!((arg.h_preset_step, arg.v_preset_step), (150, 0));
    }

    #[test]
    fn home_is_supported() {
        let dir = TempDir::new("pm-home-api");
        std::fs::write(
            dir.path().join("ptz-position.json"),
            br#"{"version":1,"pan_steps":10,"tilt_steps":10,"moving":false}"#,
        )
        .unwrap();
        // Short travel so the blind wait (and drop, which lets homing
        // finish) takes ≈ 0.7 s instead of 9.
        let cfg = PresetMotorConfig {
            border_pan_steps: Some(10),
            border_tilt_steps: Some(5),
            ..config(&dir)
        };
        let (ptz, dev) = open(&dir, &cfg);
        ptz.home().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(dev.driver().cmds().last(), Some(&Cmd::Home));
        assert!(ptz.position().moving);
        dev.driver().tick(20);
        let t = Instant::now();
        drop(ptz);
        assert!(t.elapsed() < Duration::from_secs(2));
        assert_eq!(dev.driver().cmds().last(), Some(&Cmd::GetPos));
    }

    #[test]
    fn load_module_reports_a_missing_file() {
        let dir = TempDir::new("pm-insmod");
        let err = load_module(
            &dir.path().join("presetmotor.ko"),
            &dir.path().join("preset_motor"),
            Duration::from_millis(10),
        )
        .unwrap_err();
        assert!(matches!(err, Error::LoadModule { .. }), "{err}");
    }
}
