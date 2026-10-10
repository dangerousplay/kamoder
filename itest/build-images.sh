#!/usr/bin/env bash
# Builds the two images the integration-test VM boots:
#
#   sd.img             a 128 MiB card: MBR + one FAT32 partition holding
#                      autorun.sh, Factory/ and anyka_rust/ — the same layout
#                      and filesystem as the real SD card. QEMU presents it as
#                      /dev/mmcblk0 on the PL181 controller, like the camera.
#   initramfs.cpio.gz  the camera's own root filesystem plus /itest: the app
#                      partition (mounted on /ipc and /usr as on the camera),
#                      the factory /rom Wi-Fi config, and the scripts that
#                      emulate the firmware boot and the access points.
#
#   build-images.sh ROOTFS APP_SQUASHFS CARD SUPPORT_BIN CAMERA_CONF OUT_DIR
#
# Card files the camera owns (camera.conf, etc/, logs/, backup/, metrics/) are
# not copied: the test must not see — or leak — the real card's secrets.
# No root needed: sfdisk/mkfs.vfat/mtools work on files, fakeroot covers the
# device nodes in the cpio.
set -euo pipefail

[[ $# -eq 6 ]] || { echo "usage: $0 ROOTFS APP_SQUASHFS CARD SUPPORT_BIN CAMERA_CONF OUT_DIR" >&2; exit 2; }
ROOTFS=$1 APP=$2 CARD=$3 SUPPORT=$4 CONF=$5 OUT=$6
HERE=$(cd "$(dirname "$0")" && pwd)

for path in "$ROOTFS" "$CARD/anyka_rust"; do
    [[ -d $path ]] || { echo "$0: missing $path" >&2; exit 1; }
done
[[ -f $APP ]] || { echo "$0: missing $APP" >&2; exit 1; }
[[ -x $SUPPORT/wiphy-netns ]] || { echo "$0: no wiphy-netns in $SUPPORT" >&2; exit 1; }
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)

# --- SD card -------------------------------------------------------------------
# QEMU's SD model wants a power-of-two size.
SD=$OUT/sd.img
PART=$OUT/sd.part.img
SIZE_MB=128
START_SECTOR=2048   # 1 MiB, the usual alignment on real cards

rm -f "$SD" "$PART"
truncate -s ${SIZE_MB}M "$SD"
printf 'label: dos\nstart=%d, type=c\n' "$START_SECTOR" | sfdisk --quiet "$SD"
PART_BYTES=$(( SIZE_MB * 1024 * 1024 - START_SECTOR * 512 ))
truncate -s "$PART_BYTES" "$PART"
mkfs.vfat -F 32 -n ANYKA_SD "$PART" >/dev/null

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp -a "$CARD/autorun.sh" "$CARD/Factory" "$stage/"
mkdir "$stage/anyka_rust"
for entry in "$CARD/anyka_rust"/*; do
    case ${entry##*/} in
        camera.conf|etc|logs|backup|metrics) ;;
        *) cp -a "$entry" "$stage/anyka_rust/" ;;
    esac
done
cp "$CONF" "$stage/anyka_rust/camera.conf"
MTOOLS_SKIP_CHECK=1 mcopy -i "$PART" -s -Q "$stage"/* ::/
dd if="$PART" of="$SD" bs=512 seek="$START_SECTOR" conv=notrunc status=none
rm -f "$PART"
echo "sd card  : $SD"

# --- initramfs -------------------------------------------------------------------
IMG=$OUT/initramfs.cpio.gz
root=$(mktemp -d)
trap 'rm -rf "$stage" "$root"' EXIT
fakeroot bash -euo pipefail -c '
    ROOTFS=$1 APP=$2 SUPPORT=$3 HERE=$4 IMG=$5 root=$6
    cp -a "$ROOTFS/." "$root/"
    mkdir -p "$root/itest/bin" "$root/itest/rom"
    install -m 644 "$APP" "$root/itest/app.squashfs"
    install -m 755 "$SUPPORT/wiphy-netns" "$root/itest/bin/"
    install -m 755 "$HERE/guest/ap" "$HERE/guest/ap-netns" "$root/itest/bin/"
    install -m 755 "$HERE/guest/firmware-init" "$root/itest/firmware-init"
    install -m 600 "$HERE/guest/wpa_supplicant0.conf" "$root/itest/rom/"
    mkdir -p "$root/dev"
    [ -e "$root/dev/console" ] || mknod "$root/dev/console" c 5 1
    chown -R 0:0 "$root"
    cd "$root" && find . | cpio -o -H newc 2>/dev/null | gzip -1 > "$IMG"
' _ "$ROOTFS" "$APP" "$SUPPORT" "$HERE" "$IMG" "$root"
echo "initramfs: $IMG ($(du -h "$IMG" | cut -f1))"
