# ===========================================================================
# KAMODER PATCH — replaces the vendor "#stop ipc" tail (tools/autostart).
# Original tail did: q-abort prompt, mounted the SD only to scan for vendor
# files then unmounted it, ran DateWhenIpcRunning.sh, then `/ipc/ipc &`.
# We do NOT start /ipc/ipc: it plays the "not configured" beep from its
# QR-pairing thread and rewrites the hand-maintained Wi-Fi config.
#
# Everything ABOVE this line is byte-for-byte the vendor setup.sh: sensor/ISP/
# ADC/eFuse/ak_pcm/venc modules, clk drive strength, eth0, USB+Wi-Fi module
# load, GPIO 98 USB-Wi-Fi power, and the DTB self-check. That hardware and
# network bring-up is kept because the camera must stay reachable and video
# needs the ISP later.
# ===========================================================================

# presetmotor.ko is normally insmod'd by /ipc/ipc, not by setup.sh. Our stack
# never runs ipc, so load it here to keep PTZ working. Harmless if absent.
if [ -f /ipc/patch/ko/presetmotor.ko ]; then
	insmod /ipc/patch/ko/presetmotor.ko 2>/dev/null
fi

# --- Hand over to the SD-card Rust stack, or fall back to vendor ipc --------
SD_MNT=/mnt/disc1
SD_DEV=/dev/mmcblk0p1
CONFIG_SH=$SD_MNT/Factory/config.sh
SD_WAIT=20            # seconds to wait for the card node + a good mount

# Wait for the SD block device to enumerate (mmc modules were insmod'd in
# rc.local just before us; on a cold boot the card can take a moment).
i=0
while [ $i -lt $SD_WAIT ] && [ ! -b $SD_DEV ]; do
	sleep 1
	i=$((i + 1))
done

# Mount it unless rc.local or something else already did. /mnt is a 16 MB
# tmpfs on the AV100; the card belongs on /mnt/disc1.
if [ -b $SD_DEV ] && ! grep -q " $SD_MNT " /proc/mounts; then
	mkdir -p $SD_MNT
	mount -t vfat $SD_DEV $SD_MNT 2>/dev/null || mount $SD_DEV $SD_MNT 2>/dev/null
fi

if [ -f "$CONFIG_SH" ]; then
	# config.sh starts the Rust stack if anyka_rust/ is present and
	# camera.conf does not say run_rust=0; otherwise it runs the legacy
	# anyka_happy path. It is backgrounded so a hang there can never stop
	# this boot script from returning.
	echo "kamoder: handing boot to $CONFIG_SH"
	sh "$CONFIG_SH" &
else
	# No card, or a card without our hook. The vendor app is never started
	# (user decision 2026-10-11): the hardware is up but nothing joins Wi-Fi,
	# so the camera stays offline until a card with the hook is inserted and
	# the camera rebooted. The UART console still works.
	echo "kamoder: no $CONFIG_SH; vendor ipc disabled, nothing started"
fi
