# Time Sync (NTP + timezone) Specification

## Problem Statement

The AV100 boots with its clock at 1970-01-01 UTC: there is no RTC battery-backed time in use, BusyBox has no `ntpd`/`rdate`, and no TZ is set. Logs, JWT expiry, ONVIF timestamps (WS-Security `Created`) and RTSP/recording times are all wrong. The owner (Brazil) wants local time in America/Sao_Paulo.

## Goals

- [ ] Clock correct (±1 s) within 60 s of Wi-Fi coming up, kept correct while running.
- [ ] Local time America/Sao_Paulo for every service and shell (`date` shows -03).
- [ ] Board-agnostic, vendor-free, configurable from `camera.conf`.

## Out of Scope

| Feature | Reason |
| --- | --- |
| NTP server mode | Camera is a client only. |
| Full IANA tz database on the card | uClibc takes POSIX TZ strings; a small built-in name→POSIX table plus raw POSIX passthrough is enough. |
| Web UI for time settings | Separate feature; config via camera.conf for now. |
| ONVIF SetSystemDateAndTime | Separate feature. |

## User Stories

### P1: Clock set from NTP ⭐ MVP

1. WHEN the network comes up THEN the `ntp` service SHALL query the configured servers (default `a.st1.ntp.br b.st1.ntp.br pool.ntp.org`) with SNTP (RFC 4330) and step the clock when off by more than 0.5 s (always step on first sync), slewing smaller offsets. (NTP-01)
2. WHEN a server fails/times out (2 s) THEN the next SHALL be tried; WHEN all fail THEN retry with backoff 5 s → 5 min, never exiting. (NTP-02)
3. WHEN synced THEN it SHALL resync every `NTP_INTERVAL` (default 1 h) and expose `kamoder_ntp_offset_seconds`, `kamoder_ntp_last_sync_timestamp`, `kamoder_ntp_sync_failures_total`. (NTP-03)
4. Replies SHALL be validated (mode 4, stratum 1–15, non-zero transmit time, originate timestamp matches request, LI≠3) and DNS resolved each attempt. (NTP-04)
5. `NTP_SERVERS=` (space separated) empty-string-disables is NOT allowed; `NTP_ENABLED=0` disables the service. (NTP-05)

### P1: Timezone ⭐ MVP

1. `TIMEZONE=` in camera.conf accepts an IANA name from a built-in table (at least America/Sao_Paulo → `<-03>3`, other Brazil zones, UTC, common US/EU zones) or a raw POSIX TZ string. (TZ-01)
2. At boot init.sh SHALL write the POSIX string to `/etc/TZ` (tmpfs) and export `TZ` so the supervisor and every child inherit it; unknown names → warn and keep UTC. (TZ-02)
3. This camera's `camera.conf` SHALL be set to `TIMEZONE=America/Sao_Paulo`. (TZ-03)

## Edge Cases

- WHEN the clock is 1970 THEN the first valid reply SHALL step regardless of offset size.
- WHEN a reply implies a time before the build date THEN it SHALL be rejected.
- WHEN Wi-Fi is down THEN no busy loop; backoff applies.

## Traceability

| ID | Story | Status |
| --- | --- | --- |
| NTP-01..05 | P1 Clock | Implemented (crates/ntp, tools/supervisor.toml); hardware unverified |
| TZ-01..03 | P1 Timezone | Implemented (crates/ntp/src/tz.rs, tools/init.sh apply_timezone, owner camera.conf); hardware unverified |
