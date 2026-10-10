//! The RTSP 1.0 method handlers and the session table.
//!
//! `msf-rtsp` frames requests and responses, demultiplexes interleaved data,
//! opens UDP channels and runs RTCP; everything RTSP *means* is here:
//!
//! - **OPTIONS** — `Public`. Never asks for credentials.
//! - **DESCRIBE** — SDP from `msf_sdp`: one H.264 track, payload type 96,
//!   `packetization-mode=1` plus `profile-level-id`/`sprop-parameter-sets`
//!   from the stream's own SPS/PPS, and a `Content-Base` for the track URL.
//! - **SETUP** — `RTP/AVP/TCP;interleaved` on the RTSP connection, or
//!   `RTP/AVP;unicast;client_port` with a server port pair allocated from
//!   `--udp-ports`. RTP always goes to the peer of the RTSP connection; a
//!   `destination` parameter is ignored, so the camera cannot be turned into
//!   a UDP reflector.
//! - **PLAY** — starts the session's sender (`session.rs`) and answers with
//!   the `RTP-Info` seq/rtptime it will start from.
//! - **TEARDOWN**, **GET_PARAMETER** (keepalive).
//!
//! A session lives until TEARDOWN, until nothing has been heard from it for
//! the `Session` timeout (any request naming it, or RTCP from the client),
//! or — for interleaved transport — until its RTSP connection closes. A UDP
//! session outlives its connection, as RFC 2326 allows; the timeout reaps it
//! if the client is gone.

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

/// The track's control URL, relative to `Content-Base`.
const TRACK: &str = "trackID=0";

/// How long DESCRIBE waits for the source's first SPS/PPS.
const PARAMETERS_WAIT: Duration = Duration::from_secs(5);

/// What the server is told at start-up.
pub struct Config {
    /// Stream path with its leading slash, e.g. `/vs0`.
    pub path: String,
    /// Inactivity before a session is reaped; advertised in `Session`.
    pub session_timeout: Duration,
    /// Local ports for UDP transport, used as RTP/RTCP pairs from an even
    /// start.
    pub udp_ports: Range<u16>,
    /// `None` serves anyone (`--no-auth`).
    pub auth: Option<DigestAuth>,
}

/// Serves RTSP on `listener` until the listener fails.
pub async fn serve(listener: TcpListener, stream: Stream, config: Config) -> eyre::Result<()> {
    // The connection read timeout must not fire before the session timeout
    // would: a UDP client may legitimately be silent on the RTSP connection
    // between keepalives.
    let read_timeout = config.session_timeout + Duration::from_secs(15);
    let shared = Arc::new(Shared {
        config,
        stream,
        sessions: Mutex::new(HashMap::new()),
    });
    let reaper = tokio::spawn(reap(Arc::downgrade(&shared)));
    let handler = Handler {
        shared: Arc::clone(&shared),
    };
    let served = RtspServer::builder()
        .read_timeout(Some(read_timeout))
        .build(listener)
        .serve_with_connection_handler(handler, |inner| Tracked { inner, shared })
        .await;
    reaper.abort();
    served.map_err(|error| eyre::eyre!("RTSP server stopped: {error}"))
}

/// Server-wide state behind every request.
struct Shared {
    config: Config,
    stream: Stream,
    sessions: Mutex<HashMap<String, Session>>,
}

/// One RTSP session (one client, one track).
struct Session {
    /// The RTSP connection that created it.
    peer: SocketAddr,
    /// The track URL from SETUP, echoed in `RTP-Info`.
    control: String,
    last_seen: Instant,
    state: State,
}

enum State {
    /// Set up, channels open, not playing yet.
    Ready(Transport),
    /// The sender task owns the channels; dropping this stops it.
    Playing { _sender: Sender, interleaved: bool },
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
    fn interleaved(&self) -> bool {
        match &self.state {
            State::Ready(transport) => matches!(transport, Transport::Interleaved { .. }),
            State::Playing { interleaved, .. } => *interleaved,
        }
    }
}

impl Shared {
    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Session>> {
        // A panic while holding the lock leaves a consistent map: every
        // mutation is a single insert or remove.
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

        if method != Method::Options && !self.path_matches(&request) {
            return reply::not_found();
        }
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

        match method {
            Method::Options => OutgoingResponse::builder()
                .set_status(Status::OK)
                .add_header_field(("Public", PUBLIC))
                .build(),
            Method::Describe => self.describe(&request).await,
            Method::Setup => self.setup(&request, session_id.as_deref()).await,
            Method::Play => self.play(&request, session_id.as_deref()),
            Method::Teardown => self.teardown(session_id.as_deref()),
            Method::GetParameter if session_id.is_some() && !known => reply::session_not_found(),
            Method::GetParameter => reply::ok(),
            _ => reply::method_not_allowed(PUBLIC),
        }
    }

    /// The stream path, the path with a trailing slash, or the track URL.
    fn path_matches(&self, request: &IncomingRequest) -> bool {
        let Some(url) = request.url() else {
            return false;
        };
        let path = &self.config.path;
        match url.path().strip_prefix(path.as_str()) {
            Some(rest) => matches!(rest.trim_start_matches('/'), "" | TRACK),
            None => false,
        }
    }

    /// `rtsp://host:port/path`, as the client addressed us.
    fn base_url(&self, request: &IncomingRequest) -> String {
        let netloc = request.url().map_or("", |url| url.netloc());
        // Credentials in the URL never go back out.
        let host = netloc.rsplit_once('@').map_or(netloc, |(_, host)| host);
        format!("rtsp://{host}{}", self.config.path)
    }

    async fn describe(&self, request: &IncomingRequest) -> OutgoingResponse {
        let Some(sets) = self.stream.parameters(PARAMETERS_WAIT).await else {
            tracing::warn!("DESCRIBE before the source produced SPS/PPS");
            return reply::empty_response(Status::from_static_str(503, "Service Unavailable"));
        };
        let Some(fmtp) = sets.sdp_parameters() else {
            return reply::internal_server_error("unparseable SPS");
        };
        let local = request.connection().local_addr().ip();
        let sdp = session_description(local, &fmtp);
        OutgoingResponse::builder()
            .set_status(Status::OK)
            .add_header_field(("Content-Type", "application/sdp"))
            .add_header_field(("Content-Base", format!("{}/", self.base_url(request))))
            .set_body(Bytes::from(sdp.to_string()))
            .build()
    }

    async fn setup(&self, request: &IncomingRequest, session: Option<&str>) -> OutgoingResponse {
        if let Some(id) = session {
            // One track per session: a second SETUP has nothing to add.
            return if self.sessions().contains_key(id) {
                reply::method_not_valid_in_this_state()
            } else {
                reply::session_not_found()
            };
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

        let id = format!("{:016X}", rand::random::<u64>());
        let control = match request.url() {
            Some(url) if url.path().ends_with(TRACK) => url.to_string(),
            _ => format!("{}/{TRACK}", self.base_url(request)),
        };
        let peer = connection.remote_addr();
        {
            let mut sessions = self.sessions();
            sessions.insert(
                id.clone(),
                Session {
                    peer,
                    control,
                    last_seen: Instant::now(),
                    state: State::Ready(transport),
                },
            );
            record_active(sessions.len());
        }
        metrics::counter!("kamoder_rtsp_sessions_total").increment(1);
        tracing::info!(%peer, session = %id, transport = %answer, "RTSP session set up");
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
        let Some(mut entry) = sessions.remove(id) else {
            return reply::session_not_found();
        };
        let transport = match entry.state {
            State::Ready(transport) => transport,
            // Already playing (a client re-sending PLAY): keep going. The
            // live sequence number is not known here, so no RTP-Info.
            playing @ State::Playing { .. } => {
                entry.state = playing;
                sessions.insert(id.to_owned(), entry);
                return ok().build();
            }
        };
        let start = session::Start::random();
        let info = RTPInfo::new(entry.control.as_str())
            .with_seq(start.seq)
            .with_rtp_time(start.timestamp);
        let (rtp, interleaved) = match transport {
            Transport::Interleaved { rtp, rtcp } => {
                (self.rtp_transport(id, start.ssrc, rtp, rtcp), true)
            }
            Transport::Udp { rtp, rtcp } => (self.rtp_transport(id, start.ssrc, rtp, rtcp), false),
        };
        let finished = {
            let shared = Arc::downgrade(self);
            let id = id.to_owned();
            move || {
                if let Some(shared) = shared.upgrade() {
                    shared.remove_where(|candidate, _| candidate == id);
                }
            }
        };
        let sender = session::spawn(self.stream.subscribe(), rtp, start, finished);
        entry.state = State::Playing {
            _sender: sender,
            interleaved,
        };
        sessions.insert(id.to_owned(), entry);
        drop(sessions);
        tracing::info!(peer = %request.connection().remote_addr(), session = %id, "RTSP playing");
        ok().add_header_field(("RTP-Info", info.to_string()))
            .build()
    }

    /// RTP + RTCP for one session; any RTCP from the client counts as
    /// keepalive.
    fn rtp_transport<C, E>(self: &Arc<Self>, id: &str, ssrc: u32, rtp: C, rtcp: C) -> RtpTransport
    where
        C: futures::Stream<Item = Result<Bytes, E>> + futures::Sink<Bytes> + Send + 'static,
        E: Send + 'static,
        msf_rtsp::Error: From<E> + From<<C as futures::Sink<Bytes>>::Error>,
    {
        let shared = Arc::downgrade(self);
        let id = id.to_owned();
        let rtcp = rtcp.inspect(move |_| {
            if let Some(shared) = shared.upgrade() {
                shared.touch(&id);
            }
        });
        RtpTransport::new(ssrc, crate::source::CLOCK_RATE as u32, rtp, rtcp, true)
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

/// The SDP for the single H.264 track.
fn session_description(local: IpAddr, fmtp: &msf_sdp::h264::H264Parameters) -> SessionDescription {
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
        .attribute("control", TRACK);
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
