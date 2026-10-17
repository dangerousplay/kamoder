//! ONVIF for Anyka AK3918 cameras: WS-Discovery plus the Device and Media
//! services of Profile S, so NVRs (Home Assistant, Frigate, Agent DVR) can
//! find the camera and learn its RTSP and snapshot URLs, and the PTZ service
//! when the camera has a pan/tilt head. This process owns the motor; web
//! drives it through `/tmp/anyka-ptz.sock` (see `ptz.rs`). The Imaging
//! service's IR-cut filter is the `lights` service's day/night mode, reached
//! through `/tmp/kamoder-lights.sock` (see `imaging.rs`).
//!
//! ```text
//! onvif --port 8000 --secret etc/onvif.secret
//! ```
//!
//! The transport — HTTP, the SOAP envelope and faults, WS-UsernameToken
//! (PasswordDigest and PasswordText, replay guard, lockout), the
//! WS-Discovery responder with Hello/Bye and the basic Device answers — is
//! `onvif-device-rs`. The Media, PTZ, GetCapabilities and GetServices
//! messages are the types `onvif-schema` generates from the ONVIF WSDL/XSD
//! (see `soap.rs`), so they are namespace-qualified as WSDL-driven clients
//! (python `onvif-zeep`, used by Home Assistant) expect. This binary
//! supplies what only the camera knows: its identity, its address, where
//! `rtsp` and `web` serve video.
//!
//! The board profile (`board::current()`, normally the supervisor's
//! `KAMODER_BOARD` handoff) names the sensors and their configured streams;
//! the producer's `/status` (`--producer`) says what is actually encoded.
//! Without a board profile the service refuses to start: it would not know
//! which streams exist. Device identity: the model is the profile's
//! description (else its name), the hardware id the kernel's cpuinfo
//! `Hardware` line (else the profile name), and the manufacturer is
//! "Kamoder" — the firmware answering, since nothing on the board reliably
//! names the camera's vendor.
//!
//! Why things are the way they are:
//!
//! - **A separate process on its own port (8000)**, not a route in web:
//!   NVRs keep polling ONVIF while an admin restarts the web UI, and each
//!   service gets its own log and restart budget from the supervisor.
//! - **A plaintext secret** (`etc/onvif.secret`, `user:password`): the
//!   PasswordDigest every NVR sends is SHA1 over the password itself, so the
//!   server must hold it. It is one dedicated account, not a web user, and
//!   the file is refused if other users can read it (except on FAT, which has
//!   no modes). Without the file the service refuses to start unless
//!   `--no-auth` is given explicitly.
//! - **32 MB of RAM**: a current-thread runtime, request bodies capped at
//!   64 KiB, no embedded assets. Nothing here needs 64-bit atomics, which
//!   ARMv5TE lacks.
//! - **Exit status**: SIGTERM/SIGINT send WS-Discovery Bye and exit 0 — the
//!   supervisor's "asked to stop". Anything else ending the service is an
//!   error exit, so the supervisor restarts it.
//!
//! SPEC_DEVIATION (ONVIF-05): an unsupported action gets a SOAP 1.2 Sender
//! fault (HTTP 400, reason "unsupported action: X") rather than the
//! `ter:ActionNotSupported` subcode. The library answers unknown actions
//! before any handler runs and offers no hook to change it; clients treat
//! both as a fault.

// Linked for its side effect: `os` supplies the `pthread_setname_np`
// shim that the crosstool's uClibc 0.9.33 does not export but Rust's thread
// startup references.
extern crate os;

mod announce;
mod audio;
mod device;
mod imaging;
mod media;
mod net;
mod producer;
mod ptz;
mod ptz_soap;
mod ptz_socket;
mod service;
mod soap;

// Shared with rtsp, which reads the same account for RTSP Digest.
use auth::onvif_secret as secret;

use announce::{Advert, Announcer};
use clap::Parser;
use eyre::{Context, Result};
use media::VideoStream;
use net::AddressSource;
use onvif_device_rs::config::DeviceConfig;
use ptz::PtzMode;
use secret::Secret;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};

/// How often the advertised address is re-checked (Wi-Fi may move networks).
const ADDRESS_POLL: Duration = Duration::from_secs(30);

/// ONVIF device service and WS-Discovery responder for the camera.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Address the SOAP server binds.
    #[arg(long, default_value = "0.0.0.0")]
    listen: String,
    /// SOAP (ONVIF device service) port.
    #[arg(long, default_value_t = 8000)]
    port: u16,
    /// Credentials file: one `user:password` line, `#` comments allowed.
    #[arg(long, default_value = "etc/onvif.secret")]
    secret: PathBuf,
    /// Run without authentication. Anyone on the network can then query the
    /// camera and move its PTZ head.
    #[arg(long)]
    no_auth: bool,
    /// Port of the RTSP server (rtsp). Sensor N is served at `/vsN`.
    #[arg(long, default_value_t = video_proto::DEFAULT_RTSP_PORT)]
    rtsp_port: u16,
    /// `host:port` of the video producer (the camera service), whose
    /// `/status` gives each sensor's stream parameters. Empty: use the board
    /// profile's values only.
    #[arg(long, default_value = video_proto::DEFAULT_PRODUCER)]
    producer: String,
    /// HTTP port of web, which serves the snapshot.
    #[arg(long, default_value_t = 80)]
    web_port: u16,
    /// Snapshot path on web; `?camera=N` is appended per sensor.
    #[arg(long, default_value = "/api/snapshot.jpg")]
    snapshot_path: String,
    /// Friendly name, shown by NVRs and in the discovery scopes.
    #[arg(long, default_value = "Anyka camera")]
    name: String,
    /// PTZ backend; overrides the board profile's `[ptz]` backend. Not
    /// given: the profile's backend, or no PTZ when the profile has no
    /// `[ptz]`. A backend that cannot be opened is logged and the service
    /// runs without PTZ. `ANYKA_ONVIF_PTZ` sets it when the option is not
    /// given (itest's VM has no motor and exports `sim`).
    #[arg(long, value_enum, env = "ANYKA_ONVIF_PTZ")]
    ptz: Option<PtzMode>,
    /// PTZ preset file (JSON); empty keeps presets in memory only.
    #[arg(long, default_value = "etc/ptz-presets.json")]
    ptz_presets: String,
    /// Unix socket through which web drives the motor this process
    /// owns; empty disables it.
    #[arg(long, default_value = ptz_proto::DEFAULT_SOCKET)]
    ptz_socket: String,
    /// Socket of the lights owner (the `lights` service), through which the
    /// Imaging service switches the IR-cut filter; empty disables Imaging.
    /// Imaging is advertised only while the owner answers.
    #[arg(long, default_value = lights_proto::DEFAULT_SOCKET)]
    lights_socket: String,
    /// Socket of the audio service, through which the Media service reads
    /// and sets the speaker volume (`AudioOutputConfiguration.OutputLevel`);
    /// empty: report the board profile's default volume, refuse to set it.
    /// The audio configurations themselves come from the board's `[audio]`.
    #[arg(long, default_value = audio_proto::DEFAULT_SOCKET)]
    audio_socket: String,
    /// Advertise the address of this interface (default: the first
    /// non-loopback IPv4 address of any interface).
    #[arg(long, conflicts_with = "address")]
    interface: Option<String>,
    /// Advertise this fixed address instead of detecting one.
    #[arg(long)]
    address: Option<Ipv4Addr>,
    /// Extra WS-Discovery scope URIs, comma-separated or repeated.
    #[arg(long, value_delimiter = ',')]
    scopes: Vec<String>,
}

fn main() -> Result<()> {
    // Before telemetry, so `--help` does not open log files.
    let cli = Cli::parse();
    let _telemetry = telemetry::install("onvif")?;
    // onvif-device-rs logs through `log`; route it into the same subscriber.
    if let Err(error) = tracing_log::LogTracer::init() {
        tracing::debug!(%error, "log bridge not installed");
    }
    service::describe_metrics();
    serve(cli)
}

fn serve(cli: Cli) -> Result<()> {
    let secret = if cli.no_auth {
        tracing::warn!("--no-auth: ONVIF is open to anyone on the network");
        None
    } else {
        Some(Secret::load(&cli.secret)?)
    };

    let board = board::current().wrap_err(
        "cannot determine the board profile (set KAMODER_BOARD, or BOARD and \
         KAMODER_BOARDS_DIR, or install boards/ next to bin/)",
    )?;
    let video = board_streams(&board.profile.video);
    if video.is_empty() {
        eyre::bail!("board profile {} lists no video sensor", board.name);
    }
    tracing::info!(board = %board.name, sensors = video.len(), "board profile");

    let wifi_iface = board.profile.wifi.as_ref().map(|w| w.iface.as_str());
    let mac = net::mac_address(&mac_candidates(cli.interface.as_deref(), wifi_iface));
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").ok();
    let identity = device_identity(
        &cli.name,
        mac.as_deref(),
        &board.profile,
        cpuinfo.as_deref(),
    );
    let source = match (&cli.address, &cli.interface) {
        (Some(address), _) => AddressSource::Fixed(*address),
        (None, Some(name)) => AddressSource::Interface(name.clone()),
        (None, None) => AddressSource::Auto,
    };
    let initial_ip = source.detect();
    tracing::info!(
        model = %identity.model,
        serial = %identity.serial_number,
        address = ?initial_ip,
        "device identity"
    );

    // Before the runtime: the SDK backend blocks for its homing sweep.
    let (ptz_mode, ptz_options) = ptz::plan(
        cli.ptz,
        board.profile.ptz.as_ref(),
        std::path::Path::new(&cli.ptz_presets),
    );
    let backend = ptz::open(ptz_mode, &ptz_options);
    // One thread: a handful of NVR requests a minute do not need more.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    // `Ptz::new` spawns its deadman task, which needs the runtime context.
    let motor = {
        let _context = runtime.enter();
        backend.map(ptz::Ptz::new)
    };

    let settings = service::Settings {
        port: cli.port,
        // Only the fallback for loopback/unknown connections; real requests
        // get URLs built from the address they arrived on.
        device_ip: initial_ip.unwrap_or(Ipv4Addr::LOCALHOST).to_string(),
        identity: identity.clone(),
        rtsp_port: cli.rtsp_port,
        producer: (!cli.producer.is_empty()).then(|| cli.producer.clone()),
        video,
        web_port: cli.web_port,
        snapshot_path: leading_slash(&cli.snapshot_path),
        secret,
        ptz: motor.clone(),
        lights: (!cli.lights_socket.is_empty())
            .then(|| lights_proto::Client::new(&cli.lights_socket)),
        audio: audio::Audio::new(
            board.profile.audio.as_ref(),
            (!cli.audio_socket.is_empty()).then(|| audio_proto::Client::new(&cli.audio_socket)),
        ),
    };
    let advert = Advert {
        onvif_port: cli.port,
        name: identity.name.clone(),
        hardware: identity.hardware_id.clone(),
        uuid: mac.as_deref().map(announce::uuid_from_mac),
        extra_scopes: cli.scopes.clone(),
    };
    if advert.uuid.is_none() {
        tracing::warn!("no MAC address found; the discovery UUID changes on every start");
    }

    runtime.block_on(async move {
        let server = service::build(&settings).wrap_err("invalid ONVIF configuration")?;
        // Held for the lifetime of the service; dropping it removes the file.
        let _ptz_socket = match (&motor, cli.ptz_socket.is_empty()) {
            (Some(motor), false) => {
                match ptz_socket::serve(std::path::Path::new(&cli.ptz_socket), Arc::clone(motor)) {
                    Ok(socket) => Some(socket),
                    // PTZ over ONVIF still works; only the web UI loses the pad.
                    Err(error) => {
                        tracing::warn!(%error, "PTZ socket unavailable");
                        None
                    }
                }
            }
            _ => None,
        };
        let listener = tokio::net::TcpListener::bind((cli.listen.as_str(), cli.port))
            .await
            .wrap_err_with(|| format!("cannot bind {}:{}", cli.listen, cli.port))?;
        let mut soap = server
            .start_on(listener)
            .await
            .wrap_err("cannot start the ONVIF server")?;
        tracing::info!(listen = %cli.listen, port = cli.port, "ONVIF listening");

        let mut announcer = Announcer::new(source, advert);
        announcer.refresh().await;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut poll = tokio::time::interval(ADDRESS_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let outcome = loop {
            tokio::select! {
                _ = poll.tick() => announcer.refresh().await,
                _ = terminate.recv() => break Ok(()),
                _ = interrupt.recv() => break Ok(()),
                _ = &mut soap => break Err(eyre::eyre!("the ONVIF server stopped unexpectedly")),
            }
        };
        tracing::info!("shutting down");
        announcer.stop().await;
        if outcome.is_ok() {
            soap.shutdown().await?;
        }
        outcome
    })
}

/// Interfaces whose MAC can serve as the serial number, in order: the
/// `--interface` flag, the board's Wi-Fi interface, then `eth0`.
fn mac_candidates<'a>(interface: Option<&'a str>, wifi: Option<&'a str>) -> Vec<&'a str> {
    let mut names: Vec<&str> = interface.into_iter().chain(wifi).collect();
    names.push("eth0");
    names.dedup();
    names
}

/// The board's stream settings, one per sensor (the Media profiles while
/// the producer has none).
fn board_streams(video: &board::Video) -> Vec<VideoStream> {
    video
        .sensors
        .iter()
        .map(|&index| VideoStream::from_board(index, &video.stream_for(index)))
        .collect()
}

/// What GetDeviceInformation reports; see the module doc.
fn device_identity(
    name: &str,
    mac: Option<&str>,
    board: &board::Profile,
    cpuinfo: Option<&str>,
) -> DeviceConfig {
    let model = board
        .description
        .clone()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| board.name.clone());
    let hardware_id = cpuinfo
        .and_then(net::cpuinfo_hardware)
        .unwrap_or_else(|| board.name.clone());
    DeviceConfig {
        name: name.to_owned(),
        manufacturer: "Kamoder".to_owned(),
        hardware_id,
        model,
        firmware: env!("CARGO_PKG_VERSION").to_owned(),
        serial_number: mac.unwrap_or_default().to_owned(),
    }
}

fn leading_slash(path: &str) -> String {
    format!("/{}", path.trim_start_matches('/'))
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
    fn mac_candidates_follow_flag_then_board_wifi() {
        assert_eq!(mac_candidates(None, None), vec!["eth0"]);
        assert_eq!(mac_candidates(None, Some("wlan0")), vec!["wlan0", "eth0"]);
        assert_eq!(
            mac_candidates(Some("ra0"), Some("wlan0")),
            vec!["ra0", "wlan0", "eth0"]
        );
        assert_eq!(
            mac_candidates(Some("wlan0"), Some("wlan0")),
            vec!["wlan0", "eth0"]
        );
    }

    #[test]
    fn defaults_match_the_design() {
        let cli = Cli::parse_from(["onvif"]);
        assert_eq!(cli.port, 8000);
        assert_eq!(cli.secret, PathBuf::from("etc/onvif.secret"));
        assert_eq!(cli.rtsp_port, 554);
        assert_eq!(cli.producer, "127.0.0.1:3000");
        assert_eq!(cli.web_port, 80);
        assert_eq!(cli.snapshot_path, "/api/snapshot.jpg");
        assert_eq!(cli.ptz, None, "the board profile decides");
        assert_eq!(cli.ptz_presets, "etc/ptz-presets.json");
        assert_eq!(cli.ptz_socket, "/tmp/anyka-ptz.sock");
        assert_eq!(cli.lights_socket, "/tmp/kamoder-lights.sock");
        assert_eq!(cli.audio_socket, "/tmp/kamoder-audio.sock");
        assert!(!cli.no_auth);
        assert!(cli.address.is_none() && cli.interface.is_none());
    }

    #[test]
    fn deployed_invocation_parses() {
        let cli = Cli::parse_from([
            "onvif",
            "--port",
            "8000",
            "--secret",
            "etc/onvif.secret",
            "--ptz",
            "preset-motor",
            "--interface",
            "wlan0",
            "--scopes",
            "onvif://www.onvif.org/location/garage,onvif://www.onvif.org/x/y",
        ]);
        assert_eq!(cli.ptz, Some(PtzMode::PresetMotor));
        assert_eq!(cli.interface.as_deref(), Some("wlan0"));
        assert_eq!(cli.scopes.len(), 2);

        let cli = Cli::parse_from([
            "onvif",
            "--ptz",
            "sim",
            "--no-auth",
            "--address",
            "10.0.2.15",
            "--ptz-socket",
            "",
            "--ptz-presets",
            "",
            "--lights-socket",
            "",
            "--audio-socket",
            "",
        ]);
        assert!(cli.ptz_socket.is_empty());
        assert!(cli.lights_socket.is_empty());
        assert!(cli.audio_socket.is_empty());
        assert!(cli.ptz_presets.is_empty());
        assert_eq!(cli.ptz, Some(PtzMode::Sim));
        assert!(cli.no_auth);
        assert_eq!(cli.address, Some(Ipv4Addr::new(10, 0, 2, 15)));
    }

    #[test]
    fn ptz_mode_from_the_environment() {
        use clap::CommandFactory;
        let ptz = Cli::command()
            .get_arguments()
            .find(|arg| arg.get_id() == "ptz")
            .and_then(|arg| arg.get_env().map(|env| env.to_owned()))
            .unwrap();
        assert_eq!(ptz, "ANYKA_ONVIF_PTZ");
    }

    #[test]
    fn interface_and_address_conflict() {
        assert!(
            Cli::try_parse_from(["onvif", "--interface", "wlan0", "--address", "10.0.0.1"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["onvif", "--ptz", "bogus"]).is_err());
    }

    #[test]
    fn paths_get_one_leading_slash() {
        assert_eq!(leading_slash("api"), "/api");
        assert_eq!(leading_slash("/api/snapshot.jpg"), "/api/snapshot.jpg");
        assert_eq!(leading_slash("//x"), "/x");
    }

    fn av100() -> board::Profile {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../boards/anyka-av100-ptz.toml");
        board::load(&path).unwrap()
    }

    #[test]
    fn identity_is_valid_for_the_library() {
        let board = av100();
        let cpuinfo = "Processor\t: ARM926EJ-S rev 5 (v5l)\nHardware\t: AK3918AV100\n";
        let identity = device_identity("Cam", Some("001a2b3c4d5e"), &board, Some(cpuinfo));
        assert_eq!(identity.manufacturer, "Kamoder");
        assert_eq!(identity.model, board.description.clone().unwrap());
        assert_eq!(identity.hardware_id, "AK3918AV100");
        assert_eq!(identity.firmware, env!("CARGO_PKG_VERSION"));
        assert_eq!(identity.serial_number, "001a2b3c4d5e");
        identity.validate().unwrap();

        // No cpuinfo, no description: the profile name stands in.
        let board = board::Profile {
            description: None,
            ..board
        };
        let identity = device_identity("Cam", None, &board, None);
        assert_eq!(identity.model, "anyka-av100-ptz");
        assert_eq!(identity.hardware_id, "anyka-av100-ptz");
        identity.validate().unwrap();
    }

    #[test]
    fn one_stream_per_board_sensor() {
        let streams = board_streams(&av100().video);
        assert_eq!(streams.iter().map(|s| s.index).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(
            (
                streams[1].width,
                streams[1].height,
                streams[1].fps,
                streams[1].kbps
            ),
            (1280, 720, 15, 2048)
        );
    }
}
