#!/bin/sh
# rollback.sh — write the original APP partition back to /dev/mtd6.
#
# DRY RUN BY DEFAULT; --write to act. Never reboots. Works over SSH or the
# serial console. Uses the same block-verified path as flash-app.sh.
#
#   sh rollback.sh [--write]
set -u

HERE=${0%/*}; [ "$HERE" = "$0" ] && HERE=.
MTD=${MTD:-/dev/mtd6}
MTDRO=${MTDRO:-/dev/mtd6ro}
MTDPATCH=${MTDPATCH:-$HERE/mtdpatch}
BACKUP=${BACKUP:-$HERE/app-backup.sqfs}

die() { echo "rollback: $*" >&2; exit 1; }

for f in "$MTDPATCH" "$BACKUP" "$BACKUP.md5"; do
	[ -f "$f" ] || die "missing $f"
done
[ -x "$MTDPATCH" ] || chmod +x "$MTDPATCH"
base=$(cut -d' ' -f1 < "$BACKUP.md5")
[ "$(md5sum "$BACKUP" | cut -d' ' -f1)" = "$base" ] || die "app-backup.sqfs does not match its .md5"

if [ "$(md5sum "$MTDRO" | cut -d' ' -f1)" = "$base" ]; then
	echo "$MTD already holds the original; nothing to do"
	exit 0
fi

echo "== mtdpatch dry run =="
"$MTDPATCH" -n "$MTD" "$BACKUP"
[ "${1:-}" = "--write" ] || { echo; echo "DRY RUN ONLY. Re-run with --write."; exit 0; }

"$MTDPATCH" "$MTD" "$BACKUP" || die "restore failed (rc=$?); recover from the serial console"
[ "$(md5sum "$MTDRO" | cut -d' ' -f1)" = "$base" ] || die "read-back mismatch after restore"
echo "SUCCESS: original firmware restored. Reboot by hand."
