//! `kboot`: the boot-time half of A/B updates (OTA-09, OTA-10; design §3).
//!
//! `Factory/config.sh` (BusyBox sh) runs `kboot select` before anything else
//! of the stack, `eval`s the assignments it prints and starts
//! `<slot>/init.sh`; on a trial boot it backgrounds `kboot guard`, which
//! reboots the camera unless the new slot commits itself in time.
//!
//! ```text
//! <root>/boot/sel.0 sel.1   selector records (bootctl::SdSelector)
//! <root>/boot/last          slot name kboot chose last (config.sh fallback)
//! <root>/boot/bin/kboot     this binary, outside both slots
//! <root>/slots/{a,b}/       code slots, each with init.sh
//! <root>/init.sh            legacy tree = slot a while slots/a is absent
//! <root>/logs/boot.log      decisions, with a /proc/uptime prefix
//! ```
//!
//! Why it looks the way it does:
//!
//! - **Tiny and dependency-light.** It runs on every boot before the stack and
//!   must start instantly: `bootctl` + `libc`, a hand-rolled argument parser
//!   ([`cli`]), plain stderr lines instead of a tracing subscriber.
//! - **stdout is a contract.** Only shell-safe assignments, written in one
//!   go at the very end, so a failure or panic midway prints nothing and the
//!   shell falls back (`boot/last`, then the legacy tree).
//! - **Nothing half-written.** The record is written by `SdSelector`
//!   (redundant in-place copies); `boot/last` goes through tmp + fsync +
//!   rename. A panic unwinds out of either at worst leaving the previous state.
//! - **The record decides, the tree vetoes.** A slot the record picks whose
//!   directory has no `init.sh` is marked unbootable and the choice repeats
//!   once; if that fails too the legacy root boots.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use bootctl::Slot;
use snafu::Snafu;

pub mod cli;
pub mod guard;
pub mod select;
pub mod status;

/// Errors that make a command fail (exit status 1).
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("selector: {source}"))]
    Boot { source: bootctl::Error },
    #[snafu(display("{action} {}: {source}", path.display()))]
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[snafu(display("path {path:?} cannot be passed to the shell (quote, newline or not UTF-8)"))]
    UnsafePath { path: PathBuf },
    #[snafu(display("nothing bootable under {}: no slot has init.sh, no legacy init.sh", root.display()))]
    NothingBootable { root: PathBuf },
    #[snafu(display("no slot has been booted yet; pass --slot a|b"))]
    NoLastBooted,
}

impl From<bootctl::Error> for Error {
    fn from(source: bootctl::Error) -> Error {
        Error::Boot { source }
    }
}

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Name of the slot entry script; a slot directory without it is not bootable.
pub const INIT: &str = "init.sh";

/// Paths under the data root.
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// `root` made absolute against the current directory (not canonicalised:
    /// the shell gets back the path it passed, plus a prefix if relative).
    pub fn new(root: &Path) -> Result<Layout> {
        let root = std::path::absolute(root).map_err(|source| Error::Io {
            action: "resolve",
            path: root.to_path_buf(),
            source,
        })?;
        Ok(Layout { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Selector directory (`sel.0`, `sel.1`, `last`).
    pub fn boot_dir(&self) -> PathBuf {
        self.root.join("boot")
    }

    /// `boot/last`.
    pub fn last_file(&self) -> PathBuf {
        self.boot_dir().join("last")
    }

    /// `logs/boot.log`.
    pub fn boot_log(&self) -> PathBuf {
        self.root.join("logs").join("boot.log")
    }

    /// `slots/<x>`, whether or not it exists.
    pub fn slot_path(&self, slot: Slot) -> PathBuf {
        self.root.join("slots").join(slot.name())
    }

    /// The root itself when it holds a legacy tree (`init.sh`).
    pub fn legacy(&self) -> Option<PathBuf> {
        has_init(&self.root).then(|| self.root.clone())
    }

    /// Bootable directory for `slot`: `slots/<x>` with `init.sh`; for slot a
    /// the legacy root when `slots/a` is not (yet) populated (design D-O2).
    pub fn slot_dir(&self, slot: Slot) -> Option<PathBuf> {
        let dir = self.slot_path(slot);
        if has_init(&dir) {
            return Some(dir);
        }
        match slot {
            Slot::A => self.legacy(),
            Slot::B => None,
        }
    }

    /// Whether `slot` maps to the legacy root rather than `slots/<x>`.
    pub fn is_legacy(&self, slot: Slot) -> bool {
        slot == Slot::A && !has_init(&self.slot_path(slot)) && self.legacy().is_some()
    }

    /// `boot/last` content (first line, trimmed), if readable.
    pub fn read_last(&self) -> Option<String> {
        let text = fs::read_to_string(self.last_file()).ok()?;
        Some(text.lines().next().unwrap_or("").trim().to_owned())
    }

    /// Writes `boot/last` = `slot` unless it already says so. tmp + fsync +
    /// rename + directory fsync, so a power cut leaves the old or new file.
    /// Returns whether it wrote.
    pub fn write_last(&self, slot: Slot) -> Result<bool> {
        if self.read_last().as_deref() == Some(slot.name()) {
            return Ok(false);
        }
        let path = self.last_file();
        let tmp = self.boot_dir().join("last.tmp");
        let io = |action: &'static str, path: &Path| {
            let path = path.to_path_buf();
            move |source| Error::Io {
                action,
                path,
                source,
            }
        };
        let mut file = File::create(&tmp).map_err(io("create", &tmp))?;
        file.write_all(format!("{}\n", slot.name()).as_bytes())
            .map_err(io("write", &tmp))?;
        file.sync_all().map_err(io("fsync", &tmp))?;
        drop(file);
        fs::rename(&tmp, &path).map_err(io("rename to", &path))?;
        // Some filesystems refuse fsync on a directory; the rename stands.
        if let Ok(dir) = File::open(self.boot_dir()) {
            let _ = dir.sync_all();
        }
        Ok(true)
    }
}

fn has_init(dir: &Path) -> bool {
    dir.join(INIT).is_file()
}

/// Best-effort decision log: stderr, and `logs/boot.log` unless stderr
/// already is that file (config.sh redirects kboot's stderr there; writing
/// both would double every line). Never fails.
#[derive(Debug)]
pub struct Log {
    file: Option<PathBuf>,
}

impl Log {
    /// Logs to stderr and `file`.
    pub fn new(file: PathBuf) -> Log {
        let file = (!stderr_is(&file)).then_some(file);
        Log { file }
    }

    /// Logs to stderr only.
    pub fn stderr_only() -> Log {
        Log { file: None }
    }

    /// One line, prefixed like the kernel log: `[  12.34] kboot: msg`.
    pub fn line(&self, msg: &str) {
        let line = format!("[{:>8}] kboot: {msg}\n", uptime());
        let _ = io::stderr().write_all(line.as_bytes());
        let Some(path) = &self.file else { return };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// First field of `/proc/uptime` (seconds since boot), `?` if unreadable.
fn uptime() -> String {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "?".to_owned())
}

/// Whether file descriptor 2 is `path` (same device and inode).
fn stderr_is(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    // SAFETY: `libc::stat` is plain old data; all-zero is a valid value.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a valid, writable out parameter; fd 2 is only
    // inspected (an invalid fd just makes fstat fail with EBADF).
    if unsafe { libc::fstat(2, &mut st) } != 0 {
        return false;
    }
    st.st_dev as u64 == meta.dev() && st.st_ino as u64 == meta.ino()
}
