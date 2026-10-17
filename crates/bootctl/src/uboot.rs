//! U-Boot environment codec and the env-backed [`BootControl`] (P2, OTA-21).
//!
//! The vendor U-Boot 2019.10 keeps a 4 KiB environment on the NOR flash.
//! Two image layouts exist (`env/common.c`, `include/env_internal.h`):
//!
//! ```text
//! single:     crc32 LE (4) | data (size - 4)
//! redundant:  crc32 LE (4) | flags (1) | data (size - 5)      × 2 copies
//! data:       "name=value\0name=value\0...\0\0" then padding
//! ```
//!
//! The CRC (IEEE, same as crc32fast) covers the whole data area including the
//! padding. U-Boot pads with 0x00; an erased-NOR tail is 0xFF; both are
//! accepted on read, since the CRC already vouches for whatever is there.
//!
//! Redundant copies: the valid one wins; when both are valid the flags decide,
//! as in U-Boot's `env_import_redundant` and libubootenv: a higher flag is
//! newer, except that 0 beats 255 (the incrementing counter wrapped). The
//! "boolean" scheme (active = 1, obsolete = 0) is a special case of that rule.
//! Writes use the incrementing scheme: the non-current copy gets
//! `current.flags + 1` and the current copy is never touched.
//!
//! Storage is behind [`EnvStorage`] with whole-copy `write(copy, bytes)`. A
//! plain file works as is ([`FileEnvStorage`], host tests); raw `/dev/mtdN`
//! needs the erase block erased before writing (NOR only clears bits), which
//! is the `ota` crate's `Flash` trait's job, plugged in as another
//! `EnvStorage`. With a single (non-redundant) env a power cut during the write
//! loses the environment; U-Boot then falls back to its built-in default, and
//! so does [`UbootEnvControl`] (legacy record).

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use snafu::{ResultExt, Snafu};

use crate::control::BootControl;
use crate::record::{Record, SlotState, Version};
use crate::{EnvIoSnafu, EnvSnafu, EnvVarMissingSnafu, EnvVarSnafu, Result, Slot};

/// Size of the vendor U-Boot environment on the AK3918 boards (CONFIG_ENV_SIZE).
pub const DEFAULT_ENV_SIZE: usize = 4096;

/// Codec errors.
#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum EnvError {
    #[snafu(display("image is {len} bytes, expected {expected}"))]
    ImageSize { len: usize, expected: usize },
    #[snafu(display("crc mismatch: stored {stored:#010x}, computed {computed:#010x}"))]
    Crc { stored: u32, computed: u32 },
    #[snafu(display("data area has no terminating NUL"))]
    Unterminated,
    #[snafu(display("entry {index} is not UTF-8"))]
    NotUtf8 { index: usize },
    #[snafu(display("environment needs {needed} bytes, only {available} available"))]
    TooLarge { needed: usize, available: usize },
    #[snafu(display("invalid variable name {name:?}"))]
    InvalidName { name: String },
    #[snafu(display("value of {name} contains NUL"))]
    InvalidValue { name: String },
}

/// Single or redundant image layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvLayout {
    /// One copy: `crc | data`.
    Single,
    /// Two copies: `crc | flags | data`.
    Redundant,
}

impl EnvLayout {
    /// Header bytes before the data area.
    pub fn header_len(self) -> usize {
        match self {
            EnvLayout::Single => 4,
            EnvLayout::Redundant => 5,
        }
    }

    /// Number of copies.
    pub fn copies(self) -> usize {
        match self {
            EnvLayout::Single => 1,
            EnvLayout::Redundant => 2,
        }
    }
}

/// Environment variables (sorted, like `env export`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    vars: BTreeMap<String, String>,
}

impl Env {
    pub fn new() -> Env {
        Env::default()
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// Sets a variable. Names must be non-empty without `=` or NUL; values
    /// without NUL. (U-Boot deletes a variable set to the empty string; this
    /// codec stores it, callers that care use [`Env::remove`].)
    pub fn set(&mut self, name: &str, value: &str) -> Result<(), EnvError> {
        snafu::ensure!(
            !name.is_empty() && !name.contains(['=', '\0']),
            InvalidNameSnafu { name }
        );
        snafu::ensure!(!value.contains('\0'), InvalidValueSnafu { name });
        self.vars.insert(name.to_owned(), value.to_owned());
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Option<String> {
        self.vars.remove(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.vars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// Parses a data area (`name=value\0...\0\0` + padding). Entries without
    /// `=` are skipped (U-Boot's importer ignores them too); a later duplicate
    /// wins.
    pub fn parse_data(data: &[u8]) -> Result<Env, EnvError> {
        let mut env = Env::new();
        let mut pos = 0;
        let mut index = 0;
        while pos < data.len() && data[pos] != 0 {
            let len = data[pos..]
                .iter()
                .position(|&b| b == 0)
                .ok_or(EnvError::Unterminated)?;
            let entry = std::str::from_utf8(&data[pos..pos + len])
                .map_err(|_| EnvError::NotUtf8 { index })?;
            if let Some((name, value)) = entry.split_once('=') {
                if !name.is_empty() {
                    env.vars.insert(name.to_owned(), value.to_owned());
                }
            }
            pos += len + 1;
            index += 1;
        }
        Ok(env)
    }

    /// Serialises into `out` (the data area), padding with 0x00.
    pub fn write_data(&self, out: &mut [u8]) -> Result<(), EnvError> {
        let needed = self
            .vars
            .iter()
            .map(|(k, v)| k.len() + 1 + v.len() + 1)
            .sum::<usize>()
            + 1;
        snafu::ensure!(
            needed <= out.len(),
            TooLargeSnafu {
                needed,
                available: out.len()
            }
        );
        out.fill(0);
        let mut pos = 0;
        for (k, v) in &self.vars {
            for part in [k.as_bytes(), b"=", v.as_bytes(), b"\0"] {
                out[pos..pos + part.len()].copy_from_slice(part);
                pos += part.len();
            }
        }
        Ok(())
    }
}

/// A decoded image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvImage {
    pub env: Env,
    /// Flags byte (redundant layout only).
    pub flags: Option<u8>,
}

/// Decodes one image of `layout`; `buf.len()` is the env size.
pub fn decode_image(buf: &[u8], layout: EnvLayout) -> Result<EnvImage, EnvError> {
    let header = layout.header_len();
    snafu::ensure!(
        buf.len() > header,
        ImageSizeSnafu {
            len: buf.len(),
            expected: header + 1
        }
    );
    let stored = u32::from_le_bytes(buf[0..4].try_into().expect("4 bytes"));
    let data = &buf[header..];
    let computed = crc32fast::hash(data);
    snafu::ensure!(stored == computed, CrcSnafu { stored, computed });
    Ok(EnvImage {
        env: Env::parse_data(data)?,
        flags: (layout == EnvLayout::Redundant).then_some(buf[4]),
    })
}

/// Encodes an image of `size` bytes; `flags` is written only for the
/// redundant layout.
pub fn encode_image(
    env: &Env,
    layout: EnvLayout,
    size: usize,
    flags: u8,
) -> Result<Vec<u8>, EnvError> {
    let header = layout.header_len();
    snafu::ensure!(
        size > header,
        ImageSizeSnafu {
            len: size,
            expected: header + 1
        }
    );
    let mut buf = vec![0u8; size];
    env.write_data(&mut buf[header..])?;
    if layout == EnvLayout::Redundant {
        buf[4] = flags;
    }
    let crc = crc32fast::hash(&buf[header..]);
    buf[0..4].copy_from_slice(&crc.to_le_bytes());
    Ok(buf)
}

/// Which of two valid redundant copies is current, by flags (U-Boot
/// `env_import_redundant`): 0 beats 255 (wrap), otherwise higher wins, equal
/// → copy 0.
pub fn newer_copy(flags0: u8, flags1: u8) -> usize {
    match (flags0, flags1) {
        (0xFF, 0) => 1,
        (0, 0xFF) => 0,
        (a, b) if b > a => 1,
        _ => 0,
    }
}

/// Picks the current copy from per-copy decode results (`None` if none valid).
pub fn pick_copy(copies: &[Option<EnvImage>]) -> Option<usize> {
    match copies {
        [Some(_)] => Some(0),
        [Some(a), Some(b)] => Some(newer_copy(a.flags.unwrap_or(0), b.flags.unwrap_or(0))),
        [Some(_), None] => Some(0),
        [None, Some(_)] => Some(1),
        _ => None,
    }
}

/// Raw access to the environment copies. `write` replaces a whole copy; an
/// MTD implementation must erase first (see module docs).
pub trait EnvStorage {
    fn layout(&self) -> EnvLayout;
    /// Bytes per copy (CONFIG_ENV_SIZE).
    fn size(&self) -> usize;
    /// Reads copy `copy` (exactly `size()` bytes).
    fn read(&self, copy: usize) -> std::io::Result<Vec<u8>>;
    /// Writes copy `copy` (exactly `size()` bytes) durably.
    fn write(&mut self, copy: usize, image: &[u8]) -> std::io::Result<()>;
}

/// File-backed copies at (path, offset), like `fw_env.config` lines. Works for
/// plain files; for `/dev/mtdN` only if the region is already erased.
#[derive(Debug, Clone)]
pub struct FileEnvStorage {
    locations: Vec<(PathBuf, u64)>,
    size: usize,
}

impl FileEnvStorage {
    /// One copy at `offset` in `path`.
    pub fn single(path: impl Into<PathBuf>, offset: u64, size: usize) -> FileEnvStorage {
        FileEnvStorage {
            locations: vec![(path.into(), offset)],
            size,
        }
    }

    /// Two copies (may be the same file at different offsets).
    pub fn redundant(copies: [(PathBuf, u64); 2], size: usize) -> FileEnvStorage {
        FileEnvStorage {
            locations: copies.into(),
            size,
        }
    }
}

impl EnvStorage for FileEnvStorage {
    fn layout(&self) -> EnvLayout {
        if self.locations.len() == 2 {
            EnvLayout::Redundant
        } else {
            EnvLayout::Single
        }
    }

    fn size(&self) -> usize {
        self.size
    }

    fn read(&self, copy: usize) -> std::io::Result<Vec<u8>> {
        let (path, offset) = &self.locations[copy];
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(*offset))?;
        let mut buf = vec![0u8; self.size];
        file.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn write(&mut self, copy: usize, image: &[u8]) -> std::io::Result<()> {
        let (path, offset) = &self.locations[copy];
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.seek(SeekFrom::Start(*offset))?;
        file.write_all(image)?;
        file.sync_all()
    }
}

/// Env variable names.
pub mod vars {
    pub const SEQ: &str = "kamoder_seq";
    pub const SLOT: &str = "kamoder_slot";
    pub const TRIES: [&str; 2] = ["kamoder_tries_a", "kamoder_tries_b"];
    pub const OK: [&str; 2] = ["kamoder_ok_a", "kamoder_ok_b"];
    pub const BOOTABLE: [&str; 2] = ["kamoder_bootable_a", "kamoder_bootable_b"];
    pub const PRIO: [&str; 2] = ["kamoder_prio_a", "kamoder_prio_b"];
    pub const VER: [&str; 2] = ["kamoder_ver_a", "kamoder_ver_b"];
}

/// Writes `rec` into `env` as `kamoder_*` variables (other variables kept).
/// Empty values (no last slot, empty version) are removed, as U-Boot's
/// `setenv name` with no value would.
pub fn record_to_env(rec: &Record, env: &mut Env) {
    let mut put = |name: &str, value: String| {
        if value.is_empty() {
            env.remove(name);
        } else {
            env.set(name, &value)
                .expect("kamoder_* names and values are valid");
        }
    };
    put(vars::SEQ, rec.seq.to_string());
    put(
        vars::SLOT,
        rec.last_booted.map_or("", Slot::name).to_owned(),
    );
    for slot in Slot::ALL {
        let i = slot.index();
        let s = rec.slot(slot);
        put(vars::TRIES[i], s.tries_left.to_string());
        put(vars::OK[i], u8::from(s.successful).to_string());
        put(vars::BOOTABLE[i], u8::from(s.bootable).to_string());
        put(vars::PRIO[i], s.priority.to_string());
        put(vars::VER[i], s.version.as_str().to_owned());
    }
}

/// Reads the record from `kamoder_*` variables; `Ok(None)` when the env has
/// none (`kamoder_seq` absent), an error when present but malformed.
pub fn record_from_env(env: &Env) -> Result<Option<Record>> {
    if env.get(vars::SEQ).is_none() {
        return Ok(None);
    }
    fn num<T: std::str::FromStr>(env: &Env, name: &str) -> Result<T> {
        let value = env
            .get(name)
            .ok_or_else(|| EnvVarMissingSnafu { name }.build())?;
        value
            .parse()
            .map_err(|_| EnvVarSnafu { name, value }.build())
    }
    fn flag(env: &Env, name: &str) -> Result<bool> {
        match num::<u8>(env, name)? {
            0 => Ok(false),
            1 => Ok(true),
            v => EnvVarSnafu {
                name,
                value: v.to_string(),
            }
            .fail(),
        }
    }
    let last_booted = match env.get(vars::SLOT) {
        None | Some("") => None,
        Some(v) => Some(Slot::from_name(v).ok_or_else(|| {
            EnvVarSnafu {
                name: vars::SLOT,
                value: v,
            }
            .build()
        })?),
    };
    let mut slots = [SlotState::default(); 2];
    for slot in Slot::ALL {
        let i = slot.index();
        slots[i] = SlotState {
            bootable: flag(env, vars::BOOTABLE[i])?,
            successful: flag(env, vars::OK[i])?,
            tries_left: num(env, vars::TRIES[i])?,
            priority: num(env, vars::PRIO[i])?,
            version: Version::new(env.get(vars::VER[i]).unwrap_or("")),
        };
    }
    Ok(Some(Record {
        seq: num(env, vars::SEQ)?,
        slots,
        last_booted,
    }))
}

/// Result of [`UbootEnvControl::load_env`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedEnv {
    /// Current environment (empty when no copy is valid).
    pub env: Env,
    /// Copy it came from; `None` when no copy is valid.
    pub copy: Option<usize>,
    /// Its flags byte (redundant layout).
    pub flags: Option<u8>,
}

/// [`BootControl`] over a U-Boot environment.
#[derive(Debug, Clone)]
pub struct UbootEnvControl<S> {
    storage: S,
    last_store_copy: Option<usize>,
}

impl<S: EnvStorage> UbootEnvControl<S> {
    pub fn new(storage: S) -> Self {
        UbootEnvControl {
            storage,
            last_store_copy: None,
        }
    }

    pub fn storage(&self) -> &S {
        &self.storage
    }

    pub fn into_storage(self) -> S {
        self.storage
    }

    /// Copy index written by the last successful `store`.
    pub fn last_store_copy(&self) -> Option<usize> {
        self.last_store_copy
    }

    /// Reads all copies and picks the current one. Missing/short copies are
    /// invalid; other I/O errors are returned.
    pub fn load_env(&self) -> Result<LoadedEnv> {
        let layout = self.storage.layout();
        let mut images = Vec::with_capacity(layout.copies());
        for copy in 0..layout.copies() {
            let image = match self.storage.read(copy) {
                Ok(buf) => decode_image(&buf, layout).ok(),
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::UnexpectedEof) => {
                    None
                }
                Err(source) => {
                    return Err(source).context(EnvIoSnafu {
                        action: "read",
                        copy,
                    })
                }
            };
            images.push(image);
        }
        Ok(match pick_copy(&images) {
            Some(copy) => {
                let image = images.swap_remove(copy).expect("picked copy is valid");
                LoadedEnv {
                    env: image.env,
                    copy: Some(copy),
                    flags: image.flags,
                }
            }
            None => LoadedEnv {
                env: Env::new(),
                copy: None,
                flags: None,
            },
        })
    }
}

impl<S: EnvStorage> BootControl for UbootEnvControl<S> {
    fn load(&self) -> Result<Record> {
        let loaded = self.load_env()?;
        Ok(record_from_env(&loaded.env)?.unwrap_or_else(Record::default_legacy))
    }

    /// Updates the `kamoder_*` variables, keeping every other variable, with
    /// seq = current + 1. Redundant: writes the non-current copy with flags
    /// + 1. Single: rewrites the only copy.
    fn store(&mut self, rec: &Record) -> Result<Record> {
        let loaded = self.load_env()?;
        let current = record_from_env(&loaded.env).ok().flatten();
        let base = current.map_or(rec.seq, |r| r.seq);
        let stored = Record {
            seq: base.wrapping_add(1),
            ..*rec
        };
        let mut env = loaded.env;
        record_to_env(&stored, &mut env);
        let layout = self.storage.layout();
        let (target, flags) = match layout {
            EnvLayout::Single => (0, 0),
            EnvLayout::Redundant => (
                loaded.copy.map_or(0, |c| 1 - c),
                loaded.flags.map_or(1, |f| f.wrapping_add(1)),
            ),
        };
        let image = encode_image(&env, layout, self.storage.size(), flags).context(EnvSnafu)?;
        self.storage.write(target, &image).context(EnvIoSnafu {
            action: "write",
            copy: target,
        })?;
        self.last_store_copy = Some(target);
        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{activate, mark_successful};
    use crate::test_support::TempDir;
    use crate::Error;

    const SIZE: usize = DEFAULT_ENV_SIZE;

    /// In-memory storage; `None` copies read as missing.
    #[derive(Debug, Clone)]
    struct MemStorage {
        layout: EnvLayout,
        copies: Vec<Option<Vec<u8>>>,
        writes: Vec<usize>,
    }

    impl MemStorage {
        fn new(layout: EnvLayout) -> Self {
            MemStorage {
                layout,
                copies: vec![None; layout.copies()],
                writes: Vec::new(),
            }
        }
    }

    impl EnvStorage for MemStorage {
        fn layout(&self) -> EnvLayout {
            self.layout
        }
        fn size(&self) -> usize {
            SIZE
        }
        fn read(&self, copy: usize) -> std::io::Result<Vec<u8>> {
            self.copies[copy]
                .clone()
                .ok_or_else(|| ErrorKind::NotFound.into())
        }
        fn write(&mut self, copy: usize, image: &[u8]) -> std::io::Result<()> {
            assert_eq!(image.len(), SIZE);
            self.copies[copy] = Some(image.to_vec());
            self.writes.push(copy);
            Ok(())
        }
    }

    fn vendor_env() -> Env {
        let mut env = Env::new();
        env.set(
            "bootcmd",
            "sf probe 0:0; sf read 0x81000000 0x50000 0x1f0000; bootm",
        )
        .unwrap();
        env.set("bootargs", "console=ttySAK0,115200n8 root=/dev/mtdblock4")
            .unwrap();
        env.set("bootdelay", "0").unwrap();
        env.set("empty", "").unwrap();
        env
    }

    #[test]
    fn single_round_trip() {
        let env = vendor_env();
        let img = encode_image(&env, EnvLayout::Single, SIZE, 0).unwrap();
        assert_eq!(img.len(), SIZE);
        assert_eq!(&img[0..4], &crc32fast::hash(&img[4..]).to_le_bytes());
        assert!(img[4..].starts_with(b"bootargs=console"));
        let dec = decode_image(&img, EnvLayout::Single).unwrap();
        assert_eq!(dec, EnvImage { env, flags: None });
    }

    #[test]
    fn redundant_round_trip() {
        let env = vendor_env();
        let img = encode_image(&env, EnvLayout::Redundant, SIZE, 7).unwrap();
        assert_eq!(img[4], 7);
        assert_eq!(&img[0..4], &crc32fast::hash(&img[5..]).to_le_bytes());
        assert_eq!(
            decode_image(&img, EnvLayout::Redundant).unwrap(),
            EnvImage {
                env,
                flags: Some(7)
            }
        );
    }

    #[test]
    fn empty_env_round_trip() {
        let img = encode_image(&Env::new(), EnvLayout::Single, 64, 0).unwrap();
        assert!(decode_image(&img, EnvLayout::Single)
            .unwrap()
            .env
            .is_empty());
    }

    #[test]
    fn ff_padding_and_odd_entries_accepted() {
        let mut img = vec![0xFFu8; 64];
        let data = b"a=1\0junk\0b=x=y\0\0";
        img[4..4 + data.len()].copy_from_slice(data);
        let crc = crc32fast::hash(&img[4..]);
        img[0..4].copy_from_slice(&crc.to_le_bytes());
        let env = decode_image(&img, EnvLayout::Single).unwrap().env;
        assert_eq!(env.len(), 2);
        assert_eq!(env.get("a"), Some("1"));
        assert_eq!(env.get("b"), Some("x=y"));
    }

    #[test]
    fn crc_failure_detected() {
        let mut img = encode_image(&vendor_env(), EnvLayout::Single, SIZE, 0).unwrap();
        img[10] ^= 1;
        assert!(matches!(
            decode_image(&img, EnvLayout::Single),
            Err(EnvError::Crc { .. })
        ));
        // Padding is covered too.
        let mut img = encode_image(&vendor_env(), EnvLayout::Single, SIZE, 0).unwrap();
        img[SIZE - 1] = 0xFF;
        assert!(decode_image(&img, EnvLayout::Single).is_err());
    }

    #[test]
    fn malformed_data_rejected() {
        // No NUL at all (CRC valid).
        let mut img = vec![b'x'; 32];
        let crc = crc32fast::hash(&img[4..]);
        img[0..4].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            decode_image(&img, EnvLayout::Single),
            Err(EnvError::Unterminated)
        );
        assert!(matches!(
            decode_image(&[0; 4], EnvLayout::Single),
            Err(EnvError::ImageSize { .. })
        ));
    }

    #[test]
    fn too_large_and_invalid_names() {
        let mut env = Env::new();
        env.set("k", &"v".repeat(100)).unwrap();
        assert!(matches!(
            encode_image(&env, EnvLayout::Single, 64, 0),
            Err(EnvError::TooLarge { .. })
        ));
        assert!(env.set("a=b", "x").is_err());
        assert!(env.set("", "x").is_err());
        assert!(env.set("a", "x\0y").is_err());
        // Exactly fitting: "k=v\0" + final NUL = 5 bytes of data.
        let mut env = Env::new();
        env.set("k", "v").unwrap();
        let img = encode_image(&env, EnvLayout::Single, 9, 0).unwrap();
        assert_eq!(decode_image(&img, EnvLayout::Single).unwrap().env, env);
        assert!(encode_image(&env, EnvLayout::Single, 8, 0).is_err());
    }

    #[test]
    fn flags_selection() {
        assert_eq!(newer_copy(1, 0), 0); // boolean: active / obsolete
        assert_eq!(newer_copy(0, 1), 1);
        assert_eq!(newer_copy(5, 4), 0);
        assert_eq!(newer_copy(4, 5), 1);
        assert_eq!(newer_copy(0xFF, 0), 1); // wrapped counter
        assert_eq!(newer_copy(0, 0xFF), 0);
        assert_eq!(newer_copy(7, 7), 0);
    }

    #[test]
    fn redundant_pair_falls_back_on_crc_failure() {
        let mut env_old = vendor_env();
        env_old.set("v", "old").unwrap();
        let mut env_new = vendor_env();
        env_new.set("v", "new").unwrap();
        let mut st = MemStorage::new(EnvLayout::Redundant);
        st.copies[0] = Some(encode_image(&env_old, EnvLayout::Redundant, SIZE, 3).unwrap());
        st.copies[1] = Some(encode_image(&env_new, EnvLayout::Redundant, SIZE, 4).unwrap());
        let ctl = UbootEnvControl::new(st);
        let l = ctl.load_env().unwrap();
        assert_eq!(
            (l.copy, l.flags, l.env.get("v")),
            (Some(1), Some(4), Some("new"))
        );

        let mut st = ctl.into_storage();
        st.copies[1].as_mut().unwrap()[100] ^= 0xFF;
        let ctl = UbootEnvControl::new(st);
        let l = ctl.load_env().unwrap();
        assert_eq!((l.copy, l.env.get("v")), (Some(0), Some("old")));
    }

    #[test]
    fn record_env_mapping_round_trip() {
        let mut rec = activate(&Record::default_legacy(), Slot::B, "v2.0.0", 3);
        rec.seq = 77;
        rec.last_booted = Some(Slot::B);
        let mut env = vendor_env();
        record_to_env(&rec, &mut env);
        assert_eq!(env.get("kamoder_slot"), Some("b"));
        assert_eq!(env.get("kamoder_tries_b"), Some("3"));
        assert_eq!(env.get("kamoder_ok_b"), Some("0"));
        assert_eq!(env.get("kamoder_ok_a"), Some("1"));
        assert_eq!(env.get("kamoder_bootable_a"), Some("1"));
        assert_eq!(env.get("kamoder_prio_b"), Some("2"));
        assert_eq!(env.get("kamoder_ver_a"), Some("legacy"));
        assert_eq!(env.get("kamoder_ver_b"), Some("v2.0.0"));
        assert_eq!(env.get("kamoder_seq"), Some("77"));
        assert!(env.get("bootcmd").is_some());
        assert_eq!(record_from_env(&env).unwrap(), Some(rec));

        // Through the image codec too.
        let img = encode_image(&env, EnvLayout::Single, SIZE, 0).unwrap();
        let back = decode_image(&img, EnvLayout::Single).unwrap().env;
        assert_eq!(record_from_env(&back).unwrap(), Some(rec));

        // No last slot / empty version → variables removed, still round-trips.
        let mut rec = Record::default_legacy();
        rec.seq = 1;
        record_to_env(&rec, &mut env);
        assert_eq!(env.get("kamoder_slot"), None);
        assert_eq!(env.get("kamoder_ver_b"), None);
        assert_eq!(record_from_env(&env).unwrap(), Some(rec));

        assert_eq!(record_from_env(&vendor_env()).unwrap(), None);
    }

    #[test]
    fn malformed_record_vars_rejected() {
        let mut env = Env::new();
        record_to_env(&Record::default_legacy(), &mut env);
        let mut bad = env.clone();
        bad.set("kamoder_tries_a", "many").unwrap();
        assert!(matches!(record_from_env(&bad), Err(Error::EnvVar { .. })));
        let mut bad = env.clone();
        bad.set("kamoder_ok_b", "2").unwrap();
        assert!(matches!(record_from_env(&bad), Err(Error::EnvVar { .. })));
        let mut bad = env.clone();
        bad.set("kamoder_slot", "c").unwrap();
        assert!(matches!(record_from_env(&bad), Err(Error::EnvVar { .. })));
        let mut bad = env;
        bad.remove("kamoder_prio_a");
        assert!(matches!(
            record_from_env(&bad),
            Err(Error::EnvVarMissing { .. })
        ));
    }

    #[test]
    fn control_redundant_alternates_and_keeps_vendor_vars() {
        let mut st = MemStorage::new(EnvLayout::Redundant);
        st.copies[0] = Some(encode_image(&vendor_env(), EnvLayout::Redundant, SIZE, 0xFE).unwrap());
        let mut ctl = UbootEnvControl::new(st);
        assert_eq!(ctl.load().unwrap(), Record::default_legacy());

        let c = ctl.select().unwrap();
        assert_eq!((c.slot, c.record.seq), (Slot::A, 1));
        assert_eq!(ctl.last_store_copy(), Some(1));
        let l = ctl.load_env().unwrap();
        assert_eq!((l.copy, l.flags), (Some(1), Some(0xFF)));
        assert!(l.env.get("bootcmd").is_some());

        // Flags wrap 0xFF → 0 and the wrapped copy wins.
        ctl.activate(Slot::B, "v2", 3).unwrap();
        assert_eq!(ctl.last_store_copy(), Some(0));
        let l = ctl.load_env().unwrap();
        assert_eq!((l.copy, l.flags), (Some(0), Some(0)));
        assert_eq!(ctl.load().unwrap().seq, 2);

        let c = ctl.select().unwrap();
        assert_eq!((c.slot, c.trial), (Slot::B, true));
        assert_eq!(ctl.last_store_copy(), Some(1));
        ctl.mark_successful(Slot::B).unwrap();
        assert_eq!(ctl.storage().writes, vec![1, 0, 1, 0]);
        let rec = ctl.load().unwrap();
        assert_eq!(rec.seq, 4);
        assert!(rec.slot(Slot::B).successful);
        assert_eq!(rec, mark_successful(&rec, Slot::B));
    }

    #[test]
    fn control_torn_redundant_write_keeps_previous_record() {
        let mut ctl = UbootEnvControl::new(MemStorage::new(EnvLayout::Redundant));
        let first = ctl.select().unwrap().record; // copy 0
        ctl.activate(Slot::B, "v2", 3).unwrap(); // copy 1
        let mut st = ctl.into_storage();
        st.copies[1].as_mut().unwrap()[2000..].fill(0xFF);
        let ctl = UbootEnvControl::new(st);
        assert_eq!(ctl.load().unwrap(), first);
    }

    #[test]
    fn control_single_env() {
        let mut ctl = UbootEnvControl::new(MemStorage::new(EnvLayout::Single));
        assert_eq!(ctl.load().unwrap(), Record::default_legacy());
        ctl.select().unwrap();
        ctl.activate(Slot::B, "v2", 3).unwrap();
        assert_eq!(ctl.storage().writes, vec![0, 0]);
        let rec = ctl.load().unwrap();
        assert_eq!((rec.seq, rec.slot(Slot::B).tries_left), (2, 3));
        // Corrupt single env → legacy default (U-Boot uses its default env).
        let mut st = ctl.into_storage();
        st.copies[0].as_mut().unwrap()[50] ^= 1;
        assert_eq!(
            UbootEnvControl::new(st).load().unwrap(),
            Record::default_legacy()
        );
    }

    #[test]
    fn file_storage_redundant_in_one_file() {
        let tmp = TempDir::new("env");
        let path = tmp.path().join("mtd1.bin");
        std::fs::write(&path, vec![0xFFu8; 2 * SIZE]).unwrap();
        let st = FileEnvStorage::redundant([(path.clone(), 0), (path.clone(), SIZE as u64)], SIZE);
        let mut ctl = UbootEnvControl::new(st);
        assert_eq!(
            ctl.load_env().unwrap().copy,
            None,
            "erased flash is invalid"
        );
        ctl.select().unwrap();
        ctl.activate(Slot::B, "v2", 3).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 2 * SIZE as u64);
        let raw = std::fs::read(&path).unwrap();
        let c0 = decode_image(&raw[..SIZE], EnvLayout::Redundant).unwrap();
        let c1 = decode_image(&raw[SIZE..], EnvLayout::Redundant).unwrap();
        assert_eq!((c0.flags, c1.flags), (Some(1), Some(2)));
        let rec = ctl.load().unwrap();
        assert_eq!((rec.seq, rec.slot(Slot::B).version.as_str()), (2, "v2"));

        // Short file → copy missing, not an error.
        let short = tmp.path().join("short.bin");
        std::fs::write(&short, [0u8; 10]).unwrap();
        let ctl = UbootEnvControl::new(FileEnvStorage::single(&short, 0, SIZE));
        assert_eq!(ctl.load().unwrap(), Record::default_legacy());
    }
}
