# Architecture — Kamoder

## Boot chain

```
firmware rcS -> rc.local -> /ipc/setup.sh   (drivers, Wi-Fi, GPIO 98)
   | AV100: only with the mtd6 autostart patch (tools/autostart/) or by hand
   v
/mnt/disc1/Factory/config.sh  (card/Factory/config.sh; card/autorun.sh hands off to it)
   | sources anyka_rust/camera.conf; optional rescue telnetd; run_rust=0 -> legacy anyka_happy
   v
anyka_rust/init.sh  (tools/init.sh; POSIX sh, "must never brick")
   | preflight, first-boot credential hashing, vendor-process guard loop (only while wdt_feeder runs),
   | supervisor restart loop (MAX_SUPERVISOR_RESTARTS=10)
   v
bin/supervisor --config supervisor.toml  -> resolve board (boards/*.toml), write /tmp/kamoder-board.json,
                                            skip services by `when`, start the rest in declaration order
```

`init.sh` runs `bin/supervisor board --shell` first (vendor process list, watchdog wait); on failure it keeps built-in AV100 defaults.

Evidence: `card/autorun.sh`, `card/Factory/config.sh`, `tools/init.sh`, `docs/content/docs/architecture.mdx`.

## Processes (all supervised, from `tools/supervisor.toml`)

| Service | Binary / crate | Role | Listens / owns |
|---|---|---|---|
| wdt_feeder | `demos/wdt_feeder` | feeds `/dev/watchdog`; `-w` waits while vendor app holds it; `critical = true` (cannot be stopped via control socket) | `/dev/watchdog` |
| lights (`when = "lights"`) | `crates/lights` | sole owner of the light/amp lines in the board's `[lights]` (AV100: 19 IR-cut, 67 IR LEDs, 68 white, 48 red/blue, 37 amp); state in `etc/lights.json` | `/tmp/kamoder-lights.sock` |
| wifi | `crates/wifi` | Wi-Fi reconciler (desired = `camera.conf` `wifi_ssid`), rollback to last-good in `backup/`, escalation up to a `wifi.power_gpio` power cycle (AV100: GPIO 98); iface/module from the board, built-in fallback when none resolves | wpa_supplicant ctrl socket, `/rom/wpa_supplicant0.conf` |
| sshd | `crates/sshd` | russh password/pubkey SSH, PTY or pipes | tcp 2222 |
| web | `crates/web` | axum JSON API + SPA (`www/`) | tcp 80 |
| onvif | `crates/onvif` | ONVIF Profile S (Device/Media/PTZ/Imaging) + WS-Discovery; one media profile per sensor from the producer's `/status`; **owns PTZ motor** (backend/limits from `[ptz]`) | tcp 8000, udp 3702, `/tmp/anyka-ptz.sock` |
| rtsp (`when = "video.encoder"`) | `crates/rtsp` | pure-Rust RTSP 1.0, H.264 RFC 6184, Digest auth; one `vsN=camera:N` mount per `video.sensors` entry | tcp 554 (`/vs0`, `/vs1`) |
| snapshot (`when = "video"`) | `crates/camerad` (bin `camera serve`) over `crates/camera` + `crates/anyka-av100` | capture of every sensor with the board's `video.backend` (`anyka-av100`: V4L2 + ISP + `/dev/venc`; `test-pattern`), JPEG snapshots, `/status` | `127.0.0.1:3000`, `/tmp/kamoder-video.sock` |
| supervisor | `crates/supervisor` | resolves the board once, kameo actor tree, restart policy/backoff/crash-loop guard, per-service rotated logs | `/tmp/anyka-supervisor.sock`, `/tmp/kamoder-board.json` |

Not supervised by default: SDK demos (`demos/ai_demo`, `aenc_demo`, `venc_demo`, `md_demo`, `md_demo_v8`, `rtsp_demo`, `jpeg_snapshot`) — vendor-SDK-linked ports of vendor C demos, run by hand via `tools/run-on-camera.sh`. `crates/anyka-ptz` ships `ptz-probe` bin. `itest/support` ships `wiphy-netns` (test-only).

## Inter-process communication

All local control channels are Unix sockets (mode 0600), JSON lines (one request + `\n`, one response + `\n`, `MAX_LINE` cap), with the protocol types in a library crate shared by server and clients:

| Socket | Server | Clients | Protocol crate |
|---|---|---|---|
| `/tmp/anyka-supervisor.sock` | supervisor | web | `crates/supervisor/src/control.rs` (lib half of supervisor) |
| `/tmp/anyka-ptz.sock` | onvif (`ptz_socket.rs`) | web, tools | `crates/ptz-proto` |
| `/tmp/kamoder-lights.sock` | lights | web, onvif (Imaging IR-cut) | `crates/lights-proto` |
| `/tmp/kamoder-video.sock` | camera | rtsp (`CameraSource`) | `crates/video-proto`: client writes `"N\n"`, receives `[len u32 BE][flags u8][timestamp_ms u64 BE]` + Annex-B AU, first record an IDR, laggards skipped to next IDR (`docs/notes/VIDEO-PROTOCOL.md`) |

Design rule "one owner per resource" (D4): proto crates have no SDK/GPIO deps and no build script, so feature unification cannot drag vendor `.so` links into clients (`crates/ptz-proto/src/lib.rs` doc).

HTTP between processes: web proxies `http://127.0.0.1:3000/snapshot.jpg` as `/api/snapshot.jpg` and `/api/live.mjpeg` (`crates/web/src/video.rs`) and merges `/status` into `/api/cameras` with `rtsp_url`/`rtsp_port` (`crates/web/src/cameras.rs`). onvif reads `/status` (`--producer`) for per-sensor profiles and advertises `rtsp://<ip>:554/vsN` (via `video_proto::stream_path`) and snapshot `http://<ip>:80/api/snapshot.jpg`.

Board handoff: supervisor writes `board::Resolved` JSON to `/tmp/kamoder-board.json` and sets `KAMODER_BOARD` for every child; `board::current()` reads it (or resolves itself when started by hand).

Shared files: `etc/onvif.secret` read by onvif, rtsp, web (web restarts rtsp when it changes); `etc/web-users`, `etc/web-jwt.key` (web); `etc/anyka-sshd.*` (sshd, editable from web); `camera.conf` (init.sh, wifi, web `/api/config`).

## Video data flow (AV100, current)

```
SC2331 sensor(s) -> ISP (kernel) -> /dev/video-N-M (V4L2 MMAP, NV12)
   camerad picks backend from board video.backend -> anyka_av100::Av100Backend (camera::backend::Backend)
   camera::service::capture: ONE capture thread reads every sensor in turn; Backend::start brings all up together (dual mode)
     -> Encoder (Av100Encoder: /dev/venc, one opener system-wide, channels opened once at start, outside the session)
        -> H.264 Annex-B AUs -> tokio broadcast (VIDEO_BUFFER 16) -> /tmp/kamoder-video.sock -> rtsp -> RTP/TCP-interleaved or UDP
     -> latest frame (only within DEMAND_WINDOW 10 s of a request) -> JPEG -> GET /snapshot.jpg?camera=N -> web proxy -> browser / NVR
```

Settings come from the board profile (`[video.stream]`, `[[video.override]]`, `[video.snapshot]`, `tuning_dir`), with camerad flags on top (`crates/camerad/src/settings.rs`); AV100: 1280x720, 15 fps, CBR 2048 kbit/s, GOP 30, main, snapshots halved to <= 640 wide. Unknown board -> `generic-v4l2` -> `test-pattern` backend (snapshots only, no encoder, so rtsp is skipped). Legacy SDK path (EV200): `crates/anyka` wrappers (`vi.rs`, `venc.rs`, `rtsp.rs`) used only by demos.

## Library layering

```
anyka-sys (bindgen, EV200 + v8 modules) <- anyka (EV200 safe RAII) / anyka-v8 (AV100 V8 SDK)
anyka-link (build-script helper: per-binary SDK variant ld script + $ORIGIN/../lib/<variant> RPATH)
os (watchdog chardev, shutdown signals, compat)   telemetry (tracing + Prometheus)
auth (PBKDF2, JWT, credential files, perms)       onvif-schema (generated from WSDL/XSD)
anyka-ptz (PtzBackend trait: Sdk EV200, PresetMotor AV100 ioctl, Sim)
board (profiles, validate, detect, handoff)       video-proto (producer protocol; serde only)
camera (Backend/Session/Encoder traits, service, jpeg, pattern) <- anyka-av100 (AV100 backend) <- camerad (bin `camera`)
```

Services that are vendor-free at runtime: supervisor, wifi, lights, sshd, web, onvif (default features; `sdk` feature enables EV200 PTZ SDK backend), rtsp, camera (camerad + anyka-av100), wdt_feeder.

## Configuration on the SD card (`/mnt/disc1/anyka_rust/`, layout from `xtask/src/tasks.rs` `sd`)

| File | Source | Owner |
|---|---|---|
| `supervisor.toml` | `tools/supervisor.toml` (paths relative to file) | build |
| `camera.conf` | seeded from `tools/camera.conf.example`; sourced by sh; secrets blanked after first boot | camera (preserved) |
| `etc/` | generated creds/keys, `lights.json`, `ptz-presets.json` | camera (preserved) |
| `logs/`, `backup/`, `metrics/` | runtime output | camera (preserved) |
| `bin/`, `boards/`, `lib/sys`, `www/`, `init.sh`, `run-on-camera.sh` (+ `lib/{audio,video,md,rtsp,v8}` and demo binaries only with `--with-demos`) | build | build (replaced) |
| card root `autorun.sh`, `Factory/config.sh` | `card/` | build |

Runtime env (`crates/telemetry/src/lib.rs`): `ANYKA_LOG`, `ANYKA_LOG_FILE`, `ANYKA_LOG_MAX_BYTES`, `ANYKA_LOG_KEEP`, `ANYKA_METRICS` (`textfile:<dir>` or `http:<addr>`); `ANYKA_ONVIF_PTZ` (auto|sim|...), `ANYKA_ISP_CONFIG`. Board (`crates/board`): `KAMODER_BOARD` (handoff file), `KAMODER_BOARDS_DIR`, `BOARD` (override, from `camera.conf`).

## Concurrency model

Every async binary builds a tokio current-thread runtime (`new_current_thread` in each `main.rs`). Blocking hardware work goes to dedicated threads (the camera capture thread, PTZ worker thread, supervisor log writer thread on a bounded channel, telemetry HTTP exporter thread).
