//! Per-service log files on the SD card, with size-based rotation.
//!
//! Every supervised process gets `<log_dir>/<service>.log`, rotated once it
//! passes `log_max_bytes` and kept for `log_keep` generations
//! (`<service>.log.1`, `.2`, …).
//!
//! All writes happen on one dedicated OS thread fed by a **bounded** channel.
//! Two reasons for that shape on this device:
//!
//! * writing to the SD card blocks, sometimes for a long time if the card is
//!   doing wear levelling, and blocking inside the async runtime would stall
//!   the supervision logic that has to notice process exits;
//! * bounded means a service that floods its output applies backpressure to
//!   its own log reader instead of growing a queue in 32 MB of RAM.

use crate::console::{self, Alert, Console, Level};
use file_rotate::{compression::Compression, suffix::AppendCount, ContentLimit, FileRotate};
use metrics::counter;
use snafu::{ResultExt, Snafu};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot create the log directory {}: {source}", path.display()))]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("cannot start the log writer thread: {source}"))]
    Thread { source: std::io::Error },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// How many lines may be queued before producers block.
const QUEUE_DEPTH: usize = 256;
/// Recent lines kept in RAM per service for console alerts.
const TAIL_LINES: usize = 20;
/// Longer lines are cut in the tail (the log file keeps them whole).
const TAIL_LINE_MAX: usize = 160;

/// Which of a child's streams a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    fn tag(self) -> &'static str {
        match self {
            Stream::Stdout => "out",
            Stream::Stderr => "err",
        }
    }
}

enum Command {
    Line {
        service: String,
        stream: Stream,
        line: String,
    },
    /// Supervisor-generated note, e.g. "started", "exited with 1".
    Note {
        service: String,
        note: String,
    },
    Flush,
    /// Print a failure on the console with the service's recent lines.
    Alert {
        service: String,
        headline: String,
        critical: bool,
    },
}

/// Handle used to send lines to the writer thread.
#[derive(Debug, Clone)]
pub struct LogSink {
    tx: std::sync::mpsc::SyncSender<Command>,
}

impl LogSink {
    /// Records one line of a child's output.
    ///
    /// Blocks if the writer is behind — that backpressure is deliberate.
    pub fn line(&self, service: &str, stream: Stream, line: String) {
        let _ = self.tx.send(Command::Line {
            service: service.to_owned(),
            stream,
            line,
        });
    }

    /// Records a supervisor event in the service's own log, so the log tells
    /// the whole story of that process rather than just its stdout.
    pub fn note(&self, service: &str, note: impl Into<String>) {
        let _ = self.tx.send(Command::Note {
            service: service.to_owned(),
            note: note.into(),
        });
    }

    /// Reports a failure on the serial console, with the last lines this
    /// service logged. Does nothing if no console was configured.
    pub fn alert(&self, service: &str, headline: impl Into<String>, critical: bool) {
        let _ = self.tx.send(Command::Alert {
            service: service.to_owned(),
            headline: headline.into(),
            critical,
        });
    }

    /// Asks the writer to flush. Used on shutdown.
    pub fn flush(&self) {
        let _ = self.tx.send(Command::Flush);
    }

    /// Builds a sink that discards everything, for unit tests that need a
    /// `LogSink` without touching the filesystem.
    #[cfg(test)]
    pub fn discarding() -> Self {
        // Depth 1 is enough: nothing reads the far end, so sends fail fast
        // and are ignored, which is exactly what a test wants.
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        Self { tx }
    }
}

/// Starts the log writer thread.
///
/// `console`, when given, receives failure alerts (see [`LogSink::alert`])
/// and every service output line at or above `console_level`.
pub fn start(
    log_dir: &Path,
    max_bytes: usize,
    keep: usize,
    console: Option<Console>,
    console_level: Option<Level>,
) -> Result<LogSink> {
    std::fs::create_dir_all(log_dir).context(CreateDirSnafu { path: log_dir })?;

    let (tx, rx) = std::sync::mpsc::sync_channel::<Command>(QUEUE_DEPTH);
    let dir = log_dir.to_path_buf();

    std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut files: HashMap<String, FileRotate<AppendCount>> = HashMap::new();
            let mut tails: HashMap<String, VecDeque<String>> = HashMap::new();
            while let Ok(command) = rx.recv() {
                match command {
                    Command::Line {
                        service,
                        stream,
                        line,
                    } => {
                        let bytes = line.len() + 1;
                        remember(&mut tails, &service, stream.tag(), &line);
                        if let (Some(console), Some(threshold)) = (&console, console_level) {
                            if Level::of_line(&line) <= threshold {
                                console.print(console::render_log(
                                    &console::Stamp::now(),
                                    &service,
                                    &line,
                                    false,
                                ));
                            }
                        }
                        write_line(
                            &mut files,
                            &dir,
                            max_bytes,
                            keep,
                            &service,
                            stream.tag(),
                            &line,
                        );
                        counter!("kamoder_supervisor_log_bytes_total", "service" => service)
                            .increment(bytes as u64);
                    }
                    Command::Note { service, note } => {
                        remember(&mut tails, &service, "sup", &note);
                        write_line(&mut files, &dir, max_bytes, keep, &service, "sup", &note);
                    }
                    Command::Flush => {
                        for file in files.values_mut() {
                            let _ = file.flush();
                        }
                    }
                    Command::Alert {
                        service,
                        headline,
                        critical,
                    } => {
                        if let Some(console) = &console {
                            let tail = tails
                                .get(&service)
                                .map(|t| t.iter().cloned().collect())
                                .unwrap_or_default();
                            console.send(Alert {
                                service,
                                headline,
                                critical,
                                tail,
                            });
                        }
                    }
                }
            }
            for file in files.values_mut() {
                let _ = file.flush();
            }
        })
        .context(ThreadSnafu)?;

    Ok(LogSink { tx })
}

/// Keeps the last [`TAIL_LINES`] lines of a service for console alerts.
fn remember(tails: &mut HashMap<String, VecDeque<String>>, service: &str, tag: &str, line: &str) {
    let tail = tails.entry(service.to_owned()).or_default();
    if tail.len() == TAIL_LINES {
        tail.pop_front();
    }
    let mut entry = format!("{tag} {line}");
    if entry.len() > TAIL_LINE_MAX {
        let mut cut = TAIL_LINE_MAX;
        while !entry.is_char_boundary(cut) {
            cut -= 1;
        }
        entry.truncate(cut);
        entry.push('…');
    }
    tail.push_back(entry);
}

fn write_line(
    files: &mut HashMap<String, FileRotate<AppendCount>>,
    dir: &Path,
    max_bytes: usize,
    keep: usize,
    service: &str,
    tag: &str,
    line: &str,
) {
    let file = files.entry(service.to_owned()).or_insert_with(|| {
        FileRotate::new(
            dir.join(format!("{service}.log")),
            AppendCount::new(keep),
            ContentLimit::BytesSurpassed(max_bytes),
            // No compression: gzip on a 400 MHz ARM926 costs more than the
            // SD card space it saves, and these logs are read by hand.
            Compression::None,
            None,
        )
    });

    let text = format_line(os::time::LogPrefix::now(), tag, line);
    // One write per line, so rotation never splits one.
    if file.write_all(text.as_bytes()).is_err() {
        // Nothing useful to do — the SD card may be full or gone. Drop the
        // line rather than take the supervisor down with it.
        counter!("kamoder_supervisor_log_errors_total", "service" => service.to_owned())
            .increment(1);
    }
}

/// One log file line:
///
/// ```text
/// [2026-10-14 14:03:07.123 -0300] [     81234] out  INFO listening
/// [     81234] out  INFO listening                (clock not set yet)
/// ```
///
/// Local wall time once NTP has set the clock, and always the milliseconds
/// since boot: the camera has no RTC and reads as 1970 until NTP lands, when
/// a wall time would be actively misleading (see [`os::time::LogPrefix`]).
/// The web log viewer parses both shapes.
fn format_line(prefix: os::time::LogPrefix, tag: &str, line: &str) -> String {
    format!("{prefix} {tag} {line}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use os::time::{LocalTime, LogPrefix};

    #[test]
    fn line_has_wall_time_once_the_clock_is_set() {
        let wall = LocalTime::from_unix(1_791_997_387, 123, -10_800);
        let prefix = |wall| LogPrefix {
            wall,
            uptime_ms: 81_234,
        };
        assert_eq!(
            format_line(prefix(Some(wall)), "out", " INFO listening"),
            "[2026-10-14 14:03:07.123 -0300] [     81234] out  INFO listening\n"
        );
        assert_eq!(
            format_line(prefix(None), "err", "boom"),
            "[     81234] err boom\n"
        );
    }
}
