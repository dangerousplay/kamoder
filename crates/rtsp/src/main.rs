//! RTSP server for the camera's H.264 stream.
//!
//! ```text
//! rtsp --port 554 --secret etc/onvif.secret
//! rtsp --port 554 --secret etc/onvif.secret --stream vs0=camera:0 --stream vs1=camera:1
//! rtsp --port 554 --secret etc/onvif.secret --source file:clip.h264
//! ```
//!
//! Serves each stream at `rtsp://<camera>:<port>/<path>` (`/vs0` is the URL
//! `onvif` hands to NVRs), over TCP-interleaved or UDP unicast, behind RTSP
//! Digest with the ONVIF account. Sources: `camera:N`, the hardware H.264 of
//! image sensor N from the `camera` service, or `file:PATH`, an Annex-B file
//! played in a loop. `--path`/`--source` describe a single stream;
//! `--stream PATH=SOURCE`, repeated, replaces them. With neither, the board
//! profile decides (PKG-02): one `vsN=camera:N` per sensor in
//! `video.sensors`, so the same command line serves a one- or two-sensor
//! board.
//!
//! - **Audio** (AUD-10..13): when the board profile has `[audio]`, every
//!   stream also declares a PCMA track fed by the `audio` service at
//!   `--audio-socket`, captured only while a client plays it. The service
//!   being down leaves that track silent, never the video. `--no-audio`, or
//!   a board without `[audio]` (or none resolvable), serves video only.
//! - **Talk-back** (AUD-29, AUD-30): when the board has an
//!   `[[audio.output]]`, clients that DESCRIBE with
//!   `Require: www.onvif.org/ver20/backchannel` also get the ONVIF
//!   backchannel, played on the speaker through the same socket.
//!   `--no-backchannel` (or `--no-audio`) answers that tag with 551.
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
use rtsp::audio::AudioStream;
use rtsp::backchannel::Speaker;
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
    /// Video source of `--path`: `camera:N` or `file:PATH`. Without it or
    /// `--stream`, one `vsN=camera:N` per sensor of the board profile.
    #[arg(long)]
    source: Option<SourceSpec>,
    /// A stream `PATH=SOURCE`, e.g. `vs1=camera:1`; repeat for several.
    #[arg(long, value_parser = parse_stream)]
    stream: Vec<(String, SourceSpec)>,
    /// The camera service's video socket, for `camera:N` sources.
    #[arg(long, default_value = video_proto::DEFAULT_SOCKET)]
    video_socket: PathBuf,
    /// The audio service's socket, for the microphone track.
    #[arg(long, default_value = audio_proto::DEFAULT_SOCKET)]
    audio_socket: PathBuf,
    /// Serve video only, even if the board has a microphone.
    #[arg(long)]
    no_audio: bool,
    /// Refuse the ONVIF talk-back backchannel even if the board has a
    /// speaker.
    #[arg(long)]
    no_backchannel: bool,
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

/// The streams to mount, in order of precedence: the explicit `--stream`s,
/// else `--source` at `--path`, else one [`video_proto::stream_path`]`(N)` =
/// `camera:N` per sensor the board lists. `sensors` is only called in the
/// last case, so a board that cannot be resolved does not matter when the
/// command line says what to serve.
fn plan_streams(
    explicit: &[(String, SourceSpec)],
    path: &str,
    source: Option<&SourceSpec>,
    sensors: impl FnOnce() -> Result<Vec<u32>>,
) -> Result<Vec<(String, SourceSpec)>> {
    if !explicit.is_empty() {
        return Ok(explicit.to_vec());
    }
    if let Some(source) = source {
        return Ok(vec![(path.trim_matches('/').to_owned(), source.clone())]);
    }
    let sensors = sensors()?;
    eyre::ensure!(
        !sensors.is_empty(),
        "the board profile lists no video sensors; pass --stream PATH=SOURCE"
    );
    Ok(sensors
        .into_iter()
        .map(|n| (video_proto::stream_path(n), SourceSpec::Camera(n)))
        .collect())
}

/// Sensor indices of the board this runs on.
fn board_sensors() -> Result<Vec<u32>> {
    let board = board::current().wrap_err(
        "no --stream or --source given and the board profile cannot be resolved \
         (set KAMODER_BOARD/BOARD, or pass --stream PATH=SOURCE)",
    )?;
    tracing::info!(board = %board.name, reason = %board.reason, "streams from the board profile");
    Ok(board.profile.video.sensors)
}

/// Whether to declare the audio track: not with `--no-audio`, else when the
/// board profile has `[audio]` (AUD-13). `board_audio` is only called when
/// it matters; a board that cannot be resolved means no audio, not an error,
/// since `--stream` may have made the board irrelevant to video.
fn plan_audio(no_audio: bool, board_audio: impl FnOnce() -> Result<bool>) -> bool {
    if no_audio {
        tracing::info!("--no-audio: video only");
        return false;
    }
    match board_audio() {
        Ok(true) => true,
        Ok(false) => {
            tracing::info!("the board profile has no [audio]; video only");
            false
        }
        Err(error) => {
            tracing::warn!(%error, "board profile unavailable; serving video only");
            false
        }
    }
}

/// Whether to offer the ONVIF backchannel: not with `--no-audio` or
/// `--no-backchannel`, else when the board profile lists a speaker
/// (`[[audio.output]]`). Like [`plan_audio`], an unresolvable board means no
/// backchannel, not an error.
fn plan_backchannel(
    no_audio: bool,
    no_backchannel: bool,
    board_speaker: impl FnOnce() -> Result<bool>,
) -> bool {
    if no_audio || no_backchannel {
        tracing::info!("backchannel disabled on the command line");
        return false;
    }
    match board_speaker() {
        Ok(speaker) => {
            if !speaker {
                tracing::info!("the board profile has no audio output; no backchannel");
            }
            speaker
        }
        Err(error) => {
            tracing::warn!(%error, "board profile unavailable; no backchannel");
            false
        }
    }
}

/// Whether the board this runs on has a speaker.
fn board_speaker() -> Result<bool> {
    Ok(board::current()?
        .profile
        .audio
        .is_some_and(|audio| !audio.outputs.is_empty()))
}

/// Whether the board this runs on has `[audio]`.
fn board_audio() -> Result<bool> {
    Ok(board::current()?.profile.audio.is_some())
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
        let streams = plan_streams(&cli.stream, &cli.path, cli.source.as_ref(), board_sensors)?;
        // One microphone, shared by every mount.
        let audio = plan_audio(cli.no_audio, board_audio)
            .then(|| AudioStream::socket(cli.audio_socket.clone()));
        let speaker = plan_backchannel(cli.no_audio, cli.no_backchannel, board_speaker)
            .then(|| Speaker::socket(cli.audio_socket.clone()));
        let mut mounts = Vec::new();
        let mut pumps = tokio::task::JoinSet::new();
        for (path, spec) in streams {
            let (stream, pump) = match spec {
                SourceSpec::File(file) => Stream::spawn(FileSource::open(&file, cli.fps)?),
                SourceSpec::Camera(n) => Stream::spawn(CameraSource::new(cli.video_socket.clone(), n)),
            };
            pumps.spawn(pump);
            tracing::info!(
                path = %format!("/{path}"),
                audio = audio.is_some(),
                backchannel = speaker.is_some(),
                "stream mounted"
            );
            mounts.push(Mount {
                path: format!("/{path}"),
                stream,
                audio: audio.clone(),
                speaker: speaker.clone(),
            });
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
        assert_eq!(cli.audio_socket, PathBuf::from(audio_proto::DEFAULT_SOCKET));
        assert!(!cli.no_audio);
        assert!(!cli.no_backchannel);
    }

    #[test]
    fn backchannel_follows_the_speaker_unless_disabled() {
        assert!(plan_backchannel(false, false, || Ok(true)));
        assert!(!plan_backchannel(false, false, || Ok(false)));
        assert!(!plan_backchannel(false, false, || eyre::bail!("no board")));
        for (no_audio, no_backchannel) in [(true, false), (false, true)] {
            assert!(!plan_backchannel(no_audio, no_backchannel, || panic!(
                "board not consulted when disabled"
            )));
        }
        assert!(Cli::parse_from(["rtsp", "--no-backchannel"]).no_backchannel);
    }

    #[test]
    fn audio_follows_the_board_unless_disabled() {
        assert!(plan_audio(false, || Ok(true)));
        assert!(!plan_audio(false, || Ok(false)));
        assert!(!plan_audio(false, || eyre::bail!("no board")));
        assert!(!plan_audio(true, || panic!(
            "board not consulted with --no-audio"
        )));
        let cli = Cli::parse_from(["rtsp", "--no-audio", "--audio-socket", "/run/a.sock"]);
        assert!(cli.no_audio);
        assert_eq!(cli.audio_socket, PathBuf::from("/run/a.sock"));
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
        assert!(Cli::try_parse_from(["rtsp", "--source", "venc:0"]).is_err());
        for ports in ["7000", "7000-6970", "0-10", "a-b"] {
            assert!(
                Cli::try_parse_from(["rtsp", "--source", "file:x", "--udp-ports", ports]).is_err(),
                "{ports}"
            );
        }
    }

    #[test]
    fn no_stream_or_source_is_left_to_the_board() {
        let cli = Cli::parse_from(["rtsp"]);
        assert!(cli.stream.is_empty());
        assert_eq!(cli.source, None);
    }

    fn no_board() -> Result<Vec<u32>> {
        eyre::bail!("board not consulted")
    }

    #[test]
    fn default_mounts_follow_the_board_sensors() {
        let streams = plan_streams(&[], "vs0", None, || Ok(vec![0, 1])).unwrap();
        assert_eq!(
            streams,
            vec![
                ("vs0".into(), SourceSpec::Camera(0)),
                ("vs1".into(), SourceSpec::Camera(1))
            ]
        );
        let one = plan_streams(&[], "vs0", None, || Ok(vec![0])).unwrap();
        assert_eq!(one, vec![("vs0".into(), SourceSpec::Camera(0))]);
    }

    #[test]
    fn explicit_streams_and_source_win_over_the_board() {
        let explicit = vec![("main".to_owned(), SourceSpec::Camera(1))];
        assert_eq!(
            plan_streams(&explicit, "vs0", None, no_board).unwrap(),
            explicit
        );
        let file = SourceSpec::File("clip.h264".into());
        assert_eq!(
            plan_streams(&explicit, "vs0", Some(&file), no_board).unwrap(),
            explicit,
            "--stream replaces --path/--source"
        );
        assert_eq!(
            plan_streams(&[], "/live/", Some(&file), no_board).unwrap(),
            vec![("live".into(), file)]
        );
    }

    #[test]
    fn unresolvable_board_without_streams_is_an_error() {
        let err = plan_streams(&[], "vs0", None, no_board).unwrap_err();
        assert!(err.to_string().contains("board not consulted"), "{err}");
        assert!(plan_streams(&[], "vs0", None, || Ok(vec![])).is_err());
    }
}
