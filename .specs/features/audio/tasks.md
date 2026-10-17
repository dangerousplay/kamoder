# Audio Tasks

**Spec**: `spec.md` · **Design**: `design.md` · **Decisions**: `context.md`
**Status**: Implemented, host-tested; live deploy verification in progress (2026-10-17)

## Execution Plan

```
T1 (skeletons, workspace, xtask lists, supervisor capability)
 ├─[P]→ T2 audio-proto ──┬─→ T5 audio runtime+server+main ──┐
 ├─[P]→ T3 board [audio] ┤                                  │
 ├─[P]→ T4 anyka-av100::pcm + pcm-probe (T0 spike input) ──┘
 │                       ├─→ T6 rtsp audio track ──→ T12 rtsp backchannel (P2)
 │                       ├─→ T7 onvif-schema gen ──→ T8 onvif audio capture ──→ T13 onvif outputs (P2)
 │                       └─→ T9 web TLS+WS+audio API ──→ T10 webui card (devices, PTT, listen, gain)
T0 hardware spike (camera, user OK) — gates T4 constants/amp choice, final hw test
T5..T13 → T14 packaging, docs, traceability, ./x ci
```

## Task Breakdown

| ID | What | Where | Depends | Done when / Tests | Req |
|---|---|---|---|---|---|
| T0 | Hardware spike with pcm-probe: capture rate (8k/16k), read behaviour, playback, amp GPIO owner | camera, docs/notes | T4 | findings in HARDWARE-FINDINGS + context.md; D-A5/D-A6 decided | AUD-01,03 |
| T1 | Skeletons `crates/audio-proto`, `crates/audio`; workspace members/deps; xtask `BINARIES`, `HOST_TESTABLE`, `GENERIC_CRATES`, `HARDWARE_LITERALS`; supervisor `Capability::Audio` + tests | Cargo.toml, xtask, supervisor | — | `./x check` compiles; supervisor tests pass | — |
| T2 | audio-proto: types, socket handshake, frame consts, async Client (status/set/capture/play), g711 a/µ-law | crates/audio-proto | T1 | unit: serde shapes, g711 round-trip vs ITU table points, client↔fake server | AUD-02,04,08 |
| T3 | board `[audio]` (backend, rate, inputs/outputs, gain defaults) + validation; AV100 profile entry | crates/board, boards/ | T1 | board tests; `./x check-boards` | AUD-07 |
| T4 | `anyka-av100::pcm` ioctl ABI + `tools/re/pcm-probe` bin | crates/anyka-av100, tools/re | T1 | unit: struct size 28, ioctl numbers; probe builds for armv5te | AUD-01 |
| T5 | audio service: Backend trait, Fake, ak-pcm backend (feature), resampler, store, server, runtime (broadcast, talker lock, silence fill, amp tail, on-demand capture, backoff), metrics, main | crates/audio | T2,T3,T4 | tokio tests with FakeBackend: capture fan-out, busy, amp tail, error backoff; resampler unit | AUD-01..06,26 |
| T6 | rtsp: AudioSource seam, PCMA track in SDP, multi-track session/transports, RTP-Info, PCMPacketizer | crates/rtsp | T2,T3 | unit SDP; retina test: video-only unchanged, video+audio receives PCMA | AUD-10..13 |
| T7 | gen_onvif: add Media audio ops (source/encoder/output/decoder, options, add/remove/set) | xtask, crates/onvif-schema | T1 | `./x gen-onvif` + drift test | AUD-14..17,28 |
| T8 | onvif media audio source/encoder configs in profiles + handlers | crates/onvif | T2,T3,T7 | onvif tests: GetProfiles has audio, options, ConfigModify fault | AUD-14..17,27 |
| T9 | web: TLS listener + self-signed cert, ws feature, `/api/audio/devices` GET/PUT, `/api/audio/talk` + `/listen` WS | crates/web | T2,T3 | api_tests with fake audio socket; TLS cert gen test; WS round-trip test | AUD-08,09,18..20,23..25 |
| T10 | webui: api.ts, AudioCard (devices, gain sliders, Listen, hold-to-Talk, HTTP→HTTPS hint), worklets | webui | T9 | `./x webui` builds (tsc + lint) | AUD-08,19,21..25 |
| T12 | rtsp ONVIF backchannel: Require header, sendonly trackID=2, receive RTP (TCP interleaved + UDP), decode → audio play | crates/rtsp | T6 | test: client sends PCMA RTP → fake audio socket receives PCM; busy path | AUD-29,30 |
| T13 | onvif AudioOutputs/Output+Decoder configs, SetAudioOutputConfiguration → volume | crates/onvif | T8 | onvif tests | AUD-28,31 |
| T14 | supervisor.toml service + args, card/init.sh web TLS port, docs (`docs/notes`, site), spec traceability, `./x ci` | tools, card, docs, .specs | all | `./x ci` green | — |

## Outcome (2026-10-17)

T0–T14 implemented. Hardware findings (`docs/notes/HARDWARE-FINDINGS.md` §11): capture native 8 kHz (no resampling on AV100, profile `rate = 8000`, 512 B periods); amp GPIO 37 active low and the driver drives it inverted → `audio --amp lights` default, amp off re-asserted after the playback node closes. Deviations: 73-tap FIR resampler instead of half-band (half-band is −6 dB at 4 kHz); speaker taken lazily on first backchannel packet, released after 1 s idle; msf-rtp 0.4.2 hang on sequence jumps ≥ 64 guarded by `session::SequenceGuard` (report upstream); web binary +1.2 MiB for rustls/rcgen/ws.
