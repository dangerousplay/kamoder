//! The audio socket (`/tmp/kamoder-audio.sock`, [`audio_proto`], D-A4).
//!
//! Like the lights and follow sockets: mode 0600, a stale socket from a
//! previous run is replaced (any other file is not), one task per
//! connection, at most [`MAX_CONNECTIONS`], request lines capped at
//! [`MAX_LINE`]. `status`/`set` may repeat on one connection; `capture` and
//! `play` turn it into a frame stream for good:
//!
//! - **capture**: frames as the microphone produces them. A client that
//!   stops reading loses frames (the broadcast drops them for it alone) and
//!   is disconnected after [`WRITE_TIMEOUT`] of a full socket.
//! - **play**: frames from the client until it closes, or sends nothing for
//!   [`TALK_IDLE`] (so a hung client cannot hold the speaker), or sends a
//!   header that cannot be a frame (counted, the stream is out of sync).

use crate::runtime::{Refused, Runtime};
use audio_proto::{
    check_payload_len, ErrorCode, Frame, Request, Response, StreamFormat, Talker, MAX_LINE,
};
use metrics::counter;
use snafu::{ResultExt, Snafu};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use video_proto::{PacketHeader, HEADER_LEN};

/// Connections served at once: rtsp, web listen + talk, onvif, a tool.
pub const MAX_CONNECTIONS: usize = 8;

/// A connection idle this long between requests is closed.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// A talker silent this long is disconnected, releasing the speaker.
pub const TALK_IDLE: Duration = Duration::from_secs(10);

/// A capture client whose socket stays full this long is disconnected.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

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
                tracing::warn!(path = %self.path.display(), %error, "cannot remove audio socket");
            }
        }
    }
}

/// Binds `path` and starts accepting in the background.
pub fn serve(path: &Path, runtime: Runtime) -> Result<Server, Error> {
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
    tracing::info!(path = %path.display(), "audio socket listening");

    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let accept = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::warn!(%error, "audio socket accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                tracing::warn!("too many audio socket connections, refusing one");
                counter!("kamoder_audio_socket_refused_total").increment(1);
                drop(stream);
                continue;
            };
            let runtime = runtime.clone();
            tokio::spawn(async move {
                if let Err(error) = connection(stream, &runtime).await {
                    tracing::debug!(%error, "audio connection ended with an error");
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

fn count(op: &'static str, result: &'static str) {
    counter!("kamoder_audio_socket_requests_total", "op" => op, "result" => result).increment(1);
}

fn refused(op: &'static str, r: Refused) -> Response {
    count(op, r.code.as_str());
    Response::Error {
        code: r.code,
        message: r.message,
    }
}

async fn reply(write: &mut OwnedWriteHalf, response: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(response).map_err(std::io::Error::other)?;
    line.push(b'\n');
    write.write_all(&line).await
}

/// Serves one connection: requests until it streams or closes.
async fn connection(stream: UnixStream, runtime: &Runtime) -> std::io::Result<()> {
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
        if buf.len() > MAX_LINE + usize::from(complete) {
            count("invalid", "bad_request");
            let response = Response::Error {
                code: ErrorCode::BadRequest,
                message: format!("request exceeds {MAX_LINE} bytes"),
            };
            return reply(&mut write, &response).await;
        }
        if !complete {
            return Ok(());
        }
        let request = match serde_json::from_slice::<Request>(&buf) {
            Ok(request) => request,
            Err(e) => {
                count("invalid", "bad_request");
                let response = Response::Error {
                    code: ErrorCode::BadRequest,
                    message: e.to_string(),
                };
                reply(&mut write, &response).await?;
                continue;
            }
        };
        let response = match request {
            Request::Status => {
                count("status", "ok");
                Response::Status {
                    devices: runtime.status(),
                }
            }
            Request::Set { device, gain } => match runtime.set(&device, gain) {
                Ok(()) => {
                    count("set", "ok");
                    Response::Ok
                }
                Err(r) => refused("set", r),
            },
            Request::Capture => return capture(reader, write, runtime).await,
            Request::Play { client } => return play(reader, write, runtime, client).await,
        };
        reply(&mut write, &response).await?;
    }
}

async fn capture(
    mut reader: BufReader<OwnedReadHalf>,
    mut write: OwnedWriteHalf,
    runtime: &Runtime,
) -> std::io::Result<()> {
    let mut subscription = match runtime.subscribe() {
        Ok(subscription) => subscription,
        Err(r) => return reply(&mut write, &refused("capture", r)).await,
    };
    count("capture", "ok");
    reply(&mut write, &Response::Streaming(StreamFormat::DEFAULT)).await?;
    let mut out = Vec::with_capacity(HEADER_LEN + audio_proto::FRAME_BYTES);
    let mut scratch = [0u8; 64];
    loop {
        tokio::select! {
            frame = subscription.recv() => {
                let Some(frame) = frame else { return Ok(()) };
                out.clear();
                frame.encode_into(&mut out);
                match tokio::time::timeout(WRITE_TIMEOUT, write.write_all(&out)).await {
                    Ok(result) => result?,
                    Err(_) => {
                        tracing::info!("capture client stopped reading; disconnecting it");
                        return Ok(());
                    }
                }
            }
            // The client sends nothing; this notices it closing while the
            // microphone is silent (no frames to fail on).
            read = reader.read(&mut scratch) => {
                if read? == 0 {
                    return Ok(());
                }
            }
        }
    }
}

async fn play(
    mut reader: BufReader<OwnedReadHalf>,
    mut write: OwnedWriteHalf,
    runtime: &Runtime,
    client: Talker,
) -> std::io::Result<()> {
    let talk = match runtime.talk(client).await {
        Ok(talk) => talk,
        Err(r) => return reply(&mut write, &refused("play", r)).await,
    };
    count("play", "ok");
    reply(&mut write, &Response::Streaming(StreamFormat::DEFAULT)).await?;
    let mut payload = Vec::with_capacity(audio_proto::MAX_FRAME_BYTES);
    loop {
        match tokio::time::timeout(TALK_IDLE, read_frame(&mut reader, &mut payload)).await {
            Err(_) => {
                tracing::info!(talker = %client, "talker silent too long; releasing the speaker");
                break;
            }
            Ok(Ok(Some(frame))) => talk.play(frame.samples),
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                if error.kind() == std::io::ErrorKind::InvalidData {
                    counter!("kamoder_audio_talk_bad_frames_total").increment(1);
                    tracing::info!(talker = %client, %error, "bad frame from talker; closing");
                }
                break;
            }
        }
    }
    drop(talk);
    Ok(())
}

/// The next frame; `None` on a close between frames.
async fn read_frame(
    reader: &mut BufReader<OwnedReadHalf>,
    payload: &mut Vec<u8>,
) -> std::io::Result<Option<Frame>> {
    if reader.fill_buf().await?.is_empty() {
        return Ok(None);
    }
    let mut head = [0u8; HEADER_LEN];
    reader.read_exact(&mut head).await?;
    let header = PacketHeader::decode(&head);
    check_payload_len(header.len as usize)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    payload.resize(header.len as usize, 0);
    reader.read_exact(payload).await?;
    Frame::from_parts(&header, payload)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amp::Amp;
    use crate::fake::FakeBackend;
    use crate::runtime::Settings;
    use crate::test_support::TempDir;

    fn board() -> board::Audio {
        crate::runtime::tests::board(16000)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn socket_is_private_and_replaces_only_sockets() {
        let dir = TempDir::new("server");
        let path = dir.path().join("audio.sock");
        let runtime = Runtime::start(
            &board(),
            Arc::new(FakeBackend::default()),
            Amp::Driver,
            Settings::default(),
            None,
        )
        .unwrap();
        let server = serve(&path, runtime.clone()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // A stale socket from a previous run is replaced.
        drop(server);
        assert!(!path.exists());
        let _stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = serve(&path, runtime.clone()).unwrap();
        drop(server);
        std::fs::write(&path, "x").unwrap();
        assert!(matches!(
            serve(&path, runtime),
            Err(Error::NotASocket { .. })
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bad_lines_and_repeated_requests() {
        let dir = TempDir::new("server-bad");
        let path = dir.path().join("audio.sock");
        let runtime = Runtime::start(
            &board(),
            Arc::new(FakeBackend::default()),
            Amp::Driver,
            Settings::default(),
            None,
        )
        .unwrap();
        let _server = serve(&path, runtime).unwrap();

        let stream = UnixStream::connect(&path).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        let mut ask = async |request: &[u8]| {
            write.write_all(request).await.unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            serde_json::from_str::<Response>(&line).ok()
        };
        assert!(matches!(
            ask(b"{\"op\":\"record\"}\n").await,
            Some(Response::Error {
                code: ErrorCode::BadRequest,
                ..
            })
        ));
        // The connection survives a bad request and serves several.
        assert!(matches!(
            ask(b"{\"op\":\"status\"}\n").await,
            Some(Response::Status { .. })
        ));
        assert_eq!(
            ask(b"{\"op\":\"set\",\"device\":\"spk0\",\"gain\":2}\n").await,
            Some(Response::Ok)
        );
        // An over-long line is answered, then the connection closes.
        let long = vec![b' '; MAX_LINE + 10];
        assert!(matches!(
            ask(&long).await,
            Some(Response::Error {
                code: ErrorCode::BadRequest,
                ..
            })
        ));
        line.clear();
        assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bad_frame_ends_the_talk() {
        let dir = TempDir::new("server-frame");
        let path = dir.path().join("audio.sock");
        let fake = FakeBackend::default();
        let runtime = Runtime::start(
            &board(),
            Arc::new(fake.clone()),
            Amp::Driver,
            Settings::default(),
            None,
        )
        .unwrap();
        let _server = serve(&path, runtime).unwrap();

        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream
            .write_all(b"{\"op\":\"play\",\"client\":\"other\"}\n")
            .await
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.contains("streaming"), "{line}");
        // A header announcing an odd payload: the stream is out of sync.
        let header = PacketHeader {
            len: 3,
            keyframe: false,
            timestamp_ms: 0,
        };
        reader.get_mut().write_all(&header.encode()).await.unwrap();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
        // The speaker is free again.
        let client = audio_proto::Client::new(&path);
        let sink = loop {
            match client.play(Talker::Web).await {
                Ok(sink) => break sink,
                Err(audio_proto::ClientError::Busy { .. }) => {
                    tokio::time::sleep(Duration::from_millis(5)).await
                }
                Err(e) => panic!("{e}"),
            }
        };
        sink.finish().await.unwrap();
    }
}
