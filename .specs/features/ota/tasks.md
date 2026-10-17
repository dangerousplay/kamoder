# OTA Tasks

**Spec**: `spec.md` · **Design**: `design.md` · **Decisions**: `context.md`
**Status**: T1–T12 implemented and host-verified (2026-10-17); T13 live test pending

## Execution Plan

```
T1 skeletons (ota-manifest, bootctl, ota, update-proto, kboot; workspace; xtask lists)
 ├─[P]→ T2 ota-manifest (types, minisign verify, policy)         ─┐
 ├─[P]→ T3 bootctl (selector records, choose(), SdSelector, UbootEnv codec) ─┤
 └─[P]→ T4 supervisor {slot}/--slot-dir/--data-dir + init.sh + config.sh + supervisor.toml
T3 → T5 kboot bin (select, guard, status)
T2,T3 → T6 ota: SlotStore (SdDirStore, boot-shim, MtdStore+Flash fake), kup reader/installer
T2 → T7 ota: http client (hyper-rustls), Range resume, github discovery
T2 → T8 xtask kup + package-release.sh + release.yml (minisign)
T5,T6,T7 → T9 update-proto + update service (runtime, health commit, state, policy, metrics)
T9 → T10 web /api/update* + upload ; T10 → T11 webui Updates page
all → T12 packaging (dist layout boot/, supervisor.toml service), docs, ./x ci ; T13 live migration test (user OK)
```

## Task Breakdown

| ID | What | Where | Depends | Done when | Req |
|---|---|---|---|---|---|
| T1 | Crate skeletons + workspace/xtask registration | Cargo.toml, xtask | — | `./x check` | — |
| T2 | Manifest types, minisign verify (current+next key, test key cfg(test)), policy checks | crates/ota-manifest | T1 | unit: good/tampered/wrong key/board/seq/floor/min_updater | OTA-02..04 |
| T3 | Selector record codec + CRC, redundant read/write, `choose()`, BootControl trait, SdSelector, Memory, UbootEnv codec (single + redundant) | crates/bootctl | T1 | unit: torn write, both invalid, tries countdown, rollback, env CRC/flags round-trip | OTA-07..09,21 |
| T4 | supervisor `--slot-dir/--data-dir` + `{slot}` expansion; init.sh KAMODER_DATA/SLOT; card/Factory/config.sh shim with fallbacks; supervisor.toml `{slot}` paths | crates/supervisor, tools, card | T1 | supervisor tests; shell reviewed; legacy unchanged | OTA-05,09,12,13 |
| T5 | kboot: select/guard/status/mark-good, `boot/last`, reboot | crates/kboot | T3 | tests with temp root; cross-build size | OTA-09,10 |
| T6 | SlotStore + SdDirStore + boot-shim + MtdStore (nix ioctls, /proc/mtd by name, Flash trait + file fake); `.kup` streaming reader/installer | crates/ota | T2,T3 | tests: install, corrupt file, traversal, interrupted, mtd fake erase/write/verify | OTA-06,20,22 |
| T7 | HTTPS client, redirects, Range resume, rate cap, streaming sha256; GitHub stable/beta discovery | crates/ota | T2 | tests with local hyper server (resume, redirect, bad hash) | OTA-14,15 |
| T8 | `xtask kup` + package-release + release.yml minisign step | xtask, tools, .github | T2 | `./x kup` builds+verifies a test-signed .kup | OTA-01 |
| T9 | update-proto + `update` service: state, scheduler, download, install, rollback, health commit, settings, metrics | crates/update-proto, crates/ota (bin update) | T5..T7 | tokio tests with fakes | OTA-11,14..19 |
| T10 | web `/api/update*`, streamed upload | crates/web | T9 | api_tests | OTA-16..19 |
| T11 | webui Updates page | webui | T10 | `./x webui` | OTA-16..19 |
| T12 | dist layout (boot/ kboot+lib), supervisor.toml `update` service, docs (docs/notes/OTA.md, site), spec status, `./x ci` | tools, xtask, docs | all | ci green | — |
| T13 | Live: deploy shim, migrate legacy → slot b via upload, rollback test (break b), commit test | camera | T12 | user-approved, documented | all |

## Outcome (2026-10-17)

T1–T12 done; `./x ci` steps green (1055 workspace tests). Deviations: HTTP client is its own generic crate `fetch` (not inside `ota`); install refused while the running slot is on trial; reflash (`./x flash`) resets A/B state but keeps `update/state.json`; kboot hand-rolled CLI (clap +295 KB), `opt-level = "s"` (409 KB stripped); web +229 KB for update routes, `update` binary 4.8 MB stripped. Open: production minisign key + GitHub secrets; `attest-build-provenance` with no-match subject-path on unsigned runs; seq = commit time (re-run of the same commit is a replay).
