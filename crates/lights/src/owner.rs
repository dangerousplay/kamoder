//! The running service: [`Lights`] behind a lock, persistence, the blink
//! task and the ownership socket (protocol in [`lights_proto`]).
//!
//! The socket mirrors the PTZ one in `onvif`: mode 0600, a stale socket from
//! a previous run is replaced (any other file is not), one task per
//! connection, at most [`MAX_CONNECTIONS`], lines capped at
//! [`MAX_LINE`], idle connections closed.

use crate::control::Lights;
use crate::store::{self, Saved};
use lights_proto::{
    Alarm, ErrorCode, Request, Response, State, MAX_BLINK_PERIOD_MS, MAX_LINE, MIN_BLINK_PERIOD_MS,
};
use metrics::counter;
use snafu::{ResultExt, Snafu};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinHandle;

/// Connections served at once; web and onvif are the expected clients.
const MAX_CONNECTIONS: usize = 8;

/// A connection idle this long between requests is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The lights plus what the service adds around them.
pub struct Owner {
    lights: Mutex<Lights>,
    /// State file; `None` keeps changes in memory only.
    store: Option<PathBuf>,
    /// Wakes the blink task when the alarm changes.
    alarm_changed: Notify,
}

impl Owner {
    /// Wraps started lights. `store` is where changes are saved.
    pub fn new(lights: Lights, store: Option<PathBuf>) -> Self {
        Self {
            lights: Mutex::new(lights),
            store,
            alarm_changed: Notify::new(),
        }
    }

    /// The state last applied.
    pub fn state(&self) -> State {
        self.lock().state()
    }

    /// Carries out one request.
    pub fn handle(&self, request: Request) -> Response {
        let op = request.op();
        let response = self.dispatch(request);
        let result = match &response {
            Response::Error { code, .. } => code.as_str(),
            Response::State { .. } => "ok",
        };
        counter!("kamoder_lights_socket_requests_total", "op" => op, "result" => result)
            .increment(1);
        response
    }

    /// Applies the safe state now (on SIGTERM/SIGINT). Not saved: the next
    /// start restores what the user set.
    pub fn shutdown(&self) {
        if let Err(error) = self.lock().safe_state() {
            tracing::error!(%error, "cannot apply the safe state");
        } else {
            tracing::info!("lights in the safe state");
        }
    }

    fn dispatch(&self, request: Request) -> Response {
        let mut lights = self.lock();
        let before = lights.state();
        if let Err(message) = request.check_supported(before.supported) {
            return error(ErrorCode::InvalidArgument, message);
        }
        let outcome = match request {
            Request::Get => Ok(()),
            Request::SetMode { mode } => lights.set_mode(mode),
            Request::SetWhite { on } => lights.set_white(on),
            Request::SetAlarm { alarm } => {
                if let Err(message) = check_alarm(alarm) {
                    return error(ErrorCode::InvalidArgument, message);
                }
                let outcome = lights.set_alarm(alarm);
                self.alarm_changed.notify_one();
                outcome
            }
            Request::SetMute { muted } => lights.set_muted(muted),
        };
        let after = lights.state();
        drop(lights);

        // Saved even after a partial failure: the state records what did
        // get applied, and only changes are written.
        if Saved::from_state(after) != Saved::from_state(before) {
            self.save(after);
        }
        match outcome {
            Ok(()) => Response::State { state: after },
            Err(e) => error(ErrorCode::Gpio, e.to_string()),
        }
    }

    fn save(&self, state: State) {
        let Some(path) = &self.store else { return };
        if let Err(error) = store::save(path, &Saved::from_state(state)) {
            counter!("kamoder_lights_save_errors_total").increment(1);
            tracing::warn!(path = %path.display(), %error, "cannot save the light state");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Lights> {
        // A panic while holding the lock leaves at worst a stale state
        // record; the pins themselves are still consistent.
        self.lights
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// Refuses a blink period outside the protocol's range.
pub fn check_alarm(alarm: Alarm) -> Result<(), String> {
    match alarm {
        Alarm::Blink { period_ms }
            if !(MIN_BLINK_PERIOD_MS..=MAX_BLINK_PERIOD_MS).contains(&period_ms) =>
        {
            Err(format!(
                "period_ms must be within {MIN_BLINK_PERIOD_MS}..={MAX_BLINK_PERIOD_MS}"
            ))
        }
        _ => Ok(()),
    }
}

/// A saved state with a blink period out of range (hand-edited file) gets
/// the nearest allowed one rather than a refusal at start.
pub fn sanitize(mut state: State) -> State {
    if let Alarm::Blink { period_ms } = &mut state.alarm {
        *period_ms = (*period_ms).clamp(MIN_BLINK_PERIOD_MS, MAX_BLINK_PERIOD_MS);
    }
    state
}

/// Toggles the alarm lights every half period while the alarm blinks, and
/// sleeps until the alarm changes otherwise. A change restarts the phase, so
/// a new blink shows a full lit half first.
pub async fn blink(owner: Arc<Owner>) {
    loop {
        let half = owner.lock().blink_half_period();
        let Some(half) = half else {
            owner.alarm_changed.notified().await;
            continue;
        };
        tokio::select! {
            _ = tokio::time::sleep(half) => {
                // The error is already logged and counted by Lights.
                let _ = owner.lock().blink_step();
            }
            _ = owner.alarm_changed.notified() => {}
        }
    }
}

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("{} exists and is not a socket; refusing to replace it", path.display()))]
    NotASocket { path: PathBuf },

    #[snafu(display("cannot remove stale socket {}: {source}", path.display()))]
    RemoveStale {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("cannot listen on {}: {source}", path.display()))]
    Bind {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("cannot restrict {} to mode 0600: {source}", path.display()))]
    Permissions {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The listening socket. Dropping it stops accepting and removes the file.
pub struct Server {
    path: PathBuf,
    accept: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.accept.abort();
        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.path.display(), %error, "cannot remove lights socket");
            }
        }
    }
}

/// Binds `path` and starts accepting in the background.
pub fn serve(path: &Path, owner: Arc<Owner>) -> Result<Server, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path).context(RemoveStaleSnafu { path })?;
        }
        Ok(_) => return NotASocketSnafu { path }.fail(),
        Err(_) => {}
    }
    let listener = UnixListener::bind(path).context(BindSnafu { path })?;
    // Root with umask 022: other users could connect only in this window,
    // and the camera has no other users.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .context(PermissionsSnafu { path })?;
    tracing::info!(path = %path.display(), "lights socket listening");

    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let accept = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::warn!(%error, "lights socket accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                tracing::warn!("too many lights socket connections, refusing one");
                counter!("kamoder_lights_socket_refused_total").increment(1);
                drop(stream);
                continue;
            };
            let owner = Arc::clone(&owner);
            tokio::spawn(async move {
                if let Err(error) = connection(stream, &owner).await {
                    tracing::debug!(%error, "lights connection ended with an error");
                }
                drop(permit);
            });
        }
    });
    Ok(Server {
        path: path.to_path_buf(),
        accept,
    })
}

/// Serves one connection: a request per line, a response per line.
async fn connection(stream: UnixStream, owner: &Owner) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut buf = Vec::with_capacity(128);
    loop {
        buf.clear();
        // One byte past the cap tells an over-long line from one exactly at it.
        let limited = (&mut reader).take(MAX_LINE as u64 + 1);
        let read = match tokio::time::timeout(IDLE_TIMEOUT, read_line(limited, &mut buf)).await {
            Ok(read) => read?,
            Err(_) => return Ok(()),
        };
        if read == 0 {
            return Ok(());
        }
        let complete = buf.last() == Some(&b'\n');
        let too_long = buf.len() > MAX_LINE + usize::from(complete);
        let response = if too_long {
            error(
                ErrorCode::TooLong,
                format!("request exceeds {MAX_LINE} bytes"),
            )
        } else if !complete {
            return Ok(());
        } else {
            match serde_json::from_slice::<Request>(&buf) {
                Ok(request) => owner.handle(request),
                Err(e) => {
                    counter!("kamoder_lights_socket_requests_total", "op" => "invalid", "result" => "bad_request").increment(1);
                    error(ErrorCode::BadRequest, e.to_string())
                }
            }
        };

        let mut line = serde_json::to_vec(&response).map_err(std::io::Error::other)?;
        line.push(b'\n');
        write.write_all(&line).await?;
        if too_long {
            return Ok(());
        }
    }
}

async fn read_line<R>(mut reader: R, buf: &mut Vec<u8>) -> std::io::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    reader.read_until(b'\n', buf).await
}

fn error(code: ErrorCode, message: impl Into<String>) -> Response {
    Response::Error {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::tests::Rig;
    use crate::gpio::Line;
    use lights_proto::{Client, Mode};

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kamoder-lights-owner-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn owner(tag: &str, store: Option<PathBuf>) -> (Rig, Arc<Owner>) {
        let (rig, gpio) = Rig::new(tag);
        let lights = Lights::start(gpio, State::default()).unwrap();
        (rig, Arc::new(Owner::new(lights, store)))
    }

    async fn raw(path: &Path, bytes: &[u8]) -> Response {
        let stream = UnixStream::connect(path).await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(bytes).await.unwrap();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn code(response: Response) -> ErrorCode {
        match response {
            Response::Error { code, .. } => code,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    fn state(response: Response) -> State {
        match response {
            Response::State { state } => state,
            other => panic!("expected the state, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn socket_round_trip() {
        let dir = scratch("socket");
        let path = dir.join("lights.sock");
        // A stale socket from a previous run is replaced.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let (rig, owner) = owner("socket", Some(dir.join("lights.json")));
        let server = serve(&path, Arc::clone(&owner)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let client = Client::new(&path);

        assert_eq!(state(client.get().await.unwrap()), State::default());
        rig.take();

        let night = state(client.set_mode(Mode::Night).await.unwrap());
        assert_eq!(night.mode, Mode::Night);
        assert_eq!(rig.take(), [(Line::IrCut, true), (Line::IrLeds, true)]);

        assert!(state(client.set_white(true).await.unwrap()).white);
        let blink = Alarm::Blink { period_ms: 400 };
        assert_eq!(state(client.set_alarm(blink).await.unwrap()).alarm, blink);
        let unmuted = state(client.set_mute(false).await.unwrap());
        assert!(!unmuted.muted);
        assert_eq!(owner.state(), unmuted);

        // Saved: everything but the amp.
        let saved = store::load(&dir.join("lights.json"));
        assert_eq!(
            saved,
            Saved {
                mode: Mode::Night,
                white: true,
                alarm: blink
            }
        );

        // Bad input.
        assert_eq!(
            code(
                client
                    .set_alarm(Alarm::Blink { period_ms: 10 })
                    .await
                    .unwrap()
            ),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            code(raw(&path, b"{\"op\":\"set_gpio\",\"gpio\":98}\n").await),
            ErrorCode::BadRequest
        );
        let long = vec![b'x'; MAX_LINE + 10];
        assert_eq!(code(raw(&path, &long).await), ErrorCode::TooLong);

        // Several requests on one connection.
        let stream = UnixStream::connect(&path).await.unwrap();
        let (read, mut write) = stream.into_split();
        write
            .write_all(b"{\"op\":\"set_white\",\"on\":false}\n{\"op\":\"get\"}\n")
            .await
            .unwrap();
        let mut lines = BufReader::new(read).lines();
        for _ in 0..2 {
            let response: Response =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert!(!state(response).white);
        }

        drop(server);
        assert!(!path.exists(), "socket removed on drop");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn gpio_failures_are_reported_and_partial_changes_saved() {
        let dir = scratch("gpio-fail");
        let store_path = dir.join("lights.json");
        let (rig, owner) = owner("gpio-fail", Some(store_path.clone()));
        rig.fail.lock().unwrap().push(Line::IrLeds);
        assert_eq!(
            code(owner.handle(Request::SetMode { mode: Mode::Night })),
            ErrorCode::Gpio
        );
        // The filter moved, the LEDs did not; the recorded mode is unchanged.
        assert_eq!(owner.state().mode, Mode::Day);
        assert!(!store_path.exists(), "nothing changed, nothing saved");
        rig.fail.lock().unwrap().clear();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn shutdown_applies_the_safe_state_without_saving_it() {
        let dir = scratch("shutdown");
        let store_path = dir.join("lights.json");
        let (rig, owner) = owner("shutdown", Some(store_path.clone()));
        owner.handle(Request::SetMode { mode: Mode::Night });
        owner.handle(Request::SetAlarm { alarm: Alarm::On });
        rig.take();
        owner.shutdown();
        assert_eq!(
            rig.take(),
            [
                (Line::Amp, false),
                (Line::Alarm, false),
                (Line::White, false),
                (Line::IrLeds, false),
                (Line::IrCut, false)
            ]
        );
        assert_eq!(owner.state(), State::default());
        let saved = store::load(&store_path);
        assert_eq!(saved.mode, Mode::Night);
        assert_eq!(saved.alarm, Alarm::On);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn blink_task_toggles_each_half_period() {
        let (rig, owner) = owner("blink-task", None);
        let task = tokio::spawn(blink(Arc::clone(&owner)));
        tokio::task::yield_now().await;
        rig.take();

        owner.handle(Request::SetAlarm {
            alarm: Alarm::Blink { period_ms: 1000 },
        });
        assert_eq!(rig.take(), [(Line::Alarm, true)]);
        // The lit half lasts the full 500 ms despite the wake-up.
        tokio::time::sleep(Duration::from_millis(490)).await;
        assert_eq!(rig.take(), []);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(rig.take(), [(Line::Alarm, false)]);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(rig.take(), [(Line::Alarm, true)]);

        // Steady on: no more toggles.
        owner.handle(Request::SetAlarm { alarm: Alarm::On });
        rig.take();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(rig.take(), []);
        task.abort();
    }

    #[test]
    fn requests_for_lines_the_board_lacks_are_refused_by_name() {
        let (rig, gpio, _leds) = crate::control::tests::ir_only("owner-ir-only");
        let owner = Owner::new(Lights::start(gpio, State::default()).unwrap(), None);
        rig.take();
        let supported = owner.state().supported;
        assert_eq!(supported, [Line::IrCut, Line::IrLeds].into_iter().collect());
        for (request, name) in [
            (Request::SetWhite { on: true }, "white"),
            (Request::SetAlarm { alarm: Alarm::On }, "alarm"),
            (
                Request::SetAlarm {
                    alarm: Alarm::Blink { period_ms: 500 },
                },
                "alarm",
            ),
            (Request::SetMute { muted: false }, "amp"),
        ] {
            match owner.handle(request) {
                Response::Error {
                    code: ErrorCode::InvalidArgument,
                    message,
                } => assert!(message.contains(name), "{message}"),
                other => panic!("{request:?}: expected a refusal, got {other:?}"),
            }
        }
        assert_eq!(rig.take(), []);
        assert_eq!(
            owner.state(),
            State {
                supported,
                ..State::default()
            }
        );

        // The lines it has still work, and `get` reports what is there.
        let night = state(owner.handle(Request::SetMode { mode: Mode::Night }));
        assert_eq!(night.mode, Mode::Night);
        assert_eq!(night.supported, supported);
        assert_eq!(rig.take(), [(Line::IrCut, true), (Line::IrLeds, true)]);
        assert_eq!(state(owner.handle(Request::Get)).supported, supported);
    }

    #[test]
    fn saved_blink_periods_are_clamped() {
        let state = |period_ms| State {
            alarm: Alarm::Blink { period_ms },
            ..State::default()
        };
        assert_eq!(sanitize(state(1)), state(MIN_BLINK_PERIOD_MS));
        assert_eq!(sanitize(state(u32::MAX)), state(MAX_BLINK_PERIOD_MS));
        assert_eq!(sanitize(state(700)), state(700));
        assert!(check_alarm(Alarm::Blink { period_ms: 700 }).is_ok());
        assert!(check_alarm(Alarm::On).is_ok());
    }

    #[test]
    fn refuses_to_replace_a_regular_file() {
        let dir = scratch("file");
        let path = dir.join("lights.sock");
        std::fs::write(&path, b"keep me").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_rig, owner) = owner("file", None);
            assert!(matches!(serve(&path, owner), Err(Error::NotASocket { .. })));
        });
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
