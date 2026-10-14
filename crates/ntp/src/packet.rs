//! The 48-byte SNTP packet (RFC 4330 §4) and the checks a reply must pass
//! before its timestamps are trusted (NTP-04).
//!
//! ```text
//!  0      LI(2) VN(3) Mode(3) | Stratum | Poll | Precision
//!  4      Root Delay
//!  8      Root Dispersion
//! 12      Reference Identifier
//! 16..24  Reference Timestamp
//! 24..32  Originate Timestamp   (the client's transmit, echoed)
//! 32..40  Receive Timestamp     (T2)
//! 40..48  Transmit Timestamp    (T3)
//! ```
//!
//! Times are kept as `i64` nanoseconds since the Unix epoch: good until 2262,
//! plain integer math on ARMv5, and no 64-bit atomics anywhere.

use snafu::Snafu;

/// Length of an SNTP packet without extensions or MAC.
pub const PACKET_LEN: usize = 48;
/// Seconds from the NTP epoch (1900-01-01) to the Unix epoch (1970-01-01).
pub const NTP_UNIX_OFFSET: i64 = 2_208_988_800;
/// Nanoseconds per second.
pub const NANOS: i64 = 1_000_000_000;

/// Client mode, sent in requests.
pub const MODE_CLIENT: u8 = 3;
/// Server mode, the only one accepted in replies.
pub const MODE_SERVER: u8 = 4;
/// Leap indicator "alarm": the server's clock is not synchronized.
pub const LEAP_UNSYNCHRONIZED: u8 = 3;
/// Version sent in requests.
pub const VERSION: u8 = 4;

/// A 64-bit NTP timestamp: seconds since 1900 and a binary fraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timestamp {
    pub seconds: u32,
    pub fraction: u32,
}

impl Timestamp {
    pub const ZERO: Timestamp = Timestamp {
        seconds: 0,
        fraction: 0,
    };

    pub fn from_bits(bits: u64) -> Self {
        Timestamp {
            seconds: (bits >> 32) as u32,
            fraction: bits as u32,
        }
    }

    pub fn to_bits(self) -> u64 {
        (u64::from(self.seconds) << 32) | u64::from(self.fraction)
    }

    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    /// Encodes Unix nanoseconds. The seconds wrap modulo 2^32, which is how
    /// NTP era 1 (from 2036-02-07) is written on the wire.
    pub fn from_unix_nanos(nanos: i64) -> Self {
        let secs = nanos.div_euclid(NANOS);
        let sub = nanos.rem_euclid(NANOS) as u64;
        Timestamp {
            seconds: (secs + NTP_UNIX_OFFSET).rem_euclid(1 << 32) as u32,
            fraction: ((sub << 32) / NANOS as u64) as u32,
        }
    }

    /// Decodes to Unix nanoseconds. The era is guessed the RFC 4330 §3 way:
    /// a clear top bit means era 1 (2036 onwards), so the range covered is
    /// 1968-01-20 to 2104-02-26.
    pub fn to_unix_nanos(self) -> i64 {
        let era_base: i64 = if self.seconds & 0x8000_0000 == 0 {
            1 << 32
        } else {
            0
        };
        let secs = i64::from(self.seconds) + era_base - NTP_UNIX_OFFSET;
        let sub = ((u64::from(self.fraction) * NANOS as u64) >> 32) as i64;
        secs * NANOS + sub
    }
}

/// An SNTP packet, header fields decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Packet {
    pub leap: u8,
    pub version: u8,
    pub mode: u8,
    pub stratum: u8,
    pub poll: i8,
    pub precision: i8,
    pub root_delay: u32,
    pub root_dispersion: u32,
    pub reference_id: [u8; 4],
    pub reference: Timestamp,
    pub originate: Timestamp,
    pub receive: Timestamp,
    pub transmit: Timestamp,
}

#[derive(Debug, Snafu, PartialEq, Eq)]
pub enum DecodeError {
    #[snafu(display("reply is {len} bytes, an SNTP packet is at least {PACKET_LEN}"))]
    Short { len: usize },
}

/// Why a reply was not trusted (NTP-04).
#[derive(Debug, Snafu, PartialEq, Eq)]
pub enum Reject {
    #[snafu(display("server is unsynchronized (leap indicator 3)"))]
    Unsynchronized,
    #[snafu(display("mode {mode}, not a server reply"))]
    Mode { mode: u8 },
    #[snafu(display("kiss-o'-death {code:?} (stratum 0)"))]
    KissOfDeath { code: String },
    #[snafu(display("stratum {stratum} is outside 1..=15"))]
    Stratum { stratum: u8 },
    #[snafu(display("transmit timestamp is zero"))]
    ZeroTransmit,
    #[snafu(display("originate timestamp does not match the request"))]
    Originate,
    #[snafu(display("claims Unix time {unix}, before the build floor {floor}"))]
    BeforeFloor { unix: i64, floor: i64 },
}

impl Packet {
    /// A client request whose transmit field carries `cookie`. The server
    /// copies it into the reply's originate field, which is how a reply is
    /// matched to this request; the real send time stays local.
    pub fn request(cookie: Timestamp) -> Self {
        Packet {
            leap: 0,
            version: VERSION,
            mode: MODE_CLIENT,
            transmit: cookie,
            ..Packet::default()
        }
    }

    pub fn encode(&self) -> [u8; PACKET_LEN] {
        let mut buf = [0u8; PACKET_LEN];
        buf[0] = (self.leap & 0b11) << 6 | (self.version & 0b111) << 3 | (self.mode & 0b111);
        buf[1] = self.stratum;
        buf[2] = self.poll as u8;
        buf[3] = self.precision as u8;
        buf[4..8].copy_from_slice(&self.root_delay.to_be_bytes());
        buf[8..12].copy_from_slice(&self.root_dispersion.to_be_bytes());
        buf[12..16].copy_from_slice(&self.reference_id);
        let stamps = [self.reference, self.originate, self.receive, self.transmit];
        for (i, stamp) in stamps.iter().enumerate() {
            let at = 16 + i * 8;
            buf[at..at + 8].copy_from_slice(&stamp.to_bits().to_be_bytes());
        }
        buf
    }

    /// Decodes the fixed header; anything past 48 bytes (extensions, MAC) is
    /// ignored.
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        if buf.len() < PACKET_LEN {
            return ShortSnafu { len: buf.len() }.fail();
        }
        let u32_at =
            |at: usize| u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
        let stamp_at = |at: usize| Timestamp {
            seconds: u32_at(at),
            fraction: u32_at(at + 4),
        };
        Ok(Packet {
            leap: buf[0] >> 6,
            version: (buf[0] >> 3) & 0b111,
            mode: buf[0] & 0b111,
            stratum: buf[1],
            poll: buf[2] as i8,
            precision: buf[3] as i8,
            root_delay: u32_at(4),
            root_dispersion: u32_at(8),
            reference_id: [buf[12], buf[13], buf[14], buf[15]],
            reference: stamp_at(16),
            originate: stamp_at(24),
            receive: stamp_at(32),
            transmit: stamp_at(40),
        })
    }

    /// The NTP-04 checks, in the order RFC 4330 §5 lists them, plus the
    /// build-date floor on the server's transmit time.
    pub fn validate(&self, cookie: Timestamp, floor_unix: i64) -> Result<(), Reject> {
        if self.leap == LEAP_UNSYNCHRONIZED {
            return UnsynchronizedSnafu.fail();
        }
        if self.mode != MODE_SERVER {
            return ModeSnafu { mode: self.mode }.fail();
        }
        if self.stratum == 0 {
            let code = String::from_utf8_lossy(&self.reference_id).into_owned();
            return KissOfDeathSnafu { code }.fail();
        }
        if self.stratum > 15 {
            return StratumSnafu {
                stratum: self.stratum,
            }
            .fail();
        }
        if self.transmit.is_zero() {
            return ZeroTransmitSnafu.fail();
        }
        if self.originate != cookie {
            return OriginateSnafu.fail();
        }
        let unix = self.transmit.to_unix_nanos().div_euclid(NANOS);
        if unix < floor_unix {
            return BeforeFloorSnafu {
                unix,
                floor: floor_unix,
            }
            .fail();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COOKIE: Timestamp = Timestamp {
        seconds: 0x1234_5678,
        fraction: 0x9abc_def0,
    };
    const FLOOR: i64 = crate::BUILD_FLOOR_UNIX;

    fn good_reply() -> Packet {
        Packet {
            leap: 0,
            version: 4,
            mode: MODE_SERVER,
            stratum: 1,
            reference_id: *b"GPS\0",
            originate: COOKIE,
            receive: Timestamp::from_unix_nanos((FLOOR + 10) * NANOS),
            transmit: Timestamp::from_unix_nanos((FLOOR + 10) * NANOS + 1_000),
            ..Packet::default()
        }
    }

    #[test]
    fn request_header_is_v4_client() {
        let bytes = Packet::request(COOKIE).encode();
        assert_eq!(bytes[0], 0b00_100_011, "LI 0, VN 4, mode 3");
        assert!(bytes[1..40].iter().all(|b| *b == 0));
        assert_eq!(
            &bytes[40..48],
            &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]
        );
    }

    #[test]
    fn encode_decode_round_trip() {
        let mut packet = good_reply();
        packet.poll = -6;
        packet.precision = -20;
        packet.root_delay = 0x0001_8000;
        packet.root_dispersion = 7;
        packet.reference = Timestamp::from_bits(0xdead_beef_0000_0001);
        assert_eq!(Packet::decode(&packet.encode()), Ok(packet));
    }

    #[test]
    fn decode_ignores_trailing_bytes_and_rejects_short() {
        let mut long = good_reply().encode().to_vec();
        long.extend_from_slice(&[0xff; 20]);
        assert_eq!(Packet::decode(&long), Ok(good_reply()));
        assert_eq!(
            Packet::decode(&long[..47]),
            Err(DecodeError::Short { len: 47 })
        );
    }

    #[test]
    fn timestamp_unix_round_trip() {
        for nanos in [0, 1, NANOS - 1, FLOOR * NANOS + 123_456_789] {
            let back = Timestamp::from_unix_nanos(nanos).to_unix_nanos();
            // The 32-bit fraction is ~0.23 ns; truncation loses at most 1 ns.
            assert!((nanos - back).abs() <= 1, "{nanos} -> {back}");
        }
    }

    #[test]
    fn unix_epoch_is_ntp_2208988800() {
        let stamp = Timestamp::from_unix_nanos(0);
        assert_eq!(stamp.seconds, 2_208_988_800);
        assert_eq!(stamp.fraction, 0);
        let half = Timestamp::from_unix_nanos(NANOS / 2);
        assert_eq!(half.fraction, 0x8000_0000);
    }

    #[test]
    fn era_one_wraps_and_decodes_after_2036() {
        // 2040-01-01T00:00:00Z.
        let unix = 2_208_988_800_i64;
        let stamp = Timestamp::from_unix_nanos(unix * NANOS);
        assert_eq!(i64::from(stamp.seconds), unix + NTP_UNIX_OFFSET - (1 << 32));
        assert_eq!(stamp.to_unix_nanos(), unix * NANOS);
    }

    #[test]
    fn good_reply_passes() {
        assert_eq!(good_reply().validate(COOKIE, FLOOR), Ok(()));
    }

    #[test]
    fn rejects_leap_indicator_3() {
        let reply = Packet {
            leap: 3,
            ..good_reply()
        };
        assert_eq!(reply.validate(COOKIE, FLOOR), Err(Reject::Unsynchronized));
        // Other leap values (pending leap second) are fine.
        let reply = Packet {
            leap: 1,
            ..good_reply()
        };
        assert_eq!(reply.validate(COOKIE, FLOOR), Ok(()));
    }

    #[test]
    fn rejects_wrong_mode() {
        for mode in [0, 1, 2, 3, 5, 6, 7] {
            let reply = Packet {
                mode,
                ..good_reply()
            };
            assert_eq!(reply.validate(COOKIE, FLOOR), Err(Reject::Mode { mode }));
        }
    }

    #[test]
    fn rejects_stratum_0_as_kiss_of_death() {
        let reply = Packet {
            stratum: 0,
            reference_id: *b"RATE",
            ..good_reply()
        };
        assert_eq!(
            reply.validate(COOKIE, FLOOR),
            Err(Reject::KissOfDeath {
                code: "RATE".into()
            })
        );
    }

    #[test]
    fn rejects_stratum_16_and_up() {
        for stratum in [16, 17, 255] {
            let reply = Packet {
                stratum,
                ..good_reply()
            };
            assert_eq!(
                reply.validate(COOKIE, FLOOR),
                Err(Reject::Stratum { stratum })
            );
        }
        let reply = Packet {
            stratum: 15,
            ..good_reply()
        };
        assert_eq!(reply.validate(COOKIE, FLOOR), Ok(()));
    }

    #[test]
    fn rejects_zero_transmit() {
        let reply = Packet {
            transmit: Timestamp::ZERO,
            ..good_reply()
        };
        assert_eq!(reply.validate(COOKIE, FLOOR), Err(Reject::ZeroTransmit));
    }

    #[test]
    fn rejects_originate_mismatch() {
        let reply = Packet {
            originate: Timestamp {
                seconds: COOKIE.seconds,
                fraction: COOKIE.fraction ^ 1,
            },
            ..good_reply()
        };
        assert_eq!(reply.validate(COOKIE, FLOOR), Err(Reject::Originate));
    }

    #[test]
    fn rejects_time_before_floor() {
        let reply = Packet {
            transmit: Timestamp::from_unix_nanos((FLOOR - 1) * NANOS),
            ..good_reply()
        };
        assert_eq!(
            reply.validate(COOKIE, FLOOR),
            Err(Reject::BeforeFloor {
                unix: FLOOR - 1,
                floor: FLOOR
            })
        );
    }
}
