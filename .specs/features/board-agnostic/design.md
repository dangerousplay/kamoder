# Board-agnostic — Design

**Spec:** `spec.md` · **Scope of this pass:** AV100 board end to end (user, 2026-10-13). CAM-06/07 (generic V4L2 capture + M2M/software H.264) deferred; the `generic-v4l2` fallback profile ships with the `test-pattern` backend (snapshots only).

## Crate layout

```
crates/board          NEW  vendor-free: profile types, load/validate, autodetect, runtime handoff
crates/video-proto    NEW  vendor-free: video-socket framing, /status types, stream naming, defaults
crates/camera         CHANGED → generic core lib: Backend/Encoder traits, service (fan-out, HTTP,
                           video socket), JPEG, NV12 helpers, fake + test-pattern backend
crates/anyka-av100    NEW  AV100 backend: v4l2.rs, device.rs, conf.rs, venc.rs moved from camera
                           + impl camera::Backend / camera::Encoder
crates/camerad        NEW  package `camerad`, [[bin]] name = "camera": profile → backend → service
```

Dependency direction (no cycles): `board`, `video-proto` ← `camera` ← `anyka-av100` ← `camerad`.
`rtsp`, `web`, `onvif` depend on `video-proto` (+ `board`), never on `camera`/`anyka-*`.

## Board profile (`boards/<name>.toml`)

```toml
name = "anyka-av100-ptz"
description = "AK3918AV100 dual SC2331 pan/tilt (EVB_CBDM_KM01N)"

[match]                       # all listed rules must match; score = rules matched
cpuinfo_hardware = "AK3918AV100"     # /proc/cpuinfo `Hardware`
# dt_compatible = "..."              # substring of /proc/device-tree/compatible
# dt_model = "..."
# path_exists = ["/dev/preset_motor"]

[platform]
vendor_processes = ["daemon", "gwellupdater.sh", "soft_reboot.sh", "ipc", "anyka_ipc", "main"]
watchdog_wait = true

[video]
backend = "anyka-av100"      # anyka-av100 | test-pattern | (v4l2: future)
sensors = [0, 1]             # expected; rtsp mounts one stream each
tuning_dir = "/usr/local"
[video.stream]               # per-sensor defaults; [[video.override]] index=N for exceptions
width = 1280
height = 720
fps = 15
kbps = 2048
gop = 30
profile = "main"
[video.snapshot]
max_width = 640
quality = 80

[lights]
gpio_root = "/sys/class/gpio"
[[lights.line]]
role = "ir_cut"     # ir_cut | ir_leds | white | alarm | amp
gpio = 19           # or: path = "/sys/user-gpio/gpio-ircut_a"
active_low = false

[wifi]
iface = "wlan0"
module = "/ipc/modules/wifi/ssv6x5x.ko"
module_args = ["stacfgpath=/etc/Wireless/..."]
power_gpio = 98

[ptz]
backend = "preset_motor"   # preset_motor | sdk | sim
device = "/dev/preset_motor"
module = "/ipc/patch/ko/presetmotor.ko"
pan = [0, 175]
tilt = [0, 70]
steps_per_degree = 1.0
```

Rules (BRD-04, enforced by `board::Profile::validate`, `#[serde(deny_unknown_fields)]`):
- every lights role at most once; every GPIO number at most once across lights + wifi;
- `wifi.power_gpio` must not appear in lights (PER-03);
- `line` has exactly one of `gpio`/`path`; stream sizes even, fps 1..=60, kbps 64..=8000.

### Resolution (BRD-01/02/05)

`board::resolve(dirs: &[Path], override_name: Option<&str>, root: &Path) -> Result<Resolved>`
- override set → load `<dir>/<name>.toml` or error `UnknownBoard` (no fallback, edge case).
- else score every profile whose rules all match; highest wins; tie → error (CI catches via `./x ci` profile check).
- none → `generic-v4l2` with `reason = "no profile matched (SoC: <Hardware>)"`, logged as warning.
- `Resolved { name, path, reason, profile }`.

Handoff: supervisor resolves once at start, writes `Resolved` to `/tmp/kamoder-board.json` and sets `KAMODER_BOARD=/tmp/kamoder-board.json` in every child's env. `board::current()` reads it; when unset (dev/standalone) it resolves itself from `KAMODER_BOARDS_DIR` (default `<exe dir>/../boards`) + `BOARD` env. Override source: `BOARD=` in `camera.conf` (supervisor already reads camera.conf? if not, `--board` flag + init.sh exports `BOARD`).

Shell side (PKG-03): `supervisor board --shell` prints `KAMODER_BOARD_NAME=…`, `VENDOR_PROCESSES="…"`, `WATCHDOG_WAIT=1` for `init.sh` to `eval`.

## Camera core traits (CAM-01)

```rust
// crates/camera/src/backend.rs
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub enum PixelFormat { Nv12 }
pub struct FrameRef<'a> { pub width: u32, pub height: u32, pub format: PixelFormat,
                          pub data: &'a [u8], pub phys: Option<u32>, pub timestamp_ms: u64 }
pub struct SensorInfo { pub index: u32, pub width: u32, pub height: u32,
                        pub id: Option<String>, pub tuning: Option<String> }

/// All sensors of a board, brought up together (AV100 dual mode).
pub trait Backend: Send {
    type Session: Session;
    fn start(&mut self) -> Result<Self::Session, BoxError>;   // drop Session = teardown
}
pub trait Session {
    fn sensors(&self) -> Vec<SensorInfo>;
    /// Lends the next picture of sensor `slot` (position in `sensors()`).
    fn with_frame(&mut self, slot: usize, timeout: Duration,
                  f: &mut dyn FnMut(&FrameRef<'_>) -> Result<usize, BoxError>) -> Result<usize, BoxError>;
}
/// One H.264 channel per sensor, created once at start-up and kept for the
/// life of the process (AV100 driver oopses on re-create: CAM-03).
pub trait Encoder: Send {
    fn info(&self) -> video_proto::StreamInfo;          // codec/profile/size/fps/gop/kbps/encoder
    fn encode(&mut self, frame: &FrameRef<'_>, out: &mut dyn FnMut(Packet<'_>)) -> Result<usize, BoxError>;
    fn request_idr(&mut self) -> Result<(), BoxError>;
}
pub struct Packet<'a> { pub data: &'a [u8], pub keyframe: bool }
```

`service::capture<B: Backend>(backend, cameras, encoders: Vec<Option<Box<dyn Encoder>>>, stop)` is today's loop with `Sensor`/`venc::Channel` replaced by the traits; snapshot downscale uses `video.snapshot.max_width` (halve while wider).

Backends shipped:
- `camera::fake::{FakeBackend, FakeEncoder}` (cfg(test) + `test-support` feature): scripted frames/failures, fake Annex-B packets with IDR on request. Used by CAM-05 tests.
- `camera::pattern::PatternBackend` — moving NV12 test pattern, no encoder (generic-v4l2 fallback, QEMU).
- `anyka_av100::{Av100Backend, Av100Encoder}` — wraps existing `Sensor`, `Encoder`/`Channel`; `Av100Encoder::open_all(settings, n)` does `min_free_kbytes` + compaction + open-once (moved `make_room_for_encoder`, `open_encoder`, `open_channels`). `camera probe` subcommand moves to `camerad` (AV100-only, behind backend check).

## video-proto (PRO-01/03/05)

- `DEFAULT_SOCKET = "/tmp/kamoder-video.sock"`, `DEFAULT_STATUS_URL = "http://127.0.0.1:3000"`.
- `PacketHeader { len: u32, keyframe: bool, timestamp_ms: u64 }` with `encode() -> [u8; 13]` / `decode(&[u8;13])`; `subscribe_line(index) -> String`.
- `Status`, `StreamInfo` (serde, both directions) — moved from camera; `codec`/`profile` become `String`.
- `stream_path(index) -> String` (`vs{index}`), `DEFAULT_RTSP_PORT = 554`.
- Doc: `docs/notes/VIDEO-PROTOCOL.md`.

## Consumers

- **rtsp**: `CameraSource` uses `video_proto::PacketHeader`; when no `--stream` given, mounts `stream_path(i)=camera:i` for each `board.video.sensors` (PKG-02). Retries on producer loss (exists, PRO-04).
- **onvif** (PRO-02): fetch producer `/status` (std TCP + minimal HTTP GET or existing http client in tree; reuse `web`'s approach) at GetProfiles time with short cache; one profile per running sensor (`Profile_vsN`), resolution/fps/bitrate/GOP from `StreamInfo`; stream URI uses `stream_path`. Fallback to profile `video.stream` when producer unreachable, and mark via log. Remove `VIDEO_*` consts.
- **web** (PRO-03, BRD-06): `/api/cameras` adds `rtsp_url`/`rtsp_port` built with `video_proto`; `/api/hardware` adds `board { name, reason }`; `/api/lights` passes capabilities. webui uses API values (no `554`, `vs`, `wlan0` literals).

## Peripherals

- **lights** (PER-01..03): `Line` enum stays as *role*; pin map built at runtime from `board.lights`; `Gpio` trait gains a path-addressed impl for `/sys/user-gpio/*` style (value file only). `ALLOWED` = profile lines. lights-proto `State` gains `supported: Vec<Role>`; unsupported set → error reply.
- **wifi** (PER-04): `recovery::USB_POWER_GPIO` → field from profile (`None` → skip power-cycle rung). CLI/env defaults for iface/module/args come from profile; flags still override. supervisor `ready.rs` iface from profile.
- **anyka-ptz** (PER-05): `Limits` from profile (`Limits::EV200` kept only as `sdk` backend default in that module); `OpenOptions` device/module from profile; onvif passes them.

## Supervisor & packaging (PKG-01..04)

- supervisor.toml `[[service]]` gains `when = "<capability>"` (`ptz`, `lights`, `wifi`, `video.encoder`) — service skipped (logged) when profile lacks it. Env `KAMODER_BOARD` injected.
- supervisor.toml snapshot service: `bin/camera serve` with no AV100 flags (profile-driven); rtsp: no `--stream` args.
- `./x dist`: ships `boards/`; SDK demos only with `./x dist --with-demos`.
- `./x ci` adds `check-boards` (load+validate all profiles, detect ties) and `check-literals` (grep generic crates for `AV100|EV200|EV300|wlan0|/dev/venc|/dev/preset_motor` and `gpio\s*[:=]\s*\d+`, allowlist for tests/docs comments via `// board-literal-ok`).

## Error handling / conventions

snafu enums per crate (`board::Error { Read, Parse, Invalid { file, key, reason }, UnknownBoard, Tie }`); eyre in binaries; tracing; metrics `kamoder_*`. Tests: inline `#[cfg(test)]`, fixture roots via `TempDir` helper pattern (anyka-ptz `test_support.rs`).

## Risks

- AV100 regression only verifiable on the live unit (manual step in final task; Wi-Fi drops under load — upload via chunked tools/camera-access).
- Moving `Status` to video-proto changes `&'static str` → `String`: web `cameras.rs` deserialises it — keep JSON field names identical.
- Feature unification: `anyka-av100` must not pull any SDK crate.
