//! Failure alerts on the serial console.
//!
//! When a service dies unexpectedly, is killed, cannot be spawned or is given
//! up on, the person at the UART should see it without digging through the
//! SD card: a red headline plus the last lines that service logged.
//!
//! Alerts go through a small bounded queue to their own thread and are
//! dropped, never waited for, when it is full — the console is 115200 baud
//! and must not slow down supervision. A crash-looping service gets its log
//! tail printed at most once per [`TAIL_INTERVAL`]; repeats in between are
//! one line each.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::time::{Duration, Instant};

/// Big enough to absorb a boot-time burst of log lines.
const QUEUE_DEPTH: usize = 256;
/// Stop draining the queue into one write past this size.
const BATCH_BYTES: usize = 4096;
const TAIL_INTERVAL: Duration = Duration::from_secs(60);

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const WHITE_ON_RED: &str = "\x1b[1;97;41m";
const GREEN: &str = "\x1b[32m";
const BLUE: &str = "\x1b[34m";
const CYAN: &str = "\x1b[36m";

/// One failure to report.
#[derive(Debug, Clone)]
pub struct Alert {
    pub service: String,
    pub headline: String,
    /// A `critical` service (e.g. the watchdog feeder): failure may reboot
    /// the camera, so it is shown louder.
    pub critical: bool,
    /// Most recent log lines of the service, oldest first.
    pub tail: Vec<String>,
}

/// What the console thread writes.
enum Message {
    Alert(Alert),
    /// Pre-formatted text, e.g. the ready report.
    Text(String),
}

/// Sends alerts and reports to the console thread.
#[derive(Debug, Clone)]
pub struct Console {
    tx: SyncSender<Message>,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Message::Alert(alert) => f.debug_tuple("Alert").field(alert).finish(),
            Message::Text(_) => f.write_str("Text"),
        }
    }
}

impl Console {
    /// Starts the console thread writing to `path` (normally `/dev/console`).
    pub fn start(path: PathBuf) -> std::io::Result<Self> {
        let (tx, rx) = sync_channel::<Message>(QUEUE_DEPTH);
        std::thread::Builder::new()
            .name("console".into())
            .stack_size(64 * 1024)
            .spawn(move || {
                let mut last_tail: HashMap<String, Instant> = HashMap::new();
                while let Ok(first) = rx.recv() {
                    // Drain what is already queued and write it in one go:
                    // one open and one write per burst, not per line.
                    let mut text = String::new();
                    let mut next = Some(first);
                    while let Some(message) = next.take() {
                        text.push_str(&match message {
                            Message::Text(text) => text,
                            Message::Alert(mut alert) => {
                                let now = Instant::now();
                                let show_tail = last_tail
                                    .get(&alert.service)
                                    .is_none_or(|at| now.duration_since(*at) >= TAIL_INTERVAL);
                                if show_tail && !alert.tail.is_empty() {
                                    last_tail.insert(alert.service.clone(), now);
                                } else {
                                    alert.tail.clear();
                                }
                                render(&alert, uptime())
                            }
                        });
                        if text.len() < BATCH_BYTES {
                            next = rx.try_recv().ok();
                        }
                    }
                    // O_NOCTTY: never become the serial line's controlling
                    // process. Reopened per message so a console that was
                    // briefly unavailable recovers by itself.
                    let _ = OpenOptions::new()
                        .append(true)
                        .custom_flags(libc::O_NOCTTY)
                        .open(&path)
                        .and_then(|mut tty| tty.write_all(text.as_bytes()));
                }
            })?;
        Ok(Self { tx })
    }

    /// Queues an alert; drops it if the console is behind.
    pub fn send(&self, alert: Alert) {
        self.queue(Message::Alert(alert));
    }

    /// Queues pre-formatted text; drops it if the console is behind.
    pub fn print(&self, text: String) {
        self.queue(Message::Text(text));
    }

    fn queue(&self, message: Message) {
        match self.tx.try_send(message) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                metrics::counter!("kamoder_supervisor_console_dropped_total").increment(1);
            }
        }
    }
}

/// Log severity, most severe first, as printed by `tracing`'s formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    /// Parses a console threshold. `Ok(None)` means `off`.
    pub fn parse_threshold(value: &str) -> Result<Option<Level>, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Ok(None),
            "error" => Ok(Some(Level::Error)),
            "warn" | "warning" => Ok(Some(Level::Warn)),
            "info" => Ok(Some(Level::Info)),
            "debug" => Ok(Some(Level::Debug)),
            "trace" => Ok(Some(Level::Trace)),
            other => Err(format!(
                "unknown console level `{other}` (off, error, warn, info, debug, trace)"
            )),
        }
    }

    /// Level of a service's output line. `tracing` lines start with the
    /// level (`  INFO msg`, `ERROR msg`); anything else — a plain `println!`
    /// from a demo — counts as `Info`.
    pub fn of_line(line: &str) -> Level {
        match line.split_whitespace().next() {
            Some("ERROR") => Level::Error,
            Some("WARN") => Level::Warn,
            Some("DEBUG") => Level::Debug,
            Some("TRACE") => Level::Trace,
            _ => Level::Info,
        }
    }

    /// `EnvFilter` directive for this threshold.
    pub fn directive(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }

    fn colour(self) -> &'static str {
        match self {
            Level::Error => RED,
            Level::Warn => YELLOW,
            Level::Info => GREEN,
            Level::Debug => BLUE,
            Level::Trace => DIM,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => " WARN",
            Level::Info => " INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

/// Formats one log line for the console: uptime, source, coloured level,
/// message. `line` is either a service's raw output line (no colour, level
/// token first) or a supervisor event already coloured by `tracing`.
pub fn render_log(uptime: f64, source: &str, line: &str, coloured: bool) -> String {
    let body = if coloured {
        line.trim_start().to_owned()
    } else {
        let level = Level::of_line(line);
        let trimmed = line.trim_start();
        let message = trimmed
            .strip_prefix(level.name().trim_start())
            .filter(|_| trimmed.split_whitespace().next() == Some(level.name().trim_start()))
            .map(str::trim_start)
            .unwrap_or(trimmed);
        format!("{}{}{RESET} {message}", level.colour(), level.name())
    };
    format!("{DIM}[{uptime:>9.2}]{RESET} {CYAN}{source:<10}{RESET} {body}{RESET}\r\n")
}

/// Formats one alert. `\r\n` line ends, as on a raw serial line.
pub fn render(alert: &Alert, uptime: f64) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push_str(RESET);
        out.push_str("\r\n");
    };
    let tag = if alert.critical {
        format!("{WHITE_ON_RED} CRITICAL ")
    } else {
        format!("{RED}{BOLD} FAILURE ")
    };
    line(format!(
        "{DIM}[{uptime:>9.2}]{RESET} {tag}{RESET} {BOLD}{}{RESET}: {RED}{}",
        alert.service, alert.headline
    ));
    if !alert.tail.is_empty() {
        line(format!(
            "{DIM}            last {} log lines of {}:",
            alert.tail.len(),
            alert.service
        ));
        for entry in &alert.tail {
            line(format!("{DIM}            │{RESET} {YELLOW}{entry}"));
        }
        line(format!(
            "{DIM}            full log: logs/{}.log",
            alert.service
        ));
    }
    out
}

pub fn uptime() -> f64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|t| t.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for c in s.chars() {
            match (esc, c) {
                (false, '\x1b') => esc = true,
                (true, 'm') => esc = false,
                (true, _) => {}
                (false, c) => out.push(c),
            }
        }
        out
    }

    #[test]
    fn critical_alert_with_tail() {
        let alert = Alert {
            service: "wdt_feeder".into(),
            headline: "killed by signal 9".into(),
            critical: true,
            tail: vec!["err open /dev/watchdog: busy".into()],
        };
        let text = strip(&render(&alert, 12.5));
        assert_eq!(
            text,
            "[    12.50]  CRITICAL  wdt_feeder: killed by signal 9\r\n\
             \x20           last 1 log lines of wdt_feeder:\r\n\
             \x20           │ err open /dev/watchdog: busy\r\n\
             \x20           full log: logs/wdt_feeder.log\r\n"
        );
    }

    #[test]
    fn service_lines_get_level_colour_and_source() {
        let text = strip(&render_log(
            3.0,
            "web",
            "  INFO listening on 0.0.0.0:80",
            false,
        ));
        assert_eq!(
            text,
            "[     3.00] web         INFO listening on 0.0.0.0:80\r\n"
        );
        let text = strip(&render_log(3.0, "wifi", " WARN no lease yet", false));
        assert_eq!(text, "[     3.00] wifi        WARN no lease yet\r\n");
        // A demo's plain println! is shown as INFO, unchanged.
        let text = strip(&render_log(3.0, "rtsp", "frame 12", false));
        assert_eq!(text, "[     3.00] rtsp        INFO frame 12\r\n");
    }

    #[test]
    fn levels_parse_and_order() {
        assert_eq!(Level::of_line("ERROR boom"), Level::Error);
        assert_eq!(Level::of_line(" WARN x"), Level::Warn);
        assert_eq!(Level::of_line("DEBUG x"), Level::Debug);
        assert_eq!(Level::of_line("WARNING: not tracing"), Level::Info);
        assert!(Level::Warn < Level::Info);
        assert_eq!(Level::parse_threshold("off"), Ok(None));
        assert_eq!(Level::parse_threshold("INFO"), Ok(Some(Level::Info)));
        assert!(Level::parse_threshold("loud").is_err());
    }

    #[test]
    fn plain_failure_without_tail_is_one_line() {
        let alert = Alert {
            service: "web".into(),
            headline: "exited with 1".into(),
            critical: false,
            tail: vec![],
        };
        let text = strip(&render(&alert, 1.0));
        assert_eq!(text, "[     1.00]  FAILURE  web: exited with 1\r\n");
    }
}
