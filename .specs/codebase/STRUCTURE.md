# Directory Structure — rust-cross-compile/

Self-contained repo (own `.git`; project "Kamoder"). Crates live under `crates/` (committed in `1ff076e`); board-agnostic layer (`board`, `video-proto`, `anyka-av100`, `camerad`, `boards/`) added 2026-10-13.

```
rust-cross-compile/
├── Cargo.toml / Cargo.lock      workspace (32 members), shared deps, release profile, [patch.crates-io]
├── .cargo/config.toml           target triple, linker wrapper, build-std, cross CC env
├── x                            sh wrapper: runs xtask for the host
├── deny.toml                    cargo-deny policy (bans otel sdk, aws-lc-rs, async-std)
├── cliff.toml                   git-cliff changelog config (Conventional Commits)
├── README.md                    long-form project README (568 lines)
├── LICENSE                      GPL-3.0-or-later
├── crates/
│   ├── anyka-sys/               bindgen FFI over sdk/include (+ sdk/v8 as anyka_sys::v8); wrapper.h, wrapper_v8.h
│   ├── anyka-link/              build-script helper: per-binary SDK variant ld script + RPATH
│   ├── anyka/                   safe RAII wrappers, EV200 SDK (vi, venc, ai, aenc, md, rtsp, ir, watchdog, time, log)
│   ├── anyka-v8/                safe wrappers, AV100 V8 SDK (vi, md)
│   ├── os/                      kernel-only helpers: watchdog chardev, shutdown signals, compat
│   ├── telemetry/               tracing subscriber + Prometheus exporters (textfile/http), log rotation
│   ├── auth/                    PBKDF2 passwords, JWT, web users, authorized_keys, onvif secret, file perms
│   ├── supervisor/              process supervisor (kameo) + lib: control socket protocol/client
│   ├── wifi/                    Wi-Fi reconciler (decide/reconcile/recovery/wpa/net/system) + lib
│   ├── lights/                  GPIO owner service (lines from the board's [lights]; sysfs or path-addressed) + lib
│   ├── lights-proto/            lights socket wire types + async client
│   ├── anyka-ptz/               PtzBackend trait: sdk (EV200), preset_motor (AV100 ioctl), sim; bin ptz-probe
│   ├── ptz-proto/               PTZ socket wire types + async client
│   ├── onvif-schema/            ONVIF types; schema/ (vendored WSDL/XSD), src/generated (committed codegen), tests/xml.rs
│   ├── onvif/                   ONVIF device service + WS-Discovery + PTZ socket server
│   ├── rtsp/                    RTSP server (h264, digest, session, source); tests/stream.rs, tests/data/clip.h264
│   ├── sshd/                    SSH server (main.rs, pty.rs); tests/streaming.rs
│   ├── web/                     axum admin API (one module per feature) + SPA serving; api_tests.rs, tests.rs
│   ├── board/                   board profiles: profile.rs (schema + validate), resolve.rs (detect/score/fallback), handoff.rs (KAMODER_BOARD)
│   ├── video-proto/             producer protocol: socket header, subscribe line, Status/StreamInfo, stream_path, defaults
│   ├── camera/                  camera core lib (no hardware): backend.rs (traits), service.rs, jpeg.rs, pattern.rs, fake.rs (test-support)
│   ├── anyka-av100/             AV100 video backend: conf (ISP tuning v7), device, v4l2, venc (/dev/venc); Av100Backend/Av100Encoder
│   └── camerad/                 camera service package, [[bin]] name = "camera" (serve, probe); settings.rs (profile + flags merge)
├── boards/                      board profiles: anyka-av100-ptz.toml, generic-v4l2.toml (fallback); shipped to the card next to bin/
├── demos/                       Rust ports of vendor C demos (SDK-linked, build.rs calls anyka-link)
│   ├── wdt_feeder/              watchdog feeder (no SDK; supervised)
│   ├── ai_demo/ aenc_demo/      audio capture / encode (EV200 SDK)
│   ├── venc_demo/ md_demo/      H.264+JPEG / motion detection (EV200 SDK)
│   ├── md_demo_v8/              motion detection on V8 SDK
│   ├── rtsp_demo/               vendor SDK RTSP server
│   └── jpeg_snapshot/           HTTP JPEG server on SDK
├── xtask/                       host build tool (own workspace): main.rs, tasks.rs, checks.rs (check-boards, check-literals), flash.rs, gen_onvif.rs, itest.rs
├── itest/                       system tests: pytest + labgrid on QEMU ARM926 VM
│   ├── env/qemu.yaml            labgrid environment
│   ├── guest/                   VM init (firmware-init), access-point scripts, wpa conf
│   ├── support/                 crate anyka-itest-support: bin wiphy-netns
│   ├── tests/                   conftest.py + test_01..09_*.py
│   └── build-images.sh          sd.img + initramfs
├── sdk/                         vendored vendor SDK (NDA-origin for v8)
│   ├── include/                 EV200 headers (ak_*.h)
│   ├── lib/{audio,md,rtsp,video} EV200 .so variants (not interchangeable)
│   ├── v8/{include,lib}         AV100 V8 SDK + PROVENANCE.md
│   └── runtime/                 libgcc_s, libstdc++, firmware-libc.symbols, README
├── third_party/onvif-device-rs/ patched upstream crate (PATCHES.md, UPSTREAM-PR.md)
├── card/                        SD card root files: autorun.sh, Factory/config.sh, Factory/README.md
├── tools/
│   ├── supervisor.toml          service definitions shipped to the card
│   ├── camera.conf.example      user settings template (sh-sourced)
│   ├── init.sh                  on-camera boot entry point (604 lines POSIX sh)
│   ├── run-on-camera.sh         launcher for ad hoc binaries on camera
│   ├── ak-linker.sh             linker wrapper
│   ├── install-toolchain.sh     installs Anyka crosstool to /opt
│   ├── package-release.sh       release tarballs + SHA256SUMS
│   ├── SD-README.md             README copied onto the card
│   ├── autostart/               AV100 mtd6 setup.sh patch kit (mtdpatch C tool, flash/rollback scripts)
│   ├── qemu-armv5/              VM builder (run.sh, kernel patches); output in out/
│   ├── cam.py, push.py          telnet shell / chunked push helpers
│   └── camera-access/           ssh/upload/ONVIF probe helpers (gitignored)
├── webui/                       React + Vite admin SPA (src/pages, components, hooks, lib/api.ts); dist/ built
├── docs/                        Fumadocs/Next site (app/, content/docs/*.mdx, components/, lib/) + notes/
│   └── notes/                   HARDWARE-FINDINGS, IMPLEMENTATION, VIDEO-PROTOCOL, PRESET-MOTOR-ABI, SDK-SOURCES, RUST-REWRITE-ESTIMATE
├── .github/workflows/           release.yml (build+sign+publish), docs.yml (Pages), checks.yml (check-boards, check-literals)
├── dist/                        build output: anyka_rust/ (unstripped staging), sdcard/ (gitignored)
└── target/                      cargo output (gitignored)
```

## Crate file conventions

- Binaries: `src/main.rs` with clap `Cli` struct; a `src/lib.rs` exists where another crate or tests need the internals (supervisor, wifi, lights, web, rtsp, anyka-ptz). `camera` is lib-only; its binary lives in `camerad`.
- SDK-linking binaries have a `build.rs` calling `anyka_link::emit(&[Module::...])` (demos, `crates/anyka-ptz/build.rs` when `sdk` feature on ARM).
- Integration tests only in `crates/{onvif-schema,rtsp,sshd}/tests/`; everything else is inline `#[cfg(test)] mod tests`.
