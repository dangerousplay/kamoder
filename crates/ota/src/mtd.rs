//! `MtdStore`: raw images on NOR flash partitions (P2, OTA-20, OTA-22).
//!
//! The camera's 8 MB NOR (4 KiB erase blocks) is split by the kernel's
//! `mtdparts=` into `/dev/mtd0..7`. Indices change whenever the layout
//! does, so partitions are only ever found **by name** in `/proc/mtd`:
//!
//! ```text
//! dev:    size   erasesize  name
//! mtd0: 00040000 00001000 "UBOOT"
//! mtd2: 00200000 00001000 "KERNEL_A"
//! ```
//!
//! # Slot ↔ partition names
//!
//! An `mtd-image` component names its partition either with a placeholder
//! or with a slot suffix:
//!
//! - `"KERNEL_{SLOT}"` / `"kernel_{slot}"`: `{SLOT}` becomes `A`/`B`,
//!   `{slot}` becomes `a`/`b`, for the slot being installed;
//! - `"KERNEL_A"` (suffix `_A`, `_B`, `-a`, `-b`, any case): the image
//!   belongs to that slot, and is skipped when installing the other one
//!   (a package may carry both `kernel-a` and `kernel-b`).
//!
//! A name with neither is a partition shared by both slots (U-Boot, env);
//! writing it cannot be undone by a rollback, so it is refused.
//!
//! # Write sequence
//!
//! Per erase block: `MEMUNLOCK` (errors ignored, not every chip has locks),
//! `MEMERASE`, then one write of the block's data (padded with `0xFF` to the
//! write size). Blocks past the image are erased too, so a filesystem image
//! (JFFS2) never sees stale data after its end. Then the image is read back
//! and its SHA-256 compared with the manifest. RAM: one erase block.
//!
//! The ioctls are the kernel UAPI (`include/uapi/mtd/mtd-abi.h`), declared
//! with `nix`'s ioctl macros. [`FileFlash`] is a file-backed fake with NOR
//! semantics (a write can only clear bits), so a missing erase fails tests.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use bootctl::Slot;
use ota_manifest::{Component, ComponentKind, Digest};
use ring::digest::{Context, SHA256};
use snafu::{ensure, ResultExt, Snafu};

use crate::kup::{digest_of, ComponentReader, VerifiedManifest};
use crate::progress::Reporter;
use crate::store::{
    BootedSlotSnafu, Error as StoreError, ImageTooLargeSnafu, MtdSnafu, Result as StoreResult,
    SharedPartitionSnafu, SlotStore, VerifySnafu,
};

/// Default partition table.
pub const PROC_MTD: &str = "/proc/mtd";
/// Default directory of the `mtdN` character devices.
pub const DEV_DIR: &str = "/dev";

/// Partition lookup and device errors.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("cannot read {}: {source}", path.display()))]
    ReadTable { path: PathBuf, source: io::Error },
    #[snafu(display("no MTD partition named {name:?} (have {available:?})"))]
    NotFound {
        name: String,
        available: Vec<String>,
    },
    #[snafu(display("MTD partition name {name:?} appears more than once"))]
    Ambiguous { name: String },
    #[snafu(display("cannot open {}: {source}", path.display()))]
    OpenDevice { path: PathBuf, source: io::Error },
    #[snafu(display("{}: MEMGETINFO failed: {source}", path.display()))]
    GetInfo { path: PathBuf, source: nix::Error },
    #[snafu(display("{}: unusable geometry {info:?}", path.display()))]
    Geometry { path: PathBuf, info: FlashInfo },
}

/// `struct mtd_info_user` (mtd-abi.h). `u8 type` is followed by 3 bytes of
/// padding; `padding` is a `__u64` (8-aligned on ARM EABI and x86_64, so the
/// struct is 32 bytes, which the ioctl number encodes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct MtdInfoUser {
    pub type_: u8,
    pub flags: u32,
    /// Total size of the MTD device (the partition), bytes.
    pub size: u32,
    pub erasesize: u32,
    pub writesize: u32,
    pub oobsize: u32,
    pub padding: u64,
}

/// `struct erase_info_user` (mtd-abi.h), used by MEMERASE and MEMUNLOCK.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct EraseInfoUser {
    pub start: u32,
    pub length: u32,
}

const _: () = assert!(std::mem::size_of::<MtdInfoUser>() == 32);
const _: () = assert!(std::mem::size_of::<EraseInfoUser>() == 8);

mod ioctl {
    use super::{EraseInfoUser, MtdInfoUser};
    // #define MEMGETINFO _IOR('M', 1, struct mtd_info_user)
    nix::ioctl_read!(mem_get_info, b'M', 1, MtdInfoUser);
    // #define MEMERASE _IOW('M', 2, struct erase_info_user)
    nix::ioctl_write_ptr!(mem_erase, b'M', 2, EraseInfoUser);
    // #define MEMUNLOCK _IOW('M', 6, struct erase_info_user)
    nix::ioctl_write_ptr!(mem_unlock, b'M', 6, EraseInfoUser);
}

/// Geometry of a flash partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashInfo {
    /// Partition size, bytes.
    pub size: u64,
    /// Erase block size, bytes.
    pub erase_size: u32,
    /// Smallest write unit, bytes (1 on NOR).
    pub write_size: u32,
}

/// A flash partition. Offsets are relative to the partition start.
pub trait Flash {
    /// Geometry.
    fn info(&self) -> FlashInfo;
    /// Clears write protection on a range (no-op where unsupported).
    fn unlock(&mut self, _offset: u64, _len: u64) -> io::Result<()> {
        Ok(())
    }
    /// Erases `len` bytes at `offset` (both erase-block aligned) to `0xFF`.
    fn erase(&mut self, offset: u64, len: u64) -> io::Result<()>;
    /// Programs `data` at `offset` (write-size aligned, erased beforehand).
    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    /// Reads `buf.len()` bytes at `offset`.
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

fn erase_info(offset: u64, len: u64) -> io::Result<EraseInfoUser> {
    let conv = |v: u64| {
        u32::try_from(v).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset > 4 GiB"))
    };
    Ok(EraseInfoUser {
        start: conv(offset)?,
        length: conv(len)?,
    })
}

/// A real `/dev/mtdN` character device.
#[derive(Debug)]
pub struct MtdDevice {
    file: File,
    info: FlashInfo,
}

impl MtdDevice {
    /// Opens `path` read-write and reads its geometry (MEMGETINFO).
    pub fn open(path: impl AsRef<Path>) -> Result<MtdDevice, Error> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .context(OpenDeviceSnafu { path })?;
        let mut raw = MtdInfoUser::default();
        // SAFETY: MEMGETINFO writes one `mtd_info_user` into `raw`, whose
        // layout matches the kernel's (size asserted above); the fd is open.
        unsafe { ioctl::mem_get_info(file.as_raw_fd(), &mut raw) }
            .context(GetInfoSnafu { path })?;
        let info = FlashInfo {
            size: u64::from(raw.size),
            erase_size: raw.erasesize,
            write_size: raw.writesize.max(1),
        };
        ensure!(
            info.erase_size > 0 && info.erase_size.is_multiple_of(info.write_size),
            GeometrySnafu { path, info }
        );
        Ok(MtdDevice { file, info })
    }
}

impl Flash for MtdDevice {
    fn info(&self) -> FlashInfo {
        self.info
    }

    fn unlock(&mut self, offset: u64, len: u64) -> io::Result<()> {
        let arg = erase_info(offset, len)?;
        // SAFETY: MEMUNLOCK reads one `erase_info_user` from `arg`.
        unsafe { ioctl::mem_unlock(self.file.as_raw_fd(), &arg) }
            .map(drop)
            .map_err(io::Error::from)
    }

    fn erase(&mut self, offset: u64, len: u64) -> io::Result<()> {
        let arg = erase_info(offset, len)?;
        // SAFETY: MEMERASE reads one `erase_info_user` from `arg`; it blocks
        // until the erase is done.
        unsafe { ioctl::mem_erase(self.file.as_raw_fd(), &arg) }
            .map(drop)
            .map_err(io::Error::from)
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.write_all_at(data, offset)
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }
}

/// File-backed flash with NOR semantics, for tests: `erase` sets `0xFF`,
/// `write` may only clear bits (writing a 1 over a 0 means the block was not
/// erased, and fails), alignment is enforced.
#[derive(Debug)]
pub struct FileFlash {
    file: File,
    info: FlashInfo,
    /// Erase calls so far.
    pub erases: u64,
    /// Write calls so far.
    pub writes: u64,
}

impl FileFlash {
    /// Creates (or truncates) `path` as a partition of `size` bytes filled
    /// with `fill` (use `0x00` to model old, non-erased contents).
    pub fn create(
        path: impl AsRef<Path>,
        size: u64,
        erase_size: u32,
        write_size: u32,
        fill: u8,
    ) -> io::Result<FileFlash> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        let block = vec![fill; erase_size as usize];
        let mut off = 0;
        while off < size {
            let n = (size - off).min(u64::from(erase_size)) as usize;
            file.write_all_at(&block[..n], off)?;
            off += n as u64;
        }
        Ok(FileFlash {
            file,
            info: FlashInfo {
                size,
                erase_size,
                write_size,
            },
            erases: 0,
            writes: 0,
        })
    }

    /// Opens an existing backing file; its length is the partition size.
    pub fn open(path: impl AsRef<Path>, erase_size: u32, write_size: u32) -> io::Result<FileFlash> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(FileFlash {
            file,
            info: FlashInfo {
                size,
                erase_size,
                write_size,
            },
            erases: 0,
            writes: 0,
        })
    }

    fn check_range(&self, offset: u64, len: u64, align: u32, what: &str) -> io::Result<()> {
        let a = u64::from(align);
        if !offset.is_multiple_of(a) || !len.is_multiple_of(a) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{what} at {offset:#x}+{len:#x} not aligned to {a:#x}"),
            ));
        }
        if offset
            .checked_add(len)
            .is_none_or(|end| end > self.info.size)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{what} at {offset:#x}+{len:#x} past the end"),
            ));
        }
        Ok(())
    }
}

impl Flash for FileFlash {
    fn info(&self) -> FlashInfo {
        self.info
    }

    fn erase(&mut self, offset: u64, len: u64) -> io::Result<()> {
        self.check_range(offset, len, self.info.erase_size, "erase")?;
        let ff = vec![0xFF; len as usize];
        self.file.write_all_at(&ff, offset)?;
        self.erases += 1;
        Ok(())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.check_range(offset, data.len() as u64, self.info.write_size, "write")?;
        let mut old = vec![0u8; data.len()];
        self.file.read_exact_at(&mut old, offset)?;
        if let Some(i) = old.iter().zip(data).position(|(o, n)| n & !o != 0) {
            return Err(io::Error::other(format!(
                "NOR write would set bits at {:#x} (missing erase)",
                offset + i as u64
            )));
        }
        self.file.write_all_at(data, offset)?;
        self.writes += 1;
        Ok(())
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.check_range(offset, buf.len() as u64, 1, "read")?;
        self.file.read_exact_at(buf, offset)
    }
}

/// One line of `/proc/mtd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtdPartition {
    /// `N` of `mtdN`.
    pub index: u32,
    /// Partition name (without quotes).
    pub name: String,
    /// Size, bytes.
    pub size: u64,
    /// Erase block size, bytes.
    pub erase_size: u64,
}

impl MtdPartition {
    /// `<dev_dir>/mtdN`.
    pub fn device(&self, dev_dir: &Path) -> PathBuf {
        dev_dir.join(format!("mtd{}", self.index))
    }
}

/// Parses `/proc/mtd` (`mtdN: <size hex> <erasesize hex> "<name>"`);
/// the header and malformed lines are skipped.
pub fn parse_proc_mtd(text: &str) -> Vec<MtdPartition> {
    text.lines()
        .filter_map(|line| {
            let (dev, rest) = line.split_once(':')?;
            let index = dev.trim().strip_prefix("mtd")?.parse().ok()?;
            let mut fields = rest.trim().splitn(3, char::is_whitespace);
            let size = u64::from_str_radix(fields.next()?, 16).ok()?;
            let erase_size = u64::from_str_radix(fields.next()?.trim(), 16).ok()?;
            let name = fields.next()?.trim().strip_prefix('"')?.strip_suffix('"')?;
            Some(MtdPartition {
                index,
                name: name.to_owned(),
                size,
                erase_size,
            })
        })
        .collect()
}

/// Finds the partition called exactly `name` in the table at `proc_mtd`.
pub fn find_partition(proc_mtd: &Path, name: &str) -> Result<MtdPartition, Error> {
    let text = std::fs::read_to_string(proc_mtd).context(ReadTableSnafu { path: proc_mtd })?;
    let parts = parse_proc_mtd(&text);
    let mut hits = parts.iter().filter(|p| p.name == name);
    let found = hits.next().cloned();
    ensure!(hits.next().is_none(), AmbiguousSnafu { name });
    found.ok_or_else(|| Error::NotFound {
        name: name.to_owned(),
        available: parts.into_iter().map(|p| p.name).collect(),
    })
}

/// The partition `template` names for `slot` (see the module docs):
/// `Some(name)` to write, `None` when it belongs to the other slot.
pub fn slot_partition(template: &str, slot: Slot) -> StoreResult<Option<String>> {
    if template.contains("{slot}") || template.contains("{SLOT}") {
        let upper = slot.name().to_ascii_uppercase();
        return Ok(Some(
            template
                .replace("{slot}", slot.name())
                .replace("{SLOT}", &upper),
        ));
    }
    let lower = template.to_ascii_lowercase();
    for s in Slot::ALL {
        if lower.ends_with(&format!("_{}", s.name())) || lower.ends_with(&format!("-{}", s.name()))
        {
            return Ok((s == slot).then(|| template.to_owned()));
        }
    }
    SharedPartitionSnafu { name: template }.fail()
}

fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Writes `size` bytes from `image` to `flash` (see "Write sequence" in the
/// module docs) and verifies the read-back SHA-256 against `expected`.
pub fn write_image(
    flash: &mut dyn Flash,
    partition: &str,
    image: &mut dyn Read,
    size: u64,
    expected: &Digest,
    progress: &mut Reporter<'_>,
) -> StoreResult<()> {
    let info = flash.info();
    ensure!(
        size <= info.size,
        ImageTooLargeSnafu {
            partition,
            size,
            capacity: info.size
        }
    );
    let erase = u64::from(info.erase_size);
    let write = info.write_size.max(1) as usize;
    let mut buf = vec![0u8; info.erase_size as usize];
    let flash_err = |action: &'static str, offset: u64| {
        move |source: io::Error| StoreError::Flash {
            partition: partition.to_owned(),
            action,
            offset,
            source,
        }
    };

    let mut offset = 0u64;
    while offset < info.size {
        let len = erase.min(info.size - offset);
        let want = size.saturating_sub(offset).min(len) as usize;
        let n = if want > 0 {
            read_full(image, &mut buf[..want]).map_err(|source| StoreError::Payload {
                component: partition.to_owned(),
                source,
            })?
        } else {
            0
        };
        if n < want {
            return Err(StoreError::Payload {
                component: partition.to_owned(),
                source: io::Error::new(io::ErrorKind::UnexpectedEof, "image ended early"),
            });
        }
        if let Err(e) = flash.unlock(offset, len) {
            tracing::debug!(partition, offset, error = %e, "MEMUNLOCK failed (ignored)");
        }
        flash
            .erase(offset, len)
            .map_err(flash_err("erase", offset))?;
        if n > 0 {
            let padded = n.div_ceil(write) * write;
            buf[n..padded].fill(0xFF);
            flash
                .write(offset, &buf[..padded])
                .map_err(flash_err("write", offset))?;
            progress.advance(n as u64);
        }
        offset += len;
    }

    let mut ctx = Context::new(&SHA256);
    let mut offset = 0u64;
    while offset < size {
        let n = (size - offset).min(erase) as usize;
        flash
            .read(offset, &mut buf[..n])
            .map_err(flash_err("read back", offset))?;
        ctx.update(&buf[..n]);
        offset += n as u64;
    }
    let found = digest_of(ctx);
    ensure!(
        found == *expected,
        VerifySnafu {
            partition,
            expected: *expected,
            found
        }
    );
    Ok(())
}

/// Opens the flash of a partition.
pub type Opener = Box<dyn FnMut(&MtdPartition) -> io::Result<Box<dyn Flash>>>;

/// Installs `mtd-image` components to partitions found by name.
pub struct MtdStore {
    booted: Slot,
    proc_mtd: PathBuf,
    open: Opener,
}

impl std::fmt::Debug for MtdStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MtdStore")
            .field("booted", &self.booted)
            .field("proc_mtd", &self.proc_mtd)
            .finish_non_exhaustive()
    }
}

impl MtdStore {
    /// A store reading the partition table at `proc_mtd` and opening
    /// partitions with `open` (tests pass a [`FileFlash`] factory).
    pub fn new(booted: Slot, proc_mtd: impl Into<PathBuf>, open: Opener) -> MtdStore {
        MtdStore {
            booted,
            proc_mtd: proc_mtd.into(),
            open,
        }
    }

    /// The real thing: `/proc/mtd` and `/dev/mtdN`.
    pub fn system(booted: Slot) -> MtdStore {
        MtdStore::new(
            booted,
            PROC_MTD,
            Box::new(|p: &MtdPartition| {
                MtdDevice::open(p.device(Path::new(DEV_DIR)))
                    .map(|d| Box::new(d) as Box<dyn Flash>)
                    .map_err(io::Error::other)
            }),
        )
    }

    /// The partition `component` goes to when installing `slot`, `None` if
    /// it belongs to the other slot.
    pub fn resolve(&self, slot: Slot, component: &Component) -> StoreResult<Option<MtdPartition>> {
        let ComponentKind::MtdImage { partition } = &component.kind else {
            return Ok(None);
        };
        let Some(name) = slot_partition(partition, slot)? else {
            return Ok(None);
        };
        let part = find_partition(&self.proc_mtd, &name).context(MtdSnafu)?;
        ensure!(
            component.size <= part.size,
            ImageTooLargeSnafu {
                partition: name,
                size: component.size,
                capacity: part.size
            }
        );
        Ok(Some(part))
    }
}

impl SlotStore for MtdStore {
    fn booted(&self) -> Slot {
        self.booted
    }

    fn handles(&self, kind: &ComponentKind) -> bool {
        matches!(kind, ComponentKind::MtdImage { .. })
    }

    /// Resolves every image's partition before anything is erased.
    fn begin(&mut self, slot: Slot, manifest: &VerifiedManifest) -> StoreResult<()> {
        ensure!(slot != self.booted, BootedSlotSnafu { slot });
        for c in &manifest.manifest.components {
            self.resolve(slot, c)?;
        }
        Ok(())
    }

    fn install_component(
        &mut self,
        slot: Slot,
        component: &Component,
        reader: &mut ComponentReader<'_>,
        progress: &mut Reporter<'_>,
    ) -> StoreResult<()> {
        let Some(part) = self.resolve(slot, component)? else {
            tracing::debug!(component = %component.name, "image belongs to the other slot, skipped");
            progress.advance(component.size);
            return Ok(());
        };
        let mut flash = (self.open)(&part).map_err(|source| StoreError::Flash {
            partition: part.name.clone(),
            action: "open",
            offset: 0,
            source,
        })?;
        tracing::info!(partition = %part.name, index = part.index, size = component.size, "writing MTD image");
        let written = write_image(
            flash.as_mut(),
            &part.name,
            reader,
            component.size,
            &component.sha256,
            progress,
        );
        let written = match written {
            Err(StoreError::Payload { .. }) if reader.failure().is_some() => {
                Err(StoreError::from(reader.failure().expect("checked")))
            }
            other => other,
        };
        written?;
        reader.finish()?;
        Ok(())
    }

    fn finish(&mut self, _slot: Slot, _manifest: &VerifiedManifest) -> StoreResult<()> {
        Ok(())
    }

    /// Nothing to undo: the slot's partitions hold a partial image, and the
    /// slot stays invalid in the boot record.
    fn abort(&mut self, _slot: Slot) {}
}

#[cfg(test)]
mod tests;
