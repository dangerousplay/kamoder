# Conventions — Kamoder

## Naming

- Project name **Kamoder**. Generic crates are unprefixed (`os`, `telemetry`, `auth`, `supervisor`, `wifi`, `lights`, `web`, `onvif`, `rtsp`, `camera`); `anyka-*` only for SDK/hardware-binding crates (`anyka-sys`, `anyka-link`, `anyka`, `anyka-v8`, `anyka-ptz`).
- Workspace manifest: every crate uses `version/edition/license.workspace`-style shared fields (`[workspace.package]` edition 2021, GPL-3.0-or-later) and `dep.workspace = true` for shared deps (`Cargo.toml`).
- Legacy names remain: socket `/tmp/anyka-supervisor.sock`, `/tmp/anyka-ptz.sock`, env vars `ANYKA_*`, files `etc/anyka-sshd.*`, staging dir `anyka_rust/`. Newer pieces use `kamoder-` (`/tmp/kamoder-lights.sock`, `/tmp/kamoder-video.sock`). Mixed by history, not by rule.

## Error handling

- Libraries: `snafu` enums per module (`#[derive(Debug, Snafu)] pub enum Error { #[snafu(display(...))] Variant { path, source } }`), `.context(...)`/`ResultExt`, local `type Result<T, E = Error>` (e.g. `crates/camera/src/venc.rs`, `crates/rtsp/src/source.rs`, `crates/telemetry/src/lib.rs`).
- Binaries: `eyre::Result` at `main`; library errors convert with `?`/`.into()` (`demos/wdt_feeder/src/main.rs`). xtask also uses `eyre` (`bail!`, `wrap_err_with`).
- Panics: `panic = "unwind"` so `Drop` releases hardware; supervisor actors return `ControlFlow::Continue` from `on_panic`/`on_link_died`.
- Web API errors: `{error, code}` JSON via `crates/web/src/error.rs`. Socket protocols: `{"result":"error","code":"...","message":"..."}`.
- Unix exit status is meaningful: SIGTERM/SIGINT -> exit 0 ("asked to stop"); anything else -> non-zero so the supervisor restarts (`crates/onvif/src/main.rs` doc).

## Logging and metrics

- `tracing` macros only, no `println!` in services; structured fields (`service="sshd" backoff_ms=...`). Initialized via the `telemetry` crate (`ANYKA_LOG` env filter, optional rotated `ANYKA_LOG_FILE`).
- Metrics through the `metrics` facade; each binary has a `describe_metrics()` calling `describe_counter!/describe_gauge!`.
- Metric names: `kamoder_<service>_<thing>[_total]` — counters end in `_total`, gauges do not (e.g. `kamoder_supervisor_service_up`, `kamoder_rtsp_frames_sent_total`, `kamoder_wifi_escalation_level`). No `anyka_` prefix (the only `"anyka_sdk"` string is the `links =` key / label, not a metric).
- Exporter: `ANYKA_METRICS=textfile:<dir>` writes `<service>.prom` per service; `http:` must not be set globally (children inherit env).

## CLI

- Every binary uses `clap` derive (`#[derive(Parser)]`), doc comments on fields become `--help`; defaults on the field (`default_value`, `default_value_t`, `env = "ANYKA_..."`). Legacy flag spellings kept for script compatibility (`wdt_feeder -i/-m/-w`, positional demo args).
- Subcommands where a binary has modes (`camera serve|probe`, `web ... serve`, `wifi status`, `supervisor --check`).
- CLI parse tests assert defaults (e.g. `crates/lights/src/main.rs`, `crates/onvif/src/main.rs` tests check socket paths).

## Module and code layout

- Binary crates split into `lib.rs` (testable logic, shared protocol) + `main.rs` (CLI, runtime, signals, listener).
- Pure decision functions separated from I/O for host testing (`crates/wifi/src/reconcile.rs` `decide`, `crates/anyka-ptz/src/preset_motor/machine.rs`).
- Hardware access behind a trait or an injectable root path (`MotorDevice`, `PtzBackend`, `VideoSource`, GPIO `--gpio-root`, supervisor `hardware.rs` "paths relative to `root`").
- Protocol types live in a separate dependency-light crate shared by server and client (`ptz-proto`, `lights-proto`, `supervisor` lib `control`).
- Async: tokio current-thread runtime built explicitly in `main` (`Builder::new_current_thread()`), never `#[tokio::main]` multi-thread.
- XML: never hand-formatted; ONVIF bodies are generated serde types (`onvif-schema`), regenerated with `./x gen-onvif`, never hand-edited.
- Prefer own Rust over vendor `.so` (camera, lights, wifi, rtsp are vendor-free); vendor SDK only in `anyka*` crates and demos.

## Comment style

- Every module starts with a `//!` doc that explains *why* (constraints, hardware evidence, rejected alternatives), often with `text` blocks showing wire formats or console output. See `crates/ptz-proto/src/lib.rs`, `crates/anyka-link/src/lib.rs`, `crates/os/src/lib.rs`.
- Every `unsafe` block carries a `// SAFETY:` note (counts per file: e.g. `crates/anyka/src/vi.rs` 18, `crates/camera/src/device.rs` 8).
- Constants carry doc comments with rationale and units (`/// Frames are copied out while a request came in this recently.`).
- Config files (`tools/supervisor.toml`, `tools/camera.conf.example`, `.cargo/config.toml`, `deny.toml`) are heavily commented with reasons.
- Spec traceability tags appear in code: `PTZ-04`, `ONVIF-05`, design decision `D4`, `SPEC_DEVIATION (...)`.

## Lints and formatting

- No `xtask lint` task exists; linting is `./x clippy` = `cargo clippy --workspace --all-targets -- -D warnings` (`xtask/src/tasks.rs`).
- No `rustfmt.toml`/`clippy.toml`; default rustfmt via `./x fmt` (workspace + xtask).
- Crate-level `[lints.rust]` only in `crates/anyka-sys/Cargo.toml` (bindgen naming). Local `#[allow]`s are rare and justified, sometimes with `reason = "..."` (`crates/anyka-ptz/src/lib.rs`), `clashing_extern_declarations` allowed in `anyka_sys::v8`, `clippy::all` allowed on generated `onvif-schema` code.
- `cargo-deny` (`deny.toml`): target-only graph, advisories with no ignore list, yanked=deny, license allowlist (MIT/Apache/BSD/ISC/Zlib/Unicode-3.0/GPL-3.0-or-later), wildcards denied, banned crates, crates.io only.
- Exact `=` pins for crates whose output is a wire contract (msf-*, onvif-device-rs, xsd-parser*).

## Commits / releases

- Conventional Commits expected (git-cliff groups by type, `cliff.toml`). Tags `v*` -> release; `release/**` branches -> rc prereleases.

## Shell scripts

- POSIX sh for anything on the camera (BusyBox: no `dirname`, `pidof`, `pkill`, `head`, `tail`); use `${0%/*}` and `/proc/[0-9]*/comm` scans (`tools/init.sh`, HARDWARE-FINDINGS §3).
