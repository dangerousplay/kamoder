# System tests: the SD card on a simulated camera

```sh
./x itest                          # build card + images, run everything (~10 min)
./x itest --no-build               # reuse the last card build
./x itest -- -k wifi -x            # anything after `--` goes to pytest
```

**pytest + [labgrid](https://labgrid.readthedocs.io/)**. labgrid boots the VM
(`QEMUDriver`) and drives its serial console (`ShellDriver`); the tests are
plain pytest. The same test code can later target the real camera by swapping
`env/qemu.yaml` for an environment that describes it — the VM-only scenarios
are marked `vm`.

## What is simulated

`tools/qemu-armv5` provides an ARM926EJ-S (versatilepb) VM on Linux 4.4.282
booting **the camera's own root filesystem**; `guest/firmware-init` then brings
it up the way the AK3918AV100 firmware does:

| Camera | VM |
|---|---|
| SD card, `/dev/mmcblk0p1`, FAT | QEMU SD card on the PL181, 128 MiB, MBR + FAT32 — same device name, same filesystem |
| `/rom` — JFFS2 on NOR flash | JFFS2 on an `mtdram` flash device |
| app partition on `/ipc`, bind-mounted on `/usr` | the same squashfs, from the firmware dump, on loop |
| Wi-Fi: SSV6x5x, a mac80211 driver | `mac80211_hwsim` radio 0 as `wlan0` |
| its `wpa_supplicant` 2.8, `iwconfig`, `udhcpc` | **the same binaries** |
| hardware watchdog | `softdog` |
| home router | radios 1 and 2 as access points **HomeNet** and **NewNet**: the firmware's own `hostapd` 2.8 (WPA2-PSK/CCMP) plus `udhcpd`, each in its own network namespace |

Nothing in the Wi-Fi path is a test double: association, the WPA2 4-way
handshake, DHCP and the gateway ARP check all happen over the simulated air.
The access points live in separate network namespaces (moved there by
`support/wiphy-netns`, since the camera has no `iw`), so their gateways are
genuinely remote — otherwise the camera would reach them over loopback.

`eth0` is not a camera device: it is the management link the host uses for
SSH, the web UI and ONVIF (QEMU user networking, port-forwarded at runtime;
no multicast, so WS-Discovery is tested with a unicast Probe).

The VM has no pan/tilt motor: `firmware-init` exports `ANYKA_ONVIF_PTZ=sim`,
so `onvif` (whose `--ptz` defaults to `auto` and reads that variable)
runs the simulated head.

## Layout

```
env/qemu.yaml          labgrid environment (paths come from LG_* variables)
guest/firmware-init    VM init: the firmware boot, then the console shell
guest/ap, ap-netns     access point control (create / up / down)
support/               wiphy-netns: move a radio to another netns (nl80211)
build-images.sh        sd.img + initramfs.cpio.gz
tests/conftest.py      Camera fixture: shell, processes, Wi-Fi state, logs
tests/test_0*.py       scenarios, run in file order on one booted VM
```

The test gets its own `camera.conf` with throwaway SSH, web admin and ONVIF
passwords (`LG_ITEST_PASSWORD`, `LG_ITEST_WEB_PASSWORD`,
`LG_ITEST_ONVIF_PASSWORD` in the tests). The real
card's `camera.conf`, `etc/`, `logs/` and `backup/` are never copied into the
images.

## Scenarios

* **devices** — card, flash, app partition, watchdog, radio, both APs on air,
  firmware joined HomeNet.
* **boot** — `autorun.sh` → `Factory/config.sh` → `init.sh` → supervisor;
  every service, the rescue telnetd, logs on the card, per-service log files
  without terminal escape codes.
* **ssh** — credentials hashed from `camera.conf` on the FAT card; from the
  host: wrong password rejected, root login, PTY, exit status.
* **watchdog** — held; SIGKILLed feeder restarted in time.
* **wifi** (`wifi`, timers shortened to seconds):
  - steady state: records last-good, touches nothing;
  - **moved to another working network → reconciled back** to the desired
    one, after recording the other as a way back; the APs are left alone;
  - the same while the last-good record cannot be saved (card read-only);
  - **moved away while the desired AP is down → rolls back** to the network
    it was moved to, and holds the desired one off;
  - desired network retried once the hold expires and its AP is back;
  - `camera.conf` change moves the camera; the original config is kept;
  - wrong password → rolled back; corrected password applied at once, not
    after the hold;
  - nothing desired: a move to a working network is adopted, not undone;
  - AP outage ridden out with backoff, no rollback.
* **web** (`web` on port 80, forwarded): admin seeded from
  `camera.conf`'s `web_admin_password` (plaintext blanked); health; SPA;
  login ok / 401; 401 without or with a forged token; a viewer created
  through the API gets 403 on admin routes; last-admin guard; own password
  change (403 on a wrong current one, a fresh token otherwise); services
  listed with `wdt_feeder` critical; restart sshd → new pid; stop/restart
  `wdt_feeder` → 409 and it keeps running; logs list, tail, and an SSE stream
  that receives a line appended on the camera; config GET hides secrets, PUT
  round-trip; ONVIF account; snapshot 503 (no sensor); PTZ on the simulated
  head: move/stop, deadman, goto, presets, home. Last: five failed logins →
  429 for the whole client (every forwarded connection comes from 10.0.2.2),
  then web is restarted to forget it.
* **onvif** (`onvif`, tcp 8000 and udp 3702 forwarded): unicast
  WS-Discovery Probe → ProbeMatch with the device XAddr; then
  [onvif-zeep-async](https://github.com/openvideolibs/python-onvif-zeep-async)
  with `nat_override` (the camera's URLs carry the guest address):
  GetSystemDateAndTime unauthenticated, GetDeviceInformation only with a
  PasswordDigest, capabilities with PTZ, the profile with its
  PTZConfiguration, stream and snapshot URIs, ContinuousMove/Stop and presets,
  unknown action → Sender fault; the web UI and ONVIF move the same motor.
* **shutdown** — `init.sh --stop` stops everything; the watchdog is disarmed.

## Debugging

The full console transcript of every run is in `target/itest/console/`. Inside
the VM, `/var/log/firmware.log` has the firmware boot and
`/var/run/ap-<name>/log` each access point.
