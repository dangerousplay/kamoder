#!/usr/bin/env bash
#
# QEMU ARMv5 (ARM926EJ-S) sandbox for the Anyka AK3918 camera.
#
# Purpose: a local, legal dev environment to rework the GPL kernel modules and
# experiment with kernel-version upgrades for an AK3918AV100 camera
# (armv5tejl, Linux 4.4.282 on the device).
#
# IMPORTANT — why we do NOT boot the vendor kernel directly:
#   * QEMU has no AK3918 machine model. The vendor kernel probes Anyka UART /
#     timer / interrupt controller that do not exist on any stock QEMU board.
#   * The vendor kernel is linked at PHYS_OFFSET 0x80000000 (uImage load addr
#     0x80008000); the closest QEMU arm926 board (versatilepb) has RAM at
#     0x00000000. The kernel hangs in early asm before any console output.
#
# So this script boots a *versatilepb-targeted* kernel (built from the bundled
# 3.4.35 source, or any kernel you point KERNEL= at) and mounts the camera's
# extracted squashfs rootfs. Same CPU/ISA + same userland ABI => a faithful
# place to compile and test module logic. It is NOT vermagic-compatible with
# the device's 4.4.282 build, so .ko binaries are not interchangeable; rebuild
# modules against whichever kernel tree this VM runs.
#
# Subcommands:
#   extract   strip uImage header, pull DTB + squashfs out of the flash dump
#   rootfs    unpack the squashfs and repack it as a cpio initramfs
#   build     cross-compile a versatilepb 4.4 kernel (uClibc gcc 4.9.4)
#   run       boot QEMU (default if a kernel image already exists)
#   all       extract + rootfs + build + run
#   clean     remove generated artifacts (keeps toolchain + worktree)
#
# Env overrides:
#   DUMP=<dir>        your camera's flash dump: a dir with mtdblocks/mtd*.bin
#                     (default: latest firmware_dump/backup_* at the repo root)
#   KERNEL=<file>     kernel image to boot (zImage/Image/uImage); skips build
#   KSRC=<dir>        kernel source tree to build (default: v4.4 worktree)
#   KTAG=<tag>        git tag to check out from LINUXGIT (default v4.4)
#   LINUXGIT=<dir>    local linux clone (default ~/src/linux; else download)
#   TOOLCHAIN_DIR=<d> arm-unknown-linux-uclibcgnueabi gcc for the VM kernel
#                     (default tools/qemu-armv5/toolchain/...)
#   RDINIT=<path>     guest init (default /bin/sh; /sbin/init = vendor boot)
#   FW=stock|dev      firmware variant (default dev). stock = pristine device
#                     rootfs; dev = stock + OVERLAY merged on top.
#   OVERLAY=<dir>     dev overlay tree (default tools/qemu-armv5/overlay)
#   MEM=<MB>          guest RAM (default 128)
#   JOBS=<n>          make -j (default nproc)
#
# Examples:
#   FW=stock ./run.sh run     # boot reference firmware
#   FW=dev   ./run.sh run     # boot our firmware (default)
# Inside the guest: `cat /etc/firmware-variant` shows which booted.
set -euo pipefail

# --- paths -------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
OUT="$SCRIPT_DIR/out"                       # generated artifacts
TOOLCHAIN_DIR="${TOOLCHAIN_DIR:-$SCRIPT_DIR/toolchain/arm-unknown-linux-uclibcgnueabi}"
CROSS="$TOOLCHAIN_DIR/bin/arm-unknown-linux-uclibcgnueabi-"

# Kernel source for the VM. The device runs Linux 4.4.282; we stay on the 4.4
# line (never downgrade to the vendor 3.4 tree). Source is acquired, in order:
#   1. KSRC=<dir>            an existing kernel tree you point at, else
#   2. a git worktree at KTAG cut from a local clone (LINUXGIT), else
#   3. download KURL (sandbox network may be throttled).
# Exact stable .282 lives in the linux-4.4.y stable tree; the local mainline
# clone only carries the base v4.4 tag, so KTAG defaults to v4.4 (same LTS
# series). Set KTAG=v4.4.282 once a stable remote is fetchable.
# Version string the built kernel must REPORT (uname -r / module vermagic).
# The device's stock modules carry "vermagic=4.4.282 mod_unload ARMv5" and use
# NO modversions, so matching this exact release string (plus the same vermagic
# config flags below) lets the stock .ko load and lets our rebuilt modules be
# binary-compatible with the camera. We build the latest 4.4.x source we can
# obtain offline (base v4.4 here) and relabel it to KREL via SUBLEVEL override.
KREL="${KREL:-4.4.282}"
LINUXGIT="${LINUXGIT:-$HOME/src/linux}"
KTAG="${KTAG:-v4.4}"
KVER="${KVER:-4.4}"
KSRC="${KSRC:-$SCRIPT_DIR/linux-$KVER}"
KURL="${KURL:-https://cdn.kernel.org/pub/linux/kernel/v4.x/linux-$KVER.tar.xz}"
KBUILD="$SCRIPT_DIR/kbuild"                 # out-of-tree O= build dir

# Firmware variant to build/boot:
#   stock = the camera's extracted rootfs, untouched (reference behavior)
#   dev   = stock + OVERLAY merged on top (our custom modules / binaries)
# The overlay is the dev work tree: drop files at the path they should land in
# the guest (e.g. overlay/lib/modules/4.4.0/presetmotor.ko, overlay/etc/...).
FW="${FW:-dev}"
OVERLAY="${OVERLAY:-$SCRIPT_DIR/overlay}"

MEM="${MEM:-128}"
JOBS="${JOBS:-$(nproc)}"

log()  { printf '\033[1;32m[qemu-armv5]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[qemu-armv5]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[qemu-armv5] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

# --- locate firmware dump ----------------------------------------------------
find_dump() {
    [[ -n "${DUMP:-}" ]] && { echo "$DUMP"; return; }
    local d
    for d in $(ls -d "$REPO_ROOT"/firmware_dump/backup_* 2>/dev/null | sort -r); do
        if [[ -s "$d/mtdblocks/mtd4.bin" ]]; then echo "$d"; return; fi
    done
    die "no flash dump found: set DUMP=<dir> (with mtdblocks/mtd4.bin) or put one under firmware_dump/backup_*"
}

# --- extract -----------------------------------------------------------------
do_extract() {
    local dump; dump="$(find_dump)"
    log "using dump: $dump"
    mkdir -p "$OUT"
    local mtd="$dump/mtdblocks"

    # uImage header is 64 bytes; payload is the raw (uncompressed) ARM kernel.
    [[ -s "$mtd/mtd4.bin" ]] || die "missing $mtd/mtd4.bin"
    dd if="$mtd/mtd4.bin" of="$OUT/vendor-uImage" bs=1 count=64 \
        skip=0 status=none 2>/dev/null || true
    cp "$mtd/mtd4.bin" "$OUT/vendor-uImage"            # full uImage (QEMU can parse)
    dd if="$mtd/mtd4.bin" of="$OUT/vendor-Image" bs=64 skip=1 status=none
    log "vendor kernel : $OUT/vendor-uImage (uImage) + vendor-Image (raw payload)"

    # DTB (informational; versatilepb uses its own machine, not this DTB)
    if [[ -s "$mtd/mtd3.bin" ]]; then
        cp "$mtd/mtd3.bin" "$OUT/vendor.dtb"
        log "vendor DTB    : $OUT/vendor.dtb"
    fi

    # rootfs squashfs -> raw image used as the guest SD card
    [[ -s "$mtd/mtd5.bin" ]] || die "missing $mtd/mtd5.bin (rootfs)"
    cp "$mtd/mtd5.bin" "$OUT/rootfs.squashfs"
    log "rootfs        : $OUT/rootfs.squashfs ($(du -h "$OUT/rootfs.squashfs" | cut -f1))"

    # APP partition (also squashfs) for reference
    [[ -s "$mtd/mtd6.bin" ]] && cp "$mtd/mtd6.bin" "$OUT/app.squashfs"
}

# --- acquire kernel source ---------------------------------------------------
do_fetch() {
    [[ -f "$KSRC/Makefile" ]] && { log "kernel source present: $KSRC"; return; }

    # Preferred: a git worktree of KTAG from a local clone (no network).
    if [[ -d "$LINUXGIT/.git" ]] || git -C "$LINUXGIT" rev-parse --git-dir &>/dev/null; then
        if git -C "$LINUXGIT" rev-parse -q --verify "${KTAG}^{commit}" >/dev/null; then
            log "creating worktree $KSRC @ $KTAG from $LINUXGIT"
            git -C "$LINUXGIT" worktree add --detach "$KSRC" "$KTAG"
            [[ -f "$KSRC/Makefile" ]] && return
        else
            warn "$KTAG not found in $LINUXGIT (stable point releases live in linux-4.4.y)"
        fi
    fi

    # Fallback: download a release tarball.
    local tarball="$SCRIPT_DIR/linux-$KVER.tar.xz"
    if [[ ! -f "$tarball" ]]; then
        log "downloading $KURL ..."
        command -v curl >/dev/null || die "curl required to fetch kernel source"
        curl -fL --retry 3 -o "$tarball" "$KURL" || die "download failed: $KURL"
    fi
    log "extracting $tarball ..."
    tar xf "$tarball" -C "$SCRIPT_DIR"
    [[ -f "$KSRC/Makefile" ]] || die "extraction did not yield $KSRC/Makefile"
}

# --- build initramfs from the extracted rootfs -------------------------------
# versatilepb's mmci-pl18x needs a regulator the stock 4.4 board file doesn't
# provide (probe fails -22), so /dev/mmcblk0 never appears. We sidestep block
# storage entirely: unpack the squashfs and repack it as a cpio initramfs the
# kernel mounts as a writable rootfs. fakeroot preserves device nodes/perms.
do_rootfs() {
    [[ -f "$OUT/rootfs.squashfs" ]] || die "run '$0 extract' first (no rootfs.squashfs)"
    command -v unsquashfs >/dev/null || die "squashfs-tools (unsquashfs) required"
    command -v fakeroot   >/dev/null || die "fakeroot required"
    case "$FW" in stock|dev) ;; *) die "FW must be 'stock' or 'dev' (got '$FW')";; esac

    # Unpack the pristine stock rootfs once; reuse for both variants.
    local pristine="$OUT/rootfs.pristine"
    if [[ ! -d "$pristine" ]]; then
        log "unpacking stock squashfs"
        fakeroot unsquashfs -d "$pristine" -f "$OUT/rootfs.squashfs" >/dev/null 2>&1
    fi

    local stage="$OUT/rootfs.$FW" cpio="$OUT/initramfs-$FW.cpio.gz"
    rm -rf "$stage" "$cpio"
    mkdir -p "$OVERLAY"
    log "assembling '$FW' firmware initramfs"
    fakeroot sh -c "
        set -e
        cp -a '$pristine' '$stage'
        if [ '$FW' = dev ]; then
            # our freshly-built modules (staged by do_build at the KREL path)
            [ -d '$OUT/modules/lib/modules' ] && cp -a '$OUT/modules/lib/modules/.' '$stage/lib/modules/'
            # hand-placed overlay files win last
            [ -n \"\$(ls -A '$OVERLAY' 2>/dev/null)\" ] && cp -a '$OVERLAY/.' '$stage/'
        fi
        cd '$stage'
        [ -d dev ] || mkdir dev
        [ -e dev/console ] || mknod dev/console c 5 1
        mkdir -p etc
        echo '$FW' > etc/firmware-variant
        find . | cpio -o -H newc 2>/dev/null | gzip -9 > '$cpio'
    "
    [[ -s "$cpio" ]] || die "initramfs build produced empty output"
    log "initramfs     : $cpio ($(du -h "$cpio" | cut -f1)) [variant: $FW]"
}

# --- build kernel ------------------------------------------------------------
# Apply patches/*.patch to the kernel tree, once each. A patch that is already
# in (reverse-applies cleanly) is skipped, so rebuilding is idempotent.
do_patch() {
    local patch
    for patch in "$SCRIPT_DIR"/patches/*.patch; do
        [[ -e $patch ]] || continue
        if patch -d "$KSRC" -p1 -R --dry-run -s -f < "$patch" >/dev/null 2>&1; then
            continue
        fi
        log "applying ${patch##*/}"
        patch -d "$KSRC" -p1 -s < "$patch" || die "cannot apply ${patch##*/}"
    done
}

do_build() {
    [[ -x "${CROSS}gcc" ]] || die "cross gcc not found at ${CROSS}gcc (extract toolchain first)"
    do_fetch
    do_patch
    export ARCH=arm CROSS_COMPILE="$CROSS"
    mkdir -p "$KBUILD"

    # Relabel the kernel release to KREL (e.g. 4.4.282) via make var overrides.
    # KREL is X.Y.Z[suffix]; we keep VERSION.PATCHLEVEL from source, override
    # SUBLEVEL/EXTRAVERSION so UTS_RELEASE == KREL exactly. These overrides are
    # passed to EVERY make call so kernel.release is generated consistently.
    local sub="${KREL#*.}"; sub="${sub#*.}"        # strip "X.Y." -> "Z[suffix]"
    local subnum="${sub%%[!0-9]*}"                 # numeric part
    local extra="${sub#$subnum}"                   # trailing suffix (e.g. -rc1), usually empty
    local -a VER=( SUBLEVEL="$subnum" EXTRAVERSION="$extra" LOCALVERSION= )
    log "forcing UTS_RELEASE=$KREL (SUBLEVEL=$subnum EXTRAVERSION='$extra')"

    log "configuring versatile_defconfig"
    make -C "$KSRC" O="$KBUILD" "${VER[@]}" versatile_defconfig >/dev/null

    # Config fragment: (a) vermagic match, (b) filesystems/subsystems the
    # firmware dump shows the device uses, (c) what this VM needs to boot.
    local cfg="$KBUILD/.config"
    cat > "$KBUILD/.config-frag" <<'EOF'
# --- vermagic match: "4.4.282 mod_unload ARMv5" (device modules, no modversions)
CONFIG_MODULES=y
CONFIG_MODULE_UNLOAD=y
# CONFIG_MODVERSIONS is not set
# CONFIG_SMP is not set
CONFIG_PREEMPT_NONE=y
# CONFIG_PREEMPT is not set
# CONFIG_PREEMPT_VOLUNTARY is not set
# CONFIG_LOCALVERSION_AUTO is not set
CONFIG_LOCALVERSION=""
# p2v8 in ARM vermagic comes from ARM_PATCH_PHYS_VIRT (default y, only settable
# under EMBEDDED). Device kernel lacks it, so enable EMBEDDED to unlock it off.
CONFIG_EXPERT=y
CONFIG_EMBEDDED=y
# CONFIG_ARM_PATCH_PHYS_VIRT is not set
# with p2v off, PHYS_OFFSET must be fixed; versatilepb RAM base = 0x0
CONFIG_PHYS_OFFSET=0x00000000
# --- filesystems present on the device (proc/filesystems)
CONFIG_SQUASHFS=y
CONFIG_SQUASHFS_XZ=y
CONFIG_SQUASHFS_ZLIB=y
CONFIG_VFAT_FS=y
CONFIG_MSDOS_FS=y
CONFIG_FAT_FS=y
CONFIG_JFFS2_FS=y
CONFIG_NLS_CODEPAGE_437=y
CONFIG_NLS_ISO8859_1=y
CONFIG_NLS_UTF8=y
CONFIG_DEBUG_FS=y
CONFIG_DEVPTS_FS=y
CONFIG_TMPFS=y
CONFIG_DEVTMPFS=y
CONFIG_DEVTMPFS_MOUNT=y
# --- MTD: device root/app/rom live on SPI-NOR mtd partitions
CONFIG_MTD=y
CONFIG_MTD_BLOCK=y
CONFIG_MTD_CMDLINE_PARTS=y
# --- /dev/watchdog for wdt_feeder (the camera has a hardware one; softdog
# stands in so the Rust stack's integration tests run unmodified)
CONFIG_WATCHDOG=y
CONFIG_SOFT_WATCHDOG=y
# --- simulated camera devices for the Rust stack's integration tests
# SD card: QEMU's `-drive if=sd` on the PL181, which shows up as
# /dev/mmcblk0 exactly as on the camera. Built in: as a module it never
# loaded, so the card never appeared.
CONFIG_MMC_ARMMMCI=y
CONFIG_MMC_BLOCK=y
# Wi-Fi: the camera's SSV6x5x is a mac80211 driver; mac80211_hwsim gives the
# same cfg80211/mac80211 stack with simulated radios, and WEXT compat is what
# the firmware's `wpa_supplicant -Dwext` talks to.
CONFIG_WIRELESS=y
CONFIG_WLAN=y
CONFIG_CFG80211=y
CONFIG_CFG80211_WEXT=y
CONFIG_MAC80211=y
CONFIG_MAC80211_HWSIM=y
CONFIG_CRYPTO_AES=y
CONFIG_CRYPTO_CCM=y
CONFIG_CRYPTO_CMAC=y
CONFIG_CRYPTO_ARC4=y
# /rom: JFFS2 on NOR flash on the camera; JFFS2 on a RAM-backed MTD here.
CONFIG_MTD_MTDRAM=y
CONFIG_MTDRAM_TOTAL_SIZE=2048
CONFIG_MTDRAM_ERASE_SIZE=64
# The app partition (/ipc, bind-mounted on /usr) is a squashfs on loop.
CONFIG_BLK_DEV_LOOP=y
CONFIG_NET_NS=y
# --- misc the VM / userland wants
CONFIG_BLK_DEV_RAM=y
CONFIG_BLK_DEV_INITRD=y
CONFIG_BINFMT_MISC=y
EOF
    "$KSRC/scripts/kconfig/merge_config.sh" -m -O "$KBUILD" "$cfg" "$KBUILD/.config-frag" >/dev/null 2>&1 \
        || cat "$KBUILD/.config-frag" >> "$cfg"
    make -C "$KSRC" O="$KBUILD" "${VER[@]}" olddefconfig >/dev/null

    log "building zImage + modules as $KREL (-j$JOBS) — first build is slow"
    make -C "$KSRC" O="$KBUILD" "${VER[@]}" -j"$JOBS" zImage modules

    cp "$KBUILD/arch/arm/boot/zImage" "$OUT/zImage"
    local rel; rel="$(cat "$KBUILD/include/config/kernel.release" 2>/dev/null)"
    log "kernel        : $OUT/zImage (release: $rel)"
    [[ "$rel" == "$KREL" ]] || warn "kernel.release '$rel' != KREL '$KREL' — vermagic may not match!"

    # Stage built modules at the release path so the guest's depmod/modprobe
    # (which look under /lib/modules/$(uname -r)) find them.
    rm -rf "$OUT/modules"
    make -C "$KSRC" O="$KBUILD" "${VER[@]}" INSTALL_MOD_PATH="$OUT/modules" \
        modules_install >/dev/null 2>&1 || warn "modules_install failed (non-fatal)"
    log "modules staged: $OUT/modules/lib/modules/$rel"
}

# --- run ---------------------------------------------------------------------
do_run() {
    command -v qemu-system-arm >/dev/null || die "qemu-system-arm not installed"
    mkdir -p "$OUT"

    local kernel="${KERNEL:-}"
    if [[ -z "$kernel" ]]; then
        if   [[ -f "$OUT/zImage" ]];        then kernel="$OUT/zImage"
        elif [[ -f "$OUT/vendor-uImage" ]]; then kernel="$OUT/vendor-uImage"
             warn "no built zImage; falling back to the VENDOR kernel."
             warn "expect a silent hang (no AK3918 board / wrong PHYS_OFFSET). Run 'build' for a working VM."
        else die "no kernel image. Run '$0 build' (or set KERNEL=)."
        fi
    fi
    log "booting kernel: $kernel"

    local -a args=(
        -M versatilepb -cpu arm926 -m "$MEM"
        -kernel "$kernel"
        -nographic -no-reboot
    )

    # rootfs. Default RDINIT=/bin/sh drops straight to a shell; the vendor
    # /sbin/init respawns a getty on ttySAK0 (Anyka UART, absent here), so set
    # RDINIT=/sbin/init only to exercise the real vendor boot.
    local rdinit="${RDINIT:-/bin/sh}"
    local cpio="$OUT/initramfs-$FW.cpio.gz"
    [[ -f "$cpio" ]] || { log "no '$FW' initramfs yet; building it"; do_rootfs; }
    if [[ -f "$cpio" ]]; then
        log "firmware      : $FW"
        args+=( -initrd "$cpio" )
        args+=( -append "console=ttyAMA0,115200 rdinit=$rdinit mem=${MEM}M" )
    else
        warn "no initramfs for '$FW'. Booting without rootfs."
        args+=( -append "console=ttyAMA0,115200 mem=${MEM}M" )
    fi

    log "qemu cmd: qemu-system-arm ${args[*]}"
    log "exit QEMU with: Ctrl-A X"
    exec qemu-system-arm "${args[@]}"
}

do_clean() {
    rm -rf "$OUT" "$KBUILD"
    log "removed out/ and kbuild/ (toolchain + kernel worktree kept)"
}

# --- dispatch ----------------------------------------------------------------
case "${1:-run}" in
    extract) do_extract ;;
    fetch)   do_fetch ;;
    rootfs)  do_rootfs ;;
    build)   do_build ;;
    run)     do_run ;;
    all)     do_extract; do_rootfs; do_build; do_run ;;
    clean)   do_clean ;;
    *) die "usage: $0 {extract|fetch|rootfs|build|run|all|clean}" ;;
esac
