//! The tracking loop: motion grids in, PTZ commands out.
//!
//! ```text
//! camerad ──/tmp/kamoder-motion.sock──► detector ─► controller ─► /tmp/anyka-ptz.sock ──► onvif
//!            "0\n", then a grid per line     ▲  ptz status (≤ 20 Hz)  goto/home/stop, source=follow
//! ```
//!
//! - **One grid, one decision.** Each grid runs the detector (only while the
//!   controller is idle or tracking: during a move the grid shows the head's
//!   own motion and the detector is re-seeded afterwards anyway), fetches
//!   the PTZ status at most every [`STATUS_INTERVAL`] (the 20 Hz ceiling of
//!   `PRESET-MOTOR-ABI.md`), asks the controller and carries out its action.
//! - **Disabled costs nothing.** While the switch is off the motion socket is
//!   closed, so neither camerad nor this process encodes or parses a grid;
//!   status is still served. Turning it off stops a move of ours that is
//!   still running.
//! - **Never crash on a peer.** Either socket down makes the state
//!   `unavailable` and is retried with a capped backoff ([`Backoff`],
//!   1 s → 30 s); the detector keeps running while only the PTZ is gone.
//!   A motion socket that stays silent for [`STALE_AFTER`] (default) also counts as
//!   down.
//! - **Fixed memory.** One line buffer of [`motion::MAX_LINE`] allocated at
//!   start and reused; the detector is fixed arrays. Per grid only the JSON
//!   decode and the (one-connection-per-request) PTZ client allocate.

use crate::control::{Action, ControlConfig, Controller, Input, State as Ctl};
use crate::detect::{DetectConfig, Detector};
use crate::server::Shared;
use follow_proto::{State, Target};
use metrics::counter;
use ptz_proto::{Limits, Position, Response as PtzResponse, Source};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::time::Instant;
use video_proto::motion::{self, MotionGrid};

/// Shortest time between two PTZ status requests.
pub const STATUS_INTERVAL: Duration = Duration::from_millis(50);

/// Round-trip limit for one PTZ request; the loop waits on it.
pub const PTZ_TIMEOUT: Duration = Duration::from_secs(1);

/// A connected motion socket without a grid for this long is `unavailable`.
pub const STALE_AFTER: Duration = Duration::from_secs(5);

/// Reconnect delays after a socket failure: 1 s, doubling, capped at 30 s.
#[derive(Debug, Clone)]
pub struct Backoff {
    next: Duration,
}

impl Backoff {
    /// First retry delay.
    pub const INITIAL: Duration = Duration::from_secs(1);
    /// Longest retry delay.
    pub const MAX: Duration = Duration::from_secs(30);

    /// The delay before the next attempt; each call doubles the following one.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (delay * 2).min(Self::MAX);
        delay
    }

    /// Back to [`Self::INITIAL`] after a success.
    pub fn reset(&mut self) {
        self.next = Self::INITIAL;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            next: Self::INITIAL,
        }
    }
}

/// Detector tuning from the board's `[ptz.follow]`; the baseline speed is
/// not in the profile and keeps its default.
pub fn detect_config(follow: &board::Follow) -> DetectConfig {
    DetectConfig {
        t_on: follow.t_on,
        t_off: follow.t_off.min(follow.t_on),
        min_cells: follow.min_cells,
        decision_frames: u16::from(follow.decision_frames),
        max_fraction_permille: follow.max_fraction_permille,
        ..DetectConfig::default()
    }
}

/// Controller tuning from the board's `[ptz.follow]`; gain, minimum step and
/// axis inversion are not in the profile and keep their defaults.
pub fn control_config(follow: &board::Follow) -> ControlConfig {
    ControlConfig {
        dead_zone_x_permille: follow.dead_zone_permille[0],
        dead_zone_y_permille: follow.dead_zone_permille[1],
        max_step_deg: follow.max_step_deg,
        fov_h_deg: follow.fov_deg[0],
        fov_v_deg: follow.fov_deg[1],
        settle: Duration::from_millis(u64::from(follow.settle_ms)),
        return_after: Duration::from_secs(u64::from(follow.return_after_s)),
        manual_hold: Duration::from_secs(u64::from(follow.manual_hold_s)),
        ..ControlConfig::default()
    }
}

/// Where the loop connects and how it tracks.
#[derive(Debug, Clone)]
pub struct Settings {
    pub motion_socket: PathBuf,
    /// Board sensor index to subscribe to.
    pub camera: u32,
    pub ptz_socket: PathBuf,
    pub detect: DetectConfig,
    pub control: ControlConfig,
    /// `[pan, tilt]` to return to; `None` = the PTZ owner's home.
    pub home: Option<[f32; 2]>,
    /// A connected motion socket without a grid for this long is
    /// `unavailable`; [`STALE_AFTER`] in the service.
    pub stale_after: Duration,
}

impl Settings {
    /// Settings for `follow` with the default sockets and camera 0.
    pub fn from_board(follow: &board::Follow) -> Self {
        Self {
            motion_socket: motion::DEFAULT_MOTION_SOCKET.into(),
            camera: 0,
            ptz_socket: ptz_proto::DEFAULT_SOCKET.into(),
            detect: detect_config(follow),
            control: control_config(follow),
            home: follow.home,
            stale_after: STALE_AFTER,
        }
    }
}

/// The last PTZ status.
#[derive(Debug, Clone, Copy)]
struct PtzSnapshot {
    position: Position,
    limits: Limits,
    last_manual_ms_ago: Option<u64>,
    at: Instant,
}

/// A motion command.
#[derive(Debug, Clone, Copy)]
enum Command {
    Goto { pan_deg: f32, tilt_deg: f32 },
    Home,
    Stop,
}

impl Command {
    fn op(self) -> &'static str {
        match self {
            Command::Goto { .. } => "goto",
            Command::Home => "home",
            Command::Stop => "stop",
        }
    }
}

/// The loop's state. Create with [`Runtime::new`], drive with
/// [`Runtime::run`], and call [`Runtime::shutdown`] after dropping that
/// future.
pub struct Runtime {
    settings: Settings,
    shared: Arc<Shared>,
    enabled: watch::Receiver<bool>,
    detector: Detector,
    controller: Controller,
    ptz: ptz_proto::Client,
    /// A grid arrived within `stale_after` on the current connection.
    motion_up: bool,
    /// The last PTZ request got through.
    ptz_up: bool,
    ptz_cache: Option<PtzSnapshot>,
    last_fetch: Option<Instant>,
    /// No PTZ request before this, after a failure.
    ptz_retry_at: Option<Instant>,
    ptz_backoff: Backoff,
    motion_backoff: Backoff,
    /// The move in progress (settling/returning) was commanded by us.
    ours: bool,
    /// The detector reported a target on its last run.
    had_target: bool,
    target: Option<Target>,
    /// The current motion line; capacity fixed at start.
    line: Vec<u8>,
}

impl Runtime {
    pub fn new(settings: Settings, shared: Arc<Shared>) -> Self {
        let ptz = ptz_proto::Client::new(&settings.ptz_socket)
            .with_timeout(PTZ_TIMEOUT)
            .with_source(Source::Follow);
        Self {
            enabled: shared.subscribe(),
            detector: Detector::new(settings.detect),
            controller: Controller::new(settings.control),
            ptz,
            settings,
            shared,
            motion_up: false,
            ptz_up: false,
            ptz_cache: None,
            last_fetch: None,
            ptz_retry_at: None,
            ptz_backoff: Backoff::default(),
            motion_backoff: Backoff::default(),
            ours: false,
            had_target: false,
            target: None,
            line: Vec::with_capacity(motion::MAX_LINE + 2),
        }
    }

    /// Runs until dropped.
    pub async fn run(&mut self) {
        loop {
            let enabled = *self.enabled.borrow_and_update();
            if !enabled {
                self.disable().await;
                self.publish();
                // The sender lives in `shared`, which we hold: never closed.
                let _ = self.enabled.changed().await;
                continue;
            }
            self.publish();
            match UnixStream::connect(&self.settings.motion_socket).await {
                Ok(stream) => self.read_grids(stream).await,
                Err(error) => {
                    tracing::debug!(path = %self.settings.motion_socket.display(), %error, "motion socket unavailable");
                }
            }
            self.motion_up = false;
            self.publish();
            if !*self.enabled.borrow() {
                continue;
            }
            let delay = self.motion_backoff.next_delay();
            counter!("kamoder_follow_motion_reconnects_total").increment(1);
            tracing::debug!(delay_ms = delay.as_millis() as u64, "motion socket retry");
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.enabled.changed() => {}
            }
        }
    }

    /// Stops a move of ours that is still running; for SIGTERM/SIGINT.
    pub async fn shutdown(&mut self) {
        self.disable().await;
    }

    /// Takes the controller to `disabled`, stopping the head if it is still
    /// carrying out our command.
    async fn disable(&mut self) {
        let ours = self.ours
            && matches!(
                self.controller.state(),
                Ctl::Settling { .. } | Ctl::Returning { .. }
            );
        let mut position = Position {
            pan_deg: 0.0,
            tilt_deg: 0.0,
            moving: false,
        };
        let mut limits = Limits {
            pan_min_deg: 0.0,
            pan_max_deg: 0.0,
            tilt_min_deg: 0.0,
            tilt_max_deg: 0.0,
        };
        if ours {
            if let Some(snapshot) = self.fetch_status(Instant::now()).await {
                position = snapshot.position;
                limits = snapshot.limits;
            }
        }
        let input = Input {
            enabled: false,
            detection: None,
            position,
            limits,
            last_manual_ms_ago: None,
        };
        let action = self.controller.decide(&input, Instant::now().into_std());
        self.execute(action).await;
        self.ours = false;
        self.had_target = false;
        self.target = None;
        self.motion_up = false;
    }

    /// Subscribes and handles grids until the connection ends, the switch
    /// turns off or the stream breaks the protocol.
    async fn read_grids(&mut self, stream: UnixStream) {
        // The write half stays alive: dropping it shuts the socket down for
        // writing, which camerad takes for a client that left.
        let (read, mut write) = stream.into_split();
        let subscribe = video_proto::subscribe_line(self.settings.camera);
        if let Err(error) = write.write_all(subscribe.as_bytes()).await {
            tracing::debug!(%error, "cannot subscribe to motion grids");
            return;
        }
        tracing::info!(camera = self.settings.camera, "motion socket connected");
        self.detector.reseed();
        let mut reader = BufReader::new(read);
        self.line.clear();
        loop {
            // A cancelled read leaves the bytes it got in `line`; the cap
            // counts them.
            let room = (motion::MAX_LINE + 2).saturating_sub(self.line.len()) as u64;
            let mut limited = (&mut reader).take(room);
            let read = tokio::select! {
                read = tokio::time::timeout(self.settings.stale_after, limited.read_until(b'\n', &mut self.line)) => read,
                _ = self.enabled.changed() => {
                    if !*self.enabled.borrow() {
                        return;
                    }
                    continue;
                }
            };
            match read {
                Err(_) => {
                    if self.motion_up {
                        tracing::warn!(
                            after_ms = self.settings.stale_after.as_millis() as u64,
                            "no motion grid, tracking unavailable"
                        );
                        self.motion_up = false;
                        self.publish();
                    }
                    continue;
                }
                Ok(Err(error)) => {
                    tracing::info!(%error, "motion socket read failed");
                    return;
                }
                Ok(Ok(0)) => {
                    tracing::info!("motion socket closed");
                    return;
                }
                Ok(Ok(_)) => {}
            }
            if self.line.last() != Some(&b'\n') {
                if self.line.len() > motion::MAX_LINE {
                    tracing::warn!(len = self.line.len(), "motion line too long, reconnecting");
                    counter!("kamoder_follow_motion_errors_total").increment(1);
                    return;
                }
                // End of stream in mid-line: the next read says so.
                continue;
            }
            let grid = motion::decode_line(&self.line);
            self.line.clear();
            match grid {
                Ok(grid) => self.on_grid(&grid).await,
                Err(error) => {
                    tracing::debug!(%error, "bad motion line");
                    counter!("kamoder_follow_motion_errors_total").increment(1);
                }
            }
        }
    }

    /// One grid, one decision.
    async fn on_grid(&mut self, grid: &MotionGrid) {
        self.motion_backoff.reset();
        if !self.motion_up {
            tracing::info!(cols = grid.cols, rows = grid.rows, "motion grids arriving");
            self.motion_up = true;
        }
        let now = Instant::now();

        let detecting = matches!(
            self.controller.state(),
            Ctl::Idle | Ctl::Tracking | Ctl::Disabled
        );
        let detection = if detecting {
            let detection = match self.detector.update(
                usize::from(grid.cols),
                usize::from(grid.rows),
                &grid.cells,
            ) {
                Ok(detection) => detection,
                Err(error) => {
                    tracing::debug!(%error, "grid rejected by the detector");
                    None
                }
            };
            if detection.is_some() && !self.had_target {
                counter!("kamoder_follow_targets_total").increment(1);
            }
            self.had_target = detection.is_some();
            self.target = detection.map(|d| Target {
                x_permille: d.cx_permille,
                y_permille: d.cy_permille,
                cells: d.cells,
            });
            detection
        } else {
            None
        };

        let Some(ptz) = self.ptz_status(now).await else {
            self.publish();
            return;
        };
        let input = Input {
            enabled: true,
            detection,
            position: ptz.position,
            limits: ptz.limits,
            last_manual_ms_ago: ptz.last_manual_ms_ago,
        };
        let action = self.controller.decide(&input, now.into_std());
        self.execute(action).await;
        if !matches!(
            self.controller.state(),
            Ctl::Settling { .. } | Ctl::Returning { .. }
        ) {
            self.ours = false;
        }
        self.publish();
    }

    /// The PTZ status for a decision at `now`: cached if fresh, fetched if
    /// allowed, `None` while the PTZ is down or the rate limit holds.
    async fn ptz_status(&mut self, now: Instant) -> Option<PtzSnapshot> {
        if let Some(snapshot) = self.ptz_cache {
            if now.saturating_duration_since(snapshot.at) < STATUS_INTERVAL {
                return Some(snapshot);
            }
        }
        if self.ptz_retry_at.is_some_and(|at| now < at) {
            return None;
        }
        if self
            .last_fetch
            .is_some_and(|at| now.saturating_duration_since(at) < STATUS_INTERVAL)
        {
            return None;
        }
        self.fetch_status(now).await
    }

    /// Asks the PTZ owner for its status now.
    async fn fetch_status(&mut self, now: Instant) -> Option<PtzSnapshot> {
        self.last_fetch = Some(now);
        match self.ptz.status().await {
            Ok(PtzResponse::Status {
                limits,
                position,
                last_manual_ms_ago,
                ..
            }) => {
                self.ptz_ok();
                let snapshot = PtzSnapshot {
                    position,
                    limits,
                    last_manual_ms_ago,
                    at: now,
                };
                self.ptz_cache = Some(snapshot);
                Some(snapshot)
            }
            Ok(other) => {
                self.ptz_failed(now, &format!("unexpected status response {other:?}"));
                None
            }
            Err(error) => {
                self.ptz_failed(now, &error.to_string());
                None
            }
        }
    }

    fn ptz_ok(&mut self) {
        if !self.ptz_up {
            tracing::info!(path = %self.settings.ptz_socket.display(), "ptz socket answering");
        }
        self.ptz_up = true;
        self.ptz_retry_at = None;
        self.ptz_backoff.reset();
    }

    fn ptz_failed(&mut self, now: Instant, error: &str) {
        counter!("kamoder_follow_ptz_errors_total").increment(1);
        let delay = self.ptz_backoff.next_delay();
        if self.ptz_up {
            tracing::warn!(path = %self.settings.ptz_socket.display(), error, "ptz socket failed, tracking unavailable");
        } else {
            tracing::debug!(
                error,
                delay_ms = delay.as_millis() as u64,
                "ptz socket still unavailable"
            );
        }
        self.ptz_up = false;
        self.ptz_cache = None;
        self.ptz_retry_at = Some(now + delay);
    }

    async fn execute(&mut self, action: Action) {
        let command = match action {
            Action::None => return,
            Action::Reseed => {
                self.detector.reseed();
                return;
            }
            Action::Goto { pan_deg, tilt_deg } => Command::Goto { pan_deg, tilt_deg },
            Action::GoHome => match self.settings.home {
                Some([pan_deg, tilt_deg]) => Command::Goto { pan_deg, tilt_deg },
                None => Command::Home,
            },
            Action::Stop => Command::Stop,
        };
        tracing::debug!(?command, "ptz command");
        let result = match command {
            Command::Goto { pan_deg, tilt_deg } => self.ptz.goto(pan_deg, tilt_deg).await,
            Command::Home => self.ptz.home().await,
            Command::Stop => self.ptz.stop().await,
        };
        // Whatever the outcome, the next decision wants a fresh status.
        self.ptz_cache = None;
        match result {
            Ok(PtzResponse::Ok) => {
                self.ptz_ok();
                if matches!(command, Command::Stop) {
                    self.ours = false;
                } else {
                    self.ours = true;
                    self.shared.count_move();
                }
            }
            Ok(PtzResponse::Error { code, message }) => {
                counter!("kamoder_follow_ptz_errors_total").increment(1);
                tracing::warn!(
                    op = command.op(),
                    code = code.as_str(),
                    message,
                    "ptz refused the command"
                );
            }
            Ok(other) => {
                counter!("kamoder_follow_ptz_errors_total").increment(1);
                tracing::warn!(op = command.op(), response = ?other, "unexpected ptz response");
            }
            Err(error) => {
                self.ptz_failed(Instant::now(), &error.to_string());
            }
        }
    }

    fn publish(&self) {
        let state = if !*self.enabled.borrow() {
            State::Disabled
        } else if !(self.motion_up && self.ptz_up) {
            State::Unavailable
        } else {
            self.controller.state().wire()
        };
        let target = if state == State::Disabled {
            None
        } else {
            self.target
        };
        self.shared.publish(state, target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use ptz_proto::Request as PtzRequest;
    use std::path::Path;
    use std::sync::Mutex;
    use tokio::net::UnixListener;

    #[test]
    fn backoff_doubles_to_30_s() {
        let mut backoff = Backoff::default();
        let secs: Vec<u64> = (0..7).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30]);
        backoff.reset();
        assert_eq!(backoff.next_delay(), Backoff::INITIAL);
    }

    #[test]
    fn board_tuning_maps_onto_the_configs() {
        let follow = board::Follow {
            dead_zone_permille: [200, 300],
            max_step_deg: 7.5,
            fov_deg: [60.0, 34.0],
            decision_frames: 3,
            settle_ms: 450,
            return_after_s: 20,
            manual_hold_s: 90,
            t_on: 800,
            t_off: 900,
            min_cells: 4,
            max_fraction_permille: 500,
            home: Some([175.0, 30.0]),
            ..board::Follow::default()
        };
        let d = detect_config(&follow);
        assert_eq!((d.t_on, d.t_off), (800, 800), "t_off capped at t_on");
        assert_eq!((d.min_cells, d.decision_frames), (4, 3));
        assert_eq!(d.max_fraction_permille, 500);
        assert_eq!(d.k, DetectConfig::default().k);
        let c = control_config(&follow);
        assert_eq!((c.dead_zone_x_permille, c.dead_zone_y_permille), (200, 300));
        assert_eq!(
            (c.max_step_deg, c.fov_h_deg, c.fov_v_deg),
            (7.5, 60.0, 34.0)
        );
        assert_eq!(c.settle, Duration::from_millis(450));
        assert_eq!(c.return_after, Duration::from_secs(20));
        assert_eq!(c.manual_hold, Duration::from_secs(90));
        assert_eq!(c.gain, ControlConfig::default().gain);
        assert_eq!(Settings::from_board(&follow).home, Some([175.0, 30.0]));
    }

    // The end-to-end tests run on the real clock with short intervals:
    // tokio's paused clock auto-advances past socket I/O that is still in
    // flight between tasks, so the fakes would never get to answer.

    const COLS: u8 = 32;
    const ROWS: u8 = 24;

    /// Time between fake grids.
    const GRID: Duration = Duration::from_millis(20);

    /// A quiet grid with a 2×2 blob at `col` (none if `None`).
    fn grid(seq: u64, col: Option<u8>) -> MotionGrid {
        let mut cells = vec![100u16; usize::from(COLS) * usize::from(ROWS)];
        if let Some(col) = col {
            for row in 11..13usize {
                for c in col..col + 2 {
                    cells[row * usize::from(COLS) + usize::from(c)] = 5_000;
                }
            }
        }
        MotionGrid::new(seq, seq * 100, COLS, ROWS, u16::MAX, cells).unwrap()
    }

    /// camerad's motion socket: checks the subscribe line, then sends a grid
    /// every [`GRID`], the blob (if any) drifting on the right of the frame.
    /// Counts connections.
    fn fake_motion(path: &Path, blob: bool) -> Arc<Mutex<usize>> {
        let connections = Arc::new(Mutex::new(0));
        let count = Arc::clone(&connections);
        let listener = UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                *count.lock().unwrap() += 1;
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut line = String::new();
                    BufReader::new(read).read_line(&mut line).await.unwrap();
                    assert_eq!(line, "0\n");
                    let mut out = Vec::new();
                    for seq in 0u64.. {
                        let col = blob.then_some(26 + (seq % 3) as u8);
                        motion::encode_line(&grid(seq, col), &mut out).unwrap();
                        if write.write_all(&out).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(GRID).await;
                    }
                });
            }
        });
        connections
    }

    type Seen = Arc<Mutex<Vec<PtzRequest>>>;

    /// The onvif PTZ socket: records requests, answers a stopped head.
    fn fake_ptz(path: &Path) -> Seen {
        let seen: Seen = Arc::default();
        let listener = UnixListener::bind(path).unwrap();
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                BufReader::new(read).read_line(&mut line).await.unwrap();
                let request: PtzRequest = serde_json::from_str(&line).unwrap();
                let response = match request {
                    PtzRequest::Status => PtzResponse::Status {
                        backend: "sim".into(),
                        limits: Limits {
                            pan_min_deg: 0.0,
                            pan_max_deg: 350.0,
                            tilt_min_deg: 0.0,
                            tilt_max_deg: 65.0,
                        },
                        position: Position {
                            pan_deg: 100.0,
                            tilt_deg: 30.0,
                            moving: false,
                        },
                        last_manual_ms_ago: None,
                    },
                    _ => PtzResponse::Ok,
                };
                log.lock().unwrap().push(request);
                let mut out = serde_json::to_vec(&response).unwrap();
                out.push(b'\n');
                let _ = write.write_all(&out).await;
            }
        });
        seen
    }

    fn settings(dir: &TempDir) -> Settings {
        Settings {
            motion_socket: dir.path().join("motion.sock"),
            camera: 0,
            ptz_socket: dir.path().join("ptz.sock"),
            detect: DetectConfig::default(),
            control: ControlConfig {
                settle: Duration::from_millis(60),
                ..ControlConfig::default()
            },
            home: None,
            stale_after: STALE_AFTER,
        }
    }

    fn motions(seen: &Seen) -> Vec<PtzRequest> {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|r| r.motion_source().is_some())
            .cloned()
            .collect()
    }

    /// Runs the loop for `ms` milliseconds.
    async fn run_for(runtime: &mut Runtime, ms: u64) {
        let _ = tokio::time::timeout(Duration::from_millis(ms), runtime.run()).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn follows_a_moving_blob_with_follow_tagged_gotos() {
        let dir = TempDir::new("rt-follow");
        let settings = settings(&dir);
        fake_motion(&settings.motion_socket, true);
        let seen = fake_ptz(&settings.ptz_socket);
        let shared = Shared::new(true, None);
        let mut runtime = Runtime::new(settings, Arc::clone(&shared));

        run_for(&mut runtime, 800).await;

        let moves = motions(&seen);
        assert!(!moves.is_empty(), "no goto issued");
        let PtzRequest::Goto { pan, tilt, source } = &moves[0] else {
            panic!("first motion request is {:?}", moves[0]);
        };
        assert_eq!(*source, Some(Source::Follow));
        assert!(*pan > 100.0, "blob on the right pans right, got {pan}");
        assert!(*pan <= 100.0 + ControlConfig::default().max_step_deg + 0.01);
        assert!(
            (*tilt - 30.0).abs() < 0.01,
            "blob centred vertically, got {tilt}"
        );
        for request in &moves {
            assert_eq!(request.motion_source(), Some(Source::Follow));
        }
        let status = shared.status();
        assert!(status.enabled);
        assert_eq!(status.moves, moves.len() as u64);
        assert_ne!(status.state, State::Unavailable);
        assert_ne!(status.state, State::Disabled);
        // The target itself comes and goes with every re-seed after a move;
        // `ptz_down_is_unavailable_and_recovers` checks it.
    }

    #[tokio::test(flavor = "current_thread")]
    async fn returns_to_the_board_home_without_a_target() {
        let dir = TempDir::new("rt-home");
        let mut settings = settings(&dir);
        settings.home = Some([175.0, 40.0]);
        settings.control.return_after = Duration::from_millis(300);
        fake_motion(&settings.motion_socket, false);
        let seen = fake_ptz(&settings.ptz_socket);
        let mut runtime = Runtime::new(settings, Shared::new(true, None));

        run_for(&mut runtime, 800).await;

        assert_eq!(
            motions(&seen),
            [PtzRequest::Goto {
                pan: 175.0,
                tilt: 40.0,
                source: Some(Source::Follow)
            }],
            "one return home, to the board's home"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn returns_to_the_ptz_home_without_a_board_home() {
        let dir = TempDir::new("rt-ptz-home");
        let mut settings = settings(&dir);
        settings.control.return_after = Duration::from_millis(300);
        fake_motion(&settings.motion_socket, false);
        let seen = fake_ptz(&settings.ptz_socket);
        let mut runtime = Runtime::new(settings, Shared::new(true, None));

        run_for(&mut runtime, 800).await;

        assert_eq!(
            motions(&seen),
            [PtzRequest::Home {
                source: Some(Source::Follow)
            }]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn disabled_sends_nothing_and_reads_no_grids() {
        let dir = TempDir::new("rt-disabled");
        let settings = settings(&dir);
        let connections = fake_motion(&settings.motion_socket, true);
        let seen = fake_ptz(&settings.ptz_socket);
        let shared = Shared::new(false, None);
        let mut runtime = Runtime::new(settings, Arc::clone(&shared));

        run_for(&mut runtime, 400).await;

        assert!(
            seen.lock().unwrap().is_empty(),
            "{:?}",
            seen.lock().unwrap()
        );
        assert_eq!(*connections.lock().unwrap(), 0, "no grids while disabled");
        assert_eq!(shared.status().state, State::Disabled);
        assert_eq!(shared.status().target, None);

        // Switching on starts tracking without a restart.
        assert_eq!(
            shared.handle(follow_proto::Request::Set { enabled: true }),
            follow_proto::Response::Ok
        );
        run_for(&mut runtime, 800).await;
        assert!(!motions(&seen).is_empty());
        assert_eq!(*connections.lock().unwrap(), 1);

        // And off again: the socket is closed and nothing more is sent.
        shared.handle(follow_proto::Request::Set { enabled: false });
        run_for(&mut runtime, 100).await;
        let before = seen.lock().unwrap().len();
        run_for(&mut runtime, 400).await;
        assert_eq!(seen.lock().unwrap().len(), before);
        assert_eq!(shared.status().state, State::Disabled);
        assert_eq!(shared.status().target, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ptz_down_is_unavailable_and_recovers() {
        let dir = TempDir::new("rt-ptz-down");
        let settings = settings(&dir);
        let ptz_path = settings.ptz_socket.clone();
        fake_motion(&settings.motion_socket, true);
        let shared = Shared::new(true, None);
        let mut runtime = Runtime::new(settings, Arc::clone(&shared));

        run_for(&mut runtime, 500).await;
        assert_eq!(shared.status().state, State::Unavailable);
        // Detection keeps running without the PTZ.
        let target = shared.status().target.expect("a target");
        assert!(target.x_permille > 800, "{target:?}");

        // onvif comes up: tracking resumes after the first retry (1 s).
        let seen = fake_ptz(&ptz_path);
        run_for(&mut runtime, 1500).await;
        assert!(!motions(&seen).is_empty());
        assert_ne!(shared.status().state, State::Unavailable);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn motion_down_is_unavailable_and_recovers() {
        let dir = TempDir::new("rt-motion-down");
        let settings = settings(&dir);
        let motion_path = settings.motion_socket.clone();
        let seen = fake_ptz(&settings.ptz_socket);
        let shared = Shared::new(true, None);
        let mut runtime = Runtime::new(settings, Arc::clone(&shared));

        run_for(&mut runtime, 500).await;
        assert_eq!(shared.status().state, State::Unavailable);
        assert!(seen.lock().unwrap().is_empty());

        // camerad comes up: grids flow after the first retry (1 s).
        fake_motion(&motion_path, true);
        run_for(&mut runtime, 1500).await;
        assert!(!motions(&seen).is_empty());
        assert_ne!(shared.status().state, State::Unavailable);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_motion_socket_that_falls_silent_goes_unavailable() {
        let dir = TempDir::new("rt-silent");
        let mut settings = settings(&dir);
        settings.stale_after = Duration::from_millis(200);
        // Three grids, then silence with the connection held open.
        let listener = UnixListener::bind(&settings.motion_socket).unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut out = Vec::new();
                for seq in 0..3 {
                    motion::encode_line(&grid(seq, None), &mut out).unwrap();
                    write.write_all(&out).await.unwrap();
                }
                held.push((read, write));
            }
        });
        let _seen = fake_ptz(&settings.ptz_socket);
        let shared = Shared::new(true, None);
        let mut runtime = Runtime::new(settings, Arc::clone(&shared));
        run_for(&mut runtime, 100).await;
        assert_eq!(shared.status().state, State::Idle);
        run_for(&mut runtime, 400).await;
        assert_eq!(shared.status().state, State::Unavailable);
    }
}
