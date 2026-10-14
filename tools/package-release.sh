#!/bin/sh
# package-release.sh VERSION [OUT_DIR]
#
# Turns a finished `./x dist` (dist/anyka_rust, full line tables) into the
# release artifacts that CI signs and publishes:
#
#   kamoder-VERSION-sdcard.tar.gz   the root of an SD card: autorun.sh,
#                                   Factory/ and anyka_rust/ with stripped
#                                   binaries. Extract it onto a FAT32 card.
#   kamoder-VERSION-debug.tar.gz    the unstripped binaries, to turn a panic
#                                   address from the camera into file:line.
#   mtdpatch-armv5                  static ARM tool for tools/autostart.
#   SHA256SUMS                      checksums of all of them.
#
# It never ships camera.conf, etc/, logs or keys: the card gets
# camera.conf.example, and the camera generates its own keys on first boot.
set -eu

VERSION=${1:?usage: package-release.sh VERSION [OUT_DIR]}
HERE=$(cd "$(dirname "$0")/.." && pwd)
OUT=${2:-$HERE/release}
CROSS=${ANYKA_TOOLCHAIN:-/opt/arm-anykav200-crosstool}
STRIP=$CROSS/usr/bin/arm-anykav200-linux-uclibcgnueabi-strip
DIST=$HERE/dist/anyka_rust
CARD_SRC=$HERE/card

[ -d "$DIST/bin" ] || { echo "no $DIST; run ./x dist first" >&2; exit 1; }
[ -x "$STRIP" ] || { echo "strip not found at $STRIP" >&2; exit 1; }
[ -d "$DIST/boards" ] || { echo "board profiles missing from dist; rerun ./x dist" >&2; exit 1; }
[ -d "$DIST/www" ] || { echo "web UI missing from dist; run ./x webui before ./x dist" >&2; exit 1; }

NAME=kamoder-$VERSION
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$OUT"

# --- SD card tree ------------------------------------------------------------
CARD=$WORK/$NAME-sdcard
mkdir -p "$CARD/anyka_rust/bin" "$CARD/anyka_rust/logs" "$CARD/Factory"
for f in "$DIST"/bin/*; do
	"$STRIP" -o "$CARD/anyka_rust/bin/$(basename "$f")" "$f"
done
cp -R "$DIST/lib" "$DIST/www" "$DIST/boards" "$CARD/anyka_rust/"
for f in init.sh run-on-camera.sh camera.conf.example; do
	cp "$DIST/$f" "$CARD/anyka_rust/"
done
# The shipped config (build-owned, like bin/ and boards/).
cp "$HERE/tools/supervisor.toml" "$CARD/anyka_rust/supervisor.toml"
cp "$HERE/tools/SD-README.md" "$CARD/anyka_rust/README.md"
cp "$CARD_SRC/Factory/config.sh" "$CARD/Factory/config.sh"
[ -f "$CARD_SRC/Factory/README.md" ] && cp "$CARD_SRC/Factory/README.md" "$CARD/Factory/"
cp "$CARD_SRC/autorun.sh" "$CARD/autorun.sh"
chmod 0755 "$CARD"/anyka_rust/bin/* "$CARD"/anyka_rust/*.sh "$CARD/Factory/config.sh" "$CARD/autorun.sh"

# Nothing secret may leave the build machine.
for secret in camera.conf etc backup metrics; do
	[ ! -e "$CARD/anyka_rust/$secret" ] || { echo "refusing to package $secret" >&2; exit 1; }
done

# --- debug binaries ----------------------------------------------------------
DBG=$WORK/$NAME-debug
mkdir -p "$DBG"
cp "$DIST"/bin/* "$DBG/"

# Reproducible-ish archives: fixed owner, mtime and order.
EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$HERE" log -1 --format=%ct 2>/dev/null || date +%s)}
pack() { # dir archive
	tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$EPOCH" \
		-C "$(dirname "$1")" -cf - "$(basename "$1")" | gzip -n -9 > "$2"
}
pack "$CARD" "$OUT/$NAME-sdcard.tar.gz"
pack "$DBG" "$OUT/$NAME-debug.tar.gz"

# --- autostart helper -----------------------------------------------------------
"$HERE/tools/autostart/mtdpatch/build.sh" >/dev/null
cp "$HERE/tools/autostart/mtdpatch/mtdpatch" "$OUT/mtdpatch-armv5"

(cd "$OUT" && sha256sum "$NAME-sdcard.tar.gz" "$NAME-debug.tar.gz" mtdpatch-armv5 > SHA256SUMS)
cat "$OUT/SHA256SUMS"
