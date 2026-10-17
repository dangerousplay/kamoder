# OTA — User Decisions (2026-10-17)

| Gray area | Decision |
| --- | --- |
| Update source | GitHub Releases of the public repo + manual `.kup` upload in the web UI. |
| Default policy | Download + verify in background, notify; admin clicks Install. Auto-install opt-in (maintenance window). |
| Channels | stable (latest release) + beta (pre-releases), selectable. |

## Research inputs (2026-10-17)

- Boot: stock rc.local → mtd6 `/ipc/setup.sh` patched tail (`tools/autostart/kamoder-tail.sh`) → `sh /mnt/disc1/Factory/config.sh` → `$SD/anyka_rust/init.sh`. Tree relocatable (`$ORIGIN` RPATH, relative supervisor paths). Vendor recovery in mtd5 (`button_upgrade.sh`) survives broken mtd6.
- Flash: XM25QH64C 8 MB NOR, 4 KiB erase; mtd0-7 fill the chip (no spare space). U-Boot 2019.10 vendor, env 4 KiB single CRC (ENVBK copy), no bootcount/altbootcmd; partitions via `mtdparts=` bootargs → future repartition possible via env.
- Watchdog: feeder disarms on clean exit; supervisor never reboots on crash loops → rollback trigger must live in a stable shim.
- vfat SD: no symlinks; rename not power-safe → redundant fixed-size selector records (seq + CRC), like redundant U-Boot env.
- Crates: `minisign-verify` 0.3 (0 deps), ring/sha2/flate2(miniz_oxide)/tar, hyper 1 + hyper-util client-legacy + hyper-rustls (ring, webpki-roots), crc32fast, nix ioctls for MTD; own U-Boot env codec (no good crate). GitHub: `releases/latest/download/<asset>` avoids API limit for stable; beta needs `GET /repos/:o/:r/releases` (60/h unauth).
- Release CI: `tools/package-release.sh` + `.github/workflows/release.yml` (cosign keyless + SLSA). No version injected into binaries yet.
