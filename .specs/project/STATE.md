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

- ~~Docs drift: HARDWARE-FINDINGS §1/§5/§9, IMPLEMENTATION.md missing new crates, architecture.mdx says rtsp off.~~ Done 2026-10-13 (board-agnostic T19).
- ~~`crates/camera/src/venc.rs` references missing `tools/re/venc-protocol`.~~ Done 2026-10-13: reference removed from `crates/anyka-av100/src/venc.rs` (traces not in repo; constants are the record).
- `sdk/v8` provenance (NDA mirror) vs public GPL repo — licensing risk.
- CI test gate: partly done — `checks.yml` runs `check-boards` + `check-literals` on push/PR, and `./x ci` includes `./x test`; no workflow runs `./x test`/clippy/deny yet (needs crosstool + nightly build-std).
- ~~Video socket subscribe index is a slot position, not the board sensor index.~~ Done 2026-10-14: `N` is the sensor index everywhere; the producer looks cameras up by `Camera::index` (`docs/notes/VIDEO-PROTOCOL.md` "Which index is which").

## Deferred Ideas

- Audio pipeline, OTA.

## Preferences
