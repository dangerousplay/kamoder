# Concerns — Kamoder

Severity: H = can strand/brick a camera or ship broken; M = maintenance/correctness risk; L = hygiene.

## Hardware fragility

- **H — Encoder driver oopses on memory fragmentation.** `/dev/venc` (`ak_venc_adapter.ko` V1.0.74) oopses instead of failing when it cannot get contiguous DMA. `camera` drops caches/compacts, sets `min_free_kbytes`, and creates encoder + channels once at start; they must never be re-created at runtime (`crates/camera/src/service.rs:357-367`, `main.rs:285-338`).
- **H — Dual-sensor bring-up is order-sensitive.** Both SC2331 sensors must be brought up and started together (`SET_DUAL_MODE`, `crates/camera/src/service.rs:208`, `device.rs:63`). Not covered by itest (no ISP in VM).
- **H — Watchdog handover.** Vendor app holds `/dev/watchdog` at boot; `wdt_feeder -w` waits, and `init.sh` kills vendor processes only while the feeder runs. Any change to service order, `critical`, or guard logic risks a reboot loop (`tools/supervisor.toml`, `tools/init.sh`).
- **H — GPIO 98 cuts Wi-Fi.** Only the wifi recovery ladder may touch it; lights enforces an allowlist (`crates/lights/src/gpio.rs` `ALLOWED`). Wi-Fi is the only path in on many units (no Ethernet/UART).
- **H — Autostart flash patch** writes `/dev/mtd6` (`tools/autostart/`). Reversible via `rollback.sh`, tested on one firmware (38.2.72, mtd6 md5 `f4c739e1…`); other firmware revisions untested.
- **M — Wi-Fi driver wedges under load** (SSV6x5x), hence the reload/power-cycle ladder (`crates/wifi/src/lib.rs`). Large uploads can drop the link.
- **M — 32 MB RAM budget.** Measured RSS ~1.6-3.3 MB per service (HARDWARE-FINDINGS §9, before `camera`/`rtsp` existed). Current totals with camera + rtsp not recorded in docs — unknown.

## Hardcoded, board-specific values

- GPIO numbers 19/37/48/67/68/98 (`tools/supervisor.toml` comment, `crates/lights/src/gpio.rs`).
- Sensor model/tuning naming `isp_<sensor>_mipi_2lane_mipi<N>.conf` in `/usr/local`; 1280x720, 2048 kbit/s CBR fixed in service config (`crates/camera/src/main.rs`, supervisor.toml comment).
- `/dev/venc` ioctl numbers and parameter block layouts reverse-engineered (`crates/camera/src/venc.rs`); the referenced `tools/re/venc-protocol` does **not exist** in this repo.
- PTZ limits are raw motor steps (pan 0-175, tilt 0-70) until steps-per-degree is measured (`crates/anyka-ptz/src/lib.rs`, HARDWARE-FINDINGS §9).
- Firmware paths: `/rom/wpa_supplicant0.conf`, `/etc/Wireless`, `/ipc/modules/wifi/ssv6x5x.ko`, `/ipc/patch/ko/presetmotor.ko`, `/rom/device.config`, `/usr/share/udhcpc/default.script` (CLI defaults in `crates/wifi/src/main.rs`, `crates/anyka-ptz/src/preset_motor/config.rs`).
- Vendor process names list in `tools/init.sh` (`VENDOR_PROCESSES`).
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

- **No CI gate**: no workflow runs `./x test`, `./x ci` or `./x itest`; release builds without tests or clippy (`.github/workflows/release.yml`).
- `./x ci` does not include `./x test`.
- itest cannot exercise camera/ISP/venc/audio/real PTZ; video path relies on manual hardware runs.
- Large uncommitted restructure (crates moved to `crates/`, new `crates/camera`, docs site): state of HEAD differs from working tree.

## Documentation drift (verify before trusting)

- `docs/notes/HARDWARE-FINDINGS.md` §1/§5 says ONVIF/RTSP/video blocked on AV100; superseded by pure-Rust `camera` + `rtsp`.
- §9 says autostart not made on the camera; project memory records the live unit's mtd6 patched 2026-10-11 so config.sh autostarts.
- `docs/notes/IMPLEMENTATION.md` crate table omits camera, rtsp, web, onvif, lights, auth, anyka-ptz; verification table predates them.
- `docs/content/docs/architecture.mdx` says rtsp is "off by default" and does not list the camera/snapshot service; `tools/supervisor.toml` enables both.
- Notes refer to `cross-compile/` (sibling dir outside this self-contained repo) and `tools/re/venc-protocol` (absent).

## Security posture (by design, documented)

- sshd runs as root, no privilege separation; PBKDF2 at 20 000 iterations (CPU-bound) (IMPLEMENTATION.md).
- ONVIF/RTSP secret stored plaintext in `etc/onvif.secret` (digest auth needs it); FAT card has no modes, so permission checks are relaxed on FAT.
- telnetd unauthenticated root on port 23 enabled by default (`start_telnetd=1`).
- No RTC: JWT `exp` compared only server-side; the web UI must not compare `exp` to the browser clock (open item, HARDWARE-FINDINGS §9).

## Unknowns

- Wire framing on `/tmp/kamoder-video.sock` beyond the `"N\n"` request (not mapped).
- Whether `./x test` currently passes on this working tree (not run).
- Whether `docs/notes/*` is rendered on the docs site.
