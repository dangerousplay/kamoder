# Concerns — Kamoder

Severity: H = can strand/brick a camera or ship broken; M = maintenance/correctness risk; L = hygiene.

## Hardware fragility

- **H — Encoder driver oopses on memory fragmentation.** `/dev/venc` (`ak_venc_adapter.ko` V1.0.74) oopses instead of failing when it cannot get contiguous DMA. `camerad` opens the encoder + one channel per sensor once at start via `Av100Encoder::open_all`, which drops caches/compacts and sets `min_free_kbytes` before each channel (`make_room_for_encoder`); encoders live outside the capture session and are never re-created at runtime (`crates/anyka-av100/src/lib.rs`, `crates/camerad/src/main.rs` `serve_av100`, trait contract in `crates/camera/src/backend.rs`).
- **H — Dual-sensor bring-up is order-sensitive.** Both SC2331 sensors must be brought up and started together (`SET_DUAL_MODE`; `Backend::start` brings all sensors up at once, `crates/anyka-av100/src/lib.rs`, `device.rs`). Not covered by itest (no ISP in VM; the VM runs the `test-pattern` backend).
- **H — Watchdog handover.** Vendor app holds `/dev/watchdog` at boot; `wdt_feeder -w` waits, and `init.sh` kills vendor processes only while the feeder runs. Any change to service order, `critical`, or guard logic risks a reboot loop (`tools/supervisor.toml`, `tools/init.sh`).
- **H — GPIO 98 cuts Wi-Fi.** Only the wifi recovery ladder may touch `wifi.power_gpio`; lights may drive only the lines in the board's `[lights]`, and `board` validation rejects a profile whose light line uses `wifi.power_gpio` (`crates/board/src/profile.rs` `Lights::validate`, PER-03). A wrong profile is now the risk: `./x check-boards` validates every shipped profile. Wi-Fi is the only path in on many units (no Ethernet/UART).
- **H — Autostart flash patch** writes `/dev/mtd6` (`tools/autostart/`). Reversible via `rollback.sh`, tested on one firmware (38.2.72, mtd6 md5 `f4c739e1…`); other firmware revisions untested.
- **M — Wi-Fi driver wedges under load** (SSV6x5x), hence the reload/power-cycle ladder (`crates/wifi/src/lib.rs`). Large uploads can drop the link.
- **M — 32 MB RAM budget.** Measured RSS ~1.6-3.3 MB per service (HARDWARE-FINDINGS §9, before `camera`/`rtsp` existed). Current totals with camera + rtsp not recorded in docs — unknown (to record in T20).

## Board-specific values

Since the board-agnostic feature, board facts live in `boards/*.toml` (crate `board`) and generic crates are kept free of them by `./x check-literals`. What remains:

- AV100 values are in `boards/anyka-av100-ptz.toml`: GPIOs 19/37/48/67/68 (lights) and 98 (Wi-Fi power), 1280x720 / 2048 kbit/s CBR / GOP 30, `tuning_dir = /usr/local`, Wi-Fi module path, `presetmotor.ko`, vendor process list.
- Deliberate fallbacks marked `// board-literal-ok:` in generic crates (e.g. wifi's built-in AV100 defaults when no board resolves, so Wi-Fi never depends on `boards/`), and `DEFAULT_VENDOR_PROCESSES` in `tools/init.sh` when `supervisor board --shell` fails.
- Hardware crates legitimately hold SoC specifics: tuning naming `isp_<sensor>_mipi_2lane_mipi<N>.conf` and `/dev/venc` ioctl numbers / parameter block layouts reverse-engineered in `crates/anyka-av100/src/{conf,venc}.rs` (the source traces are not in this repo; the constants are the record).
- PTZ limits are raw motor steps (pan 0-175, tilt 0-70, `steps_per_degree = 1.0` in the AV100 profile) until steps-per-degree is measured (HARDWARE-FINDINGS §9).
- Firmware paths still CLI defaults: `/rom/wpa_supplicant0.conf`, `/etc/Wireless`, `/rom/device.config`, `/usr/share/udhcpc/default.script` (`crates/wifi/src/main.rs`, `crates/anyka-ptz/src/preset_motor/config.rs`).
- `/tmp` socket paths duplicated as defaults across crates and `tools/supervisor.toml` (`/tmp/anyka-*.sock`, `/tmp/kamoder-*.sock`) — naming inconsistent (anyka vs kamoder).
- Toolchain path `/opt/arm-anykav200-crosstool` hardcoded in `.cargo/config.toml` `[env]` (follows `CC_armv5te_...` env vars, not `ANYKA_TOOLCHAIN`), `tools/ak-linker.sh`, `xtask`, `anyka-sys/build.rs`.

## Two SDK generations / legacy surface

- EV200 SDK (`sdk/lib`, `crates/anyka`, 6 demos) does not work on the AV100 unit (firmware/SDK mismatch, HARDWARE-FINDINGS §1); kept because "already-verified binaries build against it". Demos are untestable on host and unverified on AV100.
- `anyka-sys` generates both bindings; `clashing_extern_declarations` allowed in `v8`. Safety relies on `anyka-link` never linking both generations into one binary.
- Cargo feature unification has bitten twice (anyka-sys `v8` feature; anyka-ptz `control` feature leaking SDK links into web). Any new feature on an SDK crate can leak vendor `.so` into vendor-free binaries.
- V8 SDK is an NDA-origin mirror (`sdk/v8/PROVENANCE.md`): licensing risk for a public GPL repo/releases.
- `md_demo_v8` built but never run on hardware (IMPLEMENTATION.md, HARDWARE-FINDINGS §7).

## Toolchain / build risk

- Crosstool uClibc 0.9.33 vs camera uClibc-ng 1.0.31: mitigated by stubs + symbol-closure check against a recorded `sdk/runtime/firmware-libc.symbols` (one firmware). A different firmware libc may need a new symbol list.
- Tier-3 target on nightly: local default `nightly` is unpinned; only CI pins `nightly-2026-09-20`. No `rust-toolchain.toml`.
- Crosstool download from a third-party GitHub mirror; sha256 pin optional (`CROSSTOOL_SHA256`), warns only.
- `opentelemetry_sdk`, `aws-lc-rs`, `async-std` banned; any dep needing `AtomicU64` fails to compile.
- patchelf'd executables crash the camera loader; RPATHs must be set at link time.

## Process / quality gaps

- **Partial CI gate**: `.github/workflows/checks.yml` runs `./x check-boards` and `./x check-literals` on push/PR; no workflow runs `./x test`, clippy, deny or `./x itest` (they need the crosstool + nightly build-std); release builds without tests or clippy (`.github/workflows/release.yml`).
- `./x ci` now includes `./x test`, `check-boards` and `check-literals` (`xtask/src/tasks.rs` `ci`).
- itest cannot exercise camera/ISP/venc/audio/real PTZ; video path relies on manual hardware runs.
- itest runs the `generic-v4l2` profile (test-pattern backend), so the AV100 profile's lights/PTZ/video paths are only checked by host unit tests and manual hardware runs.

## Documentation drift (verify before trusting)

- Fixed 2026-10-13 (board-agnostic T19): HARDWARE-FINDINGS §1/§5/§9 carry dated updates (pure-Rust video, mtd6 autostart kit); IMPLEMENTATION.md crate list and verification table; architecture.mdx (rtsp + camera on, board profiles, video data flow); services/cameras/index pages; README crate table; `tools/re/venc-protocol` reference removed from `crates/anyka-av100/src/venc.rs`. New: `docs/notes/VIDEO-PROTOCOL.md`, docs page "Adding a board".
- Remaining: notes still refer to `cross-compile/` (sibling dir outside this self-contained repo) as historical context.
- Live verification of RTSP `/vs0`+`/vs1` and ONVIF per-sensor profiles under board profiles not yet recorded (T20).

## Security posture (by design, documented)

- sshd runs as root, no privilege separation; PBKDF2 at 20 000 iterations (CPU-bound) (IMPLEMENTATION.md).
- ONVIF/RTSP secret stored plaintext in `etc/onvif.secret` (digest auth needs it); FAT card has no modes, so permission checks are relaxed on FAT.
- telnetd unauthenticated root on port 23 enabled by default (`start_telnetd=1`).
- No RTC: JWT `exp` compared only server-side; the web UI must not compare `exp` to the browser clock (open item, HARDWARE-FINDINGS §9).

## Unknowns

- ~~Wire framing on `/tmp/kamoder-video.sock`~~ — now specified in `crates/video-proto` and `docs/notes/VIDEO-PROTOCOL.md`.
- Whether `./x test` currently passes on this working tree (not run).
- Whether `docs/notes/*` is rendered on the docs site (it is not: the site renders `docs/content/docs/*.mdx` only; site pages refer to notes by path).
