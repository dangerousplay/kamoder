# Integrations — Kamoder

## Kernel interfaces (camera, Linux 4.4.282)

| Interface | Used by | Evidence |
|---|---|---|
| `/dev/watchdog` (chardev ioctls, timeout 8 s, feed 1 s) | `demos/wdt_feeder` via `os::CharDevWatchdog` | `crates/os/src/watchdog.rs`, `demos/wdt_feeder/src/main.rs` |
| V4L2: `/dev/video-N-M` (3 nodes per sensor, MMAP NV12), `/dev/v4l-subdevN`, `/dev/isp-param-N`, sysfs `/sys/class/video4linux`; Anyka private ioctls (`SET_DUAL_MODE`, ISP tuning load, 3D-NR map) | `crates/camera` | `crates/camera/src/device.rs`, `v4l2.rs`, `main.rs` |
| `/dev/venc` (`ak_venc_adapter.ko`): raw ioctls (`VENC_FIND_CHANNEL 0x405c56ce`, `CHN_CREATE 0x405c56e0`, DMA pool create/free…), mmap'd bitstream pool, one opener system-wide | `crates/camera/src/venc.rs` (reverse-engineered from `libmpi_venc` V2.0.46) |
| ISP tuning files `/usr/local/isp_<sensor>_mipi_2lane_mipi<N>.conf` | camera (`--conf-dir`) ; SDK path search `/etc/jffs2 /usr/local /rom /ipc/local` | `crates/camera/src/conf.rs`, `crates/anyka/src/vi.rs`, `crates/anyka-v8/src/vi.rs` |
| `/proc/sys/vm/{min_free_kbytes,drop_caches,compact_memory}` writes before encoder allocation | camera | `crates/camera/src/main.rs:291`, `service.rs:361` |
| GPIO sysfs `/sys/class/gpio` (no GPIO chardev on 4.4); allowlist 19, 37, 48, 67, 68 | lights | `crates/lights/src/gpio.rs` |
| GPIO 98 (USB Wi-Fi power), `/rom/usb_module_power_on`, `rmmod/insmod /ipc/modules/wifi/ssv6x5x.ko`, `/proc/modules`, `/sys/bus/usb/devices` | wifi recovery | `crates/wifi/src/recovery.rs`, `system.rs`, `main.rs` |
| `/dev/preset_motor` (`presetmotor.ko` ioctls, AV100) ; `/dev/ak-motor0/1` via `libplat_drv` (EV200) ; `/rom/device.config` | anyka-ptz, owned by onvif | `crates/anyka-ptz/src/lib.rs`, `preset_motor/`, `docs/notes/PRESET-MOTOR-ABI.md` |
| wpa_supplicant control socket (ctrl dir `/etc/Wireless`), config `/rom/wpa_supplicant0.conf` (JFFS2, remounted rw for writes), `udhcpc` with `/usr/share/udhcpc/default.script` | wifi, web (status) | `crates/wifi/src/wpa.rs`, `system.rs` |
| Netlink/ioctl: `SIOCGIFADDR`, `/proc/net/{route,arp,wireless,if_inet6}`, `/sys/class/net/*` | wifi, web, onvif, supervisor ready-probe | `crates/wifi/src/net.rs`, `crates/supervisor/src/ready.rs`, `crates/onvif/src/net.rs` |
| `/dev/ptmx`, `/dev/pts/N`, `/etc/passwd` | sshd | `crates/sshd/src/pty.rs` |
| `/dev/console`, `/proc/uptime` (ms-since-boot timestamps; no RTC) | supervisor | `crates/supervisor/src/main.rs`, `console.rs`, `logs.rs` |
| MTD: `/dev/mtd6` APP squashfs patched by `tools/autostart/mtdpatch` (4 KiB erase-block diff writes with readback/restore) | autostart kit (host + camera) | `tools/autostart/README.md` |
| Signals: SIGCHLD via tokio process driver, SIGTERM/SIGINT shutdown | supervisor, all services | `crates/os/src/shutdown.rs` |

Firmware processes killed by `init.sh` vendor guard: `daemon gwellupdater.sh soft_reboot.sh ipc anyka_ipc main` (`tools/init.sh` `VENDOR_PROCESSES`).

## Vendor SDK (`sdk/`)

- EV200 SDK V3.1.00: headers `sdk/include/ak_*.h`, objects split into variants `sdk/lib/{audio,video,md,rtsp}` because two incompatible `libplat_thread.so` builds exist (HARDWARE-FINDINGS §2). Used by `crates/anyka` + demos + `anyka-ptz` `sdk` feature.
- AV100 V8 SDK (`AnyCloud39AV100_SDK_V1.12`, `libplat_isp_sdk` V8.0.01, `libmpi_venc` V2.0.46): `sdk/v8/{include,lib}`, NDA-origin mirror, see `sdk/v8/PROVENANCE.md`. Used by `crates/anyka-v8`, `demos/md_demo_v8`.
- Bindings: `crates/anyka-sys/build.rs` runs bindgen for both generations every build (`bindings.rs`, `bindings_v8.rs`); needs crosstool sysroot headers and libclang. No link flags from anyka-sys; `links = "anyka_sdk"`.
- Linking: `crates/anyka-link` generates an ld script `-lanyka_sdk_selected` (`--start-group` incl. `-lm -lpthread -ldl`) per binary and `-rpath $ORIGIN/../lib/<variant>`; mixing variants is rejected.
- Runtime: `sdk/runtime/` (libgcc_s, libstdc++, `firmware-libc.symbols`) staged into `lib/sys`; libc stubs generated at `./x dist`.
- Newer services (camera, rtsp, lights, wifi, web, onvif default) use no vendor library.

## ONVIF WSDL/XSD codegen

- Schemas vendored in `crates/onvif-schema/schema/` (`ver10`, `ver20`, `docs.oasis-open.org`, `www.w3.org`; sources/checksums in `schema/README.md`).
- Generator: `./x gen-onvif` (`xtask/src/gen_onvif.rs`, xsd-parser =1.5.2); only operations in its `SERVICES` list are emitted, into committed `crates/onvif-schema/src/generated/{tt,tds,trt,tptz,timg,wsnt,xs.rs}` (~116k lines). `--check` in `./x ci`.
- Transport: `onvif-device-rs` 0.8.0, patched in `third_party/onvif-device-rs` (namespace declarations on Body child; `PATCHES.md`, `UPSTREAM-PR.md`).
- Clients validated in itest with `onvif-zeep-async` (`itest/tests/test_08_onvif.py`).

## Network protocols exposed

| Port | Protocol | Service |
|---|---|---|
| tcp 80 | HTTP JSON API `/api/*` + SPA + `/health`; SSE log stream | web |
| tcp 8000, udp 3702 | ONVIF SOAP 1.2 (WS-UsernameToken digest/text), WS-Discovery | onvif |
| tcp 554 | RTSP 1.0, RTP over TCP-interleaved/UDP, Digest | rtsp |
| tcp 2222 | SSH | sshd |
| tcp 23 | BusyBox telnetd (unauthenticated root, `start_telnetd=1`) | config.sh |
| 127.0.0.1:3000 | HTTP `/snapshot.jpg`, `/status` | camera |
| optional | Prometheus `/metrics` (`ANYKA_METRICS=http:`) | telemetry |

## GitHub CI (`.github/workflows/`)

- `release.yml`: triggers on tag `v*`, branches `release/**`, manual. ubuntu-24.04: apt clang/libclang + i386 libs, cached crosstool (`vars.CROSSTOOL_SHA256`), `nightly-2026-09-20` with rust-src/rustfmt/clippy, Node 22, `./x webui`, `./x dist`, `tools/package-release.sh`, git-cliff changelog/notes, cosign keyless `sign-blob --bundle` for `*.tar.gz`, `mtdpatch-armv5`, `SHA256SUMS`, `actions/attest-build-provenance`, upload artifact, `gh release create/upload`.
- `docs.yml`: on `docs/**` changes to main / PRs: `npm ci && npm run build` in `docs/`, deploy `docs/out` to GitHub Pages.
- No workflow runs tests, clippy, fmt or deny.

## Release artifacts (`tools/package-release.sh`)

`kamoder-VERSION-sdcard.tar.gz` (card root, stripped), `kamoder-VERSION-debug.tar.gz` (unstripped), `mtdpatch-armv5`, `SHA256SUMS`, each with `.sigstore.json` bundle; verify instructions appended to release notes. Never ships `camera.conf`, `etc/`, logs or keys.

## External NVR/clients (documented targets)

Home Assistant, Frigate, Agent DVR (ONVIF + RTSP); `node_exporter` textfile collector for metrics.
