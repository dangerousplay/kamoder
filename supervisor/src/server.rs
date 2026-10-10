//! Server side of the control socket (protocol in
//! [`supervisor::control`]).
//!
//! Each connection runs in its own task, so a client that connects and then
//! says nothing costs a few hundred bytes and blocks nobody: supervision runs
//! in the actors, and the accept loop only accepts. Requests are handled
//! strictly one at a time per connection.

use crate::service::{Start, Stop};
use crate::supervisor::{Resolve, Resolved, StatusAll, StatusOf, Supervisor};
use ::supervisor::control::{ErrorCode, Request, Response, MAX_LINE};
use kameo::actor::ActorRef;
use metrics::counter;
use snafu::{ResultExt, Snafu};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

/// Connections served at once. The web server is the only expected client;
/// the cap just keeps a runaway script from exhausting memory.
const MAX_CONNECTIONS: usize = 8;

/// A connection idle for this long between requests is closed, so a client
/// that hangs cannot hold one of the [`MAX_CONNECTIONS`] slots forever.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

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
                tracing::warn!(path = %self.path.display(), %error, "cannot remove control socket");
            }
        }
    }
}

/// Binds the control socket at `path` and starts accepting in the background.
///
/// A leftover socket from a previous run (the supervisor was SIGKILLed, or the
/// camera lost power with `/tmp` on persistent storage) is removed first; any
/// other kind of file at that path is left alone and reported.
pub fn serve(path: &Path, root: ActorRef<Supervisor>) -> Result<Server, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path).context(RemoveStaleSnafu { path })?;
        }
        Ok(_) => return NotASocketSnafu { path }.fail(),
        Err(_) => {}
    }
    let listener = UnixListener::bind(path).context(BindSnafu { path })?;
    // Between bind and chmod the socket has the umask's mode. The supervisor
    // runs as root with the default 022 umask, so at worst other users could
    // connect for a moment — and on this camera there are no other users.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .context(PermissionsSnafu { path })?;
    tracing::info!(path = %path.display(), "control socket listening");

    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let accept = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(error) => {
                    // EMFILE and friends: back off instead of spinning.
                    tracing::warn!(%error, "control socket accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                tracing::warn!("too many control connections, refusing one");
                counter!("kamoder_supervisor_control_refused_total").increment(1);
                drop(stream);
                continue;
            };
            let root = root.clone();
            tokio::spawn(async move {
                if let Err(error) = connection(stream, root).await {
                    tracing::debug!(%error, "control connection ended with an error");
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
async fn connection(stream: UnixStream, root: ActorRef<Supervisor>) -> std::io::Result<()> {
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
            // EOF in the middle of a line: nothing to answer to.
            return Ok(());
        } else {
            match serde_json::from_slice::<Request>(&buf) {
                Ok(request) => handle(&root, request).await,
                Err(error) => self::error(ErrorCode::BadRequest, error.to_string()),
            }
        };

        let mut line = serde_json::to_vec(&response).map_err(std::io::Error::other)?;
        line.push(b'\n');
        write.write_all(&line).await?;
        if too_long {
            // The rest of the line is still unread; resynchronising on it is
            // not worth the trouble for a client that is already misbehaving.
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

/// Carries out one request.
pub async fn handle(root: &ActorRef<Supervisor>, request: Request) -> Response {
    let op = request.op();
    let response = dispatch(root, request).await;
    let result = match &response {
        Response::Error { code, .. } => match code {
            ErrorCode::Critical => "critical",
            ErrorCode::UnknownService => "unknown_service",
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::TooLong => "too_long",
            ErrorCode::ShuttingDown => "shutting_down",
            ErrorCode::Internal => "internal",
        },
        _ => "ok",
    };
    counter!("kamoder_supervisor_control_requests_total", "op" => op, "result" => result)
        .increment(1);
    response
}

async fn dispatch(root: &ActorRef<Supervisor>, request: Request) -> Response {
    let (name, op) = match request {
        Request::Status => {
            return match root.ask(StatusAll).await {
                Ok(services) => Response::Services { services },
                Err(error) => self::error(ErrorCode::Internal, error.to_string()),
            };
        }
        Request::Start { service } => (service, Op::Start),
        Request::Stop { service } => (service, Op::Stop),
        Request::Restart { service } => (service, Op::Restart),
    };

    // Only `start` may bring an idle service's actor into being; `restart`
    // does too, once it has established the service is not critical.
    let resolved = match root
        .ask(Resolve {
            name: name.clone(),
            spawn: op == Op::Start,
        })
        .await
    {
        Ok(resolved) => resolved,
        Err(error) => return self::error(ErrorCode::Internal, error.to_string()),
    };

    let (actor, critical, spawned) = match resolved {
        Resolved::Unknown => {
            return error(
                ErrorCode::UnknownService,
                format!("no service named {name:?}"),
            )
        }
        Resolved::ShuttingDown => {
            return error(ErrorCode::ShuttingDown, "the supervisor is shutting down")
        }
        Resolved::Idle { critical } => (None, critical, false),
        Resolved::Actor {
            actor,
            critical,
            spawned,
        } => (Some(actor), critical, spawned),
    };

    if critical && op != Op::Start {
        return error(
            ErrorCode::Critical,
            format!("{name:?} is critical and cannot be stopped or restarted"),
        );
    }
    tracing::info!(service = %name, op = op.as_str(), "control request");

    let outcome = match (op, actor) {
        // Nothing to stop.
        (Op::Stop, None) => Ok(()),
        (Op::Stop, Some(actor)) => actor.ask(Stop).await.map_err(|e| e.to_string()),
        (Op::Start, Some(actor)) if !spawned => actor.ask(Start).await.map_err(|e| e.to_string()),
        (Op::Start, _) => Ok(()),
        (Op::Restart, Some(actor)) => match actor.ask(Stop).await {
            Ok(()) => actor.ask(Start).await.map_err(|e| e.to_string()),
            Err(error) => Err(error.to_string()),
        },
        (Op::Restart, None) => match root
            .ask(Resolve {
                name: name.clone(),
                spawn: true,
            })
            .await
        {
            Ok(Resolved::ShuttingDown) => {
                return error(ErrorCode::ShuttingDown, "the supervisor is shutting down")
            }
            Ok(_) => Ok(()),
            Err(error) => Err(error.to_string()),
        },
    };
    if let Err(message) = outcome {
        return error(ErrorCode::Internal, message);
    }

    match root.ask(StatusOf(name.clone())).await {
        Ok(Some(service)) => Response::Service { service },
        Ok(None) => error(
            ErrorCode::UnknownService,
            format!("no service named {name:?}"),
        ),
        Err(error) => self::error(ErrorCode::Internal, error.to_string()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Start,
    Stop,
    Restart,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Start => "start",
            Op::Stop => "stop",
            Op::Restart => "restart",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::logs::LogSink;
    use crate::supervisor::Shutdown;
    use ::supervisor::control::{self, Client, ServiceStatus, State};
    use kameo::actor::Actor;

    fn sleep_binary() -> &'static str {
        ["/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .expect("no sleep binary on this host")
    }

    fn find<'a>(services: &'a [ServiceStatus], name: &str) -> &'a ServiceStatus {
        services
            .iter()
            .find(|service| service.name == name)
            .unwrap_or_else(|| panic!("{name} missing from {services:?}"))
    }

    async fn status(client: &Client) -> Vec<ServiceStatus> {
        match client.status().await.unwrap() {
            Response::Services { services } => services,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn service(response: Response) -> ServiceStatus {
        match response {
            Response::Service { service } => service,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn error_code(response: Response) -> ErrorCode {
        match response {
            Response::Error { code, .. } => code,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// Sends raw bytes and reads one response line, for the malformed cases
    /// the typed client cannot produce.
    async fn raw(path: &Path, bytes: &[u8]) -> Response {
        let stream = UnixStream::connect(path).await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(bytes).await.unwrap();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// End to end on the host: real processes, real actors, real socket.
    #[tokio::test]
    async fn control_socket_drives_services() {
        let dir = std::env::temp_dir().join(format!("anyka-supervisor-ctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sleep = sleep_binary();
        let toml = format!(
            r#"
            [[service]]
            name = "guard"
            command = "{sleep}"
            args = ["30"]
            critical = true
            stop_timeout_secs = 2

            [[service]]
            name = "sleeper"
            command = "{sleep}"
            args = ["30"]
            stop_timeout_secs = 2

            [[service]]
            name = "optional"
            command = "{sleep}"
            args = ["30"]
            enabled = false
            stop_timeout_secs = 2
            "#
        );
        let config_path = dir.join("supervisor.toml");
        std::fs::write(&config_path, toml).unwrap();
        let config = Config::load(&config_path).unwrap();

        let sink = LogSink::discarding();
        let root = Supervisor::spawn((Supervisor::plan(&config, sink.clone()), sink));
        let socket = dir.join("control.sock");
        // A stale socket from a "previous run" must be replaced.
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let server = serve(&socket, root.clone()).unwrap();
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let client = Client::new(&socket).with_timeout(Duration::from_secs(10));

        // Initial state: enabled services running, the disabled one not.
        let services = status(&client).await;
        assert_eq!(
            services.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["guard", "sleeper", "optional"]
        );
        let sleeper = find(&services, "sleeper");
        assert_eq!(sleeper.state, State::Running);
        let first_pid = sleeper.pid.expect("running service has a pid");
        assert_eq!(sleeper.restarts, 0);
        assert!(sleeper.uptime_secs.is_some());
        assert!(find(&services, "guard").critical);
        let optional = find(&services, "optional");
        assert_eq!(optional.state, State::Disabled);
        assert_eq!(optional.pid, None);
        assert!(!optional.enabled);

        // Critical services refuse stop and restart, and keep running.
        assert_eq!(
            error_code(client.stop("guard").await.unwrap()),
            ErrorCode::Critical
        );
        assert_eq!(
            error_code(client.restart("guard").await.unwrap()),
            ErrorCode::Critical
        );
        assert_eq!(find(&status(&client).await, "guard").state, State::Running);

        // Unknown services.
        assert_eq!(
            error_code(client.stop("nope").await.unwrap()),
            ErrorCode::UnknownService
        );

        // Stop: no process, no restart, SIGTERM recorded as 143.
        let stopped = service(client.stop("sleeper").await.unwrap());
        assert_eq!(stopped.state, State::Stopped);
        assert_eq!(stopped.pid, None);
        let first_alive =
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(first_pid as i32), None);
        assert!(first_alive.is_err(), "stopped process is gone");
        // The exit is reported asynchronously; give it a moment.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let after = find(&status(&client).await, "sleeper").clone();
        assert_eq!(
            after.state,
            State::Stopped,
            "a stopped service stays stopped"
        );
        assert_eq!(after.last_exit_code, Some(143));

        // Start: a new process.
        let started = service(client.start("sleeper").await.unwrap());
        assert_eq!(started.state, State::Running);
        let second_pid = started.pid.unwrap();
        assert_ne!(second_pid, first_pid);
        assert_eq!(started.restarts, 1);

        // Restart: another new process, still running afterwards.
        let restarted = service(client.restart("sleeper").await.unwrap());
        assert_eq!(restarted.state, State::Running);
        let third_pid = restarted.pid.unwrap();
        assert_ne!(third_pid, second_pid);
        assert_eq!(restarted.restarts, 2);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let after = find(&status(&client).await, "sleeper").clone();
        assert_eq!(
            after.state,
            State::Running,
            "late exit of the old pid is ignored"
        );
        assert_eq!(after.pid, Some(third_pid));

        // A disabled service may be started at runtime, and stopped again.
        let optional = service(client.start("optional").await.unwrap());
        assert_eq!(optional.state, State::Running);
        assert!(!optional.enabled);
        let optional = service(client.stop("optional").await.unwrap());
        assert_eq!(optional.state, State::Disabled);

        // Malformed input.
        assert_eq!(
            error_code(raw(&socket, b"{\"op\":\"reboot\"}\n").await),
            ErrorCode::BadRequest
        );
        let long = vec![b'x'; control::MAX_LINE + 10];
        assert_eq!(error_code(raw(&socket, &long).await), ErrorCode::TooLong);

        // Several requests on one connection, and many clients in a row.
        let stream = UnixStream::connect(&socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        write
            .write_all(b"{\"op\":\"status\"}\n{\"op\":\"status\"}\n")
            .await
            .unwrap();
        let mut lines = BufReader::new(read).lines();
        for _ in 0..2 {
            let line = lines.next_line().await.unwrap().unwrap();
            assert!(matches!(
                serde_json::from_str(&line).unwrap(),
                Response::Services { .. }
            ));
        }
        for _ in 0..20 {
            status(&client).await;
        }

        root.ask(Shutdown).await.unwrap();
        assert_eq!(
            error_code(client.start("sleeper").await.unwrap()),
            ErrorCode::ShuttingDown
        );
        drop(server);
        assert!(!socket.exists(), "socket removed on shutdown");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_to_replace_a_regular_file() {
        let path = std::env::temp_dir().join(format!(
            "anyka-supervisor-not-a-socket-{}",
            std::process::id()
        ));
        std::fs::write(&path, b"keep me").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let sink = LogSink::discarding();
            let root = Supervisor::spawn((Vec::new(), sink));
            assert!(matches!(serve(&path, root), Err(Error::NotASocket { .. })));
        });
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
        let _ = std::fs::remove_file(&path);
    }
}
