# Follow Movement (PTZ auto-tracking) Specification

## Problem Statement

The AV100 PTZ camera can pan/tilt but only on command. Owners want it to follow a moving subject (person, pet, car) and keep it in frame, as the stock `ipc` app and commercial cams (Wyze Pan, Tapo) do. It must fit the 400 MHz ARM926EJ-S (soft-float, no NEON) and the ~14 MB of free RAM left beside the running services, with no vendor SDK at runtime.

## Goals

- [ ] Subject kept inside the central 60 % of the frame while it moves at walking pace across the pan range.
- [ ] Tracking adds ≤ 5 % CPU and ≤ 1 MB RSS at 10 Hz on the AV100 (measured with `kamoder_*` metrics / `top`).
- [ ] No false chase from the camera's own movement, noise or lighting changes (no move without a real subject in ≥ 95 % of a 1 h idle test).
- [ ] Pure Rust, board-agnostic core; AV100 first, EV200/EV300 via the same grid interface.

## Out of Scope

| Feature | Reason |
| --- | --- |
| Person/object classification (CNN, HOG) | No NPU runtime on AV100 (`/dev/npu` absent, models missing); a 96×96 CNN is ~1 fps at best on ARM926. Revisit as P3 crop check only. |
| Encoder motion vectors | Anyka venc exposes no MV/SAD/per-MB QP output (checked AV100 `venc.rs`, EV200 `ak_venc.h`). |
| Vendor `libak_mt.so` / `libmpi_md.so` | No SDK at runtime (STATE decision 2026-10-13); decompiled `libak_mt` is RE reference only. |
| Optical zoom / multi-target selection UI | Hardware has no zoom; P1 follows the largest blob. |
| Recording/alerts on motion | Separate feature (ONVIF events, notifications). |

---

## User Stories

### P1: Motion grid from the ISP ⭐ MVP

**User Story**: As the tracker, I want the ISP's 32×24 motion grid at ≥ 10 Hz for near-zero CPU so that detection costs no per-pixel work.

**Acceptance Criteria**:

1. WHEN the camera service is streaming THEN it SHALL read the 3D-NR motion statistics (ISP private cmd `0x8004_4970`, 0x608-byte buffer, `u16[24][32]` at offset 2) from `/dev/isp-param-N` without the vendor SDK. (FOL-01)
2. WHEN read THEN the grid SHALL be published to consumers at a configurable rate (default 10 Hz, max 25 Hz) with a sequence number and the monotonic timestamp. (FOL-02)
3. WHEN the ioctl fails or is unsupported on a board THEN the service SHALL log once, expose `kamoder_motion_grid_errors_total`, and keep streaming. (FOL-03)

**Independent Test**: Wave a hand in front of the lens; the published grid's hot cells move with the hand (host tool prints an ASCII heat map).

### P1: Subject detection on the grid ⭐ MVP

1. WHEN a grid arrives THEN cells SHALL be thresholded with hysteresis (on > T_on, off < T_off) against a slow integer running baseline (`bg += (cur - bg) >> k`). (FOL-04)
2. THEN 4-connected components SHALL be found and the largest blob above `min_cells` kept; its centroid and bounding box reported in grid coordinates. (FOL-05)
3. WHEN a blob persists for `decision_frames` (default 5) consecutive grids THEN it SHALL be a target; single-frame blips SHALL be ignored. (FOL-06)
4. WHEN more than `max_fraction` (default 60 %) of cells are hot THEN the frame SHALL be treated as a global change (lighting, IR switch) and ignored, and the baseline re-seeded. (FOL-07)
5. All per-cell math SHALL be integer; no `f32` in the per-grid path. (FOL-08)

**Independent Test**: Host unit tests replay recorded grids (fixtures captured on the AV100) and assert target/no-target and centroid.

### P1: PTZ follow control ⭐ MVP

1. WHEN a target's centroid lies outside the dead zone (default central 30 % horizontally, 40 % vertically) THEN the tracker SHALL command a relative move toward it over the existing `ptz` socket, step count proportional to the offset and clamped to `max_step`. (FOL-09)
2. WHILE the motors run (`Position.moving`) and for `settle_ms` after (default 300 ms) THEN detection SHALL be suspended, and on resume the baseline SHALL be re-seeded from the first grid. (FOL-10)
3. WHEN a target is lost for `return_after` (default 15 s) THEN the camera SHALL return to the configured home preset. (FOL-11)
4. WHEN a user/ONVIF client issues a manual PTZ command THEN tracking SHALL pause for `manual_hold` (default 60 s). (FOL-12)
5. Soft limits from the board profile SHALL never be exceeded; at a limit the axis stops and tracking continues on the other. (FOL-13)

**Independent Test**: Walk across the room; camera follows and returns home 15 s after leaving.

### P1: Configuration and enable switch ⭐ MVP

1. `FOLLOW_ENABLED=0|1` in `camera.conf` (default 0) and the tuning keys above SHALL be read at start; the board profile SHALL declare `[ptz] follow = true` only for boards that support it. (FOL-14)
2. Metrics: `kamoder_follow_state` (idle/tracking/settling/returning/paused), `kamoder_follow_moves_total`, `kamoder_follow_targets_total`. (FOL-15)

### P2: Web UI control

1. WHEN the admin toggles "Follow movement" in the web UI THEN tracking SHALL start/stop without restarting services, and the live view SHALL overlay the hot cells and target box. (FOL-16)

### P2: ONVIF motion events

1. WHEN a target is acquired/lost THEN an ONVIF `tns1:VideoSource/MotionAlarm` event SHALL be raised (needs the Events service). (FOL-17)

### P3: Person check on crop

1. WHEN a target is acquired THEN an optional low-rate (≤ 1 Hz) classifier on a 96×96 luma crop MAY gate following to people only. Only if measured CPU allows. (FOL-18)

---

## Edge Cases

- WHEN IR cut / day-night switches THEN FOL-07 global-change handling SHALL suppress a chase.
- WHEN the subject leaves past a soft limit THEN the camera SHALL hold at the limit, then return home after `return_after`.
- WHEN the ptz socket is unavailable (onvif down) THEN the tracker SHALL keep detecting, retry with backoff, and not crash.
- WHEN two subjects move THEN the largest blob wins; ties keep the current target (nearest centroid).
- WHEN free RAM < 2 MB THEN tracking SHALL not be the process that grows (fixed-size buffers only).

---

## Requirement Traceability

| ID | Story | Status |
| --- | --- | --- |
| FOL-01..03 | P1 Motion grid | Implemented (anyka-av100 `GET_3D_NR_STAT`/`Sensor::motion_grid`, camera sampler, camerad `/tmp/kamoder-motion.sock`); ioctl unverified on hardware |
| FOL-04..08 | P1 Detection | Implemented (`crates/follow/src/detect.rs`); thresholds untuned |
| FOL-09..13 | P1 Control | Implemented (`crates/follow/src/control.rs`, runtime; ptz-proto `source`, onvif last-manual); PTZ never moved on hardware |
| FOL-14..15 | P1 Config | Implemented (board `[ptz.follow]`, `etc/follow.json`, `FOLLOW_ENABLED` seed, `kamoder_follow_*`) |
| FOL-16 | Web toggle (promoted to P1) | Implemented (`/api/follow`, PTZ page `FollowCard`); overlay of hot cells deferred |
| FOL-17 | P2 ONVIF events | Pending |
| FOL-18 | P3 Person check | Deferred |
