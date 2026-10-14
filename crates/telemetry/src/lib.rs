//! Tracing and Prometheus metrics for the camera binaries.
//!
//! One call — [`install`] — sets up structured logging through `tracing` and,
//! optionally, a Prometheus metrics recorder.
//!
//! # Why there is no OpenTelemetry here
//!
//! `opentelemetry_sdk` does not compile for this target. Its metrics
//! aggregation is built on `AtomicU64`/`AtomicI64`, and ARMv5TE has no 64-bit
//! atomic instructions (`LDREXD`/`STREXD` arrived with ARMv6K), so `std` does
//! not provide those types at all:
//!
//! ```text
//! error[E0432]: unresolved imports `std::sync::atomic::AtomicI64`,
//!               `std::sync::atomic::AtomicU64`
//!   --> opentelemetry_sdk-0.31.0/src/metrics/internal/mod.rs:12:37
//! ```
//!
//! That is a property of the CPU, not of a feature flag, so OTLP export has to
//! happen off-device. The path that does work is the one already planned:
//! Prometheus scrapes the camera, either directly from [`Exporter::Http`] or
//! through the `node_exporter` textfile collector via [`Exporter::Textfile`].
//! A collector elsewhere can convert to OTLP if that is wanted.

use file_rotate::{compression::Compression, suffix::AppendCount, ContentLimit, FileRotate};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use snafu::{ResultExt, Snafu};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Environment variable controlling log filtering, e.g. `ANYKA_LOG=debug`.
pub const LOG_ENV: &str = "ANYKA_LOG";
/// Environment variable selecting the metrics exporter.
pub const METRICS_ENV: &str = "ANYKA_METRICS";
/// Write logs to this file, with rotation, instead of stderr.
pub const LOG_FILE_ENV: &str = "ANYKA_LOG_FILE";
/// Rotate the log file once it passes this many bytes.
pub const LOG_MAX_BYTES_ENV: &str = "ANYKA_LOG_MAX_BYTES";
/// Keep this many rotated generations.
pub const LOG_KEEP_ENV: &str = "ANYKA_LOG_KEEP";

const DEFAULT_LOG_MAX_BYTES: usize = 1024 * 1024;
const DEFAULT_LOG_KEEP: usize = 5;

/// How often the textfile exporter rewrites its file.
const TEXTFILE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("invalid {METRICS_ENV} value {value:?}: {reason}"))]
    MetricsSpec { value: String, reason: String },

    #[snafu(display("cannot install the Prometheus recorder: {source}"))]
    Recorder {
        source: metrics_exporter_prometheus::BuildError,
    },

    #[snafu(display("cannot bind the metrics listener on {address}: {source}"))]
    Bind {
        address: SocketAddr,
        source: std::io::Error,
    },

    #[snafu(display("cannot create the metrics directory {}: {source}", path.display()))]
    TextfileDir {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("cannot start the {name} thread: {source}"))]
    Thread {
        name: &'static str,
        source: std::io::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where metrics go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exporter {
    /// Collect nothing. Recording a metric becomes a no-op.
    Off,
    /// Serve `/metrics` over HTTP for Prometheus to scrape directly.
    Http { address: SocketAddr },
    /// Rewrite `<dir>/<service>.prom` for the `node_exporter` textfile
    /// collector.
    ///
    /// Cheaper than an HTTP listener and the natural fit when `node_exporter`
    /// is already running on the camera: point it at this directory with
    /// `--collector.textfile.directory`.
    ///
    /// The value is a **directory**, not a file, and the file name comes from
    /// the service. That is not cosmetic: the supervisor passes its
    /// environment to the processes it starts, so parent and children all
    /// enable the same exporter. Sharing one path made them race on the
    /// write-and-rename and fail with `No such file or directory`.
    Textfile { dir: PathBuf },
}

impl Exporter {
    /// Parses the `ANYKA_METRICS` spelling:
    ///
    /// * `off` (default)
    /// * `http:0.0.0.0:9101`
    /// * `textfile:/var/lib/node_exporter/textfile_collector/anyka.prom`
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("off") {
            return Ok(Exporter::Off);
        }
        if let Some(address) = value.strip_prefix("http:") {
            let address = address
                .parse::<SocketAddr>()
                .map_err(|e| Error::MetricsSpec {
                    value: value.to_owned(),
                    reason: format!("{address:?} is not an address:port ({e})"),
                })?;
            return Ok(Exporter::Http { address });
        }
        if let Some(dir) = value.strip_prefix("textfile:") {
            if dir.is_empty() {
                return Err(Error::MetricsSpec {
                    value: value.to_owned(),
                    reason: "textfile: needs a directory".into(),
                });
            }
            // Tolerate a path that names a file, which is the natural thing to
            // write and what the earlier spelling accepted.
            let dir = PathBuf::from(dir);
            let dir = if dir.extension().is_some_and(|ext| ext == "prom") {
                dir.parent().unwrap_or(Path::new(".")).to_path_buf()
            } else {
                dir
            };
            return Ok(Exporter::Textfile { dir });
        }
        Err(Error::MetricsSpec {
            value: value.to_owned(),
            reason: "expected off, http:ADDR:PORT or textfile:DIR".into(),
        })
    }

    fn from_env() -> Result<Self> {
        match std::env::var(METRICS_ENV) {
            Ok(value) => Self::parse(&value),
            Err(_) => Ok(Exporter::Off),
        }
    }
}

/// Keeps the exporter alive. Metrics stop being served when this is dropped.
#[derive(Debug)]
pub struct Telemetry {
    exporter: Exporter,
}

impl Telemetry {
    /// Which exporter ended up active.
    pub fn exporter(&self) -> &Exporter {
        &self.exporter
    }
}

/// Installs logging and metrics for a binary.
///
/// `service` names the binary and is attached to every metric as a label, so
/// several camera processes can be scraped into the same Prometheus without
/// colliding.
///
/// Call once, early in `main`. Logging is always installed; metrics follow
/// `ANYKA_METRICS` and default to off.
pub fn install(service: &'static str) -> Result<Telemetry> {
    install_with_console(service, None)
}

/// Receives one formatted log event (no trailing newline) for a console.
/// Must not block: it is called on whatever thread emitted the event.
pub type ConsoleSink = std::sync::Arc<dyn Fn(String) + Send + Sync>;

/// Like [`install`], and also mirrors events at or above `level` (an
/// `EnvFilter` directive such as `info` or `warn,supervisor=debug`) to
/// `sink`, with ANSI colour — meant for a serial console.
pub fn install_with_console(
    service: &'static str,
    console: Option<(ConsoleSink, String)>,
) -> Result<Telemetry> {
    install_tracing(console);
    let exporter = Exporter::from_env()?;
    install_metrics(service, exporter.clone())?;
    Ok(Telemetry { exporter })
}

/// A rotating log file that can be shared with the subscriber.
///
/// `MakeWriter` is implemented for any `Fn() -> impl Write`, so a cheaply
/// cloneable handle is all that is needed here.
#[derive(Clone)]
struct RotatingLog(std::sync::Arc<std::sync::Mutex<FileRotate<AppendCount>>>);

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // A poisoned lock means another thread panicked mid-write. The log is
        // still usable and dropping diagnostics during a panic is the worst
        // possible moment, so carry on with the inner value.
        let mut file = self.0.lock().unwrap_or_else(|err| err.into_inner());
        file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut file = self.0.lock().unwrap_or_else(|err| err.into_inner());
        file.flush()
    }
}

/// Sets up `tracing` output.
///
/// Goes to stderr unless `ANYKA_LOG_FILE` names a file, in which case output
/// is written there and rotated by size — which is what the camera wants,
/// since stderr from a boot script otherwise ends up nowhere.
///
/// Timestamps: stderr carries none, because under the supervisor stderr is a
/// pipe into the service's log file and the supervisor stamps every line it
/// writes there (a second stamp would only repeat it). The `ANYKA_LOG_FILE`
/// mode — the supervisor's own `supervisor.log` — has no one else to stamp
/// it, so it gets the same prefix ([`FileStamp`]): local wall time once NTP
/// has set the clock (the camera has no RTC and boots at 1970), and always
/// milliseconds since boot.
fn install_tracing(console: Option<(ConsoleSink, String)>) {
    use tracing_subscriber::filter::EnvFilter;
    use tracing_subscriber::fmt;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    let filter = EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new("info"));
    let main = fmt::layer().with_target(false);
    let main = match log_file() {
        // No ANSI: this is going into a file that will be read over a
        // serial console or copied off the card.
        Some(writer) => main
            .with_timer(FileStamp)
            .with_ansi(false)
            .with_writer(fmt::writer::BoxMakeWriter::new(move || writer.clone()))
            .with_filter(filter)
            .boxed(),
        None => {
            // Colour only on a terminal. Under the supervisor, stderr is a
            // pipe into the service's log file, and escape codes there make
            // the logs unreadable and ungreppable.
            use std::io::IsTerminal;
            main.without_time()
                .with_ansi(std::io::stderr().is_terminal())
                .with_writer(fmt::writer::BoxMakeWriter::new(std::io::stderr))
                .with_filter(filter)
                .boxed()
        }
    };

    let console = console.map(|(sink, level)| {
        let filter = EnvFilter::try_new(&level).unwrap_or_else(|_| EnvFilter::new("info"));
        fmt::layer()
            .without_time()
            .with_target(false)
            .with_ansi(true)
            .with_writer(move || ConsoleLine {
                sink: sink.clone(),
                buf: Vec::new(),
            })
            .with_filter(filter)
    });

    let _ = tracing_subscriber::registry()
        .with(main)
        .with(console)
        .try_init();
}

/// The `ANYKA_LOG_FILE` line prefix, the same one the supervisor writes in
/// front of every service's lines ([`os::time::LogPrefix`]):
///
/// ```text
/// [2026-10-14 14:03:07.123 -0300] [     81234]  INFO supervisor started
/// ```
struct FileStamp;

impl tracing_subscriber::fmt::time::FormatTime for FileStamp {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", os::time::LogPrefix::now())
    }
}

/// Collects one event's bytes and hands them to the sink as a line.
struct ConsoleLine {
    sink: ConsoleSink,
    buf: Vec<u8>,
}

impl Write for ConsoleLine {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for ConsoleLine {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.buf);
        let text = text.trim_end();
        if !text.is_empty() {
            (self.sink)(text.to_owned());
        }
    }
}

/// Builds the rotating writer from the environment, if one was asked for.
fn log_file() -> Option<RotatingLog> {
    let path = PathBuf::from(std::env::var_os(LOG_FILE_ENV)?);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            // Nothing to report to yet — the subscriber is what is being set
            // up — so fall back to stderr if the directory cannot be made.
            if std::fs::create_dir_all(parent).is_err() {
                return None;
            }
        }
    }
    let max_bytes = env_usize(LOG_MAX_BYTES_ENV, DEFAULT_LOG_MAX_BYTES).max(1);
    let keep = env_usize(LOG_KEEP_ENV, DEFAULT_LOG_KEEP);
    let file = FileRotate::new(
        path,
        AppendCount::new(keep),
        ContentLimit::BytesSurpassed(max_bytes),
        Compression::None,
        None,
    );
    Some(RotatingLog(std::sync::Arc::new(std::sync::Mutex::new(
        file,
    ))))
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn install_metrics(service: &'static str, exporter: Exporter) -> Result<()> {
    if exporter == Exporter::Off {
        return Ok(());
    }

    let handle = PrometheusBuilder::new()
        .add_global_label("service", service)
        .install_recorder()
        .context(RecorderSnafu)?;

    match exporter {
        Exporter::Off => unreachable!("handled above"),
        Exporter::Http { address } => serve_http(address, handle),
        Exporter::Textfile { dir } => serve_textfile(&dir, service, handle),
    }
}

/// Serves `/metrics` from one blocking thread.
///
/// Prometheus scrapes are infrequent and serial; a thread per request, or a
/// whole async runtime, would cost more than it buys here.
fn serve_http(address: SocketAddr, handle: PrometheusHandle) -> Result<()> {
    let listener = TcpListener::bind(address).context(BindSnafu { address })?;
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        if let Err(error) = respond(stream, &handle) {
                            tracing::debug!(%error, "metrics request failed");
                        }
                    }
                    Err(error) => tracing::warn!(%error, "metrics accept failed"),
                }
            }
        })
        .context(ThreadSnafu {
            name: "metrics-http",
        })?;
    tracing::info!(%address, "serving Prometheus metrics");
    Ok(())
}

fn respond(mut stream: TcpStream, handle: &PrometheusHandle) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read};

    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    // Read only the request line, and bound it: a scraper that never sends a
    // newline must not make us buffer without limit.
    let mut request = String::new();
    BufReader::new(Read::by_ref(&mut stream).take(4096)).read_line(&mut request)?;

    let body = handle.render();
    let header = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/plain; version=0.0.4\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// Periodically rewrites a `.prom` file for the `node_exporter` textfile
/// collector.
fn serve_textfile(dir: &Path, service: &'static str, handle: PrometheusHandle) -> Result<()> {
    std::fs::create_dir_all(dir).context(TextfileDirSnafu { path: dir })?;
    // One file per service: every process started by the supervisor inherits
    // the same `ANYKA_METRICS`, and a shared file makes them race.
    let path = dir.join(format!("{service}.prom"));
    let shown = path.display().to_string();
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || loop {
            if let Err(error) = write_atomically(&path, handle.render().as_bytes()) {
                tracing::warn!(%error, path = %path.display(), "cannot write metrics textfile");
            }
            std::thread::sleep(TEXTFILE_INTERVAL);
        })
        .context(ThreadSnafu {
            name: "metrics-textfile",
        })?;
    tracing::info!(path = %shown, "writing Prometheus metrics for node_exporter");
    Ok(())
}

/// Writes through a temporary file and renames it into place.
///
/// `node_exporter` reads the directory on its own schedule; without the rename
/// it would regularly parse a half-written file and drop the whole scrape.
fn write_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("prom.tmp");
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::{format::Writer, time::FormatTime};

    #[test]
    fn log_file_lines_get_wall_time_and_uptime() {
        let mut out = String::new();
        FileStamp.format_time(&mut Writer::new(&mut out)).unwrap();
        // The host clock is set: `[YYYY-MM-DD HH:MM:SS.mmm ±HHMM] [  uptime]`.
        assert_eq!(&out[..1], "[");
        assert_eq!(&out[30..33], "] [", "{out}");
        assert!(out.ends_with(']'), "{out}");
        assert!(
            out[33..out.len() - 1].trim().parse::<u64>().is_ok(),
            "{out}"
        );
    }
}
