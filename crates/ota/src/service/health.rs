//! The trial health check that commits a freshly booted slot (OTA-11).
//!
//! After an install the new slot boots as a *trial*: `kboot guard` reboots
//! into the old slot unless the record says "successful" within the trial
//! window (10 min). This module decides when to say it:
//!
//! ```text
//! every poll (15 s):
//!   supervisor status   every enabled service running (or stopped with
//!                       exit 0: a service that has nothing to do on this
//!                       board), and no restart since the last poll
//!   web                 TCP connect + "GET / HTTP/1.0" → 2xx/3xx status line
//!   own manifest        slots/<x>/manifest.json(.minisig) verifies against
//!                       the built-in keys (skipped without keys or manifest)
//! all good continuously for `stable` (180 s) ⇒ mark_successful(booted)
//! ```
//!
//! A failing check resets the clock; the reasons are published as
//! `waiting_on` in the status. Nothing here reboots: if the slot never gets
//! healthy the guard does, and the boot shim falls back. The decision part
//! ([`Tracker`], [`evaluate_services`]) is pure; the probes are small async
//! functions with timeouts.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use ota_manifest::TrustedKeys;
use supervisor::control::{self, ServiceStatus, State};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::kup::{MANIFEST_NAME, SIGNATURE_NAME};

/// Timeout of each probe.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Thresholds of the health check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthConfig {
    /// Time between probes.
    pub poll: Duration,
    /// How long every probe must pass without interruption.
    pub stable: Duration,
    /// The boot shim's trial window, for the deadline shown in the status.
    pub trial_window: Duration,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(15),
            stable: Duration::from_secs(180),
            trial_window: Duration::from_secs(600),
        }
    }
}

/// One round of probes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    /// What is not healthy (empty = healthy).
    pub waiting_on: Vec<String>,
    /// `(service, restarts)` from the supervisor, to notice restarts
    /// between polls.
    pub restarts: Vec<(String, u32)>,
}

/// Turns probes into "healthy for how long".
#[derive(Debug, Default)]
pub struct Tracker {
    since: Option<Instant>,
    restarts: HashMap<String, u32>,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one probe taken at `now`; returns how long every probe has
    /// passed without interruption, and adds a `restarted` reason to
    /// `probe.waiting_on` for services whose restart count moved.
    pub fn observe(&mut self, probe: &mut Probe, now: Instant) -> Duration {
        for (name, restarts) in &probe.restarts {
            if self.restarts.get(name).is_some_and(|old| old != restarts) {
                probe.waiting_on.push(format!("service:{name} (restarted)"));
            }
        }
        self.restarts = probe.restarts.iter().cloned().collect();
        if !probe.waiting_on.is_empty() {
            self.since = None;
            return Duration::ZERO;
        }
        now.saturating_duration_since(*self.since.get_or_insert(now))
    }
}

/// Reasons from a supervisor status listing: every enabled service must be
/// running, or stopped after a clean exit (e.g. `ntp` with NTP disabled,
/// `follow` on a board without PTZ follow, both `restart = "on-failure"`).
pub fn evaluate_services(services: &[ServiceStatus]) -> Vec<String> {
    services
        .iter()
        .filter(|s| s.enabled)
        .filter(|s| match s.state {
            State::Running | State::Disabled => false,
            State::Stopped => s.last_exit_code != Some(0),
            State::BackingOff | State::GaveUp => true,
        })
        .map(|s| format!("service:{} ({})", s.name, state_name(s.state)))
        .collect()
}

fn state_name(state: State) -> &'static str {
    match state {
        State::Running => "running",
        State::Stopped => "stopped",
        State::BackingOff => "backing_off",
        State::GaveUp => "gave_up",
        State::Disabled => "disabled",
    }
}

/// Asks the supervisor; adds reasons and restart counts to `probe`.
pub async fn probe_supervisor(socket: &Path, probe: &mut Probe) {
    let client = control::Client::new(socket).with_timeout(PROBE_TIMEOUT);
    match client.status().await {
        Ok(control::Response::Services { services }) => {
            probe.waiting_on.extend(evaluate_services(&services));
            probe.restarts = services
                .iter()
                .filter(|s| s.enabled)
                .map(|s| (s.name.clone(), s.restarts))
                .collect();
        }
        Ok(other) => {
            tracing::debug!(response = ?other, "unexpected supervisor answer");
            probe.waiting_on.push("supervisor".into());
        }
        Err(error) => {
            tracing::debug!(%error, "supervisor not reachable");
            probe.waiting_on.push("supervisor".into());
        }
    }
}

/// Whether the web server at `addr` answers `GET /` with a 2xx or 3xx
/// status line. Plain TCP: one request, one line read.
pub async fn probe_web(addr: SocketAddr) -> bool {
    let attempt = async {
        let mut stream = TcpStream::connect(addr).await?;
        stream
            .write_all(b"GET / HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await?;
        let mut head = [0u8; 64];
        let mut len = 0;
        while len < head.len() {
            let n = stream.read(&mut head[len..]).await?;
            if n == 0 {
                break;
            }
            len += n;
            if head[..len].contains(&b'\n') {
                break;
            }
        }
        Ok::<_, std::io::Error>(status_ok(&head[..len]))
    };
    match tokio::time::timeout(PROBE_TIMEOUT, attempt).await {
        Ok(Ok(ok)) => ok,
        Ok(Err(error)) => {
            tracing::debug!(%addr, %error, "web probe failed");
            false
        }
        Err(_) => false,
    }
}

/// `HTTP/1.x NNN …` with NNN in 200..400.
fn status_ok(head: &[u8]) -> bool {
    let line = head.split(|b| *b == b'\n').next().unwrap_or(&[]);
    let text = String::from_utf8_lossy(line);
    let mut parts = text.split_whitespace();
    let proto = parts.next().unwrap_or("");
    let code = parts.next().and_then(|c| c.parse::<u16>().ok());
    proto.starts_with("HTTP/1.") && code.is_some_and(|c| (200..400).contains(&c))
}

/// Verifies the slot's own signed manifest copy. `Ok` when it verifies, when
/// no key is built in, or when the slot has no manifest (a legacy tree).
pub fn verify_own_manifest(slot_dir: &Path, keys: &TrustedKeys) -> Result<(), String> {
    if keys.is_empty() {
        return Ok(());
    }
    let json = match std::fs::read(slot_dir.join(MANIFEST_NAME)) {
        Ok(json) => json,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot read {MANIFEST_NAME}: {e}")),
    };
    let signature = std::fs::read_to_string(slot_dir.join(SIGNATURE_NAME))
        .map_err(|e| format!("cannot read {SIGNATURE_NAME}: {e}"))?;
    ota_manifest::verify(&json, &signature, keys)
        .map(drop)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, enabled: bool, state: State, exit: Option<i32>) -> ServiceStatus {
        ServiceStatus {
            name: name.into(),
            enabled,
            critical: false,
            state,
            pid: None,
            uptime_secs: None,
            restarts: 0,
            last_exit_code: exit,
        }
    }

    #[test]
    fn services_that_count() {
        let list = [
            svc("web", true, State::Running, None),
            svc("ntp", true, State::Stopped, Some(0)),
            svc("rtsp", true, State::BackingOff, Some(1)),
            svc("sshd", true, State::Stopped, Some(1)),
            svc("onvif", true, State::GaveUp, Some(101)),
            svc("demo", false, State::Disabled, None),
            svc("off", false, State::Stopped, Some(1)),
        ];
        assert_eq!(
            evaluate_services(&list),
            [
                "service:rtsp (backing_off)",
                "service:sshd (stopped)",
                "service:onvif (gave_up)"
            ]
        );
    }

    #[test]
    fn tracker_needs_an_uninterrupted_run() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let ok = |restarts: u32| Probe {
            waiting_on: Vec::new(),
            restarts: vec![("web".into(), restarts)],
        };
        let mut t = Tracker::new();
        assert_eq!(t.observe(&mut ok(0), t0), s(0));
        assert_eq!(t.observe(&mut ok(0), t0 + s(15)), s(15));
        // A failure resets.
        let mut bad = Probe {
            waiting_on: vec!["web".into()],
            restarts: vec![("web".into(), 0)],
        };
        assert_eq!(t.observe(&mut bad, t0 + s(30)), s(0));
        assert_eq!(t.observe(&mut ok(0), t0 + s(45)), s(0));
        assert_eq!(t.observe(&mut ok(0), t0 + s(60)), s(15));
        // A restart between polls also resets, and says so.
        let mut restarted = ok(1);
        assert_eq!(t.observe(&mut restarted, t0 + s(75)), s(0));
        assert_eq!(restarted.waiting_on, ["service:web (restarted)"]);
        assert_eq!(t.observe(&mut ok(1), t0 + s(90)), s(0));
        assert_eq!(t.observe(&mut ok(1), t0 + s(270)), s(180));
    }

    #[test]
    fn status_lines() {
        for (head, ok) in [
            (&b"HTTP/1.1 200 OK\r\n"[..], true),
            (b"HTTP/1.0 302 Found\r\n", true),
            (b"HTTP/1.1 404 Not Found\r\n", false),
            (b"HTTP/1.1 500 Oops\r\n", false),
            (b"SSH-2.0-x\r\n", false),
            (b"", false),
        ] {
            assert_eq!(status_ok(head), ok, "{}", String::from_utf8_lossy(head));
        }
    }
}
