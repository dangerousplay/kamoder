//! Server side of the PTZ ownership socket (protocol in
//! [`ptz_proto`]) — how web reaches the motor this process
//! owns (design D4).
//!
//! Mirrors the supervisor's control socket: mode 0600, a stale socket from a
//! previous run is replaced (any other file is not), one task per
//! connection, at most [`MAX_CONNECTIONS`], lines capped at
//! [`MAX_LINE`](ptz_proto::MAX_LINE), idle connections closed.
//!
//! The protocol crate deliberately does not depend on `anyka-ptz` (so the
//! web UI never links the SDK), so the translation from backend values and
//! errors to wire types lives here, the one place that has both.

use crate::ptz::Ptz;
use metrics::counter;
use ptz_proto::{
    ErrorCode, Request, Response, DEFAULT_MOVE_TIMEOUT_MS, MAX_LINE, MAX_MOVE_TIMEOUT_MS,
};
use snafu::{ResultExt, Snafu};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

/// Connections served at once; web is the only expected client.
const MAX_CONNECTIONS: usize = 8;

/// A connection idle this long between requests is closed.
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
                tracing::warn!(path = %self.path.display(), %error, "cannot remove PTZ socket");
            }
        }
    }
}

/// Binds `path` and starts accepting in the background.
pub fn serve(path: &Path, ptz: Arc<Ptz>) -> Result<Server, Error> {
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
    tracing::info!(path = %path.display(), "PTZ socket listening");

    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let accept = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::warn!(%error, "PTZ socket accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                tracing::warn!("too many PTZ socket connections, refusing one");
                counter!("kamoder_onvif_ptz_socket_refused_total").increment(1);
                drop(stream);
                continue;
            };
            let ptz = Arc::clone(&ptz);
            tokio::spawn(async move {
                if let Err(error) = connection(stream, &ptz).await {
                    tracing::debug!(%error, "PTZ connection ended with an error");
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
async fn connection(stream: UnixStream, ptz: &Ptz) -> std::io::Result<()> {
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
                Ok(request) => handle(ptz, request),
                Err(e) => {
                    counter!("kamoder_onvif_ptz_socket_requests_total", "op" => "invalid", "result" => "bad_request").increment(1);
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

/// The wire code for a backend failure.
fn error_code(error: &anyka_ptz::Error) -> ErrorCode {
    use anyka_ptz::Error as E;
    match error {
        E::InvalidArgument { .. } => ErrorCode::InvalidArgument,
        E::UnknownPreset { .. } => ErrorCode::UnknownPreset,
        E::TooManyPresets { .. } => ErrorCode::TooManyPresets,
        E::Unsupported { .. } => ErrorCode::Unsupported,
        _ => ErrorCode::Backend,
    }
}

/// The error response for a backend failure.
fn backend_error(e: &anyka_ptz::Error) -> Response {
    error(error_code(e), e.to_string())
}

fn wire_limits(l: anyka_ptz::Limits) -> ptz_proto::Limits {
    ptz_proto::Limits {
        pan_min_deg: l.pan_min_deg,
        pan_max_deg: l.pan_max_deg,
        tilt_min_deg: l.tilt_min_deg,
        tilt_max_deg: l.tilt_max_deg,
    }
}

fn wire_position(p: anyka_ptz::Position) -> ptz_proto::Position {
    ptz_proto::Position {
        pan_deg: p.pan_deg,
        tilt_deg: p.tilt_deg,
        moving: p.moving,
    }
}

fn wire_preset(p: anyka_ptz::Preset) -> ptz_proto::Preset {
    ptz_proto::Preset {
        token: p.token,
        name: p.name,
        pan_deg: p.pan_deg,
        tilt_deg: p.tilt_deg,
    }
}

fn error(code: ErrorCode, message: impl Into<String>) -> Response {
    Response::Error {
        code,
        message: message.into(),
    }
}

/// Carries out one request.
pub fn handle(ptz: &Ptz, request: Request) -> Response {
    let op = request.op();
    let response = dispatch(ptz, request);
    let result = match &response {
        Response::Error { code, .. } => code.as_str(),
        _ => "ok",
    };
    counter!("kamoder_onvif_ptz_socket_requests_total", "op" => op, "result" => result)
        .increment(1);
    response
}

fn dispatch(ptz: &Ptz, request: Request) -> Response {
    let done = |outcome: anyka_ptz::Result<()>| match outcome {
        Ok(()) => Response::Ok,
        Err(e) => backend_error(&e),
    };
    match request {
        Request::Status => Response::Status {
            backend: ptz.name().to_owned(),
            limits: wire_limits(ptz.limits()),
            position: wire_position(ptz.position()),
        },
        Request::Move {
            pan,
            tilt,
            timeout_ms,
        } => {
            let timeout_ms = timeout_ms.unwrap_or(DEFAULT_MOVE_TIMEOUT_MS);
            if timeout_ms == 0 {
                return error(ErrorCode::InvalidArgument, "timeout_ms must be positive");
            }
            let timeout = Duration::from_millis(timeout_ms.min(MAX_MOVE_TIMEOUT_MS).into());
            done(ptz.continuous_move(pan, tilt, Some(timeout)))
        }
        Request::Stop => done(ptz.stop()),
        Request::Goto { pan, tilt } => done(ptz.goto(pan, tilt)),
        Request::Presets => Response::Presets {
            presets: ptz.presets().into_iter().map(wire_preset).collect(),
        },
        Request::SetPreset { name, token } => match ptz.set_preset(&name, token.as_deref()) {
            Ok(preset) => Response::Preset {
                preset: wire_preset(preset),
            },
            Err(e) => backend_error(&e),
        },
        Request::GotoPreset { token } => done(ptz.goto_preset(&token)),
        Request::RemovePreset { token } => done(ptz.remove_preset(&token)),
        Request::Home => done(ptz.home()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ptz_proto::Client;

    fn socket_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("anyka-onvif-ptz-{tag}-{}.sock", std::process::id()))
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

    fn position(response: Response) -> ptz_proto::Position {
        match response {
            Response::Status { position, .. } => position,
            other => panic!("expected status, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn socket_round_trip_and_deadman() {
        let path = socket_path("rt");
        // A stale socket from a previous run is replaced.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let ptz = crate::ptz::tests::sim();
        let server = serve(&path, Arc::clone(&ptz)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let client = Client::new(&path);

        let status = client.status().await.unwrap();
        let Response::Status {
            backend, limits, ..
        } = &status
        else {
            panic!("{status:?}")
        };
        assert_eq!(backend, "sim");
        assert_eq!(*limits, wire_limits(anyka_ptz::Limits::EV200));
        let start = position(status);
        assert!(!start.moving);

        // A move with a short deadman stops by itself.
        assert_eq!(
            client.move_(1.0, 0.0, Some(80)).await.unwrap(),
            Response::Ok
        );
        assert!(position(client.status().await.unwrap()).moving);
        tokio::time::sleep(Duration::from_millis(250)).await;
        let after = position(client.status().await.unwrap());
        assert!(!after.moving, "deadman stopped the move");
        assert!(after.pan_deg > start.pan_deg);

        // The default deadman (1 s) applies without timeout_ms; stop ends it.
        assert_eq!(client.move_(0.0, -1.0, None).await.unwrap(), Response::Ok);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(position(client.status().await.unwrap()).moving);
        assert_eq!(client.stop().await.unwrap(), Response::Ok);
        assert!(!position(client.status().await.unwrap()).moving);

        // Goto, presets.
        assert_eq!(client.goto(10.0, 10.0).await.unwrap(), Response::Ok);
        assert!(position(client.status().await.unwrap()).moving);
        let Response::Preset { preset } = client.set_preset("Door", None).await.unwrap() else {
            panic!()
        };
        assert_eq!(preset.name, "Door");
        let Response::Presets { presets } = client.presets().await.unwrap() else {
            panic!()
        };
        assert_eq!(presets, vec![preset.clone()]);
        assert_eq!(
            client.goto_preset(&preset.token).await.unwrap(),
            Response::Ok
        );
        assert_eq!(
            code(client.goto_preset("nope").await.unwrap()),
            ErrorCode::UnknownPreset
        );
        assert_eq!(
            client.remove_preset(&preset.token).await.unwrap(),
            Response::Ok
        );
        assert_eq!(client.home().await.unwrap(), Response::Ok);

        // Bad input.
        assert_eq!(
            code(client.move_(f32::NAN, 0.0, None).await.unwrap()),
            // NaN is not valid JSON: serde_json writes null, the server
            // refuses the line.
            ErrorCode::BadRequest
        );
        assert_eq!(
            code(client.move_(1.0, 0.0, Some(0)).await.unwrap()),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            code(raw(&path, b"{\"op\":\"reboot\"}\n").await),
            ErrorCode::BadRequest
        );
        let long = vec![b'x'; MAX_LINE + 10];
        assert_eq!(code(raw(&path, &long).await), ErrorCode::TooLong);

        // Several requests on one connection.
        let stream = UnixStream::connect(&path).await.unwrap();
        let (read, mut write) = stream.into_split();
        write
            .write_all(b"{\"op\":\"stop\"}\n{\"op\":\"presets\"}\n")
            .await
            .unwrap();
        let mut lines = BufReader::new(read).lines();
        let first: Response =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first, Response::Ok);
        let second: Response =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(second, Response::Presets { presets: vec![] });

        drop(server);
        assert!(!path.exists(), "socket removed on drop");
    }

    #[test]
    fn backend_errors_map_to_codes() {
        use anyka_ptz::Error as E;
        let unknown = E::UnknownPreset { token: "x".into() };
        assert_eq!(error_code(&unknown), ErrorCode::UnknownPreset);
        assert_eq!(
            error_code(&E::TooManyPresets { max: 32 }),
            ErrorCode::TooManyPresets
        );
        assert_eq!(error_code(&E::AlreadyOpen), ErrorCode::Backend);
        assert!(matches!(
            backend_error(&unknown),
            Response::Error {
                code: ErrorCode::UnknownPreset,
                ..
            }
        ));
    }

    #[test]
    fn wire_types_serialise_like_the_backend_types() {
        // The protocol crate mirrors anyka-ptz's types rather than using
        // them; the JSON must stay identical.
        let limits = anyka_ptz::Limits::EV200;
        let position = anyka_ptz::Position {
            pan_deg: 1.5,
            tilt_deg: 2.5,
            moving: true,
        };
        let preset = anyka_ptz::Preset {
            token: "1".into(),
            name: "Door".into(),
            pan_deg: 90.0,
            tilt_deg: 30.0,
        };
        use serde_json::to_value as json;
        assert_eq!(json(limits).unwrap(), json(wire_limits(limits)).unwrap());
        assert_eq!(
            json(position).unwrap(),
            json(wire_position(position)).unwrap()
        );
        assert_eq!(
            json(&preset).unwrap(),
            json(wire_preset(preset.clone())).unwrap()
        );
    }

    #[test]
    fn refuses_to_replace_a_regular_file() {
        let path = socket_path("file");
        std::fs::write(&path, b"keep me").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let ptz = crate::ptz::tests::sim();
            assert!(matches!(serve(&path, ptz), Err(Error::NotASocket { .. })));
        });
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
        let _ = std::fs::remove_file(&path);
    }
}
