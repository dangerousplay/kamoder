#!/bin/sh
# install-toolchain.sh — install the Anyka uClibc cross toolchain into /opt.
#
# The toolchain (arm-anykav200-linux-uclibcgnueabi-*, GCC 4.8.5, uClibc
# 0.9.33.2) is the one Anyka ships with its SDK, mirrored in
# https://github.com/helloworld-spec/qiwen. Its binaries are 32-bit x86, so a
# 64-bit Debian/Ubuntu host needs the i386 C runtime.
#
#   tools/install-toolchain.sh             download, verify (if pinned), install
#   CROSSTOOL_SHA256=<hex> tools/...       refuse a tarball with another hash
#   CROSSTOOL_TARBALL=/path/file.tar.bz2   use an already downloaded copy
#
# Installs to /opt/arm-anykav200-crosstool (paths inside the tarball are
# absolute). Needs sudo unless run as root.
set -eu

URL=${CROSSTOOL_URL:-https://github.com/helloworld-spec/qiwen/raw/main/anycloud39ev300/SDK/tools/anyka_uclibc_gcc.tar.bz2}
PREFIX=/opt/arm-anykav200-crosstool
SUDO=
[ "$(id -u)" = 0 ] || SUDO=sudo

if [ -x "$PREFIX/usr/bin/arm-anykav200-linux-uclibcgnueabi-gcc" ]; then
	echo "toolchain already installed at $PREFIX"
	exit 0
fi

if command -v apt-get >/dev/null 2>&1; then
	$SUDO dpkg --add-architecture i386
	$SUDO apt-get update -q
	$SUDO apt-get install -y -q libc6:i386 libstdc++6:i386 zlib1g:i386
fi

TARBALL=${CROSSTOOL_TARBALL:-}
if [ -z "$TARBALL" ]; then
	TARBALL=$(mktemp --suffix=.tar.bz2)
	trap 'rm -f "$TARBALL"' EXIT
	curl -fsSL --retry 3 -o "$TARBALL" "$URL"
fi

got=$(sha256sum "$TARBALL" | cut -d' ' -f1)
if [ -n "${CROSSTOOL_SHA256:-}" ]; then
	[ "$got" = "$CROSSTOOL_SHA256" ] || {
		echo "toolchain sha256 $got != pinned $CROSSTOOL_SHA256" >&2
		exit 1
	}
	echo "toolchain sha256 verified ($got)"
else
	echo "WARNING: CROSSTOOL_SHA256 not set; tarball sha256 is $got" >&2
	echo "         pin it to make later installs reproducible" >&2
fi

# -P: the archive stores absolute paths under /opt.
$SUDO tar -Pxjf "$TARBALL"
"$PREFIX/usr/bin/arm-anykav200-linux-uclibcgnueabi-gcc" --version | head -n1
echo "installed to $PREFIX"
