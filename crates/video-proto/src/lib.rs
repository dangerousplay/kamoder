//! Protocol between the video producer (the `camera` service) and its
//! consumers (`rtsp`, `onvif`, `web`), and nothing else.
//!
//! The producer owns the image sensors and the hardware encoder; every other
//! service only reads what it publishes. This crate holds the wire formats,
//! the defaults and the naming rules both sides must agree on, and no
//! capture or encoder code, so a consumer never links a hardware backend
//! (the same split as `ptz-proto` and `lights-proto`). It depends on `serde`
//! only. This is the PRO-01 contract of the board-agnostic design.
//!
//! # Video socket
//!
//! A Unix stream socket, [`DEFAULT_SOCKET`]. One connection carries one
//! sensor's H.264. The client sends the sensor index in decimal and `\n`
//! ([`subscribe_line`]); the producer then writes, until either side closes,
//! one record per access unit:
//!
//! ```text
//! -> "0\n"
//! <- [len: u32 BE][flags: u8][timestamp_ms: u64 BE][len bytes of Annex-B H.264]
//! <- [len: u32 BE][flags: u8][timestamp_ms: u64 BE][len bytes of Annex-B H.264]
//! <- ...
//!
//!    byte  0..4   len           payload length in bytes, 1..=MAX_PACKET_LEN
//!    byte  4      flags         bit 0 = IDR (keyframe); bits 1..7 are 0
//!    byte  5..13  timestamp_ms  capture time in ms (monotonic, arbitrary zero)
//! ```
//!
//! The header is [`HEADER_LEN`] (13) bytes, [`PacketHeader`]. The payload is
//! Annex-B with start codes, one complete access unit. The first record sent is always an IDR: subscribing asks the
//! encoder for one, and a client that falls behind is skipped forward to the
//! next IDR rather than handed a broken GOP. The producer does not answer an
//! unknown index; it closes the connection. Timestamps only have meaning
//! relative to each other; a consumer takes the first one as its zero.
//!
//! The producer restarting is normal (the supervisor restarts it after a
//! crash): consumers reconnect and resubscribe instead of exiting.
//!
//! # HTTP
//!
//! The producer also serves plain HTTP on loopback, [`DEFAULT_PRODUCER`]:
//!
//! ```text
//! GET /status                    JSON array of Status, one per sensor
//! GET /snapshot.jpg[?camera=N]   image/jpeg, 503 while that sensor has no picture
//! ```
//!
//! `/status` is [`Status`] serialised with serde_json. Every field is always
//! present; `Option`s are `null`, never omitted:
//!
//! ```text
//! [{"index":0,"state":"running","width":640,"height":360,"format":"NV12",
//!   "encoding":"JPEG snapshots (software)","frames":1234,"fps":15.0,
//!   "sensor_width":1920,"sensor_height":1080,"sensor_id":"0xcb3e",
//!   "tuning":"isp_sc2331.conf",
//!   "stream":{"codec":"H.264","profile":"Main","width":1280,"height":720,
//!             "fps":15,"gop":30,"target_kbps":2048,"kbps":1987.5,
//!             "encoder":"..."},
//!   "error":null}]
//! ```
//!
//! # RTSP naming
//!
//! The `rtsp` service mounts sensor N at [`stream_path`]`(N)` = `vsN` on
//! [`DEFAULT_RTSP_PORT`], so `rtsp://<host>/vs0`. `onvif` and the web UI
//! build their URLs from these instead of repeating the literals.

use serde::{Deserialize, Serialize};

/// Path of the producer's video socket.
pub const DEFAULT_SOCKET: &str = "/tmp/kamoder-video.sock";

/// `host:port` of the producer's loopback HTTP server (`/status`,
/// `/snapshot.jpg`).
pub const DEFAULT_PRODUCER: &str = "127.0.0.1:3000";

/// Port the `rtsp` service listens on.
pub const DEFAULT_RTSP_PORT: u16 = 554;

/// Size of a [`PacketHeader`] on the wire, bytes.
pub const HEADER_LEN: usize = 13;

/// Largest payload a consumer accepts, bytes. A 1080p IDR at the highest
/// bitrate the encoder allows is far below this; anything larger means the
/// stream is out of sync.
pub const MAX_PACKET_LEN: u32 = 4 << 20;

/// Bit 0 of the flags byte: the access unit is an IDR.
const FLAG_IDR: u8 = 1;

/// RTSP path of sensor `index`: `vs{index}`.
pub fn stream_path(index: u32) -> String {
    format!("vs{index}")
}

/// What a client sends on the video socket to receive sensor `index`.
pub fn subscribe_line(index: u32) -> String {
    format!("{index}\n")
}

/// The sensor index a client asked for, from its first line; surrounding
/// whitespace (including the `\n`) is ignored. `None` when it is not a
/// decimal number.
pub fn parse_subscribe(line: &str) -> Option<u32> {
    line.trim().parse().ok()
}

/// The header in front of every access unit on the video socket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacketHeader {
    /// Payload length, bytes.
    pub len: u32,
    /// The access unit is an IDR.
    pub keyframe: bool,
    /// Capture time, ms.
    pub timestamp_ms: u64,
}

impl PacketHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut head = [0u8; HEADER_LEN];
        head[0..4].copy_from_slice(&self.len.to_be_bytes());
        head[4] = if self.keyframe { FLAG_IDR } else { 0 };
        head[5..13].copy_from_slice(&self.timestamp_ms.to_be_bytes());
        head
    }

    /// Reads a header. Flag bits other than IDR are ignored; checking `len`
    /// against [`MAX_PACKET_LEN`] is the caller's job.
    pub fn decode(head: &[u8; HEADER_LEN]) -> PacketHeader {
        let [l0, l1, l2, l3, flags, t @ ..] = *head;
        PacketHeader {
            len: u32::from_be_bytes([l0, l1, l2, l3]),
            keyframe: flags & FLAG_IDR != 0,
            timestamp_ms: u64::from_be_bytes(t),
        }
    }
}

/// The H.264 stream of one sensor.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamInfo {
    /// `H.264`.
    pub codec: String,
    /// `Baseline`, `Main` or `High`.
    pub profile: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub gop: u32,
    pub target_kbps: u32,
    /// Measured over the last second or so.
    pub kbps: f32,
    /// Encoder name and version, as the backend reports it.
    pub encoder: String,
}

/// One sensor, as `GET /status` reports it.
///
/// Missing fields deserialise to their default so an older or newer
/// producer still parses; serialisation always writes every field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub index: u32,
    /// `starting`, `running`, `failed`.
    pub state: String,
    /// Preview (sub channel) size.
    pub width: u32,
    pub height: u32,
    /// Pixel format of the captured pictures.
    pub format: String,
    /// How snapshots are encoded.
    pub encoding: String,
    pub frames: u64,
    /// Measured over the last second or so.
    pub fps: f32,
    /// Sensor active area and chip id, once brought up.
    pub sensor_width: u32,
    pub sensor_height: u32,
    pub sensor_id: Option<String>,
    /// ISP tuning file in use.
    pub tuning: Option<String>,
    /// Hardware H.264, when encoding.
    pub stream: Option<StreamInfo>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        for h in [
            PacketHeader::default(),
            PacketHeader {
                len: 1,
                keyframe: true,
                timestamp_ms: 0,
            },
            PacketHeader {
                len: MAX_PACKET_LEN,
                keyframe: false,
                timestamp_ms: u64::MAX,
            },
            PacketHeader {
                len: 0x0102_0304,
                keyframe: true,
                timestamp_ms: 0x0506_0708_090a_0b0c,
            },
        ] {
            assert_eq!(PacketHeader::decode(&h.encode()), h);
        }
    }

    #[test]
    fn header_layout_is_big_endian() {
        let h = PacketHeader {
            len: 0x0102_0304,
            keyframe: true,
            timestamp_ms: 0x0506_0708_090a_0b0c,
        };
        assert_eq!(
            h.encode(),
            [1, 2, 3, 4, 1, 5, 6, 7, 8, 9, 10, 11, 12],
            "u32 BE len, flags, u64 BE timestamp"
        );
        assert_eq!(
            PacketHeader {
                keyframe: false,
                ..h
            }
            .encode()[4],
            0
        );
    }

    #[test]
    fn header_matches_camera_service_encoding() {
        // What `camera::service::video_client` writes today.
        let (len, key, ts) = (4242usize, true, 1_700_000_000_123u64);
        let mut head = [0u8; 13];
        head[0..4].copy_from_slice(&(len as u32).to_be_bytes());
        head[4] = u8::from(key);
        head[5..13].copy_from_slice(&ts.to_be_bytes());
        let h = PacketHeader {
            len: len as u32,
            keyframe: key,
            timestamp_ms: ts,
        };
        assert_eq!(h.encode(), head);
        assert_eq!(PacketHeader::decode(&head), h);
    }

    #[test]
    fn decode_ignores_other_flag_bits() {
        let mut head = PacketHeader {
            len: 7,
            keyframe: false,
            timestamp_ms: 9,
        }
        .encode();
        head[4] = 0xfe;
        assert!(!PacketHeader::decode(&head).keyframe);
        head[4] = 0xff;
        assert!(PacketHeader::decode(&head).keyframe);
    }

    #[test]
    fn subscribe_line_parses_back() {
        for i in [0, 1, 7, u32::MAX] {
            assert_eq!(parse_subscribe(&subscribe_line(i)), Some(i));
        }
        assert_eq!(subscribe_line(1), "1\n");
        assert_eq!(parse_subscribe(" 2 \r\n"), Some(2));
        assert_eq!(parse_subscribe(""), None);
        assert_eq!(parse_subscribe("\n"), None);
        assert_eq!(parse_subscribe("-1\n"), None);
        assert_eq!(parse_subscribe("cam0\n"), None);
        assert_eq!(parse_subscribe("4294967296\n"), None);
    }

    #[test]
    fn stream_naming_and_defaults() {
        assert_eq!(stream_path(0), "vs0");
        assert_eq!(stream_path(1), "vs1");
        assert_eq!(DEFAULT_SOCKET, "/tmp/kamoder-video.sock");
        assert_eq!(DEFAULT_PRODUCER, "127.0.0.1:3000");
        assert_eq!(DEFAULT_RTSP_PORT, 554);
        assert_eq!(HEADER_LEN, 13);
    }

    fn running() -> Status {
        Status {
            index: 0,
            state: "running".into(),
            width: 640,
            height: 360,
            format: "NV12".into(),
            encoding: "JPEG snapshots (software)".into(),
            frames: 1234,
            fps: 15.0,
            sensor_width: 1920,
            sensor_height: 1080,
            sensor_id: Some("0xcb3e".into()),
            tuning: Some("isp_sc2331.conf".into()),
            stream: Some(StreamInfo {
                codec: "H.264".into(),
                profile: "Main".into(),
                width: 1280,
                height: 720,
                fps: 15,
                gop: 30,
                target_kbps: 2048,
                kbps: 1987.5,
                encoder: "venc 1.0".into(),
            }),
            error: None,
        }
    }

    #[test]
    fn status_json_roundtrip() {
        let failed = Status {
            index: 1,
            state: "failed".into(),
            error: Some("no frame in 2 s".into()),
            ..Status::default()
        };
        let all = vec![running(), failed];
        let json = serde_json::to_string(&all).unwrap();
        let back: Vec<Status> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, all);
    }

    /// Byte-for-byte what the camera service's `/status` serialises today
    /// (its `Status`/`StreamInfo` use `&'static str` and no serde attributes,
    /// so field order is declaration order and `None` is `null`).
    #[test]
    fn status_json_golden() {
        const GOLDEN: &str = concat!(
            r#"{"index":0,"state":"running","width":640,"height":360,"format":"NV12","#,
            r#""encoding":"JPEG snapshots (software)","frames":1234,"fps":15.0,"#,
            r#""sensor_width":1920,"sensor_height":1080,"sensor_id":"0xcb3e","#,
            r#""tuning":"isp_sc2331.conf","stream":{"codec":"H.264","profile":"Main","#,
            r#""width":1280,"height":720,"fps":15,"gop":30,"target_kbps":2048,"#,
            r#""kbps":1987.5,"encoder":"venc 1.0"},"error":null}"#
        );
        assert_eq!(serde_json::to_string(&running()).unwrap(), GOLDEN);
        assert_eq!(
            serde_json::from_str::<Status>(GOLDEN).unwrap(),
            running(),
            "golden parses back"
        );

        // A sensor that has not come up: every field still present.
        const STARTING: &str = concat!(
            r#"{"index":1,"state":"starting","width":0,"height":0,"format":"NV12","#,
            r#""encoding":"JPEG snapshots (software)","frames":0,"fps":0.0,"#,
            r#""sensor_width":0,"sensor_height":0,"sensor_id":null,"tuning":null,"#,
            r#""stream":null,"error":null}"#
        );
        let starting = Status {
            index: 1,
            state: "starting".into(),
            format: "NV12".into(),
            encoding: "JPEG snapshots (software)".into(),
            ..Status::default()
        };
        assert_eq!(serde_json::to_string(&starting).unwrap(), STARTING);
    }

    /// Same shape as the struct `camera::service` serialises today; kept here
    /// so a field rename on either side fails this test until consumers move.
    #[test]
    fn status_json_matches_current_camera_struct() {
        #[derive(Serialize)]
        struct OldStream {
            codec: &'static str,
            profile: &'static str,
            width: u32,
            height: u32,
            fps: u32,
            gop: u32,
            target_kbps: u32,
            kbps: f32,
            encoder: String,
        }
        #[derive(Serialize)]
        struct OldStatus {
            index: u32,
            state: &'static str,
            width: u32,
            height: u32,
            format: &'static str,
            encoding: &'static str,
            frames: u64,
            fps: f32,
            sensor_width: u32,
            sensor_height: u32,
            sensor_id: Option<String>,
            tuning: Option<String>,
            stream: Option<OldStream>,
            error: Option<String>,
        }
        let old = OldStatus {
            index: 0,
            state: "running",
            width: 640,
            height: 360,
            format: "NV12",
            encoding: "JPEG snapshots (software)",
            frames: 1234,
            fps: 15.0,
            sensor_width: 1920,
            sensor_height: 1080,
            sensor_id: Some("0xcb3e".into()),
            tuning: Some("isp_sc2331.conf".into()),
            stream: Some(OldStream {
                codec: "H.264",
                profile: "Main",
                width: 1280,
                height: 720,
                fps: 15,
                gop: 30,
                target_kbps: 2048,
                kbps: 1987.5,
                encoder: "venc 1.0".into(),
            }),
            error: None,
        };
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            serde_json::to_string(&running()).unwrap()
        );
    }

    #[test]
    fn status_tolerates_missing_fields() {
        let s: Status = serde_json::from_str(r#"{"index":2,"state":"failed"}"#).unwrap();
        assert_eq!(s.index, 2);
        assert_eq!(s.state, "failed");
        assert_eq!(s.stream, None);
    }
}
