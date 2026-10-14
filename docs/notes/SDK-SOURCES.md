# Finding the SDK that matches this camera

Short version: **the matching SDK exists publicly and has been located.** The
chip is not the one this repository was built around, which is why nothing
video-related worked.

---

## The chip is AK3918AV100, not AK3918EV200

Straight from the camera's `/proc/cpuinfo`:

```
model name : ARM926EJ-S rev 5 (v5l)
Hardware   : AK3918AV100
```

Everything vendored in `cross-compile/` and in `references/ak3918ev200/`
targets the **EV200**: kernel 3.4.35, `libakispsdk.so` **V3.1.00**, the
`ak_info_dump.ko` / `/sys/ak_info_dump/sensor_if` sensor interface, ISP configs
v3/v4.

This camera is a different part and a different generation: kernel **4.4.282**,
device-tree based, `libplat_isp_sdk` **V8.0.01**, a V4L2 subdev sensor
interface (`/dev/v4l-subdev0`), ISP configs **v7**. The AV100 is Anyka's AIoT
part — it has an NPU, which the EV200 does not.

That single mismatch explains every video and audio failure recorded in
`HARDWARE-FINDINGS.md`.

## Where the matching SDK is

**`https://github.com/KD0000001/ak3918av100`** — `AnyCloud39AV100_SDK_V1.12`,
a complete SDK tree (~200 MB): `os/` (kernel), `platform/` (headers + samples),
`rootfs/`, `image/` (device trees), `scripts/`, `tools/`.

Anyka does not publish SDKs; this is a mirror of an NDA drop. Treat the
licensing accordingly before redistributing any of it.

### The libraries are in `rootfs/usr.sqsh4`

`platform/lib/` holds only a sample. The prebuilt platform objects ship inside
the squashfs image for `/usr`:

```sh
curl -L -o usr.sqsh4 \
  https://raw.githubusercontent.com/KD0000001/ak3918av100/main/rootfs/usr.sqsh4
unsquashfs -d usrfs usr.sqsh4
ls usrfs/lib/            # 45 shared objects, 28 of them SDK
ls usrfs/modules/        # includes ak_isp.ko
```

### Confirmed to match the live camera

Two version strings from the extracted libraries are **identical** to what the
camera's own vendor application printed at runtime:

| Library | SDK tree | Live camera log |
| --- | --- | --- |
| `libplat_isp_sdk.so` | **V8.0.01** | `ispsdk_lib version:libplat_isp_sdk V8.0.01` |
| `libmpi_venc.so` | **V2.0.46** | `[Platform_AV_Init] venc version: libmpi_venc V2.0.46` |

Two independent matches is conclusive. Compare against the vendored EV200 set:
`libakispsdk.so` V3.1.00 — a different library name, even.

SDK objects present: `libplat_{ai,ao,common,dbg,isp_sdk,log,mem,npu,osal,thread,timer,uuid,vi,vpss,vqe}.so`,
`libmpi_{adec,aenc,demux,mux,osd,svp,svp2,venc}.so`,
`libapp_{ats,its,mem_ex,osd_ex,video}.so`.

Note what is **absent**: there is no `libapp_rtsp.so` or `libapp_net.so` in
this generation, so `rtsp_demo` has no direct counterpart — RTSP would have to
come from `libapp_video.so` or be implemented over the encoder output.

## The API is a different generation, not a revision

Headers live at `platform/include/` and include things the old SDK has no
concept of: `ak_npu.h`, `ak_svp.h`, `ak_svp2.h`, `ak_object_filter.h`,
`ak_mem.h`, `ak_log.h`, `ak_common_video.h`.

The video-input model changed from "open a device, set channel attributes" to a
separated device/channel model closer to other vendors' MPI:

| Old (EV200, V3.1.00) | New (AV100, V8.0.01) |
| --- | --- |
| `ak_vi_open` | `ak_vi_enable_dev` / `ak_vi_enable_chn` |
| `ak_vi_match_sensor` | `ak_vi_load_sensor_cfg` / `ak_vi_load_sensor_cfg_ex` |
| `ak_vi_capture_on` / `ak_vi_capture_off` | `ak_vi_enable_chn` / `ak_vi_disable_chn` |
| `ak_vi_set_channel_attr` | `ak_vi_get_chn_attr` / `..._ex`, `ak_vi_get_dev_attr` |
| `ak_vi_get_fps` | `ak_vi_change_chn_fps` / `ak_vi_change_dev_fps` |
| — | `ak_vi_get_sensor_id`, `ak_vi_create_stitch_chn`, `ak_vi_get_y_data`, `ak_vi_draw_box` |

`ak_vi_get_frame`, `ak_vi_close` and `ak_vi_get_sensor_resolution` survive by
name.

### What porting would involve

1. Point `sdk/include` at `platform/include` and `sdk/lib` at the objects from
   `usr.sqsh4`. The per-variant split in `anyka-link` collapses — this is one
   coherent set, which is simpler than what is there now.
2. Regenerate bindings. Automatic; `anyka-sys` needs no change beyond the
   header list in `wrapper.h`.
3. **Rewrite the safe wrappers for the device/channel model.** `anyka::vi` is a
   genuine rewrite; `venc`, `ai`, `aenc` and the VPSS motion statistics need
   checking against their new headers. This is the real work.
4. Update the demos to the new shapes, and decide what `rtsp_demo` becomes
   given there is no `libapp_rtsp.so`.

Everything outside the SDK boundary — `supervisor`, `sshd`,
`telemetry`, `os`, `xtask`, `init.sh`, the linking machinery — is
unaffected, and all of it is already verified on this exact camera.

## OpenIPC: no support for this chip

`AK3918AV100` does **not appear** on OpenIPC's Anyka SoC list at all. What is
listed, and its stage:

| SoC | Stage |
| --- | --- |
| AK3916EV300 | NEQ — SDK but no hardware |
| AK3916EV301 | R&D |
| AK3918EV200 | **HLP** — "we have hardware and SDK, basic stuff done, but we are stuck" |
| AK3918EV300 | NEQ |
| AK3918EV330 | R&D |

None has a ready solution; OpenIPC's own scale runs NEQ → R&D → HLP → WIP →
MVP → DONE, and no Anyka part has reached WIP.

Two near misses worth knowing about:

* `github.com/fdragovic/openIPC-firmware` is titled "(AK3918AV100 build)", but
  it is a personal fork with no AV100 configuration, no releases and no
  activity. There is nothing usable in it.
* `github.com/OpenIPC/ak3918ev200` is the serious reverse-engineering work —
  ioctl maps, decompiled ISP, LD_PRELOAD hooks — and is already vendored here
  under `references/ak3918ev200/`. It is **EV200**, so it describes the V3.1.00
  generation, not ours.

`thingino` tracks the AV130 in a discussion thread but does not treat the part
as a priority.

**Conclusion:** OpenIPC is not a path to video on this camera today. The
AnyCloud39AV100 SDK is.
