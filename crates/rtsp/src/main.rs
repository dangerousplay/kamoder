//! RTSP server for the camera's H.264 stream.
//!
//! ```text
//! rtsp --port 554 --secret etc/onvif.secret --stream vs0=camera:0 --stream vs1=camera:1
//! rtsp --port 554 --secret etc/onvif.secret --source file:clip.h264
//! ```
//!
//! Serves each stream at `rtsp://<camera>:<port>/<path>` (`/vs0` is the URL
//! `onvif` hands to NVRs), over TCP-interleaved or UDP unicast, behind RTSP
//! Digest with the ONVIF account. Sources: `camera:N`, the hardware H.264 of
//! image sensor N from the `camera` service, or `file:PATH`, an Annex-B file
//! played in a loop. `--path`/`--source` describe a single stream;
//! `--stream PATH=SOURCE`, repeated, replaces them.
//!
//! - **Credentials**: `etc/onvif.secret`, the same `user:password` file
//!   `onvif` reads, so an NVR uses one account for both. Without it the
//!   service refuses to start unless `--no-auth` is given explicitly.
//! - **32 MB of RAM**: a current-thread runtime and a bounded frame buffer
//!   shared by all clients; a slow client skips to the next IDR.
//! - **Exit status**: SIGTERM/SIGINT exit 0 — the supervisor's "asked to
//!   stop". The source ending or the listener failing is an error exit, so
//!   the supervisor restarts the service.

// Linked for its side effect: `os` supplies the `pthread_setname_np`
// shim that the crosstool's uClibc 0.9.33 does not export but Rust's thread
// startup references.
extern crate os;

use auth::onvif_secret::Secret;
use clap::Parser;
use eyre::{Context, Result};
use rtsp::digest::DigestAuth;
use rtsp::server::{Config, Mount};
use rtsp::source::{CameraSource, FileSource, SourceSpec};
use rtsp::stream::Stream;
use std::path::PathBuf;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};

/// RTSP server streaming H.264 from a video source.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Address the server binds.
    #[arg(long, default_value = "0.0.0.0")]
    listen: String,
    /// RTSP port.
    #[arg(long, default_value_t = 554)]
    port: u16,
    /// Stream path (the URL is rtsp://<host>:<port>/<path>).
    #[arg(long, default_value = "vs0")]
    path: String,
    /// Video source of `--path`: `camera:N` or `file:PATH`.
    #[arg(long, required_unless_present = "stream")]
    source: Option<SourceSpec>,
    /// A stream `PATH=SOURCE`, e.g. `vs1=camera:1`; repeat for several.
    #[arg(long, value_parser = parse_stream)]
    stream: Vec<(String, SourceSpec)>,
    /// The camera service's video socket, for `camera:N` sources.
    #[arg(long, default_value = rtsp::source::DEFAULT_VIDEO_SOCKET)]
    video_socket: PathBuf,
    /// Frame rate of a file source.
    #[arg(long, default_value_t = 25)]
    fps: u32,
    /// Credentials file: one `user:password` line (the ONVIF account).
    #[arg(long, default_value = "etc/onvif.secret")]
    secret: PathBuf,
    /// Run without authentication. Anyone on the network can then watch.
    #[arg(long)]
    no_auth: bool,
    /// Digest realm shown to clients.
    #[arg(long, default_value = "Camera")]
    realm: String,
    /// Seconds without a request or RTCP before a session is dropped.
    #[arg(long, default_value_t = 60)]
    session_timeout: u64,
    /// Local UDP ports for RTP/RTCP pairs, `FIRST-LAST`.
    #[arg(long, default_value = "6970-6999", value_parser = parse_ports)]
    udp_ports: std::ops::Range<u16>,
}

fn parse_stream(text: &str) -> Result<(String, SourceSpec), String> {
    let (path, source) = text.split_once('=').ok_or("expected PATH=SOURCE")?;
    let path = path.trim_matches('/');
    if path.is_empty() || path.contains(char::is_whitespace) {
        return Err("bad stream path".into());
    }
    Ok((
        path.to_owned(),
        source
            .parse()
            .map_err(|e: rtsp::source::Error| e.to_string())?,
    ))
}

fn parse_ports(text: &str) -> Result<std::ops::Range<u16>, String> {
    let (first, last) = text.split_once('-').ok_or("expected FIRST-LAST")?;
    let first: u16 = first.trim().parse().map_err(|_| "bad first port")?;
    let last: u16 = last.trim().parse().map_err(|_| "bad last port")?;
    if first == 0 || last <= first {
        return Err("need 0 < FIRST < LAST".into());
    }
    Ok(first..last.saturating_add(1))
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let cli = Cli::parse();
    let _telemetry = telemetry::install("rtsp")?;
    rtsp::describe_metrics();
    serve(cli)
}

fn serve(cli: Cli) -> Result<()> {
    let auth = if cli.no_auth {
        tracing::warn!("--no-auth: the video stream is open to anyone on the network");
        None
    } else {
        let secret = Secret::load(&cli.secret)?;
        Some(DigestAuth::new(&cli.realm, &secret.user, &secret.password))
    };
    // One thread: a handful of clients and one source need no more.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let streams = if cli.stream.is_empty() {
            let source = cli.source.clone().expect("clap requires --source without --stream");
            vec![(cli.path.trim_matches('/').to_owned(), source)]
        } else {
            cli.stream.clone()
        };
        let mut mounts = Vec::new();
        let mut pumps = tokio::task::JoinSet::new();
        for (path, spec) in streams {
            let (stream, pump) = match spec {
                SourceSpec::File(file) => Stream::spawn(FileSource::open(&file, cli.fps)?),
                SourceSpec::Camera(n) => Stream::spawn(CameraSource::new(cli.video_socket.clone(), n)),
            };
            pumps.spawn(pump);
            tracing::info!(path = %format!("/{path}"), "stream mounted");
            mounts.push(Mount { path: format!("/{path}"), stream });
        }
        let listener = tokio::net::TcpListener::bind((cli.listen.as_str(), cli.port))
            .await
            .wrap_err_with(|| format!("cannot bind {}:{}", cli.listen, cli.port))?;
        let config = Config {
            session_timeout: Duration::from_secs(cli.session_timeout.max(1)),
            udp_ports: cli.udp_ports.clone(),
            auth,
        };
        tracing::info!(listen = %cli.listen, port = cli.port, streams = mounts.len(), "RTSP listening");
        let server = rtsp::server::serve(listener, mounts, config);
        tokio::pin!(server);
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
            served = &mut server => served?,
            Some(ended) = pumps.join_next() => {
                ended.wrap_err("a video source task failed")?.wrap_err("a video source task panicked")??;
                eyre::bail!("a video source ended");
            }
        }
        tracing::info!("shutting down");
        Ok(())
    })
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
    fn defaults_match_onvif() {
        let cli = Cli::parse_from(["rtsp", "--source", "file:clip.h264"]);
        assert_eq!(cli.port, 554);
        assert_eq!(cli.path, "vs0");
        assert_eq!(cli.secret, PathBuf::from("etc/onvif.secret"));
        assert_eq!(cli.fps, 25);
        assert_eq!(cli.session_timeout, 60);
        assert_eq!(cli.udp_ports, 6970..7000);
        assert!(!cli.no_auth);
        assert_eq!(cli.source, Some(SourceSpec::File("clip.h264".into())));
    }

    #[test]
    fn several_streams() {
        let cli = Cli::parse_from([
            "rtsp",
            "--stream",
            "vs0=camera:0",
            "--stream",
            "/vs1=camera:1",
        ]);
        assert_eq!(
            cli.stream,
            vec![
                ("vs0".into(), SourceSpec::Camera(0)),
                ("vs1".into(), SourceSpec::Camera(1))
            ]
        );
        assert!(Cli::try_parse_from(["rtsp", "--stream", "vs0"]).is_err());
    }

    #[test]
    fn rejects_bad_sources_and_port_ranges() {
        assert!(Cli::try_parse_from(["rtsp"]).is_err());
        assert!(Cli::try_parse_from(["rtsp", "--source", "venc:0"]).is_err());
        for ports in ["7000", "7000-6970", "0-10", "a-b"] {
            assert!(
                Cli::try_parse_from(["rtsp", "--source", "file:x", "--udp-ports", ports]).is_err(),
                "{ports}"
            );
        }
    }
}
