# Roadmap

## Milestone 1 — Board-agnostic core (current)

Feature spec: `.specs/features/board-agnostic/spec.md`

| # | Feature slice | Req IDs | Status |
|---|---|---|---|
| 1 | Board profile format, loader, autodetect | BRD-01..06 | Pending |
| 2 | Camera core/backend split (AV100 backend extracted) | CAM-01..05 | Pending |
| 3 | Generic V4L2 backend (UVC/host dev, software or M2M encode) | CAM-06..08 | Pending |
| 4 | Producer protocol: documented, consumed for metadata (onvif/web/rtsp) | PRO-01..05 | Pending |
| 5 | Peripherals from profile: lights, wifi, PTZ | PER-01..06 | Pending |
| 6 | Packaging: one dist, profile-driven service config | PKG-01..04 | Pending |

## Feature — Follow movement (PTZ auto-tracking)

Spec: `.specs/features/follow-movement/spec.md` (FOL-01..18). Status: P1 + web toggle implemented and host-tested (2026-10-14); hardware spikes T0 (MD grid ioctl) and T0b (first PTZ move) + threshold/FOV tuning pending.

## Milestone 2 — More silicon

- EV200/EV300 pure-Rust backend (ISP + encoder RE from SDK as reference). Needs hardware.
- Audio on EV200/EV300 (AV100 audio done: `.specs/features/audio`).

## Milestone 3 — Hardening

- CI test gate (`./x ci` + `./x test` in workflows).
- Memory budget measurement with camera + rtsp.
