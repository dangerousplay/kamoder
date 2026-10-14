//! What each light should be, turned into GPIO levels in a safe order.
//!
//! [`Lights`] holds the state last applied and the GPIO backend. It is
//! synchronous and knows nothing about sockets or files; [`crate::owner`]
//! puts it behind a lock and adds persistence and blinking.

use crate::gpio::{Error, Gpio, Line};
use lights_proto::{Alarm, Mode, Roles, State};
use metrics::{counter, gauge};
use std::time::Duration;

/// The camera's lights and speaker amp, driven through `gpio`.
///
/// Only the lines the backend has ([`Gpio::supported`]) are ever written;
/// the others keep their default in the state (off, muted, day) and the
/// owner refuses requests for them.
///
/// Dropping it applies the safe state (day, every light off, amp muted),
/// unless [`Lights::safe_state`] already did.
pub struct Lights {
    gpio: Box<dyn Gpio + Send>,
    state: State,
    /// Blink phase: whether the alarm lights are lit right now.
    alarm_lit: bool,
    /// The safe state is applied and nothing has changed since.
    safe: bool,
}

impl Lights {
    /// Takes over the lines and applies `state`: amp first (so nothing can
    /// pop), then the alarm lights (lit at boot until driven), white, and
    /// last the day/night pair in its safe order. If a write fails, the lines
    /// already taken are put in the safe state before the error is returned.
    pub fn start(gpio: Box<dyn Gpio + Send>, state: State) -> Result<Self, Error> {
        let supported = gpio.supported();
        let state = restrict(state, supported);
        let mut lights = Self {
            gpio,
            state: State {
                supported,
                ..State::default()
            },
            alarm_lit: false,
            safe: false,
        };
        lights.drive(Line::Amp, !state.muted)?;
        lights.state.muted = state.muted;
        lights.set_alarm(state.alarm)?;
        lights.set_white(state.white)?;
        lights.switch(state.mode)?;
        lights.state.mode = state.mode;
        lights.publish();
        Ok(lights)
    }

    /// The state last applied.
    pub fn state(&self) -> State {
        self.state
    }

    /// Switches day/night. Both lines are written even when the mode does not
    /// change, which repairs a pin someone else moved.
    pub fn set_mode(&mut self, mode: Mode) -> Result<(), Error> {
        self.safe = false;
        self.switch(mode)?;
        if self.state.mode != mode {
            counter!("kamoder_lights_mode_transitions_total", "to" => mode.as_str()).increment(1);
            tracing::info!(
                from = self.state.mode.as_str(),
                to = mode.as_str(),
                "mode switched"
            );
            self.state.mode = mode;
        }
        self.publish();
        Ok(())
    }

    /// White light on or off.
    pub fn set_white(&mut self, on: bool) -> Result<(), Error> {
        self.safe = false;
        self.drive(Line::White, on)?;
        self.state.white = on;
        self.publish();
        Ok(())
    }

    /// Red/blue lights. A blink starts lit; [`Lights::blink_step`] toggles it.
    pub fn set_alarm(&mut self, alarm: Alarm) -> Result<(), Error> {
        self.safe = false;
        let lit = alarm != Alarm::Off;
        self.drive(Line::Alarm, lit)?;
        self.alarm_lit = lit;
        self.state.alarm = alarm;
        self.publish();
        Ok(())
    }

    /// Speaker amp: `true` mutes (turns the amp line off).
    pub fn set_muted(&mut self, muted: bool) -> Result<(), Error> {
        self.safe = false;
        self.drive(Line::Amp, !muted)?;
        self.state.muted = muted;
        self.publish();
        Ok(())
    }

    /// Half the blink period while the alarm blinks: how long each phase lasts.
    pub fn blink_half_period(&self) -> Option<Duration> {
        match self.state.alarm {
            Alarm::Blink { period_ms } => {
                Some(Duration::from_millis((period_ms / 2).max(1).into()))
            }
            _ => None,
        }
    }

    /// Ends a blink phase: toggles the alarm lights if the alarm blinks, and
    /// does nothing otherwise.
    pub fn blink_step(&mut self) -> Result<(), Error> {
        if matches!(self.state.alarm, Alarm::Blink { .. }) {
            self.drive(Line::Alarm, !self.alarm_lit)?;
            self.alarm_lit = !self.alarm_lit;
        }
        Ok(())
    }

    /// Day, every light off, amp muted — what the camera is left in when this
    /// service stops. Every line is attempted even if one fails; the first
    /// failure is returned.
    pub fn safe_state(&mut self) -> Result<(), Error> {
        let mut first = None;
        let mut attempt = |result: Result<(), Error>| {
            if let Err(error) = result {
                first.get_or_insert(error);
            }
        };
        attempt(self.drive(Line::Amp, false));
        attempt(self.drive(Line::Alarm, false));
        attempt(self.drive(Line::White, false));
        attempt(self.switch(Mode::Day));
        self.state = State {
            supported: self.state.supported,
            ..State::default()
        };
        self.alarm_lit = false;
        self.publish();
        match first {
            Some(error) => Err(error),
            None => {
                self.safe = true;
                Ok(())
            }
        }
    }

    /// Writes the day/night pair in the order that never shows IR light
    /// through the filter: to night the filter leaves before the LEDs come
    /// on, to day the LEDs go off before the filter returns.
    fn switch(&mut self, mode: Mode) -> Result<(), Error> {
        match mode {
            Mode::Night => {
                self.drive(Line::IrCut, true)?;
                self.drive(Line::IrLeds, true)
            }
            Mode::Day => {
                // On a failed LED write the filter stays where it was: better
                // than tinting an image the LEDs still light.
                self.drive(Line::IrLeds, false)?;
                self.drive(Line::IrCut, false)
            }
        }
    }

    /// Turns `line` on or off. A line this board lacks is skipped: the
    /// owner refuses requests that need only such lines, and day/night on a
    /// board with just one of the IR pair moves the one it has.
    fn drive(&mut self, line: Line, on: bool) -> Result<(), Error> {
        if !self.state.supported.contains(line) {
            return Ok(());
        }
        let result = self.gpio.set(line, on);
        if let Err(error) = &result {
            counter!("kamoder_lights_gpio_errors_total", "line" => line.as_str()).increment(1);
            tracing::warn!(line = line.as_str(), on, %error, "GPIO write failed");
        }
        result
    }

    fn publish(&self) {
        let state = self.state;
        gauge!("kamoder_lights_night").set(f64::from(u8::from(state.mode == Mode::Night)));
        gauge!("kamoder_lights_white_on").set(f64::from(u8::from(state.white)));
        let alarm = match state.alarm {
            Alarm::Off => 0.0,
            Alarm::On => 1.0,
            Alarm::Blink { .. } => 2.0,
        };
        gauge!("kamoder_lights_alarm").set(alarm);
        gauge!("kamoder_lights_amp_muted").set(f64::from(u8::from(state.muted)));
    }
}

/// `state` with everything the board lacks at its default: a light that is
/// not there is off, an amp that is not there is muted, and without either
/// IR line the camera is in day mode.
fn restrict(mut state: State, supported: Roles) -> State {
    if !supported.contains(Line::White) {
        state.white = false;
    }
    if !supported.contains(Line::Alarm) {
        state.alarm = Alarm::Off;
    }
    if !supported.contains(Line::Amp) {
        state.muted = true;
    }
    if !supported.contains(Line::IrCut) && !supported.contains(Line::IrLeds) {
        state.mode = Mode::Day;
    }
    state.supported = supported;
    state
}

impl Drop for Lights {
    fn drop(&mut self) {
        if !self.safe {
            if let Err(error) = self.safe_state() {
                tracing::error!(%error, "cannot apply the safe state");
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gpio::tests::{av100, av100_numbers, av100_sysfs, fake_tree};
    use crate::gpio::Sysfs;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// The writes a [`Recorder`] saw, in order.
    pub(crate) type Log = Arc<Mutex<Vec<(Line, bool)>>>;

    /// Records every write, then passes it on to a real [`Sysfs`] over a fake
    /// tree, or fails lines listed in `fail`.
    pub(crate) struct Recorder {
        pub(crate) inner: Sysfs,
        pub(crate) log: Log,
        pub(crate) fail: Arc<Mutex<Vec<Line>>>,
    }

    impl Gpio for Recorder {
        fn set(&mut self, line: Line, on: bool) -> Result<(), Error> {
            if self.fail.lock().unwrap().contains(&line) {
                return Err(Error::NotExported {
                    gpio: 0,
                    root: self.inner.root().to_path_buf(),
                });
            }
            self.log.lock().unwrap().push((line, on));
            self.inner.set(line, on)
        }

        fn supported(&self) -> Roles {
            self.inner.supported()
        }
    }

    pub(crate) struct Rig {
        pub(crate) log: Log,
        pub(crate) fail: Arc<Mutex<Vec<Line>>>,
        pub(crate) root: std::path::PathBuf,
    }

    impl Rig {
        /// The AV100 profile over a fake tree.
        pub(crate) fn new(tag: &str) -> (Self, Box<dyn Gpio + Send>) {
            let root = fake_tree(&format!("control-{tag}"), &av100_numbers());
            let inner = av100_sysfs(&root);
            Self::with(root, inner)
        }

        /// `inner`, recorded; `root` is removed on drop.
        pub(crate) fn with(root: std::path::PathBuf, inner: Sysfs) -> (Self, Box<dyn Gpio + Send>) {
            let log = Log::default();
            let fail = Arc::new(Mutex::new(Vec::new()));
            let gpio = Recorder {
                inner,
                log: Arc::clone(&log),
                fail: Arc::clone(&fail),
            };
            (Self { log, fail, root }, Box::new(gpio))
        }

        pub(crate) fn take(&self) -> Vec<(Line, bool)> {
            std::mem::take(&mut *self.log.lock().unwrap())
        }

        /// The last level written to every line.
        pub(crate) fn levels(&self) -> HashMap<Line, bool> {
            self.log.lock().unwrap().iter().copied().collect()
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    use Line::*;

    #[test]
    fn start_applies_the_defaults_in_order() {
        let (rig, gpio) = Rig::new("defaults");
        let lights = Lights::start(gpio, State::default()).unwrap();
        assert_eq!(lights.state(), State::default());
        assert_eq!(
            rig.take(),
            [
                (Amp, false),
                (Alarm, false),
                (White, false),
                (IrLeds, false),
                (IrCut, false)
            ]
        );
        // The physical levels the live UAT settled on: every light low, the
        // active-low amp high (muted).
        for line in av100().lights.unwrap().lines {
            let n = line.gpio.unwrap();
            let expected = if line.role == board::Role::Amp {
                "high"
            } else {
                "low"
            };
            let direction = crate::gpio::tests::read(&rig.root, n, "direction");
            assert_eq!(direction, expected, "{:?}", line.role);
        }
        drop(lights);
    }

    #[test]
    fn day_night_switch_in_the_safe_order() {
        let (rig, gpio) = Rig::new("order");
        let mut lights = Lights::start(gpio, State::default()).unwrap();
        rig.take();

        lights.set_mode(Mode::Night).unwrap();
        assert_eq!(rig.take(), [(IrCut, true), (IrLeds, true)]);
        assert_eq!(lights.state().mode, Mode::Night);

        lights.set_mode(Mode::Day).unwrap();
        assert_eq!(rig.take(), [(IrLeds, false), (IrCut, false)]);
        assert_eq!(lights.state().mode, Mode::Day);

        // Starting straight into night uses the same order.
        drop(lights);
        let (rig, gpio) = Rig::new("order-start");
        let night = State {
            mode: Mode::Night,
            ..State::default()
        };
        let _lights = Lights::start(gpio, night).unwrap();
        assert_eq!(&rig.take()[3..], [(IrCut, true), (IrLeds, true)]);
    }

    #[test]
    fn a_failed_led_write_leaves_the_filter_alone() {
        let (rig, gpio) = Rig::new("led-fail");
        let mut lights = Lights::start(gpio, State::default()).unwrap();
        lights.set_mode(Mode::Night).unwrap();
        rig.take();
        rig.fail.lock().unwrap().push(IrLeds);
        assert!(lights.set_mode(Mode::Day).is_err());
        assert_eq!(rig.take(), []);
        assert_eq!(lights.state().mode, Mode::Night);
        rig.fail.lock().unwrap().clear();
    }

    #[test]
    fn white_alarm_and_mute() {
        let (rig, gpio) = Rig::new("setters");
        let mut lights = Lights::start(gpio, State::default()).unwrap();
        rig.take();
        lights.set_white(true).unwrap();
        lights.set_alarm(lights_proto::Alarm::On).unwrap();
        lights.set_muted(false).unwrap();
        assert_eq!(rig.take(), [(White, true), (Alarm, true), (Amp, true)]);
        let state = lights.state();
        assert!(state.white && !state.muted);
        assert_eq!(state.alarm, lights_proto::Alarm::On);
    }

    #[test]
    fn blink_toggles_only_while_blinking() {
        let (rig, gpio) = Rig::new("blink");
        let mut lights = Lights::start(gpio, State::default()).unwrap();
        rig.take();
        assert_eq!(lights.blink_half_period(), None);
        lights.blink_step().unwrap();
        assert_eq!(rig.take(), []);

        lights
            .set_alarm(lights_proto::Alarm::Blink { period_ms: 600 })
            .unwrap();
        assert_eq!(lights.blink_half_period(), Some(Duration::from_millis(300)));
        for _ in 0..3 {
            lights.blink_step().unwrap();
        }
        assert_eq!(
            rig.take(),
            [(Alarm, true), (Alarm, false), (Alarm, true), (Alarm, false)]
        );
        // Off ends the blink dark.
        lights.set_alarm(lights_proto::Alarm::Off).unwrap();
        lights.blink_step().unwrap();
        assert_eq!(rig.take(), [(Alarm, false)]);
    }

    #[test]
    fn safe_state_on_drop() {
        let (rig, gpio) = Rig::new("drop");
        let mut lights = Lights::start(gpio, State::default()).unwrap();
        lights.set_mode(Mode::Night).unwrap();
        lights.set_white(true).unwrap();
        lights.set_alarm(lights_proto::Alarm::On).unwrap();
        lights.set_muted(false).unwrap();
        rig.take();
        drop(lights);
        assert_eq!(
            rig.take(),
            [
                (Amp, false),
                (Alarm, false),
                (White, false),
                (IrLeds, false),
                (IrCut, false)
            ]
        );
    }

    #[test]
    fn explicit_safe_state_is_not_repeated_on_drop() {
        let (rig, gpio) = Rig::new("safe-once");
        let mut lights = Lights::start(gpio, State::default()).unwrap();
        lights.set_white(true).unwrap();
        lights.safe_state().unwrap();
        assert_eq!(lights.state(), State::default());
        rig.take();
        drop(lights);
        assert_eq!(rig.take(), []);
    }

    #[test]
    fn safe_state_tries_every_line_despite_a_failure() {
        let (rig, gpio) = Rig::new("safe-fail");
        let mut lights = Lights::start(gpio, State::default()).unwrap();
        rig.take();
        rig.fail.lock().unwrap().push(Alarm);
        assert!(lights.safe_state().is_err());
        assert_eq!(
            rig.take(),
            [
                (Amp, false),
                (White, false),
                (IrLeds, false),
                (IrCut, false)
            ]
        );
        rig.fail.lock().unwrap().clear();
    }

    #[test]
    fn a_failed_start_still_leaves_the_safe_state() {
        let (rig, gpio) = Rig::new("start-fail");
        rig.fail.lock().unwrap().push(White);
        assert!(Lights::start(gpio, State::default()).is_err());
        let levels = rig.levels();
        assert!(!levels[&Amp]);
        assert!(!levels[&Alarm] && !levels[&IrCut] && !levels[&IrLeds]);
    }

    /// An EV-style board: only the IR pair, as `/sys/user-gpio` lines.
    pub(crate) fn ir_only(tag: &str) -> (Rig, Box<dyn Gpio + Send>, std::path::PathBuf) {
        use crate::gpio::tests::{add_named, scratch};
        use crate::gpio::{Address, Pin};
        let dir = scratch(&format!("control-{tag}"));
        let cut = add_named(&dir, "IR_CUT");
        let leds = add_named(&dir, "IR_LED");
        let pins = vec![
            Pin {
                line: IrCut,
                address: Address::Path(cut),
                active_low: false,
            },
            Pin {
                line: IrLeds,
                address: Address::Path(leds.clone()),
                active_low: true,
            },
        ];
        let inner = Sysfs::new(dir.join("no-gpio-root"), pins, None).unwrap();
        let (rig, gpio) = Rig::with(dir, inner);
        (rig, gpio, leds)
    }

    #[test]
    fn a_board_without_some_lines_never_writes_them() {
        let (rig, gpio, leds) = ir_only("ir-only");
        // A saved state from another board: white on, alarm on, unmuted.
        let saved = State {
            mode: Mode::Night,
            white: true,
            alarm: lights_proto::Alarm::On,
            muted: false,
            ..State::default()
        };
        let mut lights = Lights::start(gpio, saved).unwrap();
        let state = lights.state();
        assert_eq!(state.supported, [IrCut, IrLeds].into_iter().collect());
        assert_eq!(state.mode, Mode::Night);
        assert!(!state.white && state.muted);
        assert_eq!(state.alarm, lights_proto::Alarm::Off);
        assert_eq!(rig.take(), [(IrCut, true), (IrLeds, true)]);
        // Active-low LEDs: on = 0.
        assert_eq!(std::fs::read_to_string(leds.join("value")).unwrap(), "0");

        lights.set_mode(Mode::Day).unwrap();
        assert_eq!(rig.take(), [(IrLeds, false), (IrCut, false)]);
        lights.safe_state().unwrap();
        assert_eq!(rig.take(), [(IrLeds, false), (IrCut, false)]);
        assert_eq!(
            lights.state().supported,
            [IrCut, IrLeds].into_iter().collect()
        );
    }
}
