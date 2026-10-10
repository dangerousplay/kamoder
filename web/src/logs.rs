//! Log files: list, tail, and live stream over SSE (T13, D7).
//!
//! The supervisor writes `logs/<service>.log` and rotates it to `.1`, `.2`…
//! (`file-rotate`'s `AppendCount`); `init.sh` adds `init.log` and `boot.log`.
//! Only names that are **currently entries of that directory** and regular
//! files (not symlinks, opened with `O_NOFOLLOW`) are served, so a request
//! can never name a path outside it.
//!
//! The live stream polls the file's size every [`Tuning::log_poll`] (inotify
//! is not reliable on FAT) and sends what was appended as one SSE event whose
//! `data:` lines are the new log lines. It starts at the current end: the UI
//! fetches the tail first. Rotation is followed without loss: when the path
//! names a new inode, whatever was appended to the old file is drained from
//! the still-open descriptor before switching to the new file from its start;
//! a file truncated in place is read again from 0.
//!
//! [`Tuning::log_poll`]: crate::state::Tuning::log_poll

use crate::auth::{AuthUser, StreamUser};
use crate::error::ApiError;
use crate::state::AppState;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{Instant, UNIX_EPOCH};

/// Lines returned by a tail unless asked otherwise.
pub const DEFAULT_TAIL: usize = 500;
/// Most lines a tail returns.
pub const MAX_TAIL: usize = 5000;
/// Most bytes a tail reads, whatever the line count: 5000 very long lines
/// must not become megabytes of RAM.
pub const MAX_TAIL_BYTES: u64 = 1024 * 1024;
/// Read size when walking a file backwards.
const CHUNK: u64 = 64 * 1024;
/// Most bytes one stream poll reads; a burst is caught up over several polls.
const MAX_POLL_BYTES: u64 = 256 * 1024;
/// A "line" without a newline is sent anyway once it gets this long.
const MAX_PARTIAL: usize = 16 * 1024;
/// Longest accepted file name.
const MAX_NAME: usize = 128;

/// Routes under `/api`. (`POST /logs/stream-token` lives in `session`.)
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/logs", get(list))
        .route("/logs/{file}", get(tail))
        .route("/logs/{file}/stream", get(stream))
}

/// One file, as `api.ts` `LogFile`.
#[derive(Debug, Clone, Serialize)]
pub struct LogFile {
    pub name: String,
    pub size: u64,
    /// Unix seconds.
    pub modified: Option<u64>,
    /// A rotated generation (`sshd.log.1`), not the live file.
    pub rotated: bool,
}

/// `api.ts` `LogTail`.
#[derive(Debug, Clone, Serialize)]
pub struct LogTail {
    pub file: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct TailQuery {
    lines: Option<usize>,
}

/// Regular files in the log directory, sorted by name. A missing directory
/// is an empty list.
pub fn list_dir(dir: &std::path::Path) -> Vec<LogFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<LogFile> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !plausible_name(&name) {
                return None;
            }
            // `DirEntry::metadata` does not follow symlinks.
            let metadata = entry.metadata().ok()?;
            if !metadata.file_type().is_file() {
                return None;
            }
            Some(LogFile {
                rotated: is_rotated(&name),
                size: metadata.len(),
                modified: metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
                name,
            })
        })
        .collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    files
}

/// Cheap shape check before looking at the directory at all.
fn plausible_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('.')
        && !name.ends_with(".tmp")
        && !name.contains(['/', '\\', '\0'])
}

/// `x.log.3` → rotated.
fn is_rotated(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, suffix)| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
}

/// The path of `name` if it is a regular file currently in the log directory.
fn resolve(state: &AppState, name: &str) -> Result<PathBuf, ApiError> {
    let missing = || ApiError::not_found(format!("no log file named {name:?}"));
    if !plausible_name(name) {
        return Err(missing());
    }
    let dir = &state.config.logs;
    let listed = std::fs::read_dir(dir)
        .map_err(|_| missing())?
        .flatten()
        .any(|entry| entry.file_name().to_str() == Some(name));
    if !listed {
        return Err(missing());
    }
    Ok(dir.join(name))
}

/// Opens a log file for reading without following a symlink.
fn open(path: &std::path::Path) -> std::io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

/// `GET /api/logs` → `[LogFile]`.
async fn list(State(state): State<AppState>, _: AuthUser) -> Result<Json<Vec<LogFile>>, ApiError> {
    let dir = state.config.logs.clone();
    let files = tokio::task::spawn_blocking(move || list_dir(&dir))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(files))
}

/// `GET /api/logs/{file}?lines=N` → `LogTail`: the last N (default 500, at
/// most 5000) lines.
async fn tail(
    State(state): State<AppState>,
    _: AuthUser,
    Path(name): Path<String>,
    Query(query): Query<TailQuery>,
) -> Result<Json<LogTail>, ApiError> {
    let count = query.lines.unwrap_or(DEFAULT_TAIL).min(MAX_TAIL);
    let path = resolve(&state, &name)?;
    let lines = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<String>> {
        let mut file = open(&path)?;
        tail_lines(&mut file, count, MAX_TAIL_BYTES)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(|error| ApiError::not_found(format!("cannot read {name:?}: {error}")))?;
    Ok(Json(LogTail { file: name, lines }))
}

/// The last `count` lines of `file`, reading backwards in [`CHUNK`]s and at
/// most `max_bytes`. A line cut by the byte limit is dropped.
pub fn tail_lines<R: Read + Seek>(
    file: &mut R,
    count: usize,
    max_bytes: u64,
) -> std::io::Result<Vec<String>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let len = file.seek(SeekFrom::End(0))?;
    let mut pos = len;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut newlines = 0usize;
    let mut first = true;
    // Stop once there are more line breaks than lines wanted: the text after
    // the (count+1)-th last newline is complete. The file's final newline
    // terminates the last line and does not start a new one.
    while pos > 0 && len - pos < max_bytes {
        let size = CHUNK.min(pos).min(max_bytes - (len - pos));
        pos -= size;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0; size as usize];
        file.read_exact(&mut chunk)?;
        let mut body: &[u8] = &chunk;
        if first {
            first = false;
            if let Some(stripped) = body.strip_suffix(b"\n") {
                body = stripped;
            }
        }
        newlines += body.iter().filter(|&&b| b == b'\n').count();
        chunks.push(chunk);
        if newlines >= count {
            break;
        }
    }
    let mut data = Vec::with_capacity(chunks.iter().map(Vec::len).sum());
    for chunk in chunks.iter().rev() {
        data.extend_from_slice(chunk);
    }
    drop(chunks);
    let mut text: &[u8] = &data;
    if let Some(stripped) = text.strip_suffix(b"\n") {
        text = stripped;
    }
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
    // Not read from the start of the file: the first piece is (probably) the
    // end of a longer line, unless there are more lines than wanted anyway.
    if pos > 0 && lines.len() <= count {
        lines.remove(0);
    }
    let skip = lines.len().saturating_sub(count);
    Ok(lines[skip..].iter().map(|line| decode(line)).collect())
}

fn decode(line: &[u8]) -> String {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    String::from_utf8_lossy(line).into_owned()
}

/// Follows one file across appends, truncation and rotation.
pub struct Tailer {
    path: PathBuf,
    file: Option<File>,
    ino: u64,
    pos: u64,
    partial: Vec<u8>,
}

impl Tailer {
    /// Opens `path` positioned at its current end.
    pub fn at_end(path: PathBuf) -> std::io::Result<Self> {
        let file = open(&path)?;
        let metadata = file.metadata()?;
        Ok(Self {
            path,
            ino: metadata.ino(),
            pos: metadata.len(),
            file: Some(file),
            partial: Vec::new(),
        })
    }

    /// Complete lines appended since the last call.
    pub fn poll(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        self.drain(&mut lines);
        match std::fs::symlink_metadata(&self.path) {
            // Rotated: the old file was drained above; start the new one at 0.
            Ok(metadata) if metadata.ino() != self.ino || self.file.is_none() => {
                if let Ok(file) = open(&self.path) {
                    self.flush_partial(&mut lines);
                    self.ino = metadata.ino();
                    self.file = Some(file);
                    self.pos = 0;
                    self.drain(&mut lines);
                }
            }
            Ok(_) => {}
            // Between the rename and the new file: try again next poll.
            Err(_) => {}
        }
        lines
    }

    /// Reads what was appended to the open descriptor.
    fn drain(&mut self, lines: &mut Vec<String>) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let Ok(len) = file.metadata().map(|m| m.len()) else {
            return;
        };
        if len < self.pos {
            // Truncated in place (`> file`): what is there now is new.
            self.pos = 0;
            self.partial.clear();
        }
        if len == self.pos {
            return;
        }
        let want = (len - self.pos).min(MAX_POLL_BYTES);
        let mut buf = vec![0; want as usize];
        let read = file
            .seek(SeekFrom::Start(self.pos))
            .and_then(|_| file.read(&mut buf));
        let Ok(read) = read else {
            return;
        };
        self.pos += read as u64;
        self.partial.extend_from_slice(&buf[..read]);
        while let Some(end) = self.partial.iter().position(|&b| b == b'\n') {
            lines.push(decode(&self.partial[..end]));
            self.partial.drain(..=end);
        }
        if self.partial.len() >= MAX_PARTIAL {
            self.flush_partial(lines);
        }
    }

    fn flush_partial(&mut self, lines: &mut Vec<String>) {
        if !self.partial.is_empty() {
            lines.push(decode(&self.partial));
            self.partial.clear();
        }
    }
}

/// One SSE event carrying `lines`, one `data:` field per line.
fn event(lines: &[String]) -> Bytes {
    let mut out = String::new();
    for line in lines {
        out.push_str("data: ");
        // A lone CR would end the field early; the viewer does not need it.
        out.extend(line.chars().map(|c| if c == '\r' { ' ' } else { c }));
        out.push('\n');
    }
    out.push('\n');
    Bytes::from(out)
}

/// `GET /api/logs/{file}/stream` (stream token allowed) → `text/event-stream`
/// of lines appended from now on. 503 `too_many_streams` beyond
/// [`crate::state::MAX_LOG_STREAMS`]; 404 for an unknown file.
async fn stream(
    State(state): State<AppState>,
    StreamUser(user): StreamUser,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let path = resolve(&state, &name)?;
    let permit = state.log_streams.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "too_many_streams",
            format!(
                "too many live log streams (at most {}); close another viewer",
                crate::state::MAX_LOG_STREAMS
            ),
        )
        .retry_after(std::time::Duration::from_secs(15))
    })?;
    let tailer = tokio::task::spawn_blocking(move || Tailer::at_end(path))
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError::not_found(format!("cannot open {name:?}: {error}")))?;
    tracing::debug!(user = %user.user, file = %name, "log stream opened");

    struct Stream {
        tailer: Option<Tailer>,
        _permit: tokio::sync::OwnedSemaphorePermit,
        shutdown: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        poll: std::time::Duration,
        keepalive: std::time::Duration,
        last_sent: Instant,
        opened: bool,
    }
    let initial = Stream {
        tailer: Some(tailer),
        _permit: permit,
        shutdown: Box::pin(state.shutdown_signal()),
        poll: state.tuning.log_poll,
        keepalive: state.tuning.keepalive,
        last_sent: Instant::now(),
        opened: false,
    };
    let body = futures_util::stream::unfold(initial, |mut s| async move {
        if !s.opened {
            // Sent at once so the browser's EventSource sees the open.
            s.opened = true;
            return Some((
                Ok::<_, std::convert::Infallible>(Bytes::from_static(b": open\n\n")),
                s,
            ));
        }
        loop {
            tokio::select! {
                () = &mut s.shutdown => return None,
                () = tokio::time::sleep(s.poll) => {}
            }
            let mut tailer = s.tailer.take()?;
            let (tailer, lines) = tokio::task::spawn_blocking(move || {
                let lines = tailer.poll();
                (tailer, lines)
            })
            .await
            .ok()?;
            s.tailer = Some(tailer);
            if !lines.is_empty() {
                s.last_sent = Instant::now();
                return Some((Ok(event(&lines)), s));
            }
            if s.last_sent.elapsed() >= s.keepalive {
                s.last_sent = Instant::now();
                return Some((Ok(Bytes::from_static(b": keepalive\n\n")), s));
            }
        }
    });
    Ok((
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(body),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn tail(text: &str, count: usize, max: u64) -> Vec<String> {
        tail_lines(&mut Cursor::new(text.as_bytes().to_vec()), count, max).unwrap()
    }

    #[test]
    fn tail_counts_lines_from_the_end() {
        assert_eq!(tail("a\nb\nc\n", 2, 1 << 20), ["b", "c"]);
        assert_eq!(tail("a\nb\nc", 2, 1 << 20), ["b", "c"]);
        assert_eq!(tail("a\nb\nc\n", 10, 1 << 20), ["a", "b", "c"]);
        assert_eq!(tail("", 10, 1 << 20), Vec::<String>::new());
        assert_eq!(tail("\n\n", 10, 1 << 20), ["", ""]);
        assert_eq!(tail("a\r\nb\r\n", 1, 1 << 20), ["b"]);
        assert_eq!(tail("a\n\nb\n", 2, 1 << 20), ["", "b"]);
    }

    #[test]
    fn tail_spans_chunks_and_respects_the_byte_cap() {
        let text: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        let lines = tail(&text, 5000, 1 << 20);
        assert_eq!(lines.len(), 5000);
        assert_eq!(lines[0], "line 15000");
        assert_eq!(lines[4999], "line 19999");
        // 100 bytes hold 11 whole "line 1999x\n" lines at most; the cut one is dropped.
        let lines = tail(&text, 5000, 100);
        assert!(lines.len() < 11, "{lines:?}");
        assert_eq!(lines.last().unwrap(), "line 19999");
        assert!(lines.iter().all(|l| l.starts_with("line 199")));
    }

    #[test]
    fn names_and_rotation_suffixes() {
        assert!(is_rotated("sshd.log.1"));
        assert!(!is_rotated("sshd.log"));
        assert!(!plausible_name("../etc/passwd"));
        assert!(!plausible_name(".hidden"));
        assert!(!plausible_name("x.log.tmp"));
        assert!(plausible_name("init.log"));
    }

    #[test]
    fn tailer_follows_appends_truncation_and_rotation() {
        let dir = std::env::temp_dir().join(format!("anyka-web-tailer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("svc.log");
        std::fs::write(&path, "old\n").unwrap();
        let mut tailer = Tailer::at_end(path.clone()).unwrap();
        assert!(tailer.poll().is_empty());

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"one\ntw").unwrap();
        assert_eq!(tailer.poll(), ["one"]);
        file.write_all(b"o\n").unwrap();
        assert_eq!(tailer.poll(), ["two"]);

        // Rotation: a last line lands in the old file after the rename, and
        // the new file already has content. Neither is lost or repeated.
        std::fs::rename(&path, dir.join("svc.log.1")).unwrap();
        file.write_all(b"last of old\n").unwrap();
        std::fs::write(&path, "first of new\n").unwrap();
        assert_eq!(tailer.poll(), ["last of old", "first of new"]);
        assert!(tailer.poll().is_empty());

        // Truncation in place.
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap()
            .write_all(b"x\n")
            .unwrap();
        assert_eq!(tailer.poll(), ["x"]);
    }

    #[test]
    fn events_have_one_data_field_per_line() {
        assert_eq!(
            &event(&["a".into(), "b\rc".into()])[..],
            b"data: a\ndata: b c\n\n"
        );
    }
}
