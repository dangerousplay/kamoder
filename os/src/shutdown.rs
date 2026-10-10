//! Cooperative shutdown on `SIGINT`/`SIGTERM`.
//!
//! Every binary here has resources that must be released in order — stop the
//! stream, close the encoder, close the input — so none of them can call
//! `exit()` from a signal handler the way the C originals do. Instead the
//! handler only sets a flag and the main loop unwinds normally, letting the
//! `Drop` impls run.

use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

/// Signal handler. Storing to an `AtomicBool` is async-signal-safe; nothing
/// else in this file is allowed to be called from here.
extern "C" fn handle(_signal: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Installs handlers for `SIGINT`, `SIGTERM` and `SIGHUP`, and ignores
/// `SIGPIPE`.
///
/// `SIGPIPE` matters for the network binaries: without ignoring it, a client
/// that disconnects mid-response kills the process.
pub fn install() {
    // No `SA_RESTART`: an interrupted blocking read should return `EINTR` so
    // the loop can notice the flag, rather than restarting and hanging until
    // the next frame arrives.
    let action = SigAction::new(
        SigHandler::Handler(handle),
        SaFlags::empty(),
        SigSet::empty(),
    );
    for signal in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP] {
        // SAFETY: `handle` only touches an atomic, so it is safe to run in a
        // signal context.
        if let Err(errno) = unsafe { signal::sigaction(signal, &action) } {
            tracing::warn!(%signal, %errno, "cannot install the signal handler");
        }
    }

    let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    // SAFETY: ignoring a signal installs no handler code at all.
    if let Err(errno) = unsafe { signal::sigaction(Signal::SIGPIPE, &ignore) } {
        tracing::warn!(%errno, "cannot ignore SIGPIPE");
    }
}

/// Whether a shutdown signal has arrived.
pub fn requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Whether the process should keep running. The inverse of [`requested`], for
/// use as a `while` condition.
pub fn running() -> bool {
    !requested()
}

/// Requests shutdown from inside the program, e.g. when a worker fails.
pub fn request() {
    STOP.store(true, Ordering::SeqCst);
}
