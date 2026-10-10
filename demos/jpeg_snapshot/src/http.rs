//! A single-purpose HTTP server that serves the latest JPEG.
//!
//! The C original (`jpeg_snapshot/http_server.h`, itself derived from `nweb`)
//! parsed requests by indexing a 64 KB static buffer, called `logger()` on
//! errors and then carried on regardless — a request shorter than the
//! extension it was comparing read before the start of the buffer, and the
//! JPEG was read from a shared struct while the capture thread was writing to
//! it.
//!
//! This version reads a bounded request, only answers `GET`, and takes the
//! image from a mutex-guarded [`SnapshotBuffer`]. Nothing is served off disk,
//! so there is no path traversal surface at all.

use metrics::counter;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Longest request line accepted, to bound what a client can make us buffer.
const MAX_REQUEST_LINE: u64 = 8 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// The most recent JPEG, shared between the capture loop and the server.
///
/// The capture loop only encodes while `pending` is non-zero; each HTTP
/// request tops it back up. That keeps the encoder — and roughly 60% of the
/// CPU — idle when nobody is watching.
#[derive(Debug, Default)]
pub struct SnapshotBuffer {
    image: Mutex<Vec<u8>>,
    pending: AtomicU32,
}

/// How many frames to keep encoding after a request arrives.
const FRAMES_PER_REQUEST: u32 = 25;

impl SnapshotBuffer {
    pub fn new() -> Arc<Self> {
        let buffer = Arc::new(Self::default());
        // Prime the buffer so the very first request has something to serve.
        buffer.pending.store(5, Ordering::Relaxed);
        buffer
    }

    /// Replaces the stored image.
    pub fn store(&self, jpeg: &[u8]) {
        let mut image = self.image.lock().unwrap_or_else(|e| e.into_inner());
        image.clear();
        image.extend_from_slice(jpeg);
    }

    /// Copies the stored image out, or `None` while nothing has been captured.
    pub fn load(&self) -> Option<Vec<u8>> {
        let image = self.image.lock().unwrap_or_else(|e| e.into_inner());
        (!image.is_empty()).then(|| image.clone())
    }

    /// Whether the capture loop should keep encoding.
    pub fn wants_frame(&self) -> bool {
        self.pending.load(Ordering::Relaxed) > 0
    }

    /// Records that one frame was encoded.
    ///
    /// Only the capture loop decrements, so a plain load/store is enough. A
    /// race with [`SnapshotBuffer::request_frames`] can only cost or gain a
    /// single frame of encoding, which nothing depends on.
    pub fn frame_encoded(&self) {
        let pending = self.pending.load(Ordering::Relaxed);
        if pending > 0 {
            self.pending.store(pending - 1, Ordering::Relaxed);
        }
    }

    /// Asks the capture loop for more frames.
    fn request_frames(&self) {
        self.pending.store(FRAMES_PER_REQUEST, Ordering::Relaxed);
    }
}

/// Starts the server on a background thread.
pub fn serve(port: u16, snapshot: Arc<SnapshotBuffer>) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    tracing::info!(url = %format!("http://<camera>:{port}/snapshot.jpeg"), "snapshot server");

    // The thread is deliberately left unnamed: naming it makes std call
    // `pthread_setname_np`, which the uClibc 0.9.33 in the cross toolchain
    // does not export, and the link fails.
    std::thread::Builder::new()
        // The device has 32 MB of RAM; the default 8 MB stack per thread is
        // far more than this handler needs.
        .stack_size(64 * 1024)
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    // Requests are handled one at a time: the response is a
                    // memory copy, and a thread per connection would let a
                    // handful of clients exhaust RAM.
                    Ok(stream) => {
                        if let Err(error) = handle(stream, &snapshot) {
                            tracing::debug!(%error, "request failed");
                        }
                    }
                    Err(error) => tracing::warn!(%error, "accept failed"),
                }
            }
        })?;
    Ok(())
}

fn handle(stream: TcpStream, snapshot: &SnapshotBuffer) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    // `take` bounds the read: without it a client that never sends a newline
    // makes us buffer until the allocator gives up.
    Read::take(&mut reader, MAX_REQUEST_LINE).read_line(&mut request_line)?;

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    let mut stream = stream;
    if !method.eq_ignore_ascii_case("GET") {
        counter!("kamoder_snapshot_requests_total", "outcome" => "rejected").increment(1);
        return respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            b"GET only\n",
        );
    }

    // Any `.jpeg`/`.jpg` path serves the current frame; the C version did the
    // same, matching on the extension rather than the path.
    if target.ends_with(".jpeg") || target.ends_with(".jpg") || target == "/" {
        snapshot.request_frames();
        return match snapshot.load() {
            Some(jpeg) => {
                counter!("kamoder_snapshot_requests_total", "outcome" => "served").increment(1);
                counter!("kamoder_snapshot_bytes_total").increment(jpeg.len() as u64);
                respond(&mut stream, "200 OK", "image/jpeg", &jpeg)
            }
            None => respond(
                &mut stream,
                "503 Service Unavailable",
                "text/plain",
                b"no frame captured yet\n",
            ),
        };
    }

    counter!("kamoder_snapshot_requests_total", "outcome" => "not_found").increment(1);
    respond(&mut stream, "404 Not Found", "text/plain", b"not found\n")
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
