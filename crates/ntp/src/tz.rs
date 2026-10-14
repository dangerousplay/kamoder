//! IANA zone name → POSIX TZ string (TZ-01).
//!
//! uClibc has no zoneinfo reader: it takes a POSIX TZ string from `TZ`, or
//! from `/etc/TZ` when `TZ` is unset. Shipping the tz database for one
//! string per camera is not worth it, so this is a small table of the zones
//! this camera is likely to be in, plus passthrough of a raw POSIX string for
//! anything else. `init.sh` calls `ntp tz "$TIMEZONE"` and writes the result
//! to `/etc/TZ` and `TZ`.
//!
//! The rules are the ones in force today (tzdata 2025): Brazil has had no
//! DST since 2019, so every Brazilian zone is a fixed offset; the US switches
//! on the second Sunday of March and the first of November at 02:00 local;
//! the EU on the last Sundays of March and October at 01:00 UTC. Angle
//! brackets quote numeric abbreviations (`<-03>`), as tzdata itself does.
//!
//! ```text
//! $ ntp tz America/Sao_Paulo
//! <-03>3
//! ```

use snafu::Snafu;

/// Built-in zones. Lookup ignores ASCII case.
pub const ZONES: &[(&str, &str)] = &[
    ("UTC", "UTC0"),
    ("Etc/UTC", "UTC0"),
    // Brazil, -03.
    ("America/Sao_Paulo", "<-03>3"),
    ("America/Fortaleza", "<-03>3"),
    ("America/Recife", "<-03>3"),
    ("America/Bahia", "<-03>3"),
    ("America/Belem", "<-03>3"),
    ("America/Maceio", "<-03>3"),
    ("America/Araguaina", "<-03>3"),
    ("America/Santarem", "<-03>3"),
    // Brazil, -04.
    ("America/Manaus", "<-04>4"),
    ("America/Cuiaba", "<-04>4"),
    ("America/Campo_Grande", "<-04>4"),
    ("America/Porto_Velho", "<-04>4"),
    ("America/Boa_Vista", "<-04>4"),
    // Brazil, -05 (Acre) and -02 (Fernando de Noronha).
    ("America/Rio_Branco", "<-05>5"),
    ("America/Noronha", "<-02>2"),
    // United States.
    ("America/New_York", "EST5EDT,M3.2.0,M11.1.0"),
    ("America/Chicago", "CST6CDT,M3.2.0,M11.1.0"),
    ("America/Denver", "MST7MDT,M3.2.0,M11.1.0"),
    ("America/Los_Angeles", "PST8PDT,M3.2.0,M11.1.0"),
    // Europe.
    ("Europe/London", "GMT0BST,M3.5.0/1,M10.5.0"),
    ("Europe/Lisbon", "WET0WEST,M3.5.0/1,M10.5.0"),
    ("Europe/Berlin", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Paris", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Europe/Madrid", "CET-1CEST,M3.5.0,M10.5.0/3"),
];

/// Longest raw POSIX string accepted; real ones are under 40 characters.
const MAX_RAW_LEN: usize = 64;

#[derive(Debug, Snafu, PartialEq, Eq)]
pub enum Error {
    #[snafu(display("empty time zone"))]
    Empty,
    #[snafu(display(
        "unknown time zone {name:?}: use one of the built-in names (ntp tz --list) or a POSIX TZ string such as <-03>3"
    ))]
    Unknown { name: String },
}

/// Whether `value` looks like a POSIX TZ string rather than a zone name:
/// it has a digit (every POSIX string has an offset) and no `/` before the
/// first `,` (IANA names have one, POSIX rules only after the comma, as in
/// `M10.5.0/3`). Only the characters POSIX TZ uses are allowed, so the value
/// is safe to write to `/etc/TZ` and to export from shell.
fn is_raw_posix(value: &str) -> bool {
    let head = value.split(',').next().unwrap_or_default();
    value.len() <= MAX_RAW_LEN
        && value.bytes().any(|b| b.is_ascii_digit())
        && !head.contains('/')
        && !value.starts_with(':')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"<>+-,.:/".contains(&b))
}

/// The POSIX TZ string for `name`: a built-in zone (ASCII case ignored) or a
/// raw POSIX string, passed through unchanged.
pub fn resolve(name: &str) -> Result<&str, Error> {
    let name = name.trim();
    if name.is_empty() {
        return EmptySnafu.fail();
    }
    if let Some((_, posix)) = ZONES
        .iter()
        .find(|(zone, _)| zone.eq_ignore_ascii_case(name))
    {
        return Ok(posix);
    }
    if is_raw_posix(name) {
        return Ok(name);
    }
    UnknownSnafu { name }.fail()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sao_paulo_is_minus_three_without_dst() {
        assert_eq!(resolve("America/Sao_Paulo"), Ok("<-03>3"));
    }

    #[test]
    fn every_required_zone_is_in_the_table() {
        let expected = [
            ("UTC", "UTC0"),
            ("Etc/UTC", "UTC0"),
            ("America/Fortaleza", "<-03>3"),
            ("America/Recife", "<-03>3"),
            ("America/Bahia", "<-03>3"),
            ("America/Belem", "<-03>3"),
            ("America/Maceio", "<-03>3"),
            ("America/Araguaina", "<-03>3"),
            ("America/Santarem", "<-03>3"),
            ("America/Manaus", "<-04>4"),
            ("America/Cuiaba", "<-04>4"),
            ("America/Campo_Grande", "<-04>4"),
            ("America/Porto_Velho", "<-04>4"),
            ("America/Boa_Vista", "<-04>4"),
            ("America/Rio_Branco", "<-05>5"),
            ("America/Noronha", "<-02>2"),
            ("America/New_York", "EST5EDT,M3.2.0,M11.1.0"),
            ("America/Chicago", "CST6CDT,M3.2.0,M11.1.0"),
            ("America/Denver", "MST7MDT,M3.2.0,M11.1.0"),
            ("America/Los_Angeles", "PST8PDT,M3.2.0,M11.1.0"),
            ("Europe/London", "GMT0BST,M3.5.0/1,M10.5.0"),
            ("Europe/Lisbon", "WET0WEST,M3.5.0/1,M10.5.0"),
            ("Europe/Berlin", "CET-1CEST,M3.5.0,M10.5.0/3"),
            ("Europe/Paris", "CET-1CEST,M3.5.0,M10.5.0/3"),
            ("Europe/Madrid", "CET-1CEST,M3.5.0,M10.5.0/3"),
        ];
        for (name, posix) in expected {
            assert_eq!(resolve(name), Ok(posix), "{name}");
        }
    }

    #[test]
    fn table_names_are_unique_and_values_valid_posix() {
        for (i, (name, posix)) in ZONES.iter().enumerate() {
            assert!(
                ZONES[..i]
                    .iter()
                    .all(|(other, _)| !other.eq_ignore_ascii_case(name)),
                "{name} twice"
            );
            assert!(is_raw_posix(posix), "{name}: {posix}");
        }
    }

    #[test]
    fn lookup_ignores_case_and_surrounding_space() {
        assert_eq!(resolve(" america/sao_paulo\n"), Ok("<-03>3"));
        assert_eq!(resolve("utc"), Ok("UTC0"));
    }

    #[test]
    fn raw_posix_passes_through() {
        for raw in [
            "<-03>3",
            "UTC0",
            "EST5EDT,M3.2.0,M11.1.0",
            "CET-1CEST,M3.5.0,M10.5.0/3",
            "<+0545>-5:45",
        ] {
            assert_eq!(resolve(raw), Ok(raw));
        }
    }

    #[test]
    fn unknown_names_are_errors() {
        for name in [
            "America/Atlantis",
            "Etc/GMT+3",
            "Brazil",
            ":America/Sao_Paulo",
        ] {
            assert_eq!(
                resolve(name),
                Err(Error::Unknown { name: name.into() }),
                "{name}"
            );
        }
        assert_eq!(resolve("  "), Err(Error::Empty));
    }

    #[test]
    fn shell_characters_are_refused() {
        for name in ["UTC0;reboot", "UTC0 x", "$(id)3", "<-03>3'"] {
            assert!(resolve(name).is_err(), "{name}");
        }
    }
}
