//! `kboot guard`: reboot a trial slot that never commits (OTA-10).
//!
//! The slot under trial is whatever `select` recorded as `last_booted`. The
//! guard polls the record; it is done as soon as that slot is committed
//! (`successful`), stops being bootable, or another slot became the booted
//! one. At the deadline it reboots, and the next `select` consumes another
//! try (or rolls back when none are left). Hangs before the deadline are the
//! hardware watchdog's job, not this one's.
//!
//! A record that cannot be read *at start* ends the guard without a reboot:
//! the reboot would not be counted either, so it could only loop. Read errors
//! while polling are logged and the deadline still applies.

use std::time::{Duration, Instant};

use bootctl::{BootControl, Record, Slot};

use crate::{Log, Result};

/// How the guard ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing (left) to guard; the reason is for the log.
    Settled(String),
    /// The window elapsed with the slot still on trial: reboot.
    Deadline { slot: Slot },
}

/// Why `slot` no longer needs guarding, if it does not.
fn settled(rec: &Record, slot: Slot) -> Option<String> {
    let s = rec.slot(slot);
    if rec.last_booted != Some(slot) {
        let now = rec.last_booted.map_or("none", Slot::name);
        Some(format!(
            "slot {slot} is no longer the booted slot (now {now})"
        ))
    } else if s.successful {
        Some(format!("slot {slot} committed"))
    } else if !s.bootable {
        Some(format!("slot {slot} is no longer bootable"))
    } else {
        None
    }
}

/// Watches the trial for `window`, re-reading the record every `poll`.
pub fn guard(
    ctl: &impl BootControl,
    window: Duration,
    poll: Duration,
    log: &Log,
) -> Result<Outcome> {
    let start = Instant::now();
    let rec = ctl.load()?;
    let Some(slot) = rec.last_booted else {
        return Ok(Outcome::Settled(
            "no slot has been booted; nothing to guard".into(),
        ));
    };
    if let Some(why) = settled(&rec, slot) {
        return Ok(Outcome::Settled(why));
    }
    log.line(&format!(
        "guarding trial of slot {slot}: {} s to commit (poll {} s)",
        window.as_secs(),
        poll.as_secs()
    ));
    let deadline = start + window;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep(poll.min(deadline - now));
        match ctl.load() {
            Ok(rec) => {
                if let Some(why) = settled(&rec, slot) {
                    return Ok(Outcome::Settled(why));
                }
            }
            Err(e) => log.line(&format!("cannot read selector: {e}")),
        }
    }
    Ok(Outcome::Deadline { slot })
}

/// `sync` then `reboot(RB_AUTOBOOT)`. Only returns on failure (not root).
pub fn reboot() -> std::io::Error {
    // SAFETY: neither call takes pointers; sync() cannot fail, and reboot()
    // either does not return or returns -1 with errno set.
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_AUTOBOOT);
    }
    std::io::Error::last_os_error()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bootctl::{activate, choose, MemoryControl};

    fn trial_record() -> Record {
        let rec = activate(&Record::default_legacy(), Slot::B, "v2", 3);
        choose(&rec).record
    }

    #[test]
    fn deadline_when_never_committed() {
        let ctl = MemoryControl::with(trial_record());
        let out = guard(
            &ctl,
            Duration::from_millis(30),
            Duration::from_millis(10),
            &Log::stderr_only(),
        )
        .unwrap();
        assert_eq!(out, Outcome::Deadline { slot: Slot::B });
    }

    #[test]
    fn settled_cases() {
        let rec = trial_record();
        assert_eq!(settled(&rec, Slot::B), None);
        let ok = bootctl::mark_successful(&rec, Slot::B);
        assert_eq!(settled(&ok, Slot::B).unwrap(), "slot b committed");
        let gone = bootctl::invalidate(&rec, Slot::B);
        assert!(settled(&gone, Slot::B)
            .unwrap()
            .contains("no longer bootable"));
        let moved = Record {
            last_booted: Some(Slot::A),
            ..rec
        };
        assert!(settled(&moved, Slot::B).unwrap().contains("now a"));
    }

    #[test]
    fn nothing_to_guard_on_committed_slot() {
        let ctl = MemoryControl::with(choose(&Record::default_legacy()).record);
        let out = guard(
            &ctl,
            Duration::from_secs(60),
            Duration::from_secs(10),
            &Log::stderr_only(),
        )
        .unwrap();
        assert!(matches!(out, Outcome::Settled(_)));
    }
}
