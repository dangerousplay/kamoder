#!/bin/sh
# autorun.sh - SD card boot hook for the AK3918AV100 firmware.
# Hands off to Factory/config.sh, which picks the Rust stack or anyka_happy.
if [ -f /mnt/disc1/Factory/config.sh ]; then
  /bin/sh /mnt/disc1/Factory/config.sh &
elif [ -f /mnt/Factory/config.sh ]; then
  /bin/sh /mnt/Factory/config.sh &
fi
