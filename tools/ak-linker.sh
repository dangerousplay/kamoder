#!/bin/sh
# Linker wrapper for the Anyka SDK.
#
# Two things have to be fixed up before the cross-gcc sees the command line,
# and neither can be done with `-C link-arg` because rustc appends those after
# the libraries, which is too late to matter.
#
# 1. `-rpath-link`. The SDK shared objects carry DT_NEEDED entries for each
#    other but no SONAME and no run path, so `ld` cannot find them when they
#    are pulled in indirectly (`libapp_rtsp.so` needs `libakispsdk.so`, and so
#    on). Without this the link fails on undefined `AK_ISP_*` symbols even
#    though every object is sitting in sdk/lib. It only affects link time and
#    adds nothing to the binary.
#
# 2. `--no-as-needed` around the SDK. rustc always passes `--as-needed`, which
#    drops any shared library the Rust code does not reference *directly*. Our
#    binaries call `libplat_vi`, which calls `libakispsdk`, so `libakispsdk`
#    gets dropped and its symbols go undefined. `--as-needed` is positional,
#    so the fix is to bracket just `-lanyka_sdk_selected` (the generated ld
#    script holding this binary's SDK objects — see anyka-link) and leave rustc's own
#    libraries as-needed: the camera's uClibc merges libpthread/librt/libdl
#    into libc, so pulling those in unconditionally would produce binaries
#    that cannot start.
#
# 3. `-rpath $ORIGIN/../lib/sys`, on every binary. That directory on the card
#    holds the runtime the crosstool links against but the firmware does not
#    ship under those names (empty libpthread/libm/librt/libdl stubs) plus the
#    GCC runtime; see sdk/runtime/README.md. It goes last, after the SDK
#    variant directory anyka-link adds. It has to be baked in here: patchelf'd
#    binaries crash in the camera's ld.so.
#
# Point `ANYKA_CC` at a different cross-gcc, or `ANYKA_SDK_LIB` at a different
# SDK, if they live elsewhere.
set -eu

CC="${ANYKA_CC:-/opt/arm-anykav200-crosstool/usr/bin/arm-anykav200-linux-uclibcgnueabi-gcc}"
SDK_LIB="${ANYKA_SDK_LIB:-$(CDPATH= cd -- "$(dirname -- "$0")/../sdk/lib" && pwd)}"

# Rewrite the argument list in place: append the replacement, drop the
# original. `for arg` iterates the list as it was when the loop started, so
# after one pass only the rewritten arguments are left.
for arg do
    case "$arg" in
        -lanyka_sdk_selected)
            set -- "$@" "-Wl,--no-as-needed" "$arg" "-Wl,--as-needed"
            ;;
        *)
            set -- "$@" "$arg"
            ;;
    esac
    shift
done

# Single quotes: `$ORIGIN` is for the loader, not the shell.
exec "$CC" -Wl,-rpath-link,"$SDK_LIB" "$@" '-Wl,-rpath,$ORIGIN/../lib/sys'
