use std::cell::RefCell;
use std::rc::Rc;

use ota_manifest::TrustedKeys;

use super::*;
use crate::kup::Kup;
use crate::progress::Progress;
use crate::test_support::{sha256, Fixture, TempDir};

const ERASE: u32 = 4096;

const PROC_MTD_V1: &str = "dev:    size   erasesize  name
mtd0: 00040000 00001000 \"UBOOT\"
mtd1: 00010000 00001000 \"ENV\"
mtd2: 00010000 00001000 \"KERNEL_A\"
mtd3: 00010000 00001000 \"KERNEL_B\"
";

/// Same partitions, renamed indices and order (a re-partitioned chip).
const PROC_MTD_V2: &str = "dev:    size   erasesize  name
mtd0: 00040000 00001000 \"UBOOT\"
mtd1: 00010000 00001000 \"KERNEL_B\"
mtd2: 00001000 00001000 \"ENV\"
mtd5: 00010000 00001000 \"KERNEL_A\"
mtd6: 00020000 00001000 \"KERNEL_A_OLD\"
";

fn image(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + 3) as u8).collect()
}

fn reporter_sink() -> impl FnMut(&Progress) {
    |_| {}
}

#[test]
fn parses_proc_mtd() {
    let parts = parse_proc_mtd(PROC_MTD_V1);
    assert_eq!(parts.len(), 4);
    assert_eq!(
        parts[2],
        MtdPartition {
            index: 2,
            name: "KERNEL_A".into(),
            size: 0x10000,
            erase_size: 0x1000,
        }
    );
    assert_eq!(parts[2].device(Path::new("/dev")), Path::new("/dev/mtd2"));
}

#[test]
fn finds_partitions_by_name_not_index() {
    let tmp = TempDir::new("mtd-find");
    let table = tmp.path().join("mtd");
    std::fs::write(&table, PROC_MTD_V1).unwrap();
    assert_eq!(find_partition(&table, "KERNEL_A").unwrap().index, 2);

    std::fs::write(&table, PROC_MTD_V2).unwrap();
    let a = find_partition(&table, "KERNEL_A").unwrap();
    assert_eq!((a.index, a.size), (5, 0x10000));
    assert_eq!(find_partition(&table, "KERNEL_B").unwrap().index, 1);
    // Exact match only: KERNEL_A_OLD is not KERNEL_A, and no prefix match.
    assert!(matches!(
        find_partition(&table, "KERNEL").unwrap_err(),
        Error::NotFound { .. }
    ));

    std::fs::write(
        &table,
        format!("{PROC_MTD_V2}mtd7: 00001000 00001000 \"ENV\"\n"),
    )
    .unwrap();
    assert!(matches!(
        find_partition(&table, "ENV").unwrap_err(),
        Error::Ambiguous { .. }
    ));
}

#[test]
fn slot_partition_names() {
    assert_eq!(
        slot_partition("KERNEL_{SLOT}", Slot::B).unwrap().as_deref(),
        Some("KERNEL_B")
    );
    assert_eq!(
        slot_partition("rootfs-{slot}", Slot::A).unwrap().as_deref(),
        Some("rootfs-a")
    );
    assert_eq!(
        slot_partition("KERNEL_A", Slot::A).unwrap().as_deref(),
        Some("KERNEL_A")
    );
    assert_eq!(slot_partition("KERNEL_A", Slot::B).unwrap(), None);
    assert_eq!(slot_partition("kernel-b", Slot::A).unwrap(), None);
    assert!(matches!(
        slot_partition("UBOOT", Slot::A).unwrap_err(),
        StoreError::SharedPartition { .. }
    ));
}

#[test]
fn writes_erases_and_verifies() {
    let tmp = TempDir::new("mtd-write");
    // Old contents all zero: any write without erase would fail.
    let mut flash = FileFlash::create(tmp.path().join("p"), 0x10000, ERASE, 1, 0x00).unwrap();
    let img = image(10_000); // not a multiple of the erase size
    let mut sink = reporter_sink();
    let mut rep = Reporter::new(img.len() as u64, &mut sink);
    write_image(
        &mut flash,
        "KERNEL_A",
        &mut img.as_slice(),
        img.len() as u64,
        &sha256(&img),
        &mut rep,
    )
    .unwrap();
    assert_eq!(rep.progress().bytes_done, img.len() as u64);
    assert_eq!(flash.erases, 16, "every block of the partition erased");
    let on_flash = std::fs::read(tmp.path().join("p")).unwrap();
    assert_eq!(&on_flash[..img.len()], img.as_slice());
    assert!(on_flash[img.len()..].iter().all(|&b| b == 0xFF));
}

#[test]
fn write_size_padding() {
    let tmp = TempDir::new("mtd-pad");
    let mut flash = FileFlash::create(tmp.path().join("p"), 0x4000, ERASE, 256, 0x00).unwrap();
    let img = image(5000);
    let mut sink = reporter_sink();
    let mut rep = Reporter::new(0, &mut sink);
    write_image(
        &mut flash,
        "X_A",
        &mut img.as_slice(),
        img.len() as u64,
        &sha256(&img),
        &mut rep,
    )
    .unwrap();
}

/// A flash whose erase does nothing (a driver/sequence bug).
struct NoErase(FileFlash);

impl Flash for NoErase {
    fn info(&self) -> FlashInfo {
        self.0.info()
    }
    fn erase(&mut self, _offset: u64, _len: u64) -> io::Result<()> {
        Ok(())
    }
    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.0.write(offset, data)
    }
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.0.read(offset, buf)
    }
}

#[test]
fn fake_catches_missing_erase() {
    let tmp = TempDir::new("mtd-noerase");
    let mut flash =
        NoErase(FileFlash::create(tmp.path().join("p"), 0x4000, ERASE, 1, 0x00).unwrap());
    let img = image(5000);
    let mut sink = reporter_sink();
    let mut rep = Reporter::new(0, &mut sink);
    let err = write_image(
        &mut flash,
        "K_A",
        &mut img.as_slice(),
        img.len() as u64,
        &sha256(&img),
        &mut rep,
    )
    .unwrap_err();
    assert!(
        matches!(
            &err,
            StoreError::Flash {
                action: "write",
                offset: 0,
                ..
            }
        ),
        "{err}"
    );
    assert!(err.to_string().contains("missing erase"), "{err}");
}

#[test]
fn fake_enforces_alignment() {
    let tmp = TempDir::new("mtd-align");
    let mut flash = FileFlash::create(tmp.path().join("p"), 0x4000, ERASE, 256, 0xFF).unwrap();
    assert_eq!(
        flash.erase(100, 4096).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        flash.write(0, &[0; 100]).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        flash.erase(0, 0x8000).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

/// A flash that flips a bit on read-back.
struct BadRead(FileFlash);

impl Flash for BadRead {
    fn info(&self) -> FlashInfo {
        self.0.info()
    }
    fn erase(&mut self, offset: u64, len: u64) -> io::Result<()> {
        self.0.erase(offset, len)
    }
    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.0.write(offset, data)
    }
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.0.read(offset, buf)?;
        if offset == 0 {
            buf[0] ^= 1;
        }
        Ok(())
    }
}

#[test]
fn read_back_mismatch_and_oversize_are_errors() {
    let tmp = TempDir::new("mtd-verify");
    let img = image(5000);
    let mut sink = reporter_sink();
    let mut rep = Reporter::new(0, &mut sink);
    let mut flash = BadRead(FileFlash::create(tmp.path().join("p"), 0x4000, ERASE, 1, 0).unwrap());
    let err = write_image(
        &mut flash,
        "K_A",
        &mut img.as_slice(),
        img.len() as u64,
        &sha256(&img),
        &mut rep,
    )
    .unwrap_err();
    assert!(matches!(err, StoreError::Verify { .. }), "{err}");

    let mut small = FileFlash::create(tmp.path().join("s"), 0x1000, ERASE, 1, 0).unwrap();
    let err = write_image(
        &mut small,
        "K_A",
        &mut img.as_slice(),
        img.len() as u64,
        &sha256(&img),
        &mut rep,
    )
    .unwrap_err();
    assert!(matches!(err, StoreError::ImageTooLarge { .. }), "{err}");
    assert_eq!(small.erases, 0, "nothing erased before the size check");
}

/// An MtdStore over FileFlash files `<dir>/mtdN.bin` and a table file;
/// records which partitions were opened.
fn store(dir: &Path, table: &str, booted: Slot) -> (MtdStore, Rc<RefCell<Vec<String>>>) {
    let proc_mtd = dir.join("proc_mtd");
    std::fs::write(&proc_mtd, table).unwrap();
    let opened = Rc::new(RefCell::new(Vec::new()));
    let log = opened.clone();
    let dir = dir.to_path_buf();
    let open: Opener = Box::new(move |p: &MtdPartition| {
        log.borrow_mut().push(p.name.clone());
        let f = FileFlash::create(
            dir.join(format!("mtd{}.bin", p.index)),
            p.size,
            p.erase_size as u32,
            1,
            0x00,
        )?;
        Ok(Box::new(f) as Box<dyn Flash>)
    });
    (MtdStore::new(booted, proc_mtd, open), opened)
}

#[test]
fn store_installs_image_to_partition_by_name() {
    let tmp = TempDir::new("mtd-store");
    let img = image(20_000);
    let mut fx = Fixture::standard().with_image("kernel", "KERNEL_{SLOT}", &img);
    fx.parts.remove(0); // MTD-only package
    let kup = Kup::open(fx.write_kup(tmp.path())).unwrap();
    let verified = kup.verify(&TrustedKeys::test_key()).unwrap();

    // Reordered table: KERNEL_B is mtd1 here.
    let (mut s, opened) = store(tmp.path(), PROC_MTD_V2, Slot::A);
    let mut last = Progress::default();
    s.install(Slot::B, &kup, &verified, &mut |p| last = p.clone())
        .unwrap();
    assert_eq!(*opened.borrow(), ["KERNEL_B"]);
    let flash = std::fs::read(tmp.path().join("mtd1.bin")).unwrap();
    assert_eq!(&flash[..img.len()], img.as_slice());
    assert_eq!(
        (last.bytes_done, last.bytes_total),
        (img.len() as u64, img.len() as u64)
    );

    assert!(matches!(
        s.install(Slot::A, &kup, &verified, &mut |_| {})
            .unwrap_err(),
        StoreError::BootedSlot { .. }
    ));
}

#[test]
fn store_skips_the_other_slots_images() {
    let tmp = TempDir::new("mtd-both");
    let (ia, ib) = (image(3000), image(5000));
    let mut fx = Fixture::standard()
        .with_image("kernel-a", "KERNEL_A", &ia)
        .with_image("kernel-b", "KERNEL_B", &ib);
    fx.parts.remove(0);
    let kup = Kup::open(fx.write_kup(tmp.path())).unwrap();
    let verified = kup.verify(&TrustedKeys::test_key()).unwrap();
    let (mut s, opened) = store(tmp.path(), PROC_MTD_V1, Slot::B);
    s.install(Slot::A, &kup, &verified, &mut |_| {}).unwrap();
    assert_eq!(*opened.borrow(), ["KERNEL_A"]);
    let flash = std::fs::read(tmp.path().join("mtd2.bin")).unwrap();
    assert_eq!(&flash[..ia.len()], ia.as_slice());
}

#[test]
fn store_checks_partitions_before_erasing() {
    let tmp = TempDir::new("mtd-preflight");
    // Fits KERNEL_x (64 KiB)? No: 70 000 bytes.
    let mut fx = Fixture::standard()
        .with_image("kernel", "KERNEL_{SLOT}", &image(70_000))
        .with_image("rootfs", "ROOTFS_{SLOT}", &image(10));
    fx.parts.remove(0);
    let kup = Kup::open(fx.write_kup(tmp.path())).unwrap();
    let verified = kup.verify(&TrustedKeys::test_key()).unwrap();
    let (mut s, opened) = store(tmp.path(), PROC_MTD_V1, Slot::A);
    let err = s
        .install(Slot::B, &kup, &verified, &mut |_| {})
        .unwrap_err();
    assert!(matches!(err, StoreError::ImageTooLarge { .. }), "{err}");
    assert!(
        opened.borrow().is_empty(),
        "nothing opened before preflight passes"
    );

    let mut fx = Fixture::standard().with_image("rootfs", "ROOTFS_{SLOT}", &image(10));
    fx.parts.remove(0);
    let kup = Kup::open(fx.write_kup(tmp.path())).unwrap();
    let verified = kup.verify(&TrustedKeys::test_key()).unwrap();
    let err = s
        .install(Slot::B, &kup, &verified, &mut |_| {})
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Mtd {
                source: Error::NotFound { .. }
            }
        ),
        "{err}"
    );
}

#[test]
fn store_rejects_tampered_image() {
    let tmp = TempDir::new("mtd-tamper");
    let img = image(9000);
    let mut fx = Fixture::standard().with_image("kernel", "KERNEL_{SLOT}", &img);
    fx.parts.remove(0);
    let path = fx.write_kup(tmp.path());
    let kup = Kup::open(&path).unwrap();
    let e = kup.entry("kernel.img").unwrap().clone();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[(e.offset + 100) as usize] ^= 0x55;
    std::fs::write(&path, &bytes).unwrap();
    let kup = Kup::open(&path).unwrap();
    let verified = kup.verify(&TrustedKeys::test_key()).unwrap();
    let (mut s, _) = store(tmp.path(), PROC_MTD_V1, Slot::A);
    let err = s
        .install(Slot::B, &kup, &verified, &mut |_| {})
        .unwrap_err();
    // The read-back check (against the manifest digest) fires first.
    assert!(
        matches!(err, StoreError::Verify { .. } | StoreError::Kup { .. }),
        "{err}"
    );
}
