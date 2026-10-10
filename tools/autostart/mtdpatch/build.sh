#!/bin/sh
# Build mtdpatch (static ARMv5, uClibc) with the Anyka cross toolchain.
set -eu
cd "$(dirname "$0")"
CROSS=${ANYKA_TOOLCHAIN:-/opt/arm-anykav200-crosstool}/usr/bin/arm-anykav200-linux-uclibcgnueabi-
"${CROSS}gcc" -std=gnu99 -Wall -Wextra -O2 -static mtdpatch.c -o mtdpatch
"${CROSS}strip" mtdpatch
echo "built $(pwd)/mtdpatch"
