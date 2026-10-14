# Board-agnostic — Tasks

**Design:** `design.md` · **Status:** In Progress (scope: AV100; CAM-06/07 deferred)

Quick gate (per task): `./x test -p <touched crates>` + `cargo check --target x86_64-unknown-linux-gnu -p <touched>` (host, `--all-targets`).
Full gate (T18+): `./x ci` and `./x test`.

## Execution plan

```
Phase 1 (foundation)   T1 → T2 → T3          T4 [P with T1–T3]
Phase 2 (camera)       T4,T3 → T5 → T6 → T8   T7 [P after T5]
Phase 3 (consumers)    T9, T10, T11, T13, T14, T16  (after T3+T4; T10 → T15 sequential: both touch onvif)
Phase 4 (integration)  T12 (after T11,T13) · T17 (after T8,T9,T16) · T18 (after all code) · T19 → T20
```

## Tasks

| ID | What | Where | Depends | Reqs | Done when (tests co-located) |
|---|---|---|---|---|---|
| T1 | `board` crate: profile serde types (`deny_unknown_fields`), `load(path)`, `validate()` | crates/board | – | BRD-03, BRD-04 | unit tests: valid sample, unknown key, duplicate role/GPIO, power_gpio in lights, line with both/neither gpio/path; error names file+key |
| T2 | `board::resolve` (override, scoring, tie, generic fallback) + `detect` from root (cpuinfo, device-tree) + `current()` / `KAMODER_BOARD` handoff (`Resolved` JSON write/read) | crates/board | T1 | BRD-01, BRD-02, BRD-05 | fixture-root tests: AV100 match, unknown SoC → generic + reason, override missing → error, tie → error, handoff roundtrip |
| T3 | `boards/anyka-av100-ptz.toml` + `boards/generic-v4l2.toml` (values = today's AV100 defaults) | boards/ | T2 | BRD-03 | test in board crate loads every `boards/*.toml`, AV100 fixture resolves to av100 |
| T4 [P] | `video-proto` crate: header encode/decode, subscribe line, `Status`/`StreamInfo`, `stream_path`, defaults | crates/video-proto | – | PRO-01, PRO-03 | unit tests: header roundtrip, Status JSON field names identical to current camera `/status` |
| T5 | camera core: `backend.rs` traits, `fake.rs` (test-support feature), refactor `service::capture` onto traits, use video-proto types; temporary in-crate AV100 adapter so bin still works | crates/camera | T4 | CAM-01, CAM-05 | tests w/ fake: demand-gated snapshot, N-subscriber fan-out, IDR on subscribe, backend failure → failed + retry; existing tests pass |
| T6 | Move v4l2/device/conf/venc to new `anyka-av100` crate implementing `Backend`/`Encoder` (open-once encoder, make_room); new `camerad` pkg with `[[bin]] camera` (serve + probe); camera crate becomes lib only | crates/anyka-av100, crates/camerad, crates/camera | T5 | CAM-02, CAM-03 | moved unit tests pass (conf parse etc.); `camera` lib has no `/dev/venc`/ioctl; workspace + xtask HOST_TESTABLE updated |
| T7 [P] | `PatternBackend` moving NV12 test pattern | crates/camera | T5 | CAM-08 (partial) | test: frames change over time, snapshot JPEG decodes size |
| T8 | camerad reads `board::current()`: backend select (`anyka-av100`/`test-pattern`), stream settings + overrides, tuning_dir, snapshot max_width/quality; CLI flags override; unknown backend → clear error | crates/camerad | T3, T6, T7 | CAM-04, BRD-05 | unit tests for settings merge (profile, override, CLI) |
| T9 | rtsp: `CameraSource` uses video-proto; default mounts from `board.video.sensors` when no `--stream` | crates/rtsp | T3, T4 | PRO-05, PKG-02 | tests: header via video-proto, default-mount derivation; existing stream test passes |
| T10 | onvif: profiles per sensor from producer `/status` (fallback: board `video.stream`); remove `VIDEO_*` consts; stream path/port via video-proto; manufacturer/model from board | crates/onvif | T3, T4 | PRO-02, PRO-04 | tests: GetProfiles with 2 fake statuses → 2 profiles with matching res/fps/bitrate; producer down → fallback |
| T11 | web: `/api/hardware.board {name, reason}`; `/api/cameras` `rtsp_url`/`rtsp_port` via video-proto; deserialize video-proto `Status` | crates/web | T3, T4 | BRD-06, PRO-03 | api tests updated |
| T13 | lights: line map + allowlist from `board.lights`; path-addressed GPIO (user-gpio); lights-proto `supported` roles; unsupported role → error reply | crates/lights, crates/lights-proto | T3 | PER-01..03 | tests: AV100 profile fake root, EV-style path profile, missing path → role unavailable, power GPIO rejected |
| T14 | wifi: iface/module/args/power_gpio from board (flags override; no power_gpio → skip rung); supervisor `ready.rs` iface from board | crates/wifi, crates/supervisor | T3 | PER-04 | recovery tests with/without power gpio; ready iface test |
| T15 | anyka-ptz + onvif: PTZ backend/device/module/limits from board; `Limits::EV200` only as sdk default | crates/anyka-ptz, crates/onvif | T10, T3 | PER-05 | ptz tests with profile limits |
| T16 | supervisor: resolve board at start, write `/tmp/kamoder-board.json`, inject `KAMODER_BOARD`; `when = "<cap>"`; `supervisor board [--shell]` subcommand | crates/supervisor | T2 | BRD-05, PKG-02, PKG-03 | config tests for `when`; shell output test |
| T12 | webui: use `rtsp_url`, board name, lights `supported`; drop `554`/`vs`/`wlan0` literals; fix PTZ backend doc | webui/src | T11, T13 | PRO-03, PER-01, PER-06 | `npm run build` + lint pass |
| T17 | tools/supervisor.toml (`when`, profile-driven camera/rtsp args), init.sh (`eval supervisor board --shell`), camera.conf.example `BOARD=` | tools/ | T8, T9, T16 | PKG-02, PKG-03 | `supervisor --check`/config parse test on shipped toml; `sh -n init.sh` |
| T18 | xtask: dist ships `boards/`, `--with-demos`, BINARIES (camerad bin), `check-boards`, `check-literals` in `./x ci` | xtask | all code | PKG-01, PKG-04, BRD-04 | `./x ci` green, `./x test` green |
| T19 | docs: `docs/notes/VIDEO-PROTOCOL.md`, docs site "Adding a board", drift fixes, README crate table | docs/, README.md | T18 | DOC-01, DOC-02, PRO-01 | links resolve; docs build |
| T20 | Live AV100 verification: deploy dist, check vs0/vs1 RTSP, snapshots, ONVIF 2 profiles, PTZ, lights, wifi | camera | T19 | success criteria | manual checklist recorded in HARDWARE-FINDINGS |

## Validation

**Granularity:** each task = one crate (T6/T13/T14/T15 touch two tightly-coupled crates, accepted: move + its consumer must compile together). ✅

**Diagram ↔ Depends cross-check:**

| Task | Depends field | In diagram | ✅ |
|---|---|---|---|
| T2 | T1 | Phase 1 | ✅ |
| T3 | T2 | Phase 1 | ✅ |
| T5 | T4 | Phase 2 | ✅ |
| T6 | T5 | Phase 2 | ✅ |
| T7 | T5 | Phase 2 [P] | ✅ |
| T8 | T3,T6,T7 | Phase 2 | ✅ |
| T9–T14,T16 | T3/T4/T2 | Phase 3 | ✅ |
| T15 | T10,T3 | Phase 3 seq | ✅ |
| T12,T17,T18,T19,T20 | as listed | Phase 4 | ✅ |

**Test co-location:** every code task lists host unit tests (TESTING.md: inline `#[cfg(test)]`, fixture roots, fakes); T12 webui build/lint; T17 config parse; T20 manual (no hardware in itest). ✅

## Traceability

BRD-01..06 → T1,T2,T3,T11,T16 · CAM-01..05 → T5,T6,T8 · CAM-08 → T7 · PRO-01..05 → T4,T9,T10,T11 · PER-01..06 → T12,T13,T14,T15 · PKG-01..04 → T16,T17,T18 · DOC → T19. CAM-06/07 deferred.

| Task | Status |
|---|---|
| T1–T20 | Pending |
