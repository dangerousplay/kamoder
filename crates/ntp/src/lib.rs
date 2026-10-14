//! Time sync for the camera: an SNTPv4 client (RFC 4330) that sets the
//! system clock, and the IANA name → POSIX TZ table `init.sh` resolves
//! `TIMEZONE` through.
//!
//! Why this exists: the camera boots with its clock at 1970-01-01 UTC (no RTC
//! in use), BusyBox ships neither `ntpd` nor `rdate`, and uClibc only knows
//! POSIX TZ strings (from `TZ` or `/etc/TZ`), not zoneinfo names. Logs, JWT
//! expiry, WS-Security `Created` and RTSP times are all wrong until something
//! sets the clock.
//!
//! Why its own client and not a crate: SNTP is one 48-byte request and one
//! 48-byte reply. The whole protocol is [`packet`] plus the four-timestamp
//! formula in [`client::compute`], smaller than the surface of any crate,
//! with nothing to vet for uClibc/ARMv5 (no 64-bit atomics, no async
//! runtime).
//!
//! ```text
//!   client                 server
//!   T1 ── request ──────▶  T2
//!   T4 ◀────── reply ────  T3
//!   offset = ((T2 − T1) + (T3 − T4)) / 2
//!   delay  =  (T4 − T1) − (T3 − T2)
//! ```
//!
//! Requirements: NTP-01 (step > 0.5 s or first sync, else slew), NTP-02
//! (fallback across servers, backoff), NTP-03 (resync interval, metrics),
//! NTP-04 (reply validation, DNS per attempt), NTP-05 (`NTP_ENABLED=0`),
//! TZ-01 ([`tz`]).

pub mod backoff;
pub mod client;
pub mod clock;
pub mod packet;
pub mod tz;

use metrics::{describe_counter, describe_gauge, Unit};

/// Servers queried when none are configured: NTP.br's stratum-1 servers
/// first (the owner is in Brazil), then the global pool.
pub const DEFAULT_SERVERS: &str = "a.st1.ntp.br b.st1.ntp.br pool.ntp.org";

/// Earliest time any reply may claim, as Unix seconds: 2026-10-01T00:00:00Z,
/// shortly before this crate was written. A reply before it is a broken or
/// hostile server, never the real time, so it is rejected (spec edge case
/// "before the build date"). A constant, not the build machine's clock, so
/// release builds stay reproducible; bump it now and then.
pub const BUILD_FLOOR_UNIX: i64 = 1_790_812_800;

/// Offset measured on the last successful sync, before it was corrected.
pub const METRIC_OFFSET: &str = "kamoder_ntp_offset_seconds";
/// Unix time of the last successful sync.
pub const METRIC_LAST_SYNC: &str = "kamoder_ntp_last_sync_timestamp";
/// Sync rounds in which no server gave a usable answer.
pub const METRIC_FAILURES: &str = "kamoder_ntp_sync_failures_total";

/// Registers the metric descriptions with the installed recorder.
pub fn describe_metrics() {
    describe_gauge!(
        METRIC_OFFSET,
        Unit::Seconds,
        "Clock offset measured on the last successful sync (server minus local), before correction"
    );
    describe_gauge!(
        METRIC_LAST_SYNC,
        Unit::Seconds,
        "Unix time of the last successful sync"
    );
    describe_counter!(
        METRIC_FAILURES,
        "Sync rounds in which no server gave a valid reply"
    );
}

/// Splits a space-separated server list. An empty list falls back to
/// [`DEFAULT_SERVERS`]: an empty `NTP_SERVERS` does not disable sync
/// (NTP-05; `NTP_ENABLED=0` does).
pub fn server_list(raw: &str) -> Vec<String> {
    let servers: Vec<String> = raw.split_whitespace().map(str::to_owned).collect();
    if servers.is_empty() {
        return server_list(DEFAULT_SERVERS);
    }
    servers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_list_splits_on_whitespace() {
        assert_eq!(server_list(" a  b\tc "), ["a", "b", "c"]);
    }

    #[test]
    fn empty_server_list_does_not_disable() {
        assert_eq!(
            server_list("   "),
            ["a.st1.ntp.br", "b.st1.ntp.br", "pool.ntp.org"]
        );
    }

    #[test]
    fn floor_is_2026_10_01() {
        // 56 years of days, 14 of them leap days, plus Jan..Sep 2026.
        let days = 56 * 365 + 14 + (31 + 28 + 31 + 30 + 31 + 30 + 31 + 31 + 30);
        assert_eq!(BUILD_FLOOR_UNIX, days * 86_400);
    }
}
