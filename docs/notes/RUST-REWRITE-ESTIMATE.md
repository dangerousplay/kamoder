# Replacing the bundled vendor libraries with Rust: effort estimate

Short version: **almost all of the bundled vendor code can be deleted, not
rewritten.** Of the 50 distinct objects in `anyka_rust/lib/{audio,md,rtsp,video,v8}/`,
22 are the EV200 generation, and they open device nodes that do not exist on the
AV100. Another 18 are V8 objects that no binary loads. That leaves **10 V8 objects
(the `md_demo_v8` closure)** as actual rewrite work. On this chip the ISP 3A loops
and the video encoder run in kernel modules, so those 10 are mostly ioctl
plumbing. None of them is a codec.

| Scope | Engineer-days | Confidence |
|---|---|---|
| Drop EV200 and the unloaded V8 objects (no rewrite) | 1–3.5 | high |
| Rust video input on V8 (the 10 loaded V8 objects) | 24.5–59 | low–med |
| Rust RTSP + JPEG over V8 `/dev/venc` | 10–20 | med |
| Raw PCM audio + G.711 on V8 | 4–8 | low–med |
| **Total for a vendor-library-free video + RTSP + audio stack** | **~40–91** | low–med |

Two things are excluded from the total because a rewrite is impractical:
AEC/noise suppression (`libplat_vqe`), and AAC/AMR (`libmpi_aenc`/`adec`). Both sit
on vendor objects that are not even in the bundle. NPU/SVP is excluded for the
same reason. `libc.so.0`, `ld-uClibc.so.0` and `libgcc_s.so.1` stay in every case
(see [§6](#6-what-removes-which-bundled-library)).

Every number below was measured on the files in `SD_card_contents/anyka_rust/`
on 2026-10-09. Commands are in [§7](#7-how-the-numbers-were-taken). Nothing
was run on the camera.

---

## 1. Who loads what

`readelf -d` on every binary in `anyka_rust/bin/`:

| Binary | SDK objects in `NEEDED` | `RPATH` |
|---|---|---|
| `supervisor`, `web`, `wifi`, `wdt_feeder` | none (libc, libgcc_s, stubs) | — |
| `sshd` | none (also `libdl.so.0`) | — |
| `onvif` | `libplat_drv`, `libplat_common`, `libplat_thread` | `lib/rtsp` (EV200) |
| `ai_demo`, `aenc_demo` | 5 / 7 | `lib/audio` (EV200) |
| `md_demo` | 7 | `lib/md` (EV200) |
| `venc_demo`, `jpeg_snapshot` | 11 | `lib/video` (EV200) |
| `rtsp_demo` | 18 | `lib/rtsp` (EV200) |
| `md_demo_v8` | 10 | `lib/v8` (V8) |

So **seven binaries carry EV200 objects and one carries V8 objects**. `onvif`
binds only 9 symbols across its three objects: `ak_drv_ptz_{open,close,
check_self,get_status,get_version,set_angle_rate,set_degree,turn}` and
`ak_get_error_no`. They all come from `anyka-ptz`'s `sdk` backend
(`crates/anyka-ptz/src/sdk.rs`), which is a default feature. `libplat_thread` is
`NEEDED` but has no symbol bound to it. On the AV100, `onvif --ptz auto` picks
the `preset_motor` backend, which uses plain ioctls and no SDK
(HARDWARE-FINDINGS §9).

`rtsp_demo` and `jpeg_snapshot` are `enabled = false` in `supervisor.toml`.

### Objects that are loaded but bind nothing

Resolving every binary's undefined symbols plus every co-loaded object's
undefined symbols against each object's exports shows six objects with **zero
bound symbols**. They are loaded only because `anyka-link` puts the vendor
`build.sh` list inside `--no-as-needed`:

| Object | Gen | Loaded by | `.init_array` |
|---|---|---|---|
| `libakv_encode.so` | EV200 | `venc_demo`, `jpeg_snapshot` | `frame_dummy` only |
| `libakmedia.so` | EV200 | `rtsp_demo` | `frame_dummy` only |
| `libmpi_aed.so` | EV200 | `rtsp_demo` | `frame_dummy` only |
| `libplat_venc_cb.so` | EV200 | `venc_demo`, `jpeg_snapshot` | `frame_dummy` only |
| `libplat_ipcsrv.so` | EV200 | 6 EV200 demos | 1 entry, stripped (not resolved) |
| `libplat_timer.so` | V8 | `md_demo_v8` | 1 entry, stripped (not resolved) |

`libakv_encode.so` is the **only C++ object** in the bundle. It has 44 `_Z`
exports and `NEEDED libstdc++.so.6`, and it is why `libstdc++.so.6` is shipped.
The EV200 encoder that `libmpi_venc` actually calls is `libakstreamenc.so`
(`VideoStream_Enc_*`, `MJPEG_Enc_qlevel`).

### V8 objects nobody loads

`./x dist` stages all 28 V8 objects, but only `md_demo_v8` uses `lib/v8/`, and
its transitive closure is 10 objects. The other **18 are bundled and never
loaded**: `libapp_{ats,its,mem_ex,osd_ex,video}`,
`libmpi_{adec,aenc,demux,mux,osd,svp,svp2,venc}` and
`libplat_{ai,ao,npu,uuid,vqe}`.

Several of them could not resolve as shipped anyway. They import symbols that no
object in `sdk/v8/lib/` or the firmware's `/lib` exports:

| Object | Unresolved vendor symbols (examples) |
|---|---|
| `libmpi_aenc` / `libmpi_adec` | `_SD_Encode*`, `_SD_AAC_Encode_login`, `_SD_AMR_*`, `_SD_Decode*` |
| `libplat_vqe` | `_SD_AEC_login`, `_SD_EQ_login`, `_SD_ASLC_login`, `_SD_Echo_*`, `_SD_CryDetect_login` |
| `libmpi_mux` / `libmpi_demux` | `MediaLib_Mux_*`, `MediaLib_Dmx_*`, `MP4_FilePerfect` |
| `libmpi_svp` / `libmpi_svp2` / `libplat_npu` | `AKCNN_*`, `DT_*`, `NNA_*`, `NNE_*` |

`SDK-SOURCES.md` counts 45 objects in the SDK's `usr.sqsh4`, of which 28 were
vendored. The other 17 may provide these symbols. That was not checked.

## 2. Why the EV200 objects are dropped, not rewritten

They open device nodes the AV100 does not have. The strings in each object,
checked against `firmware_dump/backup_20260517_192614/system_info/ls_dev.txt`:

| EV200 object | Opens | On the AV100 |
|---|---|---|
| `libplat_ai` | `/dev/akpcm_cdev1` | absent (has `pcmC0D0{c,l,p}`) |
| `libakispsdk` | `/dev/isp_char` | absent (has `isp-param-{0,1}`, `isp-stats-*`) |
| `libplat_vi` | `/dev/video0` | absent (has `video-{0,1}-{0,1,2}`, `v4l-subdev{0,1}`) |
| `libplat_drv` | `/dev/ak-motor0`, `/dev/ak-motor1` | absent (has `preset_motor`) |
| `libakuio` | `/dev/uio0`, `/dev/ion` | both present, but `uio0` is the CNN block on the AV100 (`drivers/status.md`) |

This matches HARDWARE-FINDINGS §1: the vendor's own C demos fail the same way on
this unit. A Rust rewrite of an EV200 object would reproduce an ABI the AV100
kernel does not implement. The per-object EV200 figures are in §4 for
completeness. Their "rewrite" column only matters if EV200 hardware were ever a
target.

## 3. On the AV100, 3A and encoding are in the kernel

The task framing put the ISP 3A in `libakispsdk`/`isp_sdk` and the video encoder
in `libakv_encode`. **On the AV100 neither is in userspace:**

* `ak_isp.ko` has 209 404 B of `.text` and 573 functions. Its symbols include
  `ak_isp_ae_work`, `ak_isp_awb_work`, `_calc_awb_gain` and
  `_get_merge_ae_stat` (`AK3918AV100_EVB_CBDM_KM01N/drivers/ak_isp/facts.md`).
  The device exposes `/dev/isp-stats-{ae,awb,af,3dnr}-N`.
* V8 `libplat_isp_sdk.so` has only **3 016 B of `.text` for 105 exports**.
  About 70 `_IOC(…,'I',…)` constants sit in its literal pools, and it opens
  `/dev/isp-param-N`. That is about one ioctl per exported function.
* The encoder is `ak_venc_adapter.ko` (398 316 B `.text`, 1032 functions).
  V8 `libmpi_venc.so` has 29 220 B of `.text`, 32 relocations against `ioctl`,
  and opens `/dev/venc` and `/dev/venc-chn%d`. `venc-chn0`–`7` exist on the
  device. It imports no codec library.

What remains in userspace is plumbing plus some policy. V8 `libplat_vpss`
(100 784 B `.text`) holds the v7 ISP config loader (`isp_cfg_file_load`,
`isp_match_sensor_cfgfile`) and day/night, anti-flicker and fps policy
(`night_mode_check`, `isp_calc_anti_flicker_target_lumi`, `isp_fps_main`).
That logic is not codec-grade, but it is undocumented.

## 4. Per-library tables

Columns:

* **.text**: bytes, from `size -A`.
* **Used/total**: symbols bound from this object by our Rust code
  (`R`) + by other co-loaded SDK objects (`S`), out of exported functions.
* **ioctl**: direct call sites (EV200, PLT) or `R_ARM_ABS32` relocations
  against `ioctl` (V8, non-PIC text), plus calls through a local wrapper where
  one exists.
* **Hdr**: exported functions declared in `sdk/include` (EV200) or
  `sdk/v8/include` (V8).

All V8 objects are stripped. Only `libplat_ipcsrv`, `libakaudiofilter` and
`libapp_net` are stripped in EV200. Only `libakv_encode` is C++.

Classes: **(a)** thin ioctl/chardev wrapper, **(b)** proprietary
algorithm/codec, **(c)** generic (runtime, threads, logging, network, RTSP).

### 4.1 V8, loaded (`md_demo_v8` closure): the real rewrite

| Library (md5) | .text | Used R+S / total | ioctl / devices | Hdr | Class | Replaced by | Days | Conf | Blockers |
|---|---|---|---|---|---|---|---|---|---|
| `libplat_timer` (27a876) | 560 | 0+0 / 10 | — | 0/10 | c | nothing (link fix) | 0–0.5 | high | — |
| `libplat_log` (72b39e) | 2 728 | 0+4 / 9 | — | 4/9 | c | `tracing` | 0–0.5 | high | goes with its consumers |
| `libplat_dbg` (b78a51) | 16 956 | 0+2 / 59 | — | 5/59 | c | nothing | 0–0.5 | high | goes with its consumers |
| `libplat_thread` (212fce) | 1 696 | 0+25 / 41 | — | 39/41 | c | `std::thread`/`std::sync` | 0–0.5 | high | — |
| `libplat_common` (ceb604) | 9 672 | 0+7 / 47 | — | 16/47 | c | Rust error/time code | 0.5–1 | high | packed `(module<<24)\|code` errors already modelled in `anyka-v8` |
| `libplat_mem` (8081a7) | 15 944 | 0+12 / 64 | via osal | 31/64 | a | Rust DMA allocator over ION | 1–3 | med | depends on osal row |
| `libplat_osal` (4d13dc) | 19 036 | 0+34 / 85 | 31 refs; `/dev/ion`, `/dev/uio%d`, `/dev/mem`, `/dev/efuse`, `/dev/pcmC*` | 0/85 | a | `os` crate: ION alloc + mmap + cache sync | 3–6 | med | ION is the stock 4.4 staging framework (`chip/ion.md` §1), so its UAPI is public. How `osal_cb_flush_dcache`/`osal_dma_mem_sync_*` reach the kernel is not yet known |
| `libplat_isp_sdk` (e2d61a) | 3 016 | 0+94 / 105 | 1 ref via `isp_sdk_ioctl`; ~70 `'I'` codes; `/dev/isp-param-N` | **0/105** | a | Rust ioctl table | 4–10 | low–med | ~94 payload layouts with no header. 73 of the 105 names also exist in EV200 `libakispsdk`, which has a full decompile in `references/ak3918ev200/vendor/decompiled/libakispsdk.so.c`, but it is a different generation and layouts must be re-verified. Kernel side (`ak_isp.ko`) not lifted |
| `libplat_vi` (7bdda5) | 47 548 | 14+5 / 76 | 5 refs + `isp_sensor_ioctl`; ~12 `'V'` (V4L2) + vendor `'S'` codes; `/dev/video-N-M`, `/dev/v4l-subdev*` | 31/76 | a | Rust V4L2 + subdev | 6–12 | med | V4L2 UAPI is public; vendor subdev controls and ION-backed buffer exchange are not. Needs the camera |
| `libplat_vpss` (7902a6) | 100 784 | 1+12 / 166 | via isp_sdk | 104/166 | a + policy | Rust v7 config loader + MD stats; policy later | 10–25 | low | v7 `isp_*.conf` format undocumented. Whether sections can be passed to the kernel as opaque blobs is an untested hypothesis. Day/night/anti-flicker policy is algorithmic. Needs the camera |
| **Subtotal** | | | | | | | **24.5–59** | | |

`R` for `libplat_vi` is exactly the 14 calls `anyka-v8` makes
(`ak_vi_{open,close,enable_dev,disable_dev,enable_chn,disable_chn,set_dev_attr,
set_chn_attr,get_frame,release_frame,get_sensor_id,get_sensor_resolution,
load_sensor_cfg,get_version}`). `R` for `libplat_vpss` is `ak_vpss_md_get_stat`.
`vi` and `vpss` call into each other: 13 symbols one way, 5 the other.
`load_sensor_cfg` goes through `vpss` → `isp_sdk`. **Replacing `vi` alone is
therefore not possible.** The four video rows are one unit of work.

### 4.2 V8, bundled but not loaded

Removing all 18 from `./x dist` costs 0–0.5 days. The "Days" column is the cost
of a Rust replacement **if the feature is wanted**.

| Library (md5) | .text | Fn exports (hdr) | Class | Replaced by | Days | Conf | Notes |
|---|---|---|---|---|---|---|---|
| `libmpi_venc` (c6ce68) | 29 220 | 46 (38) | a | Rust `/dev/venc` client | 5–10 | med | 32 ioctl refs. H.264/HEVC/MJPEG per `ak_common_video.h`. Payload layouts not in headers. Needs the camera. **Prerequisite for RTSP and snapshots** |
| `libplat_ai` (82db94) | 29 504 | 58 (56) | a (+ b via vqe) | Rust PCM capture on `/dev/pcmC0D0c` | 3–6 | low–med | binds 35 symbols from `libplat_vqe` and 12 from `osal`. Raw capture only. `ak_pcm.ko` ABI: tuya EV330 source is the reference (not vendored) |
| `libplat_ao` (5d57bf) | 16 788 | 34 (32) | a (+ b via vqe) | same, playback | 2–4 | low | optional |
| `libplat_vqe` (0f44e7) | 46 296 | 63 (10) | **b** | none: ship without AEC/NR | — | — | wraps `_SD_AEC`/`_SD_EQ`/`_SD_ASLC`, which are absent from the bundle |
| `libmpi_aenc` (437a8d) | 12 820 | 17 (13) | **b** for AAC/AMR | G.711 in Rust (`AK_AUDIO_TYPE_PCM_ULAW` exists in `ak_common_audio.h`) | 1–2 (G.711 only) | med | AAC/AMR encoders absent from the bundle |
| `libmpi_adec` (9a1083) | 12 496 | 17 (14) | b | not needed | — | — | |
| `libmpi_mux` / `libmpi_demux` (753166 / a73ba9) | 12 128 / 8 604 | 21 / 22 | c over b | a Rust muxer, if recording is wanted (not surveyed) | — | — | `MediaLib_*` absent |
| `libmpi_svp`, `libmpi_svp2`, `libplat_npu` (96246b / 551e3c / b4139a) | 27 124 / 41 172 / 2 468 | 31 / 14 / 6 | **b** | none | — | — | CNN runtime and model blobs absent. `/dev/npu` is not in the AV100 `ls_dev.txt` |
| `libmpi_osd`, `libapp_osd_ex` (770431 / 529b74) | 13 520 / 14 036 | 20 / 32 | a | Rust OSD, if wanted | 3–6 | low | not in any current plan |
| `libapp_video`, `libapp_its`, `libapp_ats`, `libapp_mem_ex`, `libplat_uuid` | 10 744 / 7 956 / 17 132 / 2 776 / 2 404 | 30 / 5 / 6 / 7 / 10 | c | nothing | 0 | high | vendor app helpers |

### 4.3 EV200: dropped on the AV100

Action for every row: **delete the consumer** (§5 step 1). The combined cost is
in the totals. "Rewrite (EV200 hw)" is a rough figure for completeness only,
with low confidence throughout.

| Library (md5) | .text | Used R+S / fn exports | ioctl / devices | Hdr | Class | Rewrite (EV200 hw) |
|---|---|---|---|---|---|---|
| `libplat_thread` (f94db1; `video/` variant a6d3fb) | 1 024 / 1 076 | 0+14 / 17; 0+23 / 30 | — | 15/17 | c | 0–0.5 |
| `libplat_common` (abd058) | 6 840 | 5+12 / 39 | `/dev/mmcblk0` | 15/39 | c | 0.5–1 |
| `libplat_ipcsrv` (89949b) | 3 848 | 0+0 / 8 | — | 6/8 | c | 0 (unused) |
| `libplat_drv` (385740) | 8 356 | 10+0 / 26 | 3 sites; `/dev/ak-motor{0,1}`, `/dev/input/event0`, `/dev/watchdog` | 24/26 | a | 2–4 (`crates/anyka-ptz/src/stepper.rs`, `cross-compile/IOT-ANYKA-PTZdaemon`) |
| `libplat_ai` (e6429f) | 9 880 | 14+2 / 21 | 16 sites; `/dev/akpcm_cdev1` | 19/21 | a | 3–5 |
| `libakaudiofilter` (938d71) | 22 764 | 0+5 / 56 | `/dev/mem` | 0/56 | b | impractical (stripped DSP) |
| `libakaudiocodec` (827e94) | 585 224 | 0+5 / 871 | `/dev/mem` | 0/871 | b | impractical |
| `libmpi_aenc` (537336) | 6 980 | 8+1 / 13 | — | 11/13 | a over b | 1–2 (G.711 only) |
| `libmpi_aed` (d5fae0) | 5 088 | 0+0 / 9 | — | 7/9 | — | 0 (unused) |
| `libakispsdk` (9e0ece) | 2 428 | 0+95 / 100 | `isp_sdk_ioctl`; ~93 `'I'` codes; `/dev/isp_char` | 0/100 | a | 3–6 (decompile exists in `references/`) |
| `libakuio` (5e01f5) | 5 780 | 0+9 / 17 | 5 sites; `/dev/uio0`, `/dev/ion` | 0/17 | a | 1–3 |
| `libplat_vi` (f924ed) | 29 416 | 13+16 / 84 | 1 + 15 via `xioctl`; ~10 `'V'` codes; `/dev/video0` | 22/84 | a | 5–10 |
| `libplat_vpss` (aad4fb) | 4 880 | 1+3 / 31 | — | 0/31 | a | 1–2 |
| `libmpi_venc` (19bb0f) | 18 600 | 7+0 / 26 | — | 24/26 | a over b | 3–6 |
| `libakstreamenc` (3dae5c) | 109 472 | 0+13 / 171 | — (registers via `akuio_map_regs`) | 0/171 | b | 20–40+ (userspace H.264/JPEG register programming and rate control; `references/ak3918ev200/docs/HW-VENC-en.md` maps part of it) |
| `libakv_encode` (32e8d8), C++ | 235 284 | 0+0 / 572 | — | 1/572 | b | 0 (unused) |
| `libakmedia` (cf4e80) | 248 280 | 0+0 / 554 | — | 0/554 | b | 0 (unused) |
| `libplat_venc_cb` (60c9d8) | 5 516 | 0+0 / 21 | — | 0/21 | a | 0 (unused) |
| `libakae` (c32e74) | 299 020 | 0+19 / 414 | — (sockets) | 0/414 | c: an RTSP/RTP server (`akae_rtsp_server_*`, `akae_rtp_session_send_h264_1slice`) | Rust RTSP crate |
| `libapp_rtsp` (8a927f) | 53 964 | 4+5 / 74 | — | 5/74 | c | Rust RTSP crate |
| `libapp_net` (676030) | 3 548 | 0+2 / 12 | 5 sites | 0/12 | c | `std`/`nix` |

Note the circular binding: EV200 `libplat_ai` binds 5 symbols from
`libapp_rtsp`.

### 4.4 Runtime objects

| Object | .text | Needed by | Action |
|---|---|---|---|
| `libgcc_s.so.1` | 67 612 | **every Rust binary**: 13 symbols (`_Unwind_*`, `__aeabi_unwind_cpp_pr{0,1}`, `__gnu_unwind_frame`) | **Keep.** It is the unwinder for `panic = "unwind"` (workspace `Cargo.toml` explains why). Dropping it means `panic = "abort"` or statically linking the unwinder. That is a policy change, not a rewrite, and not estimated here |
| `libstdc++.so.6` | 464 036 | only `libakv_encode.so` | **Goes away** once `libakv_encode` is no longer loaded |
| `libpthread/libm/librt/libdl.so.0` | — | crosstool link names | stubs, unaffected |
| `libc.so.0`, `ld-uClibc.so.0` | — | everything | firmware's, unaffected |

## 5. Recommended order

1. **Drop the EV200 generation (1–3 days, high confidence).** Remove
   `ai_demo`, `aenc_demo`, `md_demo`, `venc_demo`, `jpeg_snapshot` and
   `rtsp_demo` from `xtask/src/tasks.rs`, and their entries from
   `supervisor.toml` (both are already disabled). Build `anyka-ptz` without its
   default `sdk` feature for `onvif`, and retire the EV200 `anyka-link` variants.
   This removes 22 objects plus `libstdc++.so.6`. Functionality lost on the
   AV100: none (§2).
2. **Stop staging the 18 unloaded V8 objects (0–0.5 days).** Stage exactly the
   closure of what is linked. As a side benefit, the SD card then carries fewer
   NDA-origin files: `sdk/v8/PROVENANCE.md` says to check licensing before
   redistributing them.
3. **Fix `--no-as-needed` for `libplat_timer` (in the step-2 change).** It is
   loaded and binds nothing.
4. **Rust RTSP server over V8 venc output (10–20 days).** This is the highest
   user-visible value: RTSP and `/api/snapshot.jpg` are the two missing features
   (HARDWARE-FINDINGS §9). It can be built **against the vendor V8 video-input
   objects first**, and those can be replaced afterwards. Split:
   * `/dev/venc` client in Rust replacing `libmpi_venc`: 5–10 days, needs the
     camera.
   * RTSP/RTP server: 5–10 days. Crates confirmed on crates.io (`cargo search`,
     2026-10-09):
     - `rtsp-types` 0.1.3: parser/serializer only (MIT).
     - `rtsp-runtime` 0.7.0: "Sans-IO RTSP 1.0 session engine — driveable client
       + server state machines" (MIT/Apache-2.0).
     - `rtsp-rs` 0.1.8: "RTSP server library for live media streaming" (MIT).
     - `shiguredo_rtsp`: canary release.
     - `rtp` 0.17.2: webrtc-rs.

     `retina` 0.4.21 is a client and does not serve. **None has been built for
     `armv5te-unknown-linux-uclibceabi`.** The no-`AtomicU64` rule
     (IMPLEMENTATION.md) has already eliminated three crates, so check each
     candidate's dependency tree first. A hand-written server on `rtsp-types`
     with RFC 6184 FU-A packetisation is the fallback, inside the same estimate.
5. **Replace the V8 video-input closure (24.5–59 days).** Do it in this order:
   `thread`/`common`/`log`/`dbg`/`timer` (trivial) → `osal` + `mem` (ION) →
   `isp_sdk` → `vi` + `vpss` (one unit). It is lower value than step 4, because
   the vendor objects already work for this hardware, and it is the largest risk:
   the v7 config format and the ~94 ISP payload layouts are undocumented. Defer
   the day/night/anti-flicker policy and replace it last.
6. **Audio, if wanted (4–8 days):** raw PCM from `/dev/pcmC0D0c` plus G.711. Do
   not attempt AEC/NR or AAC/AMR (class b, objects absent).

### What `arch-recompiler` (ARC) can take off this

ARC is early. Its roadmap (`.specs/project/ROADMAP.md`) is at M1: the F0 POC
and the F1 harness. The rule engine (F6) does not exist yet. What works today
is per-function lifting by Sonnet subagents, checked by `kotest` differential
execution, and it has only been applied to kernel modules. Measured on 5
drivers (77 functions), it costs **6–29 minutes of subagent time per KB of
`.text`** (`.specs/project/driver-metrics.md`). For the 186 KB of V8 `osal`,
`mem`, `isp_sdk`, `vi` and `vpss`, that is about 19–90 hours to get C.

The output is C, not Rust. Its value here is **ABI discovery**: ioctl numbers,
payload layouts and call order. The oracle cannot judge ioctl side effects
(STATE R3), and it has not been used on userspace `.so` files. A conservative
estimate is **10–20% off the class-(a) V8 rows (about 2–12 days)**. That
reduction is not applied in the totals. Codec rows get nothing: lifting does
not make a proprietary algorithm practical to rewrite.

### Other in-repo material that helps

* `AK3918AV100_EVB_CBDM_KM01N/`: kernel-side facts for `ak_isp`,
  `ak_venc_adapter`, `ak_venc_bridge`, `ak_pcm`, `ak_uio`. `chip/ion.md`
  (stock ION) and `chip/ptz-motor.md` are already recovered.
  `ghidra/exports/*.ko.symbols.json` has symbol tables for the same modules.
* `references/ak3918ev200/`: EV200 decompiles (`libakispsdk.so.c`,
  `libakmedia.so.c`), the `ipc_tracer` LD_PRELOAD ioctl tracer, and
  `HW-VENC-en.md`. These are a different generation. The tracer is the most
  reusable piece, for capturing V8 ioctl traffic on the camera.
* `cross-compile/IOT-ANYKA-PTZdaemon/`: EV200 `ak_drv_ptz_*` usage. It is moot
  on the AV100.
* No kernel source for the AV100 video, audio or ISP drivers is in the repo.
  `AK3918AV100_EVB_CBDM_KM01N/driver-sources.md` points to tuya's EV330
  `driver_src_anyka` (`ak_isp`, `ak_pcm`, `ak_ion`, `ak_uio`), which is not
  vendored.

## 6. What removes which bundled library

| Action | Removes from the card |
|---|---|
| Drop the six EV200 demos | every object in `lib/audio/`, `lib/md/`, `lib/video/`; everything in `lib/rtsp/` except `libplat_drv`, `libplat_common`, `libplat_thread` |
| Build `onvif` without `anyka-ptz/sdk` | the remaining `lib/rtsp/` objects (`libplat_drv`, `libplat_common`, `libplat_thread`). `lib/rtsp/` is then empty |
| No more `libakv_encode` (follows from the first action) | `lib/sys/libstdc++.so.6` |
| Stage only linked V8 objects | the 18 V8 objects listed in §1 |
| `--as-needed` for V8 | `libplat_timer.so` |
| Rust video input (§5 step 5) | `libplat_{vi,vpss,isp_sdk,osal,mem,common,thread,log,dbg}.so`. `lib/v8/` is then empty |
| Rust `/dev/venc` client | V8 `libmpi_venc.so` (never needs bundling) |
| Rust PCM + G.711 | V8 `libplat_ai.so`, `libmpi_aenc.so` (never need bundling) |
| Nothing planned | `lib/sys/libgcc_s.so.1` (Rust unwinder), `libc.so.0`/`ld-uClibc.so.0` (firmware) |

## 7. How the numbers were taken

Tools: `/opt/arm-anykav200-crosstool/usr/bin/arm-anykav200-linux-uclibcgnueabi-{readelf,nm,size}`
and host `llvm-objdump`/`llvm-readelf`.

* Dedupe: `md5sum lib/*/*.so`. The 82 files are 50 distinct objects: 22 EV200
  (two `libplat_thread` variants) and 28 V8.
* Size: `.text` from `size -A`. Exports: `nm -D --defined-only`, type `T`.
* Used: undefined symbols of each binary and of every co-loaded object
  (transitive `NEEDED` within the variant directory), intersected with each
  object's exports.
* ioctl: EV200 counts `bl … <ioctl@plt>` sites in `llvm-objdump -d`. V8 objects
  are non-PIC and call through literal pools, so the count is `R_ARM_ABS32`
  relocations against `ioctl` (`llvm-readelf -r`).
* ioctl codes: 32-bit words in executable sections that decode as `_IOC` with a
  printable type byte. This is a heuristic. Codes built with `mov`/`orr` are
  missed, so treat the counts as lower bounds.
* Devices: `strings | grep ^/dev/`, compared with the AV100's `ls_dev.txt`.
* Header coverage: each exported function name grepped in the matching
  `include/` directory.

**Estimates are judgement anchored on those measurements**, not derived from a
formula. Each one includes on-camera validation, and every class-(a) video and
audio row needs the camera reachable (HARDWARE-FINDINGS §8).
