# Audio (microphone, speaker, push-to-talk) Specification

## Problem Statement

The AV100 board has a microphone and a speaker (amplifier on GPIO 37), but Kamoder has no audio: RTSP is video-only, ONVIF reports no audio sources, and the web UI cannot list, hear or drive either device. Owners expect what the stock `ipc` app and commercial cameras offer — sound on the NVR stream and two-way talk. The vendor audio libraries are off-limits (no SDK at runtime), so capture and playback must talk to the `ak_pcm.ko` driver directly.

## Goals

- [ ] Mic audio present as a G.711 track on every `/vsN` RTSP stream and declared in every ONVIF media profile; plays in VLC, ffprobe and an ONVIF NVR (ONVIF Device Manager / Frigate).
- [ ] Web UI lists the board's microphone(s) and speaker(s), plays the live mic, and sends push-to-talk audio to the speaker with ≤ 500 ms mouth-to-speaker latency on the LAN.
- [ ] ONVIF clients can talk back through the RTSP backchannel (P2).
- [ ] New `audio` service ≤ 2 MB RSS and ≤ 5 % CPU on the AV100 while streaming; pure Rust, no vendor `.so`.

## Out of Scope

| Feature | Reason |
| --- | --- |
| AAC / Opus encoding | No encoder in the tree; G.711 is universal for ONVIF/RTSP and costs ~nothing on ARM926. |
| AEC / noise reduction / AGC | Lives in the stripped vendor `libakaudiofilter`; loopback node `pcmC0D0l` keeps a future software AEC possible. |
| Audio recording to SD, sound-detection events | Separate features. |
| CA-signed certificates / ACME | Camera lives on a LAN; self-signed cert, user accepts once. |
| Generic V4L2 board audio (ALSA/UVC mic) | Backend trait must allow it; implementation later. |
| ONVIF Media2 / DeviceIO services | WSDLs not vendored; Media1 audio ops cover NVRs in use. |

---

## User Stories

### P1: Audio service on the AV100 codec ⭐ MVP

**User Story**: As the firmware, I want one service that owns the codec so that RTSP, web and ONVIF share the mic and the speaker without fighting over the device.

**Acceptance Criteria**:

1. WHEN the board profile has an `[audio]` table THEN the `audio` service SHALL open the capture node and deliver mono S16 PCM continuously, using `ak_pcm` ioctls (SET_PARS `0x401c50e0`, start `0x50e1`) — no vendor library. (AUD-01)
2. WHEN capture runs THEN the service SHALL publish 8 kHz frames (resampled in software if the driver only gives 16 kHz) to subscribers over a unix socket, each frame stamped with a monotonic ms timestamp. (AUD-02)
3. WHEN a client sends PCM for playback THEN the service SHALL write it to the playback node and drive the speaker amplifier on for the duration (+ ≤ 1 s tail), off otherwise. (AUD-03)
4. WHEN two talkers request playback THEN the first SHALL hold the speaker and the second SHALL get a `busy` error. (AUD-04)
5. WHEN the device open/ioctl fails THEN the service SHALL log once, expose `kamoder_audio_errors_total`, retry with backoff, and never crash-loop the supervisor. (AUD-05)
6. The codec ABI SHALL sit behind a backend trait (capture/playback/gain) with a fake backend for host tests. (AUD-06)

**Independent Test**: On the camera, a host tool subscribes to the socket and writes 5 s to a `.wav`; it plays back the room sound. Sending a 1 kHz tone is heard on the speaker.

### P1: Device listing ⭐ MVP

**User Story**: As an owner, I want to see the camera's microphone and speaker in the web UI so that I know what audio the board has and whether it works.

1. The board profile SHALL declare audio devices (`[[audio.input]]`, `[[audio.output]]` with name, node, rate); boards without `[audio]` SHALL have no audio anywhere (no track, no ONVIF source, card hidden). (AUD-07)
2. WHEN `GET /api/audio/devices` is called by a logged-in user THEN web SHALL return each device (kind, name, state: active/idle/error, format, current gain/volume). (AUD-08)
3. WHEN the audio service is down THEN the API SHALL return 503 `audio_unavailable` and the UI SHALL show the card as unavailable. (AUD-09)

**Independent Test**: Dashboard shows "Microphone — active, 8 kHz G.711" and "Speaker — idle".

### P1: RTSP audio track ⭐ MVP

1. WHEN audio is configured THEN DESCRIBE on `/vsN` SHALL return a second media `m=audio 0 RTP/AVP 8` (`PCMA/8000`, `a=control:trackID=1`). (AUD-10)
2. WHEN a client SETUPs both tracks (UDP or interleaved TCP) THEN PLAY SHALL send video and audio with correct RTP-Info per track; clients setting up only video SHALL keep working unchanged. (AUD-11)
3. Audio RTP SHALL use 20 ms packets (160 samples), 8 kHz clock, timestamps derived from capture time so A/V drift stays < 100 ms over 10 min. (AUD-12)
4. WHEN the audio socket is down THEN RTSP SHALL keep serving video (the audio track stays declared but silent, or omitted at DESCRIBE time if the service is absent at startup). (AUD-13)

**Independent Test**: `ffprobe rtsp://cam/vs0` lists `pcm_alaw 8000 Hz mono`; VLC plays sound.

### P1: ONVIF audio capture ⭐ MVP

1. `GetAudioSources`, `GetAudioSourceConfigurations`, `GetAudioSourceConfiguration`, `GetCompatibleAudioSourceConfigurations` SHALL report one source per board input. (AUD-14)
2. `GetAudioEncoderConfigurations`, `GetAudioEncoderConfiguration`, `GetCompatibleAudioEncoderConfigurations`, `GetAudioEncoderConfigurationOptions` SHALL report G711, 8 kHz, 64 kbps. (AUD-15)
3. `GetProfiles`/`GetProfile` SHALL include the audio source + encoder configs on every profile; `GetStreamUri` stays `/vsN` (track carried there). (AUD-16)
4. `AddAudio*Configuration` / `RemoveAudio*Configuration` / `SetAudioEncoderConfiguration` SHALL be accepted when they request the supported values and fault `ter:ConfigModify` otherwise. All XML from generated `onvif-schema` types. (AUD-17)

**Independent Test**: ONVIF Device Manager shows audio on the profile and plays sound; `tools/camera-access/onvif_live.py` asserts the audio configs.

### P1: Push-to-talk in the web UI ⭐ MVP

1. web SHALL serve HTTPS (default port 443) with a self-signed certificate generated on first start and persisted under `etc/`; HTTP stays available. (AUD-18)
2. WHEN an admin holds the Talk button THEN the browser SHALL capture the mic, downmix/resample to 8 kHz mono S16, and stream it over a WebSocket (`/api/audio/talk`, stream-token auth); releasing (or pointer leaving, tab hidden) SHALL stop. (AUD-19)
3. WHEN talk starts THEN the speaker amp SHALL turn on, and turn off ≤ 1 s after the last frame or on disconnect. (AUD-20)
4. WHEN the page is plain HTTP THEN the Talk button SHALL be disabled with a link to the HTTPS URL. (AUD-21)
5. WHEN mic permission is denied or the speaker is busy THEN the UI SHALL show the reason and not hang. (AUD-22)
6. The admin SHALL pick which browser microphone talk uses (saved per browser) and SHALL be able to test it with a live level meter without talking; the server logs each talk's frame count and peak so silent input is diagnosable. (AUD-32, added 2026-10-17 after the default input captured silence)

**Independent Test**: Over HTTPS, hold Talk on a phone; voice comes out of the camera speaker.

### P2: Listen in browser

1. WHEN the user presses Listen THEN the UI SHALL stream live mic audio (WebSocket `/api/audio/listen`, PCM) and play it via Web Audio with ≤ 1 s latency. (AUD-23)
2. Listening SHALL not block RTSP subscribers or talk (full-duplex allowed; no echo cancellation promised). (AUD-24)
3. Listen SHALL also be offered on the video views (Cameras page, dashboard live view) when the board has a microphone. (AUD-33)

### P2: Gain and volume

1. Admin SHALL set mic gain and speaker volume from the UI (`PUT /api/audio/devices/{id}`) within driver ranges (ak_pcm gain ioctl `0x40045030`). (AUD-25)
2. Values SHALL persist across restarts (`etc/audio.json`) and be applied at service start. (AUD-26)
3. ONVIF `AudioSourceConfiguration` / `AudioOutputConfiguration.OutputLevel` SHALL reflect the same values. (AUD-27)

### P2: ONVIF talk-back (backchannel)

1. `GetAudioOutputs`, `GetAudioOutputConfiguration(s)`, `GetAudioOutputConfigurationOptions`, `GetAudioDecoderConfiguration(s)` SHALL report the speaker (G711 8 kHz) and profiles SHALL include them. (AUD-28)
2. WHEN DESCRIBE carries `Require: www.onvif.org/ver20/backchannel` THEN the SDP SHALL add a `sendonly` PCMA media (`trackID=2`); other clients SHALL not see it. (AUD-29)
3. WHEN the client sends RTP on the backchannel (interleaved TCP, and UDP) THEN rtsp SHALL decode G.711 and play it through the audio service, subject to AUD-04 busy rules. (AUD-30)
4. `SetAudioOutputConfiguration` SHALL set the volume (shares AUD-25 storage). (AUD-31)

**Independent Test**: ONVIF Device Manager / Frigate two-way audio is heard on the camera.

---

## Edge Cases

- WHEN the vendor `ipc` still holds the codec ("capture device is already open!") THEN the service SHALL report error state, not spin.
- WHEN GPIO 37 is owned by the `ak_pcm` driver (DT `speak-gpios`) THEN amp control SHALL go through the driver ioctl, and the `lights` `amp` line SHALL be dropped from the profile (decided by spike T0).
- WHEN no subscriber exists THEN capture MAY stop to save CPU; restart latency ≤ 300 ms.
- WHEN a talk WebSocket stalls THEN playback SHALL insert silence, not block capture.
- WHEN the self-signed cert is missing/corrupt THEN web SHALL regenerate it and keep serving HTTP if TLS setup fails.
- WHEN a browser sends the wrong frame size/rate THEN the server SHALL drop the frame and count `kamoder_audio_talk_bad_frames_total`.

---

## Requirement Traceability

| ID | Story | Status |
| --- | --- | --- |
| AUD-01..06 | P1 Audio service | Implemented (`crates/audio`, `anyka-av100::pcm`); capture + playback verified on AV100 with pcm-probe (2026-10-17) |
| AUD-07..09 | P1 Device listing | Implemented (board `[audio]`, web `/api/audio/devices`, webui `AudioCard`) |
| AUD-10..13 | P1 RTSP track | Implemented (`crates/rtsp` PCMA trackID=1), retina-tested |
| AUD-14..17 | P1 ONVIF capture | Implemented (`crates/onvif/src/audio.rs`); faults without subcodes (onvif-device-rs limit) |
| AUD-18..22, 32 | P1 Push-to-talk | Implemented (web `--tls-listen`, `/api/audio/talk` WS, webui hold-to-talk) |
| AUD-23..24, 33 | P2 Listen | Implemented (`/api/audio/listen` WS, worklet / HTTP fallback) |
| AUD-25..27 | P2 Gain/volume | Implemented for mic gain + speaker volume; ONVIF exposes speaker `OutputLevel` only (no mic gain field in AudioSourceConfiguration) |
| AUD-28..31 | P2 ONVIF backchannel | Implemented, PCMA tone verified on the AV100 speaker (`crates/rtsp/src/backchannel.rs`, onvif AudioOutput/Decoder ops) |
