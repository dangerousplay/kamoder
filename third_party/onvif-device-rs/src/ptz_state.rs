//! Digital PTZ state machine.
//!
//! Manages virtual pan/tilt/zoom with continuous and absolute movement,
//! preset save/restore, and position querying.  Thread-safe via interior
//! mutability (`std::sync::RwLock`) so that a single `PtzState` can be
//! shared between ONVIF handler tasks and a background tick loop.
/// Recover from a poisoned lock: a prior panic under the lock already
/// violated state consistency enough that reusing the value beats cascading panics.
fn poison<T>(p: std::sync::PoisonError<T>) -> T {
    p.into_inner()
}

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// PTZ position coordinates.
///
/// | Axis  | Range        | Direction            |
/// |-------|--------------|----------------------|
/// | `x`   | `[-1.0, 1.0]` | left → right       |
/// | `y`   | `[-1.0, 1.0]` | down → up          |
/// | `zoom`| `[ 0.0, 1.0]` | wide → tele         |
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

impl Default for Position {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            zoom: 0.0,
        }
    }
}

/// PTZ velocity vector (same ranges as [`Position`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Velocity {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

impl Default for Velocity {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            zoom: 0.0,
        }
    }
}

/// A saved PTZ position preset.
#[derive(Debug, Clone)]
pub struct Preset {
    pub token: String,
    pub name: String,
    pub position: Position,
}

// ---------------------------------------------------------------------------
// Internal movement mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum MoveMode {
    #[default]
    Idle,
    Continuous,
    Absolute {
        target: Position,
        step: u32,
    },
}

// ---------------------------------------------------------------------------
// PtzState
// ---------------------------------------------------------------------------

/// Digital PTZ state machine.
///
/// All methods are thread-safe.  Call [`tick`](Self::tick) at regular
/// intervals (e.g. 50 ms) to advance position when a movement is active.
pub struct PtzState {
    position: std::sync::RwLock<Position>,
    velocity: std::sync::RwLock<Velocity>,
    moving: std::sync::RwLock<bool>,
    presets: std::sync::RwLock<HashMap<String, Preset>>,
    mode: std::sync::RwLock<MoveMode>,
    /// Home position for GotoHomePosition (`None` until SetHomePosition).
    home: std::sync::RwLock<Option<Position>>,
    /// Configuration set through SetConfiguration (`None` = the built-in
    /// default; GetConfigurations reflects it once set).
    config: std::sync::RwLock<Option<StoredConfig>>,
}

/// The part of a PTZ configuration a virtual PTZ can honestly store:
/// the display name, the node binding, and the default movement speed
/// (issue #51).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredConfig {
    pub name: String,
    pub node_token: String,
    pub default_speed: Velocity,
}

impl PtzState {
    /// Create a new PTZ state initialized at centre (no pan/tilt, full wide).
    pub fn new() -> Self {
        Self {
            position: std::sync::RwLock::new(Position::default()),
            velocity: std::sync::RwLock::new(Velocity::default()),
            moving: std::sync::RwLock::new(false),
            presets: std::sync::RwLock::new(HashMap::new()),
            mode: std::sync::RwLock::new(MoveMode::Idle),
            home: std::sync::RwLock::new(None),
            config: std::sync::RwLock::new(None),
        }
    }

    /// Advance the state machine by `dt_ms` milliseconds.
    ///
    /// | Mode         | Behaviour                                                                 |
    /// |--------------|---------------------------------------------------------------------------|
    /// | Idle         | No-op.                                                                    |
    /// | Continuous   | `pos += vel × dt × 0.0002` (equivalent to `0.01` per 50 ms step).        |
    /// | Absolute     | Exponential easing toward target: `pos += (target − pos) × 0.15`.         |
    /// |              | Snaps to target and transitions to Idle when < 0.001 away or ≥ 20 steps.  |
    pub fn tick(&self, dt_ms: u64) {
        let mode = *self.mode.read().unwrap_or_else(poison);
        match mode {
            MoveMode::Idle => {}
            MoveMode::Continuous => self.tick_continuous(dt_ms),
            MoveMode::Absolute { target, step } => self.tick_absolute(target, step),
        }
    }

    fn tick_continuous(&self, dt_ms: u64) {
        let vel = *self.velocity.read().unwrap_or_else(poison);
        let dt = dt_ms as f64;
        let f = dt * 0.0002;
        let mut pos = self.position.write().unwrap_or_else(poison);
        pos.x = clampf(pos.x + vel.x * f, -1.0, 1.0);
        pos.y = clampf(pos.y + vel.y * f, -1.0, 1.0);
        pos.zoom = clampf(pos.zoom + vel.zoom * f, 0.0, 1.0);
    }

    fn tick_absolute(&self, target: Position, step: u32) {
        const EASE: f64 = 0.15;
        const SNAP: f64 = 0.001;
        const MAX_STEPS: u32 = 20;

        let mut pos = self.position.write().unwrap_or_else(poison);
        pos.x += (target.x - pos.x) * EASE;
        pos.y += (target.y - pos.y) * EASE;
        pos.zoom += (target.zoom - pos.zoom) * EASE;

        let near = (pos.x - target.x).abs() < SNAP
            && (pos.y - target.y).abs() < SNAP
            && (pos.zoom - target.zoom).abs() < SNAP;

        let done = near || step + 1 >= MAX_STEPS;
        if done {
            *pos = target;
            drop(pos);
            *self.velocity.write().unwrap_or_else(poison) = Velocity::default();
            *self.moving.write().unwrap_or_else(poison) = false;
            *self.mode.write().unwrap_or_else(poison) = MoveMode::Idle;
        } else {
            drop(pos);
            *self.mode.write().unwrap_or_else(poison) = MoveMode::Absolute {
                target,
                step: step + 1,
            };
        }
    }

    // -- Movement commands --------------------------------------------------

    /// Start continuous velocity-based movement.
    ///
    /// Stops any previous movement first.  Position is updated on each
    /// subsequent [`tick`](Self::tick) call.
    pub fn continuous_move(&self, vel: Velocity) {
        self.stop();
        *self.velocity.write().unwrap_or_else(poison) = vel;
        *self.moving.write().unwrap_or_else(poison) = true;
        *self.mode.write().unwrap_or_else(poison) = MoveMode::Continuous;
    }

    /// Halt all movement immediately.
    pub fn stop(&self) {
        *self.mode.write().unwrap_or_else(poison) = MoveMode::Idle;
        *self.velocity.write().unwrap_or_else(poison) = Velocity::default();
        *self.moving.write().unwrap_or_else(poison) = false;
    }

    /// Move to an absolute position with exponential easing.
    ///
    /// Stops any previous movement first.  The state machine animates toward
    /// `target` over ≈20 ticks (≈1 s at 50 ms intervals).
    pub fn absolute_move(&self, target: Position) {
        self.stop();
        let clamped = Position {
            x: clampf(target.x, -1.0, 1.0),
            y: clampf(target.y, -1.0, 1.0),
            zoom: clampf(target.zoom, 0.0, 1.0),
        };
        *self.mode.write().unwrap_or_else(poison) = MoveMode::Absolute {
            target: clamped,
            step: 0,
        };
        *self.moving.write().unwrap_or_else(poison) = true;
    }

    /// Apply relative movement immediately (no animation).
    pub fn relative_move(&self, delta: Velocity) {
        let mut pos = self.position.write().unwrap_or_else(poison);
        pos.x = clampf(pos.x + delta.x, -1.0, 1.0);
        pos.y = clampf(pos.y + delta.y, -1.0, 1.0);
        pos.zoom = clampf(pos.zoom + delta.zoom, 0.0, 1.0);
    }

    // -- Presets ------------------------------------------------------------

    /// Save current position as a named preset, auto-generating a token
    /// (e.g. `"preset-1"`).  Returns the generated token.
    pub fn save_preset(&self, name: &str) -> String {
        let pos = *self.position.read().unwrap_or_else(poison);
        let token = {
            let presets = self.presets.read().unwrap_or_else(poison);
            format!("preset-{}", presets.len() + 1)
        };
        let preset = Preset {
            token: token.clone(),
            name: name.to_string(),
            position: pos,
        };
        presets_write(&self.presets).insert(token.clone(), preset);
        token
    }

    /// Save current position with an explicit token (used by ONVIF SetPreset
    /// when the client provides a token).
    pub fn save_preset_with_token(&self, token: &str, name: &str) {
        let pos = *self.position.read().unwrap_or_else(poison);
        let preset = Preset {
            token: token.to_string(),
            name: name.to_string(),
            position: pos,
        };
        presets_write(&self.presets).insert(token.to_string(), preset);
    }

    /// Move to a saved preset position (delegates to [`absolute_move`]).
    pub fn goto_preset(&self, token: &str) -> Result<(), String> {
        let position = {
            let presets = self.presets.read().unwrap_or_else(poison);
            presets
                .get(token)
                .ok_or_else(|| format!("preset not found: {token}"))
                .map(|p| p.position)?
        };
        self.absolute_move(position);
        Ok(())
    }

    /// Remove a preset by token.
    pub fn remove_preset(&self, token: &str) -> Result<(), String> {
        let mut presets = self.presets.write().unwrap_or_else(poison);
        presets
            .remove(token)
            .ok_or_else(|| format!("preset not found: {token}"))?;
        Ok(())
    }

    // -- Queries ------------------------------------------------------------

    /// Return the current position.
    pub fn get_position(&self) -> Position {
        *self.position.read().unwrap_or_else(poison)
    }

    /// Return movement status: `"IDLE"` or `"MOVING"`.
    pub fn get_status(&self) -> &'static str {
        if *self.moving.read().unwrap_or_else(poison) {
            "MOVING"
        } else {
            "IDLE"
        }
    }

    /// Return all preset tokens.
    pub fn get_presets(&self) -> Vec<String> {
        presets_read(&self.presets).keys().cloned().collect()
    }

    /// Get a preset by token (full details).
    pub fn get_preset(&self, token: &str) -> Option<Preset> {
        presets_read(&self.presets).get(token).cloned()
    }

    /// List all presets with full details.
    /// Store the current position as the home position (SetHomePosition).
    pub fn set_home_position(&self) {
        let pos = *self.position.read().unwrap_or_else(poison);
        *self.home.write().unwrap_or_else(poison) = Some(pos);
    }

    /// The stored home position, if SetHomePosition has been called.
    #[must_use]
    pub fn home_position(&self) -> Option<Position> {
        *self.home.read().unwrap_or_else(poison)
    }

    /// Move to the stored home position (GotoHomePosition); without a
    /// stored home this targets the centre (0, 0, full wide).
    pub fn goto_home_position(&self) {
        let target = self.home_position().unwrap_or_default();
        self.absolute_move(target);
    }

    /// Store a configuration set through SetConfiguration.
    pub fn set_config(&self, cfg: StoredConfig) {
        *self.config.write().unwrap_or_else(poison) = Some(cfg);
    }

    /// The stored configuration, if SetConfiguration has been called.
    #[must_use]
    pub fn stored_config(&self) -> Option<StoredConfig> {
        self.config.read().unwrap_or_else(poison).clone()
    }

    pub fn list_presets(&self) -> Vec<Preset> {
        presets_read(&self.presets).values().cloned().collect()
    }
}

impl Default for PtzState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn clampf(v: f64, lo: f64, hi: f64) -> f64 {
    v.clamp(lo, hi)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// Locks the presets table tolerating poisoning: the table is plain data
// with no invariants, so a panicked peer thread's guard is still consistent
// to read/write. Keeps the production paths unwrap/expect-free (#15).
fn presets_write(
    lock: &std::sync::RwLock<std::collections::HashMap<String, Preset>>,
) -> std::sync::RwLockWriteGuard<'_, std::collections::HashMap<String, Preset>> {
    match lock.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn presets_read(
    lock: &std::sync::RwLock<std::collections::HashMap<String, Preset>>,
) -> std::sync::RwLockReadGuard<'_, std::collections::HashMap<String, Preset>> {
    match lock.read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- State transitions --------------------------------------------------

    #[test]
    fn test_initial_position_center() {
        let state = PtzState::new();
        let pos = state.get_position();
        assert_eq!(pos.x, 0.0);
        assert_eq!(pos.y, 0.0);
        assert_eq!(pos.zoom, 0.0);
        assert_eq!(state.get_status(), "IDLE");
    }

    #[test]
    fn test_continuous_move_advances_position() {
        let state = PtzState::new();
        state.continuous_move(Velocity {
            x: 1.0,
            y: 0.5,
            zoom: 0.2,
        });
        assert_eq!(state.get_status(), "MOVING");

        state.tick(50);
        let pos = state.get_position();
        assert!(pos.x > 0.0, "pan should advance");
        assert!(pos.y > 0.0, "tilt should advance");
        assert!(pos.zoom > 0.0, "zoom should advance");
    }

    #[test]
    fn test_stop_halts_movement() {
        let state = PtzState::new();
        state.continuous_move(Velocity {
            x: 1.0,
            y: 1.0,
            zoom: 1.0,
        });
        state.tick(50);
        state.stop();
        assert_eq!(state.get_status(), "IDLE");

        let pos1 = state.get_position();
        state.tick(50);
        let pos2 = state.get_position();
        assert_eq!(pos1, pos2, "position should not change after stop");
    }

    #[test]
    fn test_stop_continuous_and_restart() {
        let state = PtzState::new();
        state.continuous_move(Velocity {
            x: 0.5,
            y: 0.0,
            zoom: 0.0,
        });
        state.tick(100);
        state.stop();
        assert_eq!(state.get_status(), "IDLE");

        state.continuous_move(Velocity {
            x: -0.3,
            y: 0.8,
            zoom: 0.5,
        });
        state.tick(50);
        assert_eq!(state.get_status(), "MOVING");
    }

    // -- Absolute move with easing ------------------------------------------

    #[test]
    fn test_absolute_move_reaches_target() {
        let state = PtzState::new();
        state.absolute_move(Position {
            x: 0.5,
            y: -0.3,
            zoom: 0.8,
        });

        for _ in 0..25 {
            state.tick(50);
        }

        let pos = state.get_position();
        assert!((pos.x - 0.5).abs() < 0.01);
        assert!((pos.y - (-0.3)).abs() < 0.01);
        assert!((pos.zoom - 0.8).abs() < 0.01);
        assert_eq!(state.get_status(), "IDLE");
    }

    #[test]
    fn test_absolute_move_snaps_to_target() {
        let state = PtzState::new();
        state.absolute_move(Position {
            x: 1.0,
            y: 1.0,
            zoom: 1.0,
        });

        for _ in 0..22 {
            state.tick(50);
        }

        let pos = state.get_position();
        assert!((pos.x - 1.0).abs() < f64::EPSILON);
        assert!((pos.y - 1.0).abs() < f64::EPSILON);
        assert!((pos.zoom - 1.0).abs() < f64::EPSILON);
        assert_eq!(state.get_status(), "IDLE");
    }

    #[test]
    fn test_absolute_move_idle_when_target_equals_start() {
        let state = PtzState::new();
        state.absolute_move(Position {
            x: 0.0,
            y: 0.0,
            zoom: 0.0,
        });

        for _ in 0..5 {
            state.tick(50);
        }
        assert_eq!(state.get_status(), "IDLE");
    }

    // -- Relative move ------------------------------------------------------

    #[test]
    fn test_relative_move_immediate() {
        let state = PtzState::new();
        state.relative_move(Velocity {
            x: 0.3,
            y: -0.2,
            zoom: 0.1,
        });

        let pos = state.get_position();
        assert!((pos.x - 0.3).abs() < f64::EPSILON);
        assert!((pos.y - (-0.2)).abs() < f64::EPSILON);
        assert!((pos.zoom - 0.1).abs() < f64::EPSILON);
    }

    // -- Clamping -----------------------------------------------------------

    #[test]
    fn test_continuous_move_clamps_at_boundary() {
        let state = PtzState::new();
        state.continuous_move(Velocity {
            x: 1.0,
            y: -1.0,
            zoom: 1.0,
        });

        for _ in 0..1000 {
            state.tick(50);
        }

        let pos = state.get_position();
        assert!(pos.x <= 1.0, "pan ≤ 1.0 (got {})", pos.x);
        assert!(pos.y >= -1.0, "tilt ≥ -1.0 (got {})", pos.y);
        assert!(pos.zoom <= 1.0, "zoom ≤ 1.0 (got {})", pos.zoom);
        assert!((pos.x - 1.0).abs() < f64::EPSILON);
        assert!((pos.y - (-1.0)).abs() < f64::EPSILON);
        assert!((pos.zoom - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_absolute_move_clamps_target() {
        let state = PtzState::new();
        // Out-of-range target should be clamped
        state.absolute_move(Position {
            x: 2.0,
            y: -2.0,
            zoom: 2.0,
        });

        for _ in 0..25 {
            state.tick(50);
        }

        let pos = state.get_position();
        assert!((pos.x - 1.0).abs() < f64::EPSILON);
        assert!((pos.y - (-1.0)).abs() < f64::EPSILON);
        assert!((pos.zoom - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_relative_move_clamps() {
        let state = PtzState::new();
        state.relative_move(Velocity {
            x: 5.0,
            y: -5.0,
            zoom: 2.0,
        });
        let pos = state.get_position();
        assert_eq!(pos.x, 1.0);
        assert_eq!(pos.y, -1.0);
        assert_eq!(pos.zoom, 1.0);
    }

    #[test]
    fn test_negative_zoom_clamps_to_zero() {
        let state = PtzState::new();
        state.relative_move(Velocity {
            x: 0.0,
            y: 0.0,
            zoom: -0.5,
        });
        let pos = state.get_position();
        assert_eq!(pos.zoom, 0.0);
    }

    // -- Presets ------------------------------------------------------------

    #[test]
    fn test_preset_save_and_restore() {
        let state = PtzState::new();

        // Move to a specific position
        state.absolute_move(Position {
            x: 0.5,
            y: 0.3,
            zoom: 0.7,
        });
        for _ in 0..25 {
            state.tick(50);
        }

        let token = state.save_preset("home");
        assert!(token.starts_with("preset-"));

        // Verify stored position
        let preset = state.get_preset(&token).unwrap();
        assert_eq!(preset.name, "home");
        assert!((preset.position.x - 0.5).abs() < 0.01);

        // Move away
        state.absolute_move(Position::default());
        for _ in 0..25 {
            state.tick(50);
        }
        assert!((state.get_position().x - 0.0).abs() < 0.01);

        // Restore preset
        state.goto_preset(&token).unwrap();
        for _ in 0..25 {
            state.tick(50);
        }
        assert!((state.get_position().x - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_goto_missing_preset_returns_error() {
        let state = PtzState::new();
        let err = state.goto_preset("nonexistent").unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn test_remove_preset() {
        let state = PtzState::new();
        let token = state.save_preset("test");
        assert_eq!(state.get_presets().len(), 1);

        state.remove_preset(&token).unwrap();
        assert_eq!(state.get_presets().len(), 0);
    }

    #[test]
    fn test_remove_missing_preset_returns_error() {
        let state = PtzState::new();
        let err = state.remove_preset("nonexistent").unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn test_save_preset_with_explicit_token() {
        let state = PtzState::new();
        state.absolute_move(Position {
            x: -0.8,
            y: 0.6,
            zoom: 0.4,
        });
        for _ in 0..25 {
            state.tick(50);
        }

        state.save_preset_with_token("my-token", "my-name");
        let preset = state.get_preset("my-token").unwrap();
        assert_eq!(preset.name, "my-name");
        assert!((preset.position.x - (-0.8)).abs() < 0.01);
    }

    #[test]
    fn test_list_presets() {
        let state = PtzState::new();
        state.save_preset("p1");
        state.save_preset("p2");
        state.save_preset("p3");
        assert_eq!(state.list_presets().len(), 3);
        assert_eq!(state.get_presets().len(), 3);
    }

    // -- Concurrency smoke test ---------------------------------------------

    #[test]
    fn test_concurrent_read_write() {
        let state = std::sync::Arc::new(PtzState::new());
        let mut handles = Vec::new();

        // Writers: continuous move + tick
        let s = state.clone();
        handles.push(std::thread::spawn(move || {
            s.continuous_move(Velocity {
                x: 0.1,
                y: -0.1,
                zoom: 0.05,
            });
            for _ in 0..20 {
                s.tick(50);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            s.stop();
        }));

        // Readers: get_position + get_status
        for _ in 0..4 {
            let s = state.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..20 {
                    let _ = s.get_position();
                    let _ = s.get_status();
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        let pos = state.get_position();
        assert!(
            pos.x > 0.0,
            "pan should have advanced from concurrent ticks"
        );
        assert_eq!(state.get_status(), "IDLE");
    }

    #[test]
    fn test_tick_dt_scaling() {
        // Verify that a larger dt moves further
        let state_a = PtzState::new();
        let state_b = PtzState::new();

        state_a.continuous_move(Velocity {
            x: 1.0,
            y: 0.0,
            zoom: 0.0,
        });
        state_b.continuous_move(Velocity {
            x: 1.0,
            y: 0.0,
            zoom: 0.0,
        });

        state_a.tick(50); // 0.01 per unit velocity
        state_b.tick(100); // 0.02 per unit velocity

        assert!(
            state_b.get_position().x > state_a.get_position().x,
            "larger dt should advance further"
        );
    }
}
