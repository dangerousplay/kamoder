//! The RTSP 1.0 method handlers and the session table.
//!
//! `msf-rtsp` frames requests and responses, demultiplexes interleaved data,
//! opens UDP channels and runs RTCP; everything RTSP *means* is here:
//!
//! - **OPTIONS** — `Public`. Never asks for credentials.
//! - **DESCRIBE** — SDP from `msf_sdp`: the H.264 track (`trackID=0`),
//!   payload type 96, `packetization-mode=1` plus
//!   `profile-level-id`/`sprop-parameter-sets` from the stream's own
//!   SPS/PPS; when the board has a microphone, a PCMA track (`trackID=1`,
//!   static payload type 8, AUD-10); and a `Content-Base` for the track URLs.
//!   With `Require: www.onvif.org/ver20/backchannel` on a mount with a
//!   speaker, also the ONVIF backchannel (`trackID=2`, PCMA, `a=sendonly`;
//!   AUD-29, see [`crate::backchannel`]) — for that client only.
//! - **Require** — any other feature tag, or the backchannel tag where there
//!   is no speaker, is answered `551 Option not supported` with
//!   `Unsupported` (RFC 2326 § 12.32), on every method.
//! - **SETUP** — one [`Track`] per request, `RTP/AVP/TCP;interleaved` on the
//!   RTSP connection, or `RTP/AVP;unicast;client_port` with a server port
//!   pair allocated from `--udp-ports`. The first SETUP creates the session,
//!   a SETUP naming it adds the other track, in either order and each with
//!   its own transport (AUD-11). SETUP on the stream URL itself means video,
//!   as it did when video was the only track. RTP always goes to the peer of
//!   the RTSP connection; a `destination` parameter is ignored, so the camera
//!   cannot be turned into a UDP reflector.
//! - **PLAY** — starts one sender (`session.rs`) per track set up and
//!   answers with an `RTP-Info` entry (seq/rtptime) for each; a backchannel
//!   track gets a receiver instead (`backchannel.rs`, AUD-30) and no
//!   `RTP-Info`, since the server sends nothing on it.
//! - **TEARDOWN** (the whole session), **GET_PARAMETER** (keepalive).
//!
//! A session lives until TEARDOWN, until nothing has been heard from it for
//! the `Session` timeout (any request naming it, or RTCP from the client),
//! or — for interleaved transport — until its RTSP connection closes. A UDP
//! session outlives its connection, as RFC 2326 allows; the timeout reaps it
//! if the client is gone.

use crate::audio::AudioStream;
use crate::backchannel::{self, Speaker};
use crate::digest::{DigestAuth, Verdict};
use crate::session::{self, Sender};
use crate::stream::Stream;
use bytes::Bytes;
use futures::StreamExt;
use msf_rtsp::header::rtpinfo::RTPInfo;
use msf_rtsp::header::session::SessionHeader;
use msf_rtsp::header::transport::TransportHeaderV10;
use msf_rtsp::sdp::attribute::{FormatParameters, RTPMap};
use msf_rtsp::sdp::connection::{ConnectionAddress, ConnectionInfo};
use msf_rtsp::sdp::{AddressType, MediaDescription, NetworkType, Origin, SessionDescription};
use msf_rtsp::server::response as reply;
use msf_rtsp::server::{
    ConnectionHandler, DefaultConnectionHandler, DefaultConnectionInfo, IncomingRequest,
    InterleavedChannel, OutgoingResponse, RequestHandler, RtpTransport, RtspServer,
};
use msf_rtsp::udp::UdpChannel;
use msf_rtsp::{Method, Status};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};

/// The methods this server implements, for `Public` and `Allow`.
pub const PUBLIC: &str = "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER";

/// RTP payload type of the video track (dynamic range).
pub const PAYLOAD_TYPE: u8 = 96;

/// Number of [`Track`]s, the size of a session's per-track table.
const TRACKS: usize = 3;

/// A medium a client can SETUP, by its control URL `trackID=N` relative to
/// `Content-Base`. The discriminant is the track id and the slot in a
/// session's transport table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    /// H.264, `trackID=0`.
    Video = 0,
    /// PCMA from the microphone, `trackID=1`; only on mounts with audio.
    Audio = 1,
    /// The ONVIF backchannel, PCMA from the client to the speaker,
    /// `trackID=2`; only on mounts with a speaker.
    Backchannel = 2,
}

impl Track {
    pub const ALL: [Track; TRACKS] = [Track::Video, Track::Audio, Track::Backchannel];

    /// The control URL, relative to `Content-Base`.
    pub fn control(self) -> &'static str {
        match self {
            Track::Video => "trackID=0",
            Track::Audio => "trackID=1",
            Track::Backchannel => "trackID=2",
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    /// The track a URL path names after the mount path: `""` (the stream
    /// URL — video, as before there was audio) or a `trackID`.
    fn from_rest(rest: &str) -> Option<Option<Track>> {
        match rest.trim_start_matches('/') {
            "" => Some(None),
            rest => Track::ALL
                .into_iter()
                .find(|t| t.control() == rest)
                .map(Some),
        }
    }
}

/// How long DESCRIBE waits for the source's first SPS/PPS.
const PARAMETERS_WAIT: Duration = Duration::from_secs(5);

/// One stream and the path it is served at.
pub struct Mount {
    /// Stream path with its leading slash, e.g. `/vs0`.
    pub path: String,
    pub stream: Stream,
    /// The microphone, when the board has one (AUD-13: `None` declares no
    /// audio track). Mounts share one [`AudioStream`].
    pub audio: Option<AudioStream>,
    /// The speaker, for the ONVIF backchannel (AUD-29): `None` answers the
    /// backchannel `Require` tag with 551 and has no `trackID=2`.
    pub speaker: Option<Speaker>,
}

/// What the server is told at start-up.
pub struct Config {
    /// Inactivity before a session is reaped; advertised in `Session`.
    pub session_timeout: Duration,
    /// Local ports for UDP transport, used as RTP/RTCP pairs from an even
    /// start.
    pub udp_ports: Range<u16>,
    /// `None` serves anyone (`--no-auth`).
    pub auth: Option<DigestAuth>,
}

/// Serves RTSP on `listener` until the listener fails.
pub async fn serve(listener: TcpListener, mounts: Vec<Mount>, config: Config) -> eyre::Result<()> {
    // The connection read timeout must not fire before the session timeout
    // would: a UDP client may legitimately be silent on the RTSP connection
    // between keepalives.
    let read_timeout = config.session_timeout + Duration::from_secs(15);
    let shared = Arc::new(Shared {
        config,
        mounts,
        sessions: Mutex::new(HashMap::new()),
    });
    let reaper = tokio::spawn(reap(Arc::downgrade(&shared)));
    let handler = Handler {
        shared: Arc::clone(&shared),
    };
    let mut builder = RtspServer::builder().read_timeout(Some(read_timeout));
    // msf-rtsp answers 551 to any `Require` tag not registered here, before
    // our handler runs; `Shared::handle` then refuses the backchannel tag
    // per mount (a mount without a speaker).
    if shared.mounts.iter().any(|mount| mount.speaker.is_some()) {
        builder = builder.server_feature(backchannel::FEATURE);
    }
    let served = builder
        .build(listener)
        .serve_with_connection_handler(handler, |inner| Tracked { inner, shared })
        .await;
    reaper.abort();
    served.map_err(|error| eyre::eyre!("RTSP server stopped: {error}"))
}

/// Server-wide state behind every request.
struct Shared {
    config: Config,
    mounts: Vec<Mount>,
    sessions: Mutex<HashMap<String, Session>>,
}

/// One RTSP session: one client, one mount, up to [`TRACKS`] tracks.
struct Session {
    /// The RTSP connection that created it.
    peer: SocketAddr,
    /// Index into `Shared::mounts`.
    mount: usize,
    last_seen: Instant,
    /// What each [`Track`] has, indexed by [`Track::index`]; `None` if the
    /// client did not set it up.
    tracks: [Option<SetUp>; TRACKS],
}

/// One track of a session.
struct SetUp {
    /// The track URL from SETUP, echoed in `RTP-Info`.
    control: String,
    interleaved: bool,
    state: State,
}

enum State {
    /// Set up, channels open, not playing yet.
    Ready(Transport),
    /// The sender task owns the channels; dropping this stops it.
    Playing(#[allow(dead_code, reason = "held for its Drop")] Sender),
}

/// The RTP/RTCP channel pair chosen at SETUP.
enum Transport {
    Interleaved {
        rtp: InterleavedChannel,
        rtcp: InterleavedChannel,
    },
    // Boxed: a UDP channel is ~180 bytes, an interleaved one ~24.
    Udp {
        rtp: Box<UdpChannel>,
        rtcp: Box<UdpChannel>,
    },
}

impl Session {
    /// Whether any track rides on the RTSP connection, which then ends the
    /// session when it closes.
    fn interleaved(&self) -> bool {
        self.tracks.iter().flatten().any(|track| track.interleaved)
    }

    fn playing(&self) -> bool {
        self.tracks
            .iter()
            .flatten()
            .any(|track| matches!(track.state, State::Playing(_)))
    }
}

impl Shared {
    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Session>> {
        // A panic while holding the lock leaves a consistent map: every
        // mutation is a single insert, remove or track-slot assignment.
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records activity on `id`; false if there is no such session.
    fn touch(&self, id: &str) -> bool {
        match self.sessions().get_mut(id) {
            Some(session) => {
                session.last_seen = Instant::now();
                true
            }
            None => false,
        }
    }

    /// Removes the sessions `drop_it` picks and returns how many. They are
    /// dropped, stopping their senders, after the lock is released.
    fn remove_where(&self, mut drop_it: impl FnMut(&str, &Session) -> bool) -> usize {
        let removed: Vec<Session> = {
            let mut sessions = self.sessions();
            let ids: Vec<String> = sessions
                .iter()
                .filter(|(id, session)| drop_it(id, session))
                .map(|(id, _)| id.clone())
                .collect();
            let removed = ids.iter().filter_map(|id| sessions.remove(id)).collect();
            record_active(sessions.len());
            removed
        };
        removed.len()
    }

    async fn handle(self: &Arc<Self>, request: IncomingRequest) -> OutgoingResponse {
        let method = request.method();
        let session_id = request
            .get_header_field_value("Session")
            .and_then(|value| SessionHeader::try_from(value).ok())
            .map(|header| header.id().to_owned());
        let known = session_id.as_deref().is_some_and(|id| self.touch(id));

        let route = self.route(&request);
        if method != Method::Options && route.is_none() {
            return reply::not_found();
        }
        let (mount, track) = route.unwrap_or((0, None));
        let required = required_features(&request);
        if method != Method::Options {
            if let Some(auth) = &self.config.auth {
                let authorization = request.get_header_field_value("Authorization");
                match auth.verify(method.as_ref(), authorization) {
                    Verdict::Valid => {}
                    // An established session keeps the nonce it started with.
                    Verdict::Stale if known => {}
                    _ => {
                        if authorization.is_some() {
                            metrics::counter!("kamoder_rtsp_auth_failures_total").increment(1);
                            tracing::info!(
                                peer = %request.connection().remote_addr(),
                                %method,
                                "RTSP authentication failed"
                            );
                        }
                        return reply::unauthorized([auth.challenge()]);
                    }
                }
            }
        }
        // OPTIONS may name no mount (`*`): the tag is understood if any
        // mount has a speaker.
        let speaker = match route {
            Some((mount, _)) => self.mounts[mount].speaker.is_some(),
            None => self.mounts.iter().any(|mount| mount.speaker.is_some()),
        };
        let unsupported: Vec<&str> = required
            .iter()
            .map(String::as_str)
            .filter(|tag| !(speaker && *tag == backchannel::FEATURE))
            .collect();
        if !unsupported.is_empty() {
            tracing::info!(peer = %request.connection().remote_addr(), %method, ?unsupported, "RTSP Require not supported");
            return reply::option_not_supported(unsupported);
        }
        let backchannel = !required.is_empty();

        match method {
            Method::Options => OutgoingResponse::builder()
                .set_status(Status::OK)
                .add_header_field(("Public", PUBLIC))
                .build(),
            Method::Describe => self.describe(&request, mount, backchannel).await,
            Method::Setup => {
                let track = track.unwrap_or(Track::Video);
                self.setup(&request, mount, track, session_id.as_deref())
                    .await
            }
            Method::Play => self.play(&request, session_id.as_deref()),
            Method::Teardown => self.teardown(session_id.as_deref()),
            Method::GetParameter if session_id.is_some() && !known => reply::session_not_found(),
            Method::GetParameter => reply::ok(),
            _ => reply::method_not_allowed(PUBLIC),
        }
    }

    /// The mount whose path the request names — the stream path, the path
    /// with a trailing slash, or one of its track URLs — and the track, if
    /// one is named. The audio track of a mount without audio is not there,
    /// nor the backchannel of one without a speaker. SETUP of the
    /// backchannel without the `Require` tag is accepted: ONVIF's examples
    /// repeat it, but some clients only send it with DESCRIBE.
    fn route(&self, request: &IncomingRequest) -> Option<(usize, Option<Track>)> {
        let url = request.url()?;
        self.mounts.iter().enumerate().find_map(|(index, mount)| {
            let track = Track::from_rest(url.path().strip_prefix(mount.path.as_str())?)?;
            match track {
                Some(Track::Audio) if mount.audio.is_none() => return None,
                Some(Track::Backchannel) if mount.speaker.is_none() => return None,
                _ => {}
            }
            Some((index, track))
        })
    }

    /// `rtsp://host:port/path`, as the client addressed us.
    fn base_url(&self, request: &IncomingRequest, mount: usize) -> String {
        let netloc = request.url().map_or("", |url| url.netloc());
        // Credentials in the URL never go back out.
        let host = netloc.rsplit_once('@').map_or(netloc, |(_, host)| host);
        format!("rtsp://{host}{}", self.mounts[mount].path)
    }

    /// `backchannel`: the request carried the ONVIF tag (and `handle`
    /// checked the mount has a speaker).
    async fn describe(
        &self,
        request: &IncomingRequest,
        mount: usize,
        backchannel: bool,
    ) -> OutgoingResponse {
        let Some(sets) = self.mounts[mount].stream.parameters(PARAMETERS_WAIT).await else {
            tracing::warn!("DESCRIBE before the source produced SPS/PPS");
            return reply::empty_response(Status::from_static_str(503, "Service Unavailable"));
        };
        let Some(fmtp) = sets.sdp_parameters() else {
            return reply::internal_server_error("unparseable SPS");
        };
        let local = request.connection().local_addr().ip();
        let audio = self.mounts[mount].audio.is_some();
        let sdp = session_description(local, &fmtp, audio, backchannel);
        OutgoingResponse::builder()
            .set_status(Status::OK)
            .add_header_field(("Content-Type", "application/sdp"))
            .add_header_field((
                "Content-Base",
                format!("{}/", self.base_url(request, mount)),
            ))
            .set_body(Bytes::from(sdp.to_string()))
            .build()
    }

    async fn setup(
        &self,
        request: &IncomingRequest,
        mount: usize,
        track: Track,
        session: Option<&str>,
    ) -> OutgoingResponse {
        if let Some(id) = session {
            if let Some(refused) = add_refusal(self.sessions().get(id), mount, track) {
                return refused;
            }
        }
        let offers = request
            .get_header_field_value("Transport")
            .and_then(|value| value.to_str().ok())
            .and_then(|text| TransportHeaderV10::parse(text).ok())
            .unwrap_or_default();
        let connection = request.connection();
        let mut chosen = None;
        for offer in offers.iter().filter(|offer| offer.is_unicast()) {
            chosen = match offer.transport_id() {
                TransportHeaderV10::TRANSPORT_ID_RTP_AVP_TCP => interleaved(connection, offer),
                TransportHeaderV10::TRANSPORT_ID_RTP_AVP
                | TransportHeaderV10::TRANSPORT_ID_RTP_AVP_UDP => self.udp(connection, offer).await,
                _ => None,
            };
            if chosen.is_some() {
                break;
            }
        }
        let Some((transport, answer)) = chosen else {
            return reply::unsupported_transport();
        };

        let control = match request.url() {
            Some(url) if url.path().ends_with(track.control()) => url.to_string(),
            _ => format!("{}/{}", self.base_url(request, mount), track.control()),
        };
        let set_up = SetUp {
            control,
            interleaved: matches!(transport, Transport::Interleaved { .. }),
            state: State::Ready(transport),
        };
        let peer = connection.remote_addr();
        let id = match session {
            Some(id) => {
                let mut sessions = self.sessions();
                // Checked again: the lock was released while binding ports.
                if let Some(refused) = add_refusal(sessions.get(id), mount, track) {
                    return refused;
                }
                if let Some(entry) = sessions.get_mut(id) {
                    entry.tracks[track.index()] = Some(set_up);
                }
                id.to_owned()
            }
            None => {
                let id = format!("{:016X}", rand::random::<u64>());
                let mut tracks: [Option<SetUp>; TRACKS] = Default::default();
                tracks[track.index()] = Some(set_up);
                let mut sessions = self.sessions();
                sessions.insert(
                    id.clone(),
                    Session {
                        peer,
                        mount,
                        last_seen: Instant::now(),
                        tracks,
                    },
                );
                record_active(sessions.len());
                metrics::counter!("kamoder_rtsp_sessions_total").increment(1);
                id
            }
        };
        tracing::info!(%peer, session = %id, ?track, transport = %answer, "RTSP track set up");
        OutgoingResponse::builder()
            .set_status(Status::OK)
            .add_header_field(("Transport", answer.to_string()))
            .add_header_field(("Session", self.session_header(&id)))
            .build()
    }

    /// Binds an RTP/RTCP port pair for `offer`'s `client_port`.
    async fn udp(
        &self,
        connection: &msf_rtsp::server::ConnectionHandle,
        offer: &TransportHeaderV10,
    ) -> Option<(Transport, TransportHeaderV10)> {
        let client = offer.client_port_pair()?;
        let client_rtcp = client.rtcp().unwrap_or(client.rtp().wrapping_add(1));
        let ports = &self.config.udp_ports;
        let first = ports.start + ports.start % 2;
        for rtp_port in (first..ports.end.saturating_sub(1)).step_by(2) {
            let Ok(rtp) = connection
                .get_udp_channel(rtp_port, client.rtp(), None)
                .await
            else {
                continue;
            };
            let Ok(rtcp) = connection
                .get_udp_channel(rtp_port + 1, client_rtcp, None)
                .await
            else {
                continue;
            };
            let answer = TransportHeaderV10::new()
                .with_transport_id(TransportHeaderV10::TRANSPORT_ID_RTP_AVP)
                .with_unicast(true)
                .with_client_port_pair((client.rtp(), client_rtcp))
                .with_server_port_pair((rtp_port, rtp_port + 1));
            let transport = Transport::Udp {
                rtp: Box::new(rtp),
                rtcp: Box::new(rtcp),
            };
            return Some((transport, answer));
        }
        tracing::warn!(?ports, "no free UDP port pair for an RTSP session");
        None
    }

    fn play(
        self: &Arc<Self>,
        request: &IncomingRequest,
        session: Option<&str>,
    ) -> OutgoingResponse {
        let Some(id) = session else {
            return reply::session_not_found();
        };
        let ok = || {
            OutgoingResponse::builder()
                .set_status(Status::OK)
                .add_header_field(("Session", self.session_header(id)))
                .add_header_field(("Range", "npt=now-"))
        };
        let mut sessions = self.sessions();
        let Some(entry) = sessions.get_mut(id) else {
            return reply::session_not_found();
        };
        let mount = &self.mounts[entry.mount];
        let mut infos = Vec::new();
        let mut started = 0;
        for track in Track::ALL {
            let slot = &mut entry.tracks[track.index()];
            // Not set up, or already playing (a client re-sending PLAY: keep
            // going; the live sequence number is not known here, so no
            // RTP-Info).
            let Some(SetUp {
                control,
                interleaved,
                state: State::Ready(transport),
            }) = slot.take_if(|set_up| matches!(set_up.state, State::Ready(_)))
            else {
                continue;
            };
            let start = session::Start::random();
            let finished = {
                let shared = Arc::downgrade(self);
                let id = id.to_owned();
                move || {
                    if let Some(shared) = shared.upgrade() {
                        shared.remove_where(|candidate, _| candidate == id);
                    }
                }
            };
            // `route` only lets the audio track (backchannel) be set up on a
            // mount with audio (a speaker), so `mount.audio` (`speaker`) is
            // there.
            let sender = match track {
                Track::Video => {
                    let rtp = self.rtp_transport(id, start.ssrc, VIDEO_CLOCK, transport, false);
                    session::spawn(mount.stream.subscribe(), rtp, start, finished)
                }
                Track::Audio => {
                    let Some(audio) = &mount.audio else { continue };
                    let rtp = self.rtp_transport(
                        id,
                        start.ssrc,
                        crate::audio::CLOCK_RATE,
                        transport,
                        false,
                    );
                    session::spawn_audio(audio.subscribe(), rtp, start, finished)
                }
                Track::Backchannel => {
                    let Some(speaker) = &mount.speaker else {
                        continue;
                    };
                    // The client's RTP is the keepalive here: a talking
                    // client need not send RTCP.
                    let rtp = self.rtp_transport(
                        id,
                        start.ssrc,
                        crate::audio::CLOCK_RATE,
                        transport,
                        true,
                    );
                    let receiver = backchannel::spawn(rtp, speaker.clone(), finished);
                    started += 1;
                    *slot = Some(SetUp {
                        control,
                        interleaved,
                        state: State::Playing(receiver),
                    });
                    continue;
                }
            };
            started += 1;
            infos.push(
                RTPInfo::new(control.as_str())
                    .with_seq(start.seq)
                    .with_rtp_time(start.timestamp)
                    .to_string(),
            );
            *slot = Some(SetUp {
                control,
                interleaved,
                state: State::Playing(sender),
            });
        }
        drop(sessions);
        if started == 0 {
            return ok().build();
        }
        tracing::info!(peer = %request.connection().remote_addr(), session = %id, tracks = started, "RTSP playing");
        if infos.is_empty() {
            return ok().build();
        }
        ok().add_header_field(("RTP-Info", infos.join(","))).build()
    }

    /// RTP + RTCP (sender reports on `clock_rate`) for one track of a
    /// session; any RTCP from the client counts as keepalive, and so does
    /// its RTP if `rtp_keepalive` (the backchannel).
    fn rtp_transport(
        self: &Arc<Self>,
        id: &str,
        ssrc: u32,
        clock_rate: u32,
        transport: Transport,
        rtp_keepalive: bool,
    ) -> RtpTransport {
        match transport {
            Transport::Interleaved { rtp, rtcp } => {
                self.channel_pair(id, ssrc, clock_rate, rtp, rtcp, rtp_keepalive)
            }
            Transport::Udp { rtp, rtcp } => {
                self.channel_pair(id, ssrc, clock_rate, rtp, rtcp, rtp_keepalive)
            }
        }
    }

    fn channel_pair<C, E>(
        self: &Arc<Self>,
        id: &str,
        ssrc: u32,
        clock_rate: u32,
        rtp: C,
        rtcp: C,
        rtp_keepalive: bool,
    ) -> RtpTransport
    where
        C: futures::Stream<Item = Result<Bytes, E>> + futures::Sink<Bytes> + Send + 'static,
        E: Send + 'static,
        msf_rtsp::Error: From<E> + From<<C as futures::Sink<Bytes>>::Error>,
    {
        let keepalive = {
            let shared = Arc::downgrade(self);
            let id = id.to_owned();
            move |_: &Result<Bytes, E>| {
                if let Some(shared) = shared.upgrade() {
                    shared.touch(&id);
                }
            }
        };
        let rtcp = rtcp.inspect(keepalive.clone());
        // Never let client RTP reach msf-rtp's reorder buffer unguarded
        // (see `SequenceGuard`): one packet could hang the server.
        let mut guard = session::SequenceGuard::default();
        let rtp = rtp.map(move |item| item.map(|packet| guard.admit(packet)));
        if rtp_keepalive {
            RtpTransport::new(ssrc, clock_rate, rtp.inspect(keepalive), rtcp, true)
        } else {
            RtpTransport::new(ssrc, clock_rate, rtp, rtcp, true)
        }
    }

    fn teardown(&self, session: Option<&str>) -> OutgoingResponse {
        let Some(id) = session else {
            return reply::session_not_found();
        };
        if self.remove_where(|candidate, _| candidate == id) == 0 {
            return reply::session_not_found();
        }
        tracing::info!(session = %id, "RTSP session torn down");
        reply::ok()
    }

    fn session_header(&self, id: &str) -> String {
        SessionHeader::new(id)
            .with_timeout(self.config.session_timeout.as_secs().max(1))
            .to_string()
    }
}

/// The RTP clock of the video track.
const VIDEO_CLOCK: u32 = crate::source::CLOCK_RATE as u32;

/// Why `track` of `mount` cannot join `session`, if it cannot: the session
/// is unknown, plays another mount, already has the track, or is already
/// playing (tracks are added before PLAY).
fn add_refusal(session: Option<&Session>, mount: usize, track: Track) -> Option<OutgoingResponse> {
    let Some(session) = session else {
        return Some(reply::session_not_found());
    };
    let conflict =
        session.mount != mount || session.tracks[track.index()].is_some() || session.playing();
    conflict.then(reply::method_not_valid_in_this_state)
}

/// Opens the interleaved channel pair `offer` asks for (or the next free one).
fn interleaved(
    connection: &msf_rtsp::server::ConnectionHandle,
    offer: &TransportHeaderV10,
) -> Option<(Transport, TransportHeaderV10)> {
    let requested = offer.interleaved_pair();
    let rtp = connection
        .get_interleaved_channel(requested.map(|pair| pair.rtp()))
        .ok()?;
    let rtcp_id = requested
        .and_then(|pair| pair.rtcp())
        .unwrap_or(rtp.id().wrapping_add(1));
    let rtcp = connection.get_interleaved_channel(Some(rtcp_id)).ok()?;
    let answer = TransportHeaderV10::new()
        .with_transport_id(TransportHeaderV10::TRANSPORT_ID_RTP_AVP_TCP)
        .with_unicast(true)
        .with_interleaved_pair((rtp.id(), rtcp.id()));
    Some((Transport::Interleaved { rtp, rtcp }, answer))
}

/// The feature tags of every `Require` header, trimmed, empty ones skipped.
fn required_features(request: &IncomingRequest) -> Vec<String> {
    request
        .get_header_fields("Require")
        .filter_map(|field| field.value()?.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The SDP: the H.264 track, then the PCMA track when `audio` (AUD-10),
/// then, when `backchannel`, the ONVIF backchannel (AUD-29).
///
/// With the backchannel, every medium carries the direction attribute
/// ONVIF requires, from the *client's* point of view (Streaming Spec
/// Ver. 26.06 § 5.3.2: `a=sendonly` for "media being sent from the client
/// to the server", `a=recvonly` for media "sent from the server to the
/// client"). Without it the SDP is exactly what it was before, so ordinary
/// clients see no change.
fn session_description(
    local: IpAddr,
    fmtp: &msf_sdp::h264::H264Parameters,
    audio: bool,
    backchannel: bool,
) -> SessionDescription {
    let address_type = match local {
        IpAddr::V4(_) => AddressType::IPv4,
        IpAddr::V6(_) => AddressType::IPv6,
    };
    let unspecified = match local {
        IpAddr::V4(_) => IpAddr::from([0u8; 4]),
        IpAddr::V6(_) => IpAddr::from([0u16; 8]),
    };
    let mut media = MediaDescription::builder("video", 0, "RTP/AVP");
    media
        .format(PAYLOAD_TYPE)
        .attribute("rtpmap", RTPMap::new(PAYLOAD_TYPE, "H264", 90_000))
        .attribute("fmtp", FormatParameters::new(PAYLOAD_TYPE, fmtp))
        .attribute("control", Track::Video.control());
    if backchannel {
        media.flag("recvonly");
    }
    let mut sdp = SessionDescription::builder();
    sdp.origin(Origin::new(
        "-",
        rand::random::<u32>().into(),
        1,
        NetworkType::Internet,
        address_type,
        local,
    ))
    .session_name("Camera")
    .connection(ConnectionInfo::new(
        NetworkType::Internet,
        ConnectionAddress::unicast(unspecified),
    ))
    .attribute("control", "*")
    .attribute("range", "npt=now-")
    .media_description(media.build());
    if audio {
        let pt = crate::audio::PAYLOAD_TYPE;
        let mut media = MediaDescription::builder("audio", 0, "RTP/AVP");
        media
            .format(pt)
            .attribute("rtpmap", RTPMap::new(pt, "PCMA", crate::audio::CLOCK_RATE))
            .attribute("control", Track::Audio.control());
        if backchannel {
            media.flag("recvonly");
        }
        sdp.media_description(media.build());
    }
    if backchannel {
        let pt = backchannel::PCMA;
        let mut media = MediaDescription::builder("audio", 0, "RTP/AVP");
        media
            .format(pt)
            .attribute("rtpmap", RTPMap::new(pt, "PCMA", crate::audio::CLOCK_RATE))
            .attribute("control", Track::Backchannel.control())
            .flag("sendonly");
        sdp.media_description(media.build());
    }
    sdp.build()
}

/// Expires sessions nothing has been heard from.
async fn reap(shared: std::sync::Weak<Shared>) {
    let period = {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        (shared.config.session_timeout / 4).max(Duration::from_millis(250))
    };
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let timeout = shared.config.session_timeout;
        let expired = shared.remove_where(|id, session| {
            let expired = session.last_seen.elapsed() > timeout;
            if expired {
                tracing::info!(session = %id, peer = %session.peer, "RTSP session timed out");
            }
            expired
        });
        if expired > 0 {
            tracing::debug!(expired, "reaped RTSP sessions");
        }
    }
}

fn record_active(count: usize) {
    metrics::gauge!("kamoder_rtsp_sessions_active").set(count as f64);
}

/// The `msf-rtsp` request handler: a cheap handle on [`Shared`].
#[derive(Clone)]
struct Handler {
    shared: Arc<Shared>,
}

impl RequestHandler for Handler {
    async fn try_handle_request(
        self,
        request: IncomingRequest,
    ) -> Result<OutgoingResponse, msf_rtsp::Error> {
        Ok(self.shared.handle(request).await)
    }
}

/// Wraps the default connection handler to learn when a connection closes,
/// which ends its interleaved sessions at once rather than at the timeout.
struct Tracked {
    inner: DefaultConnectionHandler<TcpStream, DefaultConnectionInfo, Handler>,
    shared: Arc<Shared>,
}

impl ConnectionHandler for Tracked {
    type Connection = TcpStream;
    type ConnectionInfo = DefaultConnectionInfo;
    type RtspChannel =
        <DefaultConnectionHandler<TcpStream, DefaultConnectionInfo, Handler> as ConnectionHandler>::RtspChannel;

    fn handle_connection(
        &self,
        connection: TcpStream,
    ) -> impl std::future::Future<Output = Result<(), msf_rtsp::Error>> + Send + 'static {
        let peer = connection.peer_addr().ok();
        let served = self.inner.handle_connection(connection);
        let shared = Arc::clone(&self.shared);
        async move {
            let result = served.await;
            if let Some(peer) = peer {
                let ended =
                    shared.remove_where(|_, session| session.peer == peer && session.interleaved());
                if ended > 0 {
                    tracing::info!(%peer, sessions = ended, "RTSP connection closed");
                }
            }
            result
        }
    }

    fn create_rtsp_channel(
        &self,
        connection: TcpStream,
        info: DefaultConnectionInfo,
    ) -> Self::RtspChannel {
        self.inner.create_rtsp_channel(connection, info)
    }

    fn handle_rtsp_channel(
        &self,
        channel: Self::RtspChannel,
    ) -> impl std::future::Future<Output = Result<(), msf_rtsp::Error>> + Send + 'static {
        self.inner.handle_rtsp_channel(channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::ParameterSets;

    fn sdp(audio: bool) -> String {
        sdp_with(audio, false)
    }

    fn sdp_with(audio: bool, backchannel: bool) -> String {
        let sets = ParameterSets {
            sps: Bytes::from_static(&[0x67, 0x42, 0xc0, 0x1e, 0x8c, 0x8d]),
            pps: Bytes::from_static(&[0x68, 0xce, 0x3c, 0x80]),
        };
        let fmtp = sets.sdp_parameters().unwrap();
        session_description(IpAddr::from([192, 168, 1, 10]), &fmtp, audio, backchannel).to_string()
    }

    #[test]
    fn video_only_sdp_has_one_track() {
        let sdp = sdp(false);
        assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"), "{sdp}");
        assert!(sdp.contains("a=rtpmap:96 H264/90000\r\n"), "{sdp}");
        assert!(sdp.contains("a=control:trackID=0\r\n"), "{sdp}");
        assert!(sdp.contains("a=control:*\r\n"), "{sdp}");
        assert!(!sdp.contains("m=audio"), "{sdp}");
        assert!(!sdp.contains("trackID=1"), "{sdp}");
    }

    #[test]
    fn audio_sdp_adds_pcma_after_video() {
        let sdp = sdp(true);
        let video = sdp.find("m=video 0 RTP/AVP 96\r\n").expect("video m-line");
        let audio = sdp.find("m=audio 0 RTP/AVP 8\r\n").expect("audio m-line");
        assert!(video < audio, "{sdp}");
        let audio_section = &sdp[audio..];
        assert!(audio_section.contains("a=rtpmap:8 PCMA/8000\r\n"), "{sdp}");
        assert!(audio_section.contains("a=control:trackID=1\r\n"), "{sdp}");
        assert!(!audio_section.contains("fmtp"), "{sdp}");
        // The video section keeps its own control.
        assert!(
            sdp[video..audio].contains("a=control:trackID=0\r\n"),
            "{sdp}"
        );
    }

    #[test]
    fn sdp_without_require_has_no_backchannel_or_directions() {
        for audio in [false, true] {
            let sdp = sdp(audio);
            assert!(!sdp.contains("trackID=2"), "{sdp}");
            assert!(!sdp.contains("sendonly"), "{sdp}");
            assert!(!sdp.contains("recvonly"), "{sdp}");
        }
    }

    #[test]
    fn backchannel_sdp_adds_a_sendonly_pcma_section_last() {
        let sdp = sdp_with(true, true);
        let video = sdp.find("m=video 0 RTP/AVP 96\r\n").expect("video m-line");
        let audio = sdp.find("m=audio 0 RTP/AVP 8\r\n").expect("audio m-line");
        let back = sdp
            .rfind("m=audio 0 RTP/AVP 8\r\n")
            .expect("backchannel m-line");
        assert!(video < audio && audio < back, "{sdp}");
        // ONVIF Streaming § 5.3.2: directions from the client's side.
        assert!(sdp[video..audio].contains("a=recvonly\r\n"), "{sdp}");
        assert!(sdp[audio..back].contains("a=recvonly\r\n"), "{sdp}");
        assert!(
            sdp[audio..back].contains("a=control:trackID=1\r\n"),
            "{sdp}"
        );
        let section = &sdp[back..];
        assert!(section.contains("a=rtpmap:8 PCMA/8000\r\n"), "{sdp}");
        assert!(section.contains("a=control:trackID=2\r\n"), "{sdp}");
        assert!(section.contains("a=sendonly\r\n"), "{sdp}");
        assert!(!section.contains("recvonly"), "{sdp}");

        // A board with a speaker but no microphone.
        let sdp = sdp_with(false, true);
        assert_eq!(sdp.matches("m=audio").count(), 1, "{sdp}");
        assert!(sdp.contains("a=control:trackID=2\r\n"), "{sdp}");
        assert!(!sdp.contains("trackID=1"), "{sdp}");
    }

    #[test]
    fn track_urls() {
        assert_eq!(Track::from_rest(""), Some(None));
        assert_eq!(Track::from_rest("/"), Some(None));
        assert_eq!(Track::from_rest("/trackID=0"), Some(Some(Track::Video)));
        assert_eq!(Track::from_rest("/trackID=1"), Some(Some(Track::Audio)));
        assert_eq!(
            Track::from_rest("/trackID=2"),
            Some(Some(Track::Backchannel))
        );
        assert_eq!(Track::from_rest("/trackID=3"), None);
        assert_eq!(Track::from_rest("x"), None);
        for track in Track::ALL {
            assert_eq!(Track::ALL[track.index()], track);
        }
    }
}
