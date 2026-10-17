//! Hand-rolled argument parsing.
//!
//! clap would be the workspace norm (CONVENTIONS "CLI"), but this binary runs
//! on every boot before the stack and its whole job is a few flags; a parser
//! this size keeps it small and quick to start. `--flag value` and
//! `--flag=value` are accepted; unknown flags are errors.

use std::path::PathBuf;

use bootctl::Slot;

/// Default `guard` poll interval, seconds (design §3).
pub const DEFAULT_POLL_SECS: u64 = 10;
/// Default `activate` tries (design §6).
pub const DEFAULT_TRIES: u8 = 3;

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Select {
        root: PathBuf,
    },
    Guard {
        root: PathBuf,
        window: u64,
        poll: u64,
        dry_run: bool,
    },
    Status {
        root: PathBuf,
        json: bool,
    },
    MarkGood {
        root: PathBuf,
        slot: Option<Slot>,
    },
    Rollback {
        root: PathBuf,
    },
    Activate {
        root: PathBuf,
        slot: Slot,
        version: String,
        tries: u8,
    },
    Version,
    Help,
}

pub const USAGE: &str = "\
usage: kboot <command> [options]

  select   --root R                       choose the slot to boot; prints
                                          KSLOT= KSLOT_DIR= KTRIAL= KVERSION=
  guard    --root R --window SECS [--poll SECS] [--dry-run]
                                          reboot unless the trial commits in time
  status   --root R [--json]              show the record and both slots
  mark-good --root R [--slot a|b]         commit a slot (default: last booted)
  rollback --root R                       boot the other committed slot next
  activate --root R --slot a|b --version V [--tries N]
                                          make a slot the next trial boot
  --version                               print the kboot version
";

/// Flag values collected for one subcommand.
#[derive(Default)]
struct Flags {
    root: Option<String>,
    window: Option<String>,
    poll: Option<String>,
    slot: Option<String>,
    version: Option<String>,
    tries: Option<String>,
    dry_run: bool,
    json: bool,
}

/// Parses `args` (without the program name).
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, String> {
    let mut args = args.into_iter();
    let Some(cmd) = args.next() else {
        return Err("missing command".into());
    };
    let (value_flags, bool_flags): (&[&str], &[&str]) = match cmd.as_str() {
        "--version" | "-V" => return Ok(Command::Version),
        "--help" | "-h" | "help" => return Ok(Command::Help),
        "select" | "rollback" => (&["root"], &[]),
        "guard" => (&["root", "window", "poll"], &["dry-run"]),
        "status" => (&["root"], &["json"]),
        "mark-good" => (&["root", "slot"], &[]),
        "activate" => (&["root", "slot", "version", "tries"], &[]),
        other => return Err(format!("unknown command {other:?}")),
    };

    let mut flags = Flags::default();
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Ok(Command::Help);
        }
        let Some(body) = arg.strip_prefix("--") else {
            return Err(format!("unexpected argument {arg:?}"));
        };
        let (name, inline) = match body.split_once('=') {
            Some((n, v)) => (n, Some(v.to_owned())),
            None => (body, None),
        };
        if bool_flags.contains(&name) {
            if inline.is_some() {
                return Err(format!("--{name} takes no value"));
            }
            match name {
                "dry-run" => flags.dry_run = true,
                _ => flags.json = true,
            }
            continue;
        }
        if !value_flags.contains(&name) {
            return Err(format!("unknown option --{name} for {cmd}"));
        }
        let value = match inline {
            Some(v) => v,
            None => args
                .next()
                .ok_or_else(|| format!("--{name} needs a value"))?,
        };
        let slot = match name {
            "root" => &mut flags.root,
            "window" => &mut flags.window,
            "poll" => &mut flags.poll,
            "slot" => &mut flags.slot,
            "version" => &mut flags.version,
            _ => &mut flags.tries,
        };
        *slot = Some(value);
    }

    let root = PathBuf::from(flags.root.take().ok_or("--root is required")?);
    let slot = flags.slot.as_deref().map(parse_slot).transpose()?;
    Ok(match cmd.as_str() {
        "select" => Command::Select { root },
        "rollback" => Command::Rollback { root },
        "status" => Command::Status {
            root,
            json: flags.json,
        },
        "mark-good" => Command::MarkGood { root, slot },
        "guard" => Command::Guard {
            root,
            window: positive(
                "window",
                flags.window.as_deref().ok_or("--window is required")?,
            )?,
            poll: match flags.poll.as_deref() {
                Some(p) => positive("poll", p)?,
                None => DEFAULT_POLL_SECS,
            },
            dry_run: flags.dry_run,
        },
        _ => Command::Activate {
            root,
            slot: slot.ok_or("--slot is required")?,
            version: flags.version.ok_or("--version is required")?,
            tries: match flags.tries.as_deref() {
                Some(t) => t
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or_else(|| format!("--tries must be 1..=255, got {t:?}"))?,
                None => DEFAULT_TRIES,
            },
        },
    })
}

fn parse_slot(s: &str) -> Result<Slot, String> {
    Slot::from_name(s).ok_or_else(|| format!("--slot must be a or b, got {s:?}"))
}

fn positive(name: &str, s: &str) -> Result<u64, String> {
    s.parse()
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| format!("--{name} must be a positive number of seconds, got {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Command, String> {
        parse(s.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn parses_commands_and_defaults() {
        assert_eq!(
            p("select --root /r").unwrap(),
            Command::Select { root: "/r".into() }
        );
        assert_eq!(
            p("guard --root=/r --window 600").unwrap(),
            Command::Guard {
                root: "/r".into(),
                window: 600,
                poll: DEFAULT_POLL_SECS,
                dry_run: false
            }
        );
        assert_eq!(
            p("guard --root /r --window 2 --poll 1 --dry-run").unwrap(),
            Command::Guard {
                root: "/r".into(),
                window: 2,
                poll: 1,
                dry_run: true
            }
        );
        assert_eq!(
            p("activate --root /r --slot b --version v2").unwrap(),
            Command::Activate {
                root: "/r".into(),
                slot: Slot::B,
                version: "v2".into(),
                tries: DEFAULT_TRIES
            }
        );
        assert_eq!(
            p("mark-good --root /r").unwrap(),
            Command::MarkGood {
                root: "/r".into(),
                slot: None
            }
        );
        assert_eq!(p("--version").unwrap(), Command::Version);
        assert_eq!(
            p("status --root /r --json").unwrap(),
            Command::Status {
                root: "/r".into(),
                json: true
            }
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(p("").is_err());
        assert!(p("select").is_err());
        assert!(p("select --root").is_err());
        assert!(p("select --root /r --json").is_err());
        assert!(p("guard --root /r").is_err());
        assert!(p("guard --root /r --window 0").is_err());
        assert!(p("guard --root /r --window x").is_err());
        assert!(p("mark-good --root /r --slot c").is_err());
        assert!(p("activate --root /r --slot b --version v --tries 0").is_err());
        assert!(p("frobnicate --root /r").is_err());
        assert!(p("select /r").is_err());
    }
}
