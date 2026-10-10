# Autostart on the AK3918AV100

The AV100's stock firmware never runs anything from the SD card: `/ipc/setup.sh`
(in the read-only APP squashfs on `/dev/mtd6`) mounts the card only to look for
vendor files, unmounts it, and starts the vendor app `/ipc/ipc`.

This kit replaces only the tail of that script. It keeps the hardware bring-up
and, instead of the vendor app, mounts the card and runs `Factory/config.sh`.
`/ipc/ipc` is never started, not even without a card.

| File | Runs on | What |
| --- | --- | --- |
| `kamoder-tail.sh` | — | replaces everything from the vendor's `#stop ipc` line down |
| `build-image.sh MTD6_DUMP` | host | splices the tail into `setup.sh` from **your** dump and rebuilds the squashfs, with structural checks |
| `mtdpatch/` | camera | rewrites only the 4 KiB erase blocks that differ, reads each one back, restores automatically on failure (`build.sh` cross-compiles it) |
| `flash-app.sh [--write]` | camera | preflight + dry run by default; never reboots |
| `rollback.sh [--write]` | camera | writes the original partition back |

Tested on the AK3918AV100 EVB_CBDM_KM01N (Gwell firmware 38.2.72; mtd6 md5
`f4c739e1d60b3dc0bbaf6d9d5a15beb3`). Read the full guide before using it:
https://<owner>.github.io/<repo>/docs/getting-started/autostart/

```sh
# camera: dump the partition to the card
cat /dev/mtd6ro > /mnt/disc1/mtd6.bin && md5sum /dev/mtd6ro /mnt/disc1/mtd6.bin
# host
./build-image.sh mtd6.bin out/ && ./mtdpatch/build.sh
# copy out/*, flash-app.sh, rollback.sh, mtdpatch/mtdpatch to /mnt/disc1/kamoder-flash/
# camera
cd /mnt/disc1/kamoder-flash && sh flash-app.sh && sh flash-app.sh --write && reboot
```
