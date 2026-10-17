//! Web admin UI and JSON API for Anyka AK3918 cameras.
//!
//! Serves the precompressed React SPA from `www/` and a small JSON API under
//! `/api` for it: login, service status and control through the supervisor's
//! control socket, logs, `camera.conf`, Wi-Fi, users, snapshot, PTZ, lights
//! and audio (device list, push-to-talk, listen).
//!
//! ```text
//! # provisioning (init.sh does this from camera.conf's web_admin_password)
//! echo "$PASSWORD" | web hash admin > etc/web-users
//!
//! # serving (supervisor.toml runs this with working directory anyka_rust/)
//! web --listen 0.0.0.0:80 --tls-listen 0.0.0.0:443
//! ```
//!
//! Built for a 400 MHz ARMv5TE with 32 MB of RAM, which shapes everything:
//!
//! - a **current-thread** tokio runtime, as in the other services — no
//!   worker thread per core, only the blocking pool for PBKDF2;
//! - axum with **no default features** (HTTP/1, JSON, query strings and
//!   WebSockets only) and tower-http's `fs` only;
//! - **optional HTTPS** (`--tls-listen`) beside plain HTTP, with a
//!   self-signed certificate generated on first start (see [`web::tls`]).
//!   It exists because browsers only allow the microphone (push-to-talk) on
//!   a secure context; the rest of the UI works over HTTP as before, and a
//!   TLS setup failure only costs talk, never the server;
//! - the SPA is served from the SD card, gzip/brotli'd at build time, never
//!   embedded and never compressed on the camera;
//! - **stateless sessions** (HS256 JWTs, see [`web::auth`]) plus a per-user
//!   generation counter, so a request costs one HMAC and one table lookup
//!   while PBKDF2 (≈1 s) runs only at login;
//! - live logs use **SSE** (plain HTTP/1 responses axum already has), a
//!   one-way stream; **WebSockets** are used only where the browser has to
//!   send continuously or binary: push-to-talk audio up, live audio down
//!   (see [`web::audio`]).

// See lib.rs: keeps the uClibc `pthread_setname_np` shim linked.
extern crate os;

use ::auth::Role;
use axum::serve::ListenerExt;
use clap::{Parser, Subcommand};
use eyre::{bail, Result, WrapErr};
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use web::app;
use web::state::{AppState, Config, Tuning};
use web::tls::TlsListener;

/// How long open connections (log streams, MJPEG) get to finish after
/// SIGTERM before the process exits anyway. Well under the supervisor's
/// stop timeout.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Web admin UI and JSON API for the camera.
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
    /// Serve the UI and API (the default).
    Serve,
    /// Read a password from stdin and print an `etc/web-users` line.
    Hash {
        user: String,
        #[arg(long, default_value = "admin")]
        role: Role,
    },
}

/// Paths are relative to the working directory; the supervisor runs
/// services from its config directory (`anyka_rust/`).
#[derive(clap::Args)]
struct Options {
    #[arg(long, default_value = "0.0.0.0:80")]
    listen: SocketAddr,
    /// Also serve HTTPS here (e.g. 0.0.0.0:443), needed for push-to-talk.
    /// Off when not given.
    #[arg(long)]
    tls_listen: Option<SocketAddr>,
    /// TLS certificate (PEM); a self-signed one is generated if missing or
    /// unusable.
    #[arg(long, default_value = "etc/web-tls.crt")]
    tls_cert: PathBuf,
    /// TLS private key (PEM, mode 0600), generated with the certificate.
    #[arg(long, default_value = "etc/web-tls.key")]
    tls_key: PathBuf,
    /// Directory holding the built SPA (index.html plus .gz/.br assets).
    #[arg(long, default_value = "www")]
    www: PathBuf,
    /// Users file: `user:role:pbkdf2-sha256:…:gen` lines, as printed by `hash`.
    #[arg(long, default_value = "etc/web-users")]
    users: PathBuf,
    /// 32-byte session signing key; generated if missing. Delete to end all sessions.
    #[arg(long, default_value = "etc/web-jwt.key")]
    jwt_key: PathBuf,
    /// Session lifetime (at most 12 h).
    #[arg(long, default_value_t = 43_200, value_parser = clap::value_parser!(u64).range(60..=43_200))]
    token_ttl_secs: u64,
    /// Supervisor log directory.
    #[arg(long, default_value = "logs")]
    logs: PathBuf,
    #[arg(long, default_value = "camera.conf")]
    camera_conf: PathBuf,
    #[arg(long, default_value = "/tmp/anyka-supervisor.sock")]
    supervisor_socket: PathBuf,
    #[arg(long, default_value = "/tmp/anyka-ptz.sock")]
    ptz_socket: PathBuf,
    #[arg(long, default_value = lights_proto::DEFAULT_SOCKET)]
    lights_socket: PathBuf,
    #[arg(long, default_value = follow_proto::DEFAULT_SOCKET)]
    follow_socket: PathBuf,
    #[arg(long, default_value = audio_proto::DEFAULT_SOCKET)]
    audio_socket: PathBuf,
    #[arg(long, default_value = update_proto::DEFAULT_SOCKET)]
    update_socket: PathBuf,
    /// Port of the rtsp service, for the stream URLs /api/cameras reports.
    #[arg(long, default_value_t = video_proto::DEFAULT_RTSP_PORT)]
    rtsp_port: u16,
    /// JPEG snapshot source proxied by /api/snapshot.jpg.
    #[arg(long, default_value = "http://127.0.0.1:3000/snapshot.jpg")]
    snapshot_url: String,
    /// Frames per second of /api/live.mjpeg (each is one snapshot request).
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=15))]
    mjpeg_fps: u32,
    /// ONVIF account file (`user:password`), written by PUT /api/onvif/credentials.
    #[arg(long, default_value = "etc/onvif.secret")]
    onvif_secret: PathBuf,
    /// Stack script; reboot runs `sh <init-script> --stop` before `reboot`.
    #[arg(long, default_value = "init.sh")]
    init_script: PathBuf,
    /// sshd credentials file (as sshd --passwd), set from the setup wizard.
    #[arg(long, default_value = "etc/anyka-sshd.passwd")]
    ssh_passwd: PathBuf,
    /// sshd public keys (as sshd --authorized-keys).
    #[arg(long, default_value = "etc/anyka-sshd.authorized_keys")]
    ssh_authorized_keys: PathBuf,
    /// Written when the first-run setup wizard is finished.
    #[arg(long, default_value = "etc/setup-complete")]
    setup_marker: PathBuf,
    /// Wi-Fi interface reported by /api/wifi (as wifi --iface); defaults to
    /// the board profile's.
    #[arg(long)]
    wifi_iface: Option<String>,
    /// wpa_supplicant control socket directory (as wifi --ctrl-dir).
    #[arg(long, default_value = "/etc/Wireless")]
    wifi_ctrl_dir: PathBuf,
    /// Station config (as wifi --wpa-conf).
    #[arg(long, default_value = "/rom/wpa_supplicant0.conf")]
    wifi_wpa_conf: PathBuf,
    /// wifi's backup / last-good directory (as wifi --state-dir).
    #[arg(long, default_value = "backup")]
    wifi_state_dir: PathBuf,
}

/// Interface used when neither `--wifi-iface` nor the board profile names one.
const FALLBACK_WIFI_IFACE: &str = "wlan0"; // board-literal-ok: same fallback as the wifi service (no profile readable)

/// What `--tls-*` asked for.
struct TlsOptions {
    listen: SocketAddr,
    cert: PathBuf,
    key: PathBuf,
}

impl Options {
    fn tls_options(&self) -> Option<TlsOptions> {
        Some(TlsOptions {
            listen: self.tls_listen?,
            cert: self.tls_cert.clone(),
            key: self.tls_key.clone(),
        })
    }

    fn into_config(self, board: Option<board::Resolved>) -> Config {
        let wifi_iface = self
            .wifi_iface
            .or_else(|| Some(board.as_ref()?.profile.wifi.as_ref()?.iface.clone()))
            .unwrap_or_else(|| FALLBACK_WIFI_IFACE.to_owned());
        Config {
            listen: self.listen,
            www: self.www,
            users: self.users,
            jwt_key: self.jwt_key,
            token_ttl: Duration::from_secs(self.token_ttl_secs),
            logs: self.logs,
            camera_conf: self.camera_conf,
            supervisor_socket: self.supervisor_socket,
            ptz_socket: self.ptz_socket,
            lights_socket: self.lights_socket,
            follow_socket: self.follow_socket,
            audio_socket: self.audio_socket,
            update_socket: self.update_socket,
            // The working directory, like every relative path above.
            data_root: PathBuf::from("."),
            // Set by `serve_command` once the HTTPS listener is bound.
            https_port: None,
            rtsp_port: self.rtsp_port,
            board,
            snapshot_url: self.snapshot_url,
            mjpeg_interval: Duration::from_millis(1000 / u64::from(self.mjpeg_fps)),
            onvif_secret: self.onvif_secret,
            init_script: self.init_script,
            sys_root: PathBuf::from("/"),
            ssh_passwd: self.ssh_passwd,
            ssh_authorized_keys: self.ssh_authorized_keys,
            setup_marker: self.setup_marker,
            wifi: wifi::system::Paths {
                wpa_conf: self.wifi_wpa_conf,
                ctrl_dir: self.wifi_ctrl_dir,
                iface: wifi_iface,
                // Unused by the status probes; wifi's default.
                dhcp_script: PathBuf::from("/usr/share/udhcpc/default.script"),
                state_dir: self.wifi_state_dir,
            },
        }
    }
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let cli = Cli::parse();
    let _telemetry = telemetry::install("web")?;
    app::describe_metrics();

    match cli.command {
        Some(Command::Hash { user, role }) => hash_command(&user, role),
        Some(Command::Serve) | None => {
            let tls = cli.options.tls_options();
            serve_command(cli.options.into_config(current_board()), tls)
        }
    }
}

/// `web hash USER [--role R]` — prints a users-file line for the
/// password on stdin.
fn hash_command(user: &str, role: Role) -> Result<()> {
    // stdin, not argv: argv is world-readable through /proc and lands in
    // shell history.
    eprint!("password for {user}: ");
    let mut password = String::new();
    std::io::stdin().read_line(&mut password)?;
    let password = password.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        bail!("empty password");
    }
    eprintln!();
    println!("{}", ::auth::web_users::hash_line(user, role, password)?);
    Ok(())
}

/// The board profile, resolved once; a failure only costs `/api/hardware`
/// its board entry and the expected-sensor list, so it is not fatal.
fn current_board() -> Option<board::Resolved> {
    match board::current() {
        Ok(resolved) => {
            tracing::info!(board = %resolved.name, reason = %resolved.reason, "board profile");
            Some(resolved)
        }
        Err(error) => {
            tracing::warn!(%error, "no board profile; /api/hardware reports none");
            None
        }
    }
}

/// A bound HTTPS socket and its TLS configuration.
type Tls = (std::net::TcpListener, Arc<rustls::ServerConfig>);

/// Loads or generates the certificate and binds the HTTPS port. Any failure
/// is logged and leaves the server on HTTP only (AUD-18 edge case).
fn prepare_tls(options: &TlsOptions) -> Option<Tls> {
    let config = match web::tls::load_or_generate(&options.cert, &options.key) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "TLS setup failed; serving HTTP only");
            return None;
        }
    };
    let listener = std::net::TcpListener::bind(options.listen).and_then(|listener| {
        listener.set_nonblocking(true)?;
        Ok(listener)
    });
    match listener {
        Ok(listener) => Some((listener, config)),
        Err(error) => {
            tracing::error!(listen = %options.listen, %error, "cannot bind HTTPS; serving HTTP only");
            None
        }
    }
}

fn serve_command(mut config: Config, tls: Option<TlsOptions>) -> Result<()> {
    let tls = tls.as_ref().and_then(prepare_tls);
    config.https_port = tls
        .as_ref()
        .and_then(|(listener, _)| listener.local_addr().ok())
        .map(|addr| addr.port());
    tracing::info!(
        listen = %config.listen,
        www = %config.www.display(),
        users = %config.users.display(),
        logs = %config.logs.display(),
        camera_conf = %config.camera_conf.display(),
        supervisor_socket = %config.supervisor_socket.display(),
        ptz_socket = %config.ptz_socket.display(),
        lights_socket = %config.lights_socket.display(),
        follow_socket = %config.follow_socket.display(),
        audio_socket = %config.audio_socket.display(),
        update_socket = %config.update_socket.display(),
        https_port = ?config.https_port,
        snapshot_url = %config.snapshot_url,
        rtsp_port = config.rtsp_port,
        wifi_iface = %config.wifi.iface,
        "starting"
    );
    let state = AppState::load(config, Tuning::default())?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(state, tls))
}

async fn serve(state: AppState, tls: Option<Tls>) -> Result<()> {
    let listen = state.config.listen;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .wrap_err_with(|| format!("cannot bind {listen}"))?;
    tracing::info!(%listen, "web server listening");

    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let signalled = state.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        // Ends log streams and the live view, so graceful shutdown is not
        // left waiting for responses that never finish on their own.
        signalled.begin_shutdown();
        let _ = stop_tx.send(true);
    });

    let graceful = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|stop| *stop).await;
    };
    let service = app::app(state).into_make_service_with_connect_info::<SocketAddr>();
    let http = axum::serve(listener, service.clone())
        .with_graceful_shutdown(graceful(stop_rx.clone()))
        .into_future();
    let https_stop = stop_rx.clone();
    let https = async move {
        let Some((listener, config)) = tls else {
            return Ok(());
        };
        let listener = tokio::net::TcpListener::from_std(listener)?;
        tracing::info!(listen = %listener.local_addr()?, "HTTPS listening");
        // `tap_io` only to reach axum's `ConnectInfo<SocketAddr>` impl for
        // custom listeners; the login throttle keys on the peer address.
        let listener = TlsListener::new(listener, config).tap_io(|_| {});
        axum::serve(listener, service)
            .with_graceful_shutdown(graceful(https_stop))
            .await
    };

    tokio::select! {
        result = async { tokio::try_join!(http, https) } => {
            result.wrap_err("web server failed")?;
        }
        _ = async {
            let _ = stop_rx.wait_for(|stop| *stop).await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => tracing::warn!("connections still open after the grace period; exiting"),
    }
    tracing::info!("stopped");
    Ok(())
}

/// Resolves on `SIGTERM` (the supervisor) or `SIGINT`.
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        tracing::error!("cannot install signal handlers; stop with SIGKILL");
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("received SIGTERM"),
        _ = int.recv() => tracing::info!("received SIGINT"),
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

    #[test]
    fn defaults_and_hash_subcommand_parse() {
        let cli = Cli::try_parse_from(["web"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.options.listen, "0.0.0.0:80".parse().unwrap());
        assert!(cli.options.tls_options().is_none());
        assert_eq!(cli.options.tls_cert, PathBuf::from("etc/web-tls.crt"));
        assert_eq!(cli.options.tls_key, PathBuf::from("etc/web-tls.key"));
        assert_eq!(
            cli.options.audio_socket,
            PathBuf::from(audio_proto::DEFAULT_SOCKET)
        );
        assert_eq!(
            cli.options.update_socket,
            PathBuf::from(update_proto::DEFAULT_SOCKET)
        );
        let cli = Cli::try_parse_from(["web", "--tls-listen", "0.0.0.0:443"]).unwrap();
        let tls = cli.options.tls_options().unwrap();
        assert_eq!(tls.listen, "0.0.0.0:443".parse().unwrap());
        let cli = Cli::try_parse_from(["web", "hash", "bob", "--role", "viewer"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Hash { ref user, role: Role::Viewer }) if user == "bob"
        ));
        assert!(Cli::try_parse_from(["web", "--token-ttl-secs", "86400"]).is_err());
    }

    #[test]
    fn rtsp_port_and_wifi_iface_defaults() {
        let config = Cli::try_parse_from(["web"])
            .unwrap()
            .options
            .into_config(None);
        assert_eq!(config.rtsp_port, video_proto::DEFAULT_RTSP_PORT);
        assert_eq!(config.wifi.iface, FALLBACK_WIFI_IFACE);
        assert!(config.board.is_none());
        let config = Cli::try_parse_from(["web", "--rtsp-port", "8554", "--wifi-iface", "wlan1"])
            .unwrap()
            .options
            .into_config(None);
        assert_eq!(config.rtsp_port, 8554);
        assert_eq!(config.wifi.iface, "wlan1");
    }
}
