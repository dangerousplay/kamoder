# Testing — Kamoder

## Tiers

| Tier | Command | Runs on | Covers |
|---|---|---|---|
| Unit + crate integration | `./x test` | host triple (x86_64), not cross | logic-only tests of `HOST_TESTABLE` crates |
| Static gate | `./x ci` | host, cross build | fmt --check, gen-onvif --check, clippy -D warnings, cargo-deny, full ARM build |
| System (simulated camera) | `./x itest [--no-build] [-- pytest args]` | QEMU ARM926 VM (labgrid) | whole SD card boot, services end to end (~10 min) |
| Single binary under emulation | `./x qemu <binary> [args]` | qemu-arm-static, firmware `/lib` sysroot | smoke run of staged ARM binary |
| Real hardware | manual (`tools/camera-access/e2e.py`, `onvif_live.py`, `wfr-test.sh`, gitignored) | camera | not automated; results recorded in `docs/notes/HARDWARE-FINDINGS.md` |

## `./x test` details (`xtask/src/tasks.rs` `test`)

- Runs `cargo +$ANYKA_RUST_TOOLCHAIN test --target <host> -p <each HOST_TESTABLE>`; passing `-p`/`--package` overrides the list.
- `HOST_TESTABLE`: os, telemetry, supervisor, auth, anyka-ptz, ptz-proto, lights-proto, lights, onvif-schema, sshd, web, onvif, rtsp, anyka-v8, wifi, anyka-itest-support, camera.
- Excluded (link ARM SDK objects or are SDK helpers): anyka-sys, anyka, anyka-link and every `demos/*` crate (including the SDK-free `wdt_feeder`) — none of these is host-tested.
- Note: `build-std` stays configured from `.cargo/config.toml` for this invocation (unlike `./x` itself); not verified whether that matters for host tests.
- Not run during this mapping (skipped to avoid a long compile).

## Gate commands (exact)

```sh
./x fmt --check
./x gen-onvif --check
./x clippy             # cargo +nightly clippy --workspace --all-targets -- -D warnings  (cross target)
./x deny               # cargo deny --all-features check
./x build              # cargo +nightly build --release --workspace
./x test               # host unit/integration tests
./x ci                 # = fmt --check, gen-onvif --check, clippy, deny, build (does NOT run ./x test)
./x itest              # system tests (needs qemu-system-arm, fakeroot, cpio, sfdisk, mkfs.vfat, mcopy, sshpass, uv, VM images)
```

No GitHub workflow runs `./x ci`, `./x test` or `./x itest`; `release.yml` only builds (`./x webui`, `./x dist`). Gates are local only.

## Test layout and patterns

- Inline `#[cfg(test)] mod tests` in most modules (list via `grep -rl '#\[cfg(test)\]' crates`).
- Crate integration tests: `crates/onvif-schema/tests/xml.rs` (XML round-trips), `crates/rtsp/tests/stream.rs` (DESCRIBE/SETUP/PLAY with `retina` client over `tests/data/clip.h264`), `crates/sshd/tests/streaming.rs` (streamed upload, SHA-256 via ring).
- Web: router driven without a socket via `tower::ServiceExt` (`crates/web/src/api_tests.rs`, `crates/web/src/tests.rs`; `web` is a lib for this reason).
- Async tests: `#[tokio::test(flavor = "current_thread")]`, `start_paused = true` + `tokio test-util` for time-dependent logic (`crates/lights/src/owner.rs`, camera).
- Fakes / injected hardware:
  - `FakeDriver`/`FakeDevice` for `/dev/preset_motor`, time advanced explicitly by `tick` (`crates/anyka-ptz/src/preset_motor/fake.rs`).
  - `Sim` PTZ backend (also used by itest via `ANYKA_ONVIF_PTZ=sim`) (`crates/anyka-ptz/src/sim.rs`).
  - `TempDir` helper (hand-rolled, no tempfile crate) (`crates/anyka-ptz/src/test_support.rs`).
  - GPIO sysfs under a configurable root so tests and the VM use a fake tree (`crates/lights/src/gpio.rs` `DEFAULT_ROOT`, `--gpio-root`).
  - Supervisor hardware probe reads paths relative to `root` for fixtures (`crates/supervisor/src/hardware.rs`).
  - Wi-Fi policy as pure function `reconcile::decide` with time passed in; recovery sequences behind a trait (`crates/wifi/src/reconcile.rs`, `recovery.rs`).
  - RTSP `FileSource` replays Annex-B file instead of `CameraSource` (`crates/rtsp/src/source.rs`).
- ONVIF tests compute PasswordDigest client-side (`sha1`, `base64` dev-deps in `crates/onvif/Cargo.toml`).

## System tests (`itest/`)

- pytest + labgrid (`itest/env/qemu.yaml`, `QEMUDriver` + `ShellDriver`), Python deps via `uv` (`itest/pyproject.toml`, `uv.lock`).
- VM: versatilepb ARM926, Linux 4.4.282, the camera's own rootfs; SD card as FAT32 `/dev/mmcblk0p1`; JFFS2 `/rom` on mtdram; `softdog` watchdog; `mac80211_hwsim` radio + two hostapd APs (HomeNet, NewNet) in netns via `itest/support` `wiphy-netns`. Built by `tools/qemu-armv5/run.sh` (`extract`, `rootfs`, `build`) into `tools/qemu-armv5/out` (`ANYKA_VM_DIR`).
- Scenarios (run in file order on one booted VM): `test_01_devices`, `02_boot`, `03_ssh`, `04_watchdog`, `05_wifi`, `06_runtime`, `07_web`, `08_onvif`, `09_shutdown`. `vm`-marked scenarios are VM-only.
- Throwaway passwords `LG_ITEST_PASSWORD`, `LG_ITEST_WEB_PASSWORD`, `LG_ITEST_ONVIF_PASSWORD`; real `camera.conf`/`etc/` never copied.
- Not simulated: image sensor, ISP, `/dev/venc`, audio, real PTZ motor. So `camera`, `rtsp` with real video, snapshots (expect 503) and SDK demos have no automated end-to-end coverage.
- Console transcripts: `target/itest/console/`.

## Hardware verification status

Tracked in `docs/notes/IMPLEMENTATION.md` (Verification status) and `docs/notes/HARDWARE-FINDINGS.md` §4, §9 — partially stale (see CONCERNS.md).
