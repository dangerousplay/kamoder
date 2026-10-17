//! The storage-backend trait and an in-memory backend for tests.

use crate::machine::{self, Choice};
use crate::record::Record;
use crate::{NoRollbackTargetSnafu, Result, Slot};

/// Where the [`Record`] persists. Backends own the sequence number: `store`
/// writes `rec` with `seq` = current + 1 and returns what was written.
///
/// The provided helpers do load → pure transition → store, skipping the write
/// when nothing changed (SD/NOR wear).
pub trait BootControl {
    /// Current record; a backend with no valid record returns
    /// [`Record::default_legacy`] (seq 0) rather than failing.
    fn load(&self) -> Result<Record>;

    /// Persists `rec` with the next sequence number; returns the stored record.
    fn store(&mut self, rec: &Record) -> Result<Record>;

    /// Boot-time selection ([`machine::choose`]), persisted before returning.
    /// `Choice::record` carries the stored seq.
    fn select(&mut self) -> Result<Choice> {
        let rec = self.load()?;
        let mut choice = machine::choose(&rec);
        if choice.changed {
            choice.record = self.store(&choice.record)?;
        }
        Ok(choice)
    }

    /// [`machine::activate`], persisted.
    fn activate(&mut self, slot: Slot, version: &str, tries: u8) -> Result<Record> {
        let rec = self.load()?;
        self.store(&machine::activate(&rec, slot, version, tries))
    }

    /// [`machine::mark_successful`], persisted only if it changes anything.
    fn mark_successful(&mut self, slot: Slot) -> Result<Record> {
        let rec = self.load()?;
        let next = machine::mark_successful(&rec, slot);
        if next == rec {
            return Ok(rec);
        }
        self.store(&next)
    }

    /// [`machine::invalidate`], persisted only if it changes anything.
    fn invalidate(&mut self, slot: Slot) -> Result<Record> {
        let rec = self.load()?;
        let next = machine::invalidate(&rec, slot);
        if next == rec {
            return Ok(rec);
        }
        self.store(&next)
    }

    /// [`machine::rollback`], persisted; fails with `NoRollbackTarget`.
    fn rollback(&mut self) -> Result<Record> {
        let rec = self.load()?;
        let next = machine::rollback(&rec).ok_or_else(|| NoRollbackTargetSnafu.build())?;
        self.store(&next)
    }
}

/// In-memory backend for tests of code built on [`BootControl`].
#[derive(Debug, Clone, Default)]
pub struct MemoryControl {
    /// Stored record (`None` = nothing written yet → legacy default).
    pub record: Option<Record>,
    /// Number of `store` calls.
    pub writes: usize,
}

impl MemoryControl {
    pub fn new() -> MemoryControl {
        MemoryControl::default()
    }

    /// Starts from an already stored record.
    pub fn with(record: Record) -> MemoryControl {
        MemoryControl {
            record: Some(record),
            writes: 0,
        }
    }
}

impl BootControl for MemoryControl {
    fn load(&self) -> Result<Record> {
        Ok(self.record.unwrap_or_else(Record::default_legacy))
    }

    fn store(&mut self, rec: &Record) -> Result<Record> {
        let base = self.record.map_or(rec.seq, |r| r.seq);
        let stored = Record {
            seq: base.wrapping_add(1),
            ..*rec
        };
        self.record = Some(stored);
        self.writes += 1;
        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn lifecycle_through_trait() {
        let mut ctl = MemoryControl::new();
        let c = ctl.select().unwrap();
        assert_eq!((c.slot, c.trial, c.record.seq), (Slot::A, false, 1));
        assert!(!ctl.select().unwrap().changed);
        assert_eq!(ctl.writes, 1, "steady state does not write");

        ctl.invalidate(Slot::B).unwrap();
        assert_eq!(ctl.writes, 1, "already invalid: no write");
        ctl.activate(Slot::B, "v2", 3).unwrap();
        let c = ctl.select().unwrap();
        assert_eq!((c.slot, c.trial), (Slot::B, true));
        let rec = ctl.mark_successful(Slot::B).unwrap();
        assert!(rec.slot(Slot::B).successful);
        let writes = ctl.writes;
        ctl.mark_successful(Slot::B).unwrap();
        assert_eq!(ctl.writes, writes);
        assert_eq!(ctl.select().unwrap().slot, Slot::B);

        let rec = ctl.rollback().unwrap();
        assert_eq!(rec.slot(Slot::A).priority, 2);
        assert_eq!(ctl.select().unwrap().slot, Slot::A);
    }

    #[test]
    fn rollback_without_target_fails() {
        let mut ctl = MemoryControl::new();
        ctl.select().unwrap();
        assert!(matches!(ctl.rollback(), Err(Error::NoRollbackTarget)));
    }
}
