//! One actor per supervised process.
//!
//! The actor owns the child, decides whether and when to restart it, and is
//! the only thing that touches that child's state. Process exits arrive as
//! messages rather than being polled for: `tokio::process` drives child reaping
//! off `SIGCHLD`, so an idle supervisor costs nothing.
//!
//! Panics inside this actor are caught by [`Actor::on_panic`] and turned into
//! `ControlFlow::Continue`, so a bug in the restart logic for one service
//! cannot take the supervisor down or orphan the other services.
//!
//! The actor also publishes what it is doing into a [`SharedStatus`] after
//! every transition. The control socket reads that snapshot instead of asking
//! the actor, so a `status` request never queues behind a `stop` that is
//! waiting out a slow child's SIGTERM grace period.

use crate::config::Service;
use crate::logs::{LogSink, Stream};
use ::supervisor::control::{ServiceStatus, State};
use kameo::actor::{Actor, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible, PanicError};
use kameo::message::{Context, Message};
use metrics::{counter, gauge};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How long to wait for the reaper after `SIGKILL`.
const SIGKILL_GRACE: Duration = Duration::from_secs(2);
/// See [`ServiceActor::alert_after_exit`].
const EXIT_ALERT_DELAY: Duration = Duration::from_millis(300);

/// What the supervisor currently knows about one service.
///
/// Lives outside the actor (see [`SharedStatus`]) so it survives the actor
/// being rebuilt after a panic — the restart count keeps counting.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub state: State,
    pub pid: Option<u32>,
    /// When the running process was spawned. An `Instant`, not wall-clock
    /// time: the camera boots at 1970 and jumps when NTP syncs.
    pub started_at: Option<Instant>,
    /// Spawn attempts so far, including failed ones. Restarts are this minus
    /// the first.
    pub spawns: u32,
    pub last_exit_code: Option<i32>,
}

/// Snapshot shared between a service actor (writer) and the root and control
/// socket (readers). A plain mutex: held for a few field copies, never across
/// an `.await`.
pub type SharedStatus = Arc<Mutex<Snapshot>>;

/// A fresh snapshot for a service that has not been started yet.
pub fn new_status(enabled: bool) -> SharedStatus {
    Arc::new(Mutex::new(Snapshot {
        state: if enabled {
            State::Stopped
        } else {
            State::Disabled
        },
        pid: None,
        started_at: None,
        spawns: 0,
        last_exit_code: None,
    }))
}

/// Locks a snapshot, ignoring poisoning: the data is a handful of plain
/// fields that cannot be left half-updated in a way that matters.
fn lock(status: &SharedStatus) -> MutexGuard<'_, Snapshot> {
    status.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Everything the actor needs to run one service.
#[derive(Debug, Clone)]
pub struct ServiceArgs {
    pub service: Service,
    /// Absolute path to the executable.
    pub program: PathBuf,
    pub working_dir: PathBuf,
    pub logs: LogSink,
    pub status: SharedStatus,
}

impl ServiceArgs {
    /// The service's status in wire form.
    pub fn report(&self) -> ServiceStatus {
        let snapshot = lock(&self.status).clone();
        ServiceStatus {
            name: self.service.name.clone(),
            enabled: self.service.enabled,
            critical: self.service.critical,
            state: snapshot.state,
            pid: snapshot.pid,
            uptime_secs: snapshot.started_at.map(|at| at.elapsed().as_secs()),
            restarts: snapshot.spawns.saturating_sub(1),
            last_exit_code: snapshot.last_exit_code,
        }
    }
}

/// Start the service on request: clears a previous [`Stop`] and a given-up
/// crash loop, and cancels a pending backoff in favour of starting now.
#[derive(Debug, Clone, Copy)]
pub struct Start;

/// A backoff delay elapsed. Carries the generation it was scheduled under, so
/// a timer that was cancelled after it already fired is ignored.
#[derive(Debug, Clone, Copy)]
pub struct RestartDue(u32);

/// The child exited. Carries the decoded status.
#[derive(Debug, Clone, Copy)]
pub struct Exited {
    /// Which process exited. An exit that arrives after a `restart` already
    /// spawned the replacement must not be mistaken for the new one dying.
    pub pid: Pid,
    /// Exit code, or `None` if it was killed by a signal.
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl Exited {
    fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Exit code in shell convention: the code, or 128 + signal.
    fn shell_code(&self) -> Option<i32> {
        self.code.or_else(|| self.signal.map(|signal| 128 + signal))
    }

    fn describe(&self) -> String {
        match (self.code, self.signal) {
            (Some(code), _) => format!("exit code {code}"),
            (None, Some(signal)) => format!("killed by signal {signal}"),
            _ => "unknown status".to_owned(),
        }
    }
}

/// Stop the service and do not restart it until the next [`Start`].
#[derive(Debug, Clone, Copy)]
pub struct Stop;

/// Whether the service currently has a running process.
#[derive(Debug, Clone, Copy)]
pub struct IsRunning;

/// A child that has been spawned and not yet reaped.
///
/// The `Child` itself lives in the waiter task, because `Child::wait` needs
/// `&mut` and would otherwise be borrowed across the actor's whole lifetime.
/// The actor keeps what it needs to act on the process: its pid, to signal,
/// and a watch channel that flips when the waiter has reaped it.
#[derive(Debug)]
struct Running {
    pid: Pid,
    exited: watch::Receiver<bool>,
}

/// Supervised process state machine.
pub struct ServiceActor {
    args: ServiceArgs,
    running: Option<Running>,
    /// Restart timestamps inside the current window, for the crash-loop guard.
    recent_restarts: VecDeque<Instant>,
    consecutive_failures: u32,
    /// Set once the crash-loop guard fires; cleared by [`Start`].
    gave_up: bool,
    /// Stopped on request; no restarts until the next [`Start`].
    held: bool,
    /// The actor itself is being torn down.
    shutting_down: bool,
    /// The pending backoff timer, if a restart is scheduled.
    restart_timer: Option<JoinHandle<()>>,
    /// Bumped whenever a timer is scheduled or cancelled; see [`RestartDue`].
    generation: u32,
}

impl ServiceActor {
    fn name(&self) -> &str {
        &self.args.service.name
    }

    /// Reports a failure on the serial console with the service's recent
    /// log lines.
    fn alert(&self, headline: impl Into<String>) {
        self.args
            .logs
            .alert(self.name(), headline, self.args.service.critical);
    }

    /// Like [`Self::alert`], but a moment later, so the dying process's last
    /// output — still in the pipe when SIGCHLD arrives — makes the tail.
    fn alert_after_exit(&self, headline: String) {
        let logs = self.args.logs.clone();
        let name = self.name().to_owned();
        let critical = self.args.service.critical;
        tokio::spawn(async move {
            tokio::time::sleep(EXIT_ALERT_DELAY).await;
            logs.alert(&name, headline, critical);
        });
    }

    /// Exponential backoff, capped, derived from the consecutive failure count.
    fn backoff(&self) -> Duration {
        let service = &self.args.service;
        let initial = service.backoff_initial();
        let shift = self.consecutive_failures.saturating_sub(1).min(16);
        let scaled = initial.saturating_mul(1u32 << shift);
        scaled.min(service.backoff_max())
    }

    /// True once too many restarts have happened inside the window.
    fn crash_looping(&mut self) -> bool {
        let window = self.args.service.restart_window();
        let now = Instant::now();
        while let Some(oldest) = self.recent_restarts.front() {
            if now.duration_since(*oldest) > window {
                self.recent_restarts.pop_front();
            } else {
                break;
            }
        }
        self.recent_restarts.len() as u32 > self.args.service.max_restarts
    }

    /// Writes the current state into the shared snapshot. Called after every
    /// transition, so readers never see a state the actor has left.
    fn publish(&self) {
        let state = if self.running.is_some() {
            State::Running
        } else if self.restart_timer.is_some() {
            State::BackingOff
        } else if self.gave_up {
            State::GaveUp
        } else if !self.args.service.enabled {
            State::Disabled
        } else {
            State::Stopped
        };
        let mut snapshot = lock(&self.args.status);
        snapshot.state = state;
        if self.running.is_none() {
            snapshot.pid = None;
            snapshot.started_at = None;
        }
    }

    /// Drops a pending restart, if any. A `RestartDue` already in the mailbox
    /// is invalidated by the generation bump.
    fn cancel_restart(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(timer) = self.restart_timer.take() {
            timer.abort();
        }
    }

    /// Spawns the process and wires its output into the log sink.
    async fn spawn_process(&mut self, actor_ref: &ActorRef<Self>) {
        {
            let mut snapshot = lock(&self.args.status);
            snapshot.spawns = snapshot.spawns.saturating_add(1);
        }
        let service = &self.args.service;
        let mut command = Command::new(&self.args.program);
        command
            .args(&service.args)
            .current_dir(&self.args.working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The supervisor's own log file must not be inherited. Every Rust
        // service reads ANYKA_LOG_FILE, so passing it on made all of them
        // append to — and rotate — supervisor.log concurrently, while their
        // own <service>.log stayed empty. Without it they log to stderr,
        // which is piped into the per-service rotating sink below. A service
        // that really wants its own file can still set it in `env`.
        command.env_remove(telemetry::LOG_FILE_ENV);
        for (key, value) in &service.env {
            command.env(key, value);
        }
        // Give the child its own process group so a stray `kill` on the group
        // cannot take the supervisor with it, and so signals can be sent to
        // the whole tree later if that is ever needed.
        command.process_group(0);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::error!(
                    service = self.name(),
                    program = %self.args.program.display(),
                    %error,
                    "cannot spawn service"
                );
                self.args
                    .logs
                    .note(self.name(), format!("spawn failed: {error}"));
                counter!("kamoder_supervisor_spawn_failures_total", "service" => self.name().to_owned())
                    .increment(1);
                lock(&self.args.status).last_exit_code = Some(127);
                self.alert(format!(
                    "cannot start {}: {error}",
                    self.args.program.display()
                ));
                // Treat a failed spawn like a failed run, so backoff applies
                // and the crash-loop guard can eventually give up.
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.schedule_restart(actor_ref, "spawn failed");
                return;
            }
        };

        let Some(raw_pid) = child.id() else {
            tracing::error!(
                service = self.name(),
                "child vanished before it could be tracked"
            );
            self.publish();
            return;
        };
        let pid = Pid::from_raw(raw_pid as i32);
        tracing::info!(service = self.name(), pid = raw_pid, "service started");
        self.args
            .logs
            .note(self.name(), format!("started, pid {raw_pid}"));
        gauge!("kamoder_supervisor_service_up", "service" => self.name().to_owned()).set(1.0);

        if let Some(stdout) = child.stdout.take() {
            pump(
                self.name().to_owned(),
                self.args.logs.clone(),
                stdout,
                Stream::Stdout,
            );
        }
        if let Some(stderr) = child.stderr.take() {
            pump(
                self.name().to_owned(),
                self.args.logs.clone(),
                stderr,
                Stream::Stderr,
            );
        }

        // The waiter task owns the `Child` and is the only thing that reaps
        // it, so no zombie survives a restart. It is woken by tokio's
        // SIGCHLD-driven process driver — this is the event-driven part, and
        // nothing polls.
        let (exited_tx, exited_rx) = watch::channel(false);
        let actor_ref = actor_ref.clone();
        let service = self.name().to_owned();
        tokio::spawn(async move {
            let status = child.wait().await;
            let exited = match status {
                Ok(status) => {
                    use std::os::unix::process::ExitStatusExt;
                    Exited {
                        pid,
                        code: status.code(),
                        signal: status.signal(),
                    }
                }
                Err(error) => {
                    tracing::warn!(service, %error, "cannot wait for child");
                    Exited {
                        pid,
                        code: None,
                        signal: None,
                    }
                }
            };
            // Release `terminate` first, then report for the restart path. If
            // the actor is gone the supervisor is shutting down and there is
            // nothing to report to.
            let _ = exited_tx.send(true);
            let _ = actor_ref.tell(exited).await;
        });

        self.running = Some(Running {
            pid,
            exited: exited_rx,
        });
        {
            let mut snapshot = lock(&self.args.status);
            snapshot.pid = Some(raw_pid);
            snapshot.started_at = Some(Instant::now());
        }
        self.publish();
    }

    /// Queues a restart after the backoff has elapsed.
    fn schedule_restart(&mut self, actor_ref: &ActorRef<Self>, why: &str) {
        let delay = self.backoff();
        tracing::warn!(
            service = self.name(),
            status = why,
            backoff_ms = delay.as_millis() as u64,
            "service exited, restarting"
        );
        self.args.logs.note(
            self.name(),
            format!("{why}, restarting in {} ms", delay.as_millis()),
        );

        self.cancel_restart();
        let generation = self.generation;
        let actor_ref = actor_ref.clone();
        self.restart_timer = Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = actor_ref.tell(RestartDue(generation)).await;
        }));
        self.publish();
    }

    /// Sends `SIGTERM`, then `SIGKILL` if the process has not gone by the
    /// deadline.
    ///
    /// Waits on the watch channel rather than on `Child::wait`, because the
    /// waiter task owns the `Child` and is the one doing the reaping.
    async fn terminate(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        let name = self.name().to_owned();
        let logs = self.args.logs.clone();
        let timeout = self.args.service.stop_timeout();

        tracing::info!(service = %name, pid = running.pid.as_raw(), "stopping service");
        logs.note(&name, "stopping (SIGTERM)");
        if let Err(errno) = kill(running.pid, Signal::SIGTERM) {
            // ESRCH just means it beat us to it.
            tracing::debug!(service = %name, %errno, "cannot signal service");
        }

        let mut exited = running.exited;
        if tokio::time::timeout(timeout, exited.wait_for(|done| *done))
            .await
            .is_err()
        {
            tracing::warn!(
                service = %name,
                timeout_secs = timeout.as_secs(),
                "service did not stop, sending SIGKILL"
            );
            logs.note(&name, "did not stop in time (SIGKILL)");
            logs.alert(
                &name,
                format!(
                    "did not stop within {} s, killed with SIGKILL",
                    timeout.as_secs()
                ),
                self.args.service.critical,
            );
            let _ = kill(running.pid, Signal::SIGKILL);
            // SIGKILL cannot be caught, so this only waits for the waiter task
            // to be scheduled. Still bounded, so a stuck uninterruptible child
            // cannot hang shutdown.
            let _ = tokio::time::timeout(SIGKILL_GRACE, exited.wait_for(|done| *done)).await;
        }
        gauge!("kamoder_supervisor_service_up", "service" => name).set(0.0);
    }
}

impl Actor for ServiceActor {
    type Args = ServiceArgs;
    type Error = Infallible;

    fn name() -> &'static str {
        "service"
    }

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let mut actor = ServiceActor {
            args,
            running: None,
            recent_restarts: VecDeque::new(),
            consecutive_failures: 0,
            gave_up: false,
            held: false,
            shutting_down: false,
            restart_timer: None,
            generation: 0,
        };
        gauge!("kamoder_supervisor_service_up", "service" => actor.name().to_owned()).set(0.0);
        actor.spawn_process(&actor_ref).await;
        Ok(actor)
    }

    /// Keep going after a panic.
    ///
    /// This is the point of running each service as its own actor: a bug in
    /// the restart logic for one service becomes a logged panic and a
    /// continuing mailbox, not a dead supervisor and seven orphaned processes.
    async fn on_panic(
        &mut self,
        actor_ref: WeakActorRef<Self>,
        err: PanicError,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        tracing::error!(service = self.name(), panic = %err, "service actor panicked, continuing");
        self.args
            .logs
            .note(self.name(), format!("supervisor actor panicked: {err}"));
        self.alert(format!("supervisor actor panicked: {err}"));
        counter!("kamoder_supervisor_actor_panics_total", "service" => self.name().to_owned())
            .increment(1);

        // If the panic left us with no child and no pending restart, get the
        // service running again rather than leaving it silently stopped.
        if self.running.is_none()
            && self.restart_timer.is_none()
            && !self.gave_up
            && !self.held
            && !self.shutting_down
        {
            if let Some(actor_ref) = actor_ref.upgrade() {
                let _ = actor_ref.tell(RestartDue(self.generation)).await;
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        self.shutting_down = true;
        self.cancel_restart();
        self.terminate().await;
        self.publish();
        Ok(())
    }
}

impl Message<Start> for ServiceActor {
    type Reply = ();

    async fn handle(&mut self, _msg: Start, ctx: &mut Context<Self, Self::Reply>) {
        if self.shutting_down {
            return;
        }
        // An explicit start is a human saying "try again": forget the
        // hold, the crash-loop verdict and the backoff history.
        self.held = false;
        self.gave_up = false;
        self.recent_restarts.clear();
        self.consecutive_failures = 0;
        self.cancel_restart();
        if self.running.is_some() {
            self.publish();
            return;
        }
        self.args.logs.note(self.name(), "start requested");
        if lock(&self.args.status).spawns > 0 {
            counter!("kamoder_supervisor_restarts_total", "service" => self.name().to_owned())
                .increment(1);
        }
        self.spawn_process(&ctx.actor_ref()).await;
    }
}

impl Message<RestartDue> for ServiceActor {
    type Reply = ();

    async fn handle(&mut self, msg: RestartDue, ctx: &mut Context<Self, Self::Reply>) {
        if msg.0 != self.generation {
            // Cancelled after it had already fired.
            return;
        }
        self.restart_timer = None;
        if self.shutting_down || self.held || self.gave_up || self.running.is_some() {
            self.publish();
            return;
        }
        self.recent_restarts.push_back(Instant::now());
        if self.crash_looping() {
            self.gave_up = true;
            tracing::error!(
                service = self.name(),
                max_restarts = self.args.service.max_restarts,
                window_secs = self.args.service.restart_window().as_secs(),
                "service restarted too often, giving up"
            );
            self.args
                .logs
                .note(self.name(), "restarted too often, giving up");
            self.alert(format!(
                "restarted more than {} times in {} s, giving up (start it again from the web UI)",
                self.args.service.max_restarts,
                self.args.service.restart_window().as_secs()
            ));
            counter!("kamoder_supervisor_gave_up_total", "service" => self.name().to_owned())
                .increment(1);
            gauge!("kamoder_supervisor_service_up", "service" => self.name().to_owned()).set(0.0);
            self.publish();
            return;
        }
        counter!("kamoder_supervisor_restarts_total", "service" => self.name().to_owned())
            .increment(1);
        self.spawn_process(&ctx.actor_ref()).await;
    }
}

impl Message<Exited> for ServiceActor {
    type Reply = ();

    async fn handle(&mut self, msg: Exited, ctx: &mut Context<Self, Self::Reply>) {
        lock(&self.args.status).last_exit_code = msg.shell_code();
        gauge!("kamoder_supervisor_last_exit_code", "service" => self.name().to_owned())
            .set(msg.code.unwrap_or(-1) as f64);

        // `terminate` already let go of a process it stopped, and a `restart`
        // may have spawned its replacement before this exit was delivered.
        let current = self
            .running
            .as_ref()
            .is_some_and(|running| running.pid == msg.pid);
        if !current {
            self.publish();
            return;
        }
        self.running = None;
        gauge!("kamoder_supervisor_service_up", "service" => self.name().to_owned()).set(0.0);

        if self.shutting_down || self.held {
            self.publish();
            return;
        }

        if msg.success() {
            self.consecutive_failures = 0;
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            self.alert_after_exit(msg.describe());
        }

        if !self.args.service.restart.should_restart(msg.success()) {
            tracing::info!(
                service = self.name(),
                status = %msg.describe(),
                policy = ?self.args.service.restart,
                "service exited, not restarting"
            );
            self.args
                .logs
                .note(self.name(), format!("{}, not restarting", msg.describe()));
            self.publish();
            return;
        }

        self.schedule_restart(&ctx.actor_ref(), &msg.describe());
    }
}

impl Message<Stop> for ServiceActor {
    type Reply = ();

    async fn handle(&mut self, _msg: Stop, _ctx: &mut Context<Self, Self::Reply>) {
        self.held = true;
        self.cancel_restart();
        self.terminate().await;
        self.publish();
    }
}

impl Message<IsRunning> for ServiceActor {
    type Reply = bool;

    async fn handle(&mut self, _msg: IsRunning, _ctx: &mut Context<Self, Self::Reply>) -> bool {
        self.running.is_some()
    }
}

/// Forwards one of the child's streams into the log sink, line by line.
fn pump<R>(service: String, logs: LogSink, reader: R, stream: Stream)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        // `next_line` bounds each line internally at the buffer's growth, and
        // the sink applies backpressure, so a chatty child cannot balloon RAM.
        while let Ok(Some(line)) = lines.next_line().await {
            logs.line(&service, stream, line);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Restart;

    fn service(initial_ms: u64, max_ms: u64) -> Service {
        Service {
            name: "test".into(),
            command: "/bin/true".into(),
            args: vec![],
            env: vec![],
            working_dir: None,
            enabled: true,
            restart: Restart::Always,
            critical: false,
            backoff_initial_ms: initial_ms,
            backoff_max_ms: max_ms,
            max_restarts: 3,
            restart_window_secs: 60,
            stop_timeout_secs: 1,
        }
    }

    fn actor(service: Service, failures: u32) -> ServiceActor {
        ServiceActor {
            args: ServiceArgs {
                service,
                program: PathBuf::from("/bin/true"),
                working_dir: PathBuf::from("/"),
                logs: crate::logs::LogSink::discarding(),
                status: new_status(true),
            },
            running: None,
            recent_restarts: VecDeque::new(),
            consecutive_failures: failures,
            gave_up: false,
            held: false,
            shutting_down: false,
            restart_timer: None,
            generation: 0,
        }
    }

    #[test]
    fn backoff_grows_then_caps() {
        let cfg = service(100, 1_000);
        assert_eq!(actor(cfg.clone(), 1).backoff(), Duration::from_millis(100));
        assert_eq!(actor(cfg.clone(), 2).backoff(), Duration::from_millis(200));
        assert_eq!(actor(cfg.clone(), 4).backoff(), Duration::from_millis(800));
        // Capped, and no overflow at absurd failure counts.
        assert_eq!(
            actor(cfg.clone(), 9).backoff(),
            Duration::from_millis(1_000)
        );
        assert_eq!(actor(cfg, u32::MAX).backoff(), Duration::from_millis(1_000));
    }

    #[test]
    fn crash_loop_guard_counts_within_the_window() {
        let mut actor = actor(service(1, 1), 0);
        for _ in 0..actor.args.service.max_restarts {
            actor.recent_restarts.push_back(Instant::now());
        }
        assert!(
            !actor.crash_looping(),
            "exactly max_restarts is still allowed"
        );
        actor.recent_restarts.push_back(Instant::now());
        assert!(actor.crash_looping(), "one more than max_restarts gives up");
    }

    #[test]
    fn exit_status_is_described() {
        assert!(Exited {
            pid: Pid::from_raw(1),
            code: Some(0),
            signal: None
        }
        .success());
        assert!(!Exited {
            pid: Pid::from_raw(1),
            code: Some(1),
            signal: None
        }
        .success());
        assert!(!Exited {
            pid: Pid::from_raw(1),
            code: None,
            signal: Some(9)
        }
        .success());
        assert_eq!(
            Exited {
                pid: Pid::from_raw(1),
                code: None,
                signal: Some(15)
            }
            .describe(),
            "killed by signal 15"
        );
        assert_eq!(
            Exited {
                pid: Pid::from_raw(1),
                code: None,
                signal: Some(15)
            }
            .shell_code(),
            Some(143)
        );
    }
}
