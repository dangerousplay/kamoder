#!/usr/bin/env bash
# build-image.sh — build a patched APP (mtd6) squashfs from YOUR camera's dump.
#
# Runs on the host (squashfs-tools >= 4.5). Writes nothing to any camera.
#
#   ./build-image.sh MTD6_DUMP [OUT_DIR]
#
#   MTD6_DUMP  full read of the camera's /dev/mtd6ro (e.g. `cat /dev/mtd6ro >
#              /mnt/disc1/mtd6.bin` on the camera, then copy it off the card)
#   OUT_DIR    default ./out — receives app-patched.sqfs(+.md5) and
#              app-backup.sqfs (a copy of the dump, used for rollback)
#
# The vendor /ipc/setup.sh is not shipped here. It is taken from the dump, cut
# at its first "#stop ipc" line, and kamoder-tail.sh is appended. Everything
# above the cut (modules, clocks, Wi-Fi, GPIO 98, DTB check) stays as it was.
#
# Checks, in order:
#   1. the dump is squashfs 4.0 and holds /setup.sh with a "#stop ipc" line
#   2. an unmodified rebuild reproduces the original tree (perms + sizes), so
#      the toolchain is proven before anything changes
#   3. the patched image fits the partition, is 4 KiB-aligned, and differs
#      from the original only in /setup.sh
set -euo pipefail

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
DUMP=${1:?usage: build-image.sh MTD6_DUMP [OUT_DIR]}
OUT=${2:-$HERE/out}
TAIL=$HERE/kamoder-tail.sh
ERASE=4096
# The AK3918AV100 EVB_CBDM_KM01N firmware this was tested on (Gwell 38.2.72).
TESTED_MD5=f4c739e1d60b3dc0bbaf6d9d5a15beb3

command -v mksquashfs >/dev/null || { echo "need squashfs-tools" >&2; exit 1; }
[ -f "$DUMP" ] || { echo "dump not found: $DUMP" >&2; exit 1; }

PART_SIZE=$(stat -c%s "$DUMP")
[ $((PART_SIZE % ERASE)) -eq 0 ] || { echo "dump size $PART_SIZE is not a whole partition" >&2; exit 1; }
DUMP_MD5=$(md5sum "$DUMP" | cut -d' ' -f1)
if [ "$DUMP_MD5" = "$TESTED_MD5" ]; then
	echo "dump md5 $DUMP_MD5 = tested firmware"
else
	echo "WARNING: dump md5 $DUMP_MD5 is not the tested firmware ($TESTED_MD5)."
	echo "         Read the diff of setup.sh printed below before flashing."
fi

# Superblock options, so the rebuild matches the original.
SB=$(unsquashfs -s "$DUMP")
grep -q 'Found a valid SQUASHFS 4:0 superblock' <<<"$SB" || { echo "not a squashfs 4.0 image" >&2; exit 1; }
COMP=$(sed -n 's/^Compression //p' <<<"$SB" | head -n1)
BSIZE=$(sed -n 's/^Block size //p' <<<"$SB" | head -n1)
MKFS_TIME=$(unsquashfs -mkfs-time "$DUMP" 2>/dev/null || echo 0)
echo "squashfs: $COMP, block $BSIZE, mkfs-time $MKFS_TIME"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
SRC=$WORK/root
( umask 000; unsquashfs -d "$SRC" -no-xattrs "$DUMP" >/dev/null )
[ -f "$SRC/setup.sh" ] || { echo "no /setup.sh in the image" >&2; exit 1; }

CUT=$(grep -n -m1 '^#stop ipc' "$SRC/setup.sh" | cut -d: -f1 || true)
[ -n "$CUT" ] || { echo "no '#stop ipc' line in setup.sh; unknown firmware, not patching" >&2; exit 1; }

mksq() { # dir out
	rm -f "$2"
	mksquashfs "$1" "$2" -comp "$COMP" -b "$BSIZE" -all-root -no-xattrs \
		-noappend -mkfs-time "$MKFS_TIME" -processors 1 -no-progress -quiet
}
listing() { unsquashfs -ll "$1" 2>/dev/null | awk '{print $1,$3,$NF}' | sort; }

mksq "$SRC" "$WORK/sanity.sqfs"
diff <(listing "$DUMP") <(listing "$WORK/sanity.sqfs") >/dev/null \
	|| { echo "unmodified rebuild does not reproduce the original tree" >&2; exit 1; }
echo "sanity rebuild reproduces the original tree"

cp "$SRC/setup.sh" "$WORK/setup.orig.sh"
{ head -n $((CUT - 1)) "$WORK/setup.orig.sh"; cat "$TAIL"; } > "$SRC/setup.sh"
chmod 0750 "$SRC/setup.sh"
sh -n "$SRC/setup.sh"

mkdir -p "$OUT"
mksq "$SRC" "$OUT/app-patched.sqfs"
sz=$(stat -c%s "$OUT/app-patched.sqfs")
[ "$sz" -le "$PART_SIZE" ] || { echo "image $sz > partition $PART_SIZE" >&2; exit 1; }
[ $((sz % ERASE)) -eq 0 ] || { echo "image not $ERASE-aligned ($sz)" >&2; exit 1; }
diff <(listing "$DUMP" | grep -v '/setup.sh$') <(listing "$OUT/app-patched.sqfs" | grep -v '/setup.sh$') >/dev/null \
	|| { echo "patched image changed files other than setup.sh" >&2; exit 1; }

cp "$DUMP" "$OUT/app-backup.sqfs"
(cd "$OUT" && md5sum app-patched.sqfs > app-patched.sqfs.md5 && md5sum app-backup.sqfs > app-backup.sqfs.md5)

echo
echo "setup.sh: vendor lines 1-$((CUT - 1)) kept, tail replaced:"
diff -u "$WORK/setup.orig.sh" "$SRC/setup.sh" | sed -n '1,12p'
echo "..."
echo
echo "built $OUT/app-patched.sqfs ($sz bytes, partition $PART_SIZE)"
echo "copy $OUT/*, flash-app.sh, rollback.sh and mtdpatch/mtdpatch to /mnt/disc1/kamoder-flash/"
