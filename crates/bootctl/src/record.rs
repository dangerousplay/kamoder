//! The selector record (design D-O3): fixed 512 bytes, little-endian.
//!
//! ```text
//! off  len  field
//!   0    4  magic "KSEL"
//!   4    2  format (u16) = 1
//!   6    4  seq (u32, RFC 1982 serial-number order)
//!  10   36  slot a: bootable u8 (0/1), successful u8 (0/1), tries_left u8,
//!               priority u8, version [u8; 32] (UTF-8, NUL-padded)
//!  46   36  slot b
//!  82    1  last_booted: 0 = none, 1 = a, 2 = b
//!  83  425  reserved, written as zeros
//! 508    4  crc32 (IEEE, crc32fast) over bytes 0..508
//! ```
//!
//! Fixed size so a rewrite is always an in-place overwrite of the same sectors
//! (vfat never reallocates clusters for it); one 512-byte sector so a torn
//! write is unlikely to leave half a record, and the CRC catches it if it does.
//! Reserved bytes are not checked on decode: a later minor addition can use them
//! without bumping `format`; incompatible changes bump `format`.

use std::fmt;

use snafu::Snafu;

use crate::Slot;

/// Encoded size of a record.
pub const RECORD_LEN: usize = 512;
/// Record magic.
pub const MAGIC: [u8; 4] = *b"KSEL";
/// Current (and only) record format.
pub const FORMAT: u16 = 1;
/// Bytes reserved for the version string.
pub const VERSION_LEN: usize = 32;

const SLOT_OFF: usize = 10;
const SLOT_LEN: usize = 4 + VERSION_LEN;
const LAST_BOOTED_OFF: usize = SLOT_OFF + 2 * SLOT_LEN;
const CRC_OFF: usize = RECORD_LEN - 4;

/// Why a 512-byte buffer is not a valid record.
#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum DecodeError {
    #[snafu(display("record is {len} bytes, expected {RECORD_LEN}"))]
    Length { len: usize },
    #[snafu(display("bad magic {found:02x?}"))]
    Magic { found: [u8; 4] },
    #[snafu(display("unsupported record format {found}"))]
    Format { found: u16 },
    #[snafu(display("crc mismatch: stored {stored:#010x}, computed {computed:#010x}"))]
    Crc { stored: u32, computed: u32 },
    #[snafu(display("field {field} out of range ({value})"))]
    Field { field: &'static str, value: u8 },
    #[snafu(display("slot {slot} version is not NUL-padded UTF-8"))]
    VersionText { slot: Slot },
}

/// A slot's version label: up to 32 bytes of UTF-8, stored NUL-padded.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Version([u8; VERSION_LEN]);

impl Version {
    /// Builds a version label, truncating at a character boundary to fit
    /// 32 bytes. Interior NULs end the label (they cannot be stored).
    pub fn new(text: &str) -> Version {
        let text = text.split('\0').next().unwrap_or("");
        let mut end = text.len().min(VERSION_LEN);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut bytes = [0u8; VERSION_LEN];
        bytes[..end].copy_from_slice(&text.as_bytes()[..end]);
        Version(bytes)
    }

    /// The label as text.
    pub fn as_str(&self) -> &str {
        let end = self.0.iter().position(|&b| b == 0).unwrap_or(VERSION_LEN);
        // Constructed only via `new` (valid UTF-8, truncated at a boundary) or
        // `from_bytes` (validated).
        std::str::from_utf8(&self.0[..end]).unwrap_or("")
    }

    /// The raw NUL-padded bytes.
    pub fn as_bytes(&self) -> &[u8; VERSION_LEN] {
        &self.0
    }

    /// Validates raw bytes: UTF-8 followed only by NUL padding.
    pub fn from_bytes(bytes: [u8; VERSION_LEN]) -> Option<Version> {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(VERSION_LEN);
        if bytes[end..].iter().any(|&b| b != 0) {
            return None;
        }
        std::str::from_utf8(&bytes[..end]).ok()?;
        Some(Version(bytes))
    }
}

impl fmt::Debug for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-slot boot state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlotState {
    /// The slot may be booted.
    pub bootable: bool,
    /// The slot has passed its health check at least once (committed).
    pub successful: bool,
    /// Boot attempts left while not successful (a trial).
    pub tries_left: u8,
    /// Higher boots first among bootable slots.
    pub priority: u8,
    /// Version label of the installed code.
    pub version: Version,
}

/// The persistent boot-control state. The sequence number is owned by the
/// storage backend: decision functions never touch it, `store` bumps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub seq: u32,
    /// Indexed by [`Slot::index`]; prefer [`Record::slot`].
    pub slots: [SlotState; 2],
    pub last_booted: Option<Slot>,
}

impl Record {
    /// The state a legacy (pre-OTA) card is in: the root tree is slot `a`,
    /// committed; slot `b` is empty. Used whenever no valid record exists.
    pub fn default_legacy() -> Record {
        Record {
            seq: 0,
            slots: [
                SlotState {
                    bootable: true,
                    successful: true,
                    tries_left: 0,
                    priority: 1,
                    version: Version::new("legacy"),
                },
                SlotState::default(),
            ],
            last_booted: None,
        }
    }

    /// State of one slot.
    pub fn slot(&self, slot: Slot) -> &SlotState {
        &self.slots[slot.index()]
    }

    /// Mutable state of one slot.
    pub fn slot_mut(&mut self, slot: Slot) -> &mut SlotState {
        &mut self.slots[slot.index()]
    }

    /// Encodes to the fixed 512-byte layout (CRC included).
    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let mut buf = [0u8; RECORD_LEN];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4..6].copy_from_slice(&FORMAT.to_le_bytes());
        buf[6..10].copy_from_slice(&self.seq.to_le_bytes());
        for slot in Slot::ALL {
            let s = self.slot(slot);
            let off = SLOT_OFF + slot.index() * SLOT_LEN;
            buf[off] = u8::from(s.bootable);
            buf[off + 1] = u8::from(s.successful);
            buf[off + 2] = s.tries_left;
            buf[off + 3] = s.priority;
            buf[off + 4..off + SLOT_LEN].copy_from_slice(s.version.as_bytes());
        }
        buf[LAST_BOOTED_OFF] = match self.last_booted {
            None => 0,
            Some(Slot::A) => 1,
            Some(Slot::B) => 2,
        };
        let crc = crc32fast::hash(&buf[..CRC_OFF]);
        buf[CRC_OFF..].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Decodes and validates (length, magic, format, CRC, field ranges).
    pub fn decode(buf: &[u8]) -> Result<Record, DecodeError> {
        snafu::ensure!(buf.len() == RECORD_LEN, LengthSnafu { len: buf.len() });
        let magic: [u8; 4] = buf[0..4].try_into().expect("4 bytes");
        snafu::ensure!(magic == MAGIC, MagicSnafu { found: magic });
        let format = u16::from_le_bytes([buf[4], buf[5]]);
        snafu::ensure!(format == FORMAT, FormatSnafu { found: format });
        let stored = u32::from_le_bytes(buf[CRC_OFF..].try_into().expect("4 bytes"));
        let computed = crc32fast::hash(&buf[..CRC_OFF]);
        snafu::ensure!(stored == computed, CrcSnafu { stored, computed });

        let flag = |field: &'static str, value: u8| match value {
            0 => Ok(false),
            1 => Ok(true),
            _ => FieldSnafu { field, value }.fail(),
        };
        let mut slots = [SlotState::default(); 2];
        for slot in Slot::ALL {
            let off = SLOT_OFF + slot.index() * SLOT_LEN;
            let version: [u8; VERSION_LEN] =
                buf[off + 4..off + SLOT_LEN].try_into().expect("32 bytes");
            slots[slot.index()] = SlotState {
                bootable: flag("bootable", buf[off])?,
                successful: flag("successful", buf[off + 1])?,
                tries_left: buf[off + 2],
                priority: buf[off + 3],
                version: Version::from_bytes(version).ok_or(DecodeError::VersionText { slot })?,
            };
        }
        let last_booted = match buf[LAST_BOOTED_OFF] {
            0 => None,
            1 => Some(Slot::A),
            2 => Some(Slot::B),
            value => {
                return FieldSnafu {
                    field: "last_booted",
                    value,
                }
                .fail()
            }
        };
        Ok(Record {
            seq: u32::from_le_bytes(buf[6..10].try_into().expect("4 bytes")),
            slots,
            last_booted,
        })
    }
}

/// RFC 1982 serial-number comparison: is `a` newer than `b`? Survives the u32
/// wrap (`0` is newer than `u32::MAX`). Exactly 2^31 apart is undefined by the
/// RFC; here neither is newer.
pub fn seq_newer(a: u32, b: u32) -> bool {
    a != b && (a.wrapping_sub(b) as i32) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Record {
        let mut rec = Record::default_legacy();
        rec.seq = 0xDEAD_BEEF;
        *rec.slot_mut(Slot::B) = SlotState {
            bootable: true,
            successful: false,
            tries_left: 3,
            priority: 2,
            version: Version::new("v1.2.3-rc.1"),
        };
        rec.last_booted = Some(Slot::B);
        rec
    }

    #[test]
    fn round_trip() {
        let rec = sample();
        let buf = rec.encode();
        assert_eq!(&buf[0..4], b"KSEL");
        assert_eq!(&buf[4..6], &[1, 0]);
        assert_eq!(&buf[6..10], &0xDEAD_BEEFu32.to_le_bytes());
        assert!(buf[83..508].iter().all(|&b| b == 0));
        assert_eq!(Record::decode(&buf), Ok(rec));
        let legacy = Record::default_legacy();
        assert_eq!(Record::decode(&legacy.encode()), Ok(legacy));
    }

    #[test]
    fn legacy_default() {
        let rec = Record::default_legacy();
        assert_eq!(rec.seq, 0);
        let a = rec.slot(Slot::A);
        assert!(a.bootable && a.successful);
        assert_eq!(a.priority, 1);
        assert_eq!(a.version.as_str(), "legacy");
        assert!(!rec.slot(Slot::B).bootable);
        assert_eq!(rec.last_booted, None);
    }

    #[test]
    fn every_single_bit_flip_is_rejected() {
        let buf = sample().encode();
        for byte in 0..RECORD_LEN {
            for bit in 0..8 {
                let mut bad = buf;
                bad[byte] ^= 1 << bit;
                assert!(
                    Record::decode(&bad).is_err(),
                    "flip at {byte}.{bit} accepted"
                );
            }
        }
    }

    #[test]
    fn crc_corruption_detected() {
        let mut buf = sample().encode();
        buf[20] ^= 0x40;
        assert!(matches!(Record::decode(&buf), Err(DecodeError::Crc { .. })));
    }

    /// Re-seal a hand-modified buffer so field validation (not the CRC) is tested.
    fn reseal(buf: &mut [u8; RECORD_LEN]) {
        let crc = crc32fast::hash(&buf[..CRC_OFF]);
        buf[CRC_OFF..].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn field_validation() {
        let good = sample().encode();
        assert_eq!(
            Record::decode(&good[..511]),
            Err(DecodeError::Length { len: 511 })
        );

        let mut bad = good;
        bad[0] = b'X';
        reseal(&mut bad);
        assert!(matches!(
            Record::decode(&bad),
            Err(DecodeError::Magic { .. })
        ));

        let mut bad = good;
        bad[4] = 2;
        reseal(&mut bad);
        assert_eq!(Record::decode(&bad), Err(DecodeError::Format { found: 2 }));

        let mut bad = good;
        bad[SLOT_OFF] = 2;
        reseal(&mut bad);
        assert!(matches!(
            Record::decode(&bad),
            Err(DecodeError::Field {
                field: "bootable",
                ..
            })
        ));

        let mut bad = good;
        bad[LAST_BOOTED_OFF] = 3;
        reseal(&mut bad);
        assert!(matches!(
            Record::decode(&bad),
            Err(DecodeError::Field {
                field: "last_booted",
                ..
            })
        ));

        // Garbage after the NUL terminator of a version.
        let mut bad = good;
        bad[SLOT_OFF + 4 + 30] = b'x';
        reseal(&mut bad);
        assert_eq!(
            Record::decode(&bad),
            Err(DecodeError::VersionText { slot: Slot::A })
        );

        // Invalid UTF-8.
        let mut bad = good;
        bad[SLOT_OFF + 4] = 0xFF;
        reseal(&mut bad);
        assert!(Record::decode(&bad).is_err());

        // Reserved bytes are ignored (forward compatible within format 1).
        let mut ok = good;
        ok[100] = 0xAA;
        reseal(&mut ok);
        assert_eq!(Record::decode(&ok), Ok(sample()));
    }

    #[test]
    fn version_truncates_at_char_boundary() {
        let v = Version::new(&"é".repeat(20)); // 40 bytes
        assert_eq!(v.as_str().len(), 32);
        assert_eq!(v.as_str(), "é".repeat(16));
        let v = Version::new(&format!("a{}", "é".repeat(20))); // boundary at 33
        assert_eq!(v.as_str().len(), 31);
        assert_eq!(Version::new("v1\0junk").as_str(), "v1");
        assert_eq!(Version::new("").as_str(), "");
    }

    #[test]
    fn serial_number_order() {
        assert!(seq_newer(2, 1));
        assert!(!seq_newer(1, 2));
        assert!(!seq_newer(5, 5));
        assert!(seq_newer(0, u32::MAX));
        assert!(seq_newer(3, u32::MAX - 3));
        assert!(!seq_newer(u32::MAX, 0));
        assert!(!seq_newer(0x8000_0000, 0) && !seq_newer(0, 0x8000_0000));
    }
}
