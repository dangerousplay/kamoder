//! Shared server state: configuration, the session key, the cached users
//! table and the login throttles.
//!
//! One [`AppState`] (an `Arc`) is cloned into every handler. Everything in it
//! is either immutable or behind a short `std::sync::Mutex`: the runtime is
//! current-thread, so contention only comes from the blocking pool, and no
//! lock is ever held across an `.await` or a PBKDF2 derivation.

use crate::rate_limit::RateLimiter;
use crate::stream_token::StreamTokens;
use ::auth::{Jwt, Role, WebUsers};
use snafu::{ResultExt, Snafu};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{watch, Semaphore};

/// How often, at most, a request re-`stat`s the users file.
pub const USERS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// Minimum time a failed login takes, so a quick answer does not reveal
/// anything and a password guesser gets at most one try per second.
pub const LOGIN_FAILURE_DELAY: Duration = Duration::from_secs(1);
/// Concurrent log streams (SSE). Each holds a file descriptor and a poll
/// timer; the cap keeps a forgotten pile of browser tabs from adding up.
pub const MAX_LOG_STREAMS: usize = 4;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot load the users file: {source}"))]
    Users { source: ::auth::web_users::Error },

    #[snafu(display("cannot load the session key: {source}"))]
    Key { source: ::auth::jwt::KeyError },
}

/// Everything the command line configures. Paths are relative to the
/// working directory, which the supervisor sets to its config directory
/// (`anyka_rust/`), like every other service.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    /// Precompressed SPA (`webui/dist` as staged by `./x webui`).
    pub www: PathBuf,
    /// `etc/web-users`.
    pub users: PathBuf,
    /// `etc/web-jwt.key`, created on first start.
    pub jwt_key: PathBuf,
    pub token_ttl: Duration,
    /// Supervisor log directory (T13).
    pub logs: PathBuf,
    /// `camera.conf` (T14).
    pub camera_conf: PathBuf,
    /// `supervisor` control socket (T12).
    pub supervisor_socket: PathBuf,
    /// `onvif` PTZ socket (T16).
    pub ptz_socket: PathBuf,
    /// `lights` ownership socket (IND-07).
    pub lights_socket: PathBuf,
    /// `jpeg_snapshot` URL proxied by `/api/snapshot.jpg` (T16).
    pub snapshot_url: String,
    /// Pause between frames of `/api/live.mjpeg`.
    pub mjpeg_interval: Duration,
    /// `etc/onvif.secret`: the ONVIF account, `user:password` (T15).
    pub onvif_secret: PathBuf,
    /// `init.sh`, whose `--stop` stops the stack cleanly before a reboot.
    pub init_script: PathBuf,
    /// Where `wifi` looks for things; `/api/wifi` reads the same.
    pub wifi: wifi::system::Paths,
}

/// Knobs tests turn down; production uses [`Tuning::default`].
#[derive(Debug, Clone)]
pub struct Tuning {
    pub users_refresh: Duration,
    pub login_failure_delay: Duration,
    /// How often a log stream re-`stat`s its file (D7: no inotify on FAT).
    pub log_poll: Duration,
    /// Idle log streams send an SSE comment this often, so proxies and the
    /// browser keep the connection and a vanished client is noticed.
    pub keepalive: Duration,
    /// Whole snapshot round trip, connect to last byte.
    pub snapshot_timeout: Duration,
    /// Between answering `POST /api/system/reboot` and starting to reboot,
    /// so the response gets out.
    pub reboot_delay: Duration,
    /// What reboot does; tests swap in a recorder.
    pub reboot: fn(&Config),
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            users_refresh: USERS_REFRESH_INTERVAL,
            login_failure_delay: LOGIN_FAILURE_DELAY,
            log_poll: Duration::from_millis(500),
            keepalive: Duration::from_secs(15),
            snapshot_timeout: Duration::from_secs(3),
            reboot_delay: Duration::from_secs(1),
            reboot: crate::system::reboot_now,
        }
    }
}

/// Cheaply cloneable handle to the shared state.
#[derive(Debug, Clone)]
pub struct AppState(Arc<Inner>);

#[derive(Debug)]
pub struct Inner {
    pub config: Config,
    pub tuning: Tuning,
    pub jwt: Jwt,
    users: Mutex<CachedUsers>,
    /// Failed-login bookkeeping per client IP.
    pub limiter: Mutex<RateLimiter>,
    /// Short-lived tokens for `<img>`/`EventSource` URLs.
    pub stream_tokens: Mutex<StreamTokens>,
    /// One PBKDF2 derivation at a time: each is ~1 s of the only CPU core.
    /// Anything that hashes a password (login, T15 password changes) takes it.
    pub pbkdf2: Semaphore,
    /// Permits for concurrent log streams ([`MAX_LOG_STREAMS`]).
    pub log_streams: Arc<Semaphore>,
    /// The single MJPEG client (VID-01).
    pub mjpeg: Arc<Semaphore>,
    /// Flips to `true` when the server starts shutting down, so long-lived
    /// responses (log streams, MJPEG) end and graceful shutdown can finish.
    shutdown: watch::Sender<bool>,
}

#[derive(Debug)]
struct CachedUsers {
    table: WebUsers,
    checked: Option<Instant>,
}

impl std::ops::Deref for AppState {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    /// Loads the users file and the session key (creating the key if needed).
    pub fn load(config: Config, tuning: Tuning) -> Result<Self, Error> {
        let users = WebUsers::load(&config.users).context(UsersSnafu)?;
        if users.is_empty() {
            tracing::warn!(
                path = %config.users.display(),
                "no web users; set web_admin_password in camera.conf and reboot"
            );
        }
        let key = ::auth::load_or_create_key(&config.jwt_key).context(KeySnafu)?;
        Ok(Self::new(config, tuning, Jwt::new(&key), users))
    }

    /// Assembles state from already-loaded parts.
    pub fn new(config: Config, tuning: Tuning, jwt: Jwt, users: WebUsers) -> Self {
        Self(Arc::new(Inner {
            config,
            tuning,
            jwt,
            users: Mutex::new(CachedUsers {
                table: users,
                checked: Some(Instant::now()),
            }),
            limiter: Mutex::new(RateLimiter::default()),
            stream_tokens: Mutex::new(StreamTokens::default()),
            pbkdf2: Semaphore::new(1),
            log_streams: Arc::new(Semaphore::new(MAX_LOG_STREAMS)),
            mjpeg: Arc::new(Semaphore::new(1)),
            shutdown: watch::channel(false).0,
        }))
    }

    /// Tells every long-lived response to finish; called on SIGTERM.
    pub fn begin_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// Resolves once [`AppState::begin_shutdown`] has been called (at once
    /// if it already was).
    pub fn shutdown_signal(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.shutdown.subscribe();
        async move {
            let _ = rx.wait_for(|stop| *stop).await;
        }
    }

    /// The users table, re-read first if the file changed (checked at most
    /// once per [`Tuning::users_refresh`]).
    ///
    /// The guard is a plain mutex: keep it for a lookup or a quick mutation,
    /// never across `.await` or a password hash (clone the table for that).
    pub fn users(&self) -> UsersGuard<'_> {
        let mut cached = lock(&self.users);
        let due = cached
            .checked
            .is_none_or(|at| at.elapsed() >= self.tuning.users_refresh);
        if due {
            if let Err(error) = cached.table.refresh() {
                tracing::warn!(%error, "keeping the previous users table");
            }
            cached.checked = Some(Instant::now());
        }
        UsersGuard(cached)
    }

    /// Role and generation of `user`, if it still exists.
    pub fn lookup(&self, user: &str) -> Option<(Role, u32)> {
        self.users().lookup(user)
    }
}

/// Locked access to the cached users table; see [`AppState::users`].
pub struct UsersGuard<'a>(MutexGuard<'a, CachedUsers>);

impl std::ops::Deref for UsersGuard<'_> {
    type Target = WebUsers;

    fn deref(&self) -> &WebUsers {
        &self.0.table
    }
}

impl std::ops::DerefMut for UsersGuard<'_> {
    fn deref_mut(&mut self) -> &mut WebUsers {
        &mut self.0.table
    }
}

/// Locks `mutex`, carrying on through poisoning: every value guarded here is
/// consistent between statements, so a panicking handler cannot corrupt it.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Wall-clock Unix seconds, for token `iat`/`exp`.
///
/// The camera has no RTC and starts at 1970 until NTP runs; a token issued
/// before the clock jumps simply expires early, which fails safe.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
