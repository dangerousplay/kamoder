//! Service tests: a temp card, a fake release source serving signed
//! fixtures from memory, a fake supervisor socket and a local web listener.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bootctl::{BootControl, Record, SdSelector, Slot};
use fetch::github::{latest_stable_asset_url, Asset, Release};
use ota_manifest::{trusted_comment, TrustedKeys};
use supervisor::control::{self as sup, ServiceStatus, State};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use update_proto::{code, Channel, Phase, Request, Response, Settings, Status, DEFAULT_REPO};

use super::health::HealthConfig;
use super::schedule::ScheduleConfig;
use super::source::{kup_name, DownloadProgress, Source, SourceError};
use super::state::{self, Saved, STATE_FILE};
use super::*;
use crate::kup::{Kup, MANIFEST_NAME, SIGNATURE_NAME};
use crate::test_support::{sign, Fixture, TempDir};

/// A valid clock (after `CLOCK_VALID_FLOOR_UNIX`).
const NOW: i64 = os::time::CLOCK_VALID_FLOOR_UNIX + 86_400;

// ----- fake release source ------------------------------------------------

#[derive(Default)]
struct FakeState {
    urls: HashMap<String, Vec<u8>>,
    releases: Vec<Release>,
    gets: usize,
}

/// Serves URLs from memory.
#[derive(Clone, Default)]
struct FakeSource(Arc<Mutex<FakeState>>);

impl FakeSource {
    fn put(&self, url: impl Into<String>, body: Vec<u8>) {
        self.0.lock().unwrap().urls.insert(url.into(), body);
    }

    fn gets(&self) -> usize {
        self.0.lock().unwrap().gets
    }

    fn lookup(&self, url: &str) -> Result<Vec<u8>, SourceError> {
        let mut state = self.0.lock().unwrap();
        state.gets += 1;
        state
            .urls
            .get(url)
            .cloned()
            .ok_or_else(|| SourceError::new(code::NOT_FOUND, format!("{url} answered HTTP 404")))
    }
}

impl Source for FakeSource {
    async fn get(&self, url: &str, max_len: u64) -> Result<Vec<u8>, SourceError> {
        let body = self.lookup(url)?;
        assert!(body.len() as u64 <= max_len, "{url} larger than {max_len}");
        Ok(body)
    }

    async fn releases(&self, _repo: &str) -> Result<Vec<Release>, SourceError> {
        Ok(self.0.lock().unwrap().releases.clone())
    }

    async fn download(
        &self,
        url: &str,
        dest: &Path,
        mut progress: DownloadProgress,
    ) -> Result<u64, SourceError> {
        let body = self.lookup(url)?;
        std::fs::write(dest, &body).unwrap();
        progress(body.len() as u64, Some(body.len() as u64));
        Ok(body.len() as u64)
    }
}

/// The signed manifest, its signature and the `.kup` of `fx`.
struct Published {
    manifest: Vec<u8>,
    signature: Vec<u8>,
    kup: Vec<u8>,
}

fn publish(fx: &Fixture, scratch: &Path) -> Published {
    let dir = scratch.join(format!("pkg-{}-{}", fx.version, fx.seq));
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = fx.manifest();
    let json = manifest.to_json();
    let signature = sign(&json, &trusted_comment(&manifest)).into_bytes();
    let kup = std::fs::read(fx.write_kup(&dir)).unwrap();
    Published {
        manifest: json,
        signature,
        kup,
    }
}

/// Makes `fx` the repo's latest stable release.
fn publish_stable(source: &FakeSource, fx: &Fixture, scratch: &Path) -> Published {
    let p = publish(fx, scratch);
    source.put(
        latest_stable_asset_url(DEFAULT_REPO, "manifest.json"),
        p.manifest.clone(),
    );
    source.put(
        latest_stable_asset_url(DEFAULT_REPO, "manifest.json.minisig"),
        p.signature.clone(),
    );
    source.put(
        latest_stable_asset_url(DEFAULT_REPO, &kup_name(fx.version)),
        p.kup.clone(),
    );
    p
}

// ----- card and service ----------------------------------------------------

struct Card {
    tmp: TempDir,
    root: PathBuf,
    layout: Layout,
}

/// A legacy card: code in the data root, running 0.1.0, no boot record.
fn legacy_card(tag: &str) -> Card {
    let tmp = TempDir::new(tag);
    let root = tmp.path().join("anyka_rust");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("bin/web"), b"legacy web").unwrap();
    std::fs::write(root.join(VERSION_FILE), b"0.1.0\n").unwrap();
    std::fs::write(root.join("camera.conf"), b"WIFI=x\n").unwrap();
    let layout = Layout::new(&root);
    Card { tmp, root, layout }
}

struct Harness {
    reboots: Arc<AtomicUsize>,
    /// Unix seconds the service sees (no AtomicI64 on ARMv5TE).
    clock: Arc<Mutex<i64>>,
}

fn config(card: &Card, keys: TrustedKeys, slot_dir: &Path) -> (Config, Harness) {
    let reboots = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(Mutex::new(NOW));
    let r = Arc::clone(&reboots);
    let c = Arc::clone(&clock);
    let config = Config {
        layout: card.layout.clone(),
        slot_dir: slot_dir.to_path_buf(),
        board: "test-board".into(),
        target: TARGET.into(),
        keys,
        compiled_version: "0.1.0".parse().unwrap(),
        supervisor_socket: card.tmp.path().join("sup.sock"),
        web_addr: "127.0.0.1:9".parse().unwrap(),
        health: HealthConfig {
            poll: Duration::from_millis(20),
            stable: Duration::from_millis(150),
            trial_window: Duration::from_secs(600),
        },
        schedule: ScheduleConfig {
            first_delay: Duration::from_millis(10),
            interval: Duration::from_secs(3600),
            retry: Duration::from_secs(60),
            tick: Duration::from_millis(10),
        },
        reboot_delay: Duration::from_millis(10),
        reboot: Arc::new(move || {
            r.fetch_add(1, Ordering::SeqCst);
        }),
        clock: Arc::new(move || *c.lock().unwrap()),
        seed_repo: None,
    };
    (config, Harness { reboots, clock })
}

fn service(card: &Card, source: &FakeSource) -> (Arc<Service<FakeSource>>, Harness) {
    let (config, harness) = config(card, TrustedKeys::test_key(), &card.root);
    (Service::new(config, source.clone()).unwrap(), harness)
}

async fn request(
    svc: &Arc<Service<FakeSource>>,
    req: Request,
) -> Result<Option<Status>, (String, String)> {
    match svc.handle(req).await {
        Response::Status(s) => Ok(Some(*s)),
        Response::Ok => Ok(None),
        Response::Error { code, message } => Err((code, message)),
    }
}

fn error_code(result: Result<Option<Status>, (String, String)>) -> String {
    match result {
        Err((code, _)) => code,
        Ok(s) => panic!("expected an error, got {s:?}"),
    }
}

/// Waits until no job is running; returns the status then.
async fn settle(svc: &Arc<Service<FakeSource>>) -> Status {
    for _ in 0..1000 {
        let s = svc.status();
        if !matches!(
            s.phase,
            Phase::Checking | Phase::Downloading | Phase::Verifying | Phase::Installing
        ) && svc.job.try_lock().is_ok()
        {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("service did not settle: {:?}", svc.status());
}

fn record(card: &Card) -> Record {
    SdSelector::new(card.layout.boot_dir()).load().unwrap()
}

fn store_record(card: &Card, rec: &Record) {
    std::fs::create_dir_all(card.layout.boot_dir()).unwrap();
    SdSelector::new(card.layout.boot_dir()).store(rec).unwrap();
}

fn saved(card: &Card) -> Saved {
    state::load(&card.layout.update_dir().join(STATE_FILE)).unwrap_or_default()
}

const INSTALL: Request = Request::Install {
    source: update_proto::Source::Downloaded,
    force: false,
    reboot: true,
};

/// Check + download of `fx` as the latest stable release; returns the status.
async fn downloaded(
    svc: &Arc<Service<FakeSource>>,
    card: &Card,
    source: &FakeSource,
    fx: &Fixture,
) -> Status {
    publish_stable(source, fx, card.tmp.path());
    let status = request(svc, Request::Check).await.unwrap().unwrap();
    assert_eq!(status.available.as_ref().unwrap().version, fx.version);
    let status = settle(svc).await;
    assert!(
        status.available.as_ref().unwrap().downloaded,
        "not downloaded: {status:?}"
    );
    status
}

// ----- check & download ----------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn check_finds_and_downloads_a_stable_update() {
    let card = legacy_card("svc-check");
    let source = FakeSource::default();
    let (svc, _h) = service(&card, &source);

    let status = svc.status();
    assert_eq!(status.version, "0.1.0");
    assert_eq!(status.slot, update_proto::Slot::A);
    assert!(status.committed && !status.trial);
    assert_eq!(status.phase, Phase::Idle);
    assert!(status.trusted_key);
    assert!(status.slots[0].dir_present && !status.slots[1].dir_present);
    assert_eq!(status.slots[0].version.as_deref(), Some("0.1.0"));

    let fx = Fixture::standard();
    let published = publish_stable(&source, &fx, card.tmp.path());
    let status = request(&svc, Request::Check).await.unwrap().unwrap();
    let available = status.available.unwrap();
    assert_eq!(available.version, "0.2.0");
    assert_eq!(available.seq, fx.seq);
    assert_eq!(available.channel, Channel::Stable);
    assert!(!available.downgrade);
    assert_eq!(status.last_check, Some(NOW as u64));
    // Estimated from the manifest: within a few KiB of the real package.
    let estimate = available.size.unwrap();
    assert!(
        estimate.abs_diff(published.kup.len() as u64) < 4096,
        "{estimate}"
    );

    // The download started by itself.
    let status = settle(&svc).await;
    let available = status.available.unwrap();
    assert!(available.downloaded, "{:?}", status.last_error);
    assert_eq!(available.size, Some(published.kup.len() as u64));
    assert_eq!(status.phase, Phase::Idle);
    let file = card.layout.update_dir().join("kamoder-0.2.0-update.kup");
    assert_eq!(std::fs::read(&file).unwrap(), published.kup);
    let saved = saved(&card);
    assert_eq!(
        saved.downloaded.as_deref(),
        Some("kamoder-0.2.0-update.kup")
    );
    assert_eq!(saved.last_check, Some(NOW as u64));

    // A second check finds the same release already downloaded.
    let gets = source.gets();
    let status = request(&svc, Request::Check).await.unwrap().unwrap();
    assert!(status.available.unwrap().downloaded);
    assert_eq!(source.gets(), gets + 2, "manifest + signature only");
    assert_eq!(request(&svc, Request::Download).await, Ok(None));
}

#[tokio::test(flavor = "current_thread")]
async fn checks_need_a_clock_and_a_key() {
    let card = legacy_card("svc-clock");
    let source = FakeSource::default();
    publish_stable(&source, &Fixture::standard(), card.tmp.path());

    let (svc, h) = service(&card, &source);
    *h.clock.lock().unwrap() = 1_000;
    assert_eq!(
        error_code(request(&svc, Request::Check).await),
        code::CLOCK_NOT_SET
    );
    let status = svc.status();
    assert_eq!(status.phase, Phase::Error);
    let error = status.last_error.unwrap();
    assert_eq!(error.code, code::CLOCK_NOT_SET);
    assert_eq!(error.at, None, "no wall clock to stamp it with");
    assert_eq!(source.gets(), 0);

    let (config, _h) = config(&card, TrustedKeys::empty(), &card.root);
    let svc = Service::new(config, source.clone()).unwrap();
    assert!(!svc.status().trusted_key);
    assert_eq!(
        error_code(request(&svc, Request::Check).await),
        code::NO_TRUSTED_KEY
    );
    assert_eq!(source.gets(), 0, "no network without a key");
    let error = svc.status().last_error.unwrap();
    assert_eq!(error.at, Some(NOW as u64));
    // Uploads are refused the same way.
    let upload = card.layout.update_dir().join("upload.kup");
    std::fs::copy(Fixture::standard().write_kup(card.tmp.path()), &upload).unwrap();
    let code = error_code(
        request(
            &svc,
            Request::Install {
                source: update_proto::Source::Upload {
                    path: update_proto::UPLOAD_PATH.into(),
                },
                force: false,
                reboot: false,
            },
        )
        .await,
    );
    assert_eq!(code, code::NO_TRUSTED_KEY);
    assert!(!upload.exists(), "a refused upload is deleted");
}

#[tokio::test(flavor = "current_thread")]
async fn up_to_date_when_seq_is_not_newer() {
    let card = legacy_card("svc-uptodate");
    let source = FakeSource::default();
    let fx = Fixture::standard();
    publish_stable(&source, &fx, card.tmp.path());
    std::fs::create_dir_all(card.layout.update_dir()).unwrap();
    state::save(
        &card.layout.update_dir().join(STATE_FILE),
        &Saved {
            highest_seq: fx.seq,
            ..Saved::default()
        },
    )
    .unwrap();
    let (svc, _h) = service(&card, &source);
    let status = request(&svc, Request::Check).await.unwrap().unwrap();
    assert_eq!(status.available, None);
    assert_eq!(status.last_error, None);
    assert_eq!(
        error_code(request(&svc, Request::Download).await),
        code::NO_UPDATE
    );
}

#[tokio::test(flavor = "current_thread")]
async fn wrong_board_is_reported_with_its_code() {
    let card = legacy_card("svc-board");
    let source = FakeSource::default();
    publish_stable(&source, &Fixture::standard(), card.tmp.path());
    let (mut config, _h) = config(&card, TrustedKeys::test_key(), &card.root);
    config.board = "other-board".into();
    let svc = Service::new(config, source.clone()).unwrap();
    assert_eq!(error_code(request(&svc, Request::Check).await), "board");
    assert_eq!(svc.status().last_error.unwrap().code, "board");
}

#[tokio::test(flavor = "current_thread")]
async fn beta_channel_uses_the_release_list() {
    let card = legacy_card("svc-beta");
    let source = FakeSource::default();
    let fx = Fixture {
        version: "0.3.0-rc.1",
        ..Fixture::standard()
    };
    let p = publish(&fx, card.tmp.path());
    let base = "https://example.test/rc1";
    source.put(format!("{base}/manifest.json"), p.manifest.clone());
    source.put(format!("{base}/manifest.json.minisig"), p.signature.clone());
    source.put(format!("{base}/kup"), p.kup.clone());
    let asset = |name: &str, url: String, size| Asset {
        name: name.into(),
        browser_download_url: url,
        size,
    };
    let release = |tag: &str, prerelease, published: &str, assets| Release {
        tag_name: tag.into(),
        name: None,
        prerelease,
        draft: false,
        html_url: String::new(),
        published_at: Some(published.into()),
        assets,
    };
    source.0.lock().unwrap().releases = vec![
        release("v0.2.0", false, "2026-10-01T00:00:00Z", Vec::new()),
        release(
            "v0.3.0-rc.1",
            true,
            "2026-10-10T00:00:00Z",
            vec![
                asset("manifest.json", format!("{base}/manifest.json"), 1),
                asset(
                    "manifest.json.minisig",
                    format!("{base}/manifest.json.minisig"),
                    1,
                ),
                asset(
                    "kamoder-0.3.0-rc.1-update.kup",
                    format!("{base}/kup"),
                    p.kup.len() as u64,
                ),
            ],
        ),
    ];

    let (svc, _h) = service(&card, &source);
    let settings = Settings {
        channel: Channel::Beta,
        ..Settings::default()
    };
    assert_eq!(
        request(&svc, Request::SetSettings(settings)).await,
        Ok(None)
    );
    let status = request(&svc, Request::Check).await.unwrap().unwrap();
    let available = status.available.unwrap();
    assert_eq!(available.version, "0.3.0-rc.1");
    assert_eq!(available.size, Some(p.kup.len() as u64), "from the asset");
    let status = settle(&svc).await;
    assert!(
        status.available.unwrap().downloaded,
        "{:?}",
        status.last_error
    );
    assert!(card
        .layout
        .update_dir()
        .join("kamoder-0.3.0-rc.1-update.kup")
        .is_file());
}

#[tokio::test(flavor = "current_thread")]
async fn a_tampered_download_is_discarded() {
    let card = legacy_card("svc-tamper");
    let source = FakeSource::default();
    let fx = Fixture::standard();
    let mut p = publish_stable(&source, &fx, card.tmp.path());
    // Flip a payload byte: the manifest still verifies, the component digest
    // does not.
    let scratch = card.tmp.path().join("probe.kup");
    std::fs::write(&scratch, &p.kup).unwrap();
    let entry = Kup::open(&scratch)
        .unwrap()
        .entry("payload.tar.gz")
        .unwrap()
        .clone();
    p.kup[entry.offset as usize + 20] ^= 0xff;
    source.put(
        latest_stable_asset_url(DEFAULT_REPO, &kup_name(fx.version)),
        p.kup,
    );

    let (svc, _h) = service(&card, &source);
    request(&svc, Request::Check).await.unwrap();
    let status = settle(&svc).await;
    assert_eq!(status.phase, Phase::Error);
    assert_eq!(status.last_error.unwrap().code, code::BAD_PACKAGE);
    assert!(!status.available.unwrap().downloaded);
    assert!(!card
        .layout
        .update_dir()
        .join("kamoder-0.2.0-update.kup")
        .exists());
    assert_eq!(
        error_code(request(&svc, INSTALL).await),
        code::NOT_DOWNLOADED
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_newer_release_supersedes_the_old_download() {
    let card = legacy_card("svc-supersede");
    let source = FakeSource::default();
    let (svc, _h) = service(&card, &source);
    downloaded(&svc, &card, &source, &Fixture::standard()).await;
    // A stale partial download from some earlier attempt.
    std::fs::write(
        card.layout
            .update_dir()
            .join("kamoder-0.1.5-update.kup.part"),
        b"x",
    )
    .unwrap();

    let newer = Fixture {
        version: "0.2.1",
        seq: Fixture::standard().seq + 10,
        ..Fixture::standard()
    };
    downloaded(&svc, &card, &source, &newer).await;
    let mut names: Vec<_> = std::fs::read_dir(card.layout.update_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.contains(".kup"))
        .collect();
    names.sort();
    assert_eq!(names, ["kamoder-0.2.1-update.kup"]);
}

#[tokio::test(flavor = "current_thread")]
async fn a_deleted_package_counts_as_not_downloaded() {
    let card = legacy_card("svc-flashed");
    let source = FakeSource::default();
    {
        let (svc, _h) = service(&card, &source);
        downloaded(&svc, &card, &source, &Fixture::standard()).await;
    }
    // `./x flash` removes packages, keeps state.json.
    std::fs::remove_file(card.layout.update_dir().join("kamoder-0.2.0-update.kup")).unwrap();
    let (svc, _h) = service(&card, &source);
    let status = svc.status();
    assert!(!status.available.unwrap().downloaded);
    assert_eq!(status.last_error, None);
    assert_eq!(saved(&card).downloaded, None);
    assert_eq!(
        error_code(request(&svc, INSTALL).await),
        code::NOT_DOWNLOADED
    );
}

// ----- install --------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn install_migrates_a_legacy_card_to_slot_b() {
    let card = legacy_card("svc-install");
    let source = FakeSource::default();
    let (svc, h) = service(&card, &source);
    let fx = Fixture::standard();
    downloaded(&svc, &card, &source, &fx).await;

    assert_eq!(request(&svc, INSTALL).await, Ok(None));
    let status = settle(&svc).await;
    assert_eq!(
        status.phase,
        Phase::AwaitingReboot,
        "{:?}",
        status.last_error
    );
    assert_eq!(status.progress, None);
    assert_eq!(status.available, None, "installed");

    let rec = record(&card);
    let b = rec.slot(Slot::B);
    assert!(b.bootable && !b.successful);
    assert_eq!(b.tries_left, crate::INSTALL_TRIES);
    assert_eq!(b.version.as_str(), "0.2.0");
    assert!(b.priority > rec.slot(Slot::A).priority);
    assert!(rec.slot(Slot::A).bootable && rec.slot(Slot::A).successful);
    let slot_b = card.layout.install_dir(Slot::B);
    assert_eq!(
        std::fs::read(slot_b.join("bin/web")).unwrap(),
        b"\x7fELF web binary"
    );
    assert!(slot_b.join(MANIFEST_NAME).is_file());
    // The legacy tree and the shared data are untouched.
    assert_eq!(
        std::fs::read(card.root.join("bin/web")).unwrap(),
        b"legacy web"
    );
    assert!(card.root.join("camera.conf").is_file());

    let saved = saved(&card);
    assert_eq!(saved.highest_seq, fx.seq);
    assert_eq!(saved.security_floor.unwrap().to_string(), "0.1.0");
    assert_eq!(saved.downloaded, None);
    assert!(!card
        .layout
        .update_dir()
        .join("kamoder-0.2.0-update.kup")
        .exists());

    let status = svc.status();
    assert_eq!(status.slots[1].version.as_deref(), Some("0.2.0"));
    assert!(status.slots[1].dir_present && status.slots[1].bootable);
    // Reboot after the delay.
    for _ in 0..100 {
        if h.reboots.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.reboots.load(Ordering::SeqCst), 1);
}

fn upload(card: &Card, fx: &Fixture) -> Request {
    let dir = card.tmp.path().join(format!("up-{}", fx.seq));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(card.layout.update_dir()).unwrap();
    std::fs::copy(
        fx.write_kup(&dir),
        card.layout.update_dir().join("upload.kup"),
    )
    .unwrap();
    Request::Install {
        source: update_proto::Source::Upload {
            path: update_proto::UPLOAD_PATH.into(),
        },
        force: false,
        reboot: false,
    }
}

fn with_force(req: Request) -> Request {
    match req {
        Request::Install { source, reboot, .. } => Request::Install {
            source,
            force: true,
            reboot,
        },
        other => other,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn upload_install_then_replay_is_refused() {
    let card = legacy_card("svc-upload");
    let source = FakeSource::default();
    let (svc, h) = service(&card, &source);
    let fx = Fixture::standard();
    let upload_file = card.layout.update_dir().join("upload.kup");

    assert_eq!(request(&svc, upload(&card, &fx)).await, Ok(None));
    let status = settle(&svc).await;
    assert_eq!(
        status.phase,
        Phase::AwaitingReboot,
        "{:?}",
        status.last_error
    );
    assert!(!upload_file.exists(), "the upload is deleted after install");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.reboots.load(Ordering::SeqCst), 0, "reboot: false");
    assert_eq!(saved(&card).highest_seq, fx.seq);

    // Same package again: replay, refused before answering.
    let code = error_code(request(&svc, upload(&card, &fx)).await);
    assert_eq!(code, "replay");
    assert!(!upload_file.exists(), "a refused upload is deleted");
    assert_eq!(svc.status().last_error.unwrap().code, "replay");
    // An older one too, even forced.
    let older = Fixture {
        seq: fx.seq - 1,
        ..Fixture::standard()
    };
    assert_eq!(
        error_code(request(&svc, with_force(upload(&card, &older))).await),
        "replay"
    );
    // Forced reinstall of the same seq is allowed.
    assert_eq!(
        request(&svc, with_force(upload(&card, &fx))).await,
        Ok(None)
    );
    let status = settle(&svc).await;
    assert_eq!(
        status.phase,
        Phase::AwaitingReboot,
        "{:?}",
        status.last_error
    );
}

#[tokio::test(flavor = "current_thread")]
async fn upload_paths_must_stay_in_update_dir() {
    let card = legacy_card("svc-paths");
    let source = FakeSource::default();
    let (svc, _h) = service(&card, &source);
    std::fs::write(card.root.join("outside.kup"), b"x").unwrap();
    for path in [
        "update/missing.kup",
        "outside.kup",
        "update/../outside.kup",
        "/etc/passwd",
        "update",
    ] {
        let req = Request::Install {
            source: update_proto::Source::Upload { path: path.into() },
            force: false,
            reboot: false,
        };
        assert_eq!(
            error_code(request(&svc, req).await),
            code::BAD_UPLOAD_PATH,
            "{path}"
        );
    }
    assert!(card.root.join("outside.kup").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn a_damaged_upload_fails_without_touching_slots() {
    let card = legacy_card("svc-damaged");
    let source = FakeSource::default();
    let (svc, _h) = service(&card, &source);
    std::fs::create_dir_all(card.layout.update_dir()).unwrap();
    std::fs::write(card.layout.update_dir().join("upload.kup"), b"not a tar").unwrap();
    let req = Request::Install {
        source: update_proto::Source::Upload {
            path: update_proto::UPLOAD_PATH.into(),
        },
        force: false,
        reboot: true,
    };
    let code = error_code(request(&svc, req).await);
    assert!(code == code::BAD_PACKAGE || code == "missing", "{code}");
    assert!(!card.layout.slots_dir().exists());
    assert_eq!(record(&card), Record::default_legacy());
}

#[tokio::test(flavor = "current_thread")]
async fn installs_are_refused_during_a_trial() {
    let card = legacy_card("svc-trial-install");
    let mut rec = bootctl::activate(&Record::default_legacy(), Slot::B, "0.2.0", 3);
    rec.last_booted = Some(Slot::B);
    store_record(&card, &rec);
    let source = FakeSource::default();
    let (svc, _h) = service(&card, &source);
    let status = svc.status();
    assert!(status.trial && !status.committed);
    assert_eq!(status.phase, Phase::Trial);
    assert_eq!(status.slot, update_proto::Slot::B);
    assert_eq!(
        error_code(request(&svc, upload(&card, &Fixture::standard())).await),
        code::TRIAL
    );
    // The debug commit ends the trial.
    assert_eq!(request(&svc, Request::Commit).await, Ok(None));
    assert!(record(&card).slot(Slot::B).successful);
    let status = svc.status();
    assert!(status.committed && !status.trial);
    assert_eq!(status.phase, Phase::Idle);
}

#[tokio::test(flavor = "current_thread")]
async fn rollback_needs_a_committed_other_slot() {
    let card = legacy_card("svc-rollback");
    let source = FakeSource::default();
    {
        let (svc, _h) = service(&card, &source);
        let code = error_code(request(&svc, Request::Rollback { reboot: false }).await);
        assert_eq!(code, code::NO_ROLLBACK_TARGET);
    }
    // Both committed, b running.
    let mut rec = bootctl::activate(&Record::default_legacy(), Slot::B, "0.2.0", 3);
    rec = bootctl::mark_successful(&rec, Slot::B);
    rec.last_booted = Some(Slot::B);
    store_record(&card, &rec);
    let (svc, h) = service(&card, &source);
    assert_eq!(
        request(&svc, Request::Rollback { reboot: true }).await,
        Ok(None)
    );
    let rec = record(&card);
    assert!(rec.slot(Slot::A).priority > rec.slot(Slot::B).priority);
    assert_eq!(svc.status().phase, Phase::AwaitingReboot);
    for _ in 0..100 {
        if h.reboots.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.reboots.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn auto_install_runs_once_inside_the_window() {
    let card = legacy_card("svc-auto");
    let source = FakeSource::default();
    let (svc, h) = service(&card, &source);
    downloaded(&svc, &card, &source, &Fixture::standard()).await;

    // Off by default.
    svc.maybe_auto_install().await;
    assert_eq!(settle(&svc).await.phase, Phase::Idle);

    let settings = Settings {
        auto_install: true,
        ..Settings::default()
    };
    assert_eq!(
        request(&svc, Request::SetSettings(settings)).await,
        Ok(None)
    );
    svc.maybe_auto_install().await;
    let status = settle(&svc).await;
    assert_eq!(
        status.phase,
        Phase::AwaitingReboot,
        "{:?}",
        status.last_error
    );
    for _ in 0..100 {
        if h.reboots.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.reboots.load(Ordering::SeqCst), 1);
}

// ----- settings, state, socket ----------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn settings_persist_and_are_validated() {
    let card = legacy_card("svc-settings");
    let source = FakeSource::default();
    let settings = Settings {
        channel: Channel::Beta,
        auto_check: false,
        auto_install: true,
        window: Some(update_proto::MaintenanceWindow {
            start_hour: 22,
            end_hour: 4,
        }),
        repo: "someone/fork".into(),
    };
    {
        let (svc, _h) = service(&card, &source);
        assert_eq!(svc.status().settings, Settings::default());
        assert_eq!(
            request(&svc, Request::SetSettings(settings.clone())).await,
            Ok(None)
        );
        for bad in [
            Settings {
                repo: "no-slash".into(),
                ..Settings::default()
            },
            Settings {
                repo: "a/b/c".into(),
                ..Settings::default()
            },
            Settings {
                repo: "../x".into(),
                ..Settings::default()
            },
            Settings {
                window: Some(update_proto::MaintenanceWindow {
                    start_hour: 1,
                    end_hour: 24,
                }),
                ..Settings::default()
            },
        ] {
            assert_eq!(
                error_code(request(&svc, Request::SetSettings(bad)).await),
                code::INVALID_SETTINGS
            );
        }
        assert_eq!(svc.status().settings, settings);
    }
    let (svc, _h) = service(&card, &source);
    assert_eq!(svc.status().settings, settings);
    assert_eq!(saved(&card).settings, settings);
}

#[tokio::test(flavor = "current_thread")]
async fn seed_repo_applies_only_without_saved_settings() {
    let card = legacy_card("svc-seed");
    let (mut config, _h) = config(&card, TrustedKeys::test_key(), &card.root);
    config.seed_repo = Some("seeded/repo".into());
    let svc = Service::new(config, FakeSource::default()).unwrap();
    assert_eq!(svc.status().settings.repo, "seeded/repo");
    svc.set_settings(Settings::default()).unwrap();
    let (mut config, _h) = self::config(&card, TrustedKeys::test_key(), &card.root);
    config.seed_repo = Some("seeded/repo".into());
    let svc = Service::new(config, FakeSource::default()).unwrap();
    assert_eq!(svc.status().settings.repo, DEFAULT_REPO);
}

#[tokio::test(flavor = "current_thread")]
async fn own_manifest_restores_the_replay_memory() {
    let card = legacy_card("svc-own");
    let fx = Fixture {
        seq: 1_770_000_000,
        ..Fixture::standard()
    };
    let p = publish(&fx, card.tmp.path());
    std::fs::write(card.root.join(MANIFEST_NAME), &p.manifest).unwrap();
    std::fs::write(card.root.join(SIGNATURE_NAME), &p.signature).unwrap();
    let (svc, _h) = service(&card, &FakeSource::default());
    assert_eq!(saved(&card).highest_seq, fx.seq);
    // An older package is a replay now.
    let older = Fixture {
        seq: fx.seq - 5,
        ..Fixture::standard()
    };
    assert_eq!(
        error_code(request(&svc, upload(&card, &older)).await),
        "replay"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn socket_round_trip() {
    let card = legacy_card("svc-socket");
    let (svc, _h) = service(&card, &FakeSource::default());
    let path = card.tmp.path().join("update.sock");
    let server = server::serve(&path, Arc::clone(&svc)).unwrap();
    let client = update_proto::Client::new(&path);
    let Response::Status(status) = client.status().await.unwrap() else {
        panic!("not a status");
    };
    assert_eq!(status.version, "0.1.0");
    let settings = Settings {
        auto_check: false,
        ..Settings::default()
    };
    assert_eq!(client.set_settings(settings).await.unwrap(), Response::Ok);
    assert!(!saved(&card).settings.auto_check);
    let Response::Error { code, .. } = client.download().await.unwrap() else {
        panic!("download without an update");
    };
    assert_eq!(code, code::NO_UPDATE);

    // Malformed lines get bad_request, the connection survives.
    let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    write.write_all(b"{\"op\":\"explode\"}\n").await.unwrap();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.contains("\"code\":\"bad_request\""), "{line}");
    line.clear();
    write.write_all(b"{\"op\":\"status\"}\n").await.unwrap();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("{\"result\":\"status\""), "{line}");

    drop(server);
    assert!(!path.exists());
}

#[test]
fn repo_names() {
    for good in ["dangerousplay/kamoder", "a-b/c_d.e", "A1/B2"] {
        assert!(valid_repo(good), "{good}");
    }
    for bad in [
        "", "x", "/x", "x/", "a/b/c", "a b/c", "../x", "./x", "a/..", "a/b?c",
    ] {
        assert!(!valid_repo(bad), "{bad}");
    }
}

// ----- health commit -------------------------------------------------------

/// A supervisor control socket answering `status` with `services`.
fn fake_supervisor(path: &Path, services: Arc<Mutex<Vec<ServiceStatus>>>) {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let services = Arc::clone(&services);
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut reader = BufReader::new(read);
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let request: sup::Request = serde_json::from_str(&line).unwrap();
                    assert_eq!(request, sup::Request::Status);
                    let response = sup::Response::Services {
                        services: services.lock().unwrap().clone(),
                    };
                    let mut out = serde_json::to_vec(&response).unwrap();
                    out.push(b'\n');
                    if write.write_all(&out).await.is_err() {
                        return;
                    }
                    line.clear();
                }
            });
        }
    });
}

/// A web server answering every request with `status_line`.
async fn fake_web(status_line: &'static str) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(format!("{status_line}\r\nContent-Length: 0\r\n\r\n").as_bytes())
                    .await;
            });
        }
    });
    addr
}

fn running(name: &str) -> ServiceStatus {
    ServiceStatus {
        name: name.into(),
        enabled: true,
        critical: false,
        state: State::Running,
        pid: Some(100),
        uptime_secs: Some(10),
        restarts: 0,
        last_exit_code: None,
    }
}

/// A card booted into a trial of slot b installed from `fx`.
fn trial_card(tag: &str, fx: &Fixture, tamper_manifest: bool) -> Card {
    let card = legacy_card(tag);
    let slot_b = card.layout.install_dir(Slot::B);
    std::fs::create_dir_all(slot_b.join("bin")).unwrap();
    std::fs::write(slot_b.join(VERSION_FILE), b"0.2.0\n").unwrap();
    let p = publish(fx, card.tmp.path());
    let mut manifest = p.manifest;
    if tamper_manifest {
        let at = manifest.len() / 2;
        manifest[at] ^= 0x01;
    }
    std::fs::write(slot_b.join(MANIFEST_NAME), manifest).unwrap();
    std::fs::write(slot_b.join(SIGNATURE_NAME), p.signature).unwrap();
    let mut rec = bootctl::activate(&Record::default_legacy(), Slot::B, "0.2.0", 3);
    rec.slot_mut(Slot::B).tries_left = 2;
    rec.last_booted = Some(Slot::B);
    store_record(&card, &rec);
    card
}

async fn trial_service(
    card: &Card,
    services: Vec<ServiceStatus>,
    web: Option<&'static str>,
) -> (Arc<Service<FakeSource>>, Arc<Mutex<Vec<ServiceStatus>>>) {
    let slot_b = card.layout.install_dir(Slot::B);
    let (mut config, _h) = config(card, TrustedKeys::test_key(), &slot_b);
    // Only the health loop: no scheduled checks against the empty source.
    config.schedule.first_delay = Duration::from_secs(3600);
    let services = Arc::new(Mutex::new(services));
    fake_supervisor(&config.supervisor_socket, Arc::clone(&services));
    if let Some(line) = web {
        config.web_addr = fake_web(line).await;
    }
    let svc = Service::new(config, FakeSource::default()).unwrap();
    tokio::spawn(Arc::clone(&svc).run());
    (svc, services)
}

#[tokio::test(flavor = "current_thread")]
async fn healthy_trial_commits_after_the_stable_period() {
    let card = trial_card("svc-health-ok", &Fixture::standard(), false);
    let services = vec![
        running("web"),
        running("update"),
        // Exited cleanly by design (on-failure, nothing to do here).
        ServiceStatus {
            state: State::Stopped,
            pid: None,
            last_exit_code: Some(0),
            ..running("ntp")
        },
        // Disabled in the config: ignored.
        ServiceStatus {
            enabled: false,
            state: State::Disabled,
            ..running("demo")
        },
    ];
    let (svc, _services) = trial_service(&card, services, Some("HTTP/1.1 200 OK")).await;
    let status = svc.status();
    assert_eq!(status.slot, update_proto::Slot::B);
    assert_eq!(status.version, "0.2.0");
    assert!(status.trial);

    let mut saw_counting = false;
    for _ in 0..300 {
        if record(&card).slot(Slot::B).successful {
            break;
        }
        if let Some(h) = svc.status().health {
            assert!(h.waiting_on.is_empty(), "{h:?}");
            saw_counting = true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let rec = record(&card);
    assert!(
        rec.slot(Slot::B).successful,
        "not committed: {:?}",
        svc.status()
    );
    assert_eq!(rec.slot(Slot::B).tries_left, 0);
    assert!(saw_counting);
    let status = svc.status();
    assert!(status.committed && !status.trial);
    assert_eq!(status.health, None);
    assert_eq!(status.phase, Phase::Idle);
}

#[tokio::test(flavor = "current_thread")]
async fn unhealthy_trial_does_not_commit() {
    let card = trial_card("svc-health-bad", &Fixture::standard(), false);
    let services = vec![
        running("web"),
        ServiceStatus {
            state: State::BackingOff,
            pid: None,
            last_exit_code: Some(1),
            ..running("rtsp")
        },
    ];
    // No web server at all.
    let (svc, services) = trial_service(&card, services, None).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!record(&card).slot(Slot::B).successful);
    let health = svc.status().health.unwrap();
    assert_eq!(health.ok_for_s, 0);
    assert!(
        health
            .waiting_on
            .contains(&"service:rtsp (backing_off)".to_owned()),
        "{health:?}"
    );
    assert!(health.waiting_on.contains(&"web".to_owned()), "{health:?}");
    assert_eq!(svc.status().phase, Phase::Trial);

    // The service recovers, but the web is still down: still no commit.
    services.lock().unwrap()[1] = running("rtsp");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!record(&card).slot(Slot::B).successful);
    assert_eq!(svc.status().health.unwrap().waiting_on, ["web"]);
}

#[tokio::test(flavor = "current_thread")]
async fn web_errors_and_a_bad_own_manifest_block_the_commit() {
    let card = trial_card("svc-health-manifest", &Fixture::standard(), true);
    let (svc, _s) = trial_service(&card, vec![running("web")], Some("HTTP/1.1 500 Oops")).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!record(&card).slot(Slot::B).successful);
    let waiting = svc.status().health.unwrap().waiting_on;
    assert!(waiting.contains(&"web".to_owned()), "{waiting:?}");
    assert!(
        waiting.iter().any(|w| w.starts_with("manifest: ")),
        "{waiting:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn without_a_key_the_manifest_check_is_skipped() {
    let card = trial_card("svc-health-nokey", &Fixture::standard(), true);
    let slot_b = card.layout.install_dir(Slot::B);
    let (mut config, _h) = config(&card, TrustedKeys::empty(), &slot_b);
    config.schedule.first_delay = Duration::from_secs(3600);
    fake_supervisor(
        &config.supervisor_socket,
        Arc::new(Mutex::new(vec![running("web")])),
    );
    config.web_addr = fake_web("HTTP/1.0 302 Found").await;
    let svc = Service::new(config, FakeSource::default()).unwrap();
    tokio::spawn(Arc::clone(&svc).run());
    for _ in 0..300 {
        if record(&card).slot(Slot::B).successful {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(record(&card).slot(Slot::B).successful, "{:?}", svc.status());
}
