# Rust for the Anyka AK3918

**Kamoder**: open firmware for Anyka AK3918 IP cameras, written in Rust and run
from the SD card. It started as the Anyka SDK demos rewritten in Rust and grew
into a vendor-free stack: supervisor, watchdog, Wi-Fi with recovery, SSH, web
admin UI, ONVIF and RTSP.

📖 **Documentation:** the `website/` site, published to GitHub Pages (getting
started, supported cameras, architecture, verifying signed releases).
Everything cross-compiles to the camera's ARMv5TE CPU and links against the
vendored Anyka SDK through safe wrappers.

```sh
./x dist                 # build everything, stage dist/anyka_rust (full debug info)
./x sd                   # dist, strip, assemble the SD card tree in dist/sdcard
./x itest                # that card on a simulated camera: pytest + labgrid, 34 scenarios
./x ci                   # fmt --check, gen-onvif --check, clippy, cargo-deny, build
./x qemu wdt_feeder -h   # run a staged binary under qemu-arm-static
./x help                 # every task
```

Two longer documents alongside this one:

* [`docs/HARDWARE-FINDINGS.md`](docs/HARDWARE-FINDINGS.md) — what a live camera
  showed, including why the ISP and audio paths do not work on that unit and
  which failures are pre-existing rather than caused by this port.
* [`docs/IMPLEMENTATION.md`](docs/IMPLEMENTATION.md) — the design of each
  crate and the reasoning behind the awkward parts.
* [`docs/SDK-SOURCES.md`](docs/SDK-SOURCES.md) — the camera is an
  **AK3918AV100**, not the EV200 this repo was built around. Where the matching
  SDK is, proof that it matches, and what porting to it involves.

## Does this hardware support Rust?

Yes. The AK3918EV200 is an **ARM926EJ-S**, i.e. **ARMv5TE**, soft-float, running
**uClibc-ng 1.0.31**. Read straight off one of the vendored SDK objects:

```
Tag_CPU_name: "ARM926EJ-S"      Tag_CPU_arch: v5TEJ
NEEDED libc.so.0                (no Tag_ABI_VFP_args -> soft-float ABI)
```

LLVM has a maintained ARMv5TE backend, and rustc ships the
`armv5te-unknown-linux-uclibceabi` target. That target is the only workable
one here: the SDK objects link `libc.so.0`, so a glibc or musl `std` would put
a second, incompatible libc in the same process.

It is a **tier 3** target, so no prebuilt `std` exists and every build compiles
`std` from source. That is why the toolchain is nightly plus `rust-src`:

```sh
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
```

### What the CPU cannot do

**ARMv5TE has no 64-bit atomic instructions** (`LDREXD`/`STREXD` arrived in
ARMv6K), so `std` does not provide `AtomicU64` or `AtomicI64` at all for this
target. That is a property of the silicon, not a feature flag, and it rules out
some crates outright — see [Telemetry](#telemetry) below.

## Layout

| Crate | What it is |
| --- | --- |
| `anyka-sys` | Generated FFI bindings. `bindgen` runs over `sdk/include` at build time. |
| `anyka` | Safe wrappers: handles that close on drop, frame guards that release on drop. |
| `os` | Kernel-only helpers (watchdog character device, signals). Links no SDK library. |
| `telemetry` | `tracing` setup and a Prometheus recorder. |
| `onvif-schema` | ONVIF Device/Media/PTZ types generated from the vendored official WSDL/XSD (`./x gen-onvif`; output committed, `--check` in `./x ci`). |
| `demos/*` | The seven SDK demos, ported. |
| `rtsp` | RTSP 1.0 server: H.264 over TCP-interleaved or UDP, Digest auth with the ONVIF account. No vendor library. See [rtsp](#rtsp). |
| `sshd` | Password-authenticated SSH server. |
| `supervisor` | Event-driven process supervisor. Replaces the polling shell script. |
| `xtask` | Build, lint and packaging tasks. Runs on the host. |
| `sdk/` | Vendored Anyka SDK headers and shared objects. |
| `card/` | Files at the root of the SD card: `autorun.sh`, `Factory/config.sh`. |
| `tools/autostart/` | Optional AV100 flash patch so the stack starts at boot (with rollback). |
| `tools/qemu-armv5/` | Builds the VM for `./x itest` from your own camera's flash dump. |
| `website/` | Documentation site (Fumadocs). |

## The safe wrappers

The SDK hands out `void *` handles and expects every `open` to be paired with a
`close`, every `get_frame` with a `release_frame`, and every `request_stream`
with a `cancel_stream`. Getting that wrong leaks DMA buffers or oopses the ISP
driver on a device with 32 MB of RAM. Each pairing is a type here:

```rust
VideoInput::match_sensor("/etc/jffs2/")?;
let vi = VideoInput::open(VideoDevice::Dev0)?;   // ak_vi_close on drop
vi.capture_on()?;

let encoder = Encoder::open(param)?;             // ak_venc_close on drop
let stream = encoder.request_stream(&vi)?;       // ak_venc_cancel_stream on drop

let chunk = stream.get_stream()?;                // ak_venc_release_stream on drop
writer.write_all(chunk.data())?;                 // &[u8] borrowed from the guard
```

`stream` borrows both `vi` and `encoder`, so neither can be closed while
encoding is running — in C that is a use-after-free that surfaces as a kernel
oops. `chunk.data()` borrows the guard, so the slice cannot outlive the buffer.

Safety stops at the FFI boundary: every `ak_*` call is still `unsafe` inside a
wrapper, with a `// SAFETY:` note giving the reason it holds. What Rust buys is
everything *around* those calls — buffer handling, parsing, lifetimes, teardown
ordering — which is where most of the bugs in this class of firmware live.

### Are there existing Anyka crates?

No. crates.io has nothing for `anyka` or `ak3918`:

```console
$ curl -s 'https://crates.io/api/v1/crates?q=anyka'
{"crates":[],"meta":{"total":0,...}}
```

`anyka-sys` and `anyka` are the bindings.

## Porting notes

Behaviour is kept identical to the C originals, including the quirks. Where the
Rust differs, it is deliberate:

- **`rtsp_demo`** drives `/sys/user-gpio/*` with direct file writes instead of
  `system("echo %d > %s")`. Shelling out with an interpolated string is a
  command-injection shape, and `system()` forks the whole address space.
  GPIO polarity and ordering are preserved exactly.
- **`jpeg_snapshot`** replaces the `nweb`-derived HTTP server. The original
  indexed a 64 KB static buffer (a short request read before the start of it)
  and read the JPEG out of a struct the capture thread was writing to. This one
  reads a bounded request line, answers `GET` only, serves from a mutex-guarded
  buffer, and never touches the filesystem — so there is no path-traversal
  surface at all.
- **`venc_demo`** opens one JPEG encoder for all stills instead of one per
  picture, which the C version leaked on any early return.
- **`aenc_demo`** hides the intrusive `list_head` protocol: `drain()` walks the
  list, copies each payload out and releases every node, including on failure.
- Signals set an atomic instead of calling `exit()` from the handler, so the
  `Drop` impls actually run and the hardware is released.

## sshd

Password-authenticated SSH, as an alternative to dropping a prebuilt Dropbear
onto the SD card and running a C daemon as root on a LAN-reachable camera.

```sh
# once
sshd hash root > /etc/anyka-sshd.passwd
chmod 600 /etc/anyka-sshd.passwd

# then
sshd --port 2222 \
           --passwd /etc/anyka-sshd.passwd \
           --host-key /etc/anyka-sshd.key
```

- `russh` with the `ring` backend — `aws-lc-rs`, the default, has no ARMv5TE
  support.
- Passwords are PBKDF2-HMAC-SHA256 with a per-user random salt, verified in
  constant time. An unknown username runs a full derivation anyway, so response
  time does not reveal which accounts exist.
- The iteration count defaults to 20 000, well below the ~600 000 usually
  recommended. On a 400 MHz ARM926 with no crypto acceleration that already
  costs about a second per attempt. The salt is what defeats precomputation;
  raise it with `--iterations` if the camera has cycles to spare.
- An ed25519 host key is generated on first run with mode `0600`. Both the host
  key and the credentials file are **refused** if they are group- or
  world-accessible.
- PTY sessions go through `/dev/ptmx` directly, because `forkpty()` lives in
  `libutil`, which this uClibc does not ship.
- Scope: password auth only, one session channel per connection, no port
  forwarding, no SFTP, no agent forwarding. Anything unimplemented is refused
  rather than half-supported.

**It runs as whatever user starts it, which on this camera is root.** There is
no privilege separation. Treat it as remote root and put it on a network you
trust.

## rtsp

```sh
rtsp --port 554 --secret etc/onvif.secret --source file:clip.h264
```

Serves `rtsp://<camera>:554/vs0`, the URL `onvif` advertises, without any
vendor library. The protocol pieces are crates — `msf-rtsp` (framing,
interleaved and UDP channels, RTCP), `msf-rtp` (FU-A packetizer), `msf-sdp`
(SDP, H.264 `fmtp`), `ttpkit-auth` (Digest) — all pinned exactly; the crate
evaluation is in `.specs/features/pure-rust-onvif-rtsp/design.md`. What is
ours: the method handlers, the session table and its timeout, and the fan-out.

- One source task publishes access units on a bounded `broadcast` channel;
  each playing session packetizes on its own. A client that falls behind
  skips to the next IDR instead of growing a queue.
- SPS/PPS are cached from the stream for `sprop-parameter-sets` and put in
  front of every IDR that lacks them, so a client can join at any IDR.
- Digest is MD5 with `qop=auth` against `etc/onvif.secret` (the file `onvif`
  reads; the loader lives in `auth::onvif_secret`). `--no-auth` must be
  explicit.
- Sources sit behind the `VideoSource` trait. Today that is an Annex-B file
  played in a loop at `--fps`; the AV100 encoder (`/dev/venc`) is the Phase C
  implementation, which is why the supervisor service is still disabled.

`./x test -p rtsp` plays `rtsp/tests/data/clip.h264` to the `retina` client
over TCP and UDP and checks the received NAL units against the file.

## supervisor

Replaces `SD_card_contents/anyka_happy/service_supervisor.sh`, which woke every
10 seconds, shelled out to `pidof`/`ps` per service and restarted anything
missing. That burns CPU on an idle camera, takes up to 10 seconds to notice a
crash, and has no backoff — a service that dies instantly is restarted forever.

```sh
supervisor --config /mnt/anyka_rust/supervisor.toml
supervisor --config ... --check     # validate, list services, flag missing binaries
```

Supervision is built on **`kameo`**: one actor per supervised process, Erlang
style. `ractor` and `bastion` were both tried first and neither compiles for
ARMv5TE — they need `AtomicU64`.

Two layers of recovery, which are easy to conflate:

* the **process** died — the service actor applies the restart policy with
  exponential backoff and a crash-loop guard;
* the **actor** supervising that process died — the root rebuilds it via
  `on_link_died`, one-for-one, leaving the other services alone.

The root returns `ControlFlow::Continue` from both `on_panic` and
`on_link_died`, so no single service can take the tree down.

Nothing polls. Child exits arrive through tokio's `SIGCHLD`-driven process
driver, so an idle supervisor wakes up exactly never and a crash is noticed
immediately instead of up to 10 seconds later.

### Restart policy

`always` | `on-failure` | `never`, plus exponential backoff capped at
`backoff_max_ms`, plus a crash-loop guard: more than `max_restarts` inside
`restart_window_secs` and the service is abandoned with a loud log and a
metric. Without that guard a service that dies instantly would rewrite the SD
card forever.

### Logs

Each service gets `<log_dir>/<service>.log`, rotated at `log_max_bytes` and
kept for `log_keep` generations. Both the child's stdout/stderr *and* the
supervisor's own notes about it go into the same file, so one file tells the
whole story of that process:

```
[  27431980] sup started, pid 2183157
[  27432010] out flapper starting
[  27432010] sup exit code 3, restarting in 100 ms
[  27433520] sup restarted too often, giving up
```

Writes happen on one dedicated thread fed by a **bounded** channel: SD writes
block, sometimes for a long time during wear levelling, and doing that inside
the runtime would stall the supervision logic. Bounded means a service that
floods its output backpressures its own reader instead of growing a queue in
32 MB of RAM.

Timestamps are milliseconds since boot, not wall clock — the camera has no RTC
and reads 1970 until NTP lands.

Every Rust binary here also routes its own `tracing` output to a rotating file
when `ANYKA_LOG_FILE` is set, which `init.sh` does by default.

## Booting: init.sh

```sh
sh /mnt/anyka_rust/init.sh           # start in the background
sh /mnt/anyka_rust/init.sh --fg      # foreground, for the serial console
sh /mnt/anyka_rust/init.sh --check   # validate and exit
sh /mnt/anyka_rust/init.sh --stop    # stop the stack
```

POSIX `sh` only — the firmware has BusyBox, not bash. The design rule is that
**it must never be able to brick the camera**: every step either succeeds or is
skipped with a logged reason, and nothing touches the read-only rootfs or exits
in a way that stops `rcS` from finishing.

What it does, in order: mount the SD card if nothing else has; rotate its own
log; create the uClibc compatibility symlinks; export `LD_LIBRARY_PATH` and the
`ANYKA_LOG*` variables; run the supervisor's own `--check`; stop the vendor IPC
process (`stop_vendor_ipc=1` in `camera.conf`, the default); then run the
supervisor under a restart loop with linear backoff.

Three safety details worth knowing:

* **The watchdog feeder is the first service in the config.** Once the vendor
  IPC process is gone nothing else feeds the hardware watchdog and the board
  reboots in about eight seconds.
* **The vendor IPC is stopped only after preflight passes, and right before
  the supervisor starts.** `ipc` holds `/dev/watchdog` exclusively, so the
  feeder can only take over once it is gone. Set `stop_vendor_ipc=0` to leave
  it running.
* **If preflight fails, `init.sh` starts `wdt_feeder` standalone anyway.**
  That is the fallback path: a broken config must not leave the camera one
  `killall` away from a reboot loop.

## Telemetry

`./x` wires up `tracing` for logs and the `metrics` facade for counters.

```sh
ANYKA_LOG=debug                                  # tracing filter, default info
ANYKA_METRICS=http:0.0.0.0:9101                  # serve /metrics for Prometheus
ANYKA_METRICS=textfile:/var/lib/node_exporter/textfile_collector/kamoder.prom
```

Since `node_exporter` is running on the camera anyway, the **textfile**
exporter is the cheaper option: the process rewrites a `.prom` file every 15
seconds (through a temporary file and a rename, so a scrape never reads a
half-written file) and `node_exporter --collector.textfile.directory` picks it
up. The `http` mode serves `/metrics` from a single blocking thread — no tokio,
no hyper.

### OpenTelemetry does not work on this CPU

`opentelemetry_sdk` cannot be compiled for this target:

```
error[E0432]: unresolved imports `std::sync::atomic::AtomicI64`,
              `std::sync::atomic::AtomicU64`
  --> opentelemetry_sdk-0.31.0/src/metrics/internal/mod.rs:12:37
```

Its metric aggregation is built on 64-bit atomics, which ARMv5TE has no
instructions for. No feature flag changes that. `cargo-deny` has an explicit
ban on the crate so it cannot creep back in through a transitive dependency.

The working shape is the one already planned: the camera exposes Prometheus,
and a collector *off* the device converts to OTLP if OTLP is wanted. That also
keeps a tonic/hyper/prost stack off a 32 MB machine.

## Toolchain plumbing

Three things about this SDK need working around; all of them are handled, but
they are worth knowing before changing the build.

1. **Circular dependencies between SDK objects.** `libplat_vi` needs
   `ak_set_module_print` from `libplat_common`, which calls back into the
   platform libraries. No `-l` ordering resolves that — they have to be in one
   `--start-group`. Cargo cannot express it (`rustc-link-arg` keeps order but
   does not propagate to downstream binaries; `rustc-link-lib` propagates but
   loses order), so `anyka-sys/build.rs` generates a GNU **ld script** named
   `libanyka_sdk.so` that expands to a `GROUP (...)` of the whole SDK, and
   every binary links it as one ordinary `-lanyka_sdk`.
2. **No SONAME, no run path.** The objects reference each other by DT_NEEDED but
   cannot be found indirectly, and rustc's `--as-needed` drops the ones our Rust
   code does not call directly. `tools/ak-linker.sh` adds `-rpath-link` and
   brackets `-lanyka_sdk` with `--no-as-needed`, leaving rustc's own libraries
   as-needed — which matters, because the camera's uClibc merges
   libpthread/librt/libdl into libc and ships no separate files.
3. **uClibc version skew.** The crosstool is uClibc 0.9.33.2; the camera runs
   uClibc-ng 1.0.31. Two consequences:
   - `pthread_setname_np` does not exist in the crosstool's libc but Rust's
     thread startup references it unconditionally. `os::compat` provides
     a no-op shim.
   - The camera has no `libpthread.so.0`, `librt.so.0`, `libdl.so.0` or
     `libm.so.0`. `./x dist` ships empty stubs with those sonames in
     `lib/sys/`, next to the firmware's own `libgcc_s.so.1` and
     `libstdc++.so.6` (`sdk/runtime/README.md`). Every binary gets
     `$ORIGIN/../lib/sys` in its RPATH at link time (`tools/ak-linker.sh`),
     and every SDK object that needs more than libc gets
     `$ORIGIN:$ORIGIN/../sys` at staging time: the camera's ld.so searches
     only the *requesting* object's RPATH, so without it the vendor objects'
     dependencies would come from the firmware or not load. `./x dist` fails
     if any binary's library closure reaches outside the card for anything
     but `libc.so.0` and `ld-uClibc`. No `LD_LIBRARY_PATH`, nothing in `/tmp`.
   - Changing `tools/ak-linker.sh` does not make cargo relink: touch the
     binaries' `main.rs` (or `cargo clean`) afterwards.

`ak_ai_set_volume` is also declared by hand in `anyka-sys`: the header exposes
it only as a macro over `ak_ai_set_adc_volume`/`ak_ai_set_aslc_volume`, neither
of which the vendored `libplat_ai.so` actually exports.

## Running on the camera

`./x sd` produces the card in `dist/sdcard` (or `$ANYKA_SD_TREE`): copy
`autorun.sh`, `Factory/` and `anyka_rust/` to the card's root, fill in
`anyka_rust/camera.conf` (created from `camera.conf.example`, gitignored), and
boot. The card's own `anyka_rust/README.md` covers the boot chain, services,
the Wi-Fi reconciler and the logs.

By hand (the card is `/mnt/disc1` on the AV100; `/mnt` is a small tmpfs):

```sh
sh /mnt/disc1/anyka_rust/run-on-camera.sh venc_demo 200 8 4
sh /mnt/disc1/anyka_rust/run-on-camera.sh wifi status
```

`run-on-camera.sh` creates the uClibc compatibility symlinks and sets
`LD_LIBRARY_PATH` to the bundled SDK objects.

Most demos need the vendor `anyka_ipc` process stopped first — it holds the ISP
and the ADC, and neither can be opened twice. `init.sh` does this by default
(`stop_vendor_ipc=1`).

## Verification status

Built and exercised under `qemu-arm-static` against a uClibc-ng 1.0.31 ARMv5
rootfs (`tools/qemu-armv5`):

- `wdt_feeder` and `sshd hash` run.
- `sshd` completes a full login: key exchange, password auth, PTY
  allocation, command execution, exit status.
- Metrics scrape correctly from both the HTTP exporter and the `node_exporter`
  textfile exporter.
- `supervisor` was exercised against deliberately misbehaving services:
  `on-failure` correctly skipped a clean exit, `always` restarted with
  100/200/400 ms backoff capped at 400 ms, the crash-loop guard gave up after
  `max_restarts`, SIGTERM shut services down in reverse start order, and the
  log rotated at the configured size keeping exactly `log_keep` generations.
- `init.sh` was exercised with a stub supervisor: start, double-start guard,
  `--stop`, and the preflight-failure fallback that starts `wdt_feeder` alone.

### On real hardware

Tested on a live AK3918 (uClibc-ng 1.0.31, kernel 4.4.282, 32 MB RAM) over
telnet, binaries pushed to the SD card with `nc`:

| | |
| --- | --- |
| All 21 SDK objects load | yes |
| `wdt_feeder` on the real `/dev/watchdog` | **yes** — holds it exclusively; the old `feeder.sh` then fails with `Device or resource busy` |
| `sshd` | **yes** — real login from another machine: key exchange, PBKDF2 auth, PTY on `/dev/pts/0`, `uname -m` → `armv5tejl` |
| `supervisor` | **yes** — started both services, backed off 500→1000→2000→4000→8000→16000 ms while a port was squatted, recovered on its own, and restarted `wdt_feeder` 3 s after a `kill -9` |
| `init.sh` | **yes** — start, `--stop`, restart loop |
| Prometheus textfile metrics | **yes** — `kamoder_supervisor_service_up{service="sshd"} 1`, `kamoder_watchdog_feeds_total` climbing |
| Board stability | no reboot across the whole session (uptime 3 h 38 m) |

**The ISP and audio paths do not work on this particular camera, and it is not
the port's fault.** Both were verified against the vendor's own prebuilt C
demos on the same unit:

* **ISP.** Every `isp_*.conf` on the device is rejected by the vendored
  `libakispsdk.so` with `config file ... is old version` /
  `cannot get sensor id`. The C `md_demo` given the same configs fails
  identically, exit 255. This firmware is a different Anyka SDK generation
  than the objects vendored in `cross-compile/`.
* **Audio.** `ak_ai_open` wants `/dev/akpcm_cdev1`; this firmware exposes
  ALSA-style `pcmC0D0{c,l,p}` instead. Same mismatch.

Fixing either means sourcing SDK objects that match this firmware, not
changing Rust.

### Three bugs the hardware found

Emulation could not have caught these.

1. **A `pthread_mutex_init` segfault in `ak_ai_open`.** The core pointed at
   `libplat_thread.so`, and `cross-compile/` turned out to ship **two
   incompatible builds of it** (`a6d3fbf5` for venc/jpeg/libre, `f94db11c` for
   ai/aenc/md/rtsp). They are not interchangeable in either direction — the
   second does not even export the `ak_thread_cond_*` symbols
   `libplat_venc_cb.so` needs. `sdk/lib` is now split per variant and each
   binary links, and `RPATH`s, its own. With the right variant the audio path
   fails cleanly instead of crashing.
2. **BusyBox on this firmware has no `dirname`, `pidof` or `pkill`.**
   `run-on-camera.sh` and `init.sh` used the first two; they now use parameter
   expansion and a `/proc` scan.
3. **The metrics textfile raced.** The supervisor passes its environment to
   the processes it starts, so parent and children all wrote the same `.prom`
   file and collided on the rename (`No such file or directory`).
   `ANYKA_METRICS=textfile:` now names a **directory** and each service writes
   `<service>.prom`, which is also what `node_exporter` wants.

`venc_demo` and `jpeg_snapshot` still cannot be exercised anywhere: no ISP on
this unit, and no encoder under QEMU.

## Release profile

Binaries live on the SD card, so file size is not a constraint. The 400 MHz
ARM926EJ-S is, so the profile optimises for speed and for being debuggable on a
device you cannot easily attach a debugger to:

| Setting | Why |
| --- | --- |
| `opt-level = 3` | The encode and capture loops are the bottleneck, not flash. |
| `lto = true`, `codegen-units = 1` | Cross-crate inlining through the wrappers. |
| `panic = "unwind"` | So `Drop` runs — see below. |
| `strip = false`, `debug = "line-tables-only"` | Panics report a file and line. |

**`panic = "unwind"` is a correctness choice, not a size one.** Every SDK
resource here is released by a `Drop` impl: frames back to the ISP, streams
cancelled, encoders and the video input closed. `panic = "abort"` skips all of
it and leaves the ISP holding DMA buffers, which on this hardware usually needs
a reboot to clear. Unwinding runs the teardown, and no Rust code here is ever
called from an SDK-owned thread, so nothing unwinds across an FFI frame.

### Sizes, and what they cost at runtime

```
                 file     loadable                     file     loadable
wdt_feeder      6.8 MB     1.21 MB    md_demo         6.7 MB     1.19 MB
ai_demo         6.8 MB     1.21 MB    rtsp_demo       6.8 MB     1.21 MB
aenc_demo       6.8 MB     1.21 MB    jpeg_snapshot   6.8 MB     1.22 MB
venc_demo       6.8 MB     1.22 MB    sshd     15.4 MB     3.16 MB
```

"loadable" is the sum of the `PT_LOAD` segments. The rest is DWARF, which the
loader never maps.

Whether `opt-level = "z"` would save RAM is worth being precise about, because
the answer is *"a little, and not the part that matters"*. Measured by building
the whole workspace both ways:

| | `opt-level = 3` | `opt-level = "z"` | change |
| --- | --- | --- | --- |
| `.text` (demos) | 1 093 KB | 709 KB | **-35 %** |
| `.text` (`sshd`) | 2 751 KB | 1 511 KB | **-45 %** |
| `.data` + `.bss` (demos) | 6 512 B | 6 520 B | +8 B |
| `.data` + `.bss` (`sshd`) | 210 880 B | 210 904 B | +24 B |

`.text` is file-backed, read-only and shared: pages arrive on demand, only the
code actually executed becomes resident, and the kernel can evict all of it
under pressure. So `"z"` lowers the *ceiling* on text residency by about a
third, not the steady-state cost.

`.data`/`.bss` — the private, dirty, never-reclaimable pages — are unchanged to
within a couple of dozen bytes. Heap and stack are unaffected too: the
allocations these programs make are frame buffers and `Vec`s whose sizes come
from the resolution and the protocol, not from how the code was optimised.

So on a 32 MB device the realistic saving from `"z"` is some reclaimable page
cache, in exchange for slower code on a 400 MHz CPU that is already the
bottleneck. It is not the right trade here. Where `"z"` *would* pay off is cold
start: ~380 KB less to read off the SD card on first touch.

### Minimum toolchain

Nightly **1.100** or newer. Earlier nightlies (tested on 1.99) ICE in the
backend when building tokio for `armv5te` at `opt-level` 2 or 3:

```
internal compiler error: compiler/rustc_codegen_ssa/src/mir/operand.rs:291:18:
not immediate: OperandRef(Uninit @ ... UnsafeCell<MaybeUninit<
    runtime::task::Notified<Arc<multi_thread::handle::Handle>>>> ...)
```

That is fixed as of 1.100, so no per-package `opt-level` workaround is needed.
If you are pinned to an older nightly and hit it, pin tokio alone to `"s"`.

## Credits

Kamoder builds on the work of others. Thank you:

- [Gerge — Anyka AK3918 hacking journey](https://gitea.raspiweb.com/Gerge/Anyka_ak3918_hacking_journey):
  the SD-card hack, the `Factory/config.sh` boot hook on the EV200 and much of
  the hardware knowledge this started from.
- [helloworld-spec/qiwen](https://github.com/helloworld-spec/qiwen): Anyka SDK
  and the cross toolchain.
- [ricardojlrufino/anyka_v380ipcam_experiments](https://github.com/ricardojlrufino/anyka_v380ipcam_experiments),
  [kuhnchris/IOT-ANYKA-PTZdaemon](https://github.com/kuhnchris/IOT-ANYKA-PTZdaemon),
  [MuhammedKalkan/Anyka-Camera-Firmware](https://github.com/MuhammedKalkan/Anyka-Camera-Firmware)
  and the [E27 camera hack discussion](https://github.com/e27-camera-hack/E27-Camera-Hack/discussions/1).

## License

GPL-3.0-or-later (see `LICENSE`). Vendored third-party code under `sdk/` and
`third_party/` keeps its own terms.
