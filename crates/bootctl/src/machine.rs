//! The A/B state machine as pure functions over a [`Record`] (OTA-08, OTA-09).
//!
//! ```text
//!            activate(tries=3)        boot (choose)            mark_successful
//!  invalid ───────────────────▶ trial ────────────▶ trial-1 ... ─────────────▶ committed
//!                                 │ tries_left == 0 at boot
//!                                 ▼
//!                            unbootable  ⇒ the other slot boots (rollback)
//! ```
//!
//! None of these touch `seq`; the backend bumps it on `store`. They never
//! panic: a record in which nothing is bootable still yields a slot (the boot
//! shim must always boot *something*; the vendor recovery and UART remain).

use std::fmt;

use crate::record::{Record, Version};
use crate::Slot;

/// Something worth logging that happened while choosing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// `from` was an unsuccessful trial with no tries left: marked unbootable.
    Rollback { from: Slot },
    /// No slot was bootable; `slot` was booted anyway (highest priority).
    Forced { slot: Slot },
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Note::Rollback { from } => {
                write!(
                    f,
                    "rollback: slot {from} failed its trial, marked unbootable"
                )
            }
            Note::Forced { slot } => write!(f, "no bootable slot; forcing slot {slot}"),
        }
    }
}

/// Result of [`choose`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// Slot to boot.
    pub slot: Slot,
    /// The slot boots as a trial (consumed a try; the guard must run).
    pub trial: bool,
    /// The updated record to persist *before* booting.
    pub record: Record,
    /// `record` differs from the input (skip the write when false: SD wear).
    pub changed: bool,
    /// Events for the log, in order.
    pub notes: Vec<Note>,
}

/// Highest-priority slot among those matching `keep`; ties go to A.
fn best(rec: &Record, keep: impl Fn(Slot) -> bool) -> Option<Slot> {
    Slot::ALL
        .into_iter()
        .filter(|&s| keep(s))
        .fold(None, |acc: Option<Slot>, s| match acc {
            Some(a) if rec.slot(a).priority >= rec.slot(s).priority => Some(a),
            _ => Some(s),
        })
}

/// Chooses the slot to boot and returns the record to persist first.
///
/// Highest-priority bootable slot (tie → A). If it is not successful: with
/// tries left one is consumed and it boots as a trial; with none left it is
/// marked unbootable and the choice repeats (rollback). With nothing bootable
/// the highest-priority slot is forced.
pub fn choose(input: &Record) -> Choice {
    let mut rec = *input;
    let mut notes = Vec::new();
    let mut picked = None;
    // At most one rollback per slot, so two rounds decide.
    for _ in 0..Slot::ALL.len() {
        let Some(slot) = best(&rec, |s| rec.slot(s).bootable) else {
            break;
        };
        let state = rec.slot_mut(slot);
        if state.successful {
            picked = Some((slot, false));
            break;
        }
        if state.tries_left > 0 {
            state.tries_left -= 1;
            picked = Some((slot, true));
            break;
        }
        state.bootable = false;
        notes.push(Note::Rollback { from: slot });
    }
    let (slot, trial) = picked.unwrap_or_else(|| {
        let slot = best(&rec, |_| true).unwrap_or(Slot::A);
        notes.push(Note::Forced { slot });
        (slot, false)
    });
    rec.last_booted = Some(slot);
    Choice {
        slot,
        trial,
        changed: rec != *input,
        record: rec,
        notes,
    }
}

/// Activates `slot` as a trial with `tries` boot attempts (OTA-08): bootable,
/// not successful, version set, priority above the other slot (renormalised to
/// 2 vs 1). The other slot is left as it is, so it remains the fallback.
pub fn activate(rec: &Record, slot: Slot, version: &str, tries: u8) -> Record {
    let mut rec = *rec;
    let s = rec.slot_mut(slot);
    s.bootable = true;
    s.successful = false;
    s.tries_left = tries;
    s.priority = 2;
    s.version = Version::new(version);
    rec.slot_mut(slot.other()).priority = 1;
    rec
}

/// Commits `slot` after a healthy boot: successful, no tries pending.
pub fn mark_successful(rec: &Record, slot: Slot) -> Record {
    let mut rec = *rec;
    let s = rec.slot_mut(slot);
    s.successful = true;
    s.tries_left = 0;
    rec
}

/// Marks `slot` unusable, done before its contents are rewritten.
pub fn invalidate(rec: &Record, slot: Slot) -> Record {
    let mut rec = *rec;
    let s = rec.slot_mut(slot);
    s.bootable = false;
    s.successful = false;
    s.tries_left = 0;
    rec
}

/// Manual rollback: makes the slot other than the current one (last booted,
/// else the highest-priority one) the highest priority, if it is bootable and
/// successful. `None` when there is nothing safe to roll back to.
pub fn rollback(rec: &Record) -> Option<Record> {
    let current = rec
        .last_booted
        .or_else(|| best(rec, |_| true))
        .unwrap_or(Slot::A);
    let target = current.other();
    let t = rec.slot(target);
    if !(t.bootable && t.successful) {
        return None;
    }
    let mut rec = *rec;
    rec.slot_mut(target).priority = 2;
    rec.slot_mut(current).priority = 1;
    Some(rec)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boots once, returning (slot, trial) and the record as it would be stored.
    fn boot(rec: &mut Record) -> (Slot, bool) {
        let c = choose(rec);
        *rec = c.record;
        (c.slot, c.trial)
    }

    #[test]
    fn legacy_boots_a_without_trial() {
        let rec = Record::default_legacy();
        let c = choose(&rec);
        assert_eq!((c.slot, c.trial), (Slot::A, false));
        assert!(c.changed, "last_booted set on first boot");
        assert!(c.notes.is_empty());
        // Steady state: nothing to write.
        let again = choose(&c.record);
        assert!(!again.changed);
        assert_eq!(again.record, c.record);
    }

    #[test]
    fn trial_without_commit_rolls_back_after_three_boots() {
        let mut rec = activate(&Record::default_legacy(), Slot::B, "v2", 3);
        assert_eq!(rec.slot(Slot::B).priority, 2);
        assert_eq!(rec.slot(Slot::A).priority, 1);
        assert!(rec.slot(Slot::A).bootable && rec.slot(Slot::A).successful);
        for left in [2, 1, 0] {
            assert_eq!(boot(&mut rec), (Slot::B, true));
            assert_eq!(rec.slot(Slot::B).tries_left, left);
        }
        let c = choose(&rec);
        assert_eq!((c.slot, c.trial), (Slot::A, false));
        assert_eq!(c.notes, vec![Note::Rollback { from: Slot::B }]);
        assert!(!c.record.slot(Slot::B).bootable);
        assert_eq!(c.record.last_booted, Some(Slot::A));
        rec = c.record;
        // Stays on A afterwards.
        assert_eq!(boot(&mut rec), (Slot::A, false));
        assert!(!choose(&rec).changed);
    }

    #[test]
    fn committed_trial_stays() {
        let mut rec = activate(&Record::default_legacy(), Slot::B, "v2", 3);
        assert_eq!(boot(&mut rec), (Slot::B, true));
        rec = mark_successful(&rec, Slot::B);
        assert_eq!(rec.slot(Slot::B).tries_left, 0);
        for _ in 0..5 {
            assert_eq!(boot(&mut rec), (Slot::B, false));
        }
        assert!(!choose(&rec).changed);
        assert_eq!(rec.slot(Slot::B).version.as_str(), "v2");
    }

    #[test]
    fn next_update_goes_to_the_other_slot() {
        let mut rec = mark_successful(
            &activate(&Record::default_legacy(), Slot::B, "v2", 3),
            Slot::B,
        );
        boot(&mut rec);
        rec = invalidate(&rec, Slot::A);
        assert!(!rec.slot(Slot::A).bootable);
        assert_eq!(boot(&mut rec), (Slot::B, false));
        rec = activate(&rec, Slot::A, "v3", 3);
        assert_eq!(boot(&mut rec), (Slot::A, true));
    }

    #[test]
    fn both_unbootable_forces_highest_priority() {
        let mut rec = Record::default_legacy();
        rec = invalidate(&rec, Slot::A);
        rec.slot_mut(Slot::B).priority = 5;
        let c = choose(&rec);
        assert_eq!((c.slot, c.trial), (Slot::B, false));
        assert_eq!(c.notes, vec![Note::Forced { slot: Slot::B }]);
        // Tie → A.
        rec.slot_mut(Slot::B).priority = 0;
        rec.slot_mut(Slot::A).priority = 0;
        assert_eq!(choose(&rec).slot, Slot::A);
    }

    #[test]
    fn failed_trials_in_both_slots_force() {
        let mut rec = activate(&Record::default_legacy(), Slot::B, "v2", 0);
        rec.slot_mut(Slot::A).successful = false;
        rec.slot_mut(Slot::A).tries_left = 0;
        let c = choose(&rec);
        assert_eq!(
            c.notes,
            vec![
                Note::Rollback { from: Slot::B },
                Note::Rollback { from: Slot::A },
                Note::Forced { slot: Slot::B },
            ]
        );
        assert_eq!(c.slot, Slot::B);
    }

    #[test]
    fn rollback_falls_through_to_other_trial() {
        // B failed; A is itself a trial with tries left → A boots as a trial.
        let mut rec = activate(&Record::default_legacy(), Slot::B, "v2", 0);
        rec.slot_mut(Slot::A).successful = false;
        rec.slot_mut(Slot::A).tries_left = 2;
        let c = choose(&rec);
        assert_eq!((c.slot, c.trial), (Slot::A, true));
        assert_eq!(c.record.slot(Slot::A).tries_left, 1);
    }

    #[test]
    fn equal_priority_prefers_a() {
        let mut rec = Record::default_legacy();
        *rec.slot_mut(Slot::B) = *rec.slot(Slot::A);
        assert_eq!(choose(&rec).slot, Slot::A);
    }

    #[test]
    fn manual_rollback() {
        let mut rec = mark_successful(
            &activate(&Record::default_legacy(), Slot::B, "v2", 3),
            Slot::B,
        );
        boot(&mut rec);
        let back = rollback(&rec).expect("A is committed");
        assert_eq!(back.slot(Slot::A).priority, 2);
        assert_eq!(back.slot(Slot::B).priority, 1);
        assert_eq!(choose(&back).slot, Slot::A);
        // Nothing to roll back to on a legacy card.
        let mut legacy = Record::default_legacy();
        boot(&mut legacy);
        assert_eq!(rollback(&legacy), None);
        // Nor to an uncommitted trial.
        let mut rec = activate(&Record::default_legacy(), Slot::B, "v2", 3);
        rec.last_booted = Some(Slot::A);
        assert_eq!(rollback(&rec), None);
    }

    #[test]
    fn mutations_leave_seq_alone() {
        let mut rec = Record::default_legacy();
        rec.seq = 41;
        assert_eq!(activate(&rec, Slot::B, "x", 3).seq, 41);
        assert_eq!(mark_successful(&rec, Slot::A).seq, 41);
        assert_eq!(invalidate(&rec, Slot::A).seq, 41);
        assert_eq!(choose(&rec).record.seq, 41);
    }
}
