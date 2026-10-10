#! /bin/sh
# SD card boot hook. The AK3918EV200 firmware runs it at boot from the
# factory-test check in /usr/sbin/service.sh. The AK3918AV100 stock firmware
# runs no SD card script at all (verified against every local dump), so there
# it only runs when started by hand: sh /mnt/disc1/Factory/config.sh
#
# If anyka_rust/ is on the card and camera.conf does not say run_rust=0, the
# Rust stack starts. Otherwise the original anyka_happy path runs unchanged.

# Record every call in RAM, before anything can fail: /tmp is always
# writable, the card may not be mounted yet. This is how to tell whether the
# firmware ran this hook at all on a given boot.
read -r _up _ < /proc/uptime
echo "[$_up] config.sh called as $0 $*" >> /tmp/anyka_config_sh.log

# On the AV100, /ipc/setup.sh mounts the card only to look for vendor files
# and unmounts it again, so a hook that runs later can find it unmounted.
# Mount it here unless something already has.
if ! grep -q '^/dev/mmcblk0' /proc/mounts && [ -b /dev/mmcblk0p1 ]; then
  mkdir -p /mnt/disc1
  mount /dev/mmcblk0p1 /mnt/disc1 && echo "[$_up] config.sh mounted the card" >> /tmp/anyka_config_sh.log
fi

# The card is /mnt/disc1 on the AV100 (/mnt is a 16 MB tmpfs there) and /mnt
# on the EV200. Use whichever holds this script's siblings.
SD=
for root in /mnt/disc1 /mnt; do
  if [ -d "$root/anyka_rust" ] || [ -d "$root/anyka_happy" ]; then
    SD=$root
    break
  fi
done

RUST=$SD/anyka_rust
run_rust=1
start_telnetd=1
[ -n "$SD" ] && [ -f "$RUST/camera.conf" ] && . "$RUST/camera.conf"

if [ -n "$SD" ] && [ "$run_rust" = 1 ] && [ -f "$RUST/init.sh" ]; then
  mkdir -p "$RUST/logs" 2>/dev/null
  BOOT_LOG=$RUST/logs/boot.log
  # One generation is enough for these short, once-per-boot logs.
  for log in "$BOOT_LOG"; do
    if [ -f "$log" ] && [ "$(wc -c < "$log")" -gt 262144 ]; then
      mv -f "$log" "$log.1"
    fi
  done
  {
    read -r up _ < /proc/uptime
    echo "[$up] config.sh: rust stack from $RUST"

    # Rescue shell first: if anything below fails, this is the way back in.
    if [ "$start_telnetd" = 1 ]; then
      telnetd 2>/dev/null &
    fi

    # Wi-Fi is not set up here: wifi runs under the supervisor and
    # reconciles it continuously, with rollback to the last working network.
    # Never /ipc/ipc: it resets the Wi-Fi configuration.
    sh "$RUST/init.sh"
    echo "[$up] config.sh: init.sh returned $?"
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
