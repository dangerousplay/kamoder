# The video producer protocol

The contract between the video **producer** (the `camera` service, binary
`bin/camera` from the [`camerad`](../../crates/camerad/src/main.rs) package) and
its **consumers** (`rtsp`, `onvif`, `web`). Any producer that speaks it can
replace the camera service; any consumer that speaks it works on every board.

The authoritative definition is the
[`video-proto`](../../crates/video-proto/src/lib.rs) crate: the constants,
the header codec, the subscribe line and the `/status` types all live there,
and both sides use them instead of repeating literals. This note restates it
for people who are not reading Rust. If the two ever disagree, the crate wins
and this note is the bug.

`video-proto` depends on `serde` only, so a consumer never links capture or
encoder code (the same split as `ptz-proto` and `lights-proto`).

## Endpoints

| What | Where | Constant |
| --- | --- | --- |
| Video socket (H.264) | Unix stream socket `/tmp/kamoder-video.sock` | `DEFAULT_SOCKET` |
| HTTP (`/status`, `/snapshot.jpg`) | `127.0.0.1:3000`, loopback only | `DEFAULT_PRODUCER` |
| RTSP (served by `rtsp`, not the producer) | TCP `554` | `DEFAULT_RTSP_PORT` |

The producer's `--listen` and `--video-socket` flags, and the consumers'
matching flags (`rtsp --video-socket`, `onvif --producer`, `web
--snapshot-url`), move these; the defaults above are what `tools/supervisor.toml`
uses.

## Video socket

One connection carries one sensor's H.264. The client writes the sensor
index in decimal followed by `\n` (`subscribe_line(N)`); the producer then
writes one record per access unit until either side closes:

```text
-> "0\n"
<- [len: u32 BE][flags: u8][timestamp_ms: u64 BE][len bytes of Annex-B H.264]
<- [len: u32 BE][flags: u8][timestamp_ms: u64 BE][len bytes of Annex-B H.264]
<- ...

   byte  0..4   len           payload length in bytes, 1..=MAX_PACKET_LEN
   byte  4      flags         bit 0 = IDR (keyframe); bits 1..7 are 0
   byte  5..13  timestamp_ms  capture time in ms (monotonic, arbitrary zero)
```

- **Header.** `HEADER_LEN` = 13 bytes, `PacketHeader::{encode, decode}`.
  Decoders ignore flag bits other than bit 0. Checking `len` against
  `MAX_PACKET_LEN` (4 MiB) is the consumer's job; a larger value means the
  stream is out of sync and the consumer should drop the connection.
- **Payload.** One complete access unit, Annex-B with start codes. SPS/PPS
  come in-band with IDRs as the encoder emits them; `rtsp` caches them for
  `sprop-parameter-sets` and re-inserts them in front of any IDR that lacks
  them.
- **Timestamps** only have meaning relative to each other. A consumer takes
  the first one it sees as its zero.
- **Unknown index.** The producer does not answer; it closes the connection.
  An index is unknown when no camera the producer serves has that board
  sensor number, whatever the number of cameras (see "Which index is
  which").
  (The subscribe line is parsed with `parse_subscribe`: surrounding
  whitespace is ignored, anything that is not a decimal number is unknown.)

### IDR sync on subscribe

The first record a client receives is always an IDR. Subscribing sets an
"IDR wanted" flag on that sensor; the capture thread asks the encoder
(`Encoder::request_idr`) for one on the next picture, and until it arrives the
client's connection skips every non-IDR access unit. A client therefore
never starts mid-GOP, and joining costs at most one frame interval plus the
encode time rather than a full GOP.

### Lag behaviour

The producer fans out each access unit through a bounded broadcast queue of
`VIDEO_BUFFER` (16) access units per subscriber (`crates/camera/src/service.rs`).
A client that falls more than that far behind is not allowed to grow a queue
on a 32 MB camera:

1. its missed access units are dropped;
2. an IDR is requested from the encoder again;
3. the client is skipped forward to that next IDR, exactly as on subscribe.

The client sees a gap in timestamps and a fresh IDR, never a broken GOP.
Other subscribers of the same sensor are unaffected. Encoded output is only
copied out of the capture thread while at least one client is subscribed to
that sensor.

### Producer restarts

The supervisor restarts the producer after a crash, so the socket going away
is normal. Consumers reconnect and resubscribe instead of exiting: `rtsp`
retries every second, its RTSP clients see a stall rather than a closed
session, and the RTP clock keeps its zero across the reconnect.

## HTTP

```text
GET /status                    JSON array of Status, one per sensor
GET /snapshot.jpg[?camera=N]   image/jpeg, 503 while that sensor has no picture
```

### `GET /status`

A JSON array with one `Status` object per sensor the producer serves, in
order. Every field is always present; `Option`s are serialised as `null`,
never omitted. Readers deserialise with `#[serde(default)]`, so an older or
newer producer with fewer or extra fields still parses.

```json
[
  {
    "index": 0,
    "state": "running",
    "width": 1280,
    "height": 720,
    "format": "NV12",
    "encoding": "JPEG snapshots (software)",
    "frames": 1234,
    "fps": 15.0,
    "sensor_width": 1920,
    "sensor_height": 1080,
    "sensor_id": "0xcb3e",
    "tuning": "isp_sc2331_mipi_2lane_mipi0.conf",
    "stream": {
      "codec": "H.264",
      "profile": "Main",
      "width": 1280,
      "height": 720,
      "fps": 15,
      "gop": 30,
      "target_kbps": 2048,
      "kbps": 1987.5,
      "encoder": "..."
    },
    "error": null
  }
]
```

| Field | Type | Meaning |
| --- | --- | --- |
| `index` | u32 | Board-level sensor number (the one in `video.sensors`) |
| `state` | string | `starting`, `running` or `failed` |
| `width`, `height` | u32 | Size of the captured pictures snapshots are taken from |
| `format` | string | Pixel format of those pictures (`NV12`) |
| `encoding` | string | How snapshots are encoded |
| `frames` | u64 | Pictures captured since start |
| `fps` | f32 | Measured over the last second or so; `0` while failed |
| `sensor_width`, `sensor_height` | u32 | Sensor active area, `0` until brought up |
| `sensor_id` | string or null | Chip id, once brought up |
| `tuning` | string or null | ISP tuning file in use |
| `stream` | object or null | The H.264 stream; `null` when the backend has no encoder (the `test-pattern` backend) or `--stream off` |
| `stream.codec` | string | `H.264` |
| `stream.profile` | string | `Baseline`, `Main` or `High` |
| `stream.width`, `.height`, `.fps`, `.gop`, `.target_kbps` | u32 | Configured stream settings |
| `stream.kbps` | f32 | Measured bitrate over the last second or so |
| `stream.encoder` | string | Encoder name and version as the backend reports it |
| `error` | string or null | Last failure while `state` is `failed` |

`onvif` builds one media profile per entry from `stream` (resolution, fps,
bitrate, GOP), falling back to the board profile's `[video.stream]` when the
producer is down. `web` merges it into `/api/cameras`.

### `GET /snapshot.jpg[?camera=N]`

A JPEG of the latest picture of sensor `N` (the board sensor index; without
`camera`, the first camera the producer serves), at most `MAX_AGE` (3 s) old.

| Status | When |
| --- | --- |
| `200 image/jpeg` | A fresh picture was available or arrived within 1.5 s. `Cache-Control: no-store`. |
| `503` | That sensor has no picture yet (still starting, or `failed`; the body says which and why). |
| `404` | The producer serves no sensor `N`. |
| `500` | JPEG encoding failed. |

Snapshots are demand-gated: pictures are only copied out of the capture
buffer for `DEMAND_WINDOW` (10 s) after a request, so the first request after
a quiet spell may wait for the next frame. The picture is halved while wider
than `video.snapshot.max_width` and encoded at `video.snapshot.quality`
(board profile, overridable with `--snapshot-max-width` / `--quality`).

`web` proxies this as `/api/snapshot.jpg` and `/api/live.mjpeg`; `onvif`
advertises the `web` URL as the snapshot URI.

## Which index is which

`N` is the board's **sensor index** everywhere: on the video socket, in
`?camera=N`, in `Status.index`, in the RTSP path `vsN` and the ONVIF
profile `Profile_vsN`. It is never a position in a list.

The producer serves the board's `video.sensors` minus any sensor the kernel
does not have (`camerad` `select_sensors`), and keys each camera by its
sensor index (`camera::service::Camera::index`, `find`). A board with
`sensors = [1]`, or a two-sensor board whose sensor 0 failed to open, serves
camera `1` only: `"1\n"` and `?camera=1` work, `"0\n"` gets a closed socket
and `?camera=0` a 404.

Consumers do the same: `rtsp` subscribes with the index it mounts
(`vsN=camera:N`); `web` and `onvif` match `/status` entries to sensors by
their `index` field, never by array position, so a sparse or reordered
`/status` is fine.

## Stream naming and the RTSP port

`rtsp` mounts sensor `N` at `stream_path(N)` = `vsN` on `DEFAULT_RTSP_PORT`
(554):

```text
rtsp://<camera>/vs0      sensor 0
rtsp://<camera>/vs1      sensor 1
```

Without `--stream` or `--source`, `rtsp` derives one `vsN=camera:N` mount per
entry of the board profile's `video.sensors`. `onvif` and `web` build their
URLs from `stream_path` and the configured port; `web` returns them as
`rtsp_url` / `rtsp_port` in `/api/cameras`, and the web UI uses those instead
of hard-coding `554` or `vs`.
