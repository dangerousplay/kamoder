# OTA updates

Spec and design: `.specs/features/ota/` (spec.md, design.md). This note is for
maintainers.

## How it works

The scheme is the usual A/B one (Android, RAUC, Mender): two copies of the
firmware, the running one is never touched, the other one is written,
verified, and booted on trial. If the trial never proves healthy, the next
boot goes back to the old copy on its own.

### SD layout

```
/mnt/disc1/Factory/config.sh       boot shim (run by the mtd6 hook)
/mnt/disc1/anyka_rust/             data root, and the legacy slot
├── camera.conf etc/ logs/ backup/ metrics/ media/   shared by both slots
├── boot/sel.0 boot/sel.1          redundant selector records
├── boot/last                      last slot kboot picked (shell fallback)
├── boot/bin/kboot boot/lib/sys/   boot helper, outside the slots
├── slots/a/ slots/b/              bin/ lib/ www/ boards/ supervisor.toml init.sh VERSION manifest.json(.minisig)
└── update/                        state.json, downloads, upload.kup
```

Services run with the data root as working directory, so `etc/…` and
`camera.conf` resolve the same from either slot; code paths in
`supervisor.toml` use `{slot}`. A card flashed with `./x flash` is a legacy
tree (slot a = the data root itself); the first update writes `slots/b`, the
next one `slots/a`.

### Boot

1. `config.sh` starts telnetd (rescue), then runs `boot/bin/kboot select`
   (20 s timeout). kboot reads the newest valid selector record (seq + CRC32;
   a torn write falls back to the other copy), picks the highest-priority
   bootable slot, and if that slot is on trial uses up one of its 3 tries.
   A trial slot with no tries left is marked unbootable and the other slot
   boots instead.
2. If kboot is missing or fails: `boot/last`, then the legacy tree.
3. On a trial boot `config.sh` also starts `kboot guard --window 600`: if the
   slot is not committed within 10 minutes it syncs and reboots (consuming a
   try). A hang is caught by the hardware watchdog the same way.
4. The `update` service commits the slot (marks it successful) once every
   enabled service has been running for 3 minutes, the web UI answers, and
   the slot's own signed manifest verifies.

So a broken release costs at most three boots of ~10 minutes each before the
camera is back on the previous version, without anyone touching it.

### Updating

The `update` service checks GitHub every 12 h (stable: `releases/latest`;
beta: newest pre-release), downloads the `.kup` with resume and a 256 KiB/s
cap, verifies the minisign signature, the device policy (board, monotonic
`seq`, security floor) and every component hash, and waits for an admin to
click Install on the web UI's **Updates** page (or installs in the maintenance
window when auto-install is on). A `.kup` can also be uploaded there.
Installing writes the inactive slot, activates it on trial and reboots.

### Recovery

- **Roll back by hand:** Updates page → Roll back, or over ssh
  `boot/bin/kboot rollback --root /mnt/disc1/anyka_rust` and reboot.
- **Inspect:** `boot/bin/kboot status --root /mnt/disc1/anyka_rust` and
  `logs/boot.log`.
- **Selector broken / both slots bad:** delete `boot/sel.0`, `boot/sel.1`
  and `boot/last` from a PC: the card boots the legacy tree in `anyka_rust/`
  (if present). Re-flashing with `./x flash` does exactly this.
- **Shim broken:** telnetd still starts before kboot; UART console; the stock
  mtd5 recovery path is untouched.

### Future firmware on flash

Nothing above is SD-specific in the updater core. `bootctl::UbootEnvControl`
keeps the same record in a (redundant) U-Boot environment, and
`ota::mtd::MtdStore` writes `mtd-image` components to partitions looked up
**by name** in `/proc/mtd` (`KERNEL_{SLOT}` → `KERNEL_A`/`KERNEL_B`), so a new
`mtdparts=` layout needs no code change. Both are host-tested only; the
current AV100's 8 MB NOR has no room for a second kernel/rootfs and its
U-Boot has no boot counter.

## Release packages (`./x kup`)

`./x kup` turns the tree `./x dist` staged in `dist/anyka_rust` into a signed
update package (OTA-01, OTA-02). Release CI runs it through
`tools/package-release.sh`. You can also run it by hand.

```sh
./x webui && ./x dist        # the slot tree: bin/ lib/ www/ boards/ ...
./x kup --test-key           # local test: signs with the TEST-ONLY key
./x kup --key ~/kamoder.key  # real signature (prompts for the password,
                             # or reads it from MINISIGN_PASSWORD)
```

Output in `--out` (default `dist/kup`):

| File | What |
|---|---|
| `kamoder-<ver>-update.kup` | Uncompressed tar with `manifest.json`, `manifest.json.minisig`, `payload.tar.gz` and `boot.tar.gz`, in that order. You upload this file in the web UI. |
| `manifest.json` | The signed manifest, the same bytes as inside the `.kup` (`Manifest::to_json()`). |
| `manifest.json.minisig` | Its minisign signature. The trusted comment is `kamoder <version> seq=<seq>`. |

The release asset names are part of the protocol. The stable channel fetches
`https://github.com/<repo>/releases/latest/download/manifest.json` (and
`.minisig`), so these two assets must keep exactly these names.

### Options

| Option | Default |
|---|---|
| `--dist DIR` | `dist/anyka_rust` |
| `--out DIR` | `dist/kup` |
| `--version V` | `git describe --tags`, without the leading `v`. On a tag you get the tag itself. Past a tag you get `0.2.0-dev.3.gabc1234`. With no tag you get the workspace version. CI passes the release version. |
| `--channel stable\|beta` | `beta` if the version has a pre-release part, else `stable` |
| `--seq N` | commit time of HEAD (`git log -1 --format=%ct`). This is the anti-replay counter, and cameras refuse a lower one. |
| `--security-floor V` | `0.0.0`. Raise it after a security fix, so cameras refuse anything older. |
| `--min-updater V` | `0.1.0` |
| `--key PATH` | The minisign secret key. An encrypted key reads `MINISIGN_PASSWORD` from the environment, else it prompts. |
| `--pub PATH` (repeatable) | Public keys for the self-check after signing. The default is the keys the updater is built with (`crates/ota-manifest/keys/kamoder.pub` and `kamoder-next.pub`, or `KAMODER_UPDATE_PUBKEY[_NEXT]`). |
| `--test-key` | Signs with `crates/ota-manifest/keys/test/test.key` and checks with `test.pub`. No real camera trusts it. |
| `--boot-dir DIR` | Where to take kboot from. `DIR` holds `bin/kboot` and `lib/sys/`. Without it, kup uses `dist/anyka_rust/boot/` if present, else dist `bin/kboot` and `lib/sys/`. If kboot is not built, the package has no boot component and kup prints a warning. |
| `--no-boot` | Builds the package without the boot-shim component. |
| `--no-strip` | Packs `bin/` as it is. By default kup strips `bin/` with the cross `strip`, as for the SD card release. |

`SOURCE_DATE_EPOCH` sets the archive mtime and `created`. Without it, kup uses
the commit time.

### What is in the package

- **`slot` component (`sd-tree`, `payload.tar.gz`)**: the whole dist tree
  except the camera's data and the A/B machinery. kup leaves out
  `camera.conf`, `etc/`, `logs/`, `backup/` and `metrics/` (the `SD_PRESERVED`
  list in `xtask/src/tasks.rs`), plus `media/`, `boot/`, `slots/` and
  `update/`. It also leaves out `bin/kboot`, which lives outside the slots.
  `README.md` comes from `tools/SD-README.md`. The installer writes `VERSION`
  and the manifest copy itself (OTA-06), so they are not in the payload.
- **`boot` component (`boot-shim`, `boot.tar.gz`)**:
  - `boot/bin/kboot` and `boot/lib/sys/*`: paths relative to the data root,
    `anyka_rust/`.
  - `factory/config.sh`: from `card/Factory/config.sh`. The `factory/`
    prefix maps to the card's `Factory/` directory, next to `anyka_rust/`.

Both archives are reproducible, so the same inputs give the same bytes:

- regular files only, with no directory entries or links
- relative paths, sorted
- uid/gid 0
- mode 0755 for executables, 0644 for everything else
- one fixed mtime for every entry
- gzip level 9 (flate2, pure Rust), with no name and a zero mtime in the header

The manifest is then byte-identical too. The `.minisig` is not, because current
minisign/libsodium signatures are randomized, but every one of them verifies.

After signing, kup reads the `.kup` back the way the camera will. It checks
that the manifest comes first, then verifies the signature against the
trusted keys and checks the schema. Last, it checks every payload's size and
SHA-256 and every file's size, SHA-256 and mode. Any mismatch fails the build,
and kup deletes the unverifiable `manifest.json` pair.

### Keys and CI

Create the project key once, as described in `crates/ota-manifest/keys/README.md`
(`minisign -G`, commit `kamoder.pub`). Then add these repository secrets:

- `MINISIGN_SECRET_KEY`: the contents of `kamoder.key`.
- `MINISIGN_PASSWORD`: its password.

`.github/workflows/release.yml` installs `minisign` with apt and passes both
secrets to `tools/package-release.sh`. The script writes the key to a 0600
temporary file and runs:

```sh
./x kup --dist dist/anyka_rust --out release --version <ver> --key <tmp>
```

It then adds the `.kup`, `manifest.json` and `manifest.json.minisig` to
`SHA256SUMS`. Cosign keyless signs every asset (`<file>.sigstore.json`), and
the SLSA provenance covers the `.kup` and `manifest.json`.

If the secret is not set (local builds, forks), the script skips the OTA
package with a warning and the release has no OTA assets. If the secret is set
but `kamoder.pub` is still the placeholder, or holds a different key, kup's
self-check fails. That stops the release instead of publishing a package that
no camera would accept.

To try the release path locally with the test key:

```sh
K=crates/ota-manifest/keys/test
MINISIGN_SECRET_KEY="$(cat $K/test.key)" KAMODER_UPDATE_PUBKEY="$(cat $K/test.pub)" \
  tools/package-release.sh 0.1.0-test.1 /tmp/release
```

`KAMODER_UPDATE_PUBKEY` rebuilds xtask's copy of `ota-manifest` so that it
trusts the test key. Run any `./x` task without it afterwards to go back to
the normal keys.
