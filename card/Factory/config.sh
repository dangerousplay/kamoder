#! /bin/sh
# SD card boot hook. The AK3918EV200 firmware runs it at boot from the
# factory-test check in /usr/sbin/service.sh. The AK3918AV100 stock firmware
# runs no SD card script at all (verified against every local dump), so there
# it only runs when started by hand: sh /mnt/disc1/Factory/config.sh
#
# If anyka_rust/ is on the card and camera.conf does not say run_rust=0, the
# Rust stack starts. Otherwise the original anyka_happy path runs unchanged.
#
# A/B slots (OTA-09, 12, 13; design docs/.specs ota §3): anyka_rust/ is the
# data root; the code may be in anyka_rust/slots/a or slots/b. The slot is
# chosen, in order, by:
#   1. boot/bin/kboot select (the redundant selector records; counts trial
#      boots, rolls back), which prints KSLOT=, KSLOT_DIR=, KTRIAL=;
#      KTRIAL=1 also starts `kboot guard` (reboots if never committed);
#   2. boot/last (the slot kboot chose last time), if kboot is missing,
#      fails or prints something unusable;
#   3. the legacy tree, anyka_rust/ itself (no slots at all: as before).
# Slot a is slots/a if that exists, else anyka_rust/ itself (a legacy tree
# is slot a until the first update replaces it); slot b is slots/b. A slot is
# only used if its directory has init.sh. Each decision goes to logs/boot.log.
#
# Test hooks (never set at boot): KAMODER_SD_ROOTS replaces the card search
# list and skips the mount; KAMODER_TMP replaces /tmp.

# Record every call in RAM, before anything can fail: /tmp is always
# writable, the card may not be mounted yet. This is how to tell whether the
# firmware ran this hook at all on a given boot.
# "[uptime]", or "[local time] [uptime]" once the clock has been set (NTP):
# at boot the camera has no RTC and reads 1970, so only uptime means anything.
stamp() {
  read -r _up _ < /proc/uptime
  _wall=$(date '+%Y-%m-%d %H:%M:%S %z' 2>/dev/null) || _wall=
  case $_wall in
    19* | 20[01]* | 202[0-5]* | '') printf '[%s]' "$_up" ;;
    *) printf '[%s] [%s]' "$_wall" "$_up" ;;
  esac
}
KTMP=${KAMODER_TMP:-/tmp}
echo "$(stamp) config.sh called as $0 $*" >> "$KTMP/anyka_config_sh.log"

# On the AV100, /ipc/setup.sh mounts the card only to look for vendor files
# and unmounts it again, so a hook that runs later can find it unmounted.
# Mount it here unless something already has.
if [ -z "${KAMODER_SD_ROOTS:-}" ] && ! grep -q '^/dev/mmcblk0' /proc/mounts && [ -b /dev/mmcblk0p1 ]; then
  mkdir -p /mnt/disc1
  mount /dev/mmcblk0p1 /mnt/disc1 && echo "$(stamp) config.sh mounted the card" >> "$KTMP/anyka_config_sh.log"
fi

# The card is /mnt/disc1 on the AV100 (/mnt is a 16 MB tmpfs there) and /mnt
# on the EV200. Use whichever holds this script's siblings.
SD=
for root in ${KAMODER_SD_ROOTS:-/mnt/disc1 /mnt}; do
  if [ -d "$root/anyka_rust" ] || [ -d "$root/anyka_happy" ]; then
    SD=$root
    break
  fi
done

RUST=$SD/anyka_rust
run_rust=1
start_telnetd=1
[ -n "$SD" ] && [ -f "$RUST/camera.conf" ] && . "$RUST/camera.conf"

# Directory of slot $1 (a or b), on stdout; non-zero for any other name.
slot_dir() {
  case $1 in
    a) if [ -d "$RUST/slots/a" ]; then echo "$RUST/slots/a"; else echo "$RUST"; fi ;;
    b) echo "$RUST/slots/b" ;;
    *) return 1 ;;
  esac
}

# Whether $1 is a directory the stack can start from.
runnable() {
  [ -n "$1" ] && [ -f "$1/init.sh" ]
}

# Something to start: the legacy tree or either slot.
has_stack() {
  runnable "$RUST" || runnable "$RUST/slots/a" || runnable "$RUST/slots/b"
}

# Runs `kboot select` with a time limit (a stuck kboot must not stop the
# boot: the shell fallbacks below still work). Prints its stdout; returns
# its status, or 124 on timeout. Polls a status file, not the pid: a child
# that has exited stays a zombie, which `kill -0` still finds.
KBOOT_TIMEOUT=20
kboot_select() {
  _out=$KTMP/kamoder-kboot.out
  _rc=$KTMP/kamoder-kboot.rc
  _kpid=$KTMP/kamoder-kboot.pid
  rm -f "$_out" "$_rc" "$_kpid"
  (
    "$KBOOT" select --root "$RUST" > "$_out" &
    echo $! > "$_kpid"
    wait $!
    echo $? > "$_rc"
  ) &
  _pid=$!
  _i=0
  while [ ! -s "$_rc" ]; do
    if [ "$_i" -ge "$KBOOT_TIMEOUT" ]; then
      read -r _k < "$_kpid" 2>/dev/null && kill -9 "$_k" 2>/dev/null
      kill -9 "$_pid" 2>/dev/null
      rm -f "$_kpid" "$_out"
      return 124
    fi
    sleep 1
    _i=$((_i + 1))
  done
  read -r _status < "$_rc" || _status=1
  cat "$_out" 2>/dev/null
  rm -f "$_out" "$_rc" "$_kpid"
  return "$_status"
}

# Sets KSLOT, KSLOT_DIR and KTRIAL. Logs every decision (stdout is the boot
# log). Never fails: the last resort is the legacy tree.
choose_slot() {
  KSLOT=
  KSLOT_DIR=
  KTRIAL=0
  KBOOT=$RUST/boot/bin/kboot
  if [ -f "$KBOOT" ]; then
    if _sel=$(kboot_select); then
      # Tried in a subshell first: a syntax error in `eval` would end this
      # shell, and with it the boot.
      if [ -n "$_sel" ] && (eval "$_sel") >/dev/null 2>&1; then
        eval "$_sel"
      fi
      case "$KSLOT:$KTRIAL" in
        a:0 | a:1 | b:0 | b:1) ;;
        *) KSLOT= ;;
      esac
      if [ -n "$KSLOT" ] && runnable "$KSLOT_DIR"; then
        echo "$(stamp) config.sh: kboot chose slot $KSLOT ($KSLOT_DIR), trial=$KTRIAL"
        return 0
      fi
      echo "$(stamp) config.sh: kboot output unusable: '$_sel'"
    else
      echo "$(stamp) config.sh: kboot select failed (status $?)"
    fi
  else
    echo "$(stamp) config.sh: no $KBOOT"
  fi
  KSLOT=
  KSLOT_DIR=
  KTRIAL=0

  _last=
  if [ -f "$RUST/boot/last" ]; then
    read -r _last < "$RUST/boot/last" || :
    if _dir=$(slot_dir "$_last") && runnable "$_dir"; then
      KSLOT=$_last
      KSLOT_DIR=$_dir
      echo "$(stamp) config.sh: boot/last says slot $KSLOT ($KSLOT_DIR)"
      return 0
    fi
    echo "$(stamp) config.sh: boot/last ('$_last') names no runnable slot"
  fi

  if runnable "$RUST"; then
    KSLOT=a
    KSLOT_DIR=$RUST
    echo "$(stamp) config.sh: legacy tree $RUST"
    return 0
  fi
  # No legacy tree (it was replaced by slots/a) and nothing better known.
  for _s in a b; do
    _dir=$RUST/slots/$_s
    if runnable "$_dir"; then
      KSLOT=$_s
      KSLOT_DIR=$_dir
      echo "$(stamp) config.sh: last resort: slot $KSLOT ($KSLOT_DIR)"
      return 0
    fi
  done
  return 0
}

if [ -n "$SD" ] && [ "$run_rust" = 1 ] && has_stack; then
  mkdir -p "$RUST/logs" 2>/dev/null
  BOOT_LOG=$RUST/logs/boot.log
  # One generation is enough for these short, once-per-boot logs.
  for log in "$BOOT_LOG"; do
    if [ -f "$log" ] && [ "$(wc -c < "$log")" -gt 262144 ]; then
      mv -f "$log" "$log.1"
    fi
  done
  {
    echo "$(stamp) config.sh: rust stack from $RUST"

    # Rescue shell first: if anything below fails, this is the way back in.
    if [ "$start_telnetd" = 1 ]; then
      telnetd 2>/dev/null &
    fi

    choose_slot
    if [ -z "$KSLOT_DIR" ]; then
      echo "$(stamp) config.sh: no runnable slot, nothing started"
      exit 0
    fi
    if [ "$KTRIAL" = 1 ]; then
      # Trial boot: unless the new slot commits itself within the window,
      # the guard reboots and kboot counts the try (OTA-10). Detached and
      # immune to SIGHUP so it outlives this script.
      (trap "" HUP; exec "$KBOOT" guard --root "$RUST" --window 600) < /dev/null &
      echo "$(stamp) config.sh: trial guard started (pid $!)"
    fi

    # Wi-Fi is not set up here: wifi runs under the supervisor and
    # reconciles it continuously, with rollback to the last working network.
    # Never /ipc/ipc: it resets the Wi-Fi configuration.
    KAMODER_DATA=$RUST KAMODER_SLOT=$KSLOT_DIR sh "$KSLOT_DIR/init.sh"
    status=$?
    echo "$(stamp) config.sh: init.sh returned $status"
  } >> "$BOOT_LOG" 2>&1
  exit 0
fi

# ---------------------------------------------------------------------------
# Legacy anyka_happy path (EV200 layout, from Gerge's SD-card project). Only
# when that tree is on the card; otherwise there is nothing to hand over to.
# ---------------------------------------------------------------------------
[ -n "$SD" ] && [ -d "$SD/anyka_happy" ] || exit 0
telnetd &
if ! [[ -f /etc/jffs2/gergehappy.sh ]]; then
  #not installed yet, so copy it
  cp /mnt/anyka_happy/gergehappy.sh /etc/jffs2/gergehappy.sh
fi

if ! [[ -f /etc/jffs2/gergesettings.txt ]]; then
  #not installed yet, so copy it
  cp /mnt/anyka_happy/gergesettings.txt /etc/jffs2/gergesettings.txt
fi
#gergedaemon and time_zone are not needed for the SD exploit
/etc/jffs2/gergehappy.sh
