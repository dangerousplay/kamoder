# Kamoder

**Vision:** General-purpose, vendor-free Rust firmware for Linux IP cameras: one binary set that runs on any supported board, discovers its sensors, and serves RTSP/ONVIF/web.
**For:** owners of cheap IP cameras (Anyka AK39xx first) who want local, open, auditable firmware.
**Solves:** vendor firmware is cloud-bound, closed and board-locked; the current Kamoder code works but bakes AV100 facts (pins, sensors, encoder ABI) into generic services.

## Goals

- One release artifact runs on every supported board; the board is chosen by autodetect or a one-line override (no rebuild).
- Adding a board = a profile file + (if new silicon) one backend crate; zero edits to rtsp/web/onvif/supervisor.
- Generic services build and test on the host against a fake/UVC backend.
- No vendor `.so` at runtime: SDK used only as reverse-engineering reference.

## Tech Stack

- Rust (nightly, `-Z build-std`), target `armv5te-unknown-linux-uclibceabi`; host x86_64 for tests.
- tokio, axum, snafu/eyre, tracing, metrics, clap, serde; msf-* RTSP; onvif-device-rs (patched).
- webui: React + Vite; docs: Next/Fumadocs.
See `.specs/codebase/STACK.md`.

## Scope

**v1 includes:** board profile + autodetect; camera service split into generic core + backends (AV100, generic V4L2); producer protocol consumed by rtsp/web/onvif; lights/wifi/PTZ configured from profile; single packaging for all boards.

**Explicitly out of scope (v1):** audio pipeline; new silicon beyond AV100 implemented end-to-end (EV200/EV300 pure-Rust backend is a later milestone, needs RE + hardware); OTA; cloud features.

## Constraints

- 32 MB RAM, 400 MHz ARM926EJ-S, uClibc.
- `/dev/venc` oopses on memory fragmentation: encoder opened once at boot, never re-created.
- AV100 dual sensors must start together.
- Wi-Fi is often the only access path: never touch the wifi power GPIO outside wifi recovery.
- Public GPL repo, self-contained (no `../` paths).
