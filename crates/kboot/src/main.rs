//! `kboot`: boot-time A/B slot selector and trial guard (see the library doc).
//!
//! Exit status: 0 success, 1 the command failed (config.sh falls back),
//! 2 bad usage. A panic exits 101 having printed nothing on stdout.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use bootctl::{BootControl, SdSelector};
use kboot::cli::{self, Command};
use kboot::guard::{self, Outcome};
use kboot::{select, status, Error, Layout, Log};

fn main() -> ExitCode {
    let mut args = Vec::new();
    for arg in std::env::args_os().skip(1) {
        match arg.into_string() {
            Ok(a) => args.push(a),
            Err(a) => {
                eprintln!("kboot: argument {a:?} is not UTF-8");
                return ExitCode::from(2);
            }
        }
    }
    let cmd = match cli::parse(args) {
        Ok(cmd) => cmd,
        Err(msg) => {
            eprintln!("kboot: {msg}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    match run(cmd) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("kboot: error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Writes all of `out` at once; stdout is only ever written here.
fn emit(out: &str) -> Result<(), Error> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(out.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|source| Error::Io {
            action: "write",
            path: "<stdout>".into(),
            source,
        })
}

fn run(cmd: Command) -> Result<ExitCode, Error> {
    match cmd {
        Command::Version => {
            let build = option_env!("KAMODER_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
            emit(&format!("kboot {build}\n"))?;
        }
        Command::Help => emit(cli::USAGE)?,
        Command::Select { root } => {
            let layout = Layout::new(&root)?;
            let log = Log::new(layout.boot_log());
            let result =
                select::select(&layout, &log).and_then(|sel| select::shell_assignments(&sel));
            match result {
                Ok(out) => emit(&out)?,
                Err(e) => {
                    log.line(&format!("select failed: {e}"));
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        Command::Guard {
            root,
            window,
            poll,
            dry_run,
        } => {
            // config.sh detaches the guard; a hangup must not end it either.
            // SAFETY: installs SIG_IGN, no handler code runs.
            unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
            let layout = Layout::new(&root)?;
            let log = Log::new(layout.boot_log());
            let ctl = SdSelector::new(layout.boot_dir());
            let outcome = guard::guard(
                &ctl,
                Duration::from_secs(window),
                Duration::from_secs(poll),
                &log,
            );
            match outcome {
                Ok(Outcome::Settled(why)) => log.line(&format!("guard done: {why}")),
                Ok(Outcome::Deadline { slot }) => {
                    log.line(&format!(
                        "trial of slot {slot} not committed within {window} s; rebooting"
                    ));
                    if dry_run {
                        emit("reboot (dry run)\n")?;
                        return Ok(ExitCode::from(3));
                    }
                    let err = guard::reboot();
                    log.line(&format!("reboot failed: {err}"));
                    return Ok(ExitCode::FAILURE);
                }
                Err(e) => {
                    log.line(&format!("guard cannot start: {e}"));
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        Command::Status { root, json } => {
            let layout = Layout::new(&root)?;
            let st = status::read(&layout)?;
            emit(&if json {
                status::json(&layout, &st)
            } else {
                status::human(&layout, &st)
            })?;
        }
        Command::MarkGood { root, slot } => {
            let (layout, log, mut ctl) = admin(&root)?;
            let slot = match slot {
                Some(s) => s,
                None => ctl.load()?.last_booted.ok_or(Error::NoLastBooted)?,
            };
            ctl.mark_successful(slot)?;
            log.line(&format!("slot {slot} marked good"));
            print_status(&layout)?;
        }
        Command::Rollback { root } => {
            let (layout, log, mut ctl) = admin(&root)?;
            let rec = ctl.rollback()?;
            let target = bootctl::choose(&rec).slot;
            log.line(&format!("manual rollback: slot {target} boots next"));
            print_status(&layout)?;
        }
        Command::Activate {
            root,
            slot,
            version,
            tries,
        } => {
            let (layout, log, mut ctl) = admin(&root)?;
            ctl.activate(slot, &version, tries)?;
            log.line(&format!(
                "slot {slot} activated as trial: version {version:?}, {tries} tries"
            ));
            print_status(&layout)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Setup shared by the record-changing admin commands.
fn admin(root: &std::path::Path) -> Result<(Layout, Log, SdSelector), Error> {
    let layout = Layout::new(root)?;
    let boot = layout.boot_dir();
    std::fs::create_dir_all(&boot).map_err(|source| Error::Io {
        action: "create",
        path: boot.clone(),
        source,
    })?;
    let log = Log::new(layout.boot_log());
    Ok((layout, log, SdSelector::new(boot)))
}

fn print_status(layout: &Layout) -> Result<(), Error> {
    let st = status::read(layout)?;
    emit(&status::human(layout, &st))
}
