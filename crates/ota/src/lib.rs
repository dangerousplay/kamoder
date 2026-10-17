//! Over-the-air updates: update package reader, slot stores and installer
//! (OTA-06, OTA-20, OTA-22; design §1, §4, §5). The `update` service (bin,
//! design §6, [`service`]) drives this library from blocking tasks; everything
//! but [`service`] is synchronous, streaming and allocation-light (32 MB RAM
//! camera).
//!
//! ```text
//! Kup::open(path)                index tar members, no extraction
//!   .verify_and_check(keys, policy)  -> VerifiedManifest   (ota-manifest, OTA-02..04)
//! install_to_inactive(control, store, &kup, &verified, progress)
//!   SdDirStore   payload.tar.gz -> slots/<x>/, boot.tar.gz -> boot/ + Factory/
//!   MtdStore     *.img -> /dev/mtdN found by name in /proc/mtd (P2)
//! ```
//!
//! Modules: [`layout`] (paths on the card), [`kup`] (package reader),
//! [`store`] (the [`SlotStore`] trait, errors, install sequence), [`sd`],
//! [`shim`], [`mtd`] (backends), [`progress`]; [`service`] is the `update`
//! service (async, tokio) built on them.

pub mod kup;
pub mod layout;
pub mod mtd;
pub mod progress;
pub mod sd;
pub mod service;
pub mod shim;
pub mod store;

#[cfg(test)]
pub(crate) mod test_support;

pub use kup::{ComponentReader, Kup, KupEntry, VerifiedManifest};
pub use layout::{Layout, SlotKind};
pub use mtd::{FileFlash, Flash, FlashInfo, MtdDevice, MtdPartition, MtdStore};
pub use progress::Progress;
pub use sd::SdDirStore;
pub use store::{install_to_inactive, Error, SlotStore, INSTALL_TRIES};
