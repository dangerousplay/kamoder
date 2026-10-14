# Technology Stack — Kamoder (rust-cross-compile/)

Mapped from the working tree on 2026-10-13 (includes uncommitted move of crates into `crates/`).

## Target hardware

- SoC: Anyka AK3918AV100 (primary, live unit), AK3918EV200 (original target, legacy SDK). ARM926EJ-S, ARMv5TE, ~400 MHz, 32 MB RAM.
  Evidence: `docs/notes/HARDWARE-FINDINGS.md` header, `Cargo.toml` profile comment.
- Kernel 4.4.282 (AV100), BusyBox userland, uClibc-ng 1.0.31 (`libc.so.0`). No RTC (clock starts at 1970).
- No 64-bit atomics (`AtomicU64`/`AtomicI64` absent from std on ARMv5TE). This rules out `opentelemetry_sdk`, `ractor`, `bastion` (`docs/notes/IMPLEMENTATION.md`).

## Rust toolchain

| Item | Value | Evidence |
|---|---|---|
| Target triple | `armv5te-unknown-linux-uclibceabi` (tier 3) | `.cargo/config.toml`, `xtask/src/tasks.rs` `TARGET` |
| Rust channel | nightly + `rust-src`, `-Z build-std=std,panic_abort,panic_unwind` | `.cargo/config.toml` `[unstable]` |
| Toolchain selection | `ANYKA_RUST_TOOLCHAIN` (default `nightly`, unpinned locally; CI pins `nightly-2026-09-20`) | `xtask/src/tasks.rs`, `.github/workflows/release.yml` |
| rustflags | `-C target-cpu=arm926ej-s` | `.cargo/config.toml` |
| Linker | `tools/ak-linker.sh` wraps the cross gcc: adds `-rpath-link sdk/lib`, brackets `-lanyka_sdk_selected` with `--no-as-needed`, bakes `-rpath $ORIGIN/../lib/sys` | `tools/ak-linker.sh` |
| Cross C toolchain | `/opt/arm-anykav200-crosstool` (`arm-anykav200-linux-uclibcgnueabi-*`, GCC 4.8.5, uClibc 0.9.33.2, i386 host binaries) | `tools/install-toolchain.sh`, `ANYKA_TOOLCHAIN` env |
| C env for build scripts | `CC_/AR_/CFLAGS_armv5te_unknown_linux_uclibceabi` | `.cargo/config.toml` `[env]` |
| No `rust-toolchain.toml`, no `rustfmt.toml`, no `clippy.toml` | defaults apply | repo root listing |

### uClibc constraints

- Crosstool links against uClibc 0.9.33 (split libpthread/libm/librt/libdl); camera has uClibc-ng 1.0.31 with all in `libc.so.0`. `./x dist` builds empty stub `.so` files into `lib/sys` and checks every undefined dynamic symbol against `sdk/runtime/firmware-libc.symbols` (`xtask/src/tasks.rs` `stage_runtime`, `check_closure`).
- `forkpty()`/`libutil` absent: sshd opens `/dev/ptmx` directly (`crates/sshd/src/pty.rs`).
- patchelf on *executables* crashes the camera ld.so; only shared objects are patched (`xtask/src/tasks.rs` comment on `LIBRARY_RPATH`).

## Release profile (`Cargo.toml`)

`opt-level=3`, fat `lto`, `codegen-units=1`, `panic="unwind"` (Drop must release SDK/ISP resources), `strip=false`, `debug="line-tables-only"`. `./x sd` / `tools/package-release.sh` strip for the card.

## Key Rust dependencies (workspace `Cargo.toml` + crate manifests)

| Purpose | Crate(s) |
|---|---|
| Errors | `snafu 0.8` (libraries), `eyre 0.6` (binaries) |
| Logging / metrics | `tracing`, `tracing-subscriber` (fmt, env-filter), `metrics 0.24`, `metrics-exporter-prometheus 0.17`, `file-rotate 0.8` |
| CLI | `clap 4` (derive, env) |
| Async | `tokio 1` (always current-thread runtime), `async-std` banned |
| Actors | `kameo 0.17` (supervisor) |
| HTTP | `axum 0.8` (http1 only), `tower-http 0.7` fs (web), `futures-util` |
| SSH | `russh 0.63` with `ring` backend (`aws-lc-rs` banned) |
| Crypto | `ring 0.17`, `base64 0.22` (auth: PBKDF2, JWT) |
| RTSP | `msf-rtsp =0.3.1`, `msf-rtp =0.4.2`, `msf-sdp =0.2.4`, `msf-util =0.1.0`, `ttpkit-auth =0.2.1` (exact pins) |
| ONVIF | `onvif-device-rs =0.8.0` (patched copy in `third_party/onvif-device-rs` via `[patch.crates-io]`), `xsd-parser-types =0.2.1` |
| JPEG | `jpeg-encoder 0.7` (camera) |
| Syscalls | `nix 0.30`, `libc 0.2` |
| Config | `toml 0.9`, `serde`, `serde_json` |
| FFI | `bindgen 0.72` (build-dep of `anyka-sys`; needs clang/libclang — CI installs `clang libclang-dev`) |

Host-only (xtask, own workspace, `xtask/Cargo.toml`): `eyre`, `xsd-parser =1.5.2`, `xsd-parser-types =0.2.1`, `quick-xml 0.38`, `roxmltree 0.21`, `proc-macro2`, `url`.

## Web admin UI (`webui/`)

React 19 + Vite 8 + TypeScript ~6, Tailwind 4, shadcn/radix-ui, react-router 8, echarts 6, sonner, lucide-react. Scripts: `build:camera` = `tsc -b && vite build && node scripts/compress.mjs` (precompressed gzip/brotli). Dev server proxies to `ANYKA_URL` (default `http://127.0.0.1:8080`) — `webui/vite.config.ts`. Node >= 20 required (`xtask/src/tasks.rs` `webui`).

## Docs site (`docs/`)

Next.js 16.3.8 + Fumadocs (core 16.17.1, mdx 15.4.7, base-ui), Tailwind 4, static export to `docs/out`, deployed by `.github/workflows/docs.yml`. Content in `docs/content/docs/*.mdx`; engineering notes in `docs/notes/*.md` (not part of the site build — unverified whether they are rendered).

## Other tooling

- System tests: Python (uv, `itest/pyproject.toml`), pytest + labgrid, QEMU `qemu-system-arm` (versatilepb), `qemu-arm-static` for `./x qemu`.
- `cargo-deny` (`deny.toml`), `git-cliff` (`cliff.toml`, repo `dangerousplay/kamoder`), cosign/Sigstore in CI.
- Camera access helpers: `tools/cam.py`, `tools/push.py`, `tools/camera-access/` (gitignored).

## `./x` commands (`x` -> `xtask/src/main.rs`)

| Command | What |
|---|---|
| `./x build [args]` | `cargo +nightly build --release --workspace` (cross) |
| `./x check` / `./x clippy` | `--workspace --all-targets`; clippy with `-D warnings` |
| `./x fmt [--check]` | rustfmt workspace + xtask |
| `./x test [args]` | host-target `cargo test` over `HOST_TESTABLE` crates |
| `./x gen-onvif [--check]` | regenerate `crates/onvif-schema/src/generated` |
| `./x deny` | `cargo deny --all-features check` |
| `./x webui` | npm ci + `build:camera` into `webui/dist` |
| `./x dist` | build, stage `dist/anyka_rust` (bin, lib/<variant>, lib/sys, www, configs), closure check |
| `./x sd` | dist + strip + assemble `dist/sdcard` (or `$ANYKA_SD_TREE`), preserving `camera.conf etc logs backup metrics` |
| `./x flash` | sd + mount card (label ANYKA) + write (`xtask/src/flash.rs`) |
| `./x itest` | QEMU/labgrid system tests (`xtask/src/itest.rs`) |
| `./x qemu <bin>` | run a staged binary under qemu-arm-static with firmware `/lib` sysroot |
| `./x deploy user@host:dir` | scp `dist/anyka_rust` |
| `./x ci` | fmt --check, gen-onvif --check, clippy, deny, build |
