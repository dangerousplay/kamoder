# The Rust implementation

What exists, why each piece is shaped the way it is, and what is verified.

```
anyka-sys         generated FFI bindings (bindgen over sdk/include)
anyka-link        build-script helper: per-binary SDK library selection
anyka             safe wrappers — RAII handles, borrow-checked frame guards
os          kernel-only helpers (watchdog chardev, signals, libc shims)
telemetry   tracing + Prometheus, no OpenTelemetry (see below)
supervisor  event-driven process supervisor (kameo actors)
sshd        password-authenticated SSH server (russh + ring)
wifi        Wi-Fi reconciler with rollback to last-known-good
anyka-v8          safe wrappers for the AK3918AV100 (V8) SDK
demos/*           ports of the vendor C demos, plus md_demo_v8
xtask + ./x       build, lint, deny, dist, sd, qemu, deploy, ci

Every binary's command line is a clap derive struct: flags, defaults and
--help text live on the fields they fill. Flag spellings were kept exactly as
before (wdt_feeder -i/-m, jpeg_snapshot -w/-h/-p with --help only, the
positional demos) so supervisor.toml, init.sh and the anyka_happy scripts
did not change.
```

---

## Target and toolchain

`armv5te-unknown-linux-uclibceabi`, a **tier 3** target — no prebuilt `std`, so
every build compiles `std` from source via `-Z build-std` on nightly ≥ 1.100
plus `rust-src`.

It is the only workable target: the SDK objects link `libc.so.0`, so a glibc or
musl `std` would put a second, incompatible libc in the process.

**ARMv5TE has no 64-bit atomics** (`LDREXD` arrived in ARMv6K), so `std` does
not provide `AtomicU64`/`AtomicI64` at all. That is silicon, not a feature flag,
and it eliminated three candidate crates outright:

| Crate | Outcome |
| --- | --- |
| `opentelemetry_sdk` | does not compile — metric aggregation is built on `AtomicU64` |
| `ractor` | does not compile — `AtomicU64` in `actor_id.rs` |
| `bastion` | does not compile — same class |
| `kameo` | **compiles** — used for supervision |

`cargo-deny` carries an explicit ban on `opentelemetry_sdk` and `aws-lc-rs` so
neither returns through a transitive dependency.

---

## The safe wrappers (`anyka`)

The SDK hands out `void *` handles and expects every `open` paired with a
`close`, every `get_frame` with a `release_frame`, every `request_stream` with
a `cancel_stream`. Getting it wrong leaks DMA buffers or oopses the ISP driver.
Each pairing is a type:

```rust
let vi = VideoInput::open(VideoDevice::Dev0)?;   // ak_vi_close on drop
let encoder = Encoder::open(param)?;             // ak_venc_close on drop
let stream = encoder.request_stream(&vi)?;       // ak_venc_cancel_stream on drop
let chunk = stream.get_stream()?;                // ak_venc_release_stream on drop
writer.write_all(chunk.data())?;                 // &[u8] borrowed from the guard
```

`stream` borrows both `vi` and `encoder`, so neither can be closed while
encoding runs — in C that is a use-after-free surfacing as a kernel oops.
`chunk.data()` borrows the guard, so the slice cannot outlive the buffer.

Safety stops at the FFI boundary: every `ak_*` call is still `unsafe`, each with
a `// SAFETY:` note. What Rust buys is everything *around* those calls.

Two API decisions came directly from hardware:

* **`VideoInput::match_sensor_auto()`** searches `/etc/jffs2/`, `/usr/local/`,
  `/rom/`, `/ipc/local/` (override with `ANYKA_ISP_CONFIG`). The C demos
  hardcode `/etc/jffs2/`, which does not exist on every firmware — they die at
  the first SDK call with a message that does not say why.
* **`anyka::version::*` is one function per subsystem.** A single `versions()`
  returning all of them forced every binary to reference `ak_vi_*`, `ak_venc_*`
  and `ak_aenc_*`, which forced it to link the whole SDK — and that breaks the
  audio path (see HARDWARE-FINDINGS §2).

---

## Linking (`anyka-link`)

Three properties of the vendored objects make this harder than it looks:

1. **Circular references.** `libplat_vi` needs `ak_set_module_print` from
   `libplat_common`, which calls back into the platform libraries. No
   left-to-right `-l` ordering resolves them; they must sit inside one
   `--start-group`. Cargo cannot express that — `rustc-link-arg` preserves
   order but does not propagate to downstream binaries, `rustc-link-lib`
   propagates but loses order. Resolved with a generated **GNU ld script**
   linked as one ordinary `-l`.
2. **`-lm`, `-lpthread`, `-ldl` belong inside that group.** The SDK calls
   `floor` and the pthread API; if still unresolved when the linker reaches
   Rust's `compiler_builtins` it binds them to that rlib's *hidden* copies and
   fails with ``hidden symbol `floor' ... is referenced by DSO``.
3. **No SONAME, no run path**, and rustc's `--as-needed` drops objects the Rust
   code does not reference directly. `tools/ak-linker.sh` adds `-rpath-link`
   and brackets only the generated group with `--no-as-needed`, leaving rustc's
   own libraries as-needed — which matters, because the camera's uClibc merges
   libpthread/librt/libdl into libc and ships no separate files.

On top of that, **per-binary library sets**: each demo declares the modules it
uses and gets exactly the library list the vendor's own `build.sh` uses, from
the matching variant directory, plus an `$ORIGIN/../lib/<variant>` `RPATH`.

```rust
// demos/ai_demo/build.rs
anyka_link::emit(&[anyka_link::Module::AudioIn]);
```

---

## Supervisor (`supervisor`)

Replaces `service_supervisor.sh`, which woke every 10 s and shelled out to
`pidof`/`ps` per service — CPU on an idle camera, up to 10 s to notice a crash,
no backoff.

Two layers of recovery, easy to conflate:

* **the process died** — the service actor applies the restart policy
  (`always` / `on-failure` / `never`) with exponential backoff and a crash-loop
  guard (`max_restarts` inside `restart_window_secs`, then it gives up loudly);
* **the actor supervising it died** — the root rebuilds it via `on_link_died`,
  one-for-one, leaving other services alone.

Both `on_panic` and `on_link_died` return `ControlFlow::Continue`, so no single
service can take the tree down.

Nothing polls: child exits arrive through tokio's `SIGCHLD`-driven process
driver. Observed on hardware:

```
WARN service exited, restarting service="sshd" status=exit code 1 backoff_ms=500
WARN service exited, restarting service="sshd" status=exit code 1 backoff_ms=1000
...                                                               backoff_ms=16000
INFO service started service="sshd" pid=16117      # recovered unaided
```

Logs: `<log_dir>/<service>.log`, size-rotated, `log_keep` generations. Child
stdout/stderr *and* supervisor notes share one file so it tells the whole story;
timestamps are milliseconds since boot because the camera has no RTC. Writes go
through one dedicated thread on a **bounded** channel — SD writes block during
wear levelling, and doing that inside the runtime would stall supervision.

---

## SSH (`sshd`)

`russh` with the `ring` backend (`aws-lc-rs` has no ARMv5TE support).

- PBKDF2-HMAC-SHA256, per-user random salt, constant-time verify. An unknown
  username still runs a full derivation, so timing does not reveal which
  accounts exist.
- 20 000 iterations by default — far below the usual recommendation, because on
  a 400 MHz ARM926 with no crypto acceleration that already costs ~1 s per
  attempt. The salt defeats precomputation; raise with `--iterations`.
- ed25519 host key generated on first run at mode `0600`. Both the host key and
  the credentials file are **refused** if group- or world-accessible.
- PTY via `/dev/ptmx` directly: `forkpty()` lives in `libutil`, which this
  uClibc does not ship.
- Scope: password auth only, one session channel, no forwarding, no SFTP.

**It runs as whatever user starts it, which here is root, with no privilege
separation.** Treat it as remote root on a trusted network.

---

## Wi-Fi (`wifi`)

The camera is Wi-Fi only, configured by hand, with no UART attached — falling
off the network means someone has to go and power-cycle it. So Wi-Fi is not a
boot script but a supervised control loop whose one promise is that the camera
stays reachable.

Every 10 s it observes: the network in `/rom/wpa_supplicant0.conf`,
wpa_supplicant's `STATUS` (spoken directly over its control socket —
`wpa_cli` is not on every firmware), the IPv4 address on `wlan0`
(`SIOCGIFADDR`), and whether the default gateway resolves in ARP. Then
`reconcile::decide` — a pure function, time passed in — picks one action:

1. healthy and not yet recorded → **record** as last-known-good (on the SD
   card, so it survives reboots) *before* anything can move the camera off it;
2. configured ≠ `camera.conf`'s `wifi_ssid` → **apply**: back up, write
   atomically (remounting `/rom` rw only for the write), reassociate. This is
   what undoes a network something else configured;
3. unhealthy → 45 s grace, then **reassociate** with backoff 10 s → 5 min;
4. still down after 4 retries and a *different* last-good exists → **roll
   back** to it, and hold the desired network off for 30 min before retrying.

With `wifi_ssid` empty, step 2 never fires: it only keeps the manual
configuration associated, and restores it if `/ipc/ipc` or anyone else
replaces it with one that does not work.

The policy is covered by host unit tests, including a bad password on first
boot (the current network is recorded before the switch, so the rollback has
somewhere to go) and a config clobbered with no desired network set.
`wifi status` prints what it sees and the action it would take;
`--dry-run` logs decisions without acting. **Not yet run on the camera.**

---

## Telemetry (`telemetry`)

`tracing` for logs, the `metrics` facade for counters. No OpenTelemetry — see
the atomics note above.

```sh
ANYKA_LOG=debug
ANYKA_LOG_FILE=/mnt/disc1/anyka_rust/logs/x.log   # rotating, size-based
ANYKA_METRICS=textfile:/var/lib/node_exporter/textfile_collector
ANYKA_METRICS=http:0.0.0.0:9101
```

With `node_exporter` already on the camera the textfile exporter is the cheap
option: each service writes `<service>.prom` atomically (temp + rename) every
15 s. One file **per service**, because the supervisor passes its environment to
its children and a shared file made them race.

The `http` mode serves `/metrics` from a single blocking thread — no tokio, no
hyper.

---

## Release profile

`opt-level = 3`, fat LTO, and `panic = "unwind"` — the last is a correctness
choice, not a size one: every SDK resource is released by a `Drop` impl, and
`abort` would skip all of it and leave the ISP holding DMA buffers, which
usually needs a reboot to clear. No Rust code here is called from an SDK-owned
thread, so nothing unwinds across an FFI frame.

Binaries are ~7 MB on disk but only ~1.2 MB loadable; the rest is DWARF the
loader never maps. Strip before pushing over WiFi.

---

## Verification status

| | QEMU (ARMv5, uClibc-ng 1.0.31) | Real camera |
| --- | --- | --- |
| `wdt_feeder` | yes | **yes**, real `/dev/watchdog` |
| `sshd` | yes, full login | **yes**, full login |
| `supervisor` | yes | **yes**, backoff + restart observed |
| `init.sh` | yes | **yes** |
| `wifi` | **yes** — `./x itest`, real WPA2 over `mac80211_hwsim` | not yet |
| `md_demo_v8` | builds, links V8 objects only | not yet — camera offline |
| metrics (http / textfile) | yes | **yes** |
| ISP demos | not possible (no sensor) | **blocked** — firmware/SDK mismatch |
| audio demos | not possible (no ADC) | **blocked** — firmware/SDK mismatch |

The mismatch is now quantified: this camera's own stack runs
`libplat_isp_sdk V8.0.01` over a V4L2 subdev (`/dev/v4l-subdev0`), while every
SDK copy vendored here is `V3.1.00` and expects the older
`ak_info_dump.ko` interface. Supplying a matching SDK drop is the single change
that would unblock all six video/audio demos — no Rust in this workspace needs
to change, only `sdk/lib/`. See `HARDWARE-FINDINGS.md` §1, §5 and §5b.

### System tests (`./x itest`)

pytest + labgrid against the SD card booted on a **simulated camera**: the
camera's own rootfs on an ARM926 VM, with its SD card (`/dev/mmcblk0p1`,
FAT32), JFFS2 `/rom`, app partition, watchdog and Wi-Fi radio simulated — the
radio via `mac80211_hwsim`, talking WPA2 to two access points run by the
firmware's own `hostapd`, each in its own network namespace. 34 scenarios;
details and the full list in [`itest/README.md`](../itest/README.md).

What running on simulated devices found, all fixed:

| Found | Why only this could find it |
|---|---|
| **SSH never started on the real card**: FAT reports every file as mode 755, and `sshd` refused its credentials as world-readable | the card is FAT; earlier tests kept it in RAM |
| supervisor passed `ANYKA_LOG_FILE` to services — all wrote `supervisor.log` | first end-to-end boot |
| service logs full of terminal escape codes once they went to a pipe | first end-to-end boot |
| **DHCP livelock**: `udhcpc` started before association, backed off 20 s, and the reconciler's retry killed it just before it would succeed | real association timing |
| a last-good record that could not be saved (card read-only after a power cut) stalled the reconciler forever | a real, corruptible FAT image |
| every `hostapd`/`udhcpd` stopped by name — including other machines' | access points in other namespaces |
| **a network switched to by someone else was replaced before it was seen working**; with the desired AP down the camera then sat offline next to a working network | real association timing |

The rollback hold is also now scoped to the network that failed: correcting a
typo in `camera.conf` takes effect at once instead of after 30 minutes.

The VM kernel needed two board fixes for its SD card under QEMU
(`tools/qemu-armv5/patches/`) — the VM only, the camera's kernel is untouched.

`./x ci` — fmt, clippy `-D warnings`, cargo-deny (advisories, bans, licences,
sources), full cross build — is green.
