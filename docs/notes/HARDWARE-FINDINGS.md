# Live-camera findings — AK3918AV100 @ 192.168.20.187

> **Read [`SDK-SOURCES.md`](SDK-SOURCES.md) first.** The chip is an
> **AK3918AV100**, not the AK3918EV200 this repository was built around, and
> the matching SDK has since been located publicly. That single fact explains
> every video and audio failure below.
>
> **Update 2026-10-13.** Sections 1 to 9 are the record of what was found at
> the time and are kept as written. Since then, video, RTSP and ONVIF media on
> this AV100 no longer depend on any vendor SDK: the pure-Rust camera service
> (`crates/camerad`, backend `crates/anyka-av100`) drives the V4L2 nodes, the
> ISP and `/dev/venc` directly, and autostart is solved by the mtd6 patch kit
> in [`tools/autostart/`](../../tools/autostart/README.md). Dated notes in §1,
> §5 and §9 point at what changed.

Recorded from a session on a real camera: firmware `app_version=38.2.72`
(2025-04-22), kernel `4.4.282`, `armv5tejl`, uClibc-ng 1.0.31, 32 MB RAM,
BusyBox init, 29.7 GB SD at `/mnt/disc1`.

Everything below was verified on the device, and every claim about *whose*
fault a failure is was checked against the vendor's own prebuilt **C** demos on
the same unit.

---

## 1. This camera's firmware is a different Anyka SDK generation

This is the single fact that explains every video and audio failure here. The
SDK objects vendored in `cross-compile/` were built for a different firmware
than this unit runs. Three independent symptoms:

| Symptom | Evidence |
| --- | --- |
| ISP config rejected | every `isp_*.conf` in `/usr/local`, `/rom`, `/ipc/local` gives `config file ... is old version` then `isp_cfg_get_sensor_id: cannot get sensor id` |
| Sensor identity unavailable | `/sys/ak_info_dump/sensor_if` does not exist, and **neither `ak_info_dump.ko` nor `akcamera.ko` is present anywhere on the device** — yet `run_libre_anyka_app.sh` tries to `insmod` both |
| Audio device missing | `ak_ai_open` opens `/dev/akpcm_cdev1`; this firmware exposes ALSA-style `pcmC0D0{c,l,p}` instead |

**It is not a Rust problem.** Controls run on the same camera:

```console
# vendor prebuilt C md_demo, pointed at the same ISP configs
$ ./md_demo
[check_data:212] config file: /ipc/local/isp_sc2331... is old version
[isp_cfg_get_sensor_id:299] cannot get sensor id, check your camera driver
C_DEMO_EXIT=255

# vendor prebuilt C libre_anyka_app — the app gergesettings.txt selects
$ ./libre_anyka_app -w 640 -h 360
[ERROR ] capture <capture_init:487>: match sensor failed
open /dev/akpcm_cdev1 failed: No such file or directory
# process exits
```

So `run_libre_anyka=1` cannot produce video on this unit either. The only
software on the device that can drive this ISP is the statically linked vendor
binary `/ipc/ipc` (6.2 MB), which carries its own matching SDK — there are **no
SDK shared objects installed on the camera at all** (`/usr/lib` and `/ipc/lib`
are empty).

### Consequence for ONVIF / RTSP / PTZ

> **Update 2026-10-13: no longer blocked.** The conclusion below holds for the
> *vendored SDK*, not for the camera. Video on this unit now comes from the
> pure-Rust camera service, which needs neither the vendor SDK nor `/ipc/ipc`:
> it loads the camera's own v7 ISP tuning files, captures NV12 from both
> SC2331 sensors over V4L2 and encodes H.264 on `/dev/venc` (verified on the
> live camera 2026-10-12, after a loose sensor ribbon cable was reseated).
> `rtsp` serves it as `/vs0` and `/vs1` and `onvif` advertises one media
> profile per sensor; the end-to-end live check of those two under board
> profiles is still to be recorded here. Audio remains blocked as described.
> Wire format: [`VIDEO-PROTOCOL.md`](VIDEO-PROTOCOL.md).

Those all live in `/ipc` (`/ipc/onvif`, `/ipc/ipc`), and that stack is ruled
out here because it resets the WiFi configuration (§5). The vendored SDK cannot
substitute for it on this firmware, so **ONVIF, RTSP and PTZ are blocked on this
unit until a matching SDK drop is available** — see §5 for exactly what is
missing and the options.

---

## 2. The vendored SDK ships two incompatible `libplat_thread.so`

Found by pulling a core dump off the camera and symbolising it.

```
signal=11 pc=0xb67b709c lr=0xb6848274
PC in: /lib/libuClibc-1.0.31.so      +0x7409c  -> pthread_mutex_init+44
LR in: .../lib/libplat_ai.so         +0x2274   -> ak_ai_open+260
```

`cross-compile/` contains two builds of `libplat_thread.so`:

```
a6d3fbf5...  jpeg_snapshot, libre_anyka_app, venc_demo
f94db11c...  ai_demo, aenc_demo, md_demo, rtsp
```

They are not interchangeable **in either direction**:

* `a6d3fbf5` + the audio objects → SIGSEGV in `pthread_mutex_init`;
* `f94db11c` does not export `ak_thread_cond_*` / `ak_thread_mutexattr_*`, so
  `libplat_venc_cb.so` fails to link against it.

A naive union of every `cross-compile/*/lib` into one directory — which is what
this project originally did — silently picks one and breaks the other half.

**Fix applied:** `sdk/lib/` is split into one directory per coherent variant
(`audio`, `video`, `md`, `rtsp`), each copied from the vendor demo that ships
it. Each binary links only its own variant and carries an
`$ORIGIN/../lib/<variant>` `RPATH`, so the right objects are found at runtime
with no `LD_LIBRARY_PATH` juggling. See the `anyka-link` crate.

With the correct variant the audio path stops crashing and fails cleanly:

```
open /dev/akpcm_cdev1 failed: No such file or directory
Error: ak_ai_open returned a null handle
```

---

## 3. BusyBox on this firmware is missing common tools

`dirname`, `pidof`, `pkill`, `head` and `tail` are all absent. Scripts that
assume them fail in ways that are easy to miss — `pidof` returning "not found"
made a liveness check silently report "not running".

`run-on-camera.sh` and `init.sh` now use parameter expansion (`${0%/*}`) and a
`/proc/[0-9]*/comm` scan instead.

---

## 4. What was verified working on real hardware

| | Result |
| --- | --- |
| All 21 SDK objects load | yes |
| `wdt_feeder` on real `/dev/watchdog` | **yes** — holds it exclusively; the old `feeder.sh` then fails with `Device or resource busy`. No reboot across a 3 h 38 m session |
| `sshd` | **yes** — real login from another host: KEX, PBKDF2 auth, PTY on `/dev/pts/0`, `uname -m` → `armv5tejl` |
| `supervisor` | **yes** — started both services; backed off 500→1000→2000→4000→8000→16000 ms while a port was squatted, recovered unaided; restarted `wdt_feeder` 3 s after `kill -9` |
| `init.sh` | **yes** — start, `--stop`, restart loop, double-start guard |
| Prometheus textfile metrics | **yes** — `kamoder_supervisor_service_up{service="sshd"} 1`, `kamoder_watchdog_feeds_total` climbing |
| ISP demos (`md_demo`, `venc_demo`, `rtsp_demo`, `jpeg_snapshot`) | **blocked** by §1 — same failure as the vendor C demos |
| Audio demos (`ai_demo`, `aenc_demo`) | **blocked** by §1 — device node absent |

A third bug the hardware exposed: the supervisor passes its environment to the
processes it starts, so parent and children all enabled the same metrics
exporter and raced writing one `.prom` file (`No such file or directory` on the
rename). `ANYKA_METRICS=textfile:` now names a **directory** and each service
writes `<service>.prom`, which is also the shape `node_exporter` expects.

---

## 5. The vendor stack is not an option — it resets the WiFi configuration

> **Update 2026-10-13.** The vendor stack is still not an option, and no
> longer needed: "what would actually unblock video" below was overtaken by
> option 4, not on the list at the time — drive the kernel interfaces
> directly from Rust. See the note in §1. Option 3 (containing `/ipc/ipc`)
> was never pursued; the autostart patch in `tools/autostart/` makes
> `/ipc/setup.sh` skip `/ipc/ipc` entirely.

`/ipc/ipc` is the only software on the device that can drive this ISP, but it
**cannot be used here**: it rewrites the WiFi configuration, and this camera is
configured manually. Confirmed the hard way — running `cd /ipc && ./ipc &` over
telnet made the camera unreachable within 25 seconds, on every subnet, for
about eight minutes.

What actually happened is worth recording, because it is reassuring:

* the board **did not reboot** — uptime ran continuously through the incident
  (4 h 05 m at the end of the session);
* the Rust stack **survived unattended**: `supervisor`, `wdt_feeder` and
  `sshd` came back with the *same PIDs*, so the watchdog was still being
  fed throughout the outage;
* the WiFi re-associated on its own and the camera returned to
  `192.168.20.187`.

So the failure mode was "vendor app resets the network", not "camera bricked".
Still, **do not start `/ipc/ipc`**. An earlier draft of this document suggested
setting `run_ipc=1` and rebooting; that advice is wrong for this camera and has
been withdrawn — it would clobber the manual WiFi configuration.

### Why the vendored SDK cannot substitute for it

The ISP configuration format has moved on. Measured, not guessed:

| | Version |
| --- | --- |
| ISP configs on this camera (`/usr/local`, `/rom`, `/ipc/local`) | **7** (`u32` at offset 0; version string `6.02_122702_0`) |
| ISP configs vendored in this repo (`isp_sensor_conf/`, `references/`) | **3** and **4** |
| `libakispsdk.so` — *every* copy in the repo | **V3.1.00** (one V3.1.02) |

`check_data` in `libplat_vi.so` rejects the camera's files with
`config file ... is old version`; the surrounding disassembly compares against
`#3`, which is consistent with an accepted ceiling of 3 (the function is a
`constprop` clone, so the exact argument is not worth over-claiming).

There is also **no v3/v4 ISP configuration for this camera's sensor** (sc2331)
anywhere in the repo, and no newer `libakispsdk.so` anywhere either. Both
halves of the pair are missing, so there is nothing to combine.

### What would actually unblock video

In rough order of tractability:

1. **Obtain the matching SDK drop.** The firmware is `app_version=38.2.72`
   (2025-04-22). The Anyka SDK release that produced it ships a
   `libakispsdk.so` that reads v7 configs, plus the matching `libplat_*`
   objects. That single artefact unblocks every video demo, C and Rust alike —
   nothing in this port needs to change, only `sdk/lib/`.
2. **Produce a v3/v4 ISP config for sc2331.** Requires the older config format
   and sensor tuning data. `references/ak3918ev200/vendor/decompiled/` has a
   decompiled `libakispsdk.so.c`, which is where the format would have to come
   from. Considerable work, and the result would be untuned.
3. **Contain `/ipc/ipc` rather than avoid it** — for example remounting `/rom`
   read-only so it cannot rewrite `wpa_supplicant0.conf`, with the config
   backed up first. Plausible but unproven, and it risks another outage, so it
   should only be tried with physical access to the camera.

Note also that `ak_info_dump.ko` and `akcamera.ko` — which
`run_libre_anyka_app.sh` tries to `insmod`, and which provide
`/sys/ak_info_dump/sensor_if` — are absent from the camera. The copies in
`references/ak3918ev200/vendor/bin/` are `vermagic 3.4.35`; this kernel is
`4.4.282`, so they will not load either.

## 5b. What the vendor stack actually produced, and the RTSP findings

`/ipc/ipc` was started once by hand (before the constraint in §5 was known).
It ran for about 25 minutes. What it showed is worth keeping, because it is the
only look anyone has had at this camera's real video path.

**Its own ISP SDK is `libplat_isp_sdk V8.0.01`** — against `V3.1.00` for every
copy vendored in this repo. That single line is the cleanest statement of the
generation gap:

```
>>>>>>>>   Sensor: 0x2331
ispsdk_lib version:libplat_isp_sdk V8.0.01
open isp dev: /dev/isp-param-0
[vi_dev_open:269] open sensor dev: /dev/v4l-subdev0
[start_vi] load isp config file, isp_conf: /ipc/local/isp_sc2331_mipi_2lane_mipi0.conf
[isp.conf]version: 6.02_122702_0, sensor id: 0x2331, style id: 0
```

Note the driver interface: `/dev/isp-param-0` and **`/dev/v4l-subdev0`**, a V4L2
subdev. The vendored V3.1.00 SDK uses the older `ak_info_dump.ko` /
`/sys/ak_info_dump/sensor_if` interface, which does not exist on this kernel.
These are not two configurations of one thing; they are different driver
generations. No amount of config juggling bridges that.

### RTSP service: up; media: never confirmed

With `ipc` running, `test_camera.py` went from 50 % to 83.3 % health:

| | |
| --- | --- |
| Port 554 | OPEN |
| `OPTIONS` | `DESCRIBE, SETUP, TEARDOWN, PLAY, PAUSE, GET_PARAMETER, SET_PARAMETER` |
| Endpoints | `/onvif1`, `/live/ch0`, `/vs0`, `/vs1` |
| Also open | 5000 (UPnP/ONVIF), 50000 |

But **no video frames were ever confirmed**, for three separate reasons:

1. **The SDP mislabels the codec.** Every endpoint advertises
   `a=rtpmap:96 H265/90000`, while the accompanying
   `sprop-parameter-sets=Z0IAH5WoFAFuQA==,aM48gA==` decode to **H.264** NAL
   types 7/8 — SPS profile_idc 66 (Baseline), level 3.1, **1280x720**. A
   standard client selects an HEVC decoder, feeds it an H.264 SPS and fails
   with `Could not find codec parameters ... unspecified size`. This alone will
   break VLC, ffmpeg and any NVR pointed at the camera.
2. **Media is UDP-only and could not be routed.** `SETUP` with
   `Transport: RTP/AVP/TCP;unicast;interleaved=0-1` is answered
   `Transport: RTP/AVP;unicast;...;interleaved=0-1` — the `/TCP` profile is
   dropped, which is exactly the `Nonmatching transport in server reply`
   ffmpeg reports. `PLAY` then delivered **zero** interleaved bytes. With the
   build host on a different subnet and the camera unable to initiate traffic
   toward it, UDP RTP never arrives either.
3. **The recordings were empty.** `iv_rd_mp4_writer_init_h265` /
   `mp4 init h265 success` fired every couple of minutes, but every resulting
   `/mnt/disc1/iotVideo/record/**/*.mp4` was **0 bytes**. That evidence is
   local to the camera and independent of any network problem, so it is the
   strongest signal: the encoder was initialising but not producing data.

The likely reason for (3) is that `ipc` was launched directly rather than
through `/ipc/setup.sh`, which does the hardware bring-up — module load order,
and pokes like `devmem 0x080001a8 32 0x1aaa80aa` for clock drive strength. A
hand-started `ipc` gets a half-initialised sensor: detected over I2C, but not
delivering frames.

**Conclusion: video streams are not working on this camera, and were not made
to work.** The RTSP control plane is healthy; the media path is not.

### Stopping it also drops the network

Killing `ipc` took the camera off the network exactly as starting it did, and
the board subsequently rebooted. `/rom/wpa_supplicant0.conf` was backed up to
`/mnt/disc1/wpa_supplicant0.conf.bak` first, with `ssid="Nova-Terra-IoT"`
intact at that point.

---

## 6. Deploying to the camera

No `wget` or `curl` on the device, and the camera could not reach back to the
build host across subnets, so transfers were pushed with `nc`:

```sh
# on the camera
(nc -l -p 9000 > /tmp/p.tgz &)
# on the build host
nc -q 2 <camera-ip> 9000 < payload.tgz
# back on the camera
cd /mnt/disc1 && tar xzf /tmp/p.tgz && rm -f /tmp/p.tgz
```

`/mnt` itself is a 16 MB tmpfs — the SD card is `/mnt/disc1`. Install to
`/mnt/disc1/anyka_rust`, not `/mnt/anyka_rust`.

Strip binaries before transfer (`arm-anykav200-linux-uclibcgnueabi-strip`):
7 MB → 1.4 MB each, which matters over WiFi even though SD space does not.

## 7. The V8 SDK port — item 1 of §5's "what would unblock video"

§5 listed *"obtain the matching SDK drop"* as the most tractable route. That
drop was found and vendored, and the Rust side now builds against it.

### What was obtained

`github.com/KD0000001/ak3918av100` (`AnyCloud39AV100_SDK_V1.12`). The objects
live inside `rootfs/usr.sqsh4` and were extracted to `sdk/v8/` — 35 headers and
28 shared objects. Two version strings match the live camera exactly:
`libplat_isp_sdk.so` reports **V8.0.01** and `libmpi_venc.so` reports
**V2.0.46**. Provenance and the licensing caveat (NDA-origin mirror) are in
`sdk/v8/PROVENANCE.md`.

The prediction in §5 held: `sdk/lib/` was the only thing that needed replacing
in principle — but the API is a different *generation*, not a different
version, so the safe wrappers had to be rewritten rather than recompiled.

### How the two generations differ

| | EV200 (`anyka`) | AV100 V8 (`anyka-v8`) |
|---|---|---|
| Handle | opaque `void *` | numeric `VI_DEV` / `VI_CHN` ints |
| Start | `capture_on()` | `enable_chn()` **then** `enable_dev()` |
| Frames | main + sub per fetch | one frame per fetch |
| Errors | `ak_get_error_no()` | packed `(module << 24) \| code` return |
| Motion | `stat`, `stat_max`, `level` | `stat` only; peak recomputed |
| ISP config | v3/v4 | v7 |

Because the call order is load-bearing — getting it wrong fails deep inside the
ISP with a packed status and no message — it is encoded in the types: there is
no way to obtain a `VideoChannel` without having configured the device, and no
way to fetch a frame without an enabled channel.

### One structural decision worth recording

The SDK generation was first selected with a cargo **feature** on `anyka-sys`.
That is wrong and cannot be made right: cargo unifies features across a
workspace, so `md_demo_v8` enabling `anyka-sys/v8` swapped the bindings under
the EV200 `anyka` crate as well — 140 compile errors the moment `./x dist`
built both in one pass. Both generations are now generated on every build into
separate files and exposed as `anyka_sys::*` and `anyka_sys::v8::*`.

The `clashing_extern_declarations` lint fires on this, since `ak_vi_open` is
declared twice with different signatures. It is allowed in the `v8` module with
the reason inline: the lint assumes one symbol means one ABI, which holds per
*binary* but not per *crate*. No binary links both `libakispsdk.so` and
`libplat_vi.so`, and `anyka-link` is what enforces that.

### Status

Built and staged, **not yet run on hardware**. `md_demo_v8` cross-compiles;
`readelf -d` shows `RPATH [$ORIGIN/../lib/v8]` and only V8 objects in `NEEDED`;
`./x dist` stages `lib/v8/` with all 28 objects. Host tests and clippy are
clean.

The live run is blocked on the camera itself, not on the code — see §8.

## 8. The camera is currently unreachable (2026-09-21)

`192.168.20.187` is in a state where it cannot be worked with:

* ICMP oscillates between 75–100% loss and full unreachability for minutes at a
  time. When a reply does arrive the RTT is ~3 ms, so this is packet loss or a
  link that keeps dropping, not congestion.
* Port 23 alternates open/closed on roughly a 10–60 second cycle. When the
  connection *is* accepted, telnetd completes the TCP handshake and then sends
  nothing at all — no option negotiation, no banner, no prompt — and a command
  written to the socket produces no output before the connection dies.
* Ports 22, 554, 2222, 8080 and 9100 are all closed, so neither `sshd`
  nor anything else from the Rust stack is listening.

A kernel that accepts connections while userspace never answers points at the
camera being resource-starved or wedged, not at a network misconfiguration.
No UART adapter is attached to the build host (`/dev/ttyUSB*` is empty), so
there is no out-of-band path in.

**This needs a power cycle**, and it is worth doing with the SD card readable
on the host afterwards — the payload for the V8 test is only 879 KB
(`md_demo_v8` stripped to 1.4 MB plus the ten V8 objects it needs), so copying
it onto the card directly is faster and far more reliable than pushing it over
this link.

### Tooling added for this link

* `tools/cam.py` — telnet shell that retries the connection, answers IAC option
  negotiation, and brackets commands with sentinels instead of matching the
  prompt (which changes with the working directory and gets mangled by partial
  reads).
* `tools/push.py` — resumable chunked file push. The camera cannot open
  connections towards the build host, so each 128 KB chunk is a `nc -l` the
  camera is told to start and the host then connects to; chunks are size-checked
  individually and a rerun skips the ones already on the card.

Neither has been exercised against a healthy camera yet.

## 9. Web admin, ONVIF and boot on the live camera (2026-09-27)

Everything below was measured on the AK3918AV100 at `192.168.20.187`.

### The stock firmware never runs anything from the SD card

The boot chain is `inittab` → `rcS` → `rc.local` → `auto_upgrade.sh` (or
`button_upgrade.sh` with the reset key held) → `/ipc/setup.sh` → `/ipc/ipc &`.
The three places that mount the card each look only for a fixed list of vendor
files (upgrade images, `custom_setting.ini`, `factory_pcba_test.txt`,
`custom_apn.ini`, sound packs) and then unmount it. `mdev.conf` has no
hotplug action for `mmc*`. Nothing references `autorun.sh`, `Factory/` or
`config.sh`.

Checked three ways:

* **Runtime marker.** `Factory/config.sh` now appends to
  `/tmp/anyka_config_sh.log` as its very first action. After a reboot the file
  did not exist.
* **Live firmware.** `grep` over `/etc/init.d`, `/ipc/*.sh` and `/usr/sbin` on
  the camera; `/ipc/ipc` strings contain `/mnt/disc1/*` data files but no
  script path.
* **Every local dump**, extracted with binwalk. There is only one AV100
  firmware: the May backups are identical, and `spi_20260907/read1.bin` is the
  same image read incompletely (0xFF after 0x4AC000). The dumps match the live
  camera (`mtd6` md5 `f4c739e1…`, `/ipc/ipc` md5 `473cd02b…`).

The `/mnt/Factory/config.sh` hook that the `anyka_happy` approach relies on is
in the **AK3918EV200** firmware (`firmware_dump/anyka_3918EN080.bin`, Linux
3.4.35, `/usr/sbin/service.sh`). The four `UART_logs/*.log` and
`references/ak3918ev200/` also come from that camera, not this one.

**Consequence:** on this camera the Rust stack only runs when started by hand
(`sh /mnt/disc1/Factory/config.sh`). Earlier `boot.log` lines were manual runs.
Autostart needs a persistent change on the camera; none has been made. A
verified backup of the APP partition is in `~/backups/anyka-mtd6/` (two reads,
md5 `f4c739e1d60b3dc0bbaf6d9d5a15beb3`, equal to the camera's own
`md5sum /dev/mtd6ro` and to the May dump).

> **Update 2026-10-13.** That persistent change now exists:
> [`tools/autostart/`](../../tools/autostart/README.md) splices a new tail into
> `/ipc/setup.sh` in the mtd6 squashfs, keeping the hardware bring-up and then
> mounting the card and running `Factory/config.sh` instead of `/ipc/ipc`
> (which it never starts, even without a card). Writes are limited to the 4 KiB
> erase blocks that differ, each read back, with `rollback.sh` restoring the
> original partition. The live unit was patched on 2026-10-11 and now boots
> into the Rust stack. The vendor guard below stays, as a safety net for
> unpatched cameras.

### The vendor app holds the watchdog, and handing it over works

At boot the vendor app — `main` / `anyka_ipc` on this firmware, the names the
old `quite_console.sh` killed — owns `/dev/watchdog`. `wdt_feeder` then failed
with `EBUSY` on every start (12 restarts logged) until the vendor app died.

Fixes, both verified on hardware:

* `wdt_feeder -w` waits while the device is busy instead of exiting (retries
  every 250 ms), and stays running.
* `init.sh` runs a vendor guard: every 3 s it kills `main`, `anyka_ipc`, `ipc`,
  `daemon`, `gwellupdater.sh`, `soft_reboot.sh` — but only while `wdt_feeder`
  is running, so the guard can never be what leaves the watchdog unfed.

Manual start at uptime 7232 s: `init: stopped vendor process main` at 7232.5 s,
`watchdog feeder started` at 7234.7 s, uptime continuous — no reboot. The
earlier one-shot kill found nothing because the vendor app starts after the
point where `init.sh` would have run.

### Services and memory

With `ipc` stopped: `supervisor`, `wdt_feeder`, `wifi`,
`sshd` (2222), `web` (80), `onvif` (8000, discovery udp 3702).

| Process | VmRSS |
| --- | --- |
| web | 2980 kB |
| onvif | 2088 kB |
| sshd | 3268 kB |
| wifi | 1616 kB |
| wdt_feeder | 1692 kB |

`web` + `onvif` = 5.07 MB, inside the 6 MB budget; MemAvailable
14 MB of 32 MB.

First boot provisioning worked: `etc/web-users`, `etc/web-jwt.key` and
`etc/onvif.secret` were created and the plaintext blanked from `camera.conf`.
From the LAN: login in 0.23 s, `/api/services`, `/api/wifi` (healthy, −36 dBm),
`/api/ptz`, SPA served; `/api/snapshot.jpg` → 503 (no video pipeline, expected
at the time; served by the camera service since 2026-10-12, see §1);
`/api/system/reboot` rebooted the board.

The camera has no RTC: the clock reads 1970, so JWT `exp` is small
(e.g. 43702). The server only compares against its own clock, which is fine;
**the web UI must not compare `exp` with the browser clock** (open item).

### PTZ

`onvif --ptz auto` picked the `preset_motor` backend
(`/dev/preset_motor`, loaded by the vendor app before it was killed) and took
position (85, 19) from the driver — the vendor's saved position. Limits are raw
steps, pan 0–175 / tilt 0–70, until measured. No motion has been commanded yet;
see the checklist in `.specs/features/web-admin-onvif/tasks.md` T18.

### sshd: exec ran on a PTY and blocked the runtime

Uploading with `ssh … 'cat > file' < file` echoed the input back and never
ended; an 18 MB upload wedged the whole server (banner timeouts for every new
connection). Cause: exec always ran on a PTY (echo, no EOF), and stdin was
written with a blocking `write` on the single runtime thread. Fixed: pipes
unless `-t`, async I/O, bounded input with backpressure, EOF closes stdin.
On the camera a 2 MB upload now completes in 1.2 s with a matching md5.

`/tmp/campush.sh` (base64 chunks in the command line) was used to deploy while
the old sshd was still running; it is no longer needed.

## 10. Board-agnostic build on the live AV100 (2026-10-13)

Deployed the board-profile build (stripped binaries swapped in with
`tools/camera-access/swap.sh`; `wdt_feeder` and `sshd` left as they were)
and checked it from the LAN:

| Check | Result |
| --- | --- |
| Board resolution | `anyka-av100-ptz`, "matched cpuinfo_hardware=AK3918AV100 (score 1)"; `/tmp/kamoder-board.json` written, all services started |
| Supervisor args | camera, rtsp and onvif run with no AV100 flags (no `--conf-dir`, `--stream`, `--rtsp-path`) |
| Camera service | both sensors capturing, hardware H.264 on (`capturing sensors=2 encoding=true`) |
| RTSP | `ffprobe` `/vs0` and `/vs1`: H.264 Main 1280x720 15 fps; a decoded frame from each shows a live picture |
| ONVIF | device info "Kamoder / AK3918AV100 dual SC2331 pan/tilt"; `GetProfiles` returns `Profile_vs0` and `Profile_vs1` (H.264 1280x720) with per-sensor stream and snapshot URIs; 2 video sources; PTZ `GetStatus` answers; imaging advertised |
| Wi-Fi | profile values in use (`wlan0`, `ssv6x5x`, power GPIO 98), link healthy |
| Lights | socket up with the profile's five roles |

WS-Discovery multicast did not reach the test host, which sits on another
subnet; the unicast probe answered.

## 11. Audio codec (ak_pcm.ko) on the live AV100 (2026-10-17)

The codec nodes look like ALSA but are a vendor char device: `/dev/pcmC0D0p`
(playback, minor 0), `/dev/pcmC0D0c` (capture, 1), `/dev/pcmC0D0l`
(loopback, 2), major 248 `pcmchar`, class `/sys/class/akpcm`. No `/dev/snd`,
no ALSA tools. Ioctl numbers come from disassembling the unstripped module;
the Rust ABI is `crates/anyka-av100/src/pcm.rs`, the probe
`tools/re/pcm-probe`.

Verified with `pcm-probe` (vendor `ipc` not running):

- **GET_PARS (`0x801c50f2`) works** and confirms the 7×u32 layout
  `{?, rate, channels, bits, period_bytes, periods, ?}`. Driver defaults:
  capture `{0, 8000, 1, 16, 16384, 16, 0}`, source 2, gain 3; playback
  `{0, 8000, 2, 16, 16384, 32, 0}`, source 0, gain 4. Loopback answers
  ENOTTY to GET_PARS/source/gain.
- **Capture works natively at 8 kHz mono S16**: SET_PARS
  `{0, 8000, 1, 16, 512, 8, 0}` + START → every `read()` returns one
  512 B period in ~32 ms, 16384 B/s, no zeros, real room signal (AC RMS
  ≈ 1700, no DC). 16 kHz / 2048 B also works (64 ms reads). The board
  profile uses 8 kHz, so the service never resamples.
- `GET_RATE` (`0x800450ea`) returned 0 before SET_PARS and the set rate
  after.
- **SET_SPEAKER (`0x40045081`) drives GPIO 37**: arg 1 → sysfs value 1,
  arg 0 → 0, while `lights` keeps the line exported (the driver does not
  `gpio_request` it exclusively).
- **Playback works; the amp is active low and the driver gets it wrong.**
  The driver reads `speak-gpios` without its polarity flag and treats "on"
  as raw high: `SET_SPEAKER 1` and `SET_SOURCE 4` (route DAC → output,
  register `0x0800009c` bits 16:14 = 0b100) drive GPIO 37 **high = amp
  muted**; `SET_PARS`, `SET_SPEAKER 0` and closing the node drive it low but
  also clear the output mux. So the driver alone can never be "routed + amp
  on". Working sequence (tone heard only while the line was low):
  SET_PARS `{0, 8000, 1, 16, 512, 8, 0}` → START → SET_SOURCE 4 →
  SET_GAIN → *then* GPIO 37 = 0 via sysfs → write; after closing the node,
  set GPIO 37 = 1 again (the close left it low). The vendor app does the
  same: it drives the amp itself through sysfs (`device.config`
  `ioremap_aoamp=10037`, inverted) and never calls SET_SPEAKER. The
  `audio` service therefore defaults to `--amp lights`.
- Gain table (register `0x9c` bits 22:18): index 0..5 → 0x10, 8, 4, 2, 1, 0;
  SDK says 0 = mute, 6 = max. Source is a bitmask: 4 = DAC → output,
  2 = line-in bypass, 1 = mic bypass, 0 = mute.
