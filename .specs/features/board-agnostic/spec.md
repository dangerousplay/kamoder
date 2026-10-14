# Board-agnostic Camera Firmware Specification

## Problem Statement

Kamoder's generic services (rtsp, web, onvif, supervisor, lights, wifi) carry AV100 facts in code: GPIO numbers, sensor tuning names, 1280x720@25 ONVIF metadata, `/dev/venc` ABI inside the camera service, `wlan0`, `vs{N}` naming duplicated in four places. The shipped `supervisor.toml` only works on the AV100, so an EV board gets no video at all. Adding a board today means editing generic crates. We want one firmware that runs on any supported Linux camera board and can be extended by data + one backend crate.

## Goals

- [ ] One `dist/` artifact boots on AV100 and on a host/QEMU "generic V4L2" profile with no rebuild; board picked by autodetect or `BOARD=` override.
- [ ] Zero board/sensor/pin literals left in rtsp, web, onvif, supervisor, lights, wifi (enforced by a grep check in `./x ci`).
- [ ] Camera core (service, fan-out, HTTP, video socket, JPEG) builds and tests on the host against a fake backend and a V4L2 backend.
- [ ] No regression on AV100: dual-sensor H.264 RTSP `/vs0` `/vs1`, snapshots, PTZ, lights, wifi recovery still work on the live unit.

## Out of Scope

| Feature | Reason |
| --- | --- |
| EV200/EV300 pure-Rust ISP/encoder backend | Needs RE + hardware; Milestone 2. Spec only requires the seams it will plug into. |
| Audio (AI/AENC) | Separate feature. |
| Removing the SDK crates/demos from the repo | Kept as RE reference; just never shipped in the board-agnostic dist. |
| OTA / flash images per board | Autostart patch stays AV100-specific tooling. |
| Codecs beyond H.264 / JPEG | No consumer needs them yet. |

---

## User Stories

### P1: Board profile + autodetect ⭐ MVP

**User Story**: As a camera owner, I want the firmware to recognise my board and load its hardware description so that I don't rebuild or hand-edit service flags.

**Acceptance Criteria**:

1. WHEN the stack starts THEN it SHALL resolve one board profile: `BOARD=` in `camera.conf` if set, else the first profile whose `match` rules (cpuinfo `Hardware`, device-tree `compatible`/`model`, sysfs probes) match. (BRD-01)
2. WHEN no profile matches THEN it SHALL fall back to the `generic-v4l2` profile and log a warning naming the detected SoC. (BRD-02)
3. A profile SHALL describe: sensors/capture backend + per-sensor settings (resolution, fps, bitrate, GOP), encoder backend, GPIO lines (role, number or sysfs path, active-low), wifi (iface, driver module + args, power GPIO), PTZ (backend, device, limits, steps/degree), firmware paths (tuning dir, vendor processes to stop). (BRD-03)
4. WHEN a profile is invalid (unknown key, missing required field, pin listed twice) THEN loading SHALL fail with a message naming file, key and reason; `./x ci` SHALL validate every shipped profile. (BRD-04)
5. Services SHALL read the resolved profile from one place (a `board` crate + resolved file path passed by supervisor), not re-detect independently. (BRD-05)
6. `web` `/api/hardware` SHALL report the resolved board name and why it matched. (BRD-06)

**Independent Test**: host test with fixture sysfs roots for AV100 and an unknown SoC → AV100 profile and generic fallback resolved; invalid profile fixture → precise error.

---

### P1: Camera core / backend split ⭐ MVP

**User Story**: As a developer adding a board, I want the camera service's generic logic separated from AV100 driver code so that a new board only implements capture + encode.

**Acceptance Criteria**:

1. The camera service SHALL depend on backend traits (capture source yielding frames with format/size/timestamp; H.264 encoder with keyframe request) — no `/dev/venc` or Anyka ioctl in the core crate. (CAM-01)
2. AV100 V4L2+ISP bring-up and `/dev/venc` ABI SHALL live in an `anyka-*` backend crate, selected by the profile. (CAM-02)
3. AV100 constraints SHALL be preserved: encoder + channels opened once at start and never re-created; dual sensors started together. (CAM-03)
4. Stream settings (resolution, fps, bitrate, GOP, profile/level) SHALL come from the board profile, overridable per sensor; no fixed `fps: 15` in code. (CAM-04)
5. The core SHALL have host tests using a fake backend covering: demand-gated snapshot, fan-out to N subscribers, backend failure → `failed` state + retry. (CAM-05)

**Independent Test**: `./x test -p camera` passes on host with fake backend; AV100 build streams both sensors on the live unit.

---

### P1: Producer protocol drives consumers ⭐ MVP

**User Story**: As a viewer using RTSP/ONVIF/web, I want the advertised streams to match what the camera actually produces so that clients connect with correct settings on any board.

**Acceptance Criteria**:

1. The video socket framing and `/status` JSON SHALL be documented (docs/notes) and defined as shared types in a small vendor-free crate used by camera, rtsp, web and onvif. (PRO-01)
2. ONVIF SHALL expose one media profile per sensor with resolution/fps/bitrate/GOP read from the producer's `/status`, not constants. (PRO-02)
3. Stream path naming (`vs{N}`) and RTSP port SHALL be defined once and served to web/webui via API; webui SHALL not hardcode `554` or `vs`. (PRO-03)
4. WHEN the producer is down THEN rtsp SHALL keep retrying and onvif/web SHALL report the stream as unavailable, not crash. (PRO-04)
5. rtsp `SourceSpec` SHALL keep `file:` for tests and accept `camera:N` against any producer speaking the protocol. (PRO-05)

**Independent Test**: host integration: fake-backend camera + rtsp + onvif → `GetProfiles` returns N profiles matching fake settings; retina client receives frames.

---

### P2: Generic V4L2 backend

**User Story**: As a developer without the camera on my desk, I want to run the full stack on my PC with a USB webcam or a test pattern so that I can develop and demo without hardware.

**Acceptance Criteria**:

1. A generic backend SHALL capture from standard V4L2 capture nodes (MMAP, YUYV/NV12/MJPEG) with no vendor ioctls. (CAM-06)
2. Encoding SHALL use V4L2 M2M H.264 when the profile names one, else a software/test path sufficient for host development. (CAM-07)
3. A `test-pattern` source SHALL exist for CI/QEMU (no device needed). (CAM-08)

**Independent Test**: on x86 host, `camera serve --board generic-v4l2` with test-pattern → `ffprobe rtsp://localhost:8554/vs0` succeeds.

---

### P2: Peripherals from profile

**User Story**: As a camera owner on a different board, I want lights, IR-cut, PTZ and wifi recovery to use my board's pins and drivers so that they work without code changes.

**Acceptance Criteria**:

1. lights SHALL build its line map (IR-cut, IR LEDs, white, alarm, amp) from profile roles; roles absent from the profile SHALL be reported as unsupported over lights-proto, and webui SHALL hide them. (PER-01)
2. lights SHALL support both `/sys/class/gpio` numbers and named sysfs paths (EV `/sys/user-gpio/*`) with per-line polarity. (PER-02)
3. The lights allowlist SHALL be exactly the profile's light lines; the wifi power GPIO SHALL never be drivable by lights (load-time check). (PER-03)
4. wifi SHALL take iface, driver module/args and power GPIO from the profile; supervisor readiness SHALL use the profile iface, not `wlan0`. (PER-04)
5. PTZ backend, device path and limits SHALL come from the profile; `Limits::EV200` and device constants removed from generic code. (PER-05)
6. webui SHALL show the backend names the API actually returns (fix `"av100"` doc drift). (PER-06)

**Independent Test**: host tests with fake GPIO root for AV100 and an EV-style profile; lights-proto `capabilities` reflects each profile.

---

### P2: Single packaging

**User Story**: As a release maintainer, I want one SD-card artifact for all boards so that users don't pick the wrong download.

**Acceptance Criteria**:

1. `./x dist` SHALL ship `boards/*.toml` and only vendor-free binaries in the default dist; SDK demos only behind an explicit flag. (PKG-01)
2. supervisor SHALL enable/disable services and fill their args from the resolved profile (e.g. no `ptz` service when profile has no PTZ; one `--stream` per sensor). (PKG-02)
3. `init.sh`/`config.sh` SHALL take vendor process list and mount paths from the profile/camera.conf rather than AV100 literals, keeping the watchdog-guard ordering. (PKG-03)
4. `./x ci` SHALL include a check that fails on new board literals (GPIO numbers, `AV100`, `wlan0`, `/dev/venc`) in generic crates. (PKG-04)

**Independent Test**: `./x dist` then itest QEMU boots with `generic-v4l2` profile and the supervisor starts only applicable services.

---

### P3: EV-ready seams documented

**User Story**: As the next contributor, I want a written "how to add a board" guide so that EV200/EV300 support is a backend + profile job.

**Acceptance Criteria**:

1. docs SHALL contain "Adding a board": profile schema reference, backend trait contract, test checklist. (DOC-01)
2. docs/notes drift fixed (HARDWARE-FINDINGS §1/§5/§9, IMPLEMENTATION crates, architecture.mdx rtsp). (DOC-02)

---

## Edge Cases

- WHEN two profiles match THEN the most specific (most matched rules) SHALL win; ties are a CI validation error.
- WHEN a profile lists 2 sensors but only 1 is detected THEN camera SHALL serve the detected one and report the other `failed` with reason; on AV100 dual-mode SHALL not be attempted with 1 sensor.
- WHEN `BOARD=` names a missing profile THEN startup SHALL fail loudly (supervisor log + web banner), not silently fall back.
- WHEN profile changes stream settings at runtime THEN AV100 backend SHALL require restart of the whole camera service (no encoder re-create), and say so.
- WHEN a GPIO path from profile doesn't exist THEN lights SHALL mark that role unavailable and keep the rest working.

---

## Requirement Traceability

| Requirement ID | Story | Phase | Status |
| --- | --- | --- | --- |
| BRD-01..06 | P1: Board profile | Design | Pending |
| CAM-01..05 | P1: Core/backend split | Design | Pending |
| PRO-01..05 | P1: Producer protocol | Design | Pending |
| CAM-06..08 | P2: Generic V4L2 backend | Design | Pending |
| PER-01..06 | P2: Peripherals from profile | Design | Pending |
| PKG-01..04 | P2: Single packaging | Design | Pending |
| DOC-01..02 | P3: Docs | - | Pending |

**Coverage:** 36 total, 0 mapped to tasks.

---

## Success Criteria

- [ ] Live AV100: RTSP vs0/vs1, snapshot, PTZ, lights, wifi recovery unchanged after refactor.
- [ ] Host: `generic-v4l2` test-pattern stack serves RTSP + ONVIF + web.
- [ ] Board-literal grep check green in `./x ci`.
- [ ] A new board = 1 profile file (+ 1 backend crate if new silicon); no diff in generic crates.
