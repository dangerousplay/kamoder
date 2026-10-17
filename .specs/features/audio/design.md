# Audio — Design

Status: **draft** (spec: `spec.md`, decisions: `context.md`). Nothing verified on hardware yet.

## 1. Architecture

```
            /dev/pcmC0D0c ──┐            ┌── /dev/pcmC0D0p      (ak_pcm.ko, AV100)
                            ▼            ▲
                ┌──────────────── audio ────────────────┐
                │ Backend trait  (anyka-av100::pcm / Fake)│
                │ capture task → broadcast<Frame 20 ms>   │
                │ playback task ← mpsc (one talker lock)  │
                │ amp on/off, gain store etc/audio.json   │
                └──────── /tmp/kamoder-audio.sock ────────┘
                  ▲ capture        ▲ play          ▲ status/set
          ┌───────┘          ┌─────┴──────┐         │
        rtsp (PCMA track,  web (WS talk,  web (/api/audio/devices)
        backchannel play)   WS listen)    onvif (audio configs)
```

- **D-A1 One owner.** New service `audio` owns the codec (ARCHITECTURE D4). Not in `camerad`: venc must never restart, and audio failures must not touch video.
- **D-A2 Hardware ABI in `anyka-av100::pcm`** (hardware crate, constants + ioctls, `rustix`/`libc` like `v4l2.rs`). Generic service `crates/audio` holds the `Backend` trait, resampler, G.711, socket server. Board selects backend via `[audio] backend = "ak-pcm"`.
- **D-A3 Wire format = S16LE mono 8 kHz, 20 ms frames (160 samples, 320 B)**, framed with the video-proto header (`[len u32 BE][flags u8][ts_ms u64 BE]`), reusing `video-proto` encode/decode helpers via `audio-proto`. Consumers encode PCMA themselves (rtsp) or pass PCM through (web). G.711 a-law/µ-law tables live in `audio-proto::g711` (pure, shared by rtsp encode + backchannel decode).
- **D-A4 One socket, op-first handshake.** Client sends one JSON line: `{"op":"capture"}` → server replies `{"ok":…format}` then streams frames; `{"op":"play","client":"web"|"onvif"}` → `{"ok":…}` or `{"error":"busy"}`, then the client writes frames; `{"op":"status"}` / `{"op":"set","device":…,"gain":n}` → one JSON reply. Pattern from `lights-proto`/`follow-proto`; socket 0600, ≤ 8 connections.
- **D-A5 Sample rate.** Ask the driver for 8000 Hz (SDK enum allows it). If spike T0 shows only 16 kHz works, capture at 16 kHz and decimate 2:1 with an integer half-band FIR (11 taps, Q15); playback upsamples 1:2 symmetric. Choice recorded per board in `[audio]`.
- **D-A6 Speaker amplifier.** Resolved by spike T0:
  - (a) driver owns GPIO 37 (DT `speak-gpios`) → `audio` uses ioctl `0x40045081`; remove the `amp` line from `boards/anyka-av100-ptz.toml` `[lights]`;
  - (b) sysfs works → `audio` calls `lights-proto` `SetMute(false/true)` (lights stays sole GPIO owner).
  Either way the backend exposes `set_speaker(bool)`; amp off 1 s after last frame (AUD-20).
- **D-A7 Capture on demand.** Capture runs while ≥ 1 subscriber; stops 5 s after the last leaves (CPU on idle boards). RTSP subscribes only while a session plays audio.

## 2. Components

| Crate | Change |
| --- | --- |
| `audio-proto` (new, generic) | `Request/Response` serde types, `Device {id, kind: Input/Output, name, state, rate, gain, gain_range}`, `DEFAULT_SOCKET`, frame consts (8000 Hz, 160 samples), async `Client::{status, set, capture() -> FrameStream, play() -> FrameSink}`, `g711::{alaw_encode, alaw_decode, ulaw_*}` |
| `audio` (new, generic bin) | `Backend` trait `{open_capture(rate), read_period, open_playback(rate), write_period, set_gain(dev, n), set_speaker(bool)}`; `FakeBackend` (sine/silence, records playback); `resample` (half-band); `store` (`etc/audio.json`); `server` (socket); `runtime` (capture broadcast, single talker lock, silence fill, amp tail); metrics `kamoder_audio_*`; `main` (clap, telemetry, SIGTERM) |
| `anyka-av100::pcm` | `PcmParams` 7×u32, ioctl consts from `context.md`, `PcmDevice::{open_capture, open_playback, set_pars, start, set_gain, set_source, set_speaker}`; `impl audio::Backend` lives in `audio` behind feature `ak-pcm` (like camera's backend feature) |
| `board` | `audio: Option<Audio>` `{backend, rate, inputs: [{id,name,node}], outputs: [{id,name,node}], gain defaults}`, validation; AV100 profile `[audio]`; `generic-v4l2` none |
| `rtsp` | `AudioSource` seam (socket client / fake); `Mount` optional audio; SDP 2nd `m=audio` PCMA trackID=1; per-track transports in `Session` (interleaved 0-1 video, 2-3 audio, 4-5 backchannel); `PCMPacketizer` from `msf-rtp` `pcm` feature; RTP ts = capture ts × 8; RTP-Info both tracks. P2: `Require: …/backchannel` → `sendonly` trackID=2, receive interleaved/UDP RTP → `g711::decode` → audio `play` |
| `onvif-schema` | add Media ops (Get/Add/Remove/Set Audio Source/Encoder/Output/Decoder Configuration(s), Options, GetAudioOutputs) to `xtask/src/gen_onvif.rs` `SERVICES`; `./x gen-onvif` |
| `onvif` | `media.rs`: audio source/encoder (+P2 output/decoder) configs from board + `audio` status; profiles include them; handlers registered in `ACTIONS`; faults via generated types |
| `web` | `--tls-listen 0.0.0.0:443`, `tls.rs`: `rcgen` self-signed (SAN = hostname + IPs) persisted `etc/web-tls.{crt,key}`, `tokio-rustls` acceptor wrapped as an `axum::serve::Listener`; axum `ws` feature; `audio.rs`: `GET /api/audio/devices` (AuthUser), `PUT /api/audio/devices/{id}` (AdminUser), `GET /api/audio/talk` WS (admin, `?token=`), `GET /api/audio/listen` WS (stream user, `?token=`); 503 `audio_unavailable` |
| `webui` | `lib/api.ts` audio calls; `components/audio/audio-card.tsx` (device list, gain/volume sliders, Listen toggle, hold-to-Talk); `lib/audio/talk-worklet.ts` (mic → 8 kHz S16 frames, 20 ms) and `listen-worklet.ts` (jitter buffer ~200 ms → output); HTTP page → disabled Talk + HTTPS link |
| `supervisor` / packaging | `Capability::Audio` (board has `[audio]`); `tools/supervisor.toml` `[[service]] audio when="audio"` after lights, before rtsp; rtsp/web/onvif `--audio-socket`; web `--tls-listen`; xtask `BINARIES`, `HOST_TESTABLE`, `GENERIC_CRATES`, `HARDWARE_LITERALS += /dev/pcm` |

## 3. Key flows

**Talk (web):** button down → `getUserMedia` → AudioWorklet resample (linear + 1-pole LPF at 3.4 kHz) → 320 B frames → WS binary → web `audio-proto::Client::play("web")` (busy → close 4409 → UI toast) → audio: amp on, write periods; underrun → silence; WS close → 1 s tail → amp off.

**RTSP:** DESCRIBE adds audio only when board has `[audio]` (AUD-13). PLAY with audio set up → `Client::capture()`; frames → `alaw_encode` → `PCMPacketizer` → per-track transport. Socket drop → reconnect with backoff; video unaffected.

**TLS:** first start: no cert → generate ECDSA P-256 (ring) cert valid 10 y, write 0600 atomically. Failure → log + HTTP only.

## 4. Risks

| Risk | Mitigation |
| --- | --- |
| Capture sequence unobserved; reads may block forever / zeros | Spike T0 before T4; probe binary `tools/re/pcm-probe` |
| GPIO 37 double owner | D-A6 decided by T0 |
| RTSP multi-track refactor breaks existing clients | Keep single-track paths; retina itest for video-only + video+audio |
| Binary size: rustls+rcgen+ws in web (~0.6–0.9 MB) | Measure; `ring` already in tree; no `aws-lc` |
| RAM | audio ≤ 2 MB; broadcast capacity 25 frames (500 ms) |
| Browser AudioWorklet on old mobiles | Fallback ScriptProcessor not planned; document |

## 5. Hardware spike T0 (needs camera, user OK)

Stop vendor `ipc` (not running under Kamoder), then with `tools/re/pcm-probe` (armv5te, static, ~200 KB):
1. `ls -l /dev/pcmC0D0*; grep pcm /proc/devices; dmesg | grep -i pcm`; `cat /sys/kernel/debug/gpio` (if mounted), `ls /sys/class/gpio`.
2. Capture: SET_PARS `{0,8000,1,16,512,8,0}` → start → read 5 s → stats (RMS, zero ratio) + base64 wav; repeat 16000/2048.
3. Playback: SET_PARS, SET source 4, start, write 2 s 1 kHz tone; with/without ioctl `0x40045081` 1/0 and lights amp unmute; user listens.
4. Record findings in `docs/notes/HARDWARE-FINDINGS.md` §audio and `context.md`.
