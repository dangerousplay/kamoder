//! Shims for libc functions the cross toolchain's uClibc does not export.
//!
//! The Anyka crosstool ships uClibc 0.9.33.2, while the camera itself runs
//! uClibc-ng 1.0.31. Rust's `std` is compiled against the newer surface, so a
//! few symbols it references are missing at link time even though they exist
//! on the device.

use std::os::raw::{c_char, c_int};

/// Stub for `pthread_setname_np`, absent from uClibc 0.9.33.2.
///
/// Rust's thread startup path references it unconditionally — not only when a
/// thread is actually named — so the link fails in any binary that spawns a
/// thread. Thread names are a debugging nicety (`/proc/<pid>/task/*/comm`),
/// so reporting success without setting one loses nothing that matters here.
///
/// # Safety
/// Takes the same arguments as the real function and reads neither.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_setname_np(
    _thread: libc::pthread_t,
    _name: *const c_char,
) -> c_int {
    0
}

/// Keeps the shim in the binary.
///
/// With fat LTO the whole `os` crate is merged into the final object,
/// and a `#[no_mangle]` function nothing in Rust calls gets dropped before the
/// linker ever sees std's reference to it. A `#[used]` static holding its
/// address is the standard way to pin it.
#[used]
static KEEP_PTHREAD_SETNAME_NP: unsafe extern "C" fn(libc::pthread_t, *const c_char) -> c_int =
    pthread_setname_np;

/// A fork handler as `pthread_atfork` takes it.
#[cfg(target_env = "uclibc")]
type ForkHandler = Option<unsafe extern "C" fn()>;

#[cfg(target_env = "uclibc")]
unsafe extern "C" {
    /// What `pthread_atfork` is built on; exported by both the crosstool's
    /// libc and the camera's (uClibc-ng 1.0.31), unlike `pthread_atfork`.
    fn __register_atfork(
        prepare: ForkHandler,
        parent: ForkHandler,
        child: ForkHandler,
        dso_handle: *mut std::ffi::c_void,
    ) -> c_int;
}

/// `pthread_atfork`, which no shared libc on either side exports.
///
/// uClibc keeps it in the static `libpthread_nonshared.a`, a thin wrapper
/// over `__register_atfork`, but the linker resolves the reference against
/// the crosstool's `libc.so` without ever pulling that archive member, so it
/// ends up undefined in the binary and the camera's ld.so refuses to start it
/// (`symbol 'pthread_atfork': can't resolve symbol`). `rand` 0.8 (through
/// `onvif-device-rs`) calls it for its fork protection. This is the same
/// wrapper; uClibc builds only, so host (glibc) tests keep the real one. The
/// DSO handle is only used to drop handlers when a shared
/// object is unloaded, which never happens to an executable.
///
/// # Safety
/// Same contract as the real function.
#[cfg(target_env = "uclibc")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_atfork(
    prepare: ForkHandler,
    parent: ForkHandler,
    child: ForkHandler,
) -> c_int {
    unsafe { __register_atfork(prepare, parent, child, std::ptr::null_mut()) }
}

/// Keeps the shim in the binary; see [`KEEP_PTHREAD_SETNAME_NP`].
#[cfg(target_env = "uclibc")]
#[used]
static KEEP_PTHREAD_ATFORK: unsafe extern "C" fn(ForkHandler, ForkHandler, ForkHandler) -> c_int =
    pthread_atfork;
