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
//!
//! Audio (AUD-10..13) comes from [`FakeAudioSource`], a fixed sawtooth in
//! 20 ms frames, so the PCMA payloads are known byte for byte; an
//! [`AudioStream`] on a socket nobody serves stands for the audio service
//! being down.

use bytes::Bytes;
use futures::StreamExt;
use retina::client::{
    Credentials, PlayOptions, Session, SessionOptions, SetupOptions, TcpTransportOptions,
    Transport, UdpTransportOptions,
};
use retina::codec::{CodecItem, FrameFormat, ParameterSetInsertion};
use rtsp::audio::{AudioStream, FakeAudioSource};
use rtsp::backchannel::Speaker;
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
    audio: Option<AudioStream>,
}

impl Server {
    async fn start(auth: bool, session_timeout: Duration) -> Self {
        Self::start_with_audio(auth, session_timeout, None).await
    }

    /// Both mounts with the fake microphone.
    async fn with_audio(auth: bool) -> Self {
        let audio = AudioStream::new(|| FakeAudioSource::new(1000));
        Self::start_with_audio(auth, Duration::from_secs(60), Some(audio)).await
    }

    async fn start_with_audio(
        auth: bool,
        session_timeout: Duration,
        audio: Option<AudioStream>,
    ) -> Self {
        Self::start_full(auth, session_timeout, audio, None).await
    }

    async fn start_full(
        auth: bool,
        session_timeout: Duration,
        audio: Option<AudioStream>,
        speaker: Option<Speaker>,
    ) -> Self {
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
                audio: audio.clone(),
                speaker: speaker.clone(),
            },
            Mount {
                path: "/vs1".to_owned(),
                stream: second,
                audio: audio.clone(),
                speaker,
            },
        ];
        tokio::spawn(server::serve(listener, mounts, config));
        Self { addr, audio }
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

    assert_eq!(session.streams().len(), 1, "no audio track without audio");
    assert!(!sdp.contains("m=audio"), "{sdp}");
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

/// What a client got from a video + audio session.
struct AudioVisual {
    video: Vec<(Vec<Bytes>, bool)>,
    /// RTP timestamp (elapsed on the 8 kHz clock) and payload of each audio
    /// frame.
    audio: Vec<(i64, Vec<u8>)>,
}

/// DESCRIBE, SETUP of video and (if `with_audio`) audio — audio first, to
/// show the order does not matter — PLAY, then receive until `video` and
/// `audio` frames arrived, or for `limit` if that comes first.
async fn receive_av(
    server: &Server,
    transport: Transport,
    with_audio: bool,
    video: usize,
    audio: usize,
    limit: Duration,
) -> AudioVisual {
    let options = SessionOptions::default().creds(creds(PASSWORD));
    let mut session = Session::describe(server.url(), options).await.unwrap();
    let format = FrameFormat {
        parameter_set_insertion: ParameterSetInsertion::Never,
        ..FrameFormat::SIMPLE
    };
    if with_audio {
        assert_eq!(session.streams()[1].media(), "audio");
        session
            .setup(1, SetupOptions::default().transport(transport.clone()))
            .await
            .unwrap();
    }
    session
        .setup(
            0,
            SetupOptions::default()
                .transport(transport)
                .frame_format(format),
        )
        .await
        .unwrap();
    let mut items = session
        .play(PlayOptions::default())
        .await
        .unwrap()
        .demuxed()
        .unwrap();
    let mut got = AudioVisual {
        video: Vec::new(),
        audio: Vec::new(),
    };
    let deadline = tokio::time::Instant::now() + limit;
    while got.video.len() < video || got.audio.len() < audio {
        let Ok(item) = tokio::time::timeout_at(deadline, items.next()).await else {
            break;
        };
        match item.expect("stream ended").expect("stream error") {
            CodecItem::VideoFrame(frame) => {
                assert_eq!(frame.loss(), 0, "packet loss on loopback");
                let data = Bytes::copy_from_slice(frame.data());
                let nals = nal_units(&data)
                    .filter(|nal| !matches!(nal_type(nal), NAL_SPS | NAL_PPS))
                    .collect();
                got.video.push((nals, frame.is_random_access_point()));
            }
            CodecItem::AudioFrame(frame) => {
                assert_eq!(frame.loss(), 0, "audio packet loss on loopback");
                assert_eq!(frame.stream_id(), 1);
                got.audio
                    .push((frame.timestamp().elapsed(), frame.data().to_vec()));
            }
            _ => {}
        }
    }
    got
}

/// The audio is the fake source's sawtooth in A-law, 160 samples a packet,
/// RTP time advancing by exactly 160 (the capture clock), from 0.
fn assert_fake_pcma(audio: &[(i64, Vec<u8>)]) {
    let want: Vec<u8> = (0..160)
        .map(|n| audio_proto::g711::alaw_encode(FakeAudioSource::sample(n)))
        .collect();
    for (index, (timestamp, data)) in audio.iter().enumerate() {
        assert_eq!(
            *timestamp,
            index as i64 * 160,
            "timestamp of packet {index}"
        );
        assert_eq!(data, &want, "payload of packet {index}");
    }
}

#[tokio::test]
async fn describe_announces_the_pcma_track_when_there_is_audio() {
    let server = Server::with_audio(true).await;
    let options = SessionOptions::default().creds(creds(PASSWORD));
    let session = Session::describe(server.url(), options).await.unwrap();
    let sdp = std::str::from_utf8(session.sdp()).unwrap().to_owned();
    assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"), "{sdp}");
    assert!(sdp.contains("m=audio 0 RTP/AVP 8\r\n"), "{sdp}");
    assert!(sdp.contains("a=rtpmap:8 PCMA/8000\r\n"), "{sdp}");
    assert_eq!(session.streams().len(), 2);
    let audio = &session.streams()[1];
    assert_eq!(audio.media(), "audio");
    assert_eq!(audio.encoding_name(), "pcma");
    assert_eq!(audio.clock_rate_hz(), 8000);
    // No channel count in the rtpmap: mono by default (RFC 4566 § 6).
    assert_eq!(audio.channels(), None);
    assert_eq!(
        audio.control().map(|url| url.as_str().to_owned()),
        Some(format!("rtsp://{}/vs0/trackID=1", server.addr))
    );
    // DESCRIBE alone captures nothing (D-A7).
    assert!(!server.audio.as_ref().unwrap().capturing());
}

#[tokio::test]
async fn plays_video_and_audio_over_tcp_interleaved() {
    let server = Server::with_audio(true).await;
    let transport = Transport::Tcp(TcpTransportOptions::default());
    let got = receive_av(&server, transport, true, 40, 50, Duration::from_secs(10)).await;
    assert_matches_clip(&got.video[..40]);
    assert!(got.audio.len() >= 50, "{} audio frames", got.audio.len());
    assert_fake_pcma(&got.audio);
}

#[tokio::test]
async fn plays_video_and_audio_over_udp() {
    let server = Server::with_audio(true).await;
    let transport = Transport::Udp(UdpTransportOptions::default());
    let got = receive_av(&server, transport, true, 40, 50, Duration::from_secs(10)).await;
    assert_matches_clip(&got.video[..40]);
    assert!(got.audio.len() >= 50, "{} audio frames", got.audio.len());
    assert_fake_pcma(&got.audio);
}

#[tokio::test]
async fn video_only_clients_are_unchanged_and_capture_nothing() {
    let server = Server::with_audio(true).await;
    let transport = Transport::Tcp(TcpTransportOptions::default());
    let got = receive_av(&server, transport, false, 40, 0, Duration::from_secs(10)).await;
    assert_matches_clip(&got.video);
    assert!(got.audio.is_empty());
    assert!(
        !server.audio.as_ref().unwrap().capturing(),
        "no audio session, no capture"
    );
}

#[tokio::test]
async fn video_keeps_playing_while_the_audio_service_is_down() {
    let socket = std::env::temp_dir().join(format!("rtsp-no-audio-{}.sock", std::process::id()));
    let audio = AudioStream::socket(socket);
    let server = Server::start_with_audio(true, Duration::from_secs(60), Some(audio)).await;
    let transport = Transport::Udp(UdpTransportOptions::default());
    let got = receive_av(&server, transport, true, 40, 1, Duration::from_secs(4)).await;
    assert!(got.video.len() >= 40, "{} video frames", got.video.len());
    assert_matches_clip(&got.video);
    assert!(got.audio.is_empty(), "the audio track stays silent");
}

#[tokio::test]
async fn tracks_set_up_in_any_order_on_one_session() {
    let server = Server::with_audio(false).await;
    let audio = server.audio.clone().unwrap();
    let mut raw = RawClient::connect(server.addr).await;
    let url = format!("rtsp://{}/vs0", server.addr);
    assert_eq!(raw.request("DESCRIBE", &url, &[]).await.status, 200);

    // Audio first, on interleaved channels of its own choosing.
    let first = raw
        .request(
            "SETUP",
            &format!("{url}/trackID=1"),
            &[("Transport", "RTP/AVP/TCP;unicast;interleaved=2-3")],
        )
        .await;
    assert_eq!(first.status, 200);
    assert_eq!(
        first.header("transport"),
        Some("RTP/AVP/TCP;unicast;interleaved=2-3")
    );
    let session = first.session();
    // The same track again is refused; so is the audio of another mount.
    let again = raw
        .request(
            "SETUP",
            &format!("{url}/trackID=1"),
            &[
                ("Transport", "RTP/AVP/TCP;unicast;interleaved=4-5"),
                ("Session", &session),
            ],
        )
        .await;
    assert_eq!(again.status, 455);
    let other = format!("rtsp://{}/vs1/trackID=0", server.addr);
    let elsewhere = raw
        .request(
            "SETUP",
            &other,
            &[
                ("Transport", "RTP/AVP/TCP;unicast;interleaved=4-5"),
                ("Session", &session),
            ],
        )
        .await;
    assert_eq!(elsewhere.status, 455);
    // Then video, joining the session.
    let second = raw
        .request(
            "SETUP",
            &format!("{url}/trackID=0"),
            &[
                ("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1"),
                ("Session", &session),
            ],
        )
        .await;
    assert_eq!(second.status, 200);
    assert_eq!(second.session(), session);
    assert!(!audio.capturing(), "set up is not playing");

    let play = raw.request("PLAY", &url, &[("Session", &session)]).await;
    assert_eq!(play.status, 200);
    let rtp_info = play.header("rtp-info").unwrap();
    let entries: Vec<&str> = rtp_info.split(',').collect();
    assert_eq!(entries.len(), 2, "{rtp_info}");
    assert!(
        entries[0].starts_with(&format!("url={url}/trackID=0;seq=")),
        "{rtp_info}"
    );
    assert!(
        entries[1].starts_with(&format!("url={url}/trackID=1;seq=")),
        "{rtp_info}"
    );
    assert!(
        entries.iter().all(|e| e.contains(";rtptime=")),
        "{rtp_info}"
    );
    assert!(audio.capturing());

    let channels = raw.channels_for(Duration::from_millis(1500)).await;
    assert!(channels[0] > 10, "video RTP: {channels:?}");
    assert!(channels[2] > 30, "audio RTP (20 ms packets): {channels:?}");

    // Too late to add a track once playing.
    let late = raw
        .request(
            "SETUP",
            &format!("{url}/trackID=0"),
            &[
                ("Transport", "RTP/AVP/TCP;unicast;interleaved=6-7"),
                ("Session", &session),
            ],
        )
        .await;
    assert_eq!(late.status, 455);

    let teardown = raw
        .request("TEARDOWN", &url, &[("Session", &session)])
        .await;
    assert_eq!(teardown.status, 200);
    raw.interleaved_for(Duration::from_millis(300)).await;
    assert_eq!(raw.interleaved_for(Duration::from_millis(500)).await, 0);
    assert!(
        !audio.capturing(),
        "capture ends with the last audio session"
    );
}

#[tokio::test]
async fn audio_track_absent_without_audio() {
    let server = Server::start(false, Duration::from_secs(60)).await;
    let mut raw = RawClient::connect(server.addr).await;
    let url = format!("rtsp://{}/vs0/trackID=1", server.addr);
    let setup = raw
        .request(
            "SETUP",
            &url,
            &[("Transport", "RTP/AVP/TCP;unicast;interleaved=2-3")],
        )
        .await;
    assert_eq!(setup.status, 404);
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

/// The ONVIF backchannel feature tag.
const BACKCHANNEL: &str = "www.onvif.org/ver20/backchannel";

/// What the fake audio service saw.
#[derive(Debug)]
enum Heard {
    /// A `play` handshake from this talker, answered `busy` or not.
    Play {
        talker: String,
        busy: bool,
    },
    Frame(audio_proto::Frame),
    /// The talker closed its connection: the speaker is free again.
    Released,
}

static NEXT_SOCKET: AtomicU16 = AtomicU16::new(0);

/// A fake audio service answering `play`: the first `busy` requests are
/// refused `busy`, later ones stream; every frame is reported.
fn fake_speaker(busy: usize) -> (Speaker, tokio::sync::mpsc::UnboundedReceiver<Heard>) {
    use tokio::io::AsyncBufReadExt;
    let path = std::env::temp_dir().join(format!(
        "rtsp-speaker-{}-{}.sock",
        std::process::id(),
        NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut refused = 0;
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert!(line.contains(r#""op":"play""#), "{line}");
            let talker = line
                .split(r#""client":""#)
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_default()
                .to_owned();
            let is_busy = refused < busy;
            let _ = tx.send(Heard::Play {
                talker,
                busy: is_busy,
            });
            if is_busy {
                refused += 1;
                let answer = "{\"result\":\"error\",\"code\":\"busy\",\"message\":\"speaker held by web\"}\n";
                reader.get_mut().write_all(answer.as_bytes()).await.unwrap();
                continue;
            }
            let answer =
                "{\"result\":\"streaming\",\"rate\":8000,\"channels\":1,\"frame_samples\":160}\n";
            reader.get_mut().write_all(answer.as_bytes()).await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                loop {
                    let mut head = [0u8; video_proto::HEADER_LEN];
                    if reader.read_exact(&mut head).await.is_err() {
                        let _ = tx.send(Heard::Released);
                        return;
                    }
                    let header = video_proto::PacketHeader::decode(&head);
                    let mut payload = vec![0u8; header.len as usize];
                    reader.read_exact(&mut payload).await.unwrap();
                    let frame = audio_proto::Frame::from_parts(&header, &payload).unwrap();
                    let _ = tx.send(Heard::Frame(frame));
                }
            });
        }
    });
    (Speaker::socket(path), rx)
}

/// One RTP packet: version 2, `pt`, `seq`, `timestamp`, a fixed SSRC.
fn rtp(pt: u8, seq: u16, timestamp: u32, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![0x80, pt];
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&0x1234_5678u32.to_be_bytes());
    packet.extend_from_slice(payload);
    packet
}

/// 20 ms of a tone-ish ramp, distinct per packet `n`, as linear samples.
fn talk_samples(n: usize) -> Vec<i16> {
    (0..160)
        .map(|i| ((i as i16 - 80) * 300).wrapping_add(n as i16 * 37))
        .collect()
}

/// Waits for the next event from the fake service.
async fn heard(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Heard>) -> Heard {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the audio service heard nothing within 5 s")
        .expect("fake audio service gone")
}

#[tokio::test]
async fn backchannel_is_offered_only_on_require() {
    let (speaker, _rx) = fake_speaker(0);
    let audio = AudioStream::new(|| FakeAudioSource::new(1000));
    let server =
        Server::start_full(false, Duration::from_secs(60), Some(audio), Some(speaker)).await;
    let mut raw = RawClient::connect(server.addr).await;
    let url = format!("rtsp://{}/vs0", server.addr);

    // An ordinary client: video and microphone, no backchannel, no
    // direction attributes.
    let plain = raw.request("DESCRIBE", &url, &[]).await;
    assert_eq!(plain.status, 200);
    assert!(
        plain.body.contains("a=control:trackID=1\r\n"),
        "{}",
        plain.body
    );
    assert!(!plain.body.contains("trackID=2"), "{}", plain.body);
    assert!(!plain.body.contains("sendonly"), "{}", plain.body);

    // An ONVIF talk-back client.
    let onvif = raw
        .request("DESCRIBE", &url, &[("Require", BACKCHANNEL)])
        .await;
    assert_eq!(onvif.status, 200);
    let sdp = &onvif.body;
    let back = sdp
        .find("a=control:trackID=2\r\n")
        .expect("backchannel section");
    let section = &sdp[sdp[..back].rfind("m=").unwrap()..];
    assert!(section.starts_with("m=audio 0 RTP/AVP 8\r\n"), "{sdp}");
    assert!(section.contains("a=rtpmap:8 PCMA/8000\r\n"), "{sdp}");
    assert!(section.contains("a=sendonly\r\n"), "{sdp}");
    assert_eq!(sdp.matches("a=recvonly\r\n").count(), 2, "{sdp}");

    // Unknown tags: 551 with Unsupported naming them, on any method.
    let unknown = raw
        .request(
            "DESCRIBE",
            &url,
            &[("Require", &format!("{BACKCHANNEL}, com.example.zoom"))],
        )
        .await;
    assert_eq!(unknown.status, 551);
    assert_eq!(unknown.header("unsupported"), Some("com.example.zoom"));
    let options = raw
        .request("OPTIONS", &url, &[("Require", "com.example.zoom")])
        .await;
    assert_eq!(options.status, 551);
    let options = raw
        .request("OPTIONS", &url, &[("Require", BACKCHANNEL)])
        .await;
    assert_eq!(options.status, 200);
}

#[tokio::test]
async fn backchannel_is_refused_without_a_speaker() {
    let server = Server::with_audio(false).await;
    let mut raw = RawClient::connect(server.addr).await;
    let url = format!("rtsp://{}/vs0", server.addr);
    let describe = raw
        .request("DESCRIBE", &url, &[("Require", BACKCHANNEL)])
        .await;
    assert_eq!(describe.status, 551);
    assert_eq!(describe.header("unsupported"), Some(BACKCHANNEL));
    let setup = raw
        .request(
            "SETUP",
            &format!("{url}/trackID=2"),
            &[("Transport", "RTP/AVP/TCP;unicast;interleaved=4-5")],
        )
        .await;
    assert_eq!(setup.status, 404);
}

#[tokio::test]
async fn backchannel_plays_pcma_from_tcp_interleaved() {
    let (speaker, mut rx) = fake_speaker(0);
    let server = Server::start_full(false, Duration::from_secs(60), None, Some(speaker)).await;
    let mut raw = RawClient::connect(server.addr).await;
    let url = format!("rtsp://{}/vs0", server.addr);
    let require = ("Require", BACKCHANNEL);
    assert_eq!(raw.request("DESCRIBE", &url, &[require]).await.status, 200);
    let video = raw
        .request(
            "SETUP",
            &format!("{url}/trackID=0"),
            &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
        )
        .await;
    let session = video.session();
    let back = raw
        .request(
            "SETUP",
            &format!("{url}/trackID=2"),
            &[
                ("Transport", "RTP/AVP/TCP;unicast;interleaved=4-5"),
                ("Session", &session),
                require,
            ],
        )
        .await;
    assert_eq!(back.status, 200);
    assert_eq!(
        back.header("transport"),
        Some("RTP/AVP/TCP;unicast;interleaved=4-5")
    );
    let play = raw
        .request("PLAY", &url, &[("Session", &session), require])
        .await;
    assert_eq!(play.status, 200);
    // RTP-Info only for what the camera sends.
    let rtp_info = play.header("rtp-info").unwrap();
    assert!(!rtp_info.contains("trackID=2"), "{rtp_info}");
    assert!(
        rx.try_recv().is_err(),
        "the speaker is taken on the first packet, not at PLAY"
    );

    // Ten 20 ms packets; a duplicate and a PT 96 packet are dropped.
    let mut want = Vec::new();
    for n in 0..10u16 {
        let samples = talk_samples(n.into());
        let codes: Vec<u8> = samples
            .iter()
            .map(|&s| audio_proto::g711::alaw_encode(s))
            .collect();
        want.extend(codes.iter().map(|&c| audio_proto::g711::alaw_decode(c)));
        let packet = rtp(8, 1000 + n, 5000 + u32::from(n) * 160, &codes);
        raw.send_interleaved(4, &packet).await;
        if n == 3 {
            raw.send_interleaved(4, &packet).await;
            // Also a sequence jump that would hang msf-rtp unguarded.
            raw.send_interleaved(4, &rtp(96, 2000, 0, &[0; 160])).await;
        }
    }
    match heard(&mut rx).await {
        Heard::Play { talker, busy } => {
            assert_eq!(talker, "onvif");
            assert!(!busy);
        }
        other => panic!("{other:?}"),
    }
    let mut got = Vec::new();
    while got.len() < want.len() {
        match heard(&mut rx).await {
            Heard::Frame(frame) => {
                assert_eq!(frame.samples.len(), 160, "20 ms frames");
                got.extend(frame.samples);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(got, want);

    // TEARDOWN releases the speaker.
    let teardown = raw
        .request("TEARDOWN", &url, &[("Session", &session), require])
        .await;
    assert_eq!(teardown.status, 200);
    assert!(matches!(heard(&mut rx).await, Heard::Released));
}

#[tokio::test]
async fn backchannel_plays_pcmu_from_udp() {
    let (speaker, mut rx) = fake_speaker(0);
    let server = Server::start_full(false, Duration::from_secs(60), None, Some(speaker)).await;
    let mut raw = RawClient::connect(server.addr).await;
    let url = format!("rtsp://{}/vs0", server.addr);
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = socket.local_addr().unwrap().port();
    let setup = raw
        .request(
            "SETUP",
            &format!("{url}/trackID=2"),
            &[(
                "Transport",
                &format!("RTP/AVP;unicast;client_port={port}-{}", port + 1),
            )],
        )
        .await;
    assert_eq!(setup.status, 200, "lenient: no Require on SETUP");
    let transport = setup.header("transport").unwrap().to_owned();
    let server_port: u16 = transport
        .split("server_port=")
        .nth(1)
        .and_then(|ports| ports.split('-').next())
        .and_then(|port| port.parse().ok())
        .expect("server_port");
    let session = setup.session();
    assert_eq!(
        raw.request("PLAY", &url, &[("Session", &session)])
            .await
            .status,
        200
    );
    let samples = talk_samples(7);
    let codes: Vec<u8> = samples
        .iter()
        .map(|&s| audio_proto::g711::ulaw_encode(s))
        .collect();
    let want: Vec<i16> = codes
        .iter()
        .map(|&c| audio_proto::g711::ulaw_decode(c))
        .collect();
    socket
        .send_to(&rtp(0, 1, 0, &codes), ("127.0.0.1", server_port))
        .await
        .unwrap();
    assert!(matches!(
        heard(&mut rx).await,
        Heard::Play { busy: false, .. }
    ));
    match heard(&mut rx).await {
        Heard::Frame(frame) => assert_eq!(frame.samples, want),
        other => panic!("{other:?}"),
    }
    // Silence: the speaker goes back after the idle time.
    assert!(matches!(heard(&mut rx).await, Heard::Released));
}

/// Forged RTP towards a sending track (a sequence jump msf-rtp's reorder
/// buffer cannot absorb) must not hang the server.
#[tokio::test]
async fn client_rtp_with_a_sequence_jump_does_not_hang_the_server() {
    let server = Server::start(false, Duration::from_secs(60)).await;
    let mut raw = RawClient::connect(server.addr).await;
    let url = format!("rtsp://{}/vs0", server.addr);
    let setup = raw
        .request(
            "SETUP",
            &url,
            &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
        )
        .await;
    let session = setup.session();
    assert_eq!(
        raw.request("PLAY", &url, &[("Session", &session)])
            .await
            .status,
        200
    );
    raw.send_interleaved(0, &rtp(96, 1, 0, &[0; 10])).await;
    raw.send_interleaved(0, &rtp(96, 20_000, 0, &[0; 10])).await;
    raw.send_interleaved(0, &rtp(96, 20_001, 0, &[0; 10])).await;
    let alive = tokio::time::timeout(
        Duration::from_secs(5),
        raw.request("GET_PARAMETER", &url, &[("Session", &session)]),
    )
    .await
    .expect("the server hung");
    assert_eq!(alive.status, 200);
    assert!(raw.interleaved_for(Duration::from_millis(500)).await > 0);
}

#[tokio::test]
async fn busy_speaker_drops_backchannel_audio_until_free() {
    // The web UI holds the speaker for the first request.
    let (speaker, mut rx) = fake_speaker(1);
    let server = Server::start_full(false, Duration::from_secs(60), None, Some(speaker)).await;
    let mut raw = RawClient::connect(server.addr).await;
    let url = format!("rtsp://{}/vs0", server.addr);
    let setup = raw
        .request(
            "SETUP",
            &format!("{url}/trackID=2"),
            &[("Transport", "RTP/AVP/TCP;unicast")],
        )
        .await;
    assert_eq!(setup.status, 200);
    let channel: u8 = setup
        .header("transport")
        .and_then(|t| t.split("interleaved=").nth(1))
        .and_then(|pair| pair.split('-').next())
        .and_then(|id| id.parse().ok())
        .expect("interleaved channel");
    let session = setup.session();
    // PLAY succeeds although the speaker is busy.
    assert_eq!(
        raw.request("PLAY", &url, &[("Session", &session)])
            .await
            .status,
        200
    );
    // Talk for 1.6 s in real time: refused, dropped, retried after 1 s.
    let mut seq = 0u16;
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    let mut events = Vec::new();
    let mut first_heard = None;
    for _ in 0..80 {
        ticker.tick().await;
        let codes = vec![audio_proto::g711::alaw_encode(1000); 160];
        raw.send_interleaved(channel, &rtp(8, seq, u32::from(seq) * 160, &codes))
            .await;
        seq += 1;
        while let Ok(event) = rx.try_recv() {
            if first_heard.is_none() && matches!(event, Heard::Frame(_)) {
                first_heard = Some(seq);
            }
            events.push(event);
        }
    }
    assert!(
        matches!(events.first(), Some(Heard::Play { busy: true, .. })),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Heard::Play { busy: false, .. })),
        "asked again once the retry time passed: {events:?}"
    );
    let first_heard = first_heard.expect("audio played once the speaker was free");
    assert!(
        first_heard >= 45,
        "packets during the busy second were dropped (first heard after packet {first_heard})"
    );
    // The video session of another client is unaffected by any of this.
    let other = raw.request("DESCRIBE", &url, &[]).await;
    assert_eq!(other.status, 200);
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
    body: String,
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

    /// Reads for `period` and counts the interleaved frames per channel.
    async fn channels_for(&mut self, period: Duration) -> [usize; 256] {
        let deadline = tokio::time::Instant::now() + period;
        let mut counts = [0; 256];
        loop {
            while let Some(channel) = self.next_channel() {
                counts[usize::from(channel)] += 1;
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() || self.fill(left).await.is_none() {
                return counts;
            }
        }
    }

    /// Sends `data` on interleaved `channel`.
    async fn send_interleaved(&mut self, channel: u8, data: &[u8]) {
        let mut frame = vec![b'$', channel];
        frame.extend_from_slice(&(data.len() as u16).to_be_bytes());
        frame.extend_from_slice(data);
        self.socket.write_all(&frame).await.unwrap();
    }

    /// Drops one complete `$`-frame from the front of the buffer.
    fn skip_interleaved(&mut self) -> bool {
        self.next_channel().is_some()
    }

    /// Drops one complete `$`-frame from the front of the buffer and
    /// returns its channel.
    fn next_channel(&mut self) -> Option<u8> {
        if self.buffer.len() < 4 || self.buffer[0] != b'$' {
            return None;
        }
        let channel = self.buffer[1];
        let length = 4 + usize::from(u16::from_be_bytes([self.buffer[2], self.buffer[3]]));
        if self.buffer.len() < length {
            return None;
        }
        self.buffer.drain(..length);
        Some(channel)
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
        let length = body;
        let body = String::from_utf8_lossy(&self.buffer[end..end + length]).into_owned();
        self.buffer.drain(..end + length);
        Some(Reply {
            status,
            headers,
            body,
        })
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
