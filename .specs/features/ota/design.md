# OTA — Design

Status: **draft** (spec: `spec.md`, decisions: `context.md`). Pattern: Android/RAUC A/B state machine (bootable / successful / tries), redundant records like U-Boot redundant env, TUF-style monotonic `seq` + security floor, minisign signatures.

## 1. SD layout (backend `sd-dirs`)

```
/mnt/disc1/
├── Factory/config.sh            boot shim (stable, rarely updated)
└── anyka_rust/                  DATA ROOT (also the legacy slot)
    ├── camera.conf  etc/  logs/  backup/  metrics/  media/     shared data
    ├── boot/
    │   ├── sel.0  sel.1         redundant selector records (512 B each)
    │   ├── last                 text: last slot name kboot selected (shell fallback)
    │   ├── bin/kboot            boot helper (+ lib/sys/ it needs), outside slots
    │   └── lib/sys/
    ├── slots/a/  slots/b/       bin/ lib/ www/ boards/ supervisor.toml init.sh VERSION manifest.json(.minisig)
    └── update/                  downloads (*.part, *.kup), state.json, upload.kup
```

- **D-O1 Data vs code.** Services run with cwd = data root (all data args stay relative: `etc/…`, `camera.conf`, `logs`). Code paths use a `{slot}` placeholder the supervisor expands from `--slot-dir` (`command = "{slot}/bin/web"`, `--www {slot}/www`). Legacy tree: slot dir = data root = `.` → identical behaviour.
- **D-O2 Migration (OTA-12).** First install on a legacy tree writes slot **b**, then records slot **a** = "legacy" pointing at the root tree (`slots/a` absent ⇒ kboot maps slot a to the data root). Once b is committed, the next update replaces the legacy tree by writing `slots/a` and the root `bin/ lib/ www/ boards/` are left (cleanup task later, P3).

## 2. Boot control

- **D-O3 Selector record** (`bootctl` crate), fixed 512 B, little-endian: `magic "KSEL"`, `format u16`, `seq u32`, 2 × slot `{bootable u8, successful u8, tries_left u8, priority u8, version [u8; 32]}`, `last_booted u8`, reserved, `crc32` (crc32fast). Writer overwrites the copy with the lower `seq` (same size, in place, `fsync` file + dir). Reader picks the highest valid seq; both invalid ⇒ legacy/default (slot a bootable successful).
- **Selection** (pure fn `choose(&Record) -> (slot, Record')`): highest priority among bootable; if not successful: tries_left==0 ⇒ mark unbootable, choose other; else tries_left -= 1. Written before boot continues.
- **`BootControl` trait**: `load()`, `store(Record)`, `mark_successful(slot)`, `activate(slot, version, tries)`. Impls: `SdSelector` (P1), `UbootEnv` (P2: own codec for single/redundant env — CRC32 LE + flags byte; vars `kamoder_slot`, `kamoder_tries_a/b`, `kamoder_ok_a/b`), `Memory` (tests).

## 3. Boot shim (`kboot` + `config.sh`)

- `kboot select --root R` prints shell assignments `KSLOT=a KSLOT_DIR=/…/slots/a KTRIAL=1` and updates the record + `boot/last`. Small (no tokio), dynamic like every binary, own `boot/lib/sys` copy so it never depends on a slot.
- `kboot guard --root R --window 600` (backgrounded by config.sh when trial): polls the record every 10 s; exits when the slot is successful; at the deadline `sync` + `reboot(RB_AUTOBOOT)` (OTA-10). Hangs: watchdog (feeder) reboots.
- `config.sh`: `eval "$(kboot select …)"` → else `KSLOT_DIR=$(cat boot/last)` → else legacy root. Runs `sh $KSLOT_DIR/init.sh` with `KAMODER_DATA=$RUST KAMODER_SLOT=$KSLOT_DIR`. `init.sh` passes `--slot-dir` to the supervisor and uses `$KAMODER_DATA` for camera.conf/etc (defaults keep legacy behaviour).

## 4. Update package and manifest

- **`.kup`** = uncompressed tar: `manifest.json`, `manifest.json.minisig`, `payload.tar.gz` (slot tree, gzip via flate2/miniz_oxide — 32 KiB window, pure Rust), optional `boot.tar.gz` (kboot + lib/sys + config.sh).
- **manifest.json** (`ota-manifest` crate, serde):
  `{schema:1, version, channel, seq (u64, = tag commit unix time), security_floor, min_updater, boards:[...], target, created, components:[{name:"slot", kind:"sd-tree", path:"payload.tar.gz", size, sha256, files:[{path,size,sha256,mode}]}, {name:"boot", kind:"boot-shim", …}, {name:"kernel-a", kind:"mtd-image", partition:"KERNEL_A", …}]}`.
- **Signature**: minisign (`minisign-verify` 0.3, zero deps). Public keys `crates/ota-manifest/keys/*.pub` (current + next) compiled in; trusted comment carries version+seq. Test key under `keys/test/` only enabled in `cfg(test)`.
- **Policy** (`verify_and_check`): signature → schema → board ∈ boards → `seq > highest_seen` (or == with `force`) → `version >= security_floor` → `min_updater <= self`.

## 5. Slot stores

- **`SlotStore` trait**: `inactive()`, `begin(slot)` (mark invalid, clear), `install_component(reader, &Component)`, `finish(slot, manifest)`.
- **`SdDirStore`**: stream payload.tar.gz → tar entries → write files into `slots/<x>.tmp/`, sha256 each against manifest (reject unknown/extra files, path traversal, symlinks), fsync files+dirs, then rename `slots/<x>.tmp` → `slots/<x>` after removing the old dir (slot already invalid in the record, so non-atomic rename is safe).
- **`MtdStore`** (P2): partition by name from `/proc/mtd`; `MEMGETINFO`, `MEMUNLOCK`, `MEMERASE` per erase block, write, read-back sha256; via `nix::ioctl_*` macros. Host tests against a file-backed fake implementing the same `Flash` trait.
- **Boot-shim component**: write `boot/bin/kboot.new`, `Factory/config.sh.new`, fsync, rename; only when hash differs; version recorded.

## 6. `update` service + protocol

- Crates: `update-proto` (JSON lines over `/tmp/kamoder-update.sock`: `Status`, `Check`, `Download`, `Install{source: Downloaded|Upload{path}, force}`, `Rollback`, `Settings{…}`, `Commit` (debug)), `ota` (lib: manifest policy, stores, download, github, health), bin `update`.
- **Discovery**: stable → `GET https://github.com/<repo>/releases/latest/download/manifest.json` (+ `.minisig`), beta → `GET https://api.github.com/repos/<repo>/releases?per_page=10` newest prerelease asset URLs. Repo from `camera.conf` `UPDATE_REPO` (default `dangerousplay/kamoder`). Every 12 h + on demand; only when `os::time` clock is valid.
- **HTTP**: hyper 1 client (`hyper-util` `client-legacy`) + `hyper-rustls` (ring, webpki-roots, http1 only), redirect follow ≤ 5, Range resume to `update/<name>.part`, 64 KiB chunks, sha256 streaming (ring::digest), rate cap option (default 256 KiB/s) to protect the Wi-Fi.
- **Health/commit (OTA-11)**: when booted slot is trial: query supervisor control socket every 15 s; all enabled services `running` continuously 180 s + `GET http://127.0.0.1/` 200 + verify own slot manifest ⇒ `mark_successful` (guard then exits). Unhealthy at 9 min ⇒ log reason, let the guard reboot.
- **Install**: stop nothing; verify `.kup` → `SlotStore` install → `BootControl::activate(inactive, tries 3, priority above current)` → sync → reboot (unless "install, reboot later").
- **State** `update/state.json` (atomic like lights store): settings, highest_seq, last check, downloaded version, last error.
- Metrics `kamoder_update_*`.

## 7. Web + UI

- web: `GET /api/update` (status incl. slots, versions, available, progress), `POST /api/update/check`, `POST /api/update/install {force?}`, `POST /api/update/rollback`, `PUT /api/update/settings`, `POST /api/update/upload` (admin, streamed body to `update/upload.kup`, 64 MiB limit on that route only).
- webui: Settings → **Updates** page: current version/slot, other slot, channel select, auto-check/auto-install + window, Check now, release notes link, Install, Upload `.kup`, Roll back; progress polling.

## 8. Release pipeline

- `xtask kup` (host): builds payload.tar.gz from `dist/` slot tree (reproducible: sorted, mtime=SOURCE_DATE_EPOCH, uid 0), manifest with per-file sha256, boards from `boards/*.toml`, seq = commit time; signs with `minisign` CLI (or `MINISIGN_SECRET_KEY`) → `.kup` + manifest + sig. `package-release.sh` calls it; release.yml uploads them (cosign still signs everything). Version injected: `KAMODER_VERSION` env → slot `VERSION` file.

## 9. Supervisor/init changes

- supervisor: `--slot-dir <path>` (default = config dir), expand `{slot}` in `command`/`args`; `supervisor.toml` uses `{slot}/bin/…`, `{slot}/www`, boards dir `{slot}/boards`; cwd for services = data root (`--data-dir`, default = config dir).
- init.sh: `KAMODER_DATA`/`KAMODER_SLOT` with legacy defaults; `--stop` unchanged.
- New service `update` in supervisor.toml (always, no `when`).

## 10. Risks

| Risk | Mitigation |
| --- | --- |
| Bug in config.sh/kboot bricks boot | shell fallbacks (boot/last → legacy), telnetd rescue stays before init, mtd5 vendor recovery, UART; host tests for `choose`; kboot tiny |
| vfat corruption on power loss | redundant records + CRC; slots written only when invalid; fsync |
| RAM (download + extract) | streaming, 64 KiB buffers, gzip 32 KiB window; files go to SD not tmpfs |
| TLS cert validation needs time | checks wait for NTP-valid clock |
| Web binary size (+hyper client in update only) | client lives in `update` bin, not web |
| Signing key custody | minisign secret in GitHub Actions secret; user generates; next-key slot for rotation |
