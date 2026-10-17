# Follow Movement Tasks

**Design**: `.specs/features/follow-movement/design.md`
**Status**: Done (host-verified; hardware pending) (approved 2026-10-14: separate `follow` service, web toggle promoted to MVP)

## Decisions taken at Tasks time

- D-F1 Grid transport: new unix socket `/tmp/kamoder-motion.sock`, JSON lines, served by `camerad`; client sends `"N\n"` (sensor index) and receives one `MotionGrid` line per sample. Types in `video-proto::motion`. Binary video framing untouched.
- D-F2 Sampling inside the capture loop (`camera::service::run`), via a new default method `Session::motion_grid(slot)`; AV100 reads ISP 3D-NR stats with a new read-back private ioctl. Rate default 10 Hz.
- D-F3 Enable/disable owned by the `follow` daemon: state in `etc/follow.json` (atomic, like `lights/src/store.rs`), control over `/tmp/kamoder-follow.sock` (`follow-proto`, JSON lines). Web proxies `GET/PUT /api/follow` (admin for PUT); 503 `follow_unavailable` when the socket is down.
- D-F4 Manual-override detection: optional `source` on ptz-proto motion requests (default = manual); onvif `Ptz` records the last non-follow command; `Response::Status` gains optional `last_manual_ms_ago`.
- D-F5 Tuning in board profile `[ptz.follow]` (all optional, defaults in code); feature only when the table exists.

## Execution Plan

```
T0 (skeletons, workspace, xtask lists)
 ├─[P]→ T1 → T2 → T3 → T4        grid pipeline (video-proto, anyka-av100, camera, camerad)
 ├─[P]→ T5                       ptz-proto source + onvif last-manual
 ├─[P]→ T6 → T7 → T8             follow-proto, detector, controller (pure)
 └─[P]→ T9                       board [ptz.follow]
T4,T5,T8,T9 → T10                follow runtime + main
T6 → T11 → T12                   web API, webui toggle
T10,T12 → T13                    packaging (supervisor.toml), docs, ./x ci
```

## Task Breakdown

| ID | What | Where | Depends | Done when / Tests |
|---|---|---|---|---|
| T0 | Create `crates/follow-proto` + `crates/follow` skeletons; register in workspace members/deps, `BINARIES`, `HOST_TESTABLE`, `GENERIC_CRATES` | Cargo.toml, xtask | — | `./x check` compiles |
| T1 | `video-proto::motion`: `MotionGrid {seq, ts_ms, cols, rows, max, cells: Vec<u16>}` (+validation cols*rows==len, ≤32×24), `DEFAULT_MOTION_SOCKET`, line encode/decode | crates/video-proto | T0 | unit: round-trip, rejects bad dims (FOL-02) |
| T2 | `v4l2::private_query` (read-back), `conf::GET_3D_NR_STAT = 0x8004_4970`, `Sensor::motion_grid()` parsing `u16[24][32]` at byte 2 of a 0x608 buffer | crates/anyka-av100 | T1 | unit: parse fixture bytes → grid (FOL-01) |
| T3 | `Session::motion_grid(slot) -> Result<Option<MotionGrid>>` default `None`; AV100 + Fake impls; capture loop samples at `--motion-hz` into a per-camera `watch`; errors counted `kamoder_motion_grid_errors_total`, logged once | crates/camera, anyka-av100 | T2 | unit with FakeSession scripted grids (FOL-02, FOL-03) |
| T4 | camerad serves motion socket (0600, ≤8 conns, push latest grid on change) | crates/camerad | T3 | integration test over UnixStream with fake backend |
| T5 | ptz-proto `source: Option<Source>` on Move/Stop/Goto/GotoPreset/Home; `Status.last_manual_ms_ago: Option<u64>`; client `with_source`; onvif `Ptz` records last manual | crates/ptz-proto, crates/onvif | T0 | unit: serde back-compat (old JSON parses), onvif test manual vs follow (FOL-12) |
| T6 | follow-proto: `Request {Status, Set{enabled}}`, `Response {Status{enabled, state, target}, Ok, Error}`, async `Client`, `DEFAULT_SOCKET` | crates/follow-proto | T0 | unit: serde shapes (FOL-16) |
| T7 | Detector: integer baseline, hysteresis, 4-connected components, largest blob, persistence, global-change reject, re-seed | crates/follow/src/detect.rs | T1 | unit on synthetic grids (FOL-04..08) |
| T8 | Controller: pure `decide(state, input, now) -> (state, Action)`; Idle/Tracking/Settling/Returning/Paused; dead zone, proportional clamped step, soft limits, return-home, manual hold | crates/follow/src/control.rs | T7 | unit table tests with time injected (FOL-09..13) |
| T9 | board `[ptz.follow]` optional table (`hz, dead_zone, max_step, decision_frames, settle_ms, return_after_s, manual_hold_s, t_on, t_off, min_cells, max_fraction`) + AV100 profile entry | crates/board, boards/ | T0 | board tests; `./x check-boards` (FOL-14) |
| T10 | follow runtime: store (etc/follow.json), control socket server, motion client, ptz client (source=follow), metrics `kamoder_follow_*`, main (clap, telemetry, SIGTERM) | crates/follow | T4,T5,T8,T9 | tokio tests with fake motion + fake ptz sockets (FOL-15) |
| T11 | web `GET/PUT /api/follow` proxy, `--follow-socket`, state config | crates/web | T6 | api_tests with fake follow socket (FOL-16) |
| T12 | webui: `api.follow/updateFollow`, `FollowCard` switch + state badge on PTZ page | webui | T11 | `./x webui` builds (tsc) |
| T13 | supervisor.toml `[[service]] follow when="ptz"`, web args, docs (`docs/notes`), spec traceability, `./x ci` | tools, docs, .specs | T10,T12 | `./x ci` green |

## Validation

- Granularity: each task = one crate concern ✅ (T3/T10 touch 2 files of one concern).
- Diagram ↔ Depends: matches table ✅.
- Tests co-located per TESTING.md (unit in-module; web via api_tests; no itest — VM has no ISP) ✅.
- Hardware verification (spike T0/T0b in design) remains manual after T13.

## Outcome (2026-10-14)

All T0–T13 implemented. Deviations: `Sensor::motion_grid` returns raw `MotionStat` (conversion in `Av100Session`); `serve_motion` lives in `camera::service`; camera skips the ISP read when no motion subscriber exists; follow closes the motion socket while disabled; runtime e2e tests use the real clock. Grid rate = camerad `--motion-hz` (default 10), board `follow.hz` not yet wired to camerad.
