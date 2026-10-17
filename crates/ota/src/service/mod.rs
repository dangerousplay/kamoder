//! The `update` service (design §6; OTA-11, OTA-14..19): checks the release
//! channel, downloads and verifies packages, installs them into the
//! inactive slot, rolls back on request, and commits a trial boot once it is
//! healthy. Served over [`update_proto`] by [`server`]; driven by the `update`
//! binary (`src/main.rs`).
//!
//! ```text
//!             ┌──────────── scheduler (every tick) ────────────┐
//!             │ auto_check: 2 min after start, then 12 h ± 10 % │
//!             │ auto_install: downloaded + window → install     │
//!             └───────────────┬────────────────────────────────┘
//! socket ──► Service::handle  ▼
//!   check     discover (stable: latest/download, beta: API list)
//!             → verify_and_check(keys, policy) → available    → download job
//!   download  fetch → update/kamoder-<v>-update.kup → Kup: signature + every
//!             component digest → downloaded (older packages deleted)
//!   install   Kup::open + verify_and_check(force) → reply ok → spawn_blocking:
//!             install_to_inactive(SdSelector, SdDirStore) → highest_seq/floor
//!             → AwaitingReboot → sync + reboot
//!   rollback  SdSelector::rollback (other slot bootable + committed) → reboot
//! health loop (only on a trial boot): supervisor + web + own manifest
//!             healthy for 180 s → mark_successful(booted)          (OTA-11)
//! ```
//!
//! **One job at a time.** Check, download and install take an async mutex
//! (`try_lock`, so a second one is refused as `busy` instead of queuing);
//! everything blocking (package reads, hashing, slot writes) runs in
//! `spawn_blocking`, so the socket stays responsive on the current-thread
//! runtime. Background failures are kept as `last_error` and phase `error`.
//!
//! **No key built in** ([`TrustedKeys::builtin`] empty until the maintainer
//! adds one): the service runs, `trusted_key` is false, checks and installs
//! fail with `no_trusted_key`, and the health check skips manifest
//! verification so a trial boot still commits.
//!
//! **Installs during a trial are refused** (`trial`): the inactive slot is
//! then the previous, known-good one, the only way back.

pub mod health;
pub mod schedule;
pub mod server;
pub mod source;
pub mod state;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bootctl::{BootControl, Record, SdSelector, Slot};
use metrics::{counter, gauge};
use ota_manifest::{
    compare_versions, ComponentKind, Manifest, Policy, Rejection, TrustedKeys, Version,
};
use tokio::sync::{Notify, OwnedMutexGuard};
use update_proto::{
    code, Available, ErrorInfo, HealthInfo, Phase, Progress, Request, Response, Settings, SlotInfo,
    Status,
};

use crate::kup::{self, Kup, VerifiedManifest, MANIFEST_NAME, SIGNATURE_NAME};
use crate::layout::{Layout, SlotKind};
use crate::sd::{SdDirStore, VERSION_FILE};
use crate::store::{self, install_to_inactive, SlotStore};

use health::{HealthConfig, Probe, Tracker};
use schedule::{auto_install_due, jittered, AutoInstall, ScheduleConfig};
use source::{kup_name, Source, SourceError};
use state::{Saved, SavedAvailable, STATE_FILE};

/// The target triple this updater accepts packages for.
pub const TARGET: &str = "armv5te-unknown-linux-uclibceabi";

/// Pause between answering a request that reboots and the reboot, so the
/// answer (and a last status poll) reaches the web UI.
pub const REBOOT_DELAY: Duration = Duration::from_secs(2);

/// Reboots the device (or pretends to, in tests and `--no-reboot`).
pub type RebootFn = Arc<dyn Fn() + Send + Sync>;

/// Unix seconds now (injected so tests control clock validity).
pub type ClockFn = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Everything the service is configured with.
pub struct Config {
    /// Card layout (data root).
    pub layout: Layout,
    /// The running slot's directory (`--slot-dir`).
    pub slot_dir: PathBuf,
    /// This device's board profile name (policy: `boards`).
    pub board: String,
    /// Target triple (policy: `target`), normally [`TARGET`].
    pub target: String,
    /// Keys packages must be signed with.
    pub keys: TrustedKeys,
    /// Version compiled in; used when the slot has no `VERSION` file.
    pub compiled_version: Version,
    /// The supervisor's control socket (health check).
    pub supervisor_socket: PathBuf,
    /// The web server probed by the health check.
    pub web_addr: SocketAddr,
    pub health: HealthConfig,
    pub schedule: ScheduleConfig,
    /// See [`REBOOT_DELAY`].
    pub reboot_delay: Duration,
    pub reboot: RebootFn,
    pub clock: ClockFn,
    /// Repository to use while no settings were ever saved (`UPDATE_REPO`
    /// from `camera.conf`).
    pub seed_repo: Option<String>,
}

/// A request that could not be carried out: wire code ([`code`] or a
/// manifest rejection code) and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    pub code: String,
    pub message: String,
}

impl OpError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl From<SourceError> for OpError {
    fn from(e: SourceError) -> Self {
        OpError::new(e.code, e.message)
    }
}

/// Mutable state behind one lock (never held across an await).
struct Inner {
    saved: Saved,
    booted: Slot,
    version: Version,
    /// Phase of the running job, `None` when idle.
    job_phase: Option<Phase>,
    progress: Option<Progress>,
    /// The last job failed.
    failed: bool,
    /// A slot switch is pending until the next boot.
    awaiting_reboot: bool,
    health: Option<HealthInfo>,
    /// Seq of the package auto-install already tried.
    auto_attempted_seq: Option<u64>,
}

/// The update service.
pub struct Service<S> {
    config: Config,
    source: S,
    state_path: PathBuf,
    inner: Mutex<Inner>,
    job: Arc<tokio::sync::Mutex<()>>,
    /// Wakes the scheduler after a settings change.
    wake: Notify,
}

impl<S: Source> Service<S> {
    /// Loads the state, the boot record and the running version. Fails only
    /// if `update/` cannot be created.
    pub fn new(config: Config, source: S) -> std::io::Result<Arc<Self>> {
        let update_dir = config.layout.update_dir();
        std::fs::create_dir_all(&update_dir)?;
        let state_path = update_dir.join(STATE_FILE);
        let mut changed = false;
        let mut saved = state::load(&state_path).unwrap_or_else(|| {
            let mut saved = Saved::default();
            if let Some(repo) = config.seed_repo.as_ref().filter(|r| valid_repo(r)) {
                saved.settings.repo = repo.clone();
            }
            saved
        });

        let record = load_record(&config.layout);
        let booted = booted_slot(&record, &config.layout, &config.slot_dir);
        let version =
            read_version(&config.slot_dir).unwrap_or_else(|| config.compiled_version.clone());

        // The running slot's own manifest is the floor of the anti-replay
        // memory, whatever happened to state.json.
        if let Some(own) = own_manifest(&config.slot_dir, &config.keys) {
            if own.seq > saved.highest_seq {
                saved.highest_seq = own.seq;
                changed = true;
            }
            if raise_floor(&mut saved.security_floor, &own.security_floor) {
                changed = true;
            }
        }
        // `./x flash` deletes packages but keeps state.json.
        if let Some(name) = &saved.downloaded {
            if !update_dir.join(name).is_file() {
                tracing::info!(file = %name, "downloaded package is gone");
                saved.downloaded = None;
                changed = true;
            }
        }
        if changed {
            if let Err(error) = state::save(&state_path, &saved) {
                tracing::warn!(path = %state_path.display(), %error, "cannot save the update state");
            }
        }

        let trial = !record.slot(booted).successful;
        gauge!("kamoder_update_trial").set(f64::from(u8::from(trial)));
        gauge!("kamoder_update_available").set(f64::from(u8::from(saved.available.is_some())));
        tracing::info!(
            slot = %booted,
            %version,
            trial,
            board = %config.board,
            trusted_key = !config.keys.is_empty(),
            highest_seq = saved.highest_seq,
            "update service ready"
        );
        if config.keys.is_empty() {
            tracing::warn!(
                "no update key built in: every package will be refused (no_trusted_key)"
            );
        }
        Ok(Arc::new(Self {
            config,
            source,
            state_path,
            inner: Mutex::new(Inner {
                saved,
                booted,
                version,
                job_phase: None,
                progress: None,
                failed: false,
                awaiting_reboot: false,
                health: None,
                auto_attempted_seq: None,
            }),
            job: Arc::new(tokio::sync::Mutex::new(())),
            wake: Notify::new(),
        }))
    }

    /// Runs the background work: the health loop on a trial boot, and the
    /// scheduler. Never returns.
    pub async fn run(self: Arc<Self>) {
        if self.is_trial() {
            tokio::spawn(Arc::clone(&self).health_loop());
        }
        self.scheduler().await;
    }

    /// Carries out one request.
    pub async fn handle(self: &Arc<Self>, request: Request) -> Response {
        let op = request.op();
        let result = match request {
            Request::Status => Ok(Some(self.status())),
            Request::Check => self.check().await.map(|()| Some(self.status())),
            Request::Download => self.start_download().map(|()| None),
            Request::Install {
                source,
                force,
                reboot,
            } => self.install(source, force, reboot).await.map(|()| None),
            Request::Rollback { reboot } => self.rollback(reboot).map(|()| None),
            Request::Reboot => {
                tracing::info!("reboot requested");
                self.spawn_reboot();
                Ok(None)
            }
            Request::SetSettings(settings) => self.set_settings(settings).map(|()| None),
            Request::Commit => self.commit().map(|()| None),
        };
        let (response, result) = match result {
            Ok(Some(status)) => (Response::Status(Box::new(status)), "ok"),
            Ok(None) => (Response::Ok, "ok"),
            Err(e) => {
                if op != "status" {
                    tracing::info!(op, code = %e.code, message = %e.message, "request refused");
                }
                (
                    Response::Error {
                        code: e.code,
                        message: e.message,
                    },
                    "error",
                )
            }
        };
        counter!("kamoder_update_socket_requests_total", "op" => op, "result" => result)
            .increment(1);
        response
    }

    // ----- status --------------------------------------------------------

    /// What `status` answers.
    pub fn status(&self) -> Status {
        let record = load_record(&self.config.layout);
        let mut inner = self.lock();
        let committed = record.slot(inner.booted).successful;
        let downloaded = self.downloaded_file(&mut inner).is_some();
        let phase = if let Some(phase) = inner.job_phase {
            phase
        } else if inner.awaiting_reboot {
            Phase::AwaitingReboot
        } else if !committed {
            Phase::Trial
        } else if inner.failed {
            Phase::Error
        } else {
            Phase::Idle
        };
        let saved = &inner.saved;
        Status {
            version: inner.version.to_string(),
            slot: wire_slot(inner.booted),
            trial: !committed,
            committed,
            slots: [
                self.slot_info(&record, Slot::A),
                self.slot_info(&record, Slot::B),
            ],
            phase,
            progress: inner.progress.clone(),
            available: saved.available.as_ref().map(|a| Available {
                version: a.version.clone(),
                channel: a.channel,
                seq: a.seq,
                notes_url: a.notes_url.clone(),
                size: a.size,
                downloaded,
                downgrade: a.downgrade,
            }),
            last_check: saved.last_check,
            last_error: saved.last_error.clone(),
            settings: saved.settings.clone(),
            trusted_key: !self.config.keys.is_empty(),
            health: if committed {
                None
            } else {
                inner.health.clone()
            },
        }
    }

    fn slot_info(&self, record: &Record, slot: Slot) -> SlotInfo {
        let layout = &self.config.layout;
        let state = record.slot(slot);
        let dir_present = match layout.slot_kind(slot) {
            SlotKind::Legacy => layout.data_root().join("bin").is_dir(),
            SlotKind::Dir => layout.install_dir(slot).is_dir(),
        };
        // The slot's own VERSION file first: the record says "legacy" for a
        // tree that was never installed by the updater.
        let version = dir_present
            .then(|| read_version(&layout.slot_dir(slot)))
            .flatten()
            .map(|v| v.to_string())
            .or_else(|| Some(state.version.as_str().trim().to_owned()).filter(|v| !v.is_empty()));
        SlotInfo {
            slot: wire_slot(slot),
            version,
            bootable: state.bootable,
            successful: state.successful,
            tries_left: state.tries_left,
            dir_present,
        }
    }

    /// The running slot is not committed yet.
    pub fn is_trial(&self) -> bool {
        let booted = self.lock().booted;
        !load_record(&self.config.layout).slot(booted).successful
    }

    // ----- check ---------------------------------------------------------

    /// Checks the channel now (OTA-14); starts the download when a new
    /// update was found and is not downloaded yet.
    pub async fn check(self: &Arc<Self>) -> Result<(), OpError> {
        let guard = self.try_job()?;
        self.begin_job(Phase::Checking);
        let result = self.check_inner().await;
        self.end_job();
        drop(guard);
        match result {
            Ok(outcome) => {
                counter!("kamoder_update_checks_total", "result" => outcome.label()).increment(1);
                if outcome == CheckOutcome::NeedsDownload {
                    if let Err(e) = self.start_download() {
                        tracing::warn!(error = %e, "cannot start the download");
                    }
                }
                Ok(())
            }
            Err(e) => {
                counter!("kamoder_update_checks_total", "result" => "error").increment(1);
                self.record_error(&e);
                Err(e)
            }
        }
    }

    async fn check_inner(self: &Arc<Self>) -> Result<CheckOutcome, OpError> {
        let now = self.valid_now().ok_or_else(clock_not_set)?;
        if self.config.keys.is_empty() {
            return Err(no_trusted_key());
        }
        let (repo, channel) = {
            let inner = self.lock();
            (
                inner.saved.settings.repo.clone(),
                inner.saved.settings.channel,
            )
        };
        tracing::info!(%repo, channel = channel.as_str(), "checking for updates");
        let found = source::discover(&self.source, &repo, channel).await?;
        let policy = self.policy(false);
        let available = match ota_manifest::verify_and_check(
            &found.manifest,
            &found.signature,
            &self.config.keys,
            &policy,
        ) {
            Ok(m) => {
                let version = m.version.to_string();
                let (kup_url, size) = found.release.kup(&version)?;
                tracing::info!(%version, seq = m.seq, "update available");
                Some(SavedAvailable {
                    version,
                    channel: wire_channel(m.channel),
                    seq: m.seq,
                    notes_url: m.notes_url.clone(),
                    size: size.or_else(|| {
                        Some(estimate_kup_size(
                            &m,
                            found.manifest.len(),
                            found.signature.len(),
                        ))
                    }),
                    kup_url,
                    downgrade: policy.is_downgrade(&m),
                })
            }
            Err(Rejection::Replay { seq, highest }) => {
                tracing::info!(seq, highest, "up to date");
                None
            }
            Err(r) => {
                return Err(OpError::new(
                    r.code(),
                    format!("the release manifest was refused: {r}"),
                ))
            }
        };

        let mut inner = self.lock();
        inner.saved.last_check = Some(now);
        let old_seq = inner.saved.available.as_ref().map(|a| a.seq);
        if old_seq != available.as_ref().map(|a| a.seq) {
            if let Some(name) = inner.saved.downloaded.take() {
                remove_file_logged(&self.config.layout.update_dir().join(name));
            }
        }
        let outcome = match &available {
            None => CheckOutcome::UpToDate,
            Some(_) if self.downloaded_file(&mut inner).is_some() => CheckOutcome::Downloaded,
            Some(_) => CheckOutcome::NeedsDownload,
        };
        gauge!("kamoder_update_available").set(f64::from(u8::from(available.is_some())));
        inner.saved.available = available;
        self.persist(&inner)?;
        Ok(outcome)
    }

    // ----- download ------------------------------------------------------

    /// Starts downloading the available update in the background (OTA-15).
    pub fn start_download(self: &Arc<Self>) -> Result<(), OpError> {
        let guard = self.try_job()?;
        let available = {
            let mut inner = self.lock();
            let available = inner.saved.available.clone().ok_or_else(|| {
                OpError::new(code::NO_UPDATE, "no update is available; check first")
            })?;
            if self.downloaded_file(&mut inner).is_some() {
                return Ok(());
            }
            available
        };
        self.valid_now().ok_or_else(clock_not_set)?;
        self.begin_job(Phase::Downloading);
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let result = service.download_inner(&available).await;
            service.end_job();
            drop(guard);
            match result {
                Ok(()) => {
                    tracing::info!(version = %available.version, "update downloaded and verified")
                }
                Err(e) => {
                    tracing::warn!(error = %e, "download failed");
                    service.record_error(&e);
                }
            }
        });
        Ok(())
    }

    async fn download_inner(self: &Arc<Self>, available: &SavedAvailable) -> Result<(), OpError> {
        let update_dir = self.config.layout.update_dir();
        let name = kup_name(&available.version);
        let dest = update_dir.join(&name);
        if let Some(size) = available.size {
            let free = crate::sd::free_space(&update_dir)
                .map_err(|e| OpError::new(code::INTERNAL, e.to_string()))?;
            let need = size.saturating_mul(2);
            if free < need {
                return Err(OpError::new(
                    code::NO_SPACE,
                    format!("the card has {free} bytes free, the download needs {need}"),
                ));
            }
        }
        self.set_progress(0, available.size, "download");
        let service = Arc::clone(self);
        let estimate = available.size;
        let mut counted = 0u64;
        let progress = Box::new(move |done: u64, total: Option<u64>| {
            if done < counted {
                counted = 0;
            }
            counter!("kamoder_update_download_bytes_total").increment(done - counted);
            counted = done;
            service.set_progress(done, total.or(estimate), "download");
        });
        tracing::info!(url = %available.kup_url, dest = %dest.display(), "downloading update");
        self.source
            .download(&available.kup_url, &dest, progress)
            .await?;

        self.set_phase(Phase::Verifying);
        let keys = self.config.keys.clone();
        let policy = self.policy(false);
        let (seq, version) = (available.seq, available.version.clone());
        let path = dest.clone();
        let verified = tokio::task::spawn_blocking(move || {
            verify_package(&path, &keys, &policy, seq, &version)
        })
        .await
        .unwrap_or_else(|e| Err(join_error(e)));
        if let Err(e) = verified {
            remove_file_logged(&dest);
            return Err(e);
        }
        let size = std::fs::metadata(&dest).map(|m| m.len()).ok();
        remove_other_packages(&update_dir, &name);

        let mut inner = self.lock();
        match inner.saved.available.as_mut() {
            Some(a) if a.seq == seq => {
                a.size = size.or(a.size);
                inner.saved.downloaded = Some(name);
            }
            // A check replaced the available update meanwhile.
            _ => remove_file_logged(&dest),
        }
        self.persist(&inner)
    }

    // ----- install -------------------------------------------------------

    /// Verifies a package and, if it passes, installs it in the background
    /// into the inactive slot (OTA-16, OTA-17).
    pub async fn install(
        self: &Arc<Self>,
        source: update_proto::Source,
        force: bool,
        reboot: bool,
    ) -> Result<(), OpError> {
        let guard = self.try_job()?;
        if self.is_trial() {
            return Err(OpError::new(
                code::TRIAL,
                "the running slot is still on trial; wait for it to commit (installing now would \
                 overwrite the only known-good slot)",
            ));
        }
        let (path, upload) = match &source {
            update_proto::Source::Downloaded => {
                let mut inner = self.lock();
                let path = self.downloaded_file(&mut inner).ok_or_else(|| {
                    OpError::new(code::NOT_DOWNLOADED, "no update has been downloaded")
                })?;
                (path, false)
            }
            update_proto::Source::Upload { path } => (self.upload_path(path)?, true),
        };
        self.begin_job(Phase::Verifying);
        let keys = self.config.keys.clone();
        let policy = self.policy(force);
        let open_path = path.clone();
        let opened = tokio::task::spawn_blocking(move || {
            let kup = Kup::open(&open_path).map_err(kup_error)?;
            let verified = kup.verify_and_check(&keys, &policy).map_err(kup_error)?;
            Ok::<_, OpError>((kup, verified))
        })
        .await
        .unwrap_or_else(|e| Err(join_error(e)));
        let (kup, verified) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                if upload {
                    remove_file_logged(&path);
                }
                self.end_job();
                self.record_error(&e);
                counter!("kamoder_update_installs_total", "result" => "refused").increment(1);
                return Err(e);
            }
        };
        tracing::info!(
            version = %verified.manifest.version,
            seq = verified.manifest.seq,
            file = %path.display(),
            force,
            reboot,
            "installing update"
        );
        let service = Arc::clone(self);
        tokio::spawn(async move {
            service
                .install_job(kup, verified, InstallFrom { path, upload }, reboot)
                .await;
            drop(guard);
        });
        Ok(())
    }

    async fn install_job(
        self: &Arc<Self>,
        kup: Kup,
        verified: VerifiedManifest,
        from: InstallFrom,
        reboot: bool,
    ) {
        self.set_phase(Phase::Installing);
        let total = crate::progress::manifest_total(&verified.manifest);
        self.set_progress(0, Some(total), "install:slot");
        let seq = verified.manifest.seq;
        let floor = verified.manifest.security_floor.clone();
        let version = verified.manifest.version.to_string();
        let layout = self.config.layout.clone();
        let booted = self.lock().booted;
        let service = Arc::clone(self);
        let result = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(layout.boot_dir()).map_err(|e| {
                OpError::new(
                    code::INSTALL_FAILED,
                    format!("cannot create {}: {e}", layout.boot_dir().display()),
                )
            })?;
            let mut control = SdSelector::new(layout.boot_dir());
            let mut store = select_store(&layout, booted, &verified.manifest)?;
            install_to_inactive(
                &mut control,
                store.as_mut(),
                &kup,
                &verified,
                &mut |p: &crate::progress::Progress| {
                    service.set_progress(
                        p.bytes_done,
                        Some(p.bytes_total),
                        &format!("install:{}", p.component),
                    )
                },
            )
            .map_err(store_error)
        })
        .await
        .unwrap_or_else(|e| Err(join_error(e)));
        if from.upload {
            remove_file_logged(&from.path);
        }
        match result {
            Ok(slot) => {
                {
                    let mut inner = self.lock();
                    inner.saved.highest_seq = inner.saved.highest_seq.max(seq);
                    raise_floor(&mut inner.saved.security_floor, &floor);
                    if inner.saved.available.as_ref().is_some_and(|a| a.seq <= seq) {
                        inner.saved.available = None;
                        if let Some(name) = inner.saved.downloaded.take() {
                            remove_file_logged(&self.config.layout.update_dir().join(name));
                        }
                        gauge!("kamoder_update_available").set(0.0);
                    }
                    inner.awaiting_reboot = true;
                    inner.job_phase = None;
                    inner.progress = None;
                    if let Err(e) = self.persist(&inner) {
                        // The slot is installed and activated; only the
                        // replay memory is behind (the slot's own manifest
                        // restores it after the reboot).
                        tracing::error!(error = %e, "cannot save the update state after install");
                    }
                }
                counter!("kamoder_update_installs_total", "result" => "ok").increment(1);
                tracing::info!(%slot, %version, "update installed; slot activated for a trial boot");
                if reboot {
                    tokio::time::sleep(self.config.reboot_delay).await;
                    self.reboot_now();
                }
            }
            Err(e) => {
                counter!("kamoder_update_installs_total", "result" => "failed").increment(1);
                tracing::warn!(error = %e, "install failed; the running slot is untouched");
                self.end_job();
                self.record_error(&e);
            }
        }
    }

    /// Resolves an upload path: relative to the data root, inside `update/`.
    fn upload_path(&self, path: &str) -> Result<PathBuf, OpError> {
        let bad = |why: &str| OpError::new(code::BAD_UPLOAD_PATH, format!("{path:?}: {why}"));
        let given = Path::new(path);
        let full = if given.is_absolute() {
            given.to_path_buf()
        } else {
            self.config.layout.data_root().join(given)
        };
        let full = std::fs::canonicalize(&full).map_err(|e| bad(&e.to_string()))?;
        let dir = std::fs::canonicalize(self.config.layout.update_dir())
            .map_err(|e| bad(&e.to_string()))?;
        if !full.starts_with(&dir) || full == dir {
            return Err(bad("uploads must be inside update/"));
        }
        if !full.is_file() {
            return Err(bad("not a file"));
        }
        Ok(full)
    }

    // ----- rollback, commit, reboot, settings ----------------------------

    /// Makes the other slot the one that boots (OTA-19).
    pub fn rollback(self: &Arc<Self>, reboot: bool) -> Result<(), OpError> {
        let _guard = self.try_job()?;
        let mut control = SdSelector::new(self.config.layout.boot_dir());
        control.rollback().map_err(|e| match e {
            bootctl::Error::NoRollbackTarget => OpError::new(
                code::NO_ROLLBACK_TARGET,
                "the other slot is not bootable and committed",
            ),
            other => OpError::new(code::BOOT_CONTROL, other.to_string()),
        })?;
        tracing::info!(reboot, "rolled back: the other slot boots next");
        self.lock().awaiting_reboot = true;
        if reboot {
            self.spawn_reboot();
        }
        Ok(())
    }

    /// Marks the running slot successful (health check, or the debug op).
    pub fn commit(&self) -> Result<(), OpError> {
        let booted = self.lock().booted;
        let boot_dir = self.config.layout.boot_dir();
        std::fs::create_dir_all(&boot_dir)
            .map_err(|e| OpError::new(code::BOOT_CONTROL, e.to_string()))?;
        SdSelector::new(boot_dir)
            .mark_successful(booted)
            .map_err(|e| OpError::new(code::BOOT_CONTROL, e.to_string()))?;
        self.lock().health = None;
        gauge!("kamoder_update_trial").set(0.0);
        tracing::info!(slot = %booted, "slot committed");
        Ok(())
    }

    /// Replaces and persists the settings (OTA-18).
    pub fn set_settings(&self, settings: Settings) -> Result<(), OpError> {
        if !valid_repo(&settings.repo) {
            return Err(OpError::new(
                code::INVALID_SETTINGS,
                format!("repo {:?} is not owner/name", settings.repo),
            ));
        }
        if settings.window.is_some_and(|w| !w.is_valid()) {
            return Err(OpError::new(
                code::INVALID_SETTINGS,
                "window hours must be 0..=23",
            ));
        }
        let mut inner = self.lock();
        let old = std::mem::replace(&mut inner.saved.settings, settings);
        if let Err(e) = self.persist(&inner) {
            inner.saved.settings = old;
            return Err(e);
        }
        if old.channel != inner.saved.settings.channel || old.repo != inner.saved.settings.repo {
            // Another channel's "available" is not this one's.
            inner.saved.available = None;
            if let Some(name) = inner.saved.downloaded.take() {
                remove_file_logged(&self.config.layout.update_dir().join(name));
            }
            let _ = self.persist(&inner);
            gauge!("kamoder_update_available").set(0.0);
        }
        tracing::info!(settings = ?inner.saved.settings, "update settings changed");
        drop(inner);
        self.wake.notify_one();
        Ok(())
    }

    fn spawn_reboot(self: &Arc<Self>) {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(service.config.reboot_delay).await;
            service.reboot_now();
        });
    }

    fn reboot_now(&self) {
        tracing::warn!("rebooting");
        (self.config.reboot)();
    }

    // ----- background ----------------------------------------------------

    async fn scheduler(self: Arc<Self>) {
        let cfg = self.config.schedule;
        let mut next_check = tokio::time::Instant::now() + cfg.first_delay;
        let mut seed = jitter_seed();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(cfg.tick) => {}
                _ = self.wake.notified() => {}
            }
            let auto_check = self.lock().saved.settings.auto_check;
            let now = tokio::time::Instant::now();
            if auto_check && now >= next_check && self.valid_now().is_some() {
                match self.check().await {
                    Ok(()) => next_check = now + jittered(cfg.interval, seed),
                    Err(e) if e.code == code::BUSY => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "scheduled check failed");
                        next_check = now + cfg.retry.min(cfg.interval);
                    }
                }
                seed = seed.rotate_left(17) ^ 0x9E37_79B9_7F4A_7C15;
            }
            self.maybe_auto_install().await;
        }
    }

    /// Installs the downloaded package unattended when the settings and the
    /// window allow (once per package).
    pub async fn maybe_auto_install(self: &Arc<Self>) {
        let trial = self.is_trial();
        let local_hour = self.valid_now().map(|t| os::time::local(t as i64, 0).hour);
        let (due, seq) = {
            let mut inner = self.lock();
            let downloaded = self.downloaded_file(&mut inner).is_some();
            let seq = inner.saved.available.as_ref().map(|a| a.seq);
            let due = auto_install_due(&AutoInstall {
                settings: &inner.saved.settings,
                downloaded,
                trial,
                busy: inner.job_phase.is_some() || inner.awaiting_reboot,
                attempted: seq.is_some() && inner.auto_attempted_seq == seq,
                local_hour,
            });
            if due {
                inner.auto_attempted_seq = seq;
            }
            (due, seq)
        };
        if !due {
            return;
        }
        tracing::info!(seq, "installing the downloaded update automatically");
        if let Err(e) = self
            .install(update_proto::Source::Downloaded, false, true)
            .await
        {
            tracing::warn!(error = %e, "automatic install refused");
        }
    }

    async fn health_loop(self: Arc<Self>) {
        let cfg = self.config.health;
        let manifest = health::verify_own_manifest(&self.config.slot_dir, &self.config.keys);
        if let Err(reason) = &manifest {
            tracing::error!(%reason, "the running slot's manifest does not verify; it will not commit");
        }
        tracing::info!(
            stable_s = cfg.stable.as_secs(),
            "trial boot: committing once healthy"
        );
        let mut tracker = Tracker::new();
        let mut ticker = tokio::time::interval(cfg.poll);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_reasons: Vec<String> = Vec::new();
        loop {
            ticker.tick().await;
            if !self.is_trial() {
                tracing::info!("slot committed; health check done");
                self.lock().health = None;
                return;
            }
            let mut probe = Probe::default();
            health::probe_supervisor(&self.config.supervisor_socket, &mut probe).await;
            if !health::probe_web(self.config.web_addr).await {
                probe.waiting_on.push("web".into());
            }
            if let Err(reason) = &manifest {
                probe.waiting_on.push(format!("manifest: {reason}"));
            }
            let ok_for = tracker.observe(&mut probe, Instant::now());
            if probe.waiting_on != last_reasons {
                if !probe.waiting_on.is_empty() {
                    tracing::info!(waiting_on = ?probe.waiting_on, "trial not healthy yet");
                }
                last_reasons = probe.waiting_on.clone();
            }
            let deadline_s = cfg
                .trial_window
                .saturating_sub(os::time::uptime())
                .as_secs();
            self.lock().health = Some(HealthInfo {
                ok_for_s: ok_for.as_secs(),
                waiting_on: probe.waiting_on,
                deadline_s,
            });
            if ok_for >= cfg.stable {
                match self.commit() {
                    Ok(()) => return,
                    Err(e) => tracing::warn!(error = %e, "cannot commit the slot"),
                }
            }
        }
    }

    // ----- helpers -------------------------------------------------------

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn try_job(&self) -> Result<OwnedMutexGuard<()>, OpError> {
        Arc::clone(&self.job)
            .try_lock_owned()
            .map_err(|_| OpError::new(code::BUSY, "another check, download or install is running"))
    }

    fn begin_job(&self, phase: Phase) {
        let mut inner = self.lock();
        inner.job_phase = Some(phase);
        inner.progress = None;
        inner.failed = false;
    }

    fn set_phase(&self, phase: Phase) {
        self.lock().job_phase = Some(phase);
    }

    fn end_job(&self) {
        let mut inner = self.lock();
        inner.job_phase = None;
        inner.progress = None;
    }

    fn set_progress(&self, done: u64, total: Option<u64>, what: &str) {
        self.lock().progress = Some(Progress {
            bytes_done: done,
            bytes_total: total,
            what: what.to_owned(),
        });
    }

    fn record_error(&self, e: &OpError) {
        let at = self.valid_now();
        let mut inner = self.lock();
        inner.failed = true;
        inner.saved.last_error = Some(ErrorInfo {
            code: e.code.clone(),
            message: e.message.clone(),
            at,
        });
        if let Err(error) = self.persist(&inner) {
            tracing::warn!(error = %error, "cannot save the last error");
        }
    }

    fn persist(&self, inner: &Inner) -> Result<(), OpError> {
        state::save(&self.state_path, &inner.saved).map_err(|e| {
            OpError::new(
                code::STATE,
                format!("cannot save {}: {e}", self.state_path.display()),
            )
        })
    }

    /// The verified download, if its file still exists (clears the flag
    /// otherwise).
    fn downloaded_file(&self, inner: &mut Inner) -> Option<PathBuf> {
        let name = inner.saved.downloaded.as_ref()?;
        let path = self.config.layout.update_dir().join(name);
        if path.is_file() {
            return Some(path);
        }
        tracing::info!(file = %name, "downloaded package is gone");
        inner.saved.downloaded = None;
        let _ = self.persist(inner);
        None
    }

    /// Unix seconds, if the clock is NTP-valid.
    fn valid_now(&self) -> Option<u64> {
        let now = (self.config.clock)();
        (now >= os::time::CLOCK_VALID_FLOOR_UNIX).then_some(now as u64)
    }

    fn policy(&self, force: bool) -> Policy {
        let inner = self.lock();
        Policy {
            board: self.config.board.clone(),
            target: self.config.target.clone(),
            current_version: inner.version.clone(),
            updater_version: inner.version.clone(),
            highest_seq: inner.saved.highest_seq,
            security_floor: inner
                .saved
                .security_floor
                .clone()
                .unwrap_or_else(|| Version::new(0, 0, 0)),
            force,
        }
    }
}

/// Registers the descriptions of every metric the service emits.
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge};
    describe_counter!(
        "kamoder_update_checks_total",
        "Update checks, by result (available, downloaded, up_to_date, error)"
    );
    describe_counter!(
        "kamoder_update_download_bytes_total",
        "Bytes of update packages downloaded"
    );
    describe_counter!(
        "kamoder_update_installs_total",
        "Installs, by result (ok, refused, failed)"
    );
    describe_gauge!(
        "kamoder_update_trial",
        "1 while the running slot is on trial (not committed)"
    );
    describe_gauge!(
        "kamoder_update_available",
        "1 while the last check found a newer update"
    );
    describe_counter!(
        "kamoder_update_socket_requests_total",
        "Requests on the update socket, by op and result"
    );
}

/// `sync()` then `reboot(RB_AUTOBOOT)`. Returns only on failure (logged).
/// Services are not stopped first: everything that matters is fsynced, and
/// stopping the stack from inside it would kill this process mid-way.
pub fn system_reboot() {
    // SAFETY: neither call takes pointers; sync() cannot fail and reboot()
    // only returns on error.
    let rc = unsafe {
        libc::sync();
        libc::reboot(libc::RB_AUTOBOOT)
    };
    if rc != 0 {
        tracing::error!(error = %std::io::Error::last_os_error(), "reboot failed");
    }
}

/// Result of a successful check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckOutcome {
    UpToDate,
    Downloaded,
    NeedsDownload,
}

impl CheckOutcome {
    fn label(self) -> &'static str {
        match self {
            CheckOutcome::UpToDate => "up_to_date",
            CheckOutcome::Downloaded => "downloaded",
            CheckOutcome::NeedsDownload => "available",
        }
    }
}

/// Where an install's package came from (an upload is deleted afterwards).
struct InstallFrom {
    path: PathBuf,
    upload: bool,
}

/// The storage backend for a package. Board profiles have no `[update]`
/// table yet (OTA-22), so every board uses SD slot directories; this is the
/// seam where an `MtdStore` (`crate::mtd`) gets chosen for `mtd-image`
/// packages once a board declares MTD slots.
fn select_store(
    layout: &Layout,
    booted: Slot,
    manifest: &Manifest,
) -> Result<Box<dyn SlotStore + Send>, OpError> {
    if let Some(c) = manifest
        .components
        .iter()
        .find(|c| matches!(c.kind, ComponentKind::MtdImage { .. }))
    {
        return Err(OpError::new(
            code::BAD_PACKAGE,
            format!(
                "component {:?} is an MTD image; this board has no MTD update backend",
                c.name
            ),
        ));
    }
    Ok(Box::new(SdDirStore::new(layout.clone(), booted)))
}

/// Opens a downloaded package and checks it end to end: signature, policy,
/// that it is the release the manifest announced, every component digest.
fn verify_package(
    path: &Path,
    keys: &TrustedKeys,
    policy: &Policy,
    seq: u64,
    version: &str,
) -> Result<(), OpError> {
    let kup = Kup::open(path).map_err(kup_error)?;
    let verified = kup.verify_and_check(keys, policy).map_err(kup_error)?;
    let m = &verified.manifest;
    if m.seq != seq || m.version.to_string() != version {
        return Err(OpError::new(
            code::BAD_PACKAGE,
            format!(
                "the package is {} (seq {}), the release announced {version} (seq {seq})",
                m.version, m.seq
            ),
        ));
    }
    for component in &m.components {
        kup.component_reader(component)
            .and_then(|mut reader| reader.finish())
            .map_err(kup_error)?;
    }
    Ok(())
}

fn kup_code(e: &kup::Error) -> &'static str {
    match e {
        kup::Error::Rejected { source } => source.code(),
        e if e.is_bad_package() => code::BAD_PACKAGE,
        _ => code::INSTALL_FAILED,
    }
}

fn kup_error(e: kup::Error) -> OpError {
    OpError::new(kup_code(&e), e.to_string())
}

fn store_error(e: store::Error) -> OpError {
    use store::Error as E;
    let code = match &e {
        E::Kup { source } => kup_code(source),
        E::NoSpace { .. } => code::NO_SPACE,
        E::Boot { .. } => code::BOOT_CONTROL,
        E::Io { .. }
        | E::BootedSlot { .. }
        | E::NoSdRoot { .. }
        | E::Mtd { .. }
        | E::Flash { .. }
        | E::Verify { .. } => code::INSTALL_FAILED,
        _ => code::BAD_PACKAGE,
    };
    OpError::new(code, e.to_string())
}

fn join_error(e: tokio::task::JoinError) -> OpError {
    OpError::new(code::INTERNAL, format!("worker task failed: {e}"))
}

fn clock_not_set() -> OpError {
    OpError::new(
        code::CLOCK_NOT_SET,
        "clock not set: waiting for NTP (TLS needs the correct time)",
    )
}

fn no_trusted_key() -> OpError {
    OpError::new(
        code::NO_TRUSTED_KEY,
        "this build has no update key; packages cannot be verified",
    )
}

/// `owner/name`, GitHub's character set.
fn valid_repo(repo: &str) -> bool {
    let ok = |part: &str| {
        !part.is_empty()
            && part.len() <= 100
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && part != "."
            && part != ".."
    };
    matches!(repo.split_once('/'), Some((owner, name)) if ok(owner) && ok(name))
}

/// Keeps the higher of two floors; true when it changed.
fn raise_floor(floor: &mut Option<Version>, seen: &Version) -> bool {
    let higher = floor
        .as_ref()
        .is_none_or(|f| compare_versions(seen, f) == std::cmp::Ordering::Greater);
    if higher {
        *floor = Some(seen.clone());
    }
    higher
}

/// `.kup` size from its manifest: members + tar headers and padding.
fn estimate_kup_size(m: &Manifest, manifest_len: usize, signature_len: usize) -> u64 {
    let member = |len: u64| 512 + len.div_ceil(512) * 512;
    m.components.iter().map(|c| member(c.size)).sum::<u64>()
        + member(manifest_len as u64)
        + member(signature_len as u64)
        + 1024
}

/// The boot record; unreadable storage counts as the legacy default
/// (logged), like kboot does.
fn load_record(layout: &Layout) -> Record {
    SdSelector::new(layout.boot_dir())
        .load()
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot read the boot record; assuming a legacy card");
            Record::default_legacy()
        })
}

/// The slot this boot runs from: what kboot recorded, else inferred from the
/// slot directory (no record: a legacy card, slot `a`).
fn booted_slot(record: &Record, layout: &Layout, slot_dir: &Path) -> Slot {
    if let Some(slot) = record.last_booted {
        return slot;
    }
    let same = |a: &Path, b: &Path| {
        a == b
            || matches!(
                (std::fs::canonicalize(a), std::fs::canonicalize(b)),
                (Ok(x), Ok(y)) if x == y
            )
    };
    if same(slot_dir, &layout.install_dir(Slot::B)) {
        Slot::B
    } else {
        Slot::A
    }
}

/// A slot's `VERSION` file (`0.2.0` or `v0.2.0`).
fn read_version(slot_dir: &Path) -> Option<Version> {
    let text = std::fs::read_to_string(slot_dir.join(VERSION_FILE)).ok()?;
    let text = text.trim();
    text.strip_prefix('v').unwrap_or(text).parse().ok()
}

/// The running slot's manifest, if it verifies.
fn own_manifest(slot_dir: &Path, keys: &TrustedKeys) -> Option<Manifest> {
    if keys.is_empty() {
        return None;
    }
    let json = std::fs::read(slot_dir.join(MANIFEST_NAME)).ok()?;
    let signature = std::fs::read_to_string(slot_dir.join(SIGNATURE_NAME)).ok()?;
    match ota_manifest::verify(&json, &signature, keys) {
        Ok(m) => Some(m),
        Err(error) => {
            tracing::warn!(%error, "the running slot's manifest does not verify");
            None
        }
    }
}

/// Deletes every package and partial download in `update/` except `keep`
/// and the upload (at most one downloaded package on the card).
fn remove_other_packages(update_dir: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(update_dir) else {
        return;
    };
    let upload = Path::new(update_proto::UPLOAD_PATH)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("upload.kup");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let package = name.ends_with(".kup") || name.ends_with(".kup.part");
        if package && name != keep && name != upload {
            remove_file_logged(&entry.path());
        }
    }
}

fn remove_file_logged(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::debug!(path = %path.display(), "removed"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(path = %path.display(), %error, "cannot remove"),
    }
}

fn wire_slot(slot: Slot) -> update_proto::Slot {
    match slot {
        Slot::A => update_proto::Slot::A,
        Slot::B => update_proto::Slot::B,
    }
}

fn wire_channel(channel: ota_manifest::Channel) -> update_proto::Channel {
    match channel {
        ota_manifest::Channel::Stable => update_proto::Channel::Stable,
        ota_manifest::Channel::Beta => update_proto::Channel::Beta,
    }
}

/// Per-process seed for the check jitter (no RNG crate needed for this).
fn jitter_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    u64::from(nanos) ^ (u64::from(std::process::id()) << 32)
}
