//! Install progress, reported to a caller-supplied callback.
//!
//! Bytes are **installed payload bytes**: for archive components the sum of
//! the manifest's file sizes (what lands on the card), for MTD images the
//! image size (what is written to flash; the read-back verify is not
//! counted). Both are known from the signed manifest before anything is
//! read, so `bytes_total` is exact from the first report.

use ota_manifest::{Component, Manifest};
use serde::Serialize;

/// Report granularity: at most one callback per this many bytes, plus one
/// at every file start and one at the end of each component.
pub const REPORT_EVERY: u64 = 256 * 1024;

/// A snapshot of install progress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Progress {
    /// Installed bytes so far, all components.
    pub bytes_done: u64,
    /// Installed bytes when finished, all components.
    pub bytes_total: u64,
    /// Name of the component being installed ("slot", "boot").
    pub component: String,
    /// File being written (archive components), relative to the component.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// Installed size of one component (see the module docs).
pub fn component_total(c: &Component) -> u64 {
    if c.kind.has_files() {
        c.files.iter().map(|f| f.size).sum()
    } else {
        c.size
    }
}

/// Installed size of every component of `m`.
pub fn manifest_total(m: &Manifest) -> u64 {
    m.components.iter().map(component_total).sum()
}

/// Accumulates progress and rate-limits the callback.
pub struct Reporter<'a> {
    state: Progress,
    last_reported: u64,
    callback: &'a mut dyn FnMut(&Progress),
}

impl<'a> Reporter<'a> {
    /// Starts at 0 of `total`.
    pub fn new(total: u64, callback: &'a mut dyn FnMut(&Progress)) -> Reporter<'a> {
        Reporter {
            state: Progress {
                bytes_total: total,
                ..Progress::default()
            },
            last_reported: 0,
            callback,
        }
    }

    /// Current snapshot.
    pub fn progress(&self) -> &Progress {
        &self.state
    }

    /// Enters `component` and reports.
    pub fn component(&mut self, name: &str) {
        self.state.component = name.to_owned();
        self.state.file = None;
        self.emit();
    }

    /// Starts `file` (or clears it with `None`) and reports.
    pub fn file(&mut self, file: Option<&str>) {
        self.state.file = file.map(str::to_owned);
        self.emit();
    }

    /// Adds `n` installed bytes; reports every [`REPORT_EVERY`] bytes.
    pub fn advance(&mut self, n: u64) {
        self.state.bytes_done += n;
        if self.state.bytes_done - self.last_reported >= REPORT_EVERY {
            self.emit();
        }
    }

    /// Reports unconditionally.
    pub fn emit(&mut self) {
        self.last_reported = self.state.bytes_done;
        (self.callback)(&self.state);
    }
}
