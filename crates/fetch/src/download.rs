//! Resumable, verified downloads to a file (OTA-15).
//!
//! ```text
//! <dest>.part exists? ── hash its prefix (N bytes)
//!   N == expected size and hash ok ──────────────► rename, no network
//!   GET  Range: bytes=N-   (redirects re-resolved every attempt)
//!     206, Content-Range start == N ── append
//!     200 ─────────────────────────── truncate, start from 0
//!     416, "bytes */N" ────────────── already complete
//!   stream: ≤ 64 KiB chunk → SHA-256 → file   (rate capped)
//!   verify size + SHA-256 → fsync → rename .part → dest → fsync dir
//! ```
//!
//! The prefix is re-hashed from disk on every attempt rather than trusting
//! in-memory state, so power loss, a killed process or a torn last write all
//! resume correctly: whatever reached the SD card is what gets hashed. A size
//! or hash mismatch deletes the `.part` so the next try starts clean.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hyper::header::{CONTENT_RANGE, RANGE};
use hyper::{HeaderMap, StatusCode};
use ring::digest::{Context, SHA256};
use snafu::ResultExt;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::{debug, info, warn};

use crate::error::{Error, IoSnafu, Result};
use crate::{content_length, Fetcher, READ_BUF};

/// What the downloaded file must be (from a verified manifest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected {
    pub size: u64,
    pub sha256: [u8; 32],
}

/// A finished download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Downloaded {
    pub size: u64,
    pub sha256: [u8; 32],
    /// Bytes already on disk from an earlier attempt and reused.
    pub resumed_from: u64,
}

/// Progress report, sent after every chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Bytes in the `.part` file so far (including a resumed prefix).
    pub downloaded: u64,
    /// Full size when the server said.
    pub total: Option<u64>,
}

/// How [`Fetcher::download_with_retry`] retries transient errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts, including the first.
    pub attempts: u32,
    /// Wait before the second attempt; doubles each time.
    pub initial_backoff: Duration,
    /// Upper bound for the wait.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 6,
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(60),
        }
    }
}

/// `<dest>.part`.
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

impl Fetcher {
    /// Downloads `url` into `dest`, resuming `<dest>.part` if present. See
    /// the module docs for the flow. With `expected`, a wrong size or hash is
    /// an error and leaves neither `dest` nor `.part` behind.
    pub async fn download_to<P: FnMut(Progress)>(
        &self,
        url: &str,
        dest: &Path,
        expected: Option<Expected>,
        mut progress: P,
    ) -> Result<Downloaded> {
        let part = part_path(dest);
        let io = |source| Error::Io {
            path: part.clone(),
            source,
        };

        let (mut have, mut ctx) = hash_prefix(&part).await?;
        if let Some(exp) = expected {
            if have == exp.size && digest(&ctx) == exp.sha256 {
                debug!(path = %dest.display(), size = have, "already downloaded");
                finalize(&part, dest).await?;
                return Ok(Downloaded {
                    size: have,
                    sha256: exp.sha256,
                    resumed_from: have,
                });
            }
            if have >= exp.size {
                warn!(path = %part.display(), have, "stale partial download, restarting");
                have = 0;
                ctx = Context::new(&SHA256);
            }
        }
        let resumed_from = have;

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&part)
            .await
            .map_err(io)?;
        file.set_len(have).await.map_err(io)?;
        file.seek(std::io::SeekFrom::Start(have))
            .await
            .map_err(io)?;

        let mut restarted_on_416 = false;
        let (final_url, body, total) = loop {
            let mut headers = HeaderMap::new();
            if have > 0 {
                headers.insert(RANGE, format!("bytes={have}-").parse().expect("ascii"));
            }
            let (uri, resp) = self.send(url, &headers).await?;
            let final_url = uri.to_string();
            let status = resp.status();
            let content_range = resp
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            match status {
                StatusCode::PARTIAL_CONTENT => {
                    let cr = content_range.unwrap_or_default();
                    let Some((start, total)) = parse_content_range(&cr) else {
                        return Err(Error::BadContentRange {
                            url: final_url,
                            expected: have,
                            content_range: cr,
                        });
                    };
                    if start != have {
                        return Err(Error::BadContentRange {
                            url: final_url,
                            expected: have,
                            content_range: cr,
                        });
                    }
                    if have > 0 {
                        info!(url = %final_url, from = have, "resuming download");
                    }
                    let total = total.or_else(|| content_length(resp.headers()).map(|n| n + have));
                    break (final_url, Some(resp.into_body()), total);
                }
                StatusCode::OK => {
                    if have > 0 {
                        info!(url = %final_url, discarded = have, "server ignored Range, restarting");
                        have = 0;
                        ctx = Context::new(&SHA256);
                        file.set_len(0).await.map_err(io)?;
                        file.seek(std::io::SeekFrom::Start(0)).await.map_err(io)?;
                    }
                    let total = content_length(resp.headers());
                    break (final_url, Some(resp.into_body()), total);
                }
                StatusCode::RANGE_NOT_SATISFIABLE if have > 0 => {
                    let complete = content_range
                        .as_deref()
                        .and_then(|cr| cr.strip_prefix("bytes */"))
                        .and_then(|n| n.trim().parse::<u64>().ok())
                        == Some(have);
                    if complete {
                        debug!(url = %final_url, size = have, "server says the file is complete");
                        break (final_url, None, Some(have));
                    }
                    if restarted_on_416 {
                        return Err(Error::Http {
                            url: final_url,
                            status: status.as_u16(),
                        });
                    }
                    warn!(url = %final_url, have, "range not satisfiable, restarting");
                    restarted_on_416 = true;
                    have = 0;
                    ctx = Context::new(&SHA256);
                    file.set_len(0).await.map_err(io)?;
                    file.seek(std::io::SeekFrom::Start(0)).await.map_err(io)?;
                }
                _ => {
                    return Err(Error::Http {
                        url: final_url,
                        status: status.as_u16(),
                    })
                }
            }
        };

        if let (Some(exp), Some(total)) = (expected, total) {
            if total != exp.size {
                drop(file);
                discard(&part).await;
                return Err(Error::SizeMismatch {
                    expected: exp.size,
                    actual: total,
                });
            }
        }

        progress(Progress {
            downloaded: have,
            total,
        });
        let streamed: Result<()> = async {
            // `None` after a 416 that confirmed the file is complete.
            let Some(mut body) = body else { return Ok(()) };
            while let Some(chunk) = self.next_chunk(&final_url, &mut body).await? {
                let len = chunk.len() as u64;
                if let Some(exp) = expected {
                    if have + len > exp.size {
                        return Err(Error::SizeMismatch {
                            expected: exp.size,
                            actual: have + len,
                        });
                    }
                }
                ctx.update(&chunk);
                file.write_all(&chunk).await.map_err(io)?;
                have += len;
                progress(Progress {
                    downloaded: have,
                    total,
                });
            }
            Ok(())
        }
        .await;
        // Whatever was written must reach the file before a retry re-hashes it.
        let flushed = file.flush().await.map_err(io);
        if let Err(e) = streamed {
            if matches!(e, Error::SizeMismatch { .. }) {
                drop(file);
                discard(&part).await;
            }
            return Err(e);
        }
        flushed?;

        if let Some(total) = total {
            if have != total {
                return Err(Error::Incomplete {
                    url: final_url,
                    got: have,
                    expected: total,
                });
            }
        }

        let sha256 = digest(&ctx);
        if let Some(exp) = expected {
            let mismatch = if have != exp.size {
                Some(Error::SizeMismatch {
                    expected: exp.size,
                    actual: have,
                })
            } else if sha256 != exp.sha256 {
                Some(Error::HashMismatch {
                    expected: exp.sha256,
                    actual: sha256,
                })
            } else {
                None
            };
            if let Some(e) = mismatch {
                drop(file);
                discard(&part).await;
                return Err(e);
            }
        }

        file.sync_all().await.map_err(io)?;
        drop(file);
        finalize(&part, dest).await?;
        info!(path = %dest.display(), size = have, resumed_from, "download complete");
        Ok(Downloaded {
            size: have,
            sha256,
            resumed_from,
        })
    }

    /// [`Self::download_to`] with retries on transient errors, resuming each
    /// time, with exponential backoff.
    pub async fn download_with_retry<P: FnMut(Progress)>(
        &self,
        url: &str,
        dest: &Path,
        expected: Option<Expected>,
        policy: RetryPolicy,
        mut progress: P,
    ) -> Result<Downloaded> {
        let mut backoff = policy.initial_backoff;
        let mut attempt = 1;
        loop {
            match self.download_to(url, dest, expected, &mut progress).await {
                Ok(done) => return Ok(done),
                Err(e) if e.is_transient() && attempt < policy.attempts => {
                    warn!(
                        attempt,
                        error = %e,
                        backoff_ms = backoff.as_millis() as u64,
                        "download failed, retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(policy.max_backoff);
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn digest(ctx: &Context) -> [u8; 32] {
    let mut out = [0; 32];
    out.copy_from_slice(ctx.clone().finish().as_ref());
    out
}

/// Hashes an existing `.part` file: `(length, context)`; `(0, empty)` when
/// there is none.
async fn hash_prefix(part: &Path) -> Result<(u64, Context)> {
    let mut ctx = Context::new(&SHA256);
    let mut file = match File::open(part).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, ctx)),
        Err(source) => return Err(source).context(IoSnafu { path: part }),
    };
    let mut buf = vec![0; READ_BUF];
    let mut len = 0u64;
    loop {
        let n = file.read(&mut buf).await.context(IoSnafu { path: part })?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
        len += n as u64;
    }
    Ok((len, ctx))
}

/// Renames `.part` over `dest` and fsyncs the directory so the rename
/// survives power loss.
async fn finalize(part: &Path, dest: &Path) -> Result<()> {
    tokio::fs::rename(part, dest)
        .await
        .context(IoSnafu { path: dest })?;
    if let Some(dir) = dest.parent().filter(|d| !d.as_os_str().is_empty()) {
        let dir = dir.to_owned();
        // Some file systems refuse fsync on a directory; the rename is done
        // either way, so this is best effort.
        let synced =
            tokio::task::spawn_blocking(move || std::fs::File::open(&dir)?.sync_all()).await;
        if let Ok(Err(e)) = synced {
            debug!(error = %e, "directory fsync failed");
        }
    }
    Ok(())
}

async fn discard(part: &Path) {
    if let Err(e) = tokio::fs::remove_file(part).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %part.display(), error = %e, "cannot remove partial download");
        }
    }
}

/// `bytes START-END/TOTAL` → `(START, Some(TOTAL))`; `TOTAL` may be `*`.
fn parse_content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    if end < start {
        return None;
    }
    let total = match total.trim() {
        "*" => None,
        n => Some(n.parse().ok()?),
    };
    Some((start, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(
            parse_content_range("bytes 100-199/200"),
            Some((100, Some(200)))
        );
        assert_eq!(parse_content_range("bytes 0-9/*"), Some((0, None)));
        assert_eq!(parse_content_range("bytes */200"), None);
        assert_eq!(parse_content_range("bytes 9-1/20"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
    }

    #[test]
    fn part_suffix() {
        assert_eq!(
            part_path(Path::new("/x/update/a.kup")),
            PathBuf::from("/x/update/a.kup.part")
        );
    }
}
