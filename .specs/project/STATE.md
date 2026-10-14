# State

## Decisions

- 2026-10-13 — Target range: any Linux V4L2 camera. Anyka AV100 + EV200/EV300 first; backend traits must also fit UVC/V4L2-M2M/software encoders.
- 2026-10-13 — Board selection: profile file (`boards/<name>.toml`) + runtime autodetect, override in `camera.conf`. One binary set for all boards.
- 2026-10-13 — No Anyka SDK at runtime. EV boards get pure-Rust reimplementation of the needed libraries; SDK = RE reference only.
- 2026-10-13 — WIP committed as baseline `1ff076e` on `release/0.1.0`.
- Naming (existing): generic crates unprefixed, `anyka-*` only for SDK/hardware crates; metrics `kamoder_*`.

## Blockers

- None.

## Lessons

- AV100 encoder: open once at boot (kernel oops on re-create under fragmentation).
- Unload Strata before heavy builds (RAM contention).

## Todos

- Docs drift: HARDWARE-FINDINGS §1/§5/§9, IMPLEMENTATION.md missing new crates, architecture.mdx says rtsp off.
- `crates/camera/src/venc.rs` references missing `tools/re/venc-protocol`.
- `sdk/v8` provenance (NDA mirror) vs public GPL repo — licensing risk.
- No CI test gate.

## Deferred Ideas

- Audio pipeline, OTA.

## Preferences
