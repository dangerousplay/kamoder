//! `SdSelector`: the record as two redundant files on the vfat SD card (OTA-07).
//!
//! `boot/sel.0` and `boot/sel.1`, 512 bytes each. The reader takes the valid
//! copy with the newest seq (RFC 1982 order, so the u32 may wrap). The writer
//! overwrites the *other* copy in place: no truncate, no rename (neither is
//! power-safe on vfat), `fsync` the file, then the directory. The copy holding
//! the current record is never opened for writing, so a power cut mid-write
//! leaves at worst a corrupt older copy and the current one intact.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use snafu::ResultExt;

use crate::control::BootControl;
use crate::record::{seq_newer, Record, RECORD_LEN};
use crate::{IoSnafu, Result};

/// File names of the two copies, relative to the selector directory.
pub const SELECTOR_FILES: [&str; 2] = ["sel.0", "sel.1"];

/// Result of [`SdSelector::load_detailed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The current record (legacy default when `fresh`).
    pub record: Record,
    /// Copy index (0 = `sel.0`) the record came from; `None` when fresh.
    pub copy: Option<usize>,
    /// No valid copy existed: `record` is [`Record::default_legacy`].
    pub fresh: bool,
    /// Which copies decoded successfully.
    pub valid: [bool; 2],
}

/// Redundant selector files in one directory (normally `<data>/boot`).
#[derive(Debug, Clone)]
pub struct SdSelector {
    dir: PathBuf,
    last_store_copy: Option<usize>,
}

impl SdSelector {
    pub fn new(dir: impl Into<PathBuf>) -> SdSelector {
        SdSelector {
            dir: dir.into(),
            last_store_copy: None,
        }
    }

    /// The selector directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of copy `index` (0 or 1).
    pub fn copy_path(&self, index: usize) -> PathBuf {
        self.dir.join(SELECTOR_FILES[index])
    }

    /// Copy index written by the last successful `store` (for tests/logs).
    pub fn last_store_copy(&self) -> Option<usize> {
        self.last_store_copy
    }

    /// Loads both copies and picks the current one. Missing, short or corrupt
    /// copies count as invalid; other I/O errors (EIO, EISDIR...) are returned,
    /// so a failing card is not mistaken for a fresh one and overwritten.
    pub fn load_detailed(&self) -> Result<Loaded> {
        let mut copies = [None, None];
        for (index, copy) in copies.iter_mut().enumerate() {
            *copy = self.read_copy(index)?;
        }
        let current = match copies {
            [Some(a), Some(b)] => Some(if seq_newer(b.seq, a.seq) { 1 } else { 0 }),
            [Some(_), None] => Some(0),
            [None, Some(_)] => Some(1),
            [None, None] => None,
        };
        let valid = [copies[0].is_some(), copies[1].is_some()];
        Ok(match current {
            Some(i) => Loaded {
                record: copies[i].expect("current copy is valid"),
                copy: Some(i),
                fresh: false,
                valid,
            },
            None => Loaded {
                record: Record::default_legacy(),
                copy: None,
                fresh: true,
                valid,
            },
        })
    }

    fn read_copy(&self, index: usize) -> Result<Option<Record>> {
        let path = self.copy_path(index);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Record::decode(&bytes).ok()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(source) => Err(source).context(IoSnafu {
                action: "read",
                path,
            }),
        }
    }

    fn write_copy(&self, index: usize, bytes: &[u8; RECORD_LEN]) -> Result<()> {
        let path = self.copy_path(index);
        let io = |action: &'static str| IoSnafu {
            action,
            path: path.clone(),
        };
        // No truncate: an existing 512-byte file is overwritten in place.
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .context(io("open"))?;
        let len = file.metadata().context(io("stat"))?.len();
        if len != RECORD_LEN as u64 {
            file.set_len(RECORD_LEN as u64).context(io("resize"))?;
        }
        file.seek(SeekFrom::Start(0)).context(io("seek"))?;
        file.write_all(bytes).context(io("write"))?;
        file.sync_all().context(io("fsync"))?;
        drop(file);
        sync_dir(&self.dir).context(IoSnafu {
            action: "fsync directory",
            path: self.dir.clone(),
        })
    }
}

/// `fsync` a directory so a newly created/resized entry is durable. Some
/// filesystems reject fsync on directories with EINVAL; nothing more can be
/// done there, so that is not an error.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    match File::open(dir).and_then(|d| d.sync_all()) {
        Err(e) if e.kind() == ErrorKind::InvalidInput => Ok(()),
        other => other,
    }
}

impl BootControl for SdSelector {
    fn load(&self) -> Result<Record> {
        Ok(self.load_detailed()?.record)
    }

    /// Writes `rec` with seq = current + 1 into the copy *not* holding the
    /// current record (`sel.0` when neither is valid).
    fn store(&mut self, rec: &Record) -> Result<Record> {
        let loaded = self.load_detailed()?;
        let (target, base) = match loaded.copy {
            Some(i) => (1 - i, loaded.record.seq),
            None => (0, rec.seq),
        };
        let stored = Record {
            seq: base.wrapping_add(1),
            ..*rec
        };
        self.write_copy(target, &stored.encode())?;
        self.last_store_copy = Some(target);
        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::activate;
    use crate::test_support::TempDir;
    use crate::{Error, Slot};

    fn write_raw(sel: &SdSelector, index: usize, rec: &Record) {
        std::fs::write(sel.copy_path(index), rec.encode()).unwrap();
    }

    fn with_seq(seq: u32) -> Record {
        Record {
            seq,
            ..Record::default_legacy()
        }
    }

    #[test]
    fn fresh_directory_is_legacy() {
        let tmp = TempDir::new("fresh");
        let sel = SdSelector::new(tmp.path());
        let l = sel.load_detailed().unwrap();
        assert!(l.fresh);
        assert_eq!(l.copy, None);
        assert_eq!(l.record, Record::default_legacy());
        assert_eq!(l.valid, [false, false]);
    }

    #[test]
    fn stores_alternate_and_bump_seq() {
        let tmp = TempDir::new("alternate");
        let mut sel = SdSelector::new(tmp.path());
        let mut expected_copy = 0;
        for seq in 1..=5 {
            let rec = sel.load().unwrap();
            let stored = sel.store(&rec).unwrap();
            assert_eq!(stored.seq, seq);
            assert_eq!(sel.last_store_copy(), Some(expected_copy));
            let l = sel.load_detailed().unwrap();
            assert_eq!(
                (l.copy, l.record, l.fresh),
                (Some(expected_copy), stored, false)
            );
            expected_copy = 1 - expected_copy;
        }
        for i in 0..2 {
            assert_eq!(std::fs::metadata(sel.copy_path(i)).unwrap().len(), 512);
        }
    }

    #[test]
    fn store_never_touches_current_copy_and_writes_in_place() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new("inplace");
        let mut sel = SdSelector::new(tmp.path());
        write_raw(&sel, 0, &with_seq(10));
        write_raw(&sel, 1, &with_seq(9));
        let ino = std::fs::metadata(sel.copy_path(1)).unwrap().ino();
        let current = std::fs::read(sel.copy_path(0)).unwrap();

        let rec = activate(&sel.load().unwrap(), Slot::B, "v2", 3);
        let stored = sel.store(&rec).unwrap();
        assert_eq!(stored.seq, 11);
        assert_eq!(sel.last_store_copy(), Some(1));
        assert_eq!(std::fs::read(sel.copy_path(0)).unwrap(), current);
        assert_eq!(std::fs::metadata(sel.copy_path(1)).unwrap().ino(), ino);
        assert_eq!(sel.load().unwrap(), stored);
    }

    #[test]
    fn torn_write_falls_back_to_older_copy() {
        let tmp = TempDir::new("torn");
        let mut sel = SdSelector::new(tmp.path());
        let first = sel.store(&Record::default_legacy()).unwrap(); // sel.0, seq 1
        let second = sel.store(&activate(&first, Slot::B, "v2", 3)).unwrap(); // sel.1, seq 2
        assert_eq!(sel.load().unwrap(), second);

        // Power cut while sel.1 was written: half new bytes, half garbage.
        let mut torn = std::fs::read(sel.copy_path(1)).unwrap();
        torn[300..].fill(0xFF);
        std::fs::write(sel.copy_path(1), &torn).unwrap();
        let l = sel.load_detailed().unwrap();
        assert_eq!((l.copy, l.record, l.valid), (Some(0), first, [true, false]));

        // The next write repairs the broken copy, keeping sel.0 as the fallback.
        let third = sel.store(&l.record).unwrap();
        assert_eq!(third.seq, 2);
        assert_eq!(sel.last_store_copy(), Some(1));

        // A truncated copy is invalid too.
        std::fs::write(sel.copy_path(1), [0u8; 100]).unwrap();
        assert_eq!(sel.load_detailed().unwrap().copy, Some(0));
        // And a re-store resizes it back to 512 bytes.
        sel.store(&first).unwrap();
        assert_eq!(std::fs::metadata(sel.copy_path(1)).unwrap().len(), 512);
        assert_eq!(sel.load_detailed().unwrap().copy, Some(1));
    }

    #[test]
    fn both_invalid_is_fresh_and_next_store_uses_sel0() {
        let tmp = TempDir::new("invalid");
        let mut sel = SdSelector::new(tmp.path());
        std::fs::write(sel.copy_path(0), [0xAAu8; 512]).unwrap();
        std::fs::write(sel.copy_path(1), b"garbage").unwrap();
        let l = sel.load_detailed().unwrap();
        assert!(l.fresh);
        assert_eq!(l.record, Record::default_legacy());
        sel.store(&l.record).unwrap();
        assert_eq!(sel.last_store_copy(), Some(0));
        assert_eq!(sel.load().unwrap().seq, 1);
    }

    #[test]
    fn seq_wraparound() {
        let tmp = TempDir::new("wrap");
        let mut sel = SdSelector::new(tmp.path());
        write_raw(&sel, 0, &with_seq(u32::MAX));
        write_raw(&sel, 1, &with_seq(u32::MAX - 1));
        assert_eq!(sel.load_detailed().unwrap().copy, Some(0));
        let stored = sel.store(&sel.load().unwrap()).unwrap();
        assert_eq!(stored.seq, 0);
        assert_eq!(sel.last_store_copy(), Some(1));
        // 0 is newer than u32::MAX.
        let l = sel.load_detailed().unwrap();
        assert_eq!((l.copy, l.record.seq), (Some(1), 0));
        sel.store(&l.record).unwrap();
        assert_eq!(sel.load_detailed().unwrap().copy, Some(0));
        assert_eq!(sel.load().unwrap().seq, 1);
    }

    #[test]
    fn io_errors_are_not_mistaken_for_fresh() {
        let tmp = TempDir::new("eisdir");
        let sel = SdSelector::new(tmp.path());
        std::fs::create_dir(sel.copy_path(0)).unwrap();
        assert!(matches!(sel.load_detailed(), Err(Error::Io { .. })));
    }

    #[test]
    fn select_through_trait_persists() {
        let tmp = TempDir::new("select");
        let mut sel = SdSelector::new(tmp.path());
        let c = sel.select().unwrap();
        assert_eq!((c.slot, c.record.seq), (Slot::A, 1));
        assert!(!sel.select().unwrap().changed);
        sel.activate(Slot::B, "v2", 3).unwrap();
        for _ in 0..3 {
            let c = sel.select().unwrap();
            assert_eq!((c.slot, c.trial), (Slot::B, true));
        }
        let c = sel.select().unwrap();
        assert_eq!((c.slot, c.trial), (Slot::A, false));
        assert!(!sel.load().unwrap().slot(Slot::B).bootable);
    }
}
