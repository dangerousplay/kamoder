//! End-to-end: the server in-process on loopback, playing
//! `tests/data/clip.h264`, against `retina` (a real RTSP client that
//! depacketizes) and a raw socket for what retina hides (status codes,
//! timing).
//!
//! The clip is 2 s of `testsrc`, 320x240, Constrained Baseline, GOP 25,
//! generated with
//!
//! ```text
//! ffmpeg -f lavfi -i testsrc=size=320x240:rate=25 -t 2 -c:v libx264 \
//!     -profile:v baseline -level 3.0 -pix_fmt yuv420p -g 25 -keyint_min 25 \
//!     -sc_threshold 0 -crf 30 -f h264 clip.h264
//! ```
//!
//! Its IDRs are ~3 KB, so FU-A fragmentation is exercised too.

use bytes::Bytes;
use futures::StreamExt;
use retina::client::{
    Credentials, PlayOptions, Session, SessionOptions, SetupOptions, TcpTransportOptions,
    Transport, UdpTransportOptions,
};
use retina::codec::{CodecItem, FrameFormat, ParameterSetInsertion};
use rtsp::digest::DigestAuth;
use rtsp::h264::{nal_type, nal_units, NAL_PPS, NAL_SPS};
use rtsp::server::{self, Config, Mount};
use rtsp::source::FileSource;
use rtsp::stream::Stream;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CLIP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/clip.h264");
/// Faster than real time, so the tests do not crawl.
const FPS: u32 = 50;
const USER: &str = "admin";
const PASSWORD: &str = "s3cret";

/// A distinct UDP port block per server, so tests running in parallel do not
/// even have to retry.
static NEXT_PORTS: AtomicU16 = AtomicU16::new(41000);

struct Server {
    addr: SocketAddr,
}

impl Server {
    async fn start(auth: bool, session_timeout: Duration) -> Self {
        let source = FileSource::open(Path::new(CLIP), FPS).unwrap();
        let (stream, _pump) = Stream::spawn(source);
        // A second mount of the same clip, so path routing is exercised.
        let (second, _pump2) = Stream::spawn(FileSource::open(Path::new(CLIP), FPS).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let first = NEXT_PORTS.fetch_add(40, Ordering::Relaxed);
        let config = Config {
            session_timeout,
            udp_ports: first..first + 40,
            auth: auth.then(|| DigestAuth::new("Camera", USER, PASSWORD)),
        };
        let mounts = vec![
            Mount {
                path: "/vs0".to_owned(),
                stream,
            },
            Mount {
                path: "/vs1".to_owned(),
                stream: second,
            },
        ];
        tokio::spawn(server::serve(listener, mounts, config));
        Self { addr }
    }

    fn url(&self) -> url::Url {
        format!("rtsp://{}/vs0", self.addr).parse().unwrap()
    }
}

fn creds(password: &str) -> Option<Credentials> {
    Some(Credentials {
        username: USER.to_owned(),
        password: password.to_owned(),
    })
}

/// The clip's access units as NAL lists, without SPS/PPS (which retina moves
/// out of band), in play order.
fn expected_units() -> Vec<Vec<Bytes>> {
    let data = Bytes::from(std::fs::read(CLIP).unwrap());
    let mut splitter = rtsp::h264::AccessUnitSplitter::default();
    let mut units: Vec<Bytes> = nal_units(&data)
        .filter_map(|nal| splitter.push(&nal).map(|(au, _)| au))
        .collect();
    units.extend(splitter.finish().map(|(au, _)| au));
    units
        .iter()
        .map(|au| {
            nal_units(au)
                .filter(|nal| !matches!(nal_type(nal), NAL_SPS | NAL_PPS))
                .collect()
        })
        .collect()
}

/// DESCRIBE + SETUP + PLAY with `transport`, then `count` frames, each as
/// its NAL units (SPS/PPS stripped) and whether retina saw a keyframe.
async fn receive(server: &Server, transport: Transport, count: usize) -> Vec<(Vec<Bytes>, bool)> {
    let options = SessionOptions::default().creds(creds(PASSWORD));
    let mut session = Session::describe(server.url(), options).await.unwrap();
    let format = FrameFormat {
        parameter_set_insertion: ParameterSetInsertion::Never,
        ..FrameFormat::SIMPLE
    };
    session
        .setup(
            0,
            SetupOptions::default()
                .transport(transport)
                .frame_format(format),
        )
        .await
        .unwrap();
    let mut frames = session
        .play(PlayOptions::default())
        .await
        .unwrap()
        .demuxed()
        .unwrap();
    let mut received = Vec::new();
    while received.len() < count {
        let item = tokio::time::timeout(Duration::from_secs(5), frames.next())
            .await
            .expect("no frame within 5 s")
            .expect("stream ended")
            .expect("stream error");
        if let CodecItem::VideoFrame(frame) = item {
            assert_eq!(frame.loss(), 0, "packet loss on loopback");
            let data = Bytes::copy_from_slice(frame.data());
            let nals = nal_units(&data)
                .filter(|nal| !matches!(nal_type(nal), NAL_SPS | NAL_PPS))
                .collect();
            received.push((nals, frame.is_random_access_point()));
        }
    }
    received
}

/// The received frames are the clip, in order, from one of its IDRs on.
fn assert_matches_clip(received: &[(Vec<Bytes>, bool)]) {
    let expected = expected_units();
    assert!(received[0].1, "playback must start at a keyframe");
    let offset = (0..expected.len())
        .find(|&start| expected[start] == received[0].0)
        .expect("first frame is not an access unit of the clip");
    for (index, (nals, _)) in received.iter().enumerate() {
        let want = &expected[(offset + index) % expected.len()];
        assert_eq!(
            nals.iter().map(|nal| nal_type(nal)).collect::<Vec<_>>(),
            want.iter().map(|nal| nal_type(nal)).collect::<Vec<_>>(),
            "NAL types of frame {index}"
        );
        assert!(nals == want, "frame {index} differs from the clip");
    }
}

#[tokio::test]
async fn describe_announces_the_h264_track() {
    let server = Server::start(true, Duration::from_secs(60)).await;
    let options = SessionOptions::default().creds(creds(PASSWORD));
    let session = Session::describe(server.url(), options).await.unwrap();
    let sdp = std::str::from_utf8(session.sdp()).unwrap().to_owned();
    assert!(sdp.starts_with("v=0\r\n"), "{sdp}");
    assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"), "{sdp}");
    assert!(sdp.contains("a=rtpmap:96 H264/90000\r\n"), "{sdp}");
    assert!(sdp.contains("a=control:trackID=0\r\n"), "{sdp}");

    // sprop-parameter-sets is the clip's own SPS and PPS.
    let data = Bytes::from(std::fs::read(CLIP).unwrap());
    let sps = nal_units(&data)
        .find(|nal| nal_type(nal) == NAL_SPS)
        .unwrap();
    let pps = nal_units(&data)
        .find(|nal| nal_type(nal) == NAL_PPS)
        .unwrap();
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let fmtp = format!(
        "a=fmtp:96 packetization-mode=1;profile-level-id={:02X}{:02X}{:02X};sprop-parameter-sets={},{}\r\n",
        sps[1],
        sps[2],
        sps[3],
        b64.encode(&sps),
        b64.encode(&pps)
    );
    assert!(sdp.contains(&fmtp), "{sdp}\nwanted {fmtp}");

    let stream = &session.streams()[0];
    assert_eq!(stream.media(), "video");
    assert_eq!(stream.encoding_name(), "h264");
    assert_eq!(stream.clock_rate_hz(), 90_000);
    assert_eq!(
        stream.control().map(|url| url.as_str().to_owned()),
        Some(format!("rtsp://{}/vs0/trackID=0", server.addr))
    );
    match stream.parameters() {
        Some(retina::codec::ParametersRef::Video(video)) => {
            assert_eq!(video.pixel_dimensions(), (320, 240));
        }
        other => panic!("no video parameters from sprop: {other:?}"),
    }

    // The raw response carries Content-Base and Content-Type.
    let mut raw = RawClient::connect(server.addr).await;
    let url = format!("rtsp://{}/vs0", server.addr);
    let challenge = raw.request("DESCRIBE", &url, &[]).await;
    let authorization = raw.authorization(&challenge, "DESCRIBE", &url, PASSWORD);
    let reply = raw
        .request("DESCRIBE", &url, &[("Authorization", &authorization)])
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("content-type"), Some("application/sdp"));
    assert_eq!(
        reply.header("content-base"),
        Some(format!("{url}/").as_str())
    );
}

#[tokio::test]
async fn plays_over_tcp_interleaved() {
    let server = Server::start(true, Duration::from_secs(60)).await;
    let received = receive(&server, Transport::Tcp(TcpTransportOptions::default()), 60).await;
    assert_matches_clip(&received);
}

#[tokio::test]
async fn plays_over_udp() {
    let server = Server::start(true, Duration::from_secs(60)).await;
    let received = receive(&server, Transport::Udp(UdpTransportOptions::default()), 60).await;
    assert_matches_clip(&received);
}

#[tokio::test]
async fn two_clients_play_at_once() {
    let server = Server::start(true, Duration::from_secs(60)).await;
    let (tcp, udp) = tokio::join!(
        receive(&server, Transport::Tcp(TcpTransportOptions::default()), 40),
        receive(&server, Transport::Udp(UdpTransportOptions::default()), 40),
    );
    assert_matches_clip(&tcp);
    assert_matches_clip(&udp);
}

#[tokio::test]
async fn digest_auth_is_enforced() {
    let server = Server::start(true, Duration::from_secs(60)).await;

    // No credentials: 401 with an MD5 qop=auth challenge.
    let mut raw = RawClient::connect(server.addr).await;
    let url = format!("rtsp://{}/vs0", server.addr);
    let challenge = raw.request("DESCRIBE", &url, &[]).await;
    assert_eq!(challenge.status, 401);
    let header = challenge.header("www-authenticate").unwrap();
    assert!(header.starts_with("Digest realm=\"Camera\""), "{header}");
    assert!(
        header.contains("qop=\"auth\"") && header.contains("algorithm=MD5"),
        "{header}"
    );
    // OPTIONS stays open, for clients that probe before authenticating.
    let options = raw.request("OPTIONS", &url, &[]).await;
    assert_eq!(options.status, 200);
    assert_eq!(options.header("public"), Some(server::PUBLIC));

    // Wrong password: 401 again.
    let wrong = raw.authorization(&challenge, "DESCRIBE", &url, "wrong");
    let reply = raw
        .request("DESCRIBE", &url, &[("Authorization", &wrong)])
        .await;
    assert_eq!(reply.status, 401);

    // Right password: 200.
    let right = raw.authorization(&challenge, "DESCRIBE", &url, PASSWORD);
    let reply = raw
        .request("DESCRIBE", &url, &[("Authorization", &right)])
        .await;
    assert_eq!(reply.status, 200);

    // retina sees the same.
    let error = Session::describe(server.url(), SessionOptions::default())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("401"), "{error}");
    let error = Session::describe(server.url(), SessionOptions::default().creds(creds("nope")))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("401"), "{error}");
}

#[tokio::test]
async fn no_auth_serves_without_credentials() {
    let server = Server::start(false, Duration::from_secs(60)).await;
    Session::describe(server.url(), SessionOptions::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn teardown_stops_sending() {
    let server = Server::start(false, Duration::from_secs(60)).await;
    let mut raw = RawClient::connect(server.addr).await;
    let url = format!("rtsp://{}/vs0", server.addr);
    let setup = raw
        .request(
            "SETUP",
            &format!("{url}/trackID=0"),
            &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
        )
        .await;
    assert_eq!(setup.status, 200);
    assert_eq!(
        setup.header("transport"),
        Some("RTP/AVP/TCP;unicast;interleaved=0-1")
    );
    let session = setup.session();
    assert!(setup.header("session").unwrap().ends_with(";timeout=60"));

    let play = raw.request("PLAY", &url, &[("Session", &session)]).await;
    assert_eq!(play.status, 200);
    let rtp_info = play.header("rtp-info").unwrap();
    assert!(
        rtp_info.starts_with(&format!("url={url}/trackID=0;seq="))
            && rtp_info.contains(";rtptime="),
        "{rtp_info}"
    );
    // Up to half a second (one GOP at 50 fps) waiting for an IDR, then data.
    assert!(raw.interleaved_for(Duration::from_millis(1500)).await > 10);

    let teardown = raw
        .request("TEARDOWN", &url, &[("Session", &session)])
        .await;
    assert_eq!(teardown.status, 200);
    // Whatever was already queued may still arrive; then nothing.
    raw.interleaved_for(Duration::from_millis(300)).await;
    assert_eq!(raw.interleaved_for(Duration::from_millis(800)).await, 0);
    let gone = raw
        .request("GET_PARAMETER", &url, &[("Session", &session)])
        .await;
    assert_eq!(gone.status, 454);
}

#[tokio::test]
async fn idle_sessions_time_out_and_keepalives_prevent_it() {
    let server = Server::start(false, Duration::from_secs(1)).await;
    let mut raw = RawClient::connect(server.addr).await;
    let url = format!("rtsp://{}/vs0", server.addr);
    let transport = [("Transport", "RTP/AVP;unicast;client_port=50000-50001")];
    let kept = raw.request("SETUP", &url, &transport).await;
    let idle = raw.request("SETUP", &url, &transport).await;
    assert_eq!((kept.status, idle.status), (200, 200));
    assert!(kept.header("transport").unwrap().contains(";server_port="));
    let (kept, idle) = (kept.session(), idle.session());
    assert_ne!(kept, idle);

    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let alive = raw
            .request("GET_PARAMETER", &url, &[("Session", &kept)])
            .await;
        assert_eq!(alive.status, 200);
    }
    let gone = raw
        .request("GET_PARAMETER", &url, &[("Session", &idle)])
        .await;
    assert_eq!(gone.status, 454);
}

#[tokio::test]
async fn closing_the_connection_ends_interleaved_sessions() {
    let server = Server::start(false, Duration::from_secs(60)).await;
    let url = format!("rtsp://{}/vs0", server.addr);
    let mut raw = RawClient::connect(server.addr).await;
    let setup = raw
        .request(
            "SETUP",
            &url,
            &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
        )
        .await;
    let session = setup.session();
    drop(raw);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut other = RawClient::connect(server.addr).await;
    let gone = other
        .request("GET_PARAMETER", &url, &[("Session", &session)])
        .await;
    assert_eq!(gone.status, 454);
}

#[tokio::test]
async fn wrong_paths_and_transports_are_refused() {
    let server = Server::start(false, Duration::from_secs(60)).await;
    let mut raw = RawClient::connect(server.addr).await;
    let second = format!("rtsp://{}/vs1", server.addr);
    assert_eq!(raw.request("DESCRIBE", &second, &[]).await.status, 200);
    let other = format!("rtsp://{}/vs9", server.addr);
    assert_eq!(raw.request("DESCRIBE", &other, &[]).await.status, 404);
    let url = format!("rtsp://{}/vs0", server.addr);
    let multicast = raw
        .request("SETUP", &url, &[("Transport", "RTP/AVP;multicast")])
        .await;
    assert_eq!(multicast.status, 461);
    assert_eq!(
        raw.request("PLAY", &url, &[("Session", "nope")])
            .await
            .status,
        454
    );
    assert_eq!(raw.request("PAUSE", &url, &[]).await.status, 405);
}

/// A minimal RTSP client over a raw socket: one request at a time, skipping
/// (and counting) interleaved data.
struct RawClient {
    socket: TcpStream,
    buffer: Vec<u8>,
    cseq: u32,
}

struct Reply {
    status: u16,
    headers: HashMap<String, String>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn session(&self) -> String {
        let value = self.header("session").expect("no Session header");
        value.split(';').next().unwrap().to_owned()
    }
}

impl RawClient {
    async fn connect(addr: SocketAddr) -> Self {
        Self {
            socket: TcpStream::connect(addr).await.unwrap(),
            buffer: Vec::new(),
            cseq: 0,
        }
    }

    async fn request(&mut self, method: &str, url: &str, headers: &[(&str, &str)]) -> Reply {
        self.cseq += 1;
        let mut text = format!("{method} {url} RTSP/1.0\r\nCSeq: {}\r\n", self.cseq);
        for (name, value) in headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        text.push_str("\r\n");
        self.socket.write_all(text.as_bytes()).await.unwrap();
        loop {
            if let Some(reply) = self.parse_reply() {
                return reply;
            }
            self.fill(Duration::from_secs(10))
                .await
                .expect("connection closed");
        }
    }

    /// Reads for `period` and counts the interleaved frames that arrive.
    async fn interleaved_for(&mut self, period: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + period;
        let mut frames = 0;
        loop {
            while self.skip_interleaved() {
                frames += 1;
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() || self.fill(left).await.is_none() {
                return frames;
            }
        }
    }

    /// Reads more; `None` on timeout or EOF.
    async fn fill(&mut self, timeout: Duration) -> Option<()> {
        let mut chunk = [0u8; 4096];
        match tokio::time::timeout(timeout, self.socket.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => None,
            Ok(Ok(read)) => {
                self.buffer.extend_from_slice(&chunk[..read]);
                Some(())
            }
            Ok(Err(error)) => panic!("read failed: {error}"),
        }
    }

    /// Drops one complete `$`-frame from the front of the buffer.
    fn skip_interleaved(&mut self) -> bool {
        if self.buffer.len() < 4 || self.buffer[0] != b'$' {
            return false;
        }
        let length = 4 + usize::from(u16::from_be_bytes([self.buffer[2], self.buffer[3]]));
        if self.buffer.len() < length {
            return false;
        }
        self.buffer.drain(..length);
        true
    }

    fn parse_reply(&mut self) -> Option<Reply> {
        while self.skip_interleaved() {}
        if self.buffer.first() == Some(&b'$') {
            return None;
        }
        let end = self.buffer.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
        let head = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines.next()?.split(' ').nth(1)?.parse().ok()?;
        let headers: HashMap<String, String> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        let body: usize = headers
            .get("content-length")
            .and_then(|length| length.parse().ok())
            .unwrap_or(0);
        if self.buffer.len() < end + body {
            return None;
        }
        self.buffer.drain(..end + body);
        Some(Reply { status, headers })
    }

    /// The `Authorization` answer to a 401's challenge.
    fn authorization(&self, challenge: &Reply, method: &str, url: &str, password: &str) -> String {
        use ttpkit_auth::digest::DigestChallenge;
        let header = challenge.header("www-authenticate").unwrap().to_owned();
        let value = msf_rtsp::header::HeaderFieldValue::from(header);
        let parsed = ttpkit_auth::AuthChallenge::parse(&value).unwrap();
        let digest = DigestChallenge::try_from(&parsed[0]).unwrap();
        digest
            .response_builder()
            .build(method, url, None, USER, password)
            .unwrap()
            .to_string()
    }
}
