#!/bin/sh
# package-release.sh VERSION [OUT_DIR]
#
# Turns a finished `./x dist` (dist/anyka_rust, full line tables) into the
# release artifacts that CI signs and publishes:
#
#   kamoder-VERSION-sdcard.tar.gz   the root of an SD card: autorun.sh,
#                                   Factory/ and anyka_rust/ with stripped
#                                   binaries (a legacy tree, plus the OTA
#                                   boot helper in anyka_rust/boot/). Extract
#                                   it onto a FAT32 card.
#   kamoder-VERSION-debug.tar.gz    the unstripped binaries, to turn a panic
#                                   address from the camera into file:line.
#   mtdpatch-armv5                  static ARM tool for tools/autostart.
#   kamoder-VERSION-update.kup      OTA update package (./x kup), plus its
#   manifest.json                   signed manifest as separate assets: the
#   manifest.json.minisig           camera's stable channel fetches
#                                   releases/latest/download/manifest.json.
#                                   Only when MINISIGN_SECRET_KEY is set (the
#                                   minisign secret key file's contents; an
#                                   encrypted key also needs MINISIGN_PASSWORD).
#                                   Skipped with a warning otherwise, so local
#                                   builds still package.
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
# The OTA boot helper, outside the slots (design §1): boot/bin/kboot and the
# runtime it loads, boot/lib/sys. No selector records: a fresh card boots
# the legacy tree until the first update.
if [ -d "$DIST/boot/bin" ]; then
	mkdir -p "$CARD/anyka_rust/boot/bin"
	for f in "$DIST"/boot/bin/*; do
		"$STRIP" -o "$CARD/anyka_rust/boot/bin/$(basename "$f")" "$f"
	done
	chmod 0755 "$CARD"/anyka_rust/boot/bin/*
	[ ! -d "$DIST/boot/lib" ] || cp -R "$DIST/boot/lib" "$CARD/anyka_rust/boot/"
fi
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
[ ! -d "$DIST/boot/bin" ] || cp "$DIST"/boot/bin/* "$DBG/"

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

# --- OTA update package (OTA-01) ----------------------------------------------
# Same epoch as the archives above, so the payload is reproducible. ./x kup
# strips bin/ itself and verifies the signed result against the public keys
# the updater is built with (crates/ota-manifest/keys), so a key mismatch
# fails here instead of on every camera.
SUMS="$NAME-sdcard.tar.gz $NAME-debug.tar.gz mtdpatch-armv5"
rm -f "$OUT/$NAME-update.kup" "$OUT/manifest.json" "$OUT/manifest.json.minisig"
if [ -n "${MINISIGN_SECRET_KEY:-}" ]; then
	KEY=$WORK/minisign.key
	(umask 077 && printf '%s\n' "$MINISIGN_SECRET_KEY" > "$KEY")
	SOURCE_DATE_EPOCH=$EPOCH "$HERE/x" kup --dist "$DIST" --out "$OUT" --version "$VERSION" --key "$KEY"
	rm -f "$KEY"
	SUMS="$SUMS $NAME-update.kup manifest.json manifest.json.minisig"
else
	echo "warning: MINISIGN_SECRET_KEY not set; no OTA update package (.kup) built" >&2
fi

# shellcheck disable=SC2086 # SUMS is a list of plain file names
(cd "$OUT" && sha256sum $SUMS > SHA256SUMS)
cat "$OUT/SHA256SUMS"
