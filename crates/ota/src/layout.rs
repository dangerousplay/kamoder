//! Where things live on the SD card (design §1, D-O2).
//!
//! ```text
//! <sd root>/                       e.g. /mnt/disc1
//! ├── Factory/config.sh            boot shim, updated by the `boot-shim` component
//! └── <data root>/                 e.g. /mnt/disc1/anyka_rust (also the legacy slot)
//!     ├── boot/                    selector records, kboot + its libs (outside slots)
//!     ├── slots/a/  slots/b/       slot trees
//!     └── update/                  downloads, state
//! ```
//!
//! Slot `a` on a card that was never updated is the **legacy** tree: the data
//! root itself (`slots/a` absent, D-O2). The first update therefore goes to
//! slot `b`; once `b` is committed, the next one writes `slots/a`, and from
//! then on `a` maps to that directory. Installs always target
//! [`Layout::install_dir`] (`slots/<x>`), never the data root.

use std::path::{Path, PathBuf};

use bootctl::Slot;

/// How a slot is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// Slot `a` before migration: the code lives directly in the data root.
    Legacy,
    /// `slots/<x>/` directory.
    Dir,
}

/// Paths of one SD-card installation, all derived from the data root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    data_root: PathBuf,
}

impl Layout {
    /// `data_root` is the directory `init.sh` runs from (`$KAMODER_DATA`).
    pub fn new(data_root: impl Into<PathBuf>) -> Layout {
        Layout {
            data_root: data_root.into(),
        }
    }

    /// The data root (shared data, legacy slot `a`).
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    /// The card's root: the parent of the data root, home of `Factory/`.
    /// `None` only for a data root of `/` or a bare relative name.
    pub fn sd_root(&self) -> Option<&Path> {
        self.data_root
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
    }

    /// `<sd root>/Factory`, where the stock firmware looks for `config.sh`.
    pub fn factory_dir(&self) -> Option<PathBuf> {
        self.sd_root().map(|r| r.join("Factory"))
    }

    /// `<data root>/slots`.
    pub fn slots_dir(&self) -> PathBuf {
        self.data_root.join("slots")
    }

    /// `<data root>/boot`: selector records, `kboot`, its libraries.
    pub fn boot_dir(&self) -> PathBuf {
        self.data_root.join("boot")
    }

    /// `<data root>/update`: downloads and updater state.
    pub fn update_dir(&self) -> PathBuf {
        self.data_root.join("update")
    }

    /// How `slot` is stored right now: `a` is [`SlotKind::Legacy`] until
    /// `slots/a` exists; `b` is always a directory.
    pub fn slot_kind(&self, slot: Slot) -> SlotKind {
        match slot {
            Slot::A if !self.install_dir(Slot::A).is_dir() => SlotKind::Legacy,
            _ => SlotKind::Dir,
        }
    }

    /// The directory `slot`'s code is booted from (what `kboot` maps it to):
    /// the data root for a legacy `a`, `slots/<x>` otherwise.
    pub fn slot_dir(&self, slot: Slot) -> PathBuf {
        match self.slot_kind(slot) {
            SlotKind::Legacy => self.data_root.clone(),
            SlotKind::Dir => self.install_dir(slot),
        }
    }

    /// Where an install of `slot` ends up: always `slots/<x>`.
    pub fn install_dir(&self, slot: Slot) -> PathBuf {
        self.slots_dir().join(slot.name())
    }

    /// The staging directory of an install: `slots/<x>.tmp`.
    pub fn staging_dir(&self, slot: Slot) -> PathBuf {
        self.slots_dir().join(format!("{}.tmp", slot.name()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn legacy_a_maps_to_data_root_until_slots_a_exists() {
        let tmp = TempDir::new("layout");
        let root = tmp.path().join("anyka_rust");
        std::fs::create_dir_all(&root).unwrap();
        let l = Layout::new(&root);
        assert_eq!(l.slot_kind(Slot::A), SlotKind::Legacy);
        assert_eq!(l.slot_dir(Slot::A), root);
        assert_eq!(l.slot_kind(Slot::B), SlotKind::Dir);
        assert_eq!(l.slot_dir(Slot::B), root.join("slots/b"));
        assert_eq!(l.install_dir(Slot::A), root.join("slots/a"));

        std::fs::create_dir_all(root.join("slots/a")).unwrap();
        assert_eq!(l.slot_kind(Slot::A), SlotKind::Dir);
        assert_eq!(l.slot_dir(Slot::A), root.join("slots/a"));
    }

    #[test]
    fn derived_paths() {
        let l = Layout::new("/mnt/disc1/anyka_rust");
        assert_eq!(l.boot_dir(), Path::new("/mnt/disc1/anyka_rust/boot"));
        assert_eq!(l.update_dir(), Path::new("/mnt/disc1/anyka_rust/update"));
        assert_eq!(l.factory_dir().unwrap(), Path::new("/mnt/disc1/Factory"));
        assert_eq!(
            l.staging_dir(Slot::B),
            Path::new("/mnt/disc1/anyka_rust/slots/b.tmp")
        );
        assert_eq!(Layout::new("anyka_rust").sd_root(), None);
    }
}
