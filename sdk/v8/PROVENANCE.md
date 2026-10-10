# V8 SDK provenance — AK3918AV100

These are the vendor objects and headers for the SoC this camera actually has.
The set one level up (`sdk/include`, `sdk/lib`) is for a **different** part
(AK3918EV200) and is kept because the already-verified binaries build against
it. See `../../docs/SDK-SOURCES.md`.

## Source

`AnyCloud39AV100_SDK_V1.12`, mirrored at
<https://github.com/KD0000001/ak3918av100> (branch `main`).

Anyka does not publish SDKs. This is a mirror of a drop distributed under NDA —
**check licensing before redistributing any of it**.

## How this directory was produced

```sh
# headers — 35 files
curl -s https://api.github.com/repos/KD0000001/ak3918av100/contents/platform/include \
  | jq -r '.[] | select(.type=="file") | .download_url' \
  | xargs -n1 -P8 curl -sO
# -> sdk/v8/include/

# objects — platform/lib holds only a sample; the real ones ship inside the
# squashfs image for the camera's /usr partition
curl -L -o usr.sqsh4 \
  https://raw.githubusercontent.com/KD0000001/ak3918av100/main/rootfs/usr.sqsh4
unsquashfs -d usrfs usr.sqsh4
cp usrfs/lib/lib{plat,mpi,app}_*.so   # -> sdk/v8/lib/
```

`usr.sqsh4`: squashfs 4.0, xz, 3 406 508 bytes, 368 inodes, created
2026-06-08. It also carries `usrfs/modules/ak_isp.ko`, not vendored here.

## Why this is the right SDK

Two version strings match the live camera's own runtime log exactly:

| Library | Here | Camera `/tmp/ipc.log` |
| --- | --- | --- |
| `libplat_isp_sdk.so` | `V8.0.01` | `ispsdk_lib version:libplat_isp_sdk V8.0.01` |
| `libmpi_venc.so` | `V2.0.46` | `[Platform_AV_Init] venc version: libmpi_venc V2.0.46` |

Two independent matches. For contrast, the EV200 set has no
`libplat_isp_sdk.so` at all — its ISP library is `libakispsdk.so` V3.1.00.

## Contents

28 objects: `libplat_{ai,ao,common,dbg,isp_sdk,log,mem,npu,osal,thread,timer,
uuid,vi,vpss,vqe}.so`, `libmpi_{adec,aenc,demux,mux,osd,svp,svp2,venc}.so`,
`libapp_{ats,its,mem_ex,osd_ex,video}.so`.

35 headers, including ones with no EV200 equivalent: `ak_npu.h`, `ak_svp.h`,
`ak_svp2.h`, `ak_object_filter.h`, `ak_mem.h`, `ak_log.h`,
`ak_common_video.h`.

**Absent:** `libapp_rtsp.so` and `libapp_net.so`. This generation ships no RTSP
server, so `rtsp_demo` has no direct counterpart.

All objects are `ELF 32-bit LSB shared object, ARM, EABI5, stripped`.
