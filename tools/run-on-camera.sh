#!/bin/sh
# Launcher for the Rust binaries on the camera.
#
#   sh run-on-camera.sh venc_demo 200 8 4
#   sh run-on-camera.sh sshd --port 2222
#
# Nothing has to be set up first. Each binary carries an RPATH pointing at the
# exact SDK variant it was linked against (`lib/<variant>`, see the
# anyka-link crate) and at `lib/sys`, which holds the libc stubs and the GCC
# runtime the camera does not ship under the names the crosstool links
# against. The binaries in `bin/` can equally be run directly.
set -eu

# `dirname` is not present in every BusyBox build on these cameras, so the
# directory is taken with parameter expansion instead.
case "$0" in
    */*) SELF_DIR=${0%/*} ;;
    *)   SELF_DIR=. ;;
esac
BASE=$(CDPATH= cd -- "$SELF_DIR" && pwd)

if [ $# -lt 1 ]; then
    echo "usage: $0 <binary> [args...]" >&2
    echo "available:" >&2
    ls "$BASE/bin" >&2
    exit 1
fi

BINARY=$1
shift

if [ ! -x "$BASE/bin/$BINARY" ]; then
    echo "$0: no such binary: $BINARY" >&2
    exit 1
fi

unset LD_LIBRARY_PATH
exec "$BASE/bin/$BINARY" "$@"
