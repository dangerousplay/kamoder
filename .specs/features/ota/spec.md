# Safe A/B Auto-Updater (OTA) Specification

## Problem Statement

Updating a Kamoder camera today means copying files over ssh (`swap.sh`: rename `*.new` over the running tree) or re-imaging the SD card. Nothing verifies what is installed, a bad build leaves the camera unreachable, and large pushes knock the Wi-Fi off. Owners need signed, resumable updates that install beside the running system and roll back on their own when the new version does not come up healthy. The design must survive a later move from "firmware on the SD card" to a custom firmware on re-partitioned MTD flash.

## Goals

- [ ] A release published on GitHub reaches a camera (stable or beta channel) and is installed with one click in the web UI; a manual upload works offline.
- [ ] The running version is never modified: installs go to the inactive slot; switching is one power-safe step; power loss at any moment leaves a bootable camera.
- [ ] A new version that does not prove healthy within the trial window is rolled back automatically (no human, no UART), including when it hangs or crash-loops.
- [ ] Only updates signed with the project key are accepted; downgrades below the security floor are refused.
- [ ] Storage and boot-control are pluggable: SD directories + selector files now; MTD partitions (located by name, so repartitioning is allowed) + U-Boot env later, without changing the updater core or the update format.

## Out of Scope

| Feature | Reason |
| --- | --- |
| Delta/binary-diff updates | Stale crates, RAM cost; resumable download covers flaky Wi-Fi. Revisit if bandwidth matters. |
| Writing MTD on the current AV100 | 8 MB NOR is full (no room for a second kernel/rootfs); mtd6 hook stays as is. MTD backend is built and host-tested only. |
| Changing U-Boot / bootcmd now | No bootcount in this U-Boot; risky without a recovery path. Future firmware. |
| On-device Sigstore verification | Experimental/heavy crate; Sigstore stays a CI/host check. Device trusts a pinned minisign key. |
| Fleet management / server push | Cameras poll; no backend service. |

---

## User Stories

### P1: Signed update manifest and package ⭐ MVP

**User Story**: As the project, I want every release to carry a signed manifest describing a verifiable update package so cameras only install genuine, compatible builds.

1. Release CI SHALL produce `kamoder-<ver>-update.kup` (an uncompressed tar holding `manifest.json`, `manifest.json.minisig`, and the payload) plus `manifest.json`/`.minisig` as separate assets, signed with the project minisign key; Sigstore signing of all assets stays. (OTA-01)
2. The manifest SHALL state schema, version, channel, monotonic `seq`, `security_floor`, compatible boards/target, `min_updater`, and per-component entries (`kind`, size, sha256 of the payload and of every file). (OTA-02)
3. The device SHALL verify the minisign signature against keys compiled into the updater (current + next, for rotation) before trusting any field. (OTA-03)
4. The device SHALL refuse a manifest whose board does not match, whose `seq` is lower than the highest seen (unless an admin forces a reinstall of the same seq), or whose version is below `security_floor`. (OTA-04)

**Independent Test**: tampered manifest, wrong key, wrong board, old seq — each refused with a clear reason; a good `.kup` verifies.

### P1: A/B slots with power-safe switch ⭐ MVP

1. The SD layout SHALL have two slot directories and shared data (camera.conf, etc/, logs/, backup/, metrics/) outside both; services SHALL read data from the shared location and code from the slot. (OTA-05)
2. Install SHALL write only the inactive slot: stream-extract, verify every file's sha256, fsync, then write a per-slot `VERSION`/manifest copy; a failed or interrupted install leaves the slot marked invalid and the active slot untouched. (OTA-06)
3. Boot control SHALL keep per-slot state (bootable, successful, tries left, version) in two redundant fixed-size records with sequence number + CRC32, written alternately, so a torn write falls back to the other copy. (OTA-07)
4. Activating a slot SHALL set it trial (tries = 3, unsuccessful) as one record write; the old slot stays bootable. (OTA-08)

### P1: Boot selection and automatic rollback ⭐ MVP

1. A small boot shim outside the slots (`Factory/config.sh` + static `kboot` binary) SHALL choose the slot at boot: highest-priority bootable slot; a trial slot consumes one try per boot; with no tries left and not successful it SHALL be marked unbootable and the other slot booted. (OTA-09)
2. The shim SHALL arm a trial guard: if the slot is not committed within the trial window (default 10 min) the camera SHALL reboot (consuming a try); hangs are covered by the hardware watchdog. (OTA-10)
3. The new slot SHALL commit itself (mark successful) only after a health check: all critical/enabled supervisor services running for ≥ 3 min, web answering, updater able to verify its own manifest. (OTA-11)
4. WHEN no slot layout exists (legacy `anyka_rust/` tree) THEN the shim SHALL boot it as before; the first update SHALL migrate it to slot A without losing data. (OTA-12)
5. WHEN `kboot` is missing or fails THEN `config.sh` SHALL fall back to the last known slot directory, then the legacy tree. (OTA-13)

### P1: Updater service and web UI ⭐ MVP

1. An `update` service SHALL check the selected channel (stable = latest release, beta = newest pre-release) on start and every 12 h (≤ 60 GitHub API calls/h shared limit), only after the clock is NTP-valid. (OTA-14)
2. WHEN a newer verified manifest exists THEN it SHALL download the `.kup` in the background with HTTP Range resume, bandwidth-friendly chunks, hashing while streaming, into shared storage; the UI SHALL show "Update available vX (downloaded)". (OTA-15)
3. WHEN an admin clicks Install THEN the updater SHALL install to the inactive slot, activate it as trial and reboot; progress and errors SHALL be visible in the UI. (OTA-16)
4. Admin SHALL upload a `.kup` from the web UI (offline cameras); same verification path. (OTA-17)
5. Settings: channel (stable/beta), auto-check on/off, auto-install opt-in with maintenance window (default off). (OTA-18)
6. The UI SHALL show both slots (version, state, last boot result) and allow "roll back to previous" when the other slot is bootable. (OTA-19)

### P2: Future storage: MTD partitions and U-Boot env

1. A slot backend SHALL write raw images to MTD partitions found by name in `/proc/mtd` (never by index): erase, write, read-back verify, using MTD ioctls via `nix`. (OTA-20)
2. A boot-control backend SHALL read/write a (redundant) U-Boot environment (CRC32, flags byte) for `boot_slot`, `bootcount`, `upgrade_available`-style variables. (OTA-21)
3. The manifest component kinds SHALL include `sd-tree` and `mtd-image` (target partition name), so one update format serves both. Board profile `[update]` selects backends. (OTA-22)

**Independent Test**: host tests with file-backed MTD and env images; no AV100 flash writes.

---

## Edge Cases

- Power loss during download → resume from `.part` (re-hash prefix).
- Power loss during install → inactive slot invalid; active slot boots; UI shows "install interrupted".
- Power loss during selector write → older record valid; boots previous state.
- Wi-Fi drop mid-download → backoff retry, resume; never blocks services.
- Clock not set (no NTP) → no checks; manual upload still works (expiry not enforced, seq still enforced).
- SD nearly full (< 2× package) → refuse download with reason.
- New version breaks the updater itself → health check fails (OTA-11) → rollback.
- Both slots unbootable (should not happen) → boot the one with the higher version anyway and log loudly.
- Update while a talk/stream is active → install does not stop services; reboot only after install completes.

---

## Requirement Traceability

| ID | Story | Status |
| --- | --- | --- |
| OTA-01..04 | P1 Signed manifest | Implemented (`ota-manifest`, `./x kup`, release.yml minisign); production key not yet generated (placeholder → every manifest refused) |
| OTA-05..08 | P1 A/B slots | Implemented (`bootctl` SdSelector, `ota::sd::SdDirStore`, supervisor `{slot}`/`--data-dir`) |
| OTA-09..13 | P1 Boot + rollback | Implemented (`kboot`, `card/Factory/config.sh`); scripts tested under the camera's BusyBox in qemu; not yet on hardware |
| OTA-14..19 | P1 Updater + UI | Implemented (`update` service, `fetch`, web `/api/update*`, webui Updates page) |
| OTA-20..22 | P2 MTD + U-Boot env | Implemented, host-tested only (`ota::mtd`, `bootctl::uboot`) |
