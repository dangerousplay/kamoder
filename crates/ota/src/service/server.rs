//! The control socket (`/tmp/kamoder-update.sock`, [`update_proto`]).
//!
//! Mirrors the follow and lights sockets: mode 0600, a stale socket from a
//! previous run is replaced (any other file is not), one task per
//! connection, at most `MAX_CONNECTIONS` (8), lines capped at [`MAX_LINE`],
//! idle connections closed. Requests are carried out by
//! [`Service::handle`]; a `check` can take a while, which only holds up its
//! own connection.

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use metrics::counter;
use snafu::{ResultExt, Snafu};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use update_proto::{code, Request, Response, MAX_LINE};

use super::source::Source;
use super::Service;

/// Connections served at once; `web` is the expected client.
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
                tracing::warn!(path = %self.path.display(), %error, "cannot remove update socket");
            }
        }
    }
}

/// Binds `path` and starts accepting in the background.
pub fn serve<S: Source>(path: &Path, service: Arc<Service<S>>) -> Result<Server, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path).context(RemoveStaleSnafu { path })?;
        }
        Ok(_) => return NotASocketSnafu { path }.fail(),
        Err(_) => {}
    }
    let listener = UnixListener::bind(path).context(BindSnafu { path })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .context(PermissionsSnafu { path })?;
    tracing::info!(path = %path.display(), "update socket listening");

    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let accept = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::warn!(%error, "update socket accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                tracing::warn!("too many update socket connections, refusing one");
                drop(stream);
                continue;
            };
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                if let Err(error) = connection(stream, &service).await {
                    tracing::debug!(%error, "update connection ended with an error");
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
async fn connection<S: Source>(
    stream: UnixStream,
    service: &Arc<Service<S>>,
) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut buf = Vec::with_capacity(128);
    loop {
        buf.clear();
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
            error(code::TOO_LONG, format!("request exceeds {MAX_LINE} bytes"))
        } else if !complete {
            return Ok(());
        } else {
            match serde_json::from_slice::<Request>(&buf) {
                Ok(request) => service.handle(request).await,
                Err(e) => {
                    counter!("kamoder_update_socket_requests_total", "op" => "invalid", "result" => code::BAD_REQUEST).increment(1);
                    error(code::BAD_REQUEST, e.to_string())
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

fn error(code: &str, message: impl Into<String>) -> Response {
    Response::Error {
        code: code.to_owned(),
        message: message.into(),
    }
}
