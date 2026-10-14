//! Wall-clock and uptime stamps for log lines.
//!
//! The camera has no RTC in use: it boots at 1970-01-01 UTC and only learns
//! the time once the `ntp` service has synced. Until then a wall clock in a
//! log line is actively misleading, so [`now`] returns `None` for any clock
//! before [`CLOCK_VALID_FLOOR_UNIX`] and callers fall back to uptime alone.
//! Uptime ([`uptime`]) is always printed too: it orders lines across a clock
//! step and is the only time there is before the first sync.
//!
//! Local time comes from the C library (`localtime_r`), so it follows the
//! POSIX `TZ` string `init.sh` exports (uClibc has no zoneinfo; e.g.
//! `<-03>3` for America/Sao_Paulo). The UTC offset is not read from
//! `tm_gmtoff`: whether the crosstool's uClibc 0.9.33.2 was built with the
//! tm extensions that add it is not known, so it is derived from the broken
//! down local time instead ([`offset_from_tm`]), which needs only the fields
//! POSIX guarantees.
//!
//! ```text
//! 2026-10-14 14:03:07.123 -0300   LocalTime's Display, log files
//! 2026-10-14 14:03:07 -0300       LocalTime::seconds, the console banner
//! 14:03:07                        LocalTime::clock, console lines
//! ```

use std::fmt;
use std::sync::Once;
use std::time::Duration;

/// Earliest time the system clock may read and still be trusted, as Unix
/// seconds: 2026-10-01T00:00:00Z, shortly before the `ntp` crate was written.
/// A clock before it was never set (the camera boots at 1970), and an NTP
/// reply before it is a broken or hostile server. A constant, not the build
/// machine's clock, so release builds stay reproducible; bump it now and then.
/// `ntp::BUILD_FLOOR_UNIX` is this same value.
pub const CLOCK_VALID_FLOOR_UNIX: i64 = 1_790_812_800;

/// The largest UTC offset any real zone uses is +14:00; anything past this
/// means the C library returned nonsense, and UTC is shown instead.
const MAX_OFFSET_SECS: i64 = 26 * 3600;

/// A local date and time, with the offset from UTC it was taken at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i64,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub millis: u16,
    /// Seconds east of UTC (`-10800` for `-0300`).
    pub offset_secs: i32,
}

impl LocalTime {
    /// The local time `offset_secs` east of UTC at Unix time `unix`.
    /// Pure arithmetic, no C library: what tests and [`local`] build on.
    pub fn from_unix(unix: i64, millis: u16, offset_secs: i32) -> Self {
        let local = unix + i64::from(offset_secs);
        let (year, month, day) = civil_from_days(local.div_euclid(86_400));
        let secs = local.rem_euclid(86_400);
        Self {
            year,
            month,
            day,
            hour: (secs / 3600) as u8,
            minute: (secs % 3600 / 60) as u8,
            second: (secs % 60) as u8,
            millis: millis.min(999),
            offset_secs,
        }
    }

    /// `HH:MM:SS`, for the console where width matters.
    pub fn clock(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }

    /// `YYYY-MM-DD HH:MM:SS ±HHMM`, without milliseconds.
    pub fn seconds(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {} {}",
            self.year,
            self.month,
            self.day,
            self.clock(),
            Offset(self.offset_secs)
        )
    }
}

/// `YYYY-MM-DD HH:MM:SS.mmm ±HHMM`.
impl fmt::Display for LocalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {}",
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.millis,
            Offset(self.offset_secs)
        )
    }
}

/// `±HHMM`, as `strftime("%z")` and BusyBox `date +%z` print it.
struct Offset(i32);

impl fmt::Display for Offset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { '-' } else { '+' };
        let abs = self.0.unsigned_abs();
        write!(f, "{sign}{:02}{:02}", abs / 3600, abs % 3600 / 60)
    }
}

/// The prefix of a line in a log file on the card, shared by the
/// supervisor's per-service logs and `telemetry`'s `ANYKA_LOG_FILE` mode so
/// one parser (the web log viewer) reads both:
///
/// ```text
/// [2026-10-14 14:03:07.123 -0300] [     81234]   clock set: local time, ms since boot
/// [     81234]                                   clock not set yet
/// ```
///
/// Uptime stays after the wall time: it is the only time before NTP's first
/// sync, and it keeps lines ordered across the clock step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogPrefix {
    pub wall: Option<LocalTime>,
    pub uptime_ms: u64,
}

impl LogPrefix {
    /// Wall clock (if set) and uptime, read now.
    pub fn now() -> Self {
        Self {
            wall: now(),
            uptime_ms: uptime().as_millis() as u64,
        }
    }
}

impl fmt::Display for LogPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(wall) = &self.wall {
            write!(f, "[{wall}] ")?;
        }
        write!(f, "[{:>10}]", self.uptime_ms)
    }
}

/// The current local time, or `None` while the clock has not been set
/// (before [`CLOCK_VALID_FLOOR_UNIX`]).
pub fn now() -> Option<LocalTime> {
    let (unix, nanos) = clock(libc::CLOCK_REALTIME)?;
    checked(unix, (nanos / 1_000_000) as u16)
}

/// [`local`], or `None` for a time before [`CLOCK_VALID_FLOOR_UNIX`].
pub fn checked(unix: i64, millis: u16) -> Option<LocalTime> {
    (unix >= CLOCK_VALID_FLOOR_UNIX).then(|| local(unix, millis))
}

/// [`now`] as `YYYY-MM-DD HH:MM:SS.mmm ±HHMM`.
pub fn wall_stamp() -> Option<String> {
    now().map(|time| time.to_string())
}

/// Unix time `unix` in the local zone (the `TZ` the process started with).
/// No floor check: for times derived from an already checked clock, such as
/// "booted `uptime` seconds before now". Falls back to UTC if the C library
/// cannot convert it.
pub fn local(unix: i64, millis: u16) -> LocalTime {
    LocalTime::from_unix(unix, millis, utc_offset(unix).unwrap_or(0))
}

/// Time since boot, including time suspended — the clock `/proc/uptime`
/// shows, without opening a file per call.
pub fn uptime() -> Duration {
    clock(libc::CLOCK_BOOTTIME)
        .or_else(|| clock(libc::CLOCK_MONOTONIC))
        .map(|(secs, nanos)| Duration::new(secs.max(0) as u64, nanos))
        .unwrap_or_default()
}

/// Reads one clock as (seconds, nanoseconds).
fn clock(id: libc::clockid_t) -> Option<(i64, u32)> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
    if unsafe { libc::clock_gettime(id, &mut ts) } != 0 {
        return None;
    }
    // `time_t`/`c_long` are 32-bit on the camera, 64-bit on the host.
    #[allow(clippy::unnecessary_cast)]
    Some((ts.tv_sec as i64, ts.tv_nsec.clamp(0, 999_999_999) as u32))
}

unsafe extern "C" {
    /// POSIX, but not bound by the `libc` crate.
    fn tzset();
}

static TZSET: Once = Once::new();

/// Seconds east of UTC in the local zone at `unix`, via `localtime_r`.
fn utc_offset(unix: i64) -> Option<i32> {
    // POSIX lets localtime_r skip reading TZ; tzset once makes sure it was.
    // SAFETY: tzset takes no arguments; it only reads the environment, and
    // nothing in these binaries modifies TZ after start.
    TZSET.call_once(|| unsafe { tzset() });
    let t = libc::time_t::try_from(unix).ok()?;
    // SAFETY: all-zero is a valid `tm` (the pointer field becomes null).
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call; localtime_r is the
    // reentrant variant and writes only to `tm`.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    offset_from_tm(&tm, unix)
}

/// The UTC offset `tm` (the local broken-down form of `unix`) implies:
/// its fields read as if they were UTC, minus the real Unix time.
fn offset_from_tm(tm: &libc::tm, unix: i64) -> Option<i32> {
    let days = days_from_civil(
        i64::from(tm.tm_year) + 1900,
        i64::from(tm.tm_mon) + 1,
        i64::from(tm.tm_mday),
    );
    let local = days * 86_400
        + i64::from(tm.tm_hour) * 3600
        + i64::from(tm.tm_min) * 60
        + i64::from(tm.tm_sec);
    let offset = local - unix;
    (offset.abs() <= MAX_OFFSET_SECS).then_some(offset as i32)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: (year, month, day).
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        yoe + era * 400 + i64::from(month <= 2),
        month as u8,
        day as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-14T17:03:07Z.
    const T: i64 = 1_791_997_387;

    #[test]
    fn floor_is_2026_10_01() {
        assert_eq!(
            days_from_civil(2026, 10, 1) * 86_400,
            CLOCK_VALID_FLOOR_UNIX
        );
        // 56 years of days, 14 of them leap days, plus Jan..Sep 2026.
        let days = 56 * 365 + 14 + (31 + 28 + 31 + 30 + 31 + 30 + 31 + 31 + 30);
        assert_eq!(CLOCK_VALID_FLOOR_UNIX, days * 86_400);
    }

    #[test]
    fn formats_with_a_negative_offset() {
        let time = LocalTime::from_unix(T, 123, -3 * 3600);
        assert_eq!(time.to_string(), "2026-10-14 14:03:07.123 -0300");
        assert_eq!(time.seconds(), "2026-10-14 14:03:07 -0300");
        assert_eq!(time.clock(), "14:03:07");
    }

    #[test]
    fn formats_utc_and_half_hour_zones_across_midnight() {
        assert_eq!(
            LocalTime::from_unix(T, 5, 0).to_string(),
            "2026-10-14 17:03:07.005 +0000"
        );
        // India, +05:30: past midnight into the next day.
        assert_eq!(
            LocalTime::from_unix(T + 7 * 3600, 0, 5 * 3600 + 1800).to_string(),
            "2026-10-15 05:33:07.000 +0530"
        );
        // Newfoundland, -03:30.
        assert_eq!(
            LocalTime::from_unix(0, 0, -(3 * 3600 + 1800)).seconds(),
            "1969-12-31 20:30:00 -0330"
        );
    }

    #[test]
    fn log_prefix_with_and_without_wall_time() {
        let wall = LocalTime::from_unix(T, 123, -10_800);
        let prefix = LogPrefix {
            wall: Some(wall),
            uptime_ms: 81_234,
        };
        assert_eq!(
            prefix.to_string(),
            "[2026-10-14 14:03:07.123 -0300] [     81234]"
        );
        let prefix = LogPrefix {
            wall: None,
            uptime_ms: 81_234,
        };
        assert_eq!(prefix.to_string(), "[     81234]");
    }

    #[test]
    fn civil_round_trips() {
        for days in [-1, 0, 59, 60, 365, 11_016, 20_727, 50_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, i64::from(m), i64::from(d)), days);
        }
        assert_eq!(civil_from_days(20_727), (2026, 10, 1));
    }

    #[test]
    fn offset_from_broken_down_time() {
        // SAFETY: all-zero is a valid `tm`.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = 126;
        tm.tm_mon = 9;
        tm.tm_mday = 14;
        tm.tm_hour = 14;
        tm.tm_min = 3;
        tm.tm_sec = 7;
        assert_eq!(offset_from_tm(&tm, T), Some(-10_800));
        tm.tm_year = 70;
        assert_eq!(offset_from_tm(&tm, T), None, "nonsense offsets are refused");
    }

    /// The only test in this crate that touches `TZ`; it calls tzset itself
    /// rather than relying on the once-only call.
    #[test]
    fn follows_a_posix_tz_string() {
        for (tz, offset) in [("<-03>3", -10_800), ("UTC0", 0), ("<+0530>-5:30", 19_800)] {
            std::env::set_var("TZ", tz);
            // SAFETY: as in `utc_offset`.
            unsafe { tzset() };
            TZSET.call_once(|| {});
            assert_eq!(utc_offset(T), Some(offset), "TZ={tz}");
            assert_eq!(local(T, 0).offset_secs, offset);
        }
    }

    #[test]
    fn clock_before_the_floor_is_not_a_wall_time() {
        assert_eq!(checked(0, 0), None);
        assert_eq!(checked(CLOCK_VALID_FLOOR_UNIX - 1, 999), None);
        assert!(checked(CLOCK_VALID_FLOOR_UNIX, 0).is_some());
        // The host clock is set, so `now` gives a time.
        let time = now().expect("host clock is set");
        assert!(time.year >= 2026);
        assert!(uptime() > Duration::ZERO);
    }
}
