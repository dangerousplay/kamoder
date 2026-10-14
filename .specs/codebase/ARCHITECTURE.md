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
bin/supervisor --config supervisor.toml  -> services in declaration order
```

Evidence: `card/autorun.sh`, `card/Factory/config.sh`, `tools/init.sh`, `docs/content/docs/architecture.mdx`.

## Processes (all supervised, from `tools/supervisor.toml`)

| Service | Binary / crate | Role | Listens / owns |
|---|---|---|---|
| wdt_feeder | `demos/wdt_feeder` | feeds `/dev/watchdog`; `-w` waits while vendor app holds it; `critical = true` (cannot be stopped via control socket) | `/dev/watchdog` |
| lights | `crates/lights` | sole owner of light/amp GPIOs (19 IR-cut, 67 IR LEDs, 68 white, 48 red/blue, 37 amp); state in `etc/lights.json` | `/tmp/kamoder-lights.sock` |
| wifi | `crates/wifi` | Wi-Fi reconciler (desired = `camera.conf` `wifi_ssid`), rollback to last-good in `backup/`, escalation up to GPIO 98 power cycle | wpa_supplicant ctrl socket, `/rom/wpa_supplicant0.conf` |
| sshd | `crates/sshd` | russh password/pubkey SSH, PTY or pipes | tcp 2222 |
| web | `crates/web` | axum JSON API + SPA (`www/`) | tcp 80 |
| onvif | `crates/onvif` | ONVIF Profile S (Device/Media/PTZ/Imaging) + WS-Discovery; **owns PTZ motor** | tcp 8000, udp 3702, `/tmp/anyka-ptz.sock` |
| rtsp | `crates/rtsp` | pure-Rust RTSP 1.0, H.264 RFC 6184, Digest auth | tcp 554 (`/vs0`, `/vs1`) |
| snapshot | `crates/camera` (`camera serve`) | pure-Rust V4L2 capture of every sensor, HW H.264 encode via `/dev/venc`, JPEG snapshots | `127.0.0.1:3000`, `/tmp/kamoder-video.sock` |
| supervisor | `crates/supervisor` | kameo actor tree, restart policy/backoff/crash-loop guard, per-service rotated logs | `/tmp/anyka-supervisor.sock` |

Not supervised by default: SDK demos (`demos/ai_demo`, `aenc_demo`, `venc_demo`, `md_demo`, `md_demo_v8`, `rtsp_demo`, `jpeg_snapshot`) — vendor-SDK-linked ports of vendor C demos, run by hand via `tools/run-on-camera.sh`. `crates/anyka-ptz` ships `ptz-probe` bin. `itest/support` ships `wiphy-netns` (test-only).

## Inter-process communication

All local control channels are Unix sockets (mode 0600), JSON lines (one request + `\n`, one response + `\n`, `MAX_LINE` cap), with the protocol types in a library crate shared by server and clients:

| Socket | Server | Clients | Protocol crate |
|---|---|---|---|
| `/tmp/anyka-supervisor.sock` | supervisor | web | `crates/supervisor/src/control.rs` (lib half of supervisor) |
| `/tmp/anyka-ptz.sock` | onvif (`ptz_socket.rs`) | web, tools | `crates/ptz-proto` |
| `/tmp/kamoder-lights.sock` | lights | web, onvif (Imaging IR-cut) | `crates/lights-proto` |
| `/tmp/kamoder-video.sock` | camera | rtsp (`CameraSource`) | ad hoc: client writes `"N\n"`, receives H.264 access units of sensor N (`crates/camera/src/service.rs`, `crates/rtsp/src/source.rs`) — framing details not mapped |

Design rule "one owner per resource" (D4): proto crates have no SDK/GPIO deps and no build script, so feature unification cannot drag vendor `.so` links into clients (`crates/ptz-proto/src/lib.rs` doc).

HTTP between processes: web proxies `http://127.0.0.1:3000/snapshot.jpg` as `/api/snapshot.jpg` and `/api/live.mjpeg` (`crates/web/src/video.rs`). onvif advertises RTSP `rtsp://<ip>:554/vs0` and snapshot `http://<ip>:80/api/snapshot.jpg` (supervisor.toml args).

Shared files: `etc/onvif.secret` read by onvif, rtsp, web (web restarts rtsp when it changes); `etc/web-users`, `etc/web-jwt.key` (web); `etc/anyka-sshd.*` (sshd, editable from web); `camera.conf` (init.sh, wifi, web `/api/config`).

## Video data flow (AV100, current)

```
SC2331 sensor(s) -> ISP (kernel) -> /dev/video-N-M (V4L2 MMAP, NV12)
   camera: one capture thread per sensor; sensors brought up and started together (dual mode)
     -> /dev/venc (ak_venc_adapter.ko ioctls, one opener system-wide, channels created once at start)
        -> H.264 Annex-B AUs -> tokio broadcast -> /tmp/kamoder-video.sock -> rtsp -> RTP/TCP-interleaved or UDP
     -> latest frame -> jpeg-encoder -> GET /snapshot.jpg?camera=N -> web proxy -> browser / NVR
```

ISP tuning loaded from `--conf-dir /usr/local` (`isp_<sensor>_mipi_2lane_mipi<N>.conf`). Frames are copied only within `DEMAND_WINDOW` (10 s) of a request. Settings: 1280x720, CBR 2048 kbit/s (supervisor.toml comment). Legacy SDK path (EV200): `crates/anyka` wrappers (`vi.rs`, `venc.rs`, `rtsp.rs`) used only by demos.

## Library layering

```
anyka-sys (bindgen, EV200 + v8 modules) <- anyka (EV200 safe RAII) / anyka-v8 (AV100 V8 SDK)
anyka-link (build-script helper: per-binary SDK variant ld script + $ORIGIN/../lib/<variant> RPATH)
os (watchdog chardev, shutdown signals, compat)   telemetry (tracing + Prometheus)
auth (PBKDF2, JWT, credential files, perms)       onvif-schema (generated from WSDL/XSD)
anyka-ptz (PtzBackend trait: Sdk EV200, PresetMotor AV100 ioctl, Sim)
```

Services that are vendor-free at runtime: supervisor, wifi, lights, sshd, web, onvif (default features; `sdk` feature enables EV200 PTZ SDK backend), rtsp, camera, wdt_feeder.

## Configuration on the SD card (`/mnt/disc1/anyka_rust/`, layout from `xtask/src/tasks.rs` `sd`)

| File | Source | Owner |
|---|---|---|
| `supervisor.toml` | `tools/supervisor.toml` (paths relative to file) | build |
| `camera.conf` | seeded from `tools/camera.conf.example`; sourced by sh; secrets blanked after first boot | camera (preserved) |
| `etc/` | generated creds/keys, `lights.json`, `ptz-presets.json` | camera (preserved) |
| `logs/`, `backup/`, `metrics/` | runtime output | camera (preserved) |
| `bin/`, `lib/{audio,video,md,rtsp,v8,sys}`, `www/`, `init.sh`, `run-on-camera.sh` | build | build (replaced) |
| card root `autorun.sh`, `Factory/config.sh` | `card/` | build |

Runtime env (`crates/telemetry/src/lib.rs`): `ANYKA_LOG`, `ANYKA_LOG_FILE`, `ANYKA_LOG_MAX_BYTES`, `ANYKA_LOG_KEEP`, `ANYKA_METRICS` (`textfile:<dir>` or `http:<addr>`); `ANYKA_ONVIF_PTZ` (auto|sim|...), `ANYKA_ISP_CONFIG`.

## Concurrency model

Every async binary builds a tokio current-thread runtime (`new_current_thread` in each `main.rs`). Blocking hardware work goes to dedicated threads (camera capture threads, PTZ worker thread, supervisor log writer thread on a bounded channel, telemetry HTTP exporter thread).
