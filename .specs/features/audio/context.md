# Audio — User Decisions (2026-10-17)

| Gray area | Decision |
| --- | --- |
| Secure context for browser mic (PTT) | Self-signed HTTPS in `web`, cert generated on first boot, persisted on SD. HTTP kept. |
| ONVIF talk-back | Included as P2 (RTSP backchannel + AudioOutput ops). |
| Web extras | Listen in browser (P2) and gain/volume controls (P2), both in scope. |
| Codec | G.711 A-law (PCMA, PT 8, 8 kHz) — agent default, universal NVR support; no AAC in tree. |

## Research inputs

- AV100 codec = `ak_pcm.ko` char devices `/dev/pcmC0D0{p,c,l}` (major 248; **not ALSA**). Ioctls (type 'P'): SET_PARS `0x401c50e0` (7×u32 `{?, rate, ch, bits, period_bytes, periods, ?}`, ipc used `{0,16000,1,16,2048,8,0}`), GET_PARS `0x801c50f2`, SET/GET source `0x40045010/0x80045010`, SET/GET gain `0x40045030/0x80045030`, start `0x50e1`, `0x50e3` (?), rate `0x800450ea`, `0x40045081` (SET_SPEAKER?, playback). Capture min period 512 B. Source: disassembly of unstripped `ak_pcm.ko` + `tools/re/traces/ipcrun/ipc.trace:5208` + dmesg.
- Capture sequence never observed; GPIO 37 ownership (driver DT `speak-gpios` vs lights sysfs) unknown → hardware spike first.
- Kernel 4.4.282 (not 3.4).
