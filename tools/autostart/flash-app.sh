#!/bin/sh
# flash-app.sh — write the patched APP squashfs to /dev/mtd6 ON THE CAMERA.
#
# DRY RUN BY DEFAULT; erases/writes flash only with --write. POSIX sh
# (BusyBox: no dirname, head or tail). Never reboots.
#
#   sh flash-app.sh            preflight + list the 4 KiB blocks that differ
#   sh flash-app.sh --write    write those blocks, read each back, verify
#
# Expected beside this script (override with env):
#   mtdpatch                         static ARM tool (mtdpatch/ in the repo)
#   app-patched.sqfs (+ .md5)        from build-image.sh
#   app-backup.sqfs  (+ .md5)        the camera's original partition
#   PREVIOUS=<image>                 optional: an earlier patched image that is
#                                    on flash now (re-patching an older patch)
set -u

HERE=${0%/*}; [ "$HERE" = "$0" ] && HERE=.
MTD=${MTD:-/dev/mtd6}
MTDRO=${MTDRO:-/dev/mtd6ro}
MTDPATCH=${MTDPATCH:-$HERE/mtdpatch}
IMAGE=${IMAGE:-$HERE/app-patched.sqfs}
BACKUP=${BACKUP:-$HERE/app-backup.sqfs}
PREVIOUS=${PREVIOUS:-}

die() { echo "flash-app: $*" >&2; exit 1; }
md5() { md5sum "$1" | cut -d' ' -f1; }
# md5 of the first $1 bytes of flash ($1 is a multiple of 4096)
flash_md5() { dd if="$MTDRO" bs=4096 count=$(($1 / 4096)) 2>/dev/null | md5sum | cut -d' ' -f1; }

WRITE=0
[ "${1:-}" = "--write" ] && WRITE=1

for f in "$MTDPATCH" "$IMAGE" "$IMAGE.md5" "$BACKUP" "$BACKUP.md5"; do
	[ -f "$f" ] || die "missing $f"
done
[ -x "$MTDPATCH" ] || chmod +x "$MTDPATCH"
[ -c "$MTD" ] || die "$MTD is not a char device"

echo "== preflight =="
[ "$(md5 "$IMAGE")" = "$(cut -d' ' -f1 < "$IMAGE.md5")" ] || die "app-patched.sqfs does not match its .md5 (bad copy?)"
base=$(cut -d' ' -f1 < "$BACKUP.md5")
[ "$(md5 "$BACKUP")" = "$base" ] || die "app-backup.sqfs does not match its .md5 (bad copy?)"
sz=$(wc -c < "$IMAGE")
[ $((sz % 4096)) -eq 0 ] || die "image not 4 KiB-aligned ($sz)"
[ "$(wc -c < "$BACKUP")" -eq "$(cat /sys/class/mtd/${MTD##*/}/size 2>/dev/null || wc -c < "$BACKUP")" ] \
	|| die "app-backup.sqfs is not the size of $MTD; it must be a full dump of this partition"

cur=$(md5 "$MTDRO")
if [ "$cur" = "$base" ]; then
	echo "$MTD holds the original firmware (matches app-backup.sqfs)"
elif [ "$(flash_md5 "$sz")" = "$(md5 "$IMAGE")" ]; then
	echo "$MTD already holds this patched image; nothing to do"
	exit 0
elif [ -n "$PREVIOUS" ] && [ "$(flash_md5 "$(wc -c < "$PREVIOUS")")" = "$(md5 "$PREVIOUS")" ]; then
	echo "$MTD holds the previous patched image $PREVIOUS"
else
	die "$MTD is neither the original (app-backup.sqfs) nor a known patched image; refusing"
fi

echo
echo "== mtdpatch dry run =="
"$MTDPATCH" -n "$MTD" "$IMAGE" "$BACKUP"

if [ "$WRITE" != 1 ]; then
	echo
	echo "DRY RUN ONLY. Re-run with --write. The watchdog must be fed (wdt_feeder running)."
	exit 0
fi

echo
echo "== writing $MTD (only differing blocks; auto-restore on failure) =="
"$MTDPATCH" "$MTD" "$IMAGE" "$BACKUP"
rc=$?
case "$rc" in
	0) ;;
	2) die "write failed, the ORIGINAL was restored (rc=2)" ;;
	*) die "write FAILED and restore may have failed (rc=$rc). DO NOT REBOOT; use the serial console" ;;
esac

got=$(flash_md5 "$sz")
echo "flash: $got"
echo "image: $(md5 "$IMAGE")"
[ "$got" = "$(md5 "$IMAGE")" ] || die "read-back MISMATCH: run rollback.sh --write now, do NOT reboot"
echo
echo "SUCCESS. Reboot by hand; /tmp/anyka_config_sh.log shows the hook ran."
