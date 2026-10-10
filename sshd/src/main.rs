//! A small SSH server for Anyka AK3918 cameras.
//!
//! The stock firmware has no remote shell, and the usual answer — dropping a
//! prebuilt Dropbear onto the SD card — means running a C SSH daemon as root
//! on a device that is often reachable from the LAN. This is the same job in
//! Rust: `russh` for the protocol, a hashed credentials file for passwords,
//! and a PTY spawned without going through a shell.
//!
//! ```text
//! # once, to create a credentials file
//! sshd hash root > /etc/anyka-sshd.passwd
//! chmod 600 /etc/anyka-sshd.passwd
//!
//! # then
//! sshd --port 2222 --passwd /etc/anyka-sshd.passwd \
//!            --host-key /etc/anyka-sshd.key
//! ```
//!
//! Scope, stated plainly: password authentication only, one session channel
//! per connection, no port forwarding, no SFTP, no agent forwarding. Anything
//! not implemented is refused rather than half-supported.
//!
//! Like OpenSSH, a command gets a PTY only when the client asks for one
//! (`ssh -t`); otherwise its stdin, stdout and stderr are plain pipes, so
//! `ssh cam 'cat > file' < file` is byte-exact and ends when the client's
//! input does.
//!
//! Everything runs on one thread, so nothing on it may block: child I/O is
//! asynchronous, and client input is queued in a small bounded buffer whose
//! back-pressure stops this connection from reading its socket — which is
//! what slows an uploading client to the speed of the SD card — while every
//! other connection carries on.

// Linked for its side effect: `os` supplies the `pthread_setname_np`
// shim that the crosstool's uClibc 0.9.33 does not export but Rust's thread
// startup references. Nothing in this crate calls into it directly, so the
// dependency has to be spelled out or it is dropped before linking.
extern crate os;

mod pty;

use ::auth::credentials as auth;
use ::auth::perms;
use auth::Credentials;
use clap::{Parser, Subcommand};
use eyre::{bail, Context, Result};
use metrics::{counter, describe_counter, describe_gauge, gauge};
use pty::{Pty, WindowSize};
use russh::keys::{Algorithm, PrivateKey};
use russh::server::{Auth, ChannelOpenHandle, Handler, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, Notify};

/// Refuse a login that takes longer than this, so a stalled client cannot
/// hold a connection slot open indefinitely.
const AUTH_REJECTION_DELAY: Duration = Duration::from_secs(2);
/// Drop idle connections. The camera has 32 MB of RAM; abandoned sessions are
/// not free.
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(900);
/// Attempts per connection before the server hangs up.
const MAX_AUTH_ATTEMPTS: usize = 4;
/// Client input queued ahead of a child's stdin (on top of the 64 KiB kernel
/// pipe). When it is full, the session stops reading its socket until the
/// child catches up, and TCP slows the client down to the child's pace.
const STDIN_QUEUE_BYTES: usize = 64 * 1024;
/// Extra input accepted while the child's *output* is waiting on the session.
///
/// A parked `data` handler also stops the session from sending output (russh
/// runs handlers inline), so a child like `cat` that must write before it can
/// read more would deadlock against it. While output is waiting, input is
/// accepted up to this much further, which lets the session loop return,
/// flush the output and pick up the client's window adjustments — those sit
/// behind at most [`RECEIVE_WINDOW`] bytes of client data, hence twice that.
const STDIN_RESERVE_BYTES: usize = 2 * RECEIVE_WINDOW as usize;
/// Receive window granted to the client per channel. russh re-grants it as
/// packets arrive rather than as they are consumed, so it bounds how much
/// unread input can be in flight, not how much is buffered; smaller than the
/// 2 MiB default to keep [`STDIN_RESERVE_BYTES`] small. Still ample for
/// SD-card speeds over the camera's Wi-Fi.
const RECEIVE_WINDOW: u32 = 128 * 1024;
/// Read size for child output. russh queues at most `event_buffer_size` (10)
/// of these per session before the reader waits.
const OUTPUT_CHUNK: usize = 8 * 1024;
/// Threads for PBKDF2 verification, the only blocking work left. Keeping it
/// off the runtime thread means a login does not freeze other sessions; the
/// cap keeps a password-guessing burst from spawning threads on a 32 MB device.
const MAX_BLOCKING_THREADS: usize = 2;

/// SSH server for the camera: password login, PTY shells, exec.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    options: Options,
    /// Without a subcommand, serve.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Read a password from stdin and print a credentials line for --passwd.
    Hash {
        user: String,
        /// PBKDF2-HMAC-SHA256 rounds.
        #[arg(long, default_value_t = auth::DEFAULT_ITERATIONS)]
        iterations: u32,
    },
}

#[derive(clap::Args)]
struct Options {
    #[arg(long, default_value = "0.0.0.0")]
    listen: String,
    #[arg(long, default_value_t = 2222)]
    port: u16,
    /// Credentials file: `user:pbkdf2-sha256:iterations:salt:hash` lines, as printed by `hash`.
    #[arg(long, default_value = "/etc/anyka-sshd.passwd")]
    passwd: PathBuf,
    /// Ed25519 host key; generated on first start if missing.
    #[arg(long, default_value = "/etc/anyka-sshd.key")]
    host_key: PathBuf,
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let cli = Cli::parse();
    let _telemetry = telemetry::install("sshd")?;
    describe_metrics();

    match cli.command {
        Some(Command::Hash { user, iterations }) => hash_command(&user, iterations),
        None => serve_command(cli.options),
    }
}

/// `sshd hash USER` — prints a credentials line for the password on stdin.
fn hash_command(user: &str, iterations: u32) -> Result<()> {
    // Read from stdin rather than taking the password as an argument: argv is
    // world-readable through /proc and lands in shell history.
    eprint!("password for {user}: ");
    let mut password = String::new();
    std::io::stdin().read_line(&mut password)?;
    let password = password.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        bail!("empty password");
    }
    eprintln!();

    println!("{}", auth::hash_password(user, password, iterations)?);
    Ok(())
}

fn serve_command(options: Options) -> Result<()> {
    let credentials = Credentials::load(&options.passwd)?;
    let host_key = load_or_create_host_key(&options.host_key)?;
    tracing::info!(
        fingerprint = %host_key.public_key().fingerprint(Default::default()),
        "host key ready"
    );

    let config = russh::server::Config {
        keys: vec![host_key],
        // Password only: public-key and keyboard-interactive are not
        // implemented, so they are not advertised either.
        methods: russh::MethodSet::from(&[russh::MethodKind::Password][..]),
        auth_rejection_time: AUTH_REJECTION_DELAY,
        auth_rejection_time_initial: Some(Duration::ZERO),
        max_auth_attempts: MAX_AUTH_ATTEMPTS,
        inactivity_timeout: Some(INACTIVITY_TIMEOUT),
        window_size: RECEIVE_WINDOW,
        ..Default::default()
    };

    // A current-thread runtime: this serves a handful of interactive sessions,
    // and a thread per core would be pure overhead here.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind((options.listen.as_str(), options.port))
            .await
            .wrap_err_with(|| format!("cannot bind {}:{}", options.listen, options.port))?;
        tracing::info!(listen = %options.listen, port = options.port, "sshd listening");

        let mut server = SshServer {
            credentials: Arc::new(credentials),
        };
        let running = server.run_on_socket(Arc::new(config), &listener);
        let handle = running.handle();

        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("shutting down");
                handle.shutdown("server shutting down".into());
            }
        });

        running.await?;
        Ok::<_, eyre::Report>(())
    })
}

/// Counts sessions currently attached to a shell, for the gauge below.
///
/// `AtomicI32`, not `AtomicI64`: ARMv5TE has no 64-bit atomic instructions, so
/// `std` does not provide `AtomicI64` for this target at all.
static ACTIVE_SESSIONS: AtomicI32 = AtomicI32::new(0);

fn describe_metrics() {
    describe_counter!(
        "kamoder_sshd_auth_attempts_total",
        "Password authentication attempts by outcome"
    );
    describe_counter!(
        "kamoder_sshd_sessions_total",
        "Shell and exec sessions started"
    );
    describe_gauge!(
        "kamoder_sshd_active_sessions",
        "Sessions with a live child process"
    );
}

/// Loads the host key, generating one on first run.
///
/// A host key that other users can read lets them impersonate the camera, so a
/// loosely-permissioned file is refused rather than quietly used.
fn load_or_create_host_key(path: &Path) -> Result<PrivateKey> {
    use std::os::unix::fs::OpenOptionsExt;

    if path.exists() {
        let metadata = std::fs::metadata(path)?;
        if let Some(mode) = perms::too_open(path, &metadata) {
            bail!(
                "{} is accessible to other users (mode {mode:o}); run: chmod 600 {}",
                path.display(),
                path.display()
            );
        }
        let text = std::fs::read_to_string(path)?;
        return Ok(PrivateKey::from_openssh(&text)?);
    }

    tracing::info!(path = %path.display(), "generating a host key");
    let mut rng = rand::rng();
    let key = PrivateKey::random(&mut rng, Algorithm::Ed25519)?;
    let encoded = key.to_openssh(russh::keys::ssh_key::LineEnding::LF)?;

    // Create with 0600 from the start rather than writing and then chmod-ing:
    // otherwise the key is briefly readable by everyone.
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(encoded.as_bytes())?;
    file.sync_all()?;
    Ok(key)
}

#[derive(Clone)]
struct SshServer {
    credentials: Arc<Credentials>,
}

impl russh::server::Server for SshServer {
    type Handler = SshSession;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> Self::Handler {
        SshSession {
            credentials: self.credentials.clone(),
            peer,
            user: None,
            want_pty: false,
            term: "vt100".into(),
            window: WindowSize::default(),
            process: None,
            eof: false,
        }
    }

    fn handle_session_error(&mut self, error: <Self::Handler as Handler>::Error) {
        // Client disconnects are routine and not worth logging as failures.
        if !matches!(error, russh::Error::Disconnect) {
            tracing::warn!(%error, "session error");
        }
    }
}

/// Per-connection state.
struct SshSession {
    credentials: Arc<Credentials>,
    peer: Option<SocketAddr>,
    user: Option<String>,
    /// The client sent `pty-req`: run the next command on a terminal.
    want_pty: bool,
    term: String,
    window: WindowSize,
    /// The shell or command, once started. A second request on the same
    /// channel is refused instead of spawning another process.
    process: Option<Process>,
    /// The client has signalled end of input.
    eof: bool,
}

/// A running shell or command, as seen from the session.
struct Process {
    /// Pid of the child, which leads its own process group (a PTY child is
    /// also a session leader), so `-pid` addresses everything it started.
    pid: i32,
    /// Present for PTY sessions, for resizing.
    pty: Option<Arc<Pty>>,
    /// Client input on its way to the child's stdin, bounded by
    /// [`Flow::queued`]. Dropping it is how EOF reaches the child: the feeder
    /// task drains the queue, then closes stdin.
    stdin: Option<mpsc::UnboundedSender<Vec<u8>>>,
    flow: Arc<Flow>,
    /// Set once the child has been reaped and its pid may be reused.
    reaped: Arc<AtomicBool>,
}

/// Back-pressure bookkeeping shared by a session's `data` handler and the
/// tasks moving its child's I/O.
#[derive(Default)]
struct Flow {
    /// Client input bytes queued for the child and not yet written to it.
    queued: AtomicUsize,
    /// Output tasks currently parked waiting for the session to take data.
    output_waiting: AtomicUsize,
    /// Signalled whenever either of the above changes.
    changed: Notify,
}

impl Flow {
    /// How much input may be queued right now; see [`STDIN_RESERVE_BYTES`].
    fn input_limit(&self) -> usize {
        if self.output_waiting.load(Ordering::Acquire) > 0 {
            STDIN_QUEUE_BYTES + STDIN_RESERVE_BYTES
        } else {
            STDIN_QUEUE_BYTES
        }
    }
}

/// Marks an output task as parked for as long as it lives.
struct OutputWaiting<'a>(&'a Flow);

impl<'a> OutputWaiting<'a> {
    fn new(flow: &'a Flow) -> Self {
        flow.output_waiting.fetch_add(1, Ordering::AcqRel);
        flow.changed.notify_one();
        Self(flow)
    }
}

impl Drop for OutputWaiting<'_> {
    fn drop(&mut self) {
        self.0.output_waiting.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Process {
    /// Sends `SIGHUP` to the child's process group, as a terminal going away
    /// would, so nothing it started outlives the connection.
    fn hangup(&self) {
        if self.reaped.load(Ordering::Acquire) {
            return;
        }
        let group = nix::unistd::Pid::from_raw(-self.pid);
        if let Err(errno) = nix::sys::signal::kill(group, nix::sys::signal::Signal::SIGHUP) {
            tracing::debug!(%errno, pid = self.pid, "cannot signal the child");
        }
    }
}

impl Drop for Process {
    /// The connection is gone, however it ended.
    fn drop(&mut self) {
        self.hangup();
    }
}

impl SshSession {
    fn peer(&self) -> String {
        self.peer
            .map(|addr| addr.to_string())
            .unwrap_or_else(|| "<unknown>".into())
    }

    /// Spawns `argv` — on a PTY if the client asked for one, on pipes
    /// otherwise — and starts moving data between it and the channel.
    async fn start_process(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
        program: &str,
        argv: &[&str],
    ) -> Result<(), russh::Error> {
        if self.process.is_some() {
            session.channel_failure(channel)?;
            return Ok(());
        }

        let env = [
            ("TERM", self.term.clone()),
            ("USER", self.user.clone().unwrap_or_default()),
            ("HOME", home_for(self.user.as_deref())),
            ("PATH", "/usr/sbin:/usr/bin:/sbin:/bin".to_owned()),
        ];

        let spawned = if self.want_pty {
            spawn_on_pty(program, argv, &env, self.window, channel, session.handle())
                .map_err(|error| error.to_string())
        } else {
            spawn_on_pipes(program, argv, &env, channel, session.handle())
                .map_err(|error| error.to_string())
        };
        let mut process = match spawned {
            Ok(process) => process,
            Err(error) => {
                tracing::error!(peer = %self.peer(), program, %error, "cannot start process");
                session.channel_failure(channel)?;
                return Ok(());
            }
        };
        if self.eof {
            // Input already ended (`ssh host cmd < /dev/null` can get here
            // in either order): the child starts with a closed stdin.
            process.stdin = None;
        }

        counter!("kamoder_sshd_sessions_total").increment(1);
        gauge!("kamoder_sshd_active_sessions")
            .set(ACTIVE_SESSIONS.fetch_add(1, Ordering::Relaxed) as f64 + 1.0);
        self.process = Some(process);
        session.channel_success(channel)?;
        Ok(())
    }
}

/// Runs `argv` on a new PTY: the interactive case, `ssh -t`.
fn spawn_on_pty(
    program: &str,
    argv: &[&str],
    env: &[(&str, String)],
    window: WindowSize,
    channel: ChannelId,
    handle: russh::server::Handle,
) -> Result<Process, pty::Error> {
    let env: Vec<String> = env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    let pty = Arc::new(Pty::spawn(program, argv, &env, window)?);
    let io = pty.io()?;

    let flow = Arc::new(Flow::default());
    let (stdin, input) = mpsc::unbounded_channel();
    let feeder = tokio::spawn(feed_stdin(input, io.clone(), flow.clone()));

    let reaped = Arc::new(AtomicBool::new(false));
    let process = Process {
        pid: pty.pid(),
        pty: Some(pty.clone()),
        stdin: Some(stdin),
        flow: flow.clone(),
        reaped: reaped.clone(),
    };

    tokio::spawn(async move {
        forward_output(io, &handle, channel, None, &flow).await;
        // The terminal has closed: the child is exiting, or has exited.
        // Poll rather than block, so no thread is parked on waitpid.
        let mut delay = Duration::from_millis(10);
        let status = loop {
            match pty.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "cannot reap the child");
                    break 1;
                }
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(500));
        };
        reaped.store(true, Ordering::Release);
        finish(feeder, &handle, channel, status).await;
    });
    Ok(process)
}

/// Runs `argv` with its stdio on pipes: `ssh host cmd` without `-t`.
///
/// A PTY here would be actively harmful: its line discipline echoes input
/// back as output, rewrites CR/LF, turns ^C/^Z/^\ in binary data into
/// signals, and has no way to pass on end-of-file — so `cat > file` never
/// finishes.
fn spawn_on_pipes(
    program: &str,
    argv: &[&str],
    env: &[(&str, String)],
    channel: ChannelId,
    handle: russh::server::Handle,
) -> std::io::Result<Process> {
    use std::process::Stdio;

    let (argv0, args) = argv.split_first().unwrap_or((&program, &[]));
    let mut child = tokio::process::Command::new(program)
        .arg0(argv0)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so a hangup reaches whatever it starts.
        .process_group(0)
        .spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| std::io::Error::other("child exited during spawn"))?;
    let (Some(child_stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(std::io::Error::other("child stdio was not piped"));
    };

    let flow = Arc::new(Flow::default());
    let (stdin, input) = mpsc::unbounded_channel();
    let feeder = tokio::spawn(feed_stdin(input, child_stdin, flow.clone()));

    let reaped = Arc::new(AtomicBool::new(false));
    let process = Process {
        pid: pid as i32,
        pty: None,
        stdin: Some(stdin),
        flow: flow.clone(),
        reaped: reaped.clone(),
    };

    tokio::spawn(async move {
        tokio::join!(
            forward_output(stdout, &handle, channel, None, &flow),
            forward_output(
                stderr,
                &handle,
                channel,
                Some(SSH_EXTENDED_DATA_STDERR),
                &flow
            ),
        );
        let status = match child.wait().await {
            Ok(status) => exit_code(status),
            Err(error) => {
                tracing::warn!(%error, "cannot reap the child");
                1
            }
        };
        reaped.store(true, Ordering::Release);
        finish(feeder, &handle, channel, status).await;
    });
    Ok(process)
}

/// `SSH_EXTENDED_DATA_STDERR` from RFC 4254 §5.2.
const SSH_EXTENDED_DATA_STDERR: u32 = 1;

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    // Same convention as the PTY path and the shells: 128 + signal number.
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Copies queued client input into the child, then closes its stdin.
///
/// Every write is asynchronous: a child that is slow to read (a FAT-formatted
/// SD card) or never reads again only ever parks this task.
async fn feed_stdin<W: AsyncWrite + Unpin>(
    mut input: mpsc::UnboundedReceiver<Vec<u8>>,
    mut sink: W,
    flow: Arc<Flow>,
) {
    while let Some(chunk) = input.recv().await {
        let written = sink.write_all(&chunk).await;
        flow.queued.fetch_sub(chunk.len(), Ordering::AcqRel);
        flow.changed.notify_one();
        if let Err(error) = written {
            // The child closed its stdin or exited. Dropping `input` makes
            // further client data be discarded instead of queued.
            tracing::debug!(%error, "child stopped reading its input");
            return;
        }
    }
    // The client sent EOF (or went away) and everything before it has been
    // written. Dropping `sink` closes the pipe's write end: end of file.
    let _ = sink.shutdown().await;
}

/// Forwards one child output stream to the client until end of file.
async fn forward_output<R: AsyncRead + Unpin>(
    mut source: R,
    handle: &russh::server::Handle,
    channel: ChannelId,
    extended: Option<u32>,
    flow: &Flow,
) {
    let mut buffer = vec![0u8; OUTPUT_CHUNK];
    loop {
        let read = match source.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => {
                tracing::debug!(%error, "cannot read child output");
                break;
            }
        };
        let chunk = buffer[..read].to_vec();
        // Awaiting here is the output side's back-pressure: russh holds a
        // bounded number of these per session. If it has to wait, say so,
        // because the session may be parked on input; see `Flow`.
        let send = async {
            match extended {
                None => handle.data(channel, chunk).await,
                Some(code) => handle.extended_data(channel, code, chunk).await,
            }
        };
        tokio::pin!(send);
        let sent = tokio::select! {
            biased;
            sent = &mut send => sent,
            () = std::future::ready(()) => {
                let _waiting = OutputWaiting::new(flow);
                send.await
            }
        };
        if sent.is_err() {
            // The connection is gone; dropping `source` lets the child see
            // EPIPE instead of blocking on a full pipe.
            break;
        }
    }
}

/// Reports the exit status and closes the channel, after all output.
async fn finish(
    feeder: tokio::task::JoinHandle<()>,
    handle: &russh::server::Handle,
    channel: ChannelId,
    status: i32,
) {
    // Input that can no longer be delivered: a grandchild may still hold the
    // child's stdin without reading it, and a PTY master accepts no more once
    // the slave is gone. Either way the feeder would wait forever.
    feeder.abort();
    gauge!("kamoder_sshd_active_sessions")
        .set(ACTIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed) as f64 - 1.0);
    let _ = handle.exit_status_request(channel, status as u32).await;
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}

/// Home directory for a user, read from `/etc/passwd`.
fn home_for(user: Option<&str>) -> String {
    let Some(user) = user else {
        return "/".into();
    };
    let Ok(passwd) = std::fs::read_to_string("/etc/passwd") else {
        return "/".into();
    };
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 6 && fields[0] == user && !fields[5].is_empty() {
            return fields[5].to_owned();
        }
    }
    "/".into()
}

impl Handler for SshSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        // `verify` takes the same time for a known and an unknown user, and
        // russh pads the rejection to `auth_rejection_time` on top of that.
        // It is deliberately slow (PBKDF2), so it runs off the runtime thread:
        // on this CPU it would otherwise stall every other connection.
        let credentials = self.credentials.clone();
        let (name, secret) = (user.to_owned(), password.to_owned());
        let accepted = tokio::task::spawn_blocking(move || credentials.verify(&name, &secret))
            .await
            .unwrap_or(false);
        if accepted {
            counter!("kamoder_sshd_auth_attempts_total", "outcome" => "accepted").increment(1);
            tracing::info!(peer = %self.peer(), user, "authenticated");
            self.user = Some(user.to_owned());
            Ok(Auth::Accept)
        } else {
            counter!("kamoder_sshd_auth_attempts_total", "outcome" => "rejected").increment(1);
            tracing::warn!(peer = %self.peer(), user, "password rejected");
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        columns: u32,
        rows: u32,
        pixel_width: u32,
        pixel_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Bound what goes into the child's environment; TERM is attacker-
        // controlled and ends up in a process the server spawns.
        self.term = term
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .take(32)
            .collect();
        if self.term.is_empty() {
            self.term = "vt100".into();
        }
        self.want_pty = true;
        self.window = WindowSize::from_request(columns, rows, pixel_width, pixel_height);
        if let Some(pty) = self.process.as_ref().and_then(|p| p.pty.as_ref()) {
            let _ = pty.resize(self.window);
        }
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // SAFETY: reading the current uid cannot fail and touches no memory.
        let uid = unsafe { libc::getuid() };
        let shell = pty::login_shell(uid);
        // A leading '-' in argv[0] is how a shell is told it is a login shell.
        let argv0 = format!("-{}", shell.rsplit('/').next().unwrap_or("sh"));
        self.start_process(channel, session, &shell, &[&argv0])
            .await
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Ok(command) = std::str::from_utf8(command) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        // `sh -c` is what every SSH server does for `ssh host 'cmd'`, and the
        // client is already authenticated for a full shell — this grants no
        // capability they did not have. The command is passed as a single
        // argv element, so it is never re-parsed by anything but that shell.
        // SAFETY: reading the current uid cannot fail and touches no memory.
        let shell = pty::login_shell(unsafe { libc::getuid() });
        let argv0 = shell.rsplit('/').next().unwrap_or("sh").to_owned();
        self.start_process(channel, session, &shell, &[&argv0, "-c", command])
            .await
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(process) = self.process.as_mut() else {
            return Ok(());
        };
        let Some(stdin) = process.stdin.clone() else {
            return Ok(());
        };
        let flow = process.flow.clone();
        // Waiting for queue space is the back-pressure: until the child has
        // consumed earlier input, russh does not read this connection's
        // socket, TCP fills up, and the client's sends block. Only this
        // connection waits — the runtime thread is free the whole time.
        loop {
            // Registered before the check, so a change in between is not lost.
            let changed = flow.changed.notified();
            let queued = flow.queued.load(Ordering::Acquire);
            if queued == 0 || queued + data.len() <= flow.input_limit() {
                flow.queued.fetch_add(data.len(), Ordering::AcqRel);
                if stdin.send(data.to_vec()).is_err() {
                    flow.queued.fetch_sub(data.len(), Ordering::AcqRel);
                    process.stdin = None;
                }
                return Ok(());
            }
            tokio::select! {
                () = changed => {}
                // The child stopped reading, or has exited: discard the rest.
                () = stdin.closed() => {
                    process.stdin = None;
                    return Ok(());
                }
            }
        }
    }

    async fn channel_eof(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.eof = true;
        if let Some(process) = self.process.as_mut() {
            // Close the queue; the feeder writes what is left, then closes
            // the child's stdin. On a PTY there is nothing to close — as in
            // OpenSSH, the terminal stays open for output.
            process.stdin = None;
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        columns: u32,
        rows: u32,
        pixel_width: u32,
        pixel_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.window = WindowSize::from_request(columns, rows, pixel_width, pixel_height);
        if let Some(pty) = self.process.as_ref().and_then(|p| p.pty.as_ref()) {
            let _ = pty.resize(self.window);
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Behave like a real terminal going away, so the shell and its job
        // control children shut down instead of lingering.
        if let Some(process) = &self.process {
            process.hangup();
        }
        Ok(())
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    /// clap validates the argument definitions lazily, at parse time; this
    /// catches a clash on the host rather than on the camera.
    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    /// The exact spellings supervisor.toml and init.sh use.
    #[test]
    fn deployed_invocations_parse() {
        let cli = Cli::parse_from([
            "sshd",
            "--port",
            "2222",
            "--passwd",
            "etc/anyka-sshd.passwd",
            "--host-key",
            "etc/anyka-sshd.key",
        ]);
        assert!(cli.command.is_none());
        assert_eq!(cli.options.port, 2222);
        assert_eq!(cli.options.passwd, PathBuf::from("etc/anyka-sshd.passwd"));

        let cli = Cli::parse_from(["sshd", "hash", "root"]);
        assert!(matches!(
            cli.command,
            Some(Command::Hash { ref user, iterations }) if user == "root" && iterations == auth::DEFAULT_ITERATIONS
        ));
    }
}
