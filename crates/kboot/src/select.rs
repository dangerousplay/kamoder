//! `kboot select`: choose the slot, persist the choice, describe it for sh.
//!
//! Order matters: the record (try consumed, rollback recorded) is stored
//! *before* anything is printed, so a slot that hangs the boot has already
//! paid for the attempt. `boot/last` follows; it is only a shell fallback, so
//! failing to write it is logged, not fatal.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use bootctl::{BootControl, Choice, Record, SdSelector, Slot};

use crate::{Error, Layout, Log, Result};

/// What to boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub slot: Slot,
    /// Absolute directory holding the slot's `init.sh`.
    pub dir: PathBuf,
    /// Trial boot: config.sh must start the guard.
    pub trial: bool,
    /// Tries left after this boot (trial only).
    pub tries_left: u8,
    /// Version label from the record.
    pub version: String,
}

/// Runs the selection against `<root>/boot` and returns what to boot.
///
/// Fails only when the selector cannot be read or written (config.sh then
/// falls back to `boot/last`) or when there is no bootable tree at all.
pub fn select(layout: &Layout, log: &Log) -> Result<Selection> {
    let boot = layout.boot_dir();
    fs::create_dir_all(&boot).map_err(|source| Error::Io {
        action: "create",
        path: boot.clone(),
        source,
    })?;
    let mut ctl = SdSelector::new(&boot);

    let loaded = ctl.load_detailed()?;
    if loaded.fresh {
        log.line("no valid selector record; using legacy defaults (slot a committed)");
    } else if let Some(bad) = loaded.valid.iter().position(|ok| !ok) {
        log.line(&format!(
            "selector copy sel.{bad} invalid; using sel.{}",
            1 - bad
        ));
    }

    let choice = ctl.select()?;
    log_choice(log, &choice);
    let selection = match layout.slot_dir(choice.slot) {
        Some(dir) => from_choice(&choice, dir),
        None => {
            log.line(&format!(
                "slot {} has no {} at {}; marking it unbootable",
                choice.slot,
                crate::INIT,
                layout.slot_path(choice.slot).display()
            ));
            ctl.invalidate(choice.slot)?;
            let second = ctl.select()?;
            log_choice(log, &second);
            match layout.slot_dir(second.slot) {
                Some(dir) => from_choice(&second, dir),
                None => legacy_fallback(layout, log, &mut ctl, &second.record)?,
            }
        }
    };

    match layout.write_last(selection.slot) {
        Ok(true) => log.line(&format!("boot/last = {}", selection.slot)),
        Ok(false) => {}
        Err(e) => log.line(&format!("cannot update boot/last: {e}")),
    }
    log.line(&format!(
        "booting slot {} ({}) from {}{}",
        selection.slot,
        if selection.version.is_empty() {
            "no version"
        } else {
            &selection.version
        },
        selection.dir.display(),
        if selection.trial {
            format!(", trial ({} tries left)", selection.tries_left)
        } else {
            String::new()
        }
    ));
    Ok(selection)
}

fn log_choice(log: &Log, choice: &Choice) {
    for note in &choice.notes {
        log.line(&note.to_string());
    }
    log.line(&format!(
        "record chose slot {}{} (seq {}{})",
        choice.slot,
        if choice.trial { " as trial" } else { "" },
        choice.record.seq,
        if choice.changed { ", stored" } else { "" }
    ));
}

fn from_choice(choice: &Choice, dir: PathBuf) -> Selection {
    Selection {
        slot: choice.slot,
        dir,
        trial: choice.trial,
        tries_left: choice.record.slot(choice.slot).tries_left,
        version: choice.record.slot(choice.slot).version.to_string(),
    }
}

/// Neither record choice has a tree: boot the legacy root as slot a, and
/// record that it was slot a that booted (so the guard and `mark-good` agree).
fn legacy_fallback(
    layout: &Layout,
    log: &Log,
    ctl: &mut SdSelector,
    rec: &Record,
) -> Result<Selection> {
    let Some(dir) = layout.legacy() else {
        return Err(Error::NothingBootable {
            root: layout.root().to_path_buf(),
        });
    };
    log.line(&format!(
        "no slot directory usable; falling back to the legacy tree {}",
        dir.display()
    ));
    if rec.last_booted != Some(Slot::A) {
        let next = Record {
            last_booted: Some(Slot::A),
            ..*rec
        };
        if let Err(e) = ctl.store(&next) {
            log.line(&format!("cannot record the legacy boot: {e}"));
        }
    }
    Ok(Selection {
        slot: Slot::A,
        dir,
        trial: false,
        tries_left: 0,
        version: rec.slot(Slot::A).version.to_string(),
    })
}

/// The stdout contract: `KSLOT=`, `KSLOT_DIR=`, `KTRIAL=`, `KVERSION=`, one
/// per line, safe to `eval`. The directory is single-quoted verbatim and
/// refused if it cannot be (quote, newline, non-UTF-8); the version is
/// reduced to `[A-Za-z0-9._+~-]`, anything else becoming `_`.
pub fn shell_assignments(sel: &Selection) -> Result<String> {
    let dir = sel
        .dir
        .to_str()
        .filter(|d| !d.contains(['\'', '\n', '\r', '\0']))
        .ok_or_else(|| Error::UnsafePath {
            path: sel.dir.clone(),
        })?;
    let version: String = sel.version.chars().map(sanitise_char).collect();
    let mut out = String::new();
    // Writing to a String cannot fail.
    let _ = writeln!(out, "KSLOT={}", sel.slot);
    let _ = writeln!(out, "KSLOT_DIR='{dir}'");
    let _ = writeln!(out, "KTRIAL={}", u8::from(sel.trial));
    let _ = writeln!(out, "KVERSION='{version}'");
    Ok(out)
}

fn sanitise_char(c: char) -> char {
    if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '~' | '-') {
        c
    } else {
        '_'
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignments_quote_and_sanitise() {
        let sel = Selection {
            slot: Slot::B,
            dir: PathBuf::from("/mnt/disc1/anyka rust/slots/b"),
            trial: true,
            tries_left: 2,
            version: "v1.2.3-rc.1 $(reboot)'".to_owned(),
        };
        assert_eq!(
            shell_assignments(&sel).unwrap(),
            "KSLOT=b\nKSLOT_DIR='/mnt/disc1/anyka rust/slots/b'\nKTRIAL=1\n\
             KVERSION='v1.2.3-rc.1___reboot__'\n"
        );
    }

    #[test]
    fn assignments_refuse_quote_in_path() {
        let sel = Selection {
            slot: Slot::A,
            dir: PathBuf::from("/tmp/it's"),
            trial: false,
            tries_left: 0,
            version: String::new(),
        };
        assert!(matches!(
            shell_assignments(&sel),
            Err(Error::UnsafePath { .. })
        ));
    }
}
