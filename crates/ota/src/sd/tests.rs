use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use bootctl::{BootControl, MemoryControl, Slot};
use ota_manifest::{PathError, TrustedKeys};

use super::*;
use crate::kup::Kup;
use crate::progress::Progress;
use crate::store::{install_to_inactive, Error, SlotStore};
use crate::test_support::{gzip, specs, tar_gz, Fixture, RawTar, TempDir};

/// A legacy card: `<tmp>/anyka_rust` with code in the root.
fn card(tag: &str) -> (TempDir, Layout) {
    let tmp = TempDir::new(tag);
    let root = tmp.path().join("anyka_rust");
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/web"), b"legacy web").unwrap();
    fs::write(root.join("camera.conf"), b"WIFI=x\n").unwrap();
    (tmp, Layout::new(root))
}

struct Run {
    result: Result<Slot>,
    control: MemoryControl,
    reports: Vec<Progress>,
    store: SdDirStore,
}

fn install(fx: &Fixture, layout: &Layout, booted: Slot) -> Run {
    install_with(fx, layout, booted, MemoryControl::new())
}

fn install_with(fx: &Fixture, layout: &Layout, booted: Slot, mut control: MemoryControl) -> Run {
    let dir = layout.data_root().parent().unwrap().join("dl");
    fs::create_dir_all(&dir).unwrap();
    let kup = Kup::open(fx.write_kup(&dir)).unwrap();
    let verified = kup.verify(&TrustedKeys::test_key()).unwrap();
    let mut store = SdDirStore::new(layout.clone(), booted);
    let mut reports = Vec::new();
    let result = install_to_inactive(&mut control, &mut store, &kup, &verified, &mut |p| {
        reports.push(p.clone())
    });
    Run {
        result,
        control,
        reports,
        store,
    }
}

fn read(path: impl AsRef<Path>) -> Vec<u8> {
    fs::read(path).unwrap()
}

/// The slot is invalid, no staging is left and the legacy tree is intact.
fn assert_failed_cleanly(run: &Run, layout: &Layout, slot: Slot) {
    assert!(run.result.is_err());
    assert!(!layout.staging_dir(slot).exists(), "staging left behind");
    let rec = run.control.load().unwrap();
    assert!(!rec.slot(slot).bootable, "failed slot must stay invalid");
    assert!(
        rec.slot(slot.other()).bootable,
        "booted slot must stay bootable"
    );
    assert_eq!(read(layout.data_root().join("bin/web")), b"legacy web");
}

#[test]
fn installs_legacy_card_into_slot_b() {
    let (_tmp, layout) = card("sd-good");
    let fx = Fixture::standard();
    let run = install(&fx, &layout, Slot::A);
    assert_eq!(run.result.unwrap(), Slot::B);

    let b = layout.install_dir(Slot::B);
    for (path, data, _) in &fx.parts[0].files {
        assert_eq!(&read(b.join(path)), data, "{path}");
    }
    assert_eq!(read(b.join(VERSION_FILE)), b"0.2.0\n");
    assert_eq!(read(b.join(MANIFEST_NAME)), fx.manifest().to_json());
    let sig = String::from_utf8(read(b.join(SIGNATURE_NAME))).unwrap();
    assert!(sig.contains("trusted comment: kamoder 0.2.0 seq=1760700000"));
    assert!(!layout.staging_dir(Slot::B).exists());
    // Legacy tree and shared data untouched; slot a still maps to the root.
    assert_eq!(read(layout.data_root().join("bin/web")), b"legacy web");
    assert_eq!(layout.slot_dir(Slot::A), layout.data_root());

    let rec = run.control.load().unwrap();
    let sb = rec.slot(Slot::B);
    assert!(sb.bootable && !sb.successful);
    assert_eq!(sb.tries_left, crate::store::INSTALL_TRIES);
    assert_eq!(sb.version.as_str(), "0.2.0");
    assert!(sb.priority > rec.slot(Slot::A).priority);
    assert!(rec.slot(Slot::A).bootable && rec.slot(Slot::A).successful);

    let last = run.reports.last().unwrap();
    assert_eq!(last.bytes_done, last.bytes_total);
    assert_eq!(
        last.bytes_total,
        fx.parts[0]
            .files
            .iter()
            .map(|f| f.1.len() as u64)
            .sum::<u64>()
    );
    assert!(run
        .reports
        .iter()
        .any(|p| p.file.as_deref() == Some("bin/web")));
}

#[test]
fn reinstall_replaces_the_old_slot_and_stale_staging() {
    let (_tmp, layout) = card("sd-replace");
    let old = layout.install_dir(Slot::B);
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("stale.txt"), b"old").unwrap();
    // An earlier install that was cut off.
    let staging = layout.staging_dir(Slot::B);
    fs::create_dir_all(staging.join("bin")).unwrap();
    fs::write(staging.join("bin/half"), b"x").unwrap();

    let run = install(&Fixture::standard(), &layout, Slot::A);
    run.result.unwrap();
    assert!(!old.join("stale.txt").exists());
    assert!(old.join("bin/web").exists());
    assert!(!staging.exists());
}

#[test]
fn second_update_writes_slots_a_and_leaves_the_root_alone() {
    let (_tmp, layout) = card("sd-migrate");
    install(&Fixture::standard(), &layout, Slot::A)
        .result
        .unwrap();
    // Booted from b now: the next update goes to slots/a, not the root.
    let mut fx = Fixture::standard();
    fx.version = "0.3.0";
    let run = install(&fx, &layout, Slot::B);
    assert_eq!(run.result.unwrap(), Slot::A);
    assert_eq!(layout.slot_kind(Slot::A), crate::layout::SlotKind::Dir);
    assert_eq!(
        read(layout.install_dir(Slot::A).join(VERSION_FILE)),
        b"0.3.0\n"
    );
    assert_eq!(read(layout.data_root().join("bin/web")), b"legacy web");
}

#[test]
fn corrupted_file_is_rejected() {
    let (_tmp, layout) = card("sd-corrupt");
    let mut fx = Fixture::standard();
    // Same size, different bytes: only the per-file sha256 can tell.
    let mut evil = fx.parts[0].files.clone();
    evil[0].1 = b"\x7fELF WEB BINARY".to_vec();
    fx.parts[0].payload = tar_gz(&evil);
    let run = install(&fx, &layout, Slot::A);
    assert!(
        matches!(&run.result, Err(Error::FileHash { path, .. }) if path == "bin/web"),
        "{:?}",
        run.result
    );
    assert_failed_cleanly(&run, &layout, Slot::B);
    assert!(!layout.install_dir(Slot::B).exists());
}

#[test]
fn failed_install_keeps_previous_slot_dir_but_invalid() {
    let (_tmp, layout) = card("sd-keep");
    install(&Fixture::standard(), &layout, Slot::A)
        .result
        .unwrap();
    // Running from b, a broken update for a (payload lacks files).
    let mut fx = Fixture::standard();
    fx.parts[0].payload = tar_gz(&specs(&[("bin/web", b"\x7fELF web binary", 0o755)]));
    let run = install(&fx, &layout, Slot::B);
    assert!(
        matches!(run.result, Err(Error::MissingFile { .. })),
        "{:?}",
        run.result
    );
    assert!(!layout.staging_dir(Slot::A).exists());
    assert!(!layout.install_dir(Slot::A).exists());
    // The booted slot b is untouched.
    assert!(layout.install_dir(Slot::B).join("bin/web").exists());
}

#[test]
fn unknown_file_is_rejected() {
    let (_tmp, layout) = card("sd-unknown");
    let mut fx = Fixture::standard();
    let mut extra = fx.parts[0].files.clone();
    extra.push(("bin/backdoor".into(), b"evil".to_vec(), 0o755));
    fx.parts[0].payload = tar_gz(&extra);
    let run = install(&fx, &layout, Slot::A);
    assert!(
        matches!(&run.result, Err(Error::UnknownFile { path, .. }) if path == "bin/backdoor"),
        "{:?}",
        run.result
    );
    assert_failed_cleanly(&run, &layout, Slot::B);
}

#[test]
fn symlink_entry_is_rejected() {
    let (_tmp, layout) = card("sd-symlink");
    let mut fx = Fixture::standard();
    let mut t = RawTar::default();
    t.symlink("bin/sh", "/bin/busybox");
    for (p, d, m) in &fx.parts[0].files {
        t.file(p, d, *m);
    }
    fx.parts[0].payload = gzip(&t.finish());
    let run = install(&fx, &layout, Slot::A);
    assert!(
        matches!(&run.result, Err(Error::EntryType { kind, .. }) if kind == "symlink"),
        "{:?}",
        run.result
    );
    assert_failed_cleanly(&run, &layout, Slot::B);
}

#[test]
fn traversal_path_is_rejected() {
    let (_tmp, layout) = card("sd-traversal");
    let mut fx = Fixture::standard();
    let mut t = RawTar::default();
    t.dir("bin/");
    t.file("../../escape", b"x", 0o644);
    fx.parts[0].payload = gzip(&t.finish());
    let run = install(&fx, &layout, Slot::A);
    assert!(
        matches!(
            &run.result,
            Err(Error::BadPath {
                source: PathError::DotDot,
                ..
            })
        ),
        "{:?}",
        run.result
    );
    assert_failed_cleanly(&run, &layout, Slot::B);
    assert!(!layout.data_root().parent().unwrap().join("escape").exists());
}

#[test]
fn truncated_payload_is_rejected() {
    let (_tmp, layout) = card("sd-truncated");
    let mut fx = Fixture::standard();
    // The member's digest matches the cut bytes (as if the builder signed a
    // broken file), so the gzip/tar decoder is what notices.
    let p = &mut fx.parts[0].payload;
    p.truncate(p.len() / 2);
    let run = install(&fx, &layout, Slot::A);
    assert!(
        matches!(run.result, Err(Error::Payload { .. })),
        "{:?}",
        run.result
    );
    assert_failed_cleanly(&run, &layout, Slot::B);
}

#[test]
fn interrupted_download_is_detected_by_component_digest() {
    let (_tmp, layout) = card("sd-interrupted");
    let mut fx = Fixture::standard();
    // Bytes after the gzip stream are never decoded, only hashed: damage
    // there is invisible to gzip/tar and must be caught by the component
    // digest (drained by `ComponentReader::finish`).
    fx.parts[0].payload.extend_from_slice(&[0u8; 16]);
    let dir = layout.data_root().parent().unwrap().join("dl");
    fs::create_dir_all(&dir).unwrap();
    let path = fx.write_kup(&dir);
    let kup = Kup::open(&path).unwrap();
    let e = kup.entry("payload.tar.gz").unwrap().clone();
    let mut bytes = read(&path);
    bytes[(e.offset + e.size - 1) as usize] ^= 0xFF;
    fs::write(&path, &bytes).unwrap();
    let kup = Kup::open(&path).unwrap();
    let verified = kup.verify(&TrustedKeys::test_key()).unwrap();
    let mut control = MemoryControl::new();
    let mut store = SdDirStore::new(layout.clone(), Slot::A);
    let err =
        install_to_inactive(&mut control, &mut store, &kup, &verified, &mut |_| {}).unwrap_err();
    assert!(
        matches!(
            err,
            Error::Kup {
                source: crate::kup::Error::ComponentDigest { .. }
            }
        ),
        "{err}"
    );
    assert!(!layout.staging_dir(Slot::B).exists());
    assert!(!control.load().unwrap().slot(Slot::B).bootable);
}

#[test]
fn booted_slot_is_refused() {
    let (_tmp, layout) = card("sd-booted");
    let fx = Fixture::standard();
    let dir = layout.data_root().parent().unwrap().join("dl");
    fs::create_dir_all(&dir).unwrap();
    let kup = Kup::open(fx.write_kup(&dir)).unwrap();
    let verified = kup.verify(&TrustedKeys::test_key()).unwrap();

    for booted in [Slot::A, Slot::B] {
        let mut store = SdDirStore::new(layout.clone(), booted);
        let err = store
            .install(booted, &kup, &verified, &mut |_| {})
            .unwrap_err();
        assert!(matches!(err, Error::BootedSlot { .. }), "{err}");
        let err = store.begin(booted, &verified).unwrap_err();
        assert!(matches!(err, Error::BootedSlot { .. }), "{err}");
    }
    assert!(!layout.slots_dir().exists());
}

#[test]
fn reserved_names_and_foreign_kinds_are_refused_before_writing() {
    let (_tmp, layout) = card("sd-reserved");
    let fx = Fixture::tree(&[("manifest.json", b"{}", 0o644)]);
    let run = install(&fx, &layout, Slot::A);
    assert!(
        matches!(run.result, Err(Error::ReservedPath { .. })),
        "{:?}",
        run.result
    );

    let fx = Fixture::standard().with_image("kernel", "KERNEL_{SLOT}", b"img");
    let run = install(&fx, &layout, Slot::A);
    assert!(
        matches!(run.result, Err(Error::UnsupportedComponent { .. })),
        "{:?}",
        run.result
    );
    assert!(!layout.staging_dir(Slot::B).exists());
}

#[test]
fn shipped_version_file_is_kept() {
    let (_tmp, layout) = card("sd-version");
    let fx = Fixture::tree(&[
        ("VERSION", b"0.2.0+build7\n", 0o644),
        ("bin/web", b"w", 0o755),
    ]);
    install(&fx, &layout, Slot::A).result.unwrap();
    assert_eq!(
        read(layout.install_dir(Slot::B).join("VERSION")),
        b"0.2.0+build7\n"
    );
}

const LIBC_V1: &[u8] = b"libc v1";
const KBOOT_V1: &[u8] = b"kboot v1";
const CONFIG_V1: &[u8] = b"#!/bin/sh\n# config v1\n";

fn shim_v1() -> Fixture {
    Fixture::standard().with_boot(&[
        ("boot/bin/kboot", KBOOT_V1, 0o755),
        ("boot/lib/sys/libc.so.0", LIBC_V1, 0o755),
        ("factory/config.sh", CONFIG_V1, 0o755),
    ])
}

#[test]
fn boot_shim_installs_in_dependency_order() {
    let (tmp, layout) = card("shim-first");
    let run = install(&shim_v1(), &layout, Slot::A);
    run.result.unwrap();
    assert_eq!(
        run.store.shim_replaced(),
        [
            "boot/lib/sys/libc.so.0",
            "boot/bin/kboot",
            "factory/config.sh"
        ]
    );
    assert_eq!(read(layout.boot_dir().join("bin/kboot")), KBOOT_V1);
    assert_eq!(read(layout.boot_dir().join("lib/sys/libc.so.0")), LIBC_V1);
    assert_eq!(read(tmp.path().join("Factory/config.sh")), CONFIG_V1);
    assert!(!layout.boot_dir().join("bin/kboot.new").exists());
    // Shim files are not slot content.
    assert!(!layout.install_dir(Slot::B).join("boot").exists());
}

#[test]
fn boot_shim_replaces_only_changed_files() {
    let (tmp, layout) = card("shim-changed");
    install(&shim_v1(), &layout, Slot::A).result.unwrap();
    let libc = layout.boot_dir().join("lib/sys/libc.so.0");
    let config = tmp.path().join("Factory/config.sh");
    let (libc_ino, config_ino) = (
        fs::metadata(&libc).unwrap().ino(),
        fs::metadata(&config).unwrap().ino(),
    );

    let mut fx = Fixture::standard().with_boot(&[
        ("boot/bin/kboot", b"kboot v2", 0o755),
        ("boot/lib/sys/libc.so.0", LIBC_V1, 0o755),
        ("factory/config.sh", CONFIG_V1, 0o755),
    ]);
    fx.version = "0.3.0";
    // b holds 0.2.0 now and is booted; the update goes to a.
    let run = install(&fx, &layout, Slot::B);
    run.result.unwrap();
    assert_eq!(run.store.shim_replaced(), ["boot/bin/kboot"]);
    assert_eq!(read(layout.boot_dir().join("bin/kboot")), b"kboot v2");
    assert_eq!(fs::metadata(&libc).unwrap().ino(), libc_ino);
    assert_eq!(fs::metadata(&config).unwrap().ino(), config_ino);
}

#[test]
fn corrupt_boot_shim_changes_nothing() {
    let (tmp, layout) = card("shim-corrupt");
    install(&shim_v1(), &layout, Slot::A).result.unwrap();
    let mut fx = Fixture::standard().with_boot(&[
        ("boot/bin/kboot", b"kboot v2", 0o755),
        ("factory/config.sh", b"#!/bin/sh\n# config v2\n", 0o755),
    ]);
    // config.sh content differs from what the manifest lists.
    fx.parts[1].payload = tar_gz(&specs(&[
        ("boot/bin/kboot", b"kboot v2", 0o755),
        ("factory/config.sh", b"#!/bin/sh\n# config XX\n", 0o755),
    ]));
    let run = install(&fx, &layout, Slot::B);
    assert!(
        matches!(&run.result, Err(Error::FileHash { path, .. }) if path == "factory/config.sh"),
        "{:?}",
        run.result
    );
    assert_eq!(read(layout.boot_dir().join("bin/kboot")), KBOOT_V1);
    assert_eq!(read(tmp.path().join("Factory/config.sh")), CONFIG_V1);
    assert!(!layout.boot_dir().join("bin/kboot.new").exists());
    assert!(!tmp.path().join("Factory/config.sh.new").exists());
    // The slot payload was fine but the install as a whole failed.
    assert!(!layout.install_dir(Slot::A).exists());
    assert!(!run.control.load().unwrap().slot(Slot::A).bootable);
}

#[test]
fn boot_shim_paths_outside_boot_and_factory_are_refused() {
    let (_tmp, layout) = card("shim-path");
    let fx = Fixture::standard().with_boot(&[("etc/passwd", b"root::0:0", 0o644)]);
    let run = install(&fx, &layout, Slot::A);
    assert!(
        matches!(run.result, Err(Error::ShimPath { .. })),
        "{:?}",
        run.result
    );
    assert!(!layout.data_root().join("etc").exists());
    assert_failed_cleanly(&run, &layout, Slot::B);
}

#[test]
fn needed_space_counts_files_and_margin() {
    let m = shim_v1().manifest();
    let files: u64 = m
        .components
        .iter()
        .flat_map(|c| &c.files)
        .map(|f| f.size)
        .sum();
    assert_eq!(space_needed(&m.components), files + SPACE_MARGIN);
}
