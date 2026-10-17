//! The control socket (`/tmp/kamoder-follow.sock`, [`follow_proto`]) and the
//! state it reports, shared with the tracking loop.
//!
//! [`Shared`] is the meeting point: the loop publishes the state and target
//! after every grid, the socket reads them, and `set` persists the switch
//! and wakes the loop through a `watch` channel, so tracking starts and
//! stops without a restart (FOL-16).
//!
//! The socket mirrors the lights one: mode 0600, a stale socket from a
//! previous run is replaced (any other file is not), one task per
//! connection, at most [`MAX_CONNECTIONS`], lines capped at [`MAX_LINE`],
//! idle connections closed.

use crate::store::{self, Saved};
use follow_proto::{ErrorCode, FollowStatus, Request, Response, State, Target, MAX_LINE};
use metrics::{counter, gauge};
use snafu::{ResultExt, Snafu};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;

/// Connections served at once; `web` is the expected client.
const MAX_CONNECTIONS: usize = 8;

/// A connection idle this long between requests is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// State shared by the loop and the socket.
pub struct Shared {
    status: Mutex<FollowStatus>,
    enabled: watch::Sender<bool>,
    /// State file; `None` keeps the switch in memory only.
    store: Option<PathBuf>,
}

impl Shared {
    /// Starts with the switch at `enabled` and the state `unavailable` until
    /// the loop has heard from both sockets (`disabled` when off).
    pub fn new(enabled: bool, store: Option<PathBuf>) -> Arc<Self> {
        let state = if enabled {
            State::Unavailable
        } else {
            State::Disabled
        };
        for s in State::ALL {
            gauge!("kamoder_follow_state", "state" => s.as_str())
                .set(f64::from(u8::from(s == state)));
        }
        gauge!("kamoder_follow_enabled").set(f64::from(u8::from(enabled)));
        Arc::new(Self {
            status: Mutex::new(FollowStatus {
                enabled,
                state,
                target: None,
                moves: 0,
            }),
            enabled: watch::Sender::new(enabled),
            store,
        })
    }

    /// What `status` answers.
    pub fn status(&self) -> FollowStatus {
        *self.lock()
    }

    /// The switch, for the loop.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.enabled.subscribe()
    }

    /// The loop's view after a grid.
    pub fn publish(&self, state: State, target: Option<Target>) {
        let mut status = self.lock();
        if status.state != state {
            gauge!("kamoder_follow_state", "state" => status.state.as_str()).set(0.0);
            gauge!("kamoder_follow_state", "state" => state.as_str()).set(1.0);
            tracing::info!(from = %status.state, to = %state, "follow state");
            status.state = state;
        }
        status.target = target;
    }

    /// One accepted move (goto or home).
    pub fn count_move(&self) {
        self.lock().moves += 1;
        counter!("kamoder_follow_moves_total").increment(1);
    }

    /// Carries out one request.
    pub fn handle(&self, request: Request) -> Response {
        let op = request.op();
        let response = match request {
            Request::Status => Response::Status(self.status()),
            Request::Set { enabled } => self.set(enabled),
        };
        let result = match &response {
            Response::Error { code, .. } => code.as_str(),
            _ => "ok",
        };
        counter!("kamoder_follow_socket_requests_total", "op" => op, "result" => result)
            .increment(1);
        response
    }

    /// Persists, then applies; a failed save changes nothing.
    fn set(&self, enabled: bool) -> Response {
        // Held across the save so two clients cannot interleave file and
        // switch.
        let mut status = self.lock();
        if let Some(path) = &self.store {
            if let Err(error) = store::save(path, &Saved { enabled }) {
                tracing::warn!(path = %path.display(), %error, "cannot save the follow state");
                return error_response(
                    ErrorCode::Store,
                    format!("cannot save {}: {error}", path.display()),
                );
            }
        }
        if status.enabled != enabled {
            tracing::info!(enabled, "follow switched");
        }
        status.enabled = enabled;
        if !enabled {
            // At once, so a status right after the set shows it; the loop
            // stops the motors if needed.
            if status.state != State::Disabled {
                gauge!("kamoder_follow_state", "state" => status.state.as_str()).set(0.0);
                gauge!("kamoder_follow_state", "state" => State::Disabled.as_str()).set(1.0);
            }
            status.state = State::Disabled;
            status.target = None;
        }
        gauge!("kamoder_follow_enabled").set(f64::from(u8::from(enabled)));
        self.enabled.send_if_modified(|current| {
            let changed = *current != enabled;
            *current = enabled;
            changed
        });
        Response::Ok
    }

    fn lock(&self) -> MutexGuard<'_, FollowStatus> {
        self.status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
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
                tracing::warn!(path = %self.path.display(), %error, "cannot remove follow socket");
            }
        }
    }
}

/// Binds `path` and starts accepting in the background.
pub fn serve(path: &Path, shared: Arc<Shared>) -> Result<Server, Error> {
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
    tracing::info!(path = %path.display(), "follow socket listening");

    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let accept = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::warn!(%error, "follow socket accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                tracing::warn!("too many follow socket connections, refusing one");
                counter!("kamoder_follow_socket_refused_total").increment(1);
                drop(stream);
                continue;
            };
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                if let Err(error) = connection(stream, &shared).await {
                    tracing::debug!(%error, "follow connection ended with an error");
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
async fn connection(stream: UnixStream, shared: &Shared) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut buf = Vec::with_capacity(64);
    loop {
        buf.clear();
        // One byte past the cap tells an over-long line from one exactly at it.
        let mut limited = (&mut reader).take(MAX_LINE as u64 + 1);
        let read =
            match tokio::time::timeout(IDLE_TIMEOUT, limited.read_until(b'\n', &mut buf)).await {
                Ok(read) => read?,
                Err(_) => return Ok(()),
            };
        if read == 0 {
            return Ok(());
        }
        let complete = buf.last() == Some(&b'\n');
        let too_long = buf.len() > MAX_LINE + usize::from(complete);
        let response = if too_long {
            error_response(
                ErrorCode::TooLong,
                format!("request exceeds {MAX_LINE} bytes"),
            )
        } else if !complete {
            return Ok(());
        } else {
            match serde_json::from_slice::<Request>(&buf) {
                Ok(request) => shared.handle(request),
                Err(e) => {
                    counter!("kamoder_follow_socket_requests_total", "op" => "invalid", "result" => "bad_request").increment(1);
                    error_response(ErrorCode::BadRequest, e.to_string())
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

fn error_response(code: ErrorCode, message: impl Into<String>) -> Response {
    Response::Error {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use follow_proto::Client;

    #[tokio::test(flavor = "current_thread")]
    async fn status_and_set_over_the_socket() {
        let dir = TempDir::new("server");
        let store = dir.path().join("follow.json");
        let path = dir.path().join("follow.sock");
        let shared = Shared::new(false, Some(store.clone()));
        let mut rx = shared.subscribe();
        let server = serve(&path, Arc::clone(&shared)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let client = Client::new(&path);
        let disabled = FollowStatus {
            enabled: false,
            state: State::Disabled,
            target: None,
            moves: 0,
        };
        assert_eq!(client.status().await.unwrap(), Response::Status(disabled));

        assert_eq!(client.set_enabled(true).await.unwrap(), Response::Ok);
        assert!(rx.has_changed().unwrap());
        assert!(*rx.borrow_and_update());
        assert_eq!(store::load(&store), Some(Saved { enabled: true }));
        let Response::Status(status) = client.status().await.unwrap() else {
            panic!("not a status");
        };
        assert!(status.enabled);

        // The loop's view shows through.
        let target = Target {
            x_permille: 800,
            y_permille: 500,
            cells: 4,
        };
        shared.publish(State::Tracking, Some(target));
        shared.count_move();
        assert_eq!(
            client.status().await.unwrap(),
            Response::Status(FollowStatus {
                enabled: true,
                state: State::Tracking,
                target: Some(target),
                moves: 1,
            })
        );

        // Setting the same value again wakes nobody.
        assert_eq!(client.set_enabled(true).await.unwrap(), Response::Ok);
        assert!(!rx.has_changed().unwrap());

        assert_eq!(client.set_enabled(false).await.unwrap(), Response::Ok);
        assert!(!*rx.borrow_and_update());
        assert_eq!(
            client.status().await.unwrap(),
            Response::Status(FollowStatus {
                moves: 1,
                ..disabled
            })
        );
        assert_eq!(store::load(&store), Some(Saved { enabled: false }));

        drop(server);
        assert!(!path.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_save_changes_nothing() {
        let dir = TempDir::new("server-store");
        // A non-empty directory where the file should go: the rename fails.
        let store = dir.path().join("blocked");
        std::fs::create_dir_all(store.join("child")).unwrap();
        let shared = Shared::new(false, Some(store));
        let rx = shared.subscribe();
        let Response::Error { code, .. } = shared.handle(Request::Set { enabled: true }) else {
            panic!("save should fail");
        };
        assert_eq!(code, ErrorCode::Store);
        assert!(!shared.status().enabled);
        assert!(!rx.has_changed().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn memory_only_without_a_store() {
        let shared = Shared::new(true, None);
        assert_eq!(shared.status().state, State::Unavailable);
        assert_eq!(shared.handle(Request::Set { enabled: false }), Response::Ok);
        assert_eq!(shared.status().state, State::Disabled);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bad_and_over_long_lines() {
        let dir = TempDir::new("server-bad");
        let path = dir.path().join("follow.sock");
        let _server = serve(&path, Shared::new(false, None)).unwrap();

        let stream = UnixStream::connect(&path).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        write.write_all(b"{\"op\":\"reboot\"}\n").await.unwrap();
        reader.read_line(&mut line).await.unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert!(matches!(
            response,
            Response::Error {
                code: ErrorCode::BadRequest,
                ..
            }
        ));
        // The connection survives a bad request.
        line.clear();
        write.write_all(b"{\"op\":\"status\"}\n").await.unwrap();
        reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str(&line).unwrap(),
            Response::Status(_)
        ));
        // An over-long line is answered, then the connection closes.
        line.clear();
        write.write_all(&vec![b' '; MAX_LINE + 10]).await.unwrap();
        reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str(&line).unwrap(),
            Response::Error {
                code: ErrorCode::TooLong,
                ..
            }
        ));
        line.clear();
        assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refuses_to_replace_a_regular_file() {
        let dir = TempDir::new("server-file");
        let path = dir.path().join("follow.sock");
        std::fs::write(&path, "x").unwrap();
        assert!(matches!(
            serve(&path, Shared::new(false, None)),
            Err(Error::NotASocket { .. })
        ));
    }
}
